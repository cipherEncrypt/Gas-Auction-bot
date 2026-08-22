//! Gas Auction Bot — monitors mempool transactions and submits gas-competitive replacements.

pub mod analysis;
pub mod blockchain;
pub mod config;
pub mod error;
pub mod execution;
pub mod metrics;
pub mod runtime;
pub mod types;
pub mod utils;

pub use config::Settings;
pub use error::{BotError, Result};
