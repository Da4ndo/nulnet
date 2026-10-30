use std::sync::{Arc, OnceLock};
use tokio::sync::broadcast;

static LIVE: OnceLock<broadcast::Sender<Arc<str>>> = OnceLock::new();

const BUFFER: usize = 64;

fn sender() -> &'static broadcast::Sender<Arc<str>> {
	LIVE.get_or_init(|| broadcast::channel(BUFFER).0)
}

pub fn subscribe() -> broadcast::Receiver<Arc<str>> {
	sender().subscribe()
}

/// Publishes `data()` as a streaming response line; builds nothing when nobody listens.
pub fn publish(data: impl FnOnce() -> serde_json::Value) {
	let tx = sender();
	if tx.receiver_count() == 0 {
		return;
	}
	let line = serde_json::json!({ "id": "live", "status": "streaming", "data": data() });
	match serde_json::to_string(&line) {
		Ok(line) => {
			let _ = tx.send(line.into());
		}
		Err(e) => tracing::error!("Failed to encode live message: {}", e),
	}
}
