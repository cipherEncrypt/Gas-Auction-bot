use thiserror::Error;

#[derive(Debug, Error)]
pub enum NetworkError {
    #[error("RPC connection failed to {endpoint}: {source}")]
    ConnectionFailed {
        endpoint: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("all {attempt_count} RPC providers exhausted")]
    AllProvidersFailed { attempt_count: usize },

    #[error("subscription to {channel} dropped: {reason}")]
    SubscriptionDropped { channel: String, reason: String },

    #[error("invalid chain ID: expected {expected}, got {actual}")]
    ChainIdMismatch { expected: u64, actual: u64 },

    #[error("request timed out after {timeout_ms}ms")]
    RequestTimeout { timeout_ms: u64 },
}

#[derive(Debug, Error)]
pub enum AnalysisError {
    #[error("unable to decode transaction input: {reason}")]
    DecodeFailed { reason: String },

    #[error("insufficient liquidity for token {token_address}")]
    InsufficientLiquidity { token_address: String },

    #[error("profit calculation overflow for transaction {tx_hash}")]
    ProfitOverflow { tx_hash: String },

    #[error("unsupported DEX protocol: {protocol}")]
    UnsupportedProtocol { protocol: String },

    #[error("missing price data for pair {pair}")]
    MissingPriceData { pair: String },
}

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("transaction signing failed: {reason}")]
    SigningFailed { reason: String },

    #[error("replacement rejected by node: {reason}")]
    ReplacementRejected { reason: String },

    #[error("nonce conflict: expected {expected}, account has {actual}")]
    NonceConflict { expected: u64, actual: u64 },

    #[error("transaction {tx_hash} dropped from mempool after {wait_secs}s")]
    TransactionDropped { tx_hash: String, wait_secs: u64 },

    #[error("gas price {offered_gwei} gwei exceeds hard cap {cap_gwei} gwei")]
    GasPriceExceeded { offered_gwei: f64, cap_gwei: f64 },
}

impl From<config::ConfigError> for ConfigError {
    fn from(source: config::ConfigError) -> Self {
        Self::ParseFailed { source }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration file not found at {path}")]
    FileNotFound { path: String },

    #[error("failed to parse configuration: {source}")]
    ParseFailed {
        #[source]
        source: config::ConfigError,
    },

    #[error("missing required setting: {key}")]
    MissingRequired { key: String },

    #[error("invalid value for {key}: {reason}")]
    InvalidValue { key: String, reason: String },
}

#[derive(Debug, Error)]
pub enum SafetyError {
    #[error("circuit breaker open after {consecutive_failures} consecutive failures")]
    CircuitBreakerOpen { consecutive_failures: u32 },

    #[error("emergency stop is active")]
    EmergencyStopActive,

    #[error("daily spend limit reached: {spent_eth} ETH of {limit_eth} ETH")]
    DailySpendLimitReached { spent_eth: f64, limit_eth: f64 },

    #[error("profit below minimum ROI: {actual_percent}% < {required_percent}%")]
    InsufficientRoi {
        actual_percent: f64,
        required_percent: f64,
    },

    #[error("soft gas cap exceeded: {current_gwei} gwei > {soft_cap_gwei} gwei")]
    SoftGasCapExceeded {
        current_gwei: f64,
        soft_cap_gwei: f64,
    },
}

#[derive(Debug, Error)]
pub enum BotError {
    #[error(transparent)]
    Network(#[from] NetworkError),

    #[error(transparent)]
    Analysis(#[from] AnalysisError),

    #[error(transparent)]
    Execution(#[from] ExecutionError),

    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(transparent)]
    Safety(#[from] SafetyError),
}

pub type Result<T> = std::result::Result<T, BotError>;
