mod config;
mod live;
mod pacer;
mod release;
mod storage;
mod telemetry;
mod transport;
mod update;

use std::sync::Arc;
use std::time::Instant;
use tokio::time::{interval, sleep, Duration, MissedTickBehavior};
use crate::pacer::Pacer;
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
	tokio::spawn(telemetry_loop(config.clone(), store.clone()));

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
	config: Arc<config::Config>,
	telemetry_store: Arc<SnapshotStore>,
) {
	let telemetry = &config.telemetry;
	let mut collector = TelemetryCollector::new();
	let base = Duration::from_secs(if telemetry.interval_seconds > 0 {
		telemetry.interval_seconds
	} else {
		30
	});
	let tick = Duration::from_secs(telemetry.sample_interval_seconds.max(1)).min(base);
	let mut pacer = Pacer::new(base, tick);
	let mut ticker = interval(tick);
	ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
	// Collected per probe, attached to the snapshot that closes the window.
	let mut events = Vec::new();
	// Disk, containers and system info are pushed to live clients only when they change.
	let mut last_details = String::new();

	loop {
		ticker.tick().await;
		let probe = collector.probe().await;
		let decision = pacer.observe(&probe, Instant::now());
		live::publish(|| {
			serde_json::json!({
				"type": "reading",
				"reading": collector.last_sample(),
				"events": decision.events,
			})
		});
		events.extend(decision.events);
		if !decision.persist {
			continue;
		}
		let mut snapshot = collector.collect(probe).await;
		for event in &events {
			tracing::info!("Telemetry event: {:?} {:?} ({:.2})", event.kind, event.phase, event.value);
		}
		snapshot.events = std::mem::take(&mut events);
		snapshot.set_cached_telemetry_disk(
			telemetry_store.snapshots_disk_bytes().await,
		);
		let details = serde_json::json!({
			"os": &snapshot.os,
			"cpu": &snapshot.cpu.info,
			"gpu": &snapshot.gpu,
			"disk": &snapshot.disk,
			"containers": &snapshot.containers,
			"uptime_seconds": snapshot.uptime_seconds,
			"cached_telemetry_size": &snapshot.cached_telemetry_size,
		});
		// Uptime and GPU usage always move; key on disk and containers so only real changes push.
		let key = serde_json::to_string(&[&details["disk"], &details["containers"]])
			.unwrap_or_default();
		if key != last_details {
			live::publish(|| serde_json::json!({ "type": "details", "details": &details }));
			last_details = key;
		}
		if let Err(e) = telemetry_store.save(&snapshot).await {
			tracing::error!("Failed to save telemetry snapshot: {}", e);
		}
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
