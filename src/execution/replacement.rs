use crate::analysis::opportunity::ScoredOpportunity;
use crate::analysis::profitability::NetworkState;
use crate::blockchain::transaction::wei_to_gwei;
use crate::blockchain::ConnectionManager;
use crate::config::Settings;
use crate::error::Result;
use crate::execution::gas_auction::{
    build_typed_transaction, load_wallet, sign_transaction, GasAuctionCalculator, NonceManager,
    SignedSubmission, TransactionBuildParams,
};
use crate::execution::safety::SafetyGuard;
use ethers::signers::{LocalWallet, Signer};
use ethers::types::{Address, TxHash, U256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{info, warn};

const MAX_REPLACEMENT_ATTEMPTS: u32 = 3;

/// Outcome of a submitted or replaced transaction.
#[derive(Debug, Clone)]
pub enum SubmissionOutcome {
    Confirmed {
        hash: TxHash,
        block_number: u64,
        gas_used: U256,
    },
    Replaced {
        original_hash: TxHash,
        replacement_hash: TxHash,
    },
    Dropped {
        hash: TxHash,
        wait_secs: u64,
    },
    Rejected {
        reason: String,
    },
}

/// Tracks an in-flight submission awaiting confirmation or replacement.
#[derive(Debug, Clone)]
struct PendingSubmission {
    nonce: u64,
    gas_price: U256,
    replacement_attempts: u32,
}

/// Orchestrates gas-competitive submission, replacement, and confirmation monitoring.
pub struct ReplacementExecutor {
    connection: Arc<ConnectionManager>,
    wallet: LocalWallet,
    wallet_address: Address,
    chain_id: u64,
    gas_calculator: GasAuctionCalculator,
    nonce_manager: NonceManager,
    safety_guard: SafetyGuard,
    confirmation_timeout: Duration,
    replacement_poll_interval: Duration,
    pending_submissions: HashMap<TxHash, PendingSubmission>,
}

impl ReplacementExecutor {
    pub async fn initialize(
        connection: Arc<ConnectionManager>,
        settings: &Settings,
    ) -> Result<Self> {
        let wallet = load_wallet(settings)?;
        let wallet_address = wallet.address();
        let nonce_manager = NonceManager::new(wallet_address);

        nonce_manager
            .sync_from_chain(connection.get_account_nonce(wallet_address))
            .await?;

        Ok(Self {
            connection,
            wallet,
            wallet_address,
            chain_id: settings.network.chain_id,
            gas_calculator: GasAuctionCalculator::from_settings(settings),
            nonce_manager,
            safety_guard: SafetyGuard::from_settings(settings),
            confirmation_timeout: Duration::from_secs(
                settings.execution.confirmation_timeout_secs.max(1),
            ),
            replacement_poll_interval: Duration::from_secs(
                settings.execution.replacement_poll_interval_secs.max(1),
            ),
            pending_submissions: HashMap::new(),
        })
    }

    pub fn execution_enabled(settings: &Settings) -> bool {
        !settings.wallet.private_key.is_empty() && !settings.safety.emergency_stop
    }

    pub fn wallet_address(&self) -> Address {
        self.wallet_address
    }

    pub fn safety(&self) -> &SafetyGuard {
        &self.safety_guard
    }

    /// Submits a competitive transaction for a scored opportunity.
    pub async fn execute_opportunity(
        &mut self,
        opportunity: &ScoredOpportunity,
        network_state: &NetworkState,
    ) -> Result<SubmissionOutcome> {
        let victim_gas = opportunity.pending.parsed.effective_gas_price();
        let replacement_gas = self
            .gas_calculator
            .calculate_replacement_gas_price(victim_gas, network_state)?;

        self.safety_guard
            .preflight_check(opportunity, wei_to_gwei(replacement_gas))?;

        let nonce = self.nonce_manager.allocate_nonce()?;
        let parsed = &opportunity.pending.parsed;

        let request = build_typed_transaction(&TransactionBuildParams {
            chain_id: self.chain_id,
            nonce,
            gas_limit: parsed.gas_limit,
            gas_price: replacement_gas,
            fee_model: parsed.fee_model,
            to: parsed.to,
            value: parsed.value,
            input: parsed.input.clone(),
        });

        let signed = sign_transaction(&self.wallet, request).await?;
        self.submit_and_monitor(signed, opportunity).await
    }

    async fn submit_and_monitor(
        &mut self,
        signed: SignedSubmission,
        opportunity: &ScoredOpportunity,
    ) -> Result<SubmissionOutcome> {
        let submitted_hash = match self
            .connection
            .send_raw_transaction(signed.raw_transaction)
            .await
        {
            Ok(hash) => hash,
            Err(error) => {
                self.nonce_manager.release_nonce(signed.nonce);
                self.safety_guard.record_failure();
                return Ok(SubmissionOutcome::Rejected {
                    reason: error.to_string(),
                });
            }
        };

        info!(
            hash = %submitted_hash,
            nonce = signed.nonce,
            gas_gwei = wei_to_gwei(signed.gas_price),
            target = %opportunity.hash(),
            "replacement transaction submitted"
        );

        self.pending_submissions.insert(
            submitted_hash,
            PendingSubmission {
                nonce: signed.nonce,
                gas_price: signed.gas_price,
                replacement_attempts: 0,
            },
        );

        self.monitor_until_settled(submitted_hash, opportunity)
            .await
    }

    async fn monitor_until_settled(
        &mut self,
        mut current_hash: TxHash,
        opportunity: &ScoredOpportunity,
    ) -> Result<SubmissionOutcome> {
        let deadline = Instant::now() + self.confirmation_timeout;

        loop {
            if Instant::now() >= deadline {
                self.pending_submissions.remove(&current_hash);
                self.safety_guard.record_failure();
                return Ok(SubmissionOutcome::Dropped {
                    hash: current_hash,
                    wait_secs: self.confirmation_timeout.as_secs(),
                });
            }

            if let Some(receipt) = self
                .connection
                .get_transaction_receipt(current_hash)
                .await?
            {
                self.pending_submissions.remove(&current_hash);
                self.safety_guard.record_success();
                self.safety_guard
                    .record_spend(receipt.gas_used.unwrap_or_default().as_u128());

                return Ok(SubmissionOutcome::Confirmed {
                    hash: current_hash,
                    block_number: receipt.block_number.unwrap_or_default().as_u64(),
                    gas_used: receipt.gas_used.unwrap_or_default(),
                });
            }

            if let Some(pending) = self.pending_submissions.get(&current_hash).cloned() {
                if pending.replacement_attempts < MAX_REPLACEMENT_ATTEMPTS {
                    sleep(self.replacement_poll_interval).await;

                    if self
                        .connection
                        .get_transaction_receipt(current_hash)
                        .await?
                        .is_some()
                    {
                        continue;
                    }

                    match self
                        .attempt_replacement(current_hash, &pending, opportunity)
                        .await
                    {
                        Ok(new_hash) => {
                            self.pending_submissions.remove(&current_hash);
                            current_hash = new_hash;
                            continue;
                        }
                        Err(error) => {
                            warn!(%error, hash = %current_hash, "replacement attempt failed");
                        }
                    }
                }
            }

            sleep(self.replacement_poll_interval).await;
        }
    }

    async fn attempt_replacement(
        &mut self,
        current_hash: TxHash,
        pending: &PendingSubmission,
        opportunity: &ScoredOpportunity,
    ) -> Result<TxHash> {
        let bumped_gas = self
            .gas_calculator
            .apply_additional_bump(pending.gas_price)?;
        self.safety_guard
            .preflight_check(opportunity, wei_to_gwei(bumped_gas))?;

        let parsed = &opportunity.pending.parsed;
        let request = build_typed_transaction(&TransactionBuildParams {
            chain_id: self.chain_id,
            nonce: pending.nonce,
            gas_limit: parsed.gas_limit,
            gas_price: bumped_gas,
            fee_model: parsed.fee_model,
            to: parsed.to,
            value: parsed.value,
            input: parsed.input.clone(),
        });

        let signed = sign_transaction(&self.wallet, request).await?;
        let new_hash = self
            .connection
            .send_raw_transaction(signed.raw_transaction)
            .await?;

        self.pending_submissions.insert(
            new_hash,
            PendingSubmission {
                nonce: pending.nonce,
                gas_price: bumped_gas,
                replacement_attempts: pending.replacement_attempts + 1,
            },
        );

        info!(
            original = %current_hash,
            replacement = %new_hash,
            nonce = pending.nonce,
            gas_gwei = wei_to_gwei(bumped_gas),
            "transaction replaced with higher gas"
        );

        Ok(new_hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_disabled_without_private_key() {
        use crate::config::{
            AnalysisSettings, ExecutionSettings, GasSettings, LoggingSettings, NetworkSettings,
            ProfitSettings, SafetySettings, ServerSettings, Settings, WalletSettings,
        };

        let settings = Settings {
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
        };

        assert!(!ReplacementExecutor::execution_enabled(&settings));
    }
}
