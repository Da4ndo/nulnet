use crate::config::UpdateConfig;
use sha2::{Digest, Sha256};
use std::process::Stdio;
use tokio::process::Command;
use tokio::sync::mpsc;

const INSTALL_PATH: &str = "/opt/nulnet/bin/nulnet";
const TMP_PATH: &str = "/tmp/nulnet";
const USER_AGENT: &str = concat!("nulnet/", env!("CARGO_PKG_VERSION"));

/// Returns `true` when `candidate` is strictly newer than `current` by semver.
///
/// Handles optional leading `v` prefix and tolerates missing patch component.
pub(crate) fn semver_is_newer(candidate: &str, current: &str) -> bool {
	fn parse(v: &str) -> (u64, u64, u64) {
		let v = v.trim().trim_start_matches('v');
		let mut parts = v.splitn(3, '.');
		let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
		let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
		let patch = parts
			.next()
			.and_then(|p| p.split('-').next())
			.and_then(|p| p.parse().ok())
			.unwrap_or(0);
		(major, minor, patch)
	}
	parse(candidate) > parse(current)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOutcome {
	UpToDate,
	Installed { restart_scheduled: bool },
}

struct UpdateUrls {
	version_check_url: String,
	binary_url: String,
	checksum_url: String,
	source_label: &'static str,
}

fn urls_for_config(config: &UpdateConfig) -> Result<UpdateUrls, String> {
	if config.uses_cdn() {
		Ok(UpdateUrls {
			version_check_url: config.cdn_version_url(),
			binary_url: config.cdn_binary_url()?,
			checksum_url: config.cdn_checksum_url()?,
			source_label: "CDN",
		})
	} else {
		let base = UpdateConfig::github_download_base();
		let artifact = crate::release::binary_artifact_name()?;
		let checksum = crate::release::checksum_artifact_name()?;
		Ok(UpdateUrls {
			version_check_url: UpdateConfig::github_latest_api_url(),
			binary_url: format!("{}/{}", base, artifact),
			checksum_url: format!("{}/{}", base, checksum),
			source_label: "GitHub",
		})
	}
}

fn dim(s: &str) -> String {
	format!("[dim]{}[/dim]", s)
}
fn green(s: &str) -> String {
	format!("[green]{}[/green]", s)
}
fn yellow(s: &str) -> String {
	format!("[yellow]{}[/yellow]", s)
}
fn red(s: &str) -> String {
	format!("[red]{}[/red]", s)
}

async fn line(tx: &mpsc::Sender<String>, msg: impl Into<String>) {
	let s = msg.into();
	tracing::info!("update: {}", s);
	let _ = tx.send(s).await;
}

async fn err_line(tx: &mpsc::Sender<String>, msg: impl Into<String>) {
	let raw = msg.into();
	tracing::error!("update: {}", raw);
	let _ = tx.send(red(&raw)).await;
}

pub async fn run_update_stream(
	tx: mpsc::Sender<String>,
	config: &UpdateConfig,
) -> Result<UpdateOutcome, String> {
	let current = env!("CARGO_PKG_VERSION");
	let urls = urls_for_config(config)?;

	line(&tx, dim("Checking for updates...")).await;
	let latest = match fetch_latest_version(config, &urls).await {
		Ok(v) if !v.is_empty() => v,
		Ok(_) => {
			err_line(
				&tx,
				format!("{} version info is empty.", urls.source_label),
			)
			.await;
			return Ok(UpdateOutcome::UpToDate);
		}
		Err(e) => {
			err_line(
				&tx,
				format!("{} unreachable: {}", urls.source_label, e),
			)
			.await;
			return Err(format!("{} fetch failed: {}", urls.source_label, e));
		}
	};

	if !semver_is_newer(&latest, current) {
		line(
			&tx,
			format!(
				"  {} {}  {}",
				dim("version"),
				current,
				green("up to date"),
			),
		)
		.await;
		return Ok(UpdateOutcome::UpToDate);
	}

	line(
		&tx,
		format!(
			"  {}  {}  {}  {}",
			dim("current"),
			current,
			dim("→"),
			latest,
		),
	)
	.await;
	line(&tx, "".to_string()).await;

	// Download binary to a fixed temp path.
	line(&tx, dim("  Downloading...")).await;
	let curl = Command::new("curl")
		.args([
			"-fSL",
			"--connect-timeout",
			"10",
			"--max-time",
			"120",
			"-A",
			USER_AGENT,
			"-o",
			TMP_PATH,
			&urls.binary_url,
		])
		.stdout(Stdio::null())
		.stderr(Stdio::piped())
		.output()
		.await
		.map_err(|e| format!("Failed to run curl: {}", e))?;

	if !curl.status.success() {
		let msg = format!(
			"Download failed: {}",
			String::from_utf8_lossy(&curl.stderr).trim()
		);
		err_line(&tx, &msg).await;
		return Err(msg);
	}

	// Verify SHA-256 checksum before touching the filesystem.
	match fetch_text(&urls.checksum_url).await {
		Ok(expected) if expected.len() == 64 => {
			let binary = tokio::fs::read(TMP_PATH)
				.await
				.map_err(|e| format!("Failed to read downloaded binary: {}", e))?;
			if !sha256_matches(&binary, &expected) {
				let _ = tokio::fs::remove_file(TMP_PATH).await;
				let msg = "Checksum mismatch — aborting.".to_string();
				err_line(&tx, &msg).await;
				return Err(msg);
			}
			line(&tx, dim("  Checksum verified")).await;
		}
		Ok(_) => line(&tx, yellow("  Checksum malformed, skipping verification")).await,
		Err(_) => line(&tx, dim("  Checksum unavailable, skipping")).await,
	}

	// chmod +x the downloaded binary.
	let chmod = Command::new("chmod")
		.args(["+x", TMP_PATH])
		.status()
		.await
		.map_err(|e| format!("chmod failed: {}", e))?;
	if !chmod.success() {
		let _ = tokio::fs::remove_file(TMP_PATH).await;
		let msg = format!("chmod +x {} failed", TMP_PATH);
		err_line(&tx, &msg).await;
		return Err(msg);
	}

	// Ensure install directory exists.
	let mkdir = Command::new("mkdir")
		.args(["-p", "/opt/nulnet/bin"])
		.status()
		.await
		.map_err(|e| format!("mkdir -p failed: {}", e))?;
	if !mkdir.success() {
		let _ = tokio::fs::remove_file(TMP_PATH).await;
		let msg = "Could not create /opt/nulnet/bin".to_string();
		err_line(&tx, &msg).await;
		return Err(msg);
	}

	// Atomically replace the running binary.
	let mv = Command::new("mv")
		.args(["-f", TMP_PATH, INSTALL_PATH])
		.status()
		.await
		.map_err(|e| format!("mv failed: {}", e))?;
	if !mv.success() {
		let _ = tokio::fs::remove_file(TMP_PATH).await;
		let msg = format!("Install failed: mv {} → {} failed", TMP_PATH, INSTALL_PATH);
		err_line(&tx, &msg).await;
		return Err(msg);
	}

	line(&tx, format!("  {} {}", dim("Installed"), latest)).await;
	line(&tx, "".to_string()).await;

	// Detached restart: break out of the process group so systemd stopping us
	// does not kill the background shell before it restarts.
	let spawn = Command::new("setsid")
		.args(["sh", "-c", "sleep 3 && sudo systemctl restart nulnet"])
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.spawn();

	let restart_scheduled = match spawn {
		Ok(_) => {
			line(&tx, dim("  Restarting in 3 s — connection will close.")).await;
			true
		}
		Err(e) => {
			err_line(
				&tx,
				format!(
					"Could not schedule restart: {}  Run: sudo systemctl restart nulnet",
					e
				),
			)
			.await;
			false
		}
	};

	Ok(UpdateOutcome::Installed { restart_scheduled })
}

async fn fetch_latest_version(
	config: &UpdateConfig,
	urls: &UpdateUrls,
) -> Result<String, String> {
	if config.uses_cdn() {
		fetch_text(&urls.version_check_url).await
	} else {
		github_latest_tag(&urls.version_check_url).await
	}
}

fn parse_github_tag(body: &str) -> Result<String, String> {
	let value: serde_json::Value = serde_json::from_str(body)
		.map_err(|e| format!("invalid GitHub API response: {}", e))?;
	let tag = value
		.get("tag_name")
		.and_then(|v| v.as_str())
		.ok_or_else(|| "GitHub API response missing tag_name".to_string())?;
	Ok(tag.trim().to_string())
}

fn sha256_matches(data: &[u8], expected: &str) -> bool {
	expected.len() == 64 && hex::encode(Sha256::digest(data)) == expected
}

async fn github_latest_tag(api_url: &str) -> Result<String, String> {
	let body = fetch_text(api_url).await?;
	parse_github_tag(&body)
}

/// Fetch a small text resource using curl with conservative timeouts.
async fn fetch_text(url: &str) -> Result<String, String> {
	let out = Command::new("curl")
		.args([
			"-fsSL",
			"--connect-timeout",
			"10",
			"--max-time",
			"30",
			"-A",
			USER_AGENT,
			url,
		])
		.output()
		.await
		.map_err(|e| format!("curl error: {}", e))?;

	if !out.status.success() {
		return Err(format!(
			"curl exited {}: {}",
			out.status,
			String::from_utf8_lossy(&out.stderr).trim()
		));
	}

	Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn semver_is_newer_compares_patch() {
		assert!(semver_is_newer("1.3.0", "1.2.0"));
		assert!(!semver_is_newer("1.2.0", "1.3.0"));
	}

	#[test]
	fn semver_is_newer_strips_v_prefix() {
		assert!(semver_is_newer("v2.0.0", "1.9.9"));
	}

	#[test]
	fn semver_is_newer_equal_returns_false() {
		assert!(!semver_is_newer("1.3.2", "1.3.2"));
	}

	#[test]
	fn semver_is_newer_handles_v_tagged_equal() {
		assert!(!semver_is_newer("v1.3.2", "1.3.2"));
	}

	#[test]
	fn parse_github_tag_reads_tag_name() {
		let body = r#"{"tag_name":"v1.4.0","name":"1.4.0"}"#;
		assert_eq!(parse_github_tag(body).unwrap(), "v1.4.0");
	}

	#[test]
	fn parse_github_tag_rejects_missing_field() {
		assert!(parse_github_tag(r#"{"name":"1.4.0"}"#).is_err());
	}

	#[test]
	fn sha256_matches_accepts_valid_digest() {
		let data = b"nulnet-binary";
		let digest = hex::encode(Sha256::digest(data));
		assert!(sha256_matches(data, &digest));
		assert!(!sha256_matches(data, "0".repeat(64).as_str()));
	}
}
