mod config;
mod release;
mod storage;
mod telemetry;
mod transport;
mod update;

use std::sync::Arc;
use tokio::time::{sleep, Duration};
use crate::storage::SnapshotStore;
use crate::telemetry::TelemetryCollector;

#[tokio::main]
async fn main() {
	let log_filter = std::env::var("NULNET_LOG")
		.unwrap_or_else(|_| "nulnet=info".to_string());

	tracing_subscriber::fmt()
		.with_env_filter(log_filter)
		.init();

	tracing::info!("Starting NULNET agent v{}", env!("CARGO_PKG_VERSION"));

	let config = Arc::new(config::load_config());
	let store = Arc::new(SnapshotStore::new(&config.agent.data_dir));
	tokio::spawn(telemetry_loop(
		config.telemetry.interval_seconds,
		store.clone(),
	));

	tokio::spawn(rotation_loop(
		config.agent.retention_days,
		store.clone(),
	));

	let socket_path = config.agent.socket_path.clone();
	let transport_config = config.clone();
	let transport_store = store.clone();

	tokio::select! {
		_ = transport::start_server(transport_store, transport_config) => {},
		_ = shutdown_signal() => {
			tracing::info!("Received shutdown signal, cleaning up");
			let _ = std::fs::remove_file(&socket_path);
		}
	}

	tracing::info!("NULNET agent shut down");
}

async fn telemetry_loop(
	interval_seconds: u64,
	telemetry_store: Arc<SnapshotStore>,
) {
	let mut collector = TelemetryCollector::new();
	let secs = if interval_seconds > 0 {
		interval_seconds
	} else {
		30
	};
	let interval = Duration::from_secs(secs);

	loop {
		let mut snapshot = collector.collect().await;
		snapshot.set_cached_telemetry_disk(
			telemetry_store.snapshots_disk_bytes().await,
		);
		if let Err(e) = telemetry_store.save(&snapshot).await {
			tracing::error!("Failed to save telemetry snapshot: {}", e);
		}
		sleep(interval).await;
	}
}

async fn rotation_loop(
	retention_days: u64,
	rotation_store: Arc<SnapshotStore>,
) {
	let interval = Duration::from_secs(3600);

	// Run immediately on startup, then hourly.
	if let Err(e) = rotation_store.rotate(retention_days).await {
		tracing::error!("Failed to rotate old snapshots: {}", e);
	}

	loop {
		sleep(interval).await;
		if let Err(e) = rotation_store.rotate(retention_days).await {
			tracing::error!("Failed to rotate old snapshots: {}", e);
		}
	}
}

async fn shutdown_signal() {
	#[cfg(unix)]
	{
		use tokio::signal::unix::{signal, SignalKind};
		let mut sigterm = match signal(SignalKind::terminate()) {
			Ok(s) => s,
			Err(e) => {
				tracing::error!("Failed to install SIGTERM handler: {}", e);
				std::future::pending::<()>().await;
				return;
			}
		};
		tokio::select! {
			_ = sigterm.recv() => tracing::info!("Received SIGTERM"),
			_ = tokio::signal::ctrl_c() => tracing::info!("Received Ctrl+C"),
		}
	}
	#[cfg(not(unix))]
	{
		let _ = tokio::signal::ctrl_c().await;
	}
}
