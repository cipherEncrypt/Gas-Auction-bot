pub mod opportunity;
pub mod profitability;
pub mod risk_assessment;

pub use opportunity::{OpportunityDetector, OpportunityType, ScoredOpportunity};
pub use profitability::{
    analyze_transaction_profitability, fetch_network_state, infer_dex_quotes, DexProtocol,
    DexQuote, NetworkState, ProfitabilityAnalysis,
};
pub use risk_assessment::{assess_transaction_risk, RiskAssessment, SuccessRateTracker};
