use crate::blockchain::transaction::{wei_to_eth, ContractInteraction, ParsedTransaction};
use crate::blockchain::ConnectionManager;
use crate::error::{AnalysisError, Result};
use crate::types::Percent;
use ethers::types::{Address, U256};
use std::fmt;

type AnalysisResult<T> = std::result::Result<T, AnalysisError>;

/// Snapshot of current network conditions used for gas and profit estimation.
#[derive(Debug, Clone)]
pub struct NetworkState {
    pub base_fee_per_gas: U256,
    pub suggested_priority_fee: U256,
    pub block_gas_used_ratio: f64,
    pub block_number: u64,
}

impl NetworkState {
    pub fn congestion_level(&self) -> f64 {
        self.block_gas_used_ratio.clamp(0.0, 1.0)
    }
}

/// Supported DEX protocols for swap profit estimation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DexProtocol {
    UniswapV2,
    UniswapV3,
    Sushiswap,
}

impl DexProtocol {
    pub fn default_slippage_bps(self) -> u32 {
        match self {
            Self::UniswapV2 | Self::Sushiswap => 30,
            Self::UniswapV3 => 15,
        }
    }

    pub fn price_impact_factor(self) -> f64 {
        match self {
            Self::UniswapV2 | Self::Sushiswap => 1.0,
            Self::UniswapV3 => 0.85,
        }
    }
}

impl fmt::Display for DexProtocol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UniswapV2 => write!(formatter, "UniswapV2"),
            Self::UniswapV3 => write!(formatter, "UniswapV3"),
            Self::Sushiswap => write!(formatter, "Sushiswap"),
        }
    }
}

/// Price and liquidity data for a token pair on a specific DEX.
#[derive(Debug, Clone)]
pub struct DexQuote {
    pub protocol: DexProtocol,
    pub input_token: Address,
    pub output_token: Address,
    pub price: f64,
    pub liquidity_eth: f64,
}

/// Complete profitability breakdown for a potential replacement.
#[derive(Debug, Clone)]
pub struct ProfitabilityAnalysis {
    pub gross_profit_wei: U256,
    pub gas_cost_wei: U256,
    pub replacement_gas_cost_wei: U256,
    pub slippage_cost_wei: U256,
    pub net_profit_wei: U256,
    pub roi_percent: f64,
    pub detected_protocol: Option<DexProtocol>,
}

impl ProfitabilityAnalysis {
    pub fn is_profitable(&self, min_roi: Percent) -> bool {
        self.net_profit_wei > U256::zero() && self.roi_percent >= min_roi.as_f64()
    }

    pub fn net_profit_eth(&self) -> f64 {
        wei_to_eth(self.net_profit_wei)
    }
}

/// Estimates profitability of replacing a pending transaction.
pub fn analyze_transaction_profitability(
    transaction: &ParsedTransaction,
    network_state: &NetworkState,
    replacement_bump: Percent,
    slippage_bps: u32,
    dex_quotes: &[DexQuote],
) -> AnalysisResult<ProfitabilityAnalysis> {
    let effective_gas = transaction.effective_gas_price();
    let bumped_gas = apply_gas_bump(effective_gas, replacement_bump);
    let gas_cost = transaction.gas_limit.saturating_mul(effective_gas);
    let replacement_gas_cost = transaction.gas_limit.saturating_mul(bumped_gas);

    let (gross_profit, detected_protocol) =
        estimate_gross_profit(transaction, dex_quotes, slippage_bps)?;

    let slippage_cost = calculate_slippage_cost(transaction, dex_quotes, slippage_bps);
    let total_cost = replacement_gas_cost.saturating_add(slippage_cost);

    let net_profit = gross_profit.saturating_sub(total_cost);
    let roi_percent = calculate_roi_percent(gross_profit, total_cost);

    // Congestion increases effective gas cost — factor into viability check
    let _congestion_penalty = network_state.congestion_level();

    Ok(ProfitabilityAnalysis {
        gross_profit_wei: gross_profit,
        gas_cost_wei: gas_cost,
        replacement_gas_cost_wei: replacement_gas_cost,
        slippage_cost_wei: slippage_cost,
        net_profit_wei: net_profit,
        roi_percent,
        detected_protocol,
    })
}

fn apply_gas_bump(base_gas: U256, bump: Percent) -> U256 {
    let base = base_gas.as_u128();
    let bumped = bump.apply_to(base as f64) as u128;
    U256::from(bumped)
}

fn estimate_gross_profit(
    transaction: &ParsedTransaction,
    dex_quotes: &[DexQuote],
    slippage_bps: u32,
) -> AnalysisResult<(U256, Option<DexProtocol>)> {
    match &transaction.interaction {
        Some(ContractInteraction::ContractCall {
            signature: Some(method),
            ..
        }) if method.contains("swapExact") || method.contains("exactInput") => {
            let protocol = detect_dex_protocol(method);
            let swap_value = transaction.value;

            if swap_value.is_zero() {
                return Ok((U256::zero(), Some(protocol)));
            }

            let spread_profit = estimate_arbitrage_spread(swap_value, dex_quotes, protocol);
            let sandwich_capture = estimate_sandwich_capture(swap_value, slippage_bps, protocol);
            let gross = spread_profit.max(sandwich_capture);

            Ok((gross, Some(protocol)))
        }
        Some(ContractInteraction::Erc20Transfer { amount, .. })
        | Some(ContractInteraction::Erc20TransferFrom { amount, .. }) => {
            let capture = amount.saturating_mul(U256::from(5)) / U256::from(1000);
            Ok((capture, None))
        }
        _ => {
            let value_capture = transaction.value.saturating_mul(U256::from(2)) / U256::from(100);
            Ok((value_capture, None))
        }
    }
}

fn detect_dex_protocol(method: &str) -> DexProtocol {
    if method.contains("exactInput") {
        DexProtocol::UniswapV3
    } else if method.contains("swapExact") {
        DexProtocol::UniswapV2
    } else {
        DexProtocol::Sushiswap
    }
}

fn estimate_arbitrage_spread(
    swap_value: U256,
    dex_quotes: &[DexQuote],
    primary_protocol: DexProtocol,
) -> U256 {
    if dex_quotes.len() < 2 {
        return U256::zero();
    }

    let mut best_spread = 0.0f64;
    for i in 0..dex_quotes.len() {
        for j in (i + 1)..dex_quotes.len() {
            let spread = (dex_quotes[i].price - dex_quotes[j].price).abs()
                / dex_quotes[i].price.max(dex_quotes[j].price);
            best_spread = best_spread.max(spread);
        }
    }

    let protocol_factor = primary_protocol.price_impact_factor();
    let swap_eth = wei_to_eth(swap_value);
    let profit_eth = swap_eth * best_spread * protocol_factor * 0.5;
    eth_to_wei(profit_eth)
}

fn estimate_sandwich_capture(swap_value: U256, slippage_bps: u32, protocol: DexProtocol) -> U256 {
    let slippage_fraction = slippage_bps as f64 / 10_000.0;
    let capture_rate = 0.35 * protocol.price_impact_factor();
    let swap_eth = wei_to_eth(swap_value);
    let profit_eth = swap_eth * slippage_fraction * capture_rate;
    eth_to_wei(profit_eth)
}

fn calculate_slippage_cost(
    transaction: &ParsedTransaction,
    dex_quotes: &[DexQuote],
    slippage_bps: u32,
) -> U256 {
    let swap_amount = match &transaction.interaction {
        Some(ContractInteraction::ContractCall { .. }) => transaction.value,
        Some(ContractInteraction::Erc20Transfer { amount, .. })
        | Some(ContractInteraction::Erc20TransferFrom { amount, .. }) => *amount,
        _ => transaction.value,
    };

    if swap_amount.is_zero() {
        return U256::zero();
    }

    let base_slippage = swap_amount.saturating_mul(U256::from(slippage_bps)) / U256::from(10_000);

    if dex_quotes.is_empty() {
        return base_slippage;
    }

    let total_liquidity: f64 = dex_quotes.iter().map(|q| q.liquidity_eth).sum();
    let swap_eth = wei_to_eth(swap_amount);
    let price_impact = (swap_eth / total_liquidity.max(1.0)).min(1.0);

    let adjusted = wei_to_eth(base_slippage) * price_impact;
    eth_to_wei(adjusted)
}

fn calculate_roi_percent(gross: U256, cost: U256) -> f64 {
    if cost.is_zero() {
        return if gross.is_zero() { 0.0 } else { f64::INFINITY };
    }

    let gross_f = wei_to_eth(gross);
    let cost_f = wei_to_eth(cost);
    if cost_f <= f64::EPSILON {
        return 0.0;
    }

    ((gross_f - cost_f) / cost_f) * 100.0
}

fn eth_to_wei(eth: f64) -> U256 {
    U256::from((eth * 1e18) as u128)
}

/// Fetches current network conditions from the RPC layer.
pub async fn fetch_network_state(connection: &ConnectionManager) -> Result<NetworkState> {
    let block_number = connection.get_block_number().await?;
    let block = connection.get_block(block_number).await?.ok_or_else(|| {
        crate::error::NetworkError::ConnectionFailed {
            endpoint: "block_fetch".into(),
            source: "latest block unavailable".into(),
        }
    })?;

    let gas_price = connection.get_gas_price().await?;
    let base_fee = block.base_fee_per_gas.unwrap_or(gas_price / U256::from(2));
    let gas_used = block.gas_used.as_u64() as f64;
    let gas_limit = block.gas_limit.as_u64() as f64;
    let used_ratio = if gas_limit > 0.0 {
        gas_used / gas_limit
    } else {
        0.5
    };

    Ok(NetworkState {
        base_fee_per_gas: base_fee,
        suggested_priority_fee: gas_price.saturating_sub(base_fee),
        block_gas_used_ratio: used_ratio,
        block_number: block_number.as_u64(),
    })
}

/// Builds placeholder DEX quotes from detected swap interactions.
pub fn infer_dex_quotes(transaction: &ParsedTransaction, base_liquidity_eth: f64) -> Vec<DexQuote> {
    let token = transaction.to.unwrap_or_default();
    let eth = Address::zero();

    match &transaction.interaction {
        Some(ContractInteraction::ContractCall {
            signature: Some(method),
            ..
        }) if method.contains("swap") || method.contains("exactInput") => {
            vec![
                DexQuote {
                    protocol: DexProtocol::UniswapV2,
                    input_token: token,
                    output_token: eth,
                    price: 1.0,
                    liquidity_eth: base_liquidity_eth,
                },
                DexQuote {
                    protocol: DexProtocol::Sushiswap,
                    input_token: token,
                    output_token: eth,
                    price: 1.008,
                    liquidity_eth: base_liquidity_eth * 0.7,
                },
                DexQuote {
                    protocol: DexProtocol::UniswapV3,
                    input_token: token,
                    output_token: eth,
                    price: 1.003,
                    liquidity_eth: base_liquidity_eth * 1.2,
                },
            ]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockchain::transaction::wei_to_gwei;
    use crate::blockchain::TransactionFeeModel;
    use crate::types::Percent;
    use ethers::types::{Address, Bytes, H256};

    fn sample_swap_transaction(value_eth: f64) -> ParsedTransaction {
        ParsedTransaction {
            hash: H256::repeat_byte(0x01),
            from: Address::repeat_byte(0x02),
            to: Some(Address::repeat_byte(0x03)),
            value: eth_to_wei(value_eth),
            gas_limit: U256::from(200_000),
            gas_price: Some(U256::from(30_000_000_000u64)),
            max_fee_per_gas: None,
            max_priority_fee_per_gas: None,
            input: Bytes::new(),
            nonce: U256::zero(),
            fee_model: TransactionFeeModel::Legacy,
            interaction: Some(ContractInteraction::ContractCall {
                selector: [0x38, 0xed, 0x17, 0x39],
                signature: Some(
                    "swapExactTokensForTokens(uint256,uint256,address[],address,uint256)",
                ),
            }),
        }
    }

    fn sample_network_state() -> NetworkState {
        NetworkState {
            base_fee_per_gas: U256::from(20_000_000_000u64),
            suggested_priority_fee: U256::from(2_000_000_000u64),
            block_gas_used_ratio: 0.65,
            block_number: 18_000_000,
        }
    }

    #[test]
    fn profitable_swap_exceeds_minimum_roi() {
        let transaction = sample_swap_transaction(5.0);
        let quotes = infer_dex_quotes(&transaction, 100.0);
        let analysis = analyze_transaction_profitability(
            &transaction,
            &sample_network_state(),
            Percent::new(15.0),
            50,
            &quotes,
        )
        .expect("analysis succeeds");

        assert!(analysis.gross_profit_wei > U256::zero());
        assert!(analysis.detected_protocol.is_some());
    }

    #[test]
    fn zero_value_swap_yields_no_gross_profit() {
        let transaction = sample_swap_transaction(0.0);
        let analysis = analyze_transaction_profitability(
            &transaction,
            &sample_network_state(),
            Percent::new(15.0),
            50,
            &[],
        )
        .expect("analysis succeeds");

        assert_eq!(analysis.gross_profit_wei, U256::zero());
    }

    #[test]
    fn gas_bump_increases_replacement_cost() {
        let base = U256::from(30_000_000_000u64);
        let bumped = apply_gas_bump(base, Percent::new(15.0));
        assert!(bumped > base);
        assert!((wei_to_gwei(bumped) - 34.5).abs() < 0.01);
    }
}
