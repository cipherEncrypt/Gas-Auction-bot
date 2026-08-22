use crate::analysis::profitability::{DexQuote, NetworkState};
use crate::blockchain::transaction::{ContractInteraction, ParsedTransaction};
use std::collections::HashMap;
use std::sync::RwLock;

const DEFAULT_SUCCESS_RATE: f64 = 0.72;

/// Risk breakdown for a pending transaction on a 0–100 scale (higher = riskier).
#[derive(Debug, Clone)]
pub struct RiskAssessment {
    pub score: u8,
    pub congestion_component: u8,
    pub liquidity_component: u8,
    pub volatility_component: u8,
    pub history_component: u8,
    pub historical_success_rate: f64,
}

impl RiskAssessment {
    pub fn is_acceptable(&self, max_score: u8) -> bool {
        self.score <= max_score
    }

    pub fn risk_adjusted_roi(&self, raw_roi_percent: f64) -> f64 {
        let confidence = 1.0 - (self.score as f64 / 100.0);
        raw_roi_percent * confidence
    }
}

/// Tracks historical success rates per contract address.
#[derive(Debug, Default)]
pub struct SuccessRateTracker {
    records: RwLock<HashMap<String, (u64, u64)>>,
}

impl SuccessRateTracker {
    pub fn record_outcome(&self, contract: &str, succeeded: bool) {
        let mut guard = self.records.write().expect("lock poisoned");
        let entry = guard.entry(contract.to_lowercase()).or_insert((0, 0));
        entry.1 += 1;
        if succeeded {
            entry.0 += 1;
        }
    }

    pub fn success_rate(&self, contract: &str) -> f64 {
        let guard = self.records.read().expect("lock poisoned");
        guard
            .get(&contract.to_lowercase())
            .map(|(successes, total)| {
                if *total == 0 {
                    DEFAULT_SUCCESS_RATE
                } else {
                    *successes as f64 / *total as f64
                }
            })
            .unwrap_or(DEFAULT_SUCCESS_RATE)
    }
}

/// Scores a transaction's risk based on network, liquidity, and historical factors.
pub fn assess_transaction_risk(
    transaction: &ParsedTransaction,
    network_state: &NetworkState,
    dex_quotes: &[DexQuote],
    success_tracker: &SuccessRateTracker,
) -> RiskAssessment {
    let congestion_component = score_congestion(network_state.block_gas_used_ratio);
    let liquidity_component = score_liquidity(dex_quotes, transaction);
    let volatility_component = score_volatility(transaction);
    let history_component = score_history(transaction, success_tracker);

    let historical_success_rate = contract_success_rate(transaction, success_tracker);

    let weighted = (congestion_component as f64 * 0.30)
        + (liquidity_component as f64 * 0.25)
        + (volatility_component as f64 * 0.20)
        + (history_component as f64 * 0.25);

    RiskAssessment {
        score: weighted.round().clamp(0.0, 100.0) as u8,
        congestion_component,
        liquidity_component,
        volatility_component,
        history_component,
        historical_success_rate,
    }
}

fn score_congestion(gas_used_ratio: f64) -> u8 {
    (gas_used_ratio * 100.0).round().clamp(0.0, 100.0) as u8
}

fn score_liquidity(dex_quotes: &[DexQuote], transaction: &ParsedTransaction) -> u8 {
    if dex_quotes.is_empty() {
        return match &transaction.interaction {
            Some(ContractInteraction::ContractCall { .. }) => 75,
            _ => 40,
        };
    }

    let min_liquidity = dex_quotes
        .iter()
        .map(|quote| quote.liquidity_eth)
        .fold(f64::INFINITY, f64::min);

    if min_liquidity >= 500.0 {
        10
    } else if min_liquidity >= 100.0 {
        30
    } else if min_liquidity >= 10.0 {
        55
    } else {
        85
    }
}

fn score_volatility(transaction: &ParsedTransaction) -> u8 {
    let value_eth = transaction.value_eth();
    if value_eth >= 50.0 {
        80
    } else if value_eth >= 10.0 {
        60
    } else if value_eth >= 1.0 {
        35
    } else {
        20
    }
}

fn score_history(transaction: &ParsedTransaction, tracker: &SuccessRateTracker) -> u8 {
    let rate = contract_success_rate(transaction, tracker);
    ((1.0 - rate) * 100.0).round().clamp(0.0, 100.0) as u8
}

fn contract_success_rate(transaction: &ParsedTransaction, tracker: &SuccessRateTracker) -> f64 {
    let contract = transaction
        .to
        .map(|address| format!("{address:?}"))
        .unwrap_or_else(|| "unknown".into());
    tracker.success_rate(&contract)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::profitability::DexProtocol;
    use crate::blockchain::transaction::TransactionFeeModel;
    use ethers::types::{Address, Bytes, H256, U256};

    fn sample_transaction(value_eth: f64) -> ParsedTransaction {
        ParsedTransaction {
            hash: H256::repeat_byte(0x01),
            from: Address::repeat_byte(0x02),
            to: Some(Address::repeat_byte(0x03)),
            value: U256::from((value_eth * 1e18) as u128),
            gas_limit: U256::from(200_000),
            gas_price: Some(U256::from(30_000_000_000u64)),
            max_fee_per_gas: None,
            max_priority_fee_per_gas: None,
            input: Bytes::new(),
            nonce: U256::zero(),
            fee_model: TransactionFeeModel::Legacy,
            interaction: None,
        }
    }

    fn high_liquidity_quotes() -> Vec<DexQuote> {
        vec![DexQuote {
            protocol: DexProtocol::UniswapV2,
            input_token: Address::repeat_byte(0x03),
            output_token: Address::zero(),
            price: 1.0,
            liquidity_eth: 500.0,
        }]
    }

    #[test]
    fn high_congestion_increases_risk_score() {
        let network = NetworkState {
            base_fee_per_gas: U256::from(20_000_000_000u64),
            suggested_priority_fee: U256::from(2_000_000_000u64),
            block_gas_used_ratio: 0.95,
            block_number: 1,
        };

        let assessment = assess_transaction_risk(
            &sample_transaction(1.0),
            &network,
            &high_liquidity_quotes(),
            &SuccessRateTracker::default(),
        );

        assert!(assessment.congestion_component >= 90);
    }

    #[test]
    fn low_liquidity_raises_risk() {
        let network = NetworkState {
            base_fee_per_gas: U256::zero(),
            suggested_priority_fee: U256::zero(),
            block_gas_used_ratio: 0.3,
            block_number: 1,
        };

        let assessment = assess_transaction_risk(
            &sample_transaction(1.0),
            &network,
            &[],
            &SuccessRateTracker::default(),
        );

        assert!(assessment.liquidity_component >= 40);
    }

    #[test]
    fn success_tracker_updates_history() {
        let tracker = SuccessRateTracker::default();
        tracker.record_outcome("0xabc", true);
        tracker.record_outcome("0xabc", false);

        assert!((tracker.success_rate("0xabc") - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn risk_adjusted_roi_scales_with_score() {
        let low_risk = RiskAssessment {
            score: 20,
            congestion_component: 20,
            liquidity_component: 20,
            volatility_component: 20,
            history_component: 20,
            historical_success_rate: 0.8,
        };

        let high_risk = RiskAssessment {
            score: 80,
            congestion_component: 80,
            liquidity_component: 80,
            volatility_component: 80,
            history_component: 80,
            historical_success_rate: 0.2,
        };

        assert!(low_risk.risk_adjusted_roi(50.0) > high_risk.risk_adjusted_roi(50.0));
    }
}
