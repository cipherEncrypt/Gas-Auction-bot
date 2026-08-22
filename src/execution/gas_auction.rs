use crate::analysis::profitability::NetworkState;
use crate::blockchain::transaction::{wei_to_gwei, TransactionFeeModel};
use crate::config::Settings;
use crate::error::{ExecutionError, Result};
use crate::types::{Gwei, Percent};
use ethers::signers::{LocalWallet, Signer};
use ethers::types::transaction::eip1559::Eip1559TransactionRequest;
use ethers::types::transaction::eip2718::TypedTransaction;
use ethers::types::{Address, TxHash, U256};
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{debug, warn};
use zeroize::Zeroize;

/// Manages wallet nonce allocation and tracks in-flight submissions.
#[derive(Debug)]
pub struct NonceManager {
    wallet_address: Address,
    next_nonce: AtomicU64,
    synced: AtomicU64,
}

impl NonceManager {
    pub fn new(wallet_address: Address) -> Self {
        Self {
            wallet_address,
            next_nonce: AtomicU64::new(0),
            synced: AtomicU64::new(0),
        }
    }

    pub fn wallet_address(&self) -> Address {
        self.wallet_address
    }

    pub async fn sync_from_chain(
        &self,
        fetch_nonce: impl std::future::Future<Output = Result<U256>>,
    ) -> Result<u64> {
        let chain_nonce = fetch_nonce.await?;
        let nonce = chain_nonce.as_u64();
        self.next_nonce.store(nonce, Ordering::SeqCst);
        self.synced.store(1, Ordering::SeqCst);
        debug!(wallet = %self.wallet_address, nonce, "nonce synced from chain");
        Ok(nonce)
    }

    pub fn allocate_nonce(&self) -> Result<u64> {
        if self.synced.load(Ordering::SeqCst) == 0 {
            return Err(ExecutionError::NonceConflict {
                expected: 0,
                actual: 0,
            }
            .into());
        }

        Ok(self.next_nonce.fetch_add(1, Ordering::SeqCst))
    }

    pub fn release_nonce(&self, nonce: u64) {
        let current = self.next_nonce.load(Ordering::SeqCst);
        if nonce + 1 == current {
            self.next_nonce.store(nonce, Ordering::SeqCst);
        }
    }

    pub fn current_nonce(&self) -> u64 {
        self.next_nonce.load(Ordering::SeqCst)
    }
}

/// Computes competitive gas prices for mempool replacement bidding.
pub struct GasAuctionCalculator {
    replacement_bump: Percent,
    min_gas_price: Gwei,
    max_gas_price: Gwei,
    soft_gas_cap: Gwei,
}

impl GasAuctionCalculator {
    pub fn from_settings(settings: &Settings) -> Self {
        let soft_cap_gwei = settings.gas.max_gas_price_gwei * settings.execution.soft_gas_cap_ratio;
        Self {
            replacement_bump: settings.replacement_bump(),
            min_gas_price: settings.min_gas_price(),
            max_gas_price: settings.max_gas_price(),
            soft_gas_cap: Gwei::new(soft_cap_gwei),
        }
    }

    pub fn soft_gas_cap(&self) -> Gwei {
        self.soft_gas_cap
    }

    /// Calculates replacement gas as victim price + bump, clamped to configured bounds.
    pub fn calculate_replacement_gas_price(
        &self,
        victim_gas_price: U256,
        network_state: &NetworkState,
    ) -> Result<U256> {
        let victim_gwei = wei_to_gwei(victim_gas_price);
        let bumped_gwei = self.replacement_bump.apply_to(victim_gwei);

        let network_floor = wei_to_gwei(
            network_state
                .base_fee_per_gas
                .saturating_add(network_state.suggested_priority_fee),
        );
        let competitive_gwei = bumped_gwei
            .max(network_floor)
            .max(self.min_gas_price.as_f64());

        if competitive_gwei > self.max_gas_price.as_f64() {
            return Err(ExecutionError::GasPriceExceeded {
                offered_gwei: competitive_gwei,
                cap_gwei: self.max_gas_price.as_f64(),
            }
            .into());
        }

        if competitive_gwei > self.soft_gas_cap.as_f64() {
            warn!(
                offered_gwei = competitive_gwei,
                soft_cap = %self.soft_gas_cap,
                "replacement gas exceeds soft cap"
            );
        }

        Ok(U256::from((competitive_gwei * 1_000_000_000.0) as u128))
    }

    pub fn apply_additional_bump(&self, current_gas: U256) -> Result<U256> {
        let current_gwei = wei_to_gwei(current_gas);
        let bumped = self.replacement_bump.apply_to(current_gwei);

        if bumped > self.max_gas_price.as_f64() {
            return Err(ExecutionError::GasPriceExceeded {
                offered_gwei: bumped,
                cap_gwei: self.max_gas_price.as_f64(),
            }
            .into());
        }

        Ok(U256::from((bumped * 1_000_000_000.0) as u128))
    }
}

/// Loads and validates the bot wallet from configuration.
///
/// The private key string is zeroized after parsing so it does not linger in heap memory.
pub fn load_wallet(settings: &Settings) -> Result<LocalWallet> {
    let mut private_key = settings.wallet.private_key.clone();
    if private_key.is_empty() {
        return Err(ExecutionError::SigningFailed {
            reason: "wallet private key not configured".into(),
        }
        .into());
    }

    let wallet: LocalWallet =
        private_key
            .parse()
            .map_err(|source| ExecutionError::SigningFailed {
                reason: format!("invalid private key: {source}"),
            })?;

    private_key.zeroize();

    Ok(wallet.with_chain_id(settings.network.chain_id))
}

/// Parameters for constructing a typed Ethereum transaction.
#[derive(Debug, Clone)]
pub struct TransactionBuildParams {
    pub chain_id: u64,
    pub nonce: u64,
    pub gas_limit: U256,
    pub gas_price: U256,
    pub fee_model: TransactionFeeModel,
    pub to: Option<Address>,
    pub value: U256,
    pub input: ethers::types::Bytes,
}

/// Builds a typed transaction for competitive submission or replacement.
pub fn build_typed_transaction(params: &TransactionBuildParams) -> TypedTransaction {
    match params.fee_model {
        TransactionFeeModel::Eip1559 => {
            let priority_fee = params.gas_price / U256::from(5);
            let mut request = Eip1559TransactionRequest::new()
                .chain_id(params.chain_id)
                .nonce(params.nonce)
                .gas(params.gas_limit)
                .value(params.value)
                .data(params.input.clone())
                .max_fee_per_gas(params.gas_price)
                .max_priority_fee_per_gas(priority_fee);

            if let Some(target) = params.to {
                request = request.to(target);
            }

            TypedTransaction::Eip1559(request)
        }
        TransactionFeeModel::Legacy => {
            let mut request = ethers::types::TransactionRequest::new()
                .chain_id(params.chain_id)
                .nonce(params.nonce)
                .gas(params.gas_limit)
                .value(params.value)
                .data(params.input.clone())
                .gas_price(params.gas_price);

            if let Some(target) = params.to {
                request = request.to(target);
            }

            request.into()
        }
    }
}

#[derive(Debug, Clone)]
pub struct SignedSubmission {
    pub hash: TxHash,
    pub nonce: u64,
    pub gas_price: U256,
    pub raw_transaction: ethers::types::Bytes,
}

pub async fn sign_transaction(
    wallet: &LocalWallet,
    typed: TypedTransaction,
) -> Result<SignedSubmission> {
    let signature =
        wallet
            .sign_transaction(&typed)
            .await
            .map_err(|source| ExecutionError::SigningFailed {
                reason: source.to_string(),
            })?;

    let raw = typed.rlp_signed(&signature);
    let hash = typed.hash(&signature);
    let nonce = typed_nonce(&typed).as_u64();
    let gas_price = typed_gas_price(&typed);

    Ok(SignedSubmission {
        hash,
        nonce,
        gas_price,
        raw_transaction: raw,
    })
}

fn typed_nonce(typed: &TypedTransaction) -> U256 {
    match typed {
        TypedTransaction::Legacy(inner) => inner.nonce.unwrap_or_default(),
        TypedTransaction::Eip2930(inner) => inner.tx.nonce.unwrap_or_default(),
        TypedTransaction::Eip1559(inner) => inner.nonce.unwrap_or_default(),
    }
}

fn typed_gas_price(typed: &TypedTransaction) -> U256 {
    match typed {
        TypedTransaction::Legacy(inner) => inner.gas_price.unwrap_or_default(),
        TypedTransaction::Eip2930(inner) => inner.tx.gas_price.unwrap_or_default(),
        TypedTransaction::Eip1559(inner) => inner.max_fee_per_gas.unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AnalysisSettings, ExecutionSettings, GasSettings, LoggingSettings, NetworkSettings,
        ProfitSettings, SafetySettings, ServerSettings, Settings, WalletSettings,
    };

    fn test_settings() -> Settings {
        Settings {
            network: NetworkSettings {
                chain_id: 1,
                rpc_urls: vec!["http://127.0.0.1:8545".to_string()],
            },
            gas: GasSettings {
                max_gas_price_gwei: 500.0,
                min_gas_price_gwei: 1.0,
                replacement_bump_percent: 15.0,
            },
            profit: ProfitSettings {
                min_profit_percent: 15.0,
                min_tx_value_eth: 0.1,
            },
            safety: SafetySettings {
                circuit_breaker_enabled: true,
                max_consecutive_failures: 5,
                max_daily_spend_eth: 1.0,
                emergency_stop: false,
            },
            logging: LoggingSettings {
                level: "info".into(),
                log_file: "logs/test.log".into(),
                json_log_enabled: false,
            },
            wallet: WalletSettings {
                private_key: String::new(),
            },
            analysis: AnalysisSettings {
                slippage_bps: 50,
                max_risk_score: 70,
                base_liquidity_eth: 100.0,
                max_queue_size: 256,
            },
            execution: ExecutionSettings {
                confirmation_timeout_secs: 60,
                replacement_poll_interval_secs: 5,
                soft_gas_cap_ratio: 0.8,
            },
            server: ServerSettings {
                bind_address: "127.0.0.1:9090".into(),
                worker_count: 4,
                network_cache_ttl_secs: 12,
                shutdown_drain_secs: 10,
            },
        }
    }

    fn sample_network_state() -> NetworkState {
        NetworkState {
            base_fee_per_gas: U256::from(20_000_000_000u64),
            suggested_priority_fee: U256::from(2_000_000_000u64),
            block_gas_used_ratio: 0.5,
            block_number: 1,
        }
    }

    #[test]
    fn replacement_gas_includes_bump_over_victim() {
        let calculator = GasAuctionCalculator::from_settings(&test_settings());
        let victim_gas = U256::from(30_000_000_000u64);
        let replacement = calculator
            .calculate_replacement_gas_price(victim_gas, &sample_network_state())
            .expect("within gas cap");

        assert!(replacement > victim_gas);
    }

    #[test]
    fn rejects_gas_above_hard_cap() {
        let mut settings = test_settings();
        settings.gas.max_gas_price_gwei = 40.0;
        let calculator = GasAuctionCalculator::from_settings(&settings);
        let victim_gas = U256::from(50_000_000_000u64);

        assert!(calculator
            .calculate_replacement_gas_price(victim_gas, &sample_network_state())
            .is_err());
    }

    #[test]
    fn nonce_manager_allocates_sequential_nonces() {
        let manager = NonceManager::new(Address::repeat_byte(0x01));
        manager.synced.store(1, Ordering::SeqCst);
        manager.next_nonce.store(5, Ordering::SeqCst);

        assert_eq!(manager.allocate_nonce().expect("synced"), 5);
        assert_eq!(manager.allocate_nonce().expect("synced"), 6);
    }
}
