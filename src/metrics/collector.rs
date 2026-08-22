use prometheus::{
    Counter, CounterVec, Encoder, HistogramOpts, HistogramVec, IntCounter, IntGauge, Opts,
    Registry, TextEncoder,
};
use std::sync::Arc;
use std::time::Instant;

/// Prometheus metrics for bot throughput, latency, and financial outcomes.
#[derive(Clone)]
pub struct BotMetrics {
    registry: Arc<Registry>,
    pub transactions_processed: IntCounter,
    pub opportunities_detected: IntCounter,
    pub execution_successes: IntCounter,
    pub execution_failures: IntCounter,
    pub gas_spent_wei: Counter,
    pub profit_earned_wei: Counter,
    pub processing_latency: HistogramVec,
    pub opportunity_by_type: CounterVec,
    pub bot_ready: IntGauge,
    pub active_workers: IntGauge,
    pub opportunity_queue_depth: IntGauge,
}

impl BotMetrics {
    pub fn new() -> Result<Self, prometheus::Error> {
        let registry = Arc::new(Registry::new());

        let transactions_processed = IntCounter::new(
            "bot_transactions_processed_total",
            "Total pending transactions analyzed",
        )?;
        let opportunities_detected = IntCounter::new(
            "bot_opportunities_detected_total",
            "Total profitable opportunities detected",
        )?;
        let execution_successes = IntCounter::new(
            "bot_execution_success_total",
            "Total successful transaction submissions",
        )?;
        let execution_failures = IntCounter::new(
            "bot_execution_failure_total",
            "Total failed or dropped transaction submissions",
        )?;
        let gas_spent_wei = Counter::new(
            "bot_gas_spent_wei_total",
            "Cumulative gas spent on confirmed transactions (wei)",
        )?;
        let profit_earned_wei = Counter::new(
            "bot_profit_earned_wei_total",
            "Estimated cumulative profit from successful executions (wei)",
        )?;
        let processing_latency = HistogramVec::new(
            HistogramOpts::new(
                "bot_transaction_processing_seconds",
                "End-to-end transaction processing latency",
            )
            .buckets(vec![0.001, 0.005, 0.01, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0]),
            &["stage"],
        )?;
        let opportunity_by_type = CounterVec::new(
            Opts::new(
                "bot_opportunities_by_type_total",
                "Opportunities detected grouped by type",
            ),
            &["type"],
        )?;
        let bot_ready = IntGauge::new("bot_ready", "1 when bot is connected and ready")?;
        let active_workers = IntGauge::new(
            "bot_active_workers",
            "Number of in-flight transaction processing tasks",
        )?;
        let opportunity_queue_depth = IntGauge::new(
            "bot_opportunity_queue_depth",
            "Current depth of the opportunity priority queue",
        )?;

        registry.register(Box::new(transactions_processed.clone()))?;
        registry.register(Box::new(opportunities_detected.clone()))?;
        registry.register(Box::new(execution_successes.clone()))?;
        registry.register(Box::new(execution_failures.clone()))?;
        registry.register(Box::new(gas_spent_wei.clone()))?;
        registry.register(Box::new(profit_earned_wei.clone()))?;
        registry.register(Box::new(processing_latency.clone()))?;
        registry.register(Box::new(opportunity_by_type.clone()))?;
        registry.register(Box::new(bot_ready.clone()))?;
        registry.register(Box::new(active_workers.clone()))?;
        registry.register(Box::new(opportunity_queue_depth.clone()))?;

        Ok(Self {
            registry,
            transactions_processed,
            opportunities_detected,
            execution_successes,
            execution_failures,
            gas_spent_wei,
            profit_earned_wei,
            processing_latency,
            opportunity_by_type,
            bot_ready,
            active_workers,
            opportunity_queue_depth,
        })
    }

    pub fn set_ready(&self, ready: bool) {
        self.bot_ready.set(if ready { 1 } else { 0 });
    }

    pub fn record_processing(&self, stage: &str, started_at: Instant) {
        self.processing_latency
            .with_label_values(&[stage])
            .observe(started_at.elapsed().as_secs_f64());
    }

    pub fn record_gas_spent(&self, wei: u128) {
        self.gas_spent_wei.inc_by(wei as f64);
    }

    pub fn record_profit(&self, wei: u128) {
        self.profit_earned_wei.inc_by(wei as f64);
    }

    pub fn encode_prometheus(&self) -> Result<String, prometheus::Error> {
        let mut bot_buffer = Vec::new();
        let encoder = TextEncoder::new();
        let bot_families = self.registry.gather();
        encoder.encode(&bot_families, &mut bot_buffer)?;

        let mut global_buffer = Vec::new();
        let global_families = prometheus::gather();
        encoder.encode(&global_families, &mut global_buffer)?;

        Ok(format!(
            "{}{}",
            String::from_utf8_lossy(&bot_buffer),
            String::from_utf8_lossy(&global_buffer)
        ))
    }
}

impl Default for BotMetrics {
    fn default() -> Self {
        Self::new().expect("metrics must initialize")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_register_and_increment() {
        let metrics = BotMetrics::new().expect("registry init");
        metrics.transactions_processed.inc();
        metrics.opportunities_detected.inc_by(2);
        metrics.record_gas_spent(1_000_000_000_000_000);

        let output = metrics.encode_prometheus().expect("encode");
        assert!(output.contains("bot_transactions_processed_total"));
        assert!(output.contains("bot_gas_spent_wei_total"));
    }
}
