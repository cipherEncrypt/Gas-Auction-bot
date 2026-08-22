use crate::metrics::BotMetrics;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{error, info};

#[derive(Clone)]
pub struct HealthStatus {
    pub ready: bool,
    pub rpc_healthy: bool,
    pub execution_enabled: bool,
}

pub struct MetricsServer {
    bind_address: SocketAddr,
    metrics: Arc<BotMetrics>,
    health: Arc<tokio::sync::RwLock<HealthStatus>>,
    shutdown: watch::Receiver<bool>,
}

impl MetricsServer {
    pub fn new(
        bind_address: SocketAddr,
        metrics: Arc<BotMetrics>,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        Self {
            bind_address,
            metrics,
            health: Arc::new(tokio::sync::RwLock::new(HealthStatus {
                ready: false,
                rpc_healthy: false,
                execution_enabled: false,
            })),
            shutdown,
        }
    }

    pub fn health_handle(&self) -> HealthHandle {
        HealthHandle {
            inner: Arc::clone(&self.health),
        }
    }

    pub async fn run(mut self) -> Result<(), std::io::Error> {
        let listener = TcpListener::bind(self.bind_address).await?;
        info!(address = %self.bind_address, "metrics and health server listening");

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    let (mut stream, _) = accept_result?;
                    let metrics = Arc::clone(&self.metrics);
                    let health = Arc::clone(&self.health);

                    tokio::spawn(async move {
                        if let Err(error) = handle_connection(&mut stream, &metrics, &health).await {
                            error!(%error, "HTTP connection handler failed");
                        }
                    });
                }
                changed = self.shutdown.changed() => {
                    if changed.is_ok() && *self.shutdown.borrow() {
                        info!("metrics server shutting down");
                        break;
                    }
                }
            }
        }

        Ok(())
    }
}

#[derive(Clone)]
pub struct HealthHandle {
    inner: Arc<tokio::sync::RwLock<HealthStatus>>,
}

impl HealthHandle {
    pub async fn set_ready(&self, ready: bool) {
        self.inner.write().await.ready = ready;
    }

    pub async fn set_rpc_healthy(&self, healthy: bool) {
        self.inner.write().await.rpc_healthy = healthy;
    }

    pub async fn set_execution_enabled(&self, enabled: bool) {
        self.inner.write().await.execution_enabled = enabled;
    }
}

async fn handle_connection(
    stream: &mut tokio::net::TcpStream,
    metrics: &BotMetrics,
    health: &tokio::sync::RwLock<HealthStatus>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut buffer = [0u8; 1024];
    let bytes_read = stream.read(&mut buffer).await?;
    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");

    let (status_code, content_type, body) = match path {
        "/metrics" => {
            let body = metrics.encode_prometheus().unwrap_or_default();
            ("200 OK", "text/plain; version=0.0.4", body)
        }
        "/health" => {
            let status = health.read().await;
            let body = serde_json::json!({
                "status": if status.ready { "healthy" } else { "starting" },
                "rpc_healthy": status.rpc_healthy,
                "execution_enabled": status.execution_enabled,
            })
            .to_string();
            ("200 OK", "application/json", body)
        }
        "/ready" => {
            let status = health.read().await;
            if status.ready && status.rpc_healthy {
                ("200 OK", "application/json", r#"{"status":"ready"}"#.into())
            } else {
                (
                    "503 Service Unavailable",
                    "application/json",
                    r#"{"status":"not_ready"}"#.into(),
                )
            }
        }
        _ => ("404 Not Found", "text/plain", "not found".to_string()),
    };

    let response = format!(
        "HTTP/1.1 {status_code}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn metrics_endpoint_returns_prometheus_format() {
        let metrics = Arc::new(BotMetrics::new().expect("metrics"));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let server = MetricsServer::new(
            "127.0.0.1:0".parse().expect("parse"),
            Arc::clone(&metrics),
            shutdown_rx,
        );

        let health = server.health_handle();
        health.set_ready(true).await;
        health.set_rpc_healthy(true).await;

        shutdown_tx.send(true).ok();
        let output = metrics.encode_prometheus().expect("encode");
        assert!(output.contains("bot_ready"));
    }
}
