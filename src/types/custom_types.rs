use serde::{Deserialize, Serialize};
use std::fmt;

/// Gas price denominated in gwei for human-readable configuration.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Gwei(f64);

impl Gwei {
    pub fn new(value: f64) -> Self {
        Self(value)
    }

    pub fn as_f64(self) -> f64 {
        self.0
    }

    pub fn to_wei(self) -> u128 {
        (self.0 * 1_000_000_000.0) as u128
    }
}

impl fmt::Display for Gwei {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:.2} gwei", self.0)
    }
}

/// ETH-denominated value for profit thresholds and spend limits.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct EthAmount(f64);

impl EthAmount {
    pub fn new(value: f64) -> Self {
        Self(value)
    }

    pub fn as_f64(self) -> f64 {
        self.0
    }

    pub fn to_wei(self) -> u128 {
        (self.0 * 1e18) as u128
    }
}

impl fmt::Display for EthAmount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:.6} ETH", self.0)
    }
}

/// Percentage value used for profit margins and gas bumps.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Percent(f64);

impl Percent {
    pub fn new(value: f64) -> Self {
        Self(value)
    }

    pub fn as_f64(self) -> f64 {
        self.0
    }

    pub fn apply_to(self, base: f64) -> f64 {
        base * (1.0 + self.0 / 100.0)
    }
}

impl fmt::Display for Percent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:.2}%", self.0)
    }
}

/// Correlation identifier attached to log spans for request tracing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TraceId(String);

impl TraceId {
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TraceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl Default for TraceId {
    fn default() -> Self {
        Self::generate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gwei_converts_to_wei_correctly() {
        let gas_price = Gwei::new(50.0);
        assert_eq!(gas_price.to_wei(), 50_000_000_000);
    }

    #[test]
    fn percent_bump_applies_correctly() {
        let bump = Percent::new(15.0);
        assert!((bump.apply_to(100.0) - 115.0).abs() < 1e-9);
    }

    #[test]
    fn eth_amount_converts_to_wei() {
        let amount = EthAmount::new(1.0);
        assert_eq!(amount.to_wei(), 1_000_000_000_000_000_000);
    }
}
