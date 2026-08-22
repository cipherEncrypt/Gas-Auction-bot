use crate::analysis::opportunity::ScoredOpportunity;
use crate::config::Settings;
use crate::error::{Result, SafetyError};
use crate::execution::gas_auction::GasAuctionCalculator;
use crate::types::{EthAmount, Percent};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::RwLock;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Enforces spending limits, circuit breaking, and profitability guardrails.
pub struct SafetyGuard {
    circuit_breaker_enabled: bool,
    max_consecutive_failures: u32,
    max_daily_spend: EthAmount,
    min_profit_threshold: Percent,
    emergency_stop: bool,
    soft_gas_cap: crate::types::Gwei,
    consecutive_failures: AtomicU32,
    daily_spend_wei: RwLock<u128>,
    spend_day_started: RwLock<Instant>,
}

impl SafetyGuard {
    pub fn from_settings(settings: &Settings) -> Self {
        let calculator = GasAuctionCalculator::from_settings(settings);
        Self {
            circuit_breaker_enabled: settings.safety.circuit_breaker_enabled,
            max_consecutive_failures: settings.safety.max_consecutive_failures,
            max_daily_spend: settings.max_daily_spend(),
            min_profit_threshold: settings.min_profit_threshold(),
            emergency_stop: settings.safety.emergency_stop,
            soft_gas_cap: calculator.soft_gas_cap(),
            consecutive_failures: AtomicU32::new(0),
            daily_spend_wei: RwLock::new(0),
            spend_day_started: RwLock::new(Instant::now()),
        }
    }

    pub fn preflight_check(
        &self,
        opportunity: &ScoredOpportunity,
        gas_price_gwei: f64,
    ) -> Result<()> {
        if self.emergency_stop {
            return Err(SafetyError::EmergencyStopActive.into());
        }

        if self.circuit_breaker_enabled {
            let failures = self.consecutive_failures.load(Ordering::Relaxed);
            if failures >= self.max_consecutive_failures {
                return Err(SafetyError::CircuitBreakerOpen {
                    consecutive_failures: failures,
                }
                .into());
            }
        }

        if gas_price_gwei > self.soft_gas_cap.as_f64() {
            return Err(SafetyError::SoftGasCapExceeded {
                current_gwei: gas_price_gwei,
                soft_cap_gwei: self.soft_gas_cap.as_f64(),
            }
            .into());
        }

        if !opportunity
            .profitability
            .is_profitable(self.min_profit_threshold)
        {
            return Err(SafetyError::InsufficientRoi {
                actual_percent: opportunity.profitability.roi_percent,
                required_percent: self.min_profit_threshold.as_f64(),
            }
            .into());
        }

        let estimated_spend_wei = opportunity.profitability.replacement_gas_cost_wei.as_u128();
        self.check_daily_spend_limit(estimated_spend_wei)?;

        Ok(())
    }

    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    pub fn record_failure(&self) {
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_spend(&self, spent_wei: u128) {
        self.reset_daily_spend_if_needed();
        let mut guard = self.daily_spend_wei.write().expect("lock poisoned");
        *guard = guard.saturating_add(spent_wei);
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }

    pub fn trigger_emergency_stop(&mut self) {
        warn!("emergency stop triggered");
        self.emergency_stop = true;
    }

    fn check_daily_spend_limit(&self, additional_wei: u128) -> Result<()> {
        self.reset_daily_spend_if_needed();
        let current = *self.daily_spend_wei.read().expect("lock poisoned");
        let limit_wei = self.max_daily_spend.to_wei();

        if current.saturating_add(additional_wei) > limit_wei {
            let spent_eth = current as f64 / 1e18;
            return Err(SafetyError::DailySpendLimitReached {
                spent_eth,
                limit_eth: self.max_daily_spend.as_f64(),
            }
            .into());
        }

        Ok(())
    }

    fn reset_daily_spend_if_needed(&self) {
        let mut started = self.spend_day_started.write().expect("lock poisoned");
        if started.elapsed() >= Duration::from_secs(86_400) {
            *started = Instant::now();
            *self.daily_spend_wei.write().expect("lock poisoned") = 0;
            info!("daily spend counter reset");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::opportunity::{OpportunityType, ScoredOpportunity};
    use crate::analysis::profitability::ProfitabilityAnalysis;
    use crate::analysis::risk_assessment::RiskAssessment;
    use crate::blockchain::mempool::PendingTransaction;
    use crate::blockchain::transaction::{ParsedTransaction, TransactionFeeModel};
    use crate::config::{
        AnalysisSettings, ExecutionSettings, GasSettings, LoggingSettings, NetworkSettings,
        ProfitSettings, SafetySettings, ServerSettings, Settings, WalletSettings,
    };
    use ethers::types::{Address, Bytes, Transaction, H256, U256};
    use std::time::Instant;

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
                max_consecutive_failures: 3,
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

    fn sample_opportunity(roi: f64) -> ScoredOpportunity {
        ScoredOpportunity {
            pending: PendingTransaction {
                parsed: ParsedTransaction {
                    hash: H256::repeat_byte(0x01),
                    from: Address::repeat_byte(0x02),
                    to: Some(Address::repeat_byte(0x03)),
                    value: U256::from(100_000_000_000_000_000u64),
                    gas_limit: U256::from(200_000),
                    gas_price: Some(U256::from(30_000_000_000u64)),
                    max_fee_per_gas: None,
                    max_priority_fee_per_gas: None,
                    input: Bytes::new(),
                    nonce: U256::zero(),
                    fee_model: TransactionFeeModel::Legacy,
                    interaction: None,
                },
                raw: Transaction::default(),
                received_at: Instant::now(),
            },
            profitability: ProfitabilityAnalysis {
                gross_profit_wei: U256::from(1_000_000_000_000_000u64),
                gas_cost_wei: U256::from(100_000_000_000_000u64),
                replacement_gas_cost_wei: U256::from(150_000_000_000_000u64),
                slippage_cost_wei: U256::zero(),
                net_profit_wei: U256::from(850_000_000_000_000u64),
                roi_percent: roi,
                detected_protocol: None,
            },
            risk: RiskAssessment {
                score: 30,
                congestion_component: 30,
                liquidity_component: 30,
                volatility_component: 30,
                history_component: 30,
                historical_success_rate: 0.8,
            },
            opportunity_type: OpportunityType::GasReplacement,
            priority_score: 50.0,
            detected_at: Instant::now(),
        }
    }

    #[test]
    fn blocks_when_circuit_breaker_open() {
        let guard = SafetyGuard::from_settings(&test_settings());
        guard.consecutive_failures.store(3, Ordering::Relaxed);

        assert!(guard
            .preflight_check(&sample_opportunity(20.0), 50.0)
            .is_err());
    }

    #[test]
    fn blocks_insufficient_roi() {
        let guard = SafetyGuard::from_settings(&test_settings());
        assert!(guard
            .preflight_check(&sample_opportunity(5.0), 50.0)
            .is_err());
    }

    #[test]
    fn allows_valid_opportunity() {
        let guard = SafetyGuard::from_settings(&test_settings());
        assert!(guard
            .preflight_check(&sample_opportunity(20.0), 50.0)
            .is_ok());
    }

    #[test]
    fn success_resets_circuit_breaker_counter() {
        let guard = SafetyGuard::from_settings(&test_settings());
        guard.record_failure();
        guard.record_failure();
        guard.record_success();
        assert_eq!(guard.consecutive_failures(), 0);
    }
}
