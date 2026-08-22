use crate::analysis::fetch_network_state;
use crate::analysis::opportunity::{OpportunityDetector, OpportunityType, ScoredOpportunity};
use crate::analysis::profitability::NetworkState;
use crate::blockchain::mempool::PendingTransaction;
use crate::blockchain::ConnectionManager;
use crate::execution::{ReplacementExecutor, SubmissionOutcome};
use crate::metrics::BotMetrics;
use crate::runtime::NetworkStateCache;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, Semaphore};
use tracing::{info, warn};

/// Parallel transaction processor with bounded concurrency.
pub struct TransactionWorkerPool {
    semaphore: Arc<Semaphore>,
    metrics: Arc<BotMetrics>,
    network_cache: NetworkStateCache,
}

impl TransactionWorkerPool {
    pub fn new(worker_count: usize, metrics: Arc<BotMetrics>, cache_ttl_secs: u64) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(worker_count.max(1))),
            metrics,
            network_cache: NetworkStateCache::new(std::time::Duration::from_secs(
                cache_ttl_secs.max(1),
            )),
        }
    }

    pub fn network_cache(&self) -> NetworkStateCache {
        self.network_cache.clone()
    }

    pub async fn spawn_analysis(
        &self,
        transaction: PendingTransaction,
        connection: Arc<ConnectionManager>,
        detector: Arc<Mutex<OpportunityDetector>>,
        executor: Option<Arc<Mutex<ReplacementExecutor>>>,
        fallback_network_state: NetworkState,
    ) {
        let permit = match self.semaphore.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => return,
        };

        let metrics = Arc::clone(&self.metrics);
        let network_cache = self.network_cache.clone();

        tokio::spawn(async move {
            metrics.active_workers.inc();
            let started_at = Instant::now();
            metrics.transactions_processed.inc();

            let network_state =
                resolve_network_state(&connection, &network_cache, fallback_network_state).await;

            let opportunity = {
                let mut detector = detector.lock().await;
                let result = detector.evaluate(transaction, &network_state);
                metrics
                    .opportunity_queue_depth
                    .set(detector.queue_depth() as i64);
                result
            };

            metrics.record_processing("analysis", started_at);

            if let Some(opportunity) = opportunity {
                handle_opportunity(opportunity, executor, &network_state, &metrics).await;
            }

            metrics.active_workers.dec();
            drop(permit);
        });
    }
}

async fn resolve_network_state(
    connection: &ConnectionManager,
    cache: &NetworkStateCache,
    fallback: NetworkState,
) -> NetworkState {
    if let Some(cached) = cache.get().await {
        return cached;
    }

    match fetch_network_state(connection).await {
        Ok(state) => {
            cache.update(state.clone()).await;
            state
        }
        Err(error) => {
            warn!(%error, "using cached or fallback network state");
            cache.snapshot().await.unwrap_or(fallback)
        }
    }
}

async fn handle_opportunity(
    opportunity: ScoredOpportunity,
    executor: Option<Arc<Mutex<ReplacementExecutor>>>,
    network_state: &NetworkState,
    metrics: &BotMetrics,
) {
    metrics.opportunities_detected.inc();
    metrics
        .opportunity_by_type
        .with_label_values(&[opportunity_type_label(opportunity.opportunity_type)])
        .inc();

    info!(
        hash = %opportunity.hash(),
        kind = %opportunity.opportunity_type,
        net_profit_eth = opportunity.net_profit_eth(),
        roi = format!("{:.2}%", opportunity.profitability.roi_percent),
        risk_score = opportunity.risk.score,
        priority = format!("{:.2}", opportunity.priority_score),
        "opportunity detected"
    );

    let Some(executor) = executor else {
        return;
    };

    let execution_started = Instant::now();
    let mut executor = executor.lock().await;
    match executor
        .execute_opportunity(&opportunity, network_state)
        .await
    {
        Ok(SubmissionOutcome::Confirmed {
            hash,
            block_number,
            gas_used,
        }) => {
            metrics.execution_successes.inc();
            metrics.record_gas_spent(gas_used.as_u128());
            metrics.record_profit(opportunity.profitability.net_profit_wei.as_u128());
            metrics.record_processing("execution", execution_started);
            info!(
                hash = %hash,
                block = block_number,
                gas_used = %gas_used,
                "replacement confirmed"
            );
        }
        Ok(SubmissionOutcome::Replaced {
            original_hash,
            replacement_hash,
        }) => {
            metrics.execution_successes.inc();
            info!(
                original = %original_hash,
                replacement = %replacement_hash,
                "transaction replaced in mempool"
            );
        }
        Ok(SubmissionOutcome::Dropped { hash, wait_secs }) => {
            metrics.execution_failures.inc();
            warn!(hash = %hash, wait_secs, "transaction dropped from mempool");
            executor.safety().record_failure();
        }
        Ok(SubmissionOutcome::Rejected { reason }) => {
            metrics.execution_failures.inc();
            warn!(%reason, "submission rejected by node");
        }
        Err(error) => {
            metrics.execution_failures.inc();
            warn!(%error, "execution failed");
        }
    }
}

fn opportunity_type_label(kind: OpportunityType) -> &'static str {
    match kind {
        OpportunityType::Arbitrage => "arbitrage",
        OpportunityType::Sandwich => "sandwich",
        OpportunityType::GasReplacement => "gas_replacement",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_pool_respects_concurrency_limit() {
        let metrics = Arc::new(BotMetrics::new().expect("metrics"));
        let pool = TransactionWorkerPool::new(4, metrics, 12);
        assert_eq!(pool.semaphore.available_permits(), 4);
    }
}
