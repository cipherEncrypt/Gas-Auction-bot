use crate::analysis::profitability::{
    analyze_transaction_profitability, infer_dex_quotes, DexQuote, NetworkState,
    ProfitabilityAnalysis,
};
use crate::analysis::risk_assessment::{
    assess_transaction_risk, RiskAssessment, SuccessRateTracker,
};
use crate::blockchain::mempool::PendingTransaction;
use crate::blockchain::transaction::ContractInteraction;
use crate::config::Settings;
use crate::types::Percent;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::time::Instant;

/// Classification of the detected MEV opportunity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpportunityType {
    Arbitrage,
    Sandwich,
    GasReplacement,
}

impl std::fmt::Display for OpportunityType {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Arbitrage => write!(formatter, "arbitrage"),
            Self::Sandwich => write!(formatter, "sandwich"),
            Self::GasReplacement => write!(formatter, "gas_replacement"),
        }
    }
}

/// Fully scored opportunity ready for the execution layer.
#[derive(Debug, Clone)]
pub struct ScoredOpportunity {
    pub pending: PendingTransaction,
    pub profitability: ProfitabilityAnalysis,
    pub risk: RiskAssessment,
    pub opportunity_type: OpportunityType,
    pub priority_score: f64,
    pub detected_at: Instant,
}

impl ScoredOpportunity {
    pub fn hash(&self) -> ethers::types::TxHash {
        self.pending.hash()
    }

    pub fn net_profit_eth(&self) -> f64 {
        self.profitability.net_profit_eth()
    }
}

impl PartialEq for ScoredOpportunity {
    fn eq(&self, other: &Self) -> bool {
        self.priority_score == other.priority_score
    }
}

impl Eq for ScoredOpportunity {}

impl PartialOrd for ScoredOpportunity {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredOpportunity {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority_score
            .partial_cmp(&other.priority_score)
            .unwrap_or(Ordering::Equal)
    }
}

/// Detects, scores, and ranks opportunities from the mempool stream.
pub struct OpportunityDetector {
    min_profit_threshold: Percent,
    replacement_bump: Percent,
    max_risk_score: u8,
    slippage_bps: u32,
    base_liquidity_eth: f64,
    success_tracker: SuccessRateTracker,
    priority_queue: BinaryHeap<ScoredOpportunity>,
    max_queue_size: usize,
}

impl OpportunityDetector {
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            min_profit_threshold: settings.min_profit_threshold(),
            replacement_bump: settings.replacement_bump(),
            max_risk_score: settings.analysis.max_risk_score,
            slippage_bps: settings.analysis.slippage_bps,
            base_liquidity_eth: settings.analysis.base_liquidity_eth,
            success_tracker: SuccessRateTracker::default(),
            priority_queue: BinaryHeap::new(),
            max_queue_size: settings.analysis.max_queue_size,
        }
    }

    pub fn evaluate(
        &mut self,
        pending: PendingTransaction,
        network_state: &NetworkState,
    ) -> Option<ScoredOpportunity> {
        let dex_quotes = infer_dex_quotes(&pending.parsed, self.base_liquidity_eth);
        let opportunity_type = classify_opportunity(&pending, &dex_quotes);

        let profitability = analyze_transaction_profitability(
            &pending.parsed,
            network_state,
            self.replacement_bump,
            self.slippage_bps,
            &dex_quotes,
        )
        .ok()?;

        if !profitability.is_profitable(self.min_profit_threshold) {
            return None;
        }

        let risk = assess_transaction_risk(
            &pending.parsed,
            network_state,
            &dex_quotes,
            &self.success_tracker,
        );

        if !risk.is_acceptable(self.max_risk_score) {
            return None;
        }

        let priority_score =
            risk.risk_adjusted_roi(profitability.roi_percent) * opportunity_type.weight();

        let scored = ScoredOpportunity {
            pending,
            profitability,
            risk,
            opportunity_type,
            priority_score,
            detected_at: Instant::now(),
        };

        self.enqueue(scored.clone());
        Some(scored)
    }

    pub fn next_opportunity(&mut self) -> Option<ScoredOpportunity> {
        self.priority_queue.pop()
    }

    pub fn queue_depth(&self) -> usize {
        self.priority_queue.len()
    }

    pub fn record_execution_outcome(&self, contract: &str, succeeded: bool) {
        self.success_tracker.record_outcome(contract, succeeded);
    }

    fn enqueue(&mut self, opportunity: ScoredOpportunity) {
        if self.priority_queue.len() >= self.max_queue_size {
            if let Some(lowest) = self.priority_queue.peek() {
                if opportunity.priority_score <= lowest.priority_score {
                    return;
                }
            }
            self.priority_queue.pop();
        }
        self.priority_queue.push(opportunity);
    }
}

impl OpportunityType {
    fn weight(self) -> f64 {
        match self {
            Self::Arbitrage => 1.2,
            Self::Sandwich => 1.0,
            Self::GasReplacement => 0.8,
        }
    }
}

fn classify_opportunity(pending: &PendingTransaction, dex_quotes: &[DexQuote]) -> OpportunityType {
    match &pending.parsed.interaction {
        Some(ContractInteraction::ContractCall {
            signature: Some(method),
            ..
        }) if method.contains("swap") || method.contains("exactInput") => {
            if dex_quotes.len() >= 2 && has_price_discrepancy(dex_quotes) {
                OpportunityType::Arbitrage
            } else {
                OpportunityType::Sandwich
            }
        }
        _ => OpportunityType::GasReplacement,
    }
}

fn has_price_discrepancy(quotes: &[DexQuote]) -> bool {
    if quotes.len() < 2 {
        return false;
    }

    let max_price = quotes.iter().map(|q| q.price).fold(0.0f64, f64::max);
    let min_price = quotes.iter().map(|q| q.price).fold(f64::INFINITY, f64::min);

    if max_price <= f64::EPSILON {
        return false;
    }

    (max_price - min_price) / max_price > 0.003
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockchain::transaction::{ParsedTransaction, TransactionFeeModel};
    use crate::config::settings::{
        AnalysisSettings, ExecutionSettings, GasSettings, LoggingSettings, NetworkSettings,
        ProfitSettings, SafetySettings, ServerSettings, WalletSettings,
    };
    use crate::config::Settings;
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
                min_profit_percent: 1.0,
                min_tx_value_eth: 0.01,
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
                max_risk_score: 90,
                base_liquidity_eth: 100.0,
                max_queue_size: 100,
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

    fn pending_swap(value_eth: f64) -> PendingTransaction {
        PendingTransaction {
            parsed: ParsedTransaction {
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
                interaction: Some(ContractInteraction::ContractCall {
                    selector: [0x38, 0xed, 0x17, 0x39],
                    signature: Some(
                        "swapExactTokensForTokens(uint256,uint256,address[],address,uint256)",
                    ),
                }),
            },
            raw: Transaction {
                hash: H256::repeat_byte(0x01),
                ..Default::default()
            },
            received_at: Instant::now(),
        }
    }

    fn network_state() -> NetworkState {
        NetworkState {
            base_fee_per_gas: U256::from(20_000_000_000u64),
            suggested_priority_fee: U256::from(2_000_000_000u64),
            block_gas_used_ratio: 0.5,
            block_number: 1,
        }
    }

    #[test]
    fn detects_opportunity_on_large_swap() {
        let mut detector = OpportunityDetector::from_settings(&test_settings());
        let opportunity = detector
            .evaluate(pending_swap(10.0), &network_state())
            .expect("should detect opportunity");

        assert_eq!(opportunity.opportunity_type, OpportunityType::Arbitrage);
        assert!(opportunity.priority_score > 0.0);
    }

    #[test]
    fn priority_queue_returns_highest_score_first() {
        let mut detector = OpportunityDetector::from_settings(&test_settings());

        detector.evaluate(pending_swap(5.0), &network_state());
        detector.evaluate(pending_swap(20.0), &network_state());

        let best = detector.next_opportunity().expect("queue not empty");
        let second = detector.next_opportunity().expect("queue has second entry");

        assert!(best.priority_score >= second.priority_score);
    }

    #[test]
    fn rejects_unprofitable_transactions() {
        let mut settings = test_settings();
        settings.profit.min_profit_percent = 99_999.0;
        let mut detector = OpportunityDetector::from_settings(&settings);

        assert!(detector
            .evaluate(pending_swap(0.001), &network_state())
            .is_none());
    }
}
