use crate::analysis::profitability::NetworkState;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Caches network state to avoid redundant RPC calls between refreshes.
#[derive(Clone)]
pub struct NetworkStateCache {
    inner: Arc<RwLock<CachedNetworkState>>,
    ttl: Duration,
}

struct CachedNetworkState {
    state: Option<NetworkState>,
    fetched_at: Option<Instant>,
}

impl NetworkStateCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            inner: Arc::new(RwLock::new(CachedNetworkState {
                state: None,
                fetched_at: None,
            })),
            ttl,
        }
    }

    pub async fn get(&self) -> Option<NetworkState> {
        let guard = self.inner.read().await;
        match (&guard.state, guard.fetched_at) {
            (Some(state), Some(fetched_at)) if fetched_at.elapsed() < self.ttl => {
                Some(state.clone())
            }
            _ => None,
        }
    }

    pub async fn update(&self, state: NetworkState) {
        let mut guard = self.inner.write().await;
        guard.state = Some(state);
        guard.fetched_at = Some(Instant::now());
    }

    pub async fn snapshot(&self) -> Option<NetworkState> {
        self.inner.read().await.state.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::U256;

    #[tokio::test]
    async fn cache_returns_state_within_ttl() {
        let cache = NetworkStateCache::new(Duration::from_secs(30));
        let state = NetworkState {
            base_fee_per_gas: U256::from(1),
            suggested_priority_fee: U256::from(1),
            block_gas_used_ratio: 0.5,
            block_number: 42,
        };

        cache.update(state.clone()).await;
        let cached = cache.get().await.expect("cached");
        assert_eq!(cached.block_number, 42);
    }
}
