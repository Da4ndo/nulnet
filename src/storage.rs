use crate::telemetry::TelemetrySnapshot;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;

pub struct SnapshotStore {
	data_dir: PathBuf,
}

impl SnapshotStore {
	pub fn new<P: AsRef<Path>>(data_dir: P) -> Self {
		let dir = data_dir.as_ref().to_path_buf();
		// Synchronous at startup — runtime not yet needed here.
		if !dir.exists() && let Err(e) = std::fs::create_dir_all(&dir) {
			tracing::warn!("Failed to create data dir {:?}: {}", dir, e);
		}
		Self { data_dir: dir }
	}

	pub async fn save(&self, snapshot: &TelemetrySnapshot) -> Result<(), String> {
		let file_name = format!("{}.json", snapshot.timestamp);
		let path = self.data_dir.join(file_name);

		let json = serde_json::to_string_pretty(snapshot)
			.map_err(|e| format!("Failed to serialize snapshot: {}", e))?;

		fs::write(&path, json).await
			.map_err(|e| format!("Failed to write snapshot to {:?}: {}", path, e))?;

		tracing::debug!("Saved snapshot to {:?}", path);
		Ok(())
	}

	pub async fn rotate(&self, retention_days: u64) -> Result<(), String> {
		let now = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or_default()
			.as_secs();

		let cutoff_time = now.saturating_sub(retention_days * 24 * 60 * 60);

		let mut entries = fs::read_dir(&self.data_dir).await
			.map_err(|e| format!("Failed to read data dir: {}", e))?;

		let mut deleted_count = 0;

		while let Ok(Some(entry)) = entries.next_entry().await {
			let path = entry.path();
			if path.extension().and_then(|s| s.to_str()) != Some("json") {
				continue;
			}
			let Some(stem) = path.file_stem().and_then(|s| s.to_str().map(str::to_owned)) else {
				continue;
			};
			let Ok(timestamp) = stem.parse::<u64>() else {
				continue;
			};
			if timestamp < cutoff_time {
				if let Err(e) = fs::remove_file(&path).await {
					tracing::warn!("Failed to delete old snapshot {:?}: {}", path, e);
				} else {
					deleted_count += 1;
				}
			}
		}

		if deleted_count > 0 {
			tracing::info!("Rotated {} old snapshots", deleted_count);
		}

		Ok(())
	}

	/// Sum of byte sizes for all `*.json` snapshots in `data_dir`.
	pub async fn snapshots_disk_bytes(&self) -> u64 {
		let mut total = 0u64;
		if let Ok(mut entries) = fs::read_dir(&self.data_dir).await {
			while let Ok(Some(entry)) = entries.next_entry().await {
				let path = entry.path();
				if path.extension().and_then(|s| s.to_str()) != Some("json") {
					continue;
				}
				if let Ok(meta) = fs::metadata(&path).await {
					total = total.saturating_add(meta.len());
				}
			}
		}
		total
	}

	pub async fn get_latest(&self) -> Result<Option<TelemetrySnapshot>, String> {
		let mut latest_path: Option<PathBuf> = None;
		let mut latest_ts = 0u64;

		if let Ok(mut entries) = fs::read_dir(&self.data_dir).await {
			while let Ok(Some(entry)) = entries.next_entry().await {
				let path = entry.path();
				if path.extension().and_then(|s| s.to_str()) != Some("json") {
					continue;
				}
				let Some(stem) = path.file_stem().and_then(|s| s.to_str().map(str::to_owned)) else {
					continue;
				};
				let Ok(timestamp) = stem.parse::<u64>() else {
					continue;
				};
				if timestamp > latest_ts {
					latest_ts = timestamp;
					latest_path = Some(path);
				}
			}
		}

		if let Some(path) = latest_path {
			let content = fs::read_to_string(&path).await
				.map_err(|e| format!("Failed to read snapshot: {}", e))?;
			let snapshot = serde_json::from_str(&content)
				.map_err(|e| format!("Failed to parse snapshot: {}", e))?;
			Ok(Some(snapshot))
		} else {
			Ok(None)
		}
	}

	pub async fn get_range(
		&self,
		since: u64,
		until: u64,
		limit: Option<usize>,
	) -> Result<Vec<TelemetrySnapshot>, String> {
		let mut snapshots: Vec<TelemetrySnapshot> = Vec::new();

		if let Ok(mut entries) = fs::read_dir(&self.data_dir).await {
			while let Ok(Some(entry)) = entries.next_entry().await {
				let path = entry.path();
				if path.extension().and_then(|s| s.to_str()) != Some("json") {
					continue;
				}
				let Some(stem) = path.file_stem().and_then(|s| s.to_str().map(str::to_owned)) else {
					continue;
				};
				let Ok(timestamp) = stem.parse::<u64>() else {
					continue;
				};
				if timestamp < since || timestamp > until {
					continue;
				}
				let Ok(content) = fs::read_to_string(&path).await else {
					continue;
				};
				let Ok(snapshot) = serde_json::from_str::<TelemetrySnapshot>(&content) else {
					continue;
				};
				snapshots.push(snapshot);
			}
		}

		snapshots.sort_by_key(|s| s.timestamp);

		if let Some(max) = limit
			&& snapshots.len() > max
		{
			let start = snapshots.len() - max;
			snapshots = snapshots.split_off(start);
		}

		Ok(snapshots)
	}

	pub async fn get_bounds(&self) -> SnapshotBounds {
		let mut oldest = None::<u64>;
		let mut newest = None::<u64>;
		let mut count = 0u64;

		if let Ok(mut entries) = fs::read_dir(&self.data_dir).await {
			while let Ok(Some(entry)) = entries.next_entry().await {
				let path = entry.path();
				if path.extension().and_then(|s| s.to_str()) != Some("json") {
					continue;
				}
				let Some(stem) = path.file_stem().and_then(|s| s.to_str().map(str::to_owned))
				else {
					continue;
				};
				let Ok(timestamp) = stem.parse::<u64>() else {
					continue;
				};
				count += 1;
				oldest = Some(oldest.map_or(timestamp, |o| o.min(timestamp)));
				newest = Some(newest.map_or(timestamp, |n| n.max(timestamp)));
			}
		}

		SnapshotBounds { oldest, newest, count }
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotBounds {
	pub oldest: Option<u64>,
	pub newest: Option<u64>,
	pub count: u64,
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::telemetry::{Cpu, LoadAverage, MemoryStats, TelemetrySnapshot};

	fn sample(ts: u64) -> TelemetrySnapshot {
		TelemetrySnapshot {
			timestamp: ts,
			os: None,
			cpu: Cpu { usage: 0.0, info: None },
			gpu: vec![],
			memory: MemoryStats {
				usage: 0.0,
				used_gb: 0.0,
				total_gb: 0.0,
			},
			disk: vec![],
			containers: vec![],
			uptime_seconds: 0,
			load_average: LoadAverage {
				one: 0.0,
				five: 0.0,
				fifteen: 0.0,
			},
			cached_telemetry_size: "0 B".to_string(),
		}
	}

	use std::sync::atomic::{AtomicU64, Ordering};

	static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(0);

	async fn temp_store() -> (SnapshotStore, std::path::PathBuf) {
		let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
		let dir = std::env::temp_dir().join(format!("nulnet-store-{seq}"));
		let store = SnapshotStore::new(&dir);
		(store, dir)
	}

	#[tokio::test]
	async fn get_bounds_tracks_oldest_newest_and_count() {
		let (store, dir) = temp_store().await;
		store.save(&sample(100)).await.unwrap();
		store.save(&sample(200)).await.unwrap();
		store.save(&sample(150)).await.unwrap();

		let bounds = store.get_bounds().await;
		assert_eq!(bounds.count, 3);
		assert_eq!(bounds.oldest, Some(100));
		assert_eq!(bounds.newest, Some(200));

		let _ = tokio::fs::remove_dir_all(dir).await;
	}

	#[tokio::test]
	async fn get_range_without_limit_returns_all_in_window() {
		let (store, dir) = temp_store().await;
		for ts in [100u64, 200, 300, 400, 500] {
			store.save(&sample(ts)).await.unwrap();
		}

		let all = store.get_range(0, u64::MAX, None).await.unwrap();
		assert_eq!(all.len(), 5);

		let limited = store.get_range(0, u64::MAX, Some(2)).await.unwrap();
		assert_eq!(limited.len(), 2);
		assert_eq!(limited[0].timestamp, 400);
		assert_eq!(limited[1].timestamp, 500);

		let _ = tokio::fs::remove_dir_all(dir).await;
	}

	#[tokio::test]
	async fn get_latest_returns_newest_snapshot() {
		let (store, dir) = temp_store().await;
		store.save(&sample(100)).await.unwrap();
		store.save(&sample(200)).await.unwrap();
		store.save(&sample(150)).await.unwrap();

		let latest = store.get_latest().await.unwrap();
		let latest = latest.expect("expected a latest snapshot");
		assert_eq!(latest.timestamp, 200);

		let _ = tokio::fs::remove_dir_all(dir).await;
	}

	#[tokio::test]
	async fn rotate_deletes_snapshots_older_than_retention() {
		let (store, dir) = temp_store().await;

		// Use a retention window expressed in days: cutoff_time = now - retention_days*86400.
		// We create snapshots just old enough to be deleted.
		let now = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.unwrap_or_default()
			.as_secs();

		let retention_days = 1u64;
		let cutoff = now.saturating_sub(retention_days * 24 * 60 * 60);

		let old_ts = cutoff.saturating_sub(10);
		let new_ts = cutoff.saturating_add(10);

		store.save(&sample(old_ts)).await.unwrap();
		store.save(&sample(new_ts)).await.unwrap();

		store.rotate(retention_days).await.unwrap();

		let latest = store.get_latest().await.unwrap();
		let latest = latest.expect("expected a latest snapshot");
		assert_eq!(latest.timestamp, new_ts);

		let _ = tokio::fs::remove_dir_all(dir).await;
	}
}
