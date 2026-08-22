use gas_auction_bot::analysis::{fetch_network_state, OpportunityDetector};
use gas_auction_bot::blockchain::{ConnectionConfig, ConnectionManager, MempoolSubscriber};
use gas_auction_bot::execution::ReplacementExecutor;
use gas_auction_bot::metrics::{BotMetrics, MetricsServer};
use gas_auction_bot::runtime::{ShutdownCoordinator, TransactionWorkerPool};
use gas_auction_bot::utils::{correlation_span, init_logging};
use gas_auction_bot::{Result, Settings};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;
use tokio::sync::Mutex;
use tokio::time::{interval, sleep};
use tracing::{info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    let settings = Settings::load()?;

    init_logging(&settings.logging).map_err(|error| {
        gas_auction_bot::error::ConfigError::InvalidValue {
            key: "logging".into(),
            reason: error.to_string(),
        }
    })?;

    let _span = correlation_span("startup").entered();

    info!(
        chain_id = settings.network.chain_id,
        rpc_count = settings.network.rpc_urls.len(),
        max_gas = %settings.max_gas_price(),
        min_profit = %settings.min_profit_threshold(),
        max_risk = settings.analysis.max_risk_score,
        workers = settings.server.worker_count,
        metrics_addr = %settings.server.bind_address,
        "gas auction bot starting"
    );

    if settings.safety.emergency_stop {
        warn!("emergency stop is active — bot will not execute replacements");
        return Ok(());
    }

    let execution_enabled = ReplacementExecutor::execution_enabled(&settings);
    if !execution_enabled {
        warn!("execution disabled — configure wallet private key to submit replacements");
    }

    let shutdown = ShutdownCoordinator::new();
    let metrics = Arc::new(BotMetrics::new().map_err(|error| {
        gas_auction_bot::error::ConfigError::InvalidValue {
            key: "metrics".into(),
            reason: error.to_string(),
        }
    })?);

    let bind_address: SocketAddr =
        settings
            .server
            .bind_address
            .parse()
            .map_err(|error: std::net::AddrParseError| {
                gas_auction_bot::error::ConfigError::InvalidValue {
                    key: "server.bind_address".into(),
                    reason: error.to_string(),
                }
            })?;

    let metrics_server =
        MetricsServer::new(bind_address, Arc::clone(&metrics), shutdown.subscribe());
    let health = metrics_server.health_handle();
    health.set_execution_enabled(execution_enabled).await;

    tokio::spawn(async move {
        if let Err(error) = metrics_server.run().await {
            warn!(%error, "metrics server exited");
        }
    });

    let connection_config = ConnectionConfig::from_settings(&settings);
    let connection = Arc::new(ConnectionManager::connect(&connection_config).await?);

    let provider_health = connection.health_snapshots().await;
    let rpc_healthy = provider_health.iter().any(|snapshot| snapshot.is_healthy);
    health.set_rpc_healthy(rpc_healthy).await;

    for snapshot in &provider_health {
        info!(
            endpoint = %snapshot.endpoint,
            healthy = snapshot.is_healthy,
            failures = snapshot.consecutive_failures,
            "RPC provider status"
        );
    }

    let executor = if execution_enabled {
        let executor = ReplacementExecutor::initialize(Arc::clone(&connection), &settings).await?;
        info!(wallet = %executor.wallet_address(), "execution engine initialized");
        Some(Arc::new(Mutex::new(executor)))
    } else {
        None
    };

    let worker_pool = TransactionWorkerPool::new(
        settings.server.worker_count,
        Arc::clone(&metrics),
        settings.server.network_cache_ttl_secs,
    );

    let mut mempool = MempoolSubscriber::from_settings(Arc::clone(&connection), &settings)
        .subscribe()
        .await?;

    let opportunity_detector = Arc::new(Mutex::new(OpportunityDetector::from_settings(&settings)));

    let mut network_state = fetch_network_state(&connection).await?;
    worker_pool
        .network_cache()
        .update(network_state.clone())
        .await;

    metrics.set_ready(true);
    health.set_ready(true).await;

    let mut network_refresh = interval(Duration::from_secs(
        settings.server.network_cache_ttl_secs.max(1),
    ));

    info!(
        block = network_state.block_number,
        congestion = format!("{:.1}%", network_state.block_gas_used_ratio * 100.0),
        "production pipeline active — mempool → analysis → execution"
    );

    loop {
        tokio::select! {
            pending = mempool.recv() => {
                if shutdown.is_shutdown() {
                    break;
                }

                match pending {
                    Some(Ok(transaction)) => {
                        worker_pool.spawn_analysis(
                            transaction,
                            Arc::clone(&connection),
                            Arc::clone(&opportunity_detector),
                            executor.clone(),
                            network_state.clone(),
                        ).await;
                    }
                    Some(Err(error)) => {
                        warn!(%error, "mempool stream error");
                    }
                    None => {
                        warn!("mempool stream closed");
                        break;
                    }
                }
            }
            _ = network_refresh.tick() => {
                match fetch_network_state(&connection).await {
                    Ok(state) => {
                        network_state = state.clone();
                        worker_pool.network_cache().update(state).await;
                    }
                    Err(error) => warn!(%error, "failed to refresh network state"),
                }
            }
            _ = signal::ctrl_c() => {
                info!("shutdown signal received, draining in-flight work");
                shutdown.trigger();
                metrics.set_ready(false);
                health.set_ready(false).await;
                break;
            }
        }
    }

    sleep(Duration::from_secs(settings.server.shutdown_drain_secs)).await;
    info!("gas auction bot stopped");
    Ok(())
}
