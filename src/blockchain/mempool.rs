use crate::blockchain::connection::{ConnectionManager, ReconnectPolicy};
use crate::blockchain::transaction::{
    gwei_to_wei, parse_transaction, wei_to_eth, ParsedTransaction,
};
use crate::config::Settings;
use crate::error::{NetworkError, Result};
use ethers::providers::{Middleware, Provider, StreamExt, Ws};
use ethers::types::{Transaction, TxHash, U256};
use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

const DEFAULT_CACHE_CAPACITY: usize = 10_000;
const CACHE_ENTRY_TTL: Duration = Duration::from_secs(300);
const CHANNEL_BUFFER: usize = 2_048;

/// Thresholds applied before forwarding a transaction to the analysis pipeline.
#[derive(Debug, Clone)]
pub struct MempoolFilter {
    pub min_value_wei: U256,
    pub min_gas_price_wei: U256,
    pub max_gas_price_wei: U256,
}

impl MempoolFilter {
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            min_value_wei: U256::from(settings.min_transaction_value().to_wei()),
            min_gas_price_wei: gwei_to_wei(settings.min_gas_price()),
            max_gas_price_wei: gwei_to_wei(settings.max_gas_price()),
        }
    }

    pub fn passes(&self, transaction: &ParsedTransaction) -> bool {
        if transaction.value < self.min_value_wei {
            return false;
        }

        let effective_gas = transaction.effective_gas_price();
        effective_gas >= self.min_gas_price_wei && effective_gas <= self.max_gas_price_wei
    }
}

/// A pending transaction that cleared mempool filtering.
#[derive(Debug, Clone)]
pub struct PendingTransaction {
    pub parsed: ParsedTransaction,
    pub raw: Transaction,
    pub received_at: Instant,
}

impl PendingTransaction {
    pub fn hash(&self) -> TxHash {
        self.parsed.hash
    }

    pub fn value_eth(&self) -> f64 {
        wei_to_eth(self.parsed.value)
    }

    pub fn gas_price_gwei(&self) -> f64 {
        self.parsed.effective_gas_price_gwei()
    }
}

/// Receives filtered pending transactions from the mempool subscription loop.
pub struct MempoolStream {
    receiver: mpsc::Receiver<Result<PendingTransaction>>,
}

impl MempoolStream {
    pub async fn recv(&mut self) -> Option<Result<PendingTransaction>> {
        self.receiver.recv().await
    }
}

/// Subscribes to pending transactions, deduplicates, parses, and filters them.
pub struct MempoolSubscriber {
    connection: Arc<ConnectionManager>,
    filter: MempoolFilter,
    cache_capacity: usize,
}

impl MempoolSubscriber {
    pub fn new(connection: Arc<ConnectionManager>, filter: MempoolFilter) -> Self {
        Self {
            connection,
            filter,
            cache_capacity: DEFAULT_CACHE_CAPACITY,
        }
    }

    pub fn from_settings(connection: Arc<ConnectionManager>, settings: &Settings) -> Self {
        Self::new(connection, MempoolFilter::from_settings(settings))
    }

    pub fn with_cache_capacity(mut self, capacity: usize) -> Self {
        self.cache_capacity = capacity.max(1);
        self
    }

    /// Starts the background subscription task and returns a channel receiver.
    pub async fn subscribe(self) -> Result<MempoolStream> {
        let ws_url = self
            .connection
            .websocket_url()
            .ok_or_else(|| NetworkError::AllProvidersFailed { attempt_count: 0 })?;

        let (hash_sender, hash_receiver) = mpsc::channel::<TxHash>(CHANNEL_BUFFER);
        let (output_sender, output_receiver) = mpsc::channel(CHANNEL_BUFFER);

        let dedup_cache = Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(self.cache_capacity).expect("non-zero cache capacity"),
        )));

        tokio::spawn(subscription_loop(ws_url, hash_sender));

        tokio::spawn(process_pending_hashes(
            Arc::clone(&self.connection),
            self.filter,
            dedup_cache,
            hash_receiver,
            output_sender,
        ));

        Ok(MempoolStream {
            receiver: output_receiver,
        })
    }
}

async fn subscription_loop(ws_url: String, hash_sender: mpsc::Sender<TxHash>) {
    let reconnect = ReconnectPolicy::new(Duration::from_millis(500), Duration::from_secs(30));

    loop {
        match Provider::<Ws>::connect(&ws_url).await {
            Ok(provider) => {
                reconnect.reset();
                info!(url = %ws_url, "connected to mempool websocket");

                match provider.subscribe_pending_txs().await {
                    Ok(mut stream) => {
                        while let Some(hash) = stream.next().await {
                            if hash_sender.send(hash).await.is_err() {
                                return;
                            }
                        }
                        warn!("pending transaction stream ended, reconnecting");
                    }
                    Err(source) => {
                        warn!(%source, "failed to subscribe to pending transactions");
                    }
                }
            }
            Err(source) => {
                warn!(%source, url = %ws_url, "websocket connection failed");
            }
        }

        reconnect.wait_before_retry().await;
    }
}

async fn process_pending_hashes(
    connection: Arc<ConnectionManager>,
    filter: MempoolFilter,
    dedup_cache: Arc<Mutex<LruCache<TxHash, Instant>>>,
    mut hash_receiver: mpsc::Receiver<TxHash>,
    output_sender: mpsc::Sender<Result<PendingTransaction>>,
) {
    while let Some(hash) = hash_receiver.recv().await {
        if is_duplicate(&dedup_cache, hash).await {
            continue;
        }

        match connection.get_transaction(hash).await {
            Ok(Some(raw_transaction)) => match parse_transaction(raw_transaction.clone()) {
                Ok(parsed) => {
                    if !filter.passes(&parsed) {
                        debug!(
                            hash = %hash,
                            value_eth = parsed.value_eth(),
                            gas_gwei = parsed.effective_gas_price_gwei(),
                            "transaction filtered out"
                        );
                        continue;
                    }

                    let pending = PendingTransaction {
                        parsed,
                        raw: raw_transaction,
                        received_at: Instant::now(),
                    };

                    if output_sender.send(Ok(pending)).await.is_err() {
                        break;
                    }
                }
                Err(error) => {
                    debug!(hash = %hash, %error, "failed to parse transaction");
                }
            },
            Ok(None) => {
                debug!(hash = %hash, "transaction not yet available from RPC");
            }
            Err(error) => {
                warn!(hash = %hash, %error, "failed to fetch transaction");
            }
        }
    }

    error!("mempool processing loop stopped");
}

async fn is_duplicate(cache: &Arc<Mutex<LruCache<TxHash, Instant>>>, hash: TxHash) -> bool {
    let mut guard = cache.lock().await;
    let now = Instant::now();

    if let Some(seen_at) = guard.get(&hash) {
        if now.duration_since(*seen_at) < CACHE_ENTRY_TTL {
            return true;
        }
    }

    guard.put(hash, now);
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockchain::transaction::{ParsedTransaction, TransactionFeeModel};
    use crate::types::{EthAmount, Gwei};
    use ethers::types::{Address, Bytes, H256};

    fn sample_parsed(value_wei: u64, gas_price_wei: u64) -> ParsedTransaction {
        ParsedTransaction {
            hash: H256::repeat_byte(0x01),
            from: Address::repeat_byte(0x02),
            to: Some(Address::repeat_byte(0x03)),
            value: U256::from(value_wei),
            gas_limit: U256::from(21000),
            gas_price: Some(U256::from(gas_price_wei)),
            max_fee_per_gas: None,
            max_priority_fee_per_gas: None,
            input: Bytes::new(),
            nonce: U256::zero(),
            fee_model: TransactionFeeModel::Legacy,
            interaction: None,
        }
    }

    #[test]
    fn filter_rejects_low_value_transactions() {
        let filter = MempoolFilter {
            min_value_wei: U256::from(EthAmount::new(0.1).to_wei()),
            min_gas_price_wei: gwei_to_wei(Gwei::new(1.0)),
            max_gas_price_wei: gwei_to_wei(Gwei::new(500.0)),
        };

        let low_value = sample_parsed(1_000_000_000_000_000, 30_000_000_000);
        assert!(!filter.passes(&low_value));
    }

    #[test]
    fn filter_accepts_qualifying_transactions() {
        let filter = MempoolFilter {
            min_value_wei: U256::from(EthAmount::new(0.1).to_wei()),
            min_gas_price_wei: gwei_to_wei(Gwei::new(1.0)),
            max_gas_price_wei: gwei_to_wei(Gwei::new(500.0)),
        };

        let qualifying = sample_parsed(100_000_000_000_000_000, 30_000_000_000);
        assert!(filter.passes(&qualifying));
    }

    #[test]
    fn filter_rejects_excessive_gas_price() {
        let filter = MempoolFilter {
            min_value_wei: U256::zero(),
            min_gas_price_wei: gwei_to_wei(Gwei::new(1.0)),
            max_gas_price_wei: gwei_to_wei(Gwei::new(100.0)),
        };

        let expensive = sample_parsed(0, 200_000_000_000);
        assert!(!filter.passes(&expensive));
    }

    #[tokio::test]
    async fn dedup_cache_skips_recent_hashes() {
        let cache = Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(100).unwrap())));
        let hash = H256::repeat_byte(0x55);

        assert!(!is_duplicate(&cache, hash).await);
        assert!(is_duplicate(&cache, hash).await);
    }
}
