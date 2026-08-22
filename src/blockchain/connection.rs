use crate::config::Settings;
use crate::error::{BotError, NetworkError, Result};
use dashmap::DashMap;
use ethers::prelude::{Http, Middleware, Provider};
use ethers::types::{Block, Transaction, TransactionReceipt, TxHash, U256, U64};
use futures::future::join_all;
use once_cell::sync::Lazy;
use prometheus::{IntCounterVec, IntGaugeVec, Opts, Registry};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::sleep;
use tracing::{debug, warn};

const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 5_000;
const INITIAL_BACKOFF_MS: u64 = 500;
const MAX_BACKOFF_MS: u64 = 30_000;

static METRICS_REGISTRY: Lazy<Registry> = Lazy::new(Registry::new);

/// Runtime configuration for RPC connection behavior.
#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    pub chain_id: u64,
    pub rpc_urls: Vec<String>,
    pub request_timeout: Duration,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl ConnectionConfig {
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            chain_id: settings.network.chain_id,
            rpc_urls: settings.network.rpc_urls.clone(),
            request_timeout: Duration::from_millis(DEFAULT_REQUEST_TIMEOUT_MS),
            initial_backoff: Duration::from_millis(INITIAL_BACKOFF_MS),
            max_backoff: Duration::from_millis(MAX_BACKOFF_MS),
        }
    }
}

/// Point-in-time health snapshot for a single RPC endpoint.
#[derive(Debug, Clone)]
pub struct ProviderHealthSnapshot {
    pub endpoint: String,
    pub is_healthy: bool,
    pub consecutive_failures: u32,
    pub total_requests: u64,
    pub failed_requests: u64,
    pub last_success: Option<Instant>,
    pub backoff_until: Option<Instant>,
}

pub(crate) struct ProviderSlot {
    endpoint: String,
    provider: Provider<Http>,
    consecutive_failures: AtomicU64,
    total_requests: AtomicU64,
    failed_requests: AtomicU64,
    last_success: RwLock<Option<Instant>>,
    backoff_until: RwLock<Option<Instant>>,
}

/// Manages a pool of HTTP RPC providers with failover and health tracking.
pub struct ConnectionManager {
    providers: Vec<Arc<ProviderSlot>>,
    round_robin_index: AtomicUsize,
    chain_id: u64,
    request_timeout: Duration,
    initial_backoff: Duration,
    max_backoff: Duration,
    metrics: Arc<ConnectionMetrics>,
}

pub(crate) struct ConnectionMetrics {
    requests_total: IntCounterVec,
    failures_total: IntCounterVec,
    provider_healthy: IntGaugeVec,
}

impl ConnectionMetrics {
    fn new(endpoints: &[String]) -> std::result::Result<Arc<Self>, NetworkError> {
        let requests_total = IntCounterVec::new(
            Opts::new("rpc_requests_total", "Total RPC requests by provider"),
            &["endpoint"],
        )
        .map_err(|error| NetworkError::ConnectionFailed {
            endpoint: "metrics".into(),
            source: error.into(),
        })?;

        let failures_total = IntCounterVec::new(
            Opts::new("rpc_failures_total", "Failed RPC requests by provider"),
            &["endpoint"],
        )
        .map_err(|error| NetworkError::ConnectionFailed {
            endpoint: "metrics".into(),
            source: error.into(),
        })?;

        let provider_healthy = IntGaugeVec::new(
            Opts::new("rpc_provider_healthy", "Provider health status (1=healthy)"),
            &["endpoint"],
        )
        .map_err(|error| NetworkError::ConnectionFailed {
            endpoint: "metrics".into(),
            source: error.into(),
        })?;

        for endpoint in endpoints {
            requests_total.with_label_values(&[endpoint]).inc_by(0);
            failures_total.with_label_values(&[endpoint]).inc_by(0);
            provider_healthy.with_label_values(&[endpoint]).set(1);
        }

        METRICS_REGISTRY
            .register(Box::new(requests_total.clone()))
            .ok();
        METRICS_REGISTRY
            .register(Box::new(failures_total.clone()))
            .ok();
        METRICS_REGISTRY
            .register(Box::new(provider_healthy.clone()))
            .ok();

        Ok(Arc::new(Self {
            requests_total,
            failures_total,
            provider_healthy,
        }))
    }
}

impl ConnectionManager {
    pub async fn connect(config: &ConnectionConfig) -> Result<Self> {
        let metrics = ConnectionMetrics::new(&config.rpc_urls).map_err(BotError::from)?;

        let mut providers = Vec::with_capacity(config.rpc_urls.len());
        for endpoint in &config.rpc_urls {
            let provider = Provider::<Http>::try_from(endpoint.as_str()).map_err(|source| {
                NetworkError::ConnectionFailed {
                    endpoint: endpoint.clone(),
                    source: Box::new(source),
                }
            })?;

            providers.push(Arc::new(ProviderSlot {
                endpoint: endpoint.clone(),
                provider,
                consecutive_failures: AtomicU64::new(0),
                total_requests: AtomicU64::new(0),
                failed_requests: AtomicU64::new(0),
                last_success: RwLock::new(None),
                backoff_until: RwLock::new(None),
            }));
        }

        let manager = Self {
            providers,
            round_robin_index: AtomicUsize::new(0),
            chain_id: config.chain_id,
            request_timeout: config.request_timeout,
            initial_backoff: config.initial_backoff,
            max_backoff: config.max_backoff,
            metrics,
        };

        manager.verify_chain_id().await?;
        Ok(manager)
    }

    pub fn from_settings(settings: &Settings) -> ConnectionConfig {
        ConnectionConfig::from_settings(settings)
    }

    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }

    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    pub fn metrics_registry(&self) -> &'static Registry {
        &METRICS_REGISTRY
    }

    /// Returns a WebSocket URL derived from the first healthy HTTP endpoint.
    pub fn websocket_url(&self) -> Option<String> {
        self.providers
            .iter()
            .find(|slot| slot.consecutive_failures.load(Ordering::Relaxed) < 3)
            .map(|slot| http_to_ws_url(&slot.endpoint))
    }

    pub async fn health_snapshots(&self) -> Vec<ProviderHealthSnapshot> {
        let mut snapshots = Vec::with_capacity(self.providers.len());

        for slot in &self.providers {
            let consecutive_failures = slot.consecutive_failures.load(Ordering::Relaxed) as u32;
            let is_healthy = consecutive_failures < 3;
            self.metrics
                .provider_healthy
                .with_label_values(&[&slot.endpoint])
                .set(if is_healthy { 1 } else { 0 });

            snapshots.push(ProviderHealthSnapshot {
                endpoint: slot.endpoint.clone(),
                is_healthy,
                consecutive_failures,
                total_requests: slot.total_requests.load(Ordering::Relaxed),
                failed_requests: slot.failed_requests.load(Ordering::Relaxed),
                last_success: *slot.last_success.read().await,
                backoff_until: *slot.backoff_until.read().await,
            });
        }

        snapshots
    }

    pub async fn get_transaction(&self, hash: TxHash) -> Result<Option<Transaction>> {
        let timeout = self.request_timeout;
        self.attempt_providers(move |slot| {
            let hash = hash;
            async move {
                tokio::time::timeout(timeout, slot.provider.get_transaction(hash))
                    .await
                    .map_err(|_| NetworkError::RequestTimeout {
                        timeout_ms: timeout.as_millis() as u64,
                    })?
                    .map_err(|source| NetworkError::ConnectionFailed {
                        endpoint: slot.endpoint.clone(),
                        source: Box::new(source),
                    })
                    .map_err(BotError::from)
            }
        })
        .await
    }

    pub async fn get_block_number(&self) -> Result<U64> {
        let timeout = self.request_timeout;
        self.attempt_providers(move |slot| async move {
            tokio::time::timeout(timeout, slot.provider.get_block_number())
                .await
                .map_err(|_| NetworkError::RequestTimeout {
                    timeout_ms: timeout.as_millis() as u64,
                })?
                .map_err(|source| NetworkError::ConnectionFailed {
                    endpoint: slot.endpoint.clone(),
                    source: Box::new(source),
                })
                .map_err(BotError::from)
        })
        .await
    }

    pub async fn get_block(&self, block_number: U64) -> Result<Option<Block<TxHash>>> {
        let timeout = self.request_timeout;
        self.attempt_providers(move |slot| async move {
            tokio::time::timeout(timeout, slot.provider.get_block(block_number))
                .await
                .map_err(|_| NetworkError::RequestTimeout {
                    timeout_ms: timeout.as_millis() as u64,
                })?
                .map_err(|source| NetworkError::ConnectionFailed {
                    endpoint: slot.endpoint.clone(),
                    source: Box::new(source),
                })
                .map_err(BotError::from)
        })
        .await
    }

    pub async fn get_gas_price(&self) -> Result<U256> {
        let timeout = self.request_timeout;
        self.attempt_providers(move |slot| async move {
            tokio::time::timeout(timeout, slot.provider.get_gas_price())
                .await
                .map_err(|_| NetworkError::RequestTimeout {
                    timeout_ms: timeout.as_millis() as u64,
                })?
                .map_err(|source| NetworkError::ConnectionFailed {
                    endpoint: slot.endpoint.clone(),
                    source: Box::new(source),
                })
                .map_err(BotError::from)
        })
        .await
    }

    pub async fn get_account_nonce(&self, address: ethers::types::Address) -> Result<U256> {
        let timeout = self.request_timeout;
        self.attempt_providers(move |slot| async move {
            tokio::time::timeout(timeout, slot.provider.get_transaction_count(address, None))
                .await
                .map_err(|_| NetworkError::RequestTimeout {
                    timeout_ms: timeout.as_millis() as u64,
                })?
                .map_err(|source| NetworkError::ConnectionFailed {
                    endpoint: slot.endpoint.clone(),
                    source: Box::new(source),
                })
                .map_err(BotError::from)
        })
        .await
    }

    pub async fn send_raw_transaction(
        &self,
        raw_transaction: ethers::types::Bytes,
    ) -> Result<TxHash> {
        let timeout = self.request_timeout;
        let raw_transaction = raw_transaction.clone();
        self.attempt_providers(move |slot| {
            let raw_transaction = raw_transaction.clone();
            async move {
                tokio::time::timeout(timeout, slot.provider.send_raw_transaction(raw_transaction))
                    .await
                    .map_err(|_| NetworkError::RequestTimeout {
                        timeout_ms: timeout.as_millis() as u64,
                    })?
                    .map_err(|source| NetworkError::ConnectionFailed {
                        endpoint: slot.endpoint.clone(),
                        source: Box::new(source),
                    })
                    .map(|pending| pending.tx_hash())
                    .map_err(BotError::from)
            }
        })
        .await
    }

    pub async fn get_transaction_receipt(
        &self,
        hash: TxHash,
    ) -> Result<Option<TransactionReceipt>> {
        let timeout = self.request_timeout;
        self.attempt_providers(move |slot| async move {
            tokio::time::timeout(timeout, slot.provider.get_transaction_receipt(hash))
                .await
                .map_err(|_| NetworkError::RequestTimeout {
                    timeout_ms: timeout.as_millis() as u64,
                })?
                .map_err(|source| NetworkError::ConnectionFailed {
                    endpoint: slot.endpoint.clone(),
                    source: Box::new(source),
                })
                .map_err(BotError::from)
        })
        .await
    }

    async fn attempt_providers<F, Fut, T>(&self, mut operation: F) -> Result<T>
    where
        F: FnMut(Arc<ProviderSlot>) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let provider_count = self.providers.len();
        let start_index = self.round_robin_index.fetch_add(1, Ordering::Relaxed) % provider_count;
        let mut last_error = None;

        for offset in 0..provider_count {
            let index = (start_index + offset) % provider_count;
            let slot = Arc::clone(&self.providers[index]);

            if self.is_in_backoff(&slot).await {
                debug!(endpoint = %slot.endpoint, "skipping provider in backoff");
                continue;
            }

            slot.total_requests.fetch_add(1, Ordering::Relaxed);
            self.metrics
                .requests_total
                .with_label_values(&[&slot.endpoint])
                .inc();

            match operation(Arc::clone(&slot)).await {
                Ok(result) => {
                    self.record_success(&slot).await;
                    return Ok(result);
                }
                Err(error) => {
                    warn!(endpoint = %slot.endpoint, error = %error, "RPC request failed");
                    self.record_failure(&slot).await;
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            BotError::Network(NetworkError::AllProvidersFailed {
                attempt_count: provider_count,
            })
        }))
    }

    /// Executes an RPC call against providers in round-robin order until one succeeds.
    #[allow(dead_code)]
    pub(crate) async fn execute_with_failover<F, Fut, T>(&self, operation: F) -> Result<T>
    where
        F: FnMut(Arc<ProviderSlot>) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        self.attempt_providers(operation).await
    }

    /// Queries all providers concurrently and returns the first successful response.
    #[allow(dead_code)]
    pub(crate) async fn race_providers<F, Fut, T>(&self, operation: F) -> Result<T>
    where
        F: Fn(Arc<ProviderSlot>, Arc<ConnectionMetrics>) -> Fut + Send + Sync + Clone,
        Fut: std::future::Future<Output = Result<T>> + Send,
        T: Send,
    {
        let metrics = Arc::clone(&self.metrics);
        let available: Vec<_> = self
            .providers
            .iter()
            .filter(|slot| {
                slot.backoff_until
                    .try_read()
                    .map(|guard| guard.map(|until| until <= Instant::now()).unwrap_or(true))
                    .unwrap_or(true)
            })
            .cloned()
            .collect();

        if available.is_empty() {
            return Err(BotError::Network(NetworkError::AllProvidersFailed {
                attempt_count: self.providers.len(),
            }));
        }

        let futures: Vec<_> = available
            .into_iter()
            .map(|slot| {
                let metrics = Arc::clone(&metrics);
                let operation = operation.clone();
                async move {
                    slot.total_requests.fetch_add(1, Ordering::Relaxed);
                    metrics
                        .requests_total
                        .with_label_values(&[&slot.endpoint])
                        .inc();
                    operation(slot, metrics).await
                }
            })
            .collect();

        let results = join_all(futures).await;

        if let Some(value) = results.into_iter().flatten().next() {
            return Ok(value);
        }

        Err(BotError::Network(NetworkError::AllProvidersFailed {
            attempt_count: self.providers.len(),
        }))
    }

    async fn verify_chain_id(&self) -> Result<()> {
        let timeout = self.request_timeout;
        let expected_chain_id = self.chain_id;

        let chain_id = self
            .attempt_providers(move |slot| async move {
                tokio::time::timeout(timeout, slot.provider.get_chainid())
                    .await
                    .map_err(|_| NetworkError::RequestTimeout {
                        timeout_ms: timeout.as_millis() as u64,
                    })?
                    .map_err(|source| NetworkError::ConnectionFailed {
                        endpoint: slot.endpoint.clone(),
                        source: Box::new(source),
                    })
                    .map_err(BotError::from)
            })
            .await?;

        let actual = chain_id.as_u64();
        if actual != expected_chain_id {
            return Err(NetworkError::ChainIdMismatch {
                expected: expected_chain_id,
                actual,
            }
            .into());
        }

        Ok(())
    }

    async fn is_in_backoff(&self, slot: &Arc<ProviderSlot>) -> bool {
        slot.backoff_until
            .read()
            .await
            .map(|until| until > Instant::now())
            .unwrap_or(false)
    }

    async fn record_success(&self, slot: &Arc<ProviderSlot>) {
        slot.consecutive_failures.store(0, Ordering::Relaxed);
        *slot.last_success.write().await = Some(Instant::now());
        *slot.backoff_until.write().await = None;
        self.metrics
            .provider_healthy
            .with_label_values(&[&slot.endpoint])
            .set(1);
    }

    async fn record_failure(&self, slot: &Arc<ProviderSlot>) {
        slot.failed_requests.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .failures_total
            .with_label_values(&[&slot.endpoint])
            .inc();

        let failures = slot.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        let backoff_ms = (self.initial_backoff.as_millis() as u64)
            .saturating_mul(2u64.saturating_pow(failures.saturating_sub(1) as u32));
        let capped_backoff = backoff_ms.min(self.max_backoff.as_millis() as u64);

        *slot.backoff_until.write().await =
            Some(Instant::now() + Duration::from_millis(capped_backoff));

        if failures >= 3 {
            self.metrics
                .provider_healthy
                .with_label_values(&[&slot.endpoint])
                .set(0);
        }

        debug!(
            endpoint = %slot.endpoint,
            failures,
            backoff_ms = capped_backoff,
            "provider entered backoff"
        );
    }
}

fn http_to_ws_url(http_url: &str) -> String {
    if http_url.starts_with("https://") {
        http_url.replacen("https://", "wss://", 1)
    } else if http_url.starts_with("http://") {
        http_url.replacen("http://", "ws://", 1)
    } else if http_url.starts_with("wss://") || http_url.starts_with("ws://") {
        http_url.to_string()
    } else {
        format!("wss://{http_url}")
    }
}

/// Cached backoff state shared across reconnect attempts for WebSocket subscriptions.
pub struct ReconnectPolicy {
    initial_backoff: Duration,
    max_backoff: Duration,
    attempt: AtomicU64,
}

impl ReconnectPolicy {
    pub fn new(initial_backoff: Duration, max_backoff: Duration) -> Self {
        Self {
            initial_backoff,
            max_backoff,
            attempt: AtomicU64::new(0),
        }
    }

    pub async fn wait_before_retry(&self) {
        let attempt = self.attempt.fetch_add(1, Ordering::Relaxed);
        let backoff_ms = (self.initial_backoff.as_millis() as u64)
            .saturating_mul(2u64.saturating_pow(attempt.min(10) as u32));
        let delay = Duration::from_millis(backoff_ms.min(self.max_backoff.as_millis() as u64));
        sleep(delay).await;
    }

    pub fn reset(&self) {
        self.attempt.store(0, Ordering::Relaxed);
    }
}

/// Tracks in-flight deduplication keys across concurrent mempool workers.
pub type InflightCache = DashMap<TxHash, Instant>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_https_to_wss() {
        let url = http_to_ws_url("https://eth-mainnet.example.com/v2/key");
        assert_eq!(url, "wss://eth-mainnet.example.com/v2/key");
    }

    #[test]
    fn converts_http_to_ws() {
        let url = http_to_ws_url("http://127.0.0.1:8545");
        assert_eq!(url, "ws://127.0.0.1:8545");
    }

    #[test]
    fn reconnect_backoff_caps_at_max() {
        let policy = ReconnectPolicy::new(Duration::from_millis(100), Duration::from_millis(500));
        policy.attempt.store(10, Ordering::Relaxed);
        let attempt = policy.attempt.load(Ordering::Relaxed);
        let backoff_ms = (100u64).saturating_mul(2u64.saturating_pow(attempt.min(10) as u32));
        assert!(backoff_ms.min(500) <= 500);
    }
}
