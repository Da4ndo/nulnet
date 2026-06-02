use crate::storage::SnapshotStore;
use crate::update::run_update_stream;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;
use tokio::time::{timeout, Duration};

/// Maximum simultaneous client connections.
const MAX_CONNECTIONS: usize = 32;

/// How long a challenge nonce stays valid (prevents stale auth sessions).
const AUTH_NONCE_TTL: Duration = Duration::from_secs(30);

/// Close connections that send nothing for this long.
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Deserialize)]
pub struct AgentRequest {
	pub id: String,
	pub command: String,
	#[serde(default)]
	pub params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentResponse {
	pub id: String,
	pub status: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub data: Option<serde_json::Value>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub error: Option<String>,
}

pub async fn start_server(store: Arc<SnapshotStore>, config: Arc<crate::config::Config>) {
	let socket_path = &config.agent.socket_path;

	let _ = std::fs::remove_file(socket_path);

	let listener = match UnixListener::bind(socket_path) {
		Ok(l) => l,
		Err(e) => {
			tracing::error!("Failed to bind to socket {}: {}", socket_path, e);
			return;
		}
	};

	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		let mode = std::fs::Permissions::from_mode(0o660);
		if let Err(e) = std::fs::set_permissions(socket_path, mode) {
			tracing::warn!("Could not chmod socket {}: {}", socket_path, e);
		}
	}

	tracing::info!("Listening on Unix socket: {}", socket_path);

	let sem = Arc::new(Semaphore::new(MAX_CONNECTIONS));

	loop {
		match listener.accept().await {
			Ok((stream, _)) => {
				let permit = match Arc::clone(&sem).try_acquire_owned() {
					Ok(p) => p,
					Err(_) => {
						tracing::warn!(
							"Max connections ({}) reached, rejecting connection",
							MAX_CONNECTIONS
						);
						drop(stream);
						continue;
					}
				};
				let store = Arc::clone(&store);
				let config = Arc::clone(&config);
				tokio::spawn(async move {
					let _permit = permit;
					handle_connection(stream, store, config).await;
				});
			}
			Err(e) => {
				tracing::error!("Failed to accept connection: {}", e);
			}
		}
	}
}

async fn handle_connection(
	stream: UnixStream,
	store: Arc<SnapshotStore>,
	config: Arc<crate::config::Config>,
) {
	let (reader_half, mut writer) = tokio::io::split(stream);
	let mut reader = BufReader::new(reader_half);
	let mut buffer = String::new();

	let mut authenticated = false;
	let mut pending_nonce: Option<(Instant, [u8; 32])> = None;
	let mut current_pubkey: Option<VerifyingKey> = None;

	loop {
		buffer.clear();

		let read_res = timeout(READ_IDLE_TIMEOUT, reader.read_line(&mut buffer)).await;

		match read_res {
			Err(_) => {
				tracing::debug!("Connection idle for {}s, closing", READ_IDLE_TIMEOUT.as_secs());
				break;
			}
			Ok(Err(e)) => {
				tracing::error!("Socket read error: {}", e);
				break;
			}
			Ok(Ok(0)) => break, // EOF
			Ok(Ok(_)) => {}
		}

		if buffer.trim().is_empty() {
			continue;
		}

		let request = match serde_json::from_str::<AgentRequest>(&buffer) {
			Ok(r) => r,
			Err(e) => {
				let _ = write_response(
					&mut writer,
					&AgentResponse {
						id: String::new(),
						status: "error".to_string(),
						data: None,
						error: Some(format!("Invalid JSON: {}", e)),
					},
				)
				.await;
				continue;
			}
		};

		let keep_going = route_request(
			request,
			&store,
			&config,
			&mut writer,
			&mut authenticated,
			&mut pending_nonce,
			&mut current_pubkey,
		)
		.await;

		if !keep_going {
			break;
		}
	}
}

/// Returns `false` when the connection should be closed (write error or protocol error).
async fn route_request(
	request: AgentRequest,
	store: &Arc<SnapshotStore>,
	config: &Arc<crate::config::Config>,
	writer: &mut tokio::io::WriteHalf<UnixStream>,
	authenticated: &mut bool,
	pending_nonce: &mut Option<(Instant, [u8; 32])>,
	current_pubkey: &mut Option<VerifyingKey>,
) -> bool {
	match request.command.as_str() {
		"agent.auth.request" => {
			let res = handle_auth_request(request, config, pending_nonce, current_pubkey);
			write_response(writer, &res).await.is_ok()
		}
		"agent.auth.verify" => {
			let res = handle_auth_verify(request, authenticated, pending_nonce, current_pubkey);
			write_response(writer, &res).await.is_ok()
		}
		_ if !*authenticated => {
			let res = AgentResponse {
				id: request.id,
				status: "error".to_string(),
				data: None,
				error: Some("Not authenticated".to_string()),
			};
			write_response(writer, &res).await.is_ok()
		}
		"agent.update" => handle_update_streaming(request, config, writer).await,
		_ => {
			let res = handle_command(request, store, config).await;
			write_response(writer, &res).await.is_ok()
		}
	}
}

async fn handle_update_streaming(
	request: AgentRequest,
	config: &Arc<crate::config::Config>,
	writer: &mut tokio::io::WriteHalf<UnixStream>,
) -> bool {
	let req_id = request.id;
	let (tx, mut rx) = tokio::sync::mpsc::channel(100);
	let update_config = config.update.clone();

	let task = tokio::spawn(async move {
		let result = run_update_stream(tx.clone(), &update_config).await;
		if let Err(ref e) = result {
			let _ = tx.send(format!("ERROR: {}", e)).await;
		}
		result
	});

	while let Some(line) = rx.recv().await {
		let res = AgentResponse {
			id: req_id.clone(),
			status: "streaming".to_string(),
			data: Some(serde_json::json!({ "output": line })),
			error: None,
		};
		if write_response(writer, &res).await.is_err() {
			return false;
		}
	}

	let outcome = match task.await {
		Ok(Ok(o)) => Some(o),
		Ok(Err(_)) | Err(_) => None,
	};

	let final_res = match outcome {
		Some(crate::update::UpdateOutcome::UpToDate) => AgentResponse {
			id: req_id,
			status: "ok".to_string(),
			data: Some(serde_json::json!({ "up_to_date": true })),
			error: None,
		},
		Some(crate::update::UpdateOutcome::Installed { restart_scheduled }) => AgentResponse {
			id: req_id,
			status: "ok".to_string(),
			data: Some(serde_json::json!({
				"updated": true,
				"restarting": restart_scheduled,
			})),
			error: None,
		},
		None => AgentResponse {
			id: req_id,
			status: "error".to_string(),
			data: None,
			error: Some("Agent update failed".to_string()),
		},
	};

	write_response(writer, &final_res).await.is_ok()
}

async fn handle_command(
	req: AgentRequest,
	store: &SnapshotStore,
	config: &crate::config::Config,
) -> AgentResponse {
	let AgentRequest { id, command, params } = req;
	tracing::info!("Processing command: {} (id: {})", command, id);

	match command.as_str() {
		"telemetry.get_latest" => match store.get_latest().await {
			Ok(Some(snapshot)) => match serde_json::to_value(snapshot) {
				Ok(data) => AgentResponse {
					id,
					status: "ok".to_string(),
					data: Some(data),
					error: None,
				},
				Err(e) => AgentResponse {
					id,
					status: "error".to_string(),
					data: None,
					error: Some(format!("Serialization error: {}", e)),
				},
			},
			Ok(None) => AgentResponse {
				id,
				status: "ok".to_string(),
				data: None,
				error: None,
			},
			Err(e) => AgentResponse {
				id,
				status: "error".to_string(),
				data: None,
				error: Some(e),
			},
		},

		"telemetry.get_range" => {
			let since = params.get("since").and_then(|v| v.as_u64()).unwrap_or(0);
			let until = params.get("until").and_then(|v| v.as_u64()).unwrap_or(u64::MAX);
			let limit = params.get("limit").and_then(|v| v.as_u64()).map(|n| n as usize);
			match store.get_range(since, until, limit).await {
				Ok(snapshots) => match serde_json::to_value(snapshots) {
					Ok(data) => AgentResponse {
						id,
						status: "ok".to_string(),
						data: Some(data),
						error: None,
					},
					Err(e) => AgentResponse {
						id,
						status: "error".to_string(),
						data: None,
						error: Some(format!("Serialization error: {}", e)),
					},
				},
				Err(e) => AgentResponse {
					id,
					status: "error".to_string(),
					data: None,
					error: Some(e),
				},
			}
		}

		"telemetry.get_info" => {
			let bounds = store.get_bounds().await;
			AgentResponse {
				id,
				status: "ok".to_string(),
				data: Some(serde_json::json!({
					"oldest_timestamp": bounds.oldest,
					"newest_timestamp": bounds.newest,
					"snapshot_count": bounds.count,
					"retention_days": config.agent.retention_days,
					"interval_seconds": config.telemetry.interval_seconds,
				})),
				error: None,
			}
		}

		"telemetry.get_bulk" => {
			let hours = params.get("hours").and_then(|v| v.as_u64()).unwrap_or(1);
			let limit = params.get("limit").and_then(|v| v.as_u64()).map(|n| n as usize);
			let now = std::time::SystemTime::now()
				.duration_since(std::time::UNIX_EPOCH)
				.unwrap_or_default()
				.as_secs();
			let since = now.saturating_sub(hours * 3600);
			match store.get_range(since, now, limit).await {
				Ok(snapshots) => match serde_json::to_value(snapshots) {
					Ok(data) => AgentResponse {
						id,
						status: "ok".to_string(),
						data: Some(data),
						error: None,
					},
					Err(e) => AgentResponse {
						id,
						status: "error".to_string(),
						data: None,
						error: Some(format!("Serialization error: {}", e)),
					},
				},
				Err(e) => AgentResponse {
					id,
					status: "error".to_string(),
					data: None,
					error: Some(e),
				},
			}
		}

		"agent.version" => {
			if let Some(client_name) = params.get("client_name").and_then(|v| v.as_str()) {
				tracing::info!("New connection from {}", client_name);
			}
			AgentResponse {
				id,
				status: "ok".to_string(),
				data: Some(serde_json::json!({ "version": env!("CARGO_PKG_VERSION") })),
				error: None,
			}
		}

		cmd => AgentResponse {
			id,
			status: "error".to_string(),
			data: None,
			error: Some(format!("Unknown command: {}", cmd)),
		},
	}
}

fn handle_auth_request(
	req: AgentRequest,
	config: &crate::config::Config,
	pending_nonce: &mut Option<(Instant, [u8; 32])>,
	current_pubkey: &mut Option<VerifyingKey>,
) -> AgentResponse {
	let id = req.id;

	let pubkey_hex = match req.params.get("pubkey").and_then(|v| v.as_str()) {
		Some(pk) => pk,
		None => {
			return AgentResponse {
				id,
				status: "error".to_string(),
				data: None,
				error: Some("Missing pubkey".to_string()),
			}
		}
	};

	if !config.agent.allowed_keys.contains(&pubkey_hex.to_string()) {
		return AgentResponse {
			id,
			status: "error".to_string(),
			data: None,
			error: Some("Public key not allowed".to_string()),
		};
	}

	let pubkey_bytes = match hex::decode(pubkey_hex) {
		Ok(b) => b,
		Err(_) => {
			return AgentResponse {
				id,
				status: "error".to_string(),
				data: None,
				error: Some("Invalid pubkey hex".to_string()),
			}
		}
	};

	let key_arr: [u8; 32] = match pubkey_bytes.as_slice().try_into() {
		Ok(a) => a,
		Err(_) => {
			return AgentResponse {
				id,
				status: "error".to_string(),
				data: None,
				error: Some("Public key must be 32 bytes".to_string()),
			}
		}
	};

	let pubkey = match VerifyingKey::from_bytes(&key_arr) {
		Ok(pk) => pk,
		Err(_) => {
			return AgentResponse {
				id,
				status: "error".to_string(),
				data: None,
				error: Some("Invalid pubkey format".to_string()),
			}
		}
	};

	let mut nonce = [0u8; 32];
	if let Err(e) = getrandom::fill(&mut nonce) {
		return AgentResponse {
			id,
			status: "error".to_string(),
			data: None,
			error: Some(format!("Failed to generate nonce: {}", e)),
		};
	}

	*pending_nonce = Some((Instant::now(), nonce));
	*current_pubkey = Some(pubkey);

	AgentResponse {
		id,
		status: "ok".to_string(),
		data: Some(serde_json::json!({ "nonce": hex::encode(nonce) })),
		error: None,
	}
}

fn handle_auth_verify(
	req: AgentRequest,
	authenticated: &mut bool,
	pending_nonce: &mut Option<(Instant, [u8; 32])>,
	current_pubkey: &Option<VerifyingKey>,
) -> AgentResponse {
	let id = req.id;

	let (issued_at, nonce) = match pending_nonce.take() {
		Some(n) => n,
		None => {
			return AgentResponse {
				id,
				status: "error".to_string(),
				data: None,
				error: Some("No pending authentication request".to_string()),
			}
		}
	};

	if issued_at.elapsed() > AUTH_NONCE_TTL {
		return AgentResponse {
			id,
			status: "error".to_string(),
			data: None,
			error: Some("Authentication challenge expired".to_string()),
		};
	}

	let pubkey = match current_pubkey {
		Some(pk) => pk,
		None => {
			return AgentResponse {
				id,
				status: "error".to_string(),
				data: None,
				error: Some("No pending authentication request".to_string()),
			}
		}
	};

	let sig_hex = match req.params.get("signature").and_then(|v| v.as_str()) {
		Some(s) => s,
		None => {
			return AgentResponse {
				id,
				status: "error".to_string(),
				data: None,
				error: Some("Missing signature".to_string()),
			}
		}
	};

	let sig_bytes = match hex::decode(sig_hex) {
		Ok(b) => b,
		Err(_) => {
			return AgentResponse {
				id,
				status: "error".to_string(),
				data: None,
				error: Some("Invalid signature hex".to_string()),
			}
		}
	};

	let sig_arr: [u8; 64] = match sig_bytes.as_slice().try_into() {
		Ok(a) => a,
		Err(_) => {
			return AgentResponse {
				id,
				status: "error".to_string(),
				data: None,
				error: Some("Signature must be 64 bytes".to_string()),
			}
		}
	};

	let signature = Signature::from_bytes(&sig_arr);

	if pubkey.verify_strict(&nonce, &signature).is_ok() {
		*authenticated = true;
		AgentResponse {
			id,
			status: "ok".to_string(),
			data: None,
			error: None,
		}
	} else {
		AgentResponse {
			id,
			status: "error".to_string(),
			data: None,
			error: Some("Invalid signature".to_string()),
		}
	}
}

async fn write_response(
	writer: &mut tokio::io::WriteHalf<UnixStream>,
	response: &AgentResponse,
) -> Result<(), ()> {
	let json = match serde_json::to_string(response) {
		Ok(j) => j,
		Err(e) => {
			tracing::error!("Failed to serialize response: {}", e);
			return Ok(());
		}
	};
	writer.write_all(json.as_bytes()).await.map_err(|_| ())?;
	writer.write_all(b"\n").await.map_err(|_| ())?;
	writer.flush().await.map_err(|_| ())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::config::{AgentConfig, Config, TelemetryConfig, UpdateConfig};
	use crate::storage::SnapshotStore;
	use crate::telemetry::{Cpu, LoadAverage, MemoryStats, TelemetrySnapshot};
	use ed25519_dalek::{SigningKey, Signer};
	use serde_json::Value;
	use std::time::{SystemTime, UNIX_EPOCH};
	use std::sync::Arc;
	use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
	use tokio::net::UnixStream;

	fn temp_base_dir(name: &str) -> std::path::PathBuf {
		let now_ms = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or_default()
			.as_millis();
		std::env::temp_dir().join(format!("nulnet-test-{name}-{now_ms}"))
	}

	fn sample_snapshot(ts: u64) -> TelemetrySnapshot {
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

	async fn send_line(
		writer: &mut tokio::io::WriteHalf<UnixStream>,
		value: &Value,
	) {
		let s = value.to_string();
		writer.write_all(s.as_bytes()).await.unwrap();
		writer.write_all(b"\n").await.unwrap();
		writer.flush().await.unwrap();
	}

	async fn read_one_response(
		reader: &mut BufReader<tokio::io::ReadHalf<UnixStream>>,
		timeout_secs: u64,
	) -> AgentResponse {
		let mut line = String::new();
		let n = tokio::time::timeout(
			std::time::Duration::from_secs(timeout_secs),
			reader.read_line(&mut line),
		)
		.await
		.unwrap()
		.unwrap();
		assert!(n > 0, "expected at least one response line");
		serde_json::from_str::<AgentResponse>(&line).unwrap()
	}

	#[tokio::test]
	async fn auth_commands_and_update_up_to_date() {
		let base = temp_base_dir("transport");
		let _ = std::fs::create_dir_all(&base);

		let data_dir = base.join("data");
		let cdn_dir = base.join("release");
		let socket_path = base.join("nulnet.sock");
		let allowed_keys_path = base.join("allowed_keys");

		// Auth keypair: deterministic seed to keep tests stable.
		let secret = [7u8; 32];
		let keypair = SigningKey::from_bytes(&secret);
		let pub_hex = hex::encode(keypair.verifying_key().as_bytes());

		// Create local "release" files for up-to-date checking.
		// We set version.txt to match the current agent version, so no install
		// occurs and the test doesn't need permission for /opt/nulnet.
		let _ = std::fs::create_dir_all(&cdn_dir);
		let version_txt = cdn_dir.join("version.txt");
		tokio::fs::write(&version_txt, env!("CARGO_PKG_VERSION"))
			.await
			.unwrap();

		let cdn_base = format!("file://{}", cdn_dir.display());

		// Prepare store data for command coverage.
		let store = SnapshotStore::new(&data_dir);
		store.save(&sample_snapshot(100)).await.unwrap();
		store.save(&sample_snapshot(200)).await.unwrap();
		store.save(&sample_snapshot(150)).await.unwrap();

		let config = Arc::new(Config {
			agent: AgentConfig {
				data_dir: data_dir.to_string_lossy().to_string(),
				retention_days: 5,
				socket_path: socket_path.to_string_lossy().to_string(),
				allowed_keys: vec![pub_hex.clone()],
			},
			telemetry: TelemetryConfig { interval_seconds: 30 },
			update: UpdateConfig { cdn: Some(cdn_base) },
		});

		let server_handle = tokio::spawn(start_server(
			Arc::new(store),
			config.clone(),
		));

		// Give the server a moment to bind.
		tokio::time::sleep(std::time::Duration::from_millis(50)).await;

		let stream = UnixStream::connect(&socket_path).await.unwrap();
		let (read_half, mut write_half) = tokio::io::split(stream);
		let mut reader =
			BufReader::new(read_half);

		// Not authenticated yet.
		let unauth = serde_json::json!({
			"id": "unauth",
			"command": "telemetry.get_latest",
			"params": {}
		});
		send_line(&mut write_half, &unauth).await;
		let res = read_one_response(&mut reader, 5).await;
		assert_eq!(res.status, "error");
		assert_eq!(res.error.as_deref(), Some("Not authenticated"));

		// Auth request -> receive nonce.
		let req_pub = serde_json::json!({
			"id": "auth_req",
			"command": "agent.auth.request",
			"params": { "pubkey": pub_hex }
		});
		send_line(&mut write_half, &req_pub).await;
		let nonce_res = read_one_response(&mut reader, 5).await;
		assert_eq!(nonce_res.status, "ok");
		let nonce_hex = nonce_res
			.data
			.as_ref()
			.and_then(|d| d.get("nonce").and_then(|v| v.as_str()))
			.unwrap();
		let nonce_bytes = hex::decode(nonce_hex).unwrap();

		let signature: ed25519_dalek::Signature =
			keypair.sign(&nonce_bytes);
		let sig_hex = hex::encode(signature.to_bytes());

		// Auth verify.
		let verify = serde_json::json!({
			"id": "auth_verify",
			"command": "agent.auth.verify",
			"params": { "signature": sig_hex }
		});
		send_line(&mut write_half, &verify).await;
		let verify_res = read_one_response(&mut reader, 5).await;
		assert_eq!(verify_res.status, "ok");
		assert!(verify_res.error.is_none());

		// agent.update should stream output and then return up_to_date=true.
		let update_req = serde_json::json!({
			"id": "update",
			"command": "agent.update",
			"params": {}
		});
		send_line(&mut write_half, &update_req).await;

		let mut final_ok: Option<AgentResponse> = None;
		for _ in 0..10 {
			let res = read_one_response(&mut reader, 5).await;
			if res.status == "streaming" {
				continue;
			}
			final_ok = Some(res);
			break;
		}
		let final_ok = final_ok.expect("expected a final agent.update response");
		assert_eq!(final_ok.status, "ok");
		assert_eq!(
			final_ok.data.as_ref().and_then(|d| d.get("up_to_date")).and_then(|v| v.as_bool()),
			Some(true)
		);

		// telemetry.get_latest should return the newest snapshot (200).
		let latest_req = serde_json::json!({
			"id": "latest",
			"command": "telemetry.get_latest",
			"params": {}
		});
		send_line(&mut write_half, &latest_req).await;
		let latest_res = read_one_response(&mut reader, 5).await;
		assert_eq!(latest_res.status, "ok");
		let snapshot_ts = latest_res
			.data
			.as_ref()
			.and_then(|d| d.get("timestamp"))
			.and_then(|v| v.as_u64())
			.unwrap();
		assert_eq!(snapshot_ts, 200);

		// telemetry.get_range should filter by timestamp.
		let range_req = serde_json::json!({
			"id": "range",
			"command": "telemetry.get_range",
			"params": { "since": 140, "until": 210, "limit": 10 }
		});
		send_line(&mut write_half, &range_req).await;
		let range_res = read_one_response(&mut reader, 5).await;
		assert_eq!(range_res.status, "ok");
		let arr = range_res.data.as_ref().and_then(|d| d.as_array()).unwrap();
		let ts: Vec<u64> = arr.iter().filter_map(|v| v.get("timestamp").and_then(|x| x.as_u64())).collect();
		assert!(ts.contains(&150));
		assert!(ts.contains(&200));

		// telemetry.get_info returns bounds and config values.
		let info_req = serde_json::json!({
			"id": "info",
			"command": "telemetry.get_info",
			"params": {}
		});
		send_line(&mut write_half, &info_req).await;
		let info_res = read_one_response(&mut reader, 5).await;
		assert_eq!(info_res.status, "ok");
		let oldest = info_res.data.as_ref().and_then(|d| d.get("oldest_timestamp")).and_then(|v| v.as_u64()).unwrap();
		let newest = info_res.data.as_ref().and_then(|d| d.get("newest_timestamp")).and_then(|v| v.as_u64()).unwrap();
		let count = info_res.data.as_ref().and_then(|d| d.get("snapshot_count")).and_then(|v| v.as_u64()).unwrap();
		assert_eq!(oldest, 100);
		assert_eq!(newest, 200);
		assert_eq!(count, 3);

		// Cleanup.
		server_handle.abort();
		let _ = tokio::fs::remove_dir_all(&base).await;
		let _ = tokio::fs::remove_dir_all(&allowed_keys_path).await;
	}
}
