use crate::error::{BotError, ConfigError, Result};
use crate::types::{EthAmount, Gwei, Percent};
use config::{Config, Environment, File};
use serde::{Deserialize, Deserializer};
use std::path::Path;

const ENV_PREFIX: &str = "GAS_BOT";

fn deserialize_rpc_urls<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    use std::fmt;

    struct RpcUrlVisitor;

    impl<'de> Visitor<'de> for RpcUrlVisitor {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a URL string, comma-separated URLs, or JSON array of URLs")
        }

        fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            let trimmed = value.trim();
            if trimmed.starts_with('[') {
                serde_json::from_str(trimmed).map_err(de::Error::custom)
            } else if trimmed.contains(',') {
                Ok(trimmed
                    .split(',')
                    .map(str::trim)
                    .filter(|url| !url.is_empty())
                    .map(str::to_string)
                    .collect())
            } else {
                Ok(vec![trimmed.to_string()])
            }
        }

        fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            let mut urls = Vec::new();
            while let Some(url) = seq.next_element::<String>()? {
                urls.push(url);
            }
            Ok(urls)
        }
    }

    deserializer.deserialize_any(RpcUrlVisitor)
}

#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    pub network: NetworkSettings,
    pub gas: GasSettings,
    pub profit: ProfitSettings,
    pub safety: SafetySettings,
    pub logging: LoggingSettings,
    pub wallet: WalletSettings,
    pub analysis: AnalysisSettings,
    pub execution: ExecutionSettings,
    pub server: ServerSettings,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkSettings {
    pub chain_id: u64,
    #[serde(deserialize_with = "deserialize_rpc_urls")]
    pub rpc_urls: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GasSettings {
    pub max_gas_price_gwei: f64,
    pub min_gas_price_gwei: f64,
    pub replacement_bump_percent: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProfitSettings {
    pub min_profit_percent: f64,
    pub min_tx_value_eth: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SafetySettings {
    pub circuit_breaker_enabled: bool,
    pub max_consecutive_failures: u32,
    pub max_daily_spend_eth: f64,
    pub emergency_stop: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoggingSettings {
    pub level: String,
    pub log_file: String,
    pub json_log_enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WalletSettings {
    pub private_key: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnalysisSettings {
    pub slippage_bps: u32,
    pub max_risk_score: u8,
    pub base_liquidity_eth: f64,
    pub max_queue_size: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExecutionSettings {
    pub confirmation_timeout_secs: u64,
    pub replacement_poll_interval_secs: u64,
    pub soft_gas_cap_ratio: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerSettings {
    pub bind_address: String,
    pub worker_count: usize,
    pub network_cache_ttl_secs: u64,
    pub shutdown_drain_secs: u64,
}

impl Settings {
    pub fn load() -> Result<Self> {
        Self::load_from_sources().map_err(BotError::from)
    }

    fn load_from_sources() -> std::result::Result<Self, ConfigError> {
        dotenv::dotenv().ok();

        let config_path = Self::resolve_config_path();

        let mut builder = Config::builder()
            .set_default("network.chain_id", 1_i64)?
            .set_default(
                "network.rpc_urls",
                vec!["http://127.0.0.1:8545".to_string()],
            )?
            .set_default("gas.max_gas_price_gwei", 500.0)?
            .set_default("gas.min_gas_price_gwei", 1.0)?
            .set_default("gas.replacement_bump_percent", 15.0)?
            .set_default("profit.min_profit_percent", 15.0)?
            .set_default("profit.min_tx_value_eth", 0.1)?
            .set_default("safety.circuit_breaker_enabled", true)?
            .set_default("safety.max_consecutive_failures", 5_i64)?
            .set_default("safety.max_daily_spend_eth", 1.0)?
            .set_default("safety.emergency_stop", false)?
            .set_default("logging.level", "info")?
            .set_default("logging.log_file", "logs/gas-auction-bot.log")?
            .set_default("logging.json_log_enabled", true)?
            .set_default("wallet.private_key", "")?
            .set_default("analysis.slippage_bps", 50_i64)?
            .set_default("analysis.max_risk_score", 70_i64)?
            .set_default("analysis.base_liquidity_eth", 100.0)?
            .set_default("analysis.max_queue_size", 256_i64)?
            .set_default("execution.confirmation_timeout_secs", 60_i64)?
            .set_default("execution.replacement_poll_interval_secs", 5_i64)?
            .set_default("execution.soft_gas_cap_ratio", 0.8)?
            .set_default("server.bind_address", "0.0.0.0:9090")?
            .set_default("server.worker_count", 8_i64)?
            .set_default("server.network_cache_ttl_secs", 12_i64)?
            .set_default("server.shutdown_drain_secs", 10_i64)?;

        if config_path.exists() {
            builder = builder.add_source(File::from(config_path.as_path()));
        }

        // Environment variables override file and defaults (highest priority).
        builder = builder.add_source(
            Environment::with_prefix(ENV_PREFIX)
                .separator("__")
                .try_parsing(true),
        );

        let config = builder.build()?;

        let settings: Settings = config.try_deserialize()?;

        settings.validate()?;
        Ok(settings)
    }

    pub fn max_gas_price(&self) -> Gwei {
        Gwei::new(self.gas.max_gas_price_gwei)
    }

    pub fn min_gas_price(&self) -> Gwei {
        Gwei::new(self.gas.min_gas_price_gwei)
    }

    pub fn replacement_bump(&self) -> Percent {
        Percent::new(self.gas.replacement_bump_percent)
    }

    pub fn min_profit_threshold(&self) -> Percent {
        Percent::new(self.profit.min_profit_percent)
    }

    pub fn min_transaction_value(&self) -> EthAmount {
        EthAmount::new(self.profit.min_tx_value_eth)
    }

    pub fn max_daily_spend(&self) -> EthAmount {
        EthAmount::new(self.safety.max_daily_spend_eth)
    }

    fn resolve_config_path() -> std::path::PathBuf {
        if let Ok(path) = std::env::var(format!("{ENV_PREFIX}__CONFIG_PATH")) {
            return Path::new(&path).to_path_buf();
        }
        Path::new("config.toml").to_path_buf()
    }

    fn validate(&self) -> std::result::Result<(), ConfigError> {
        if self.network.rpc_urls.is_empty() {
            return Err(ConfigError::MissingRequired {
                key: "network.rpc_urls".into(),
            });
        }

        for (index, url) in self.network.rpc_urls.iter().enumerate() {
            if url.trim().is_empty() {
                return Err(ConfigError::InvalidValue {
                    key: format!("network.rpc_urls[{index}]"),
                    reason: "RPC URL must not be empty".into(),
                });
            }

            if !url.starts_with("http://")
                && !url.starts_with("https://")
                && !url.starts_with("ws://")
                && !url.starts_with("wss://")
            {
                return Err(ConfigError::InvalidValue {
                    key: format!("network.rpc_urls[{index}]"),
                    reason: "RPC URL must use http(s) or ws(s) scheme".into(),
                });
            }
        }

        if self.gas.min_gas_price_gwei >= self.gas.max_gas_price_gwei {
            return Err(ConfigError::InvalidValue {
                key: "gas.min_gas_price_gwei".into(),
                reason: "must be less than max_gas_price_gwei".into(),
            });
        }

        if self.gas.replacement_bump_percent <= 0.0 {
            return Err(ConfigError::InvalidValue {
                key: "gas.replacement_bump_percent".into(),
                reason: "must be positive".into(),
            });
        }

        if self.profit.min_profit_percent <= 0.0 {
            return Err(ConfigError::InvalidValue {
                key: "profit.min_profit_percent".into(),
                reason: "must be positive".into(),
            });
        }

        if self.profit.min_tx_value_eth < 0.0 {
            return Err(ConfigError::InvalidValue {
                key: "profit.min_tx_value_eth".into(),
                reason: "must be non-negative".into(),
            });
        }

        if self.safety.max_consecutive_failures == 0 {
            return Err(ConfigError::InvalidValue {
                key: "safety.max_consecutive_failures".into(),
                reason: "must be at least 1".into(),
            });
        }

        if self.safety.max_daily_spend_eth <= 0.0 {
            return Err(ConfigError::InvalidValue {
                key: "safety.max_daily_spend_eth".into(),
                reason: "must be positive".into(),
            });
        }

        if self.analysis.max_risk_score == 0 || self.analysis.max_risk_score > 100 {
            return Err(ConfigError::InvalidValue {
                key: "analysis.max_risk_score".into(),
                reason: "must be between 1 and 100".into(),
            });
        }

        if !(0.0..=1.0).contains(&self.execution.soft_gas_cap_ratio) {
            return Err(ConfigError::InvalidValue {
                key: "execution.soft_gas_cap_ratio".into(),
                reason: "must be between 0.0 and 1.0".into(),
            });
        }

        if self.server.worker_count == 0 {
            return Err(ConfigError::InvalidValue {
                key: "server.worker_count".into(),
                reason: "must be at least 1".into(),
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                level: "info".to_string(),
                log_file: "logs/test.log".to_string(),
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
                bind_address: "127.0.0.1:9090".to_string(),
                worker_count: 4,
                network_cache_ttl_secs: 12,
                shutdown_drain_secs: 10,
            },
        }
    }

    #[test]
    fn rejects_empty_rpc_urls() {
        let mut settings = test_settings();
        settings.network.rpc_urls.clear();
        assert!(settings.validate().is_err());
    }

    #[test]
    fn rejects_inverted_gas_bounds() {
        let mut settings = test_settings();
        settings.gas.min_gas_price_gwei = 600.0;
        assert!(settings.validate().is_err());
    }

    #[test]
    fn typed_accessors_return_wrappers() {
        let settings = test_settings();
        assert_eq!(settings.max_gas_price().as_f64(), 500.0);
        assert_eq!(settings.min_transaction_value().as_f64(), 0.1);
    }

    #[test]
    fn rpc_urls_parse_from_json_string() {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(deserialize_with = "super::deserialize_rpc_urls")]
            rpc_urls: Vec<String>,
        }

        let wrapper: Wrapper =
            serde_json::from_str(r#"{"rpc_urls":"[\"http://127.0.0.1:8545\"]"}"#).unwrap();
        assert_eq!(wrapper.rpc_urls, vec!["http://127.0.0.1:8545"]);
    }

    #[test]
    fn rpc_urls_parse_from_comma_separated_string() {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(deserialize_with = "super::deserialize_rpc_urls")]
            rpc_urls: Vec<String>,
        }

        let wrapper: Wrapper =
            serde_json::from_str(r#"{"rpc_urls":"http://127.0.0.1:8545,https://rpc.example.com"}"#)
                .unwrap();
        assert_eq!(wrapper.rpc_urls.len(), 2);
    }

    #[test]
    fn rejects_zero_daily_spend_limit() {
        let mut settings = test_settings();
        settings.safety.max_daily_spend_eth = 0.0;
        assert!(settings.validate().is_err());
    }

    #[test]
    fn rejects_invalid_rpc_scheme() {
        let mut settings = test_settings();
        settings.network.rpc_urls = vec!["ftp://example.com".into()];
        assert!(settings.validate().is_err());
    }

    #[test]
    fn rejects_invalid_soft_gas_cap_ratio() {
        let mut settings = test_settings();
        settings.execution.soft_gas_cap_ratio = 1.5;
        assert!(settings.validate().is_err());
    }
}
