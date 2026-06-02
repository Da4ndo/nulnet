use serde::Deserialize;
use std::fs;
use std::path::Path;

const DEFAULT_GITHUB_REPO: &str = "nulnet/nulnet";

#[derive(Debug, Deserialize, Default)]
pub struct Config {
	#[serde(default)]
	pub agent: AgentConfig,
	#[serde(default)]
	pub telemetry: TelemetryConfig,
	#[serde(default)]
	pub update: UpdateConfig,
}

/// Update source: set `cdn` to use a custom CDN; omit it to use GitHub Releases.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct UpdateConfig {
	pub cdn: Option<String>,
}

impl UpdateConfig {
	pub fn uses_cdn(&self) -> bool {
		self.cdn
			.as_ref()
			.is_some_and(|s| !s.trim().is_empty())
	}

	fn cdn_base(&self) -> &str {
		self.cdn.as_ref().map(|s| s.trim()).unwrap_or("")
	}

	pub fn github_latest_api_url() -> String {
		format!(
			"https://api.github.com/repos/{}/releases/latest",
			DEFAULT_GITHUB_REPO
		)
	}

	pub fn github_download_base() -> String {
		format!(
			"https://github.com/{}/releases/latest/download",
			DEFAULT_GITHUB_REPO
		)
	}

	pub fn cdn_binary_url(&self) -> String {
		format!("{}/nulnet", self.cdn_base().trim_end_matches('/'))
	}

	pub fn cdn_version_url(&self) -> String {
		format!("{}/version.txt", self.cdn_base().trim_end_matches('/'))
	}

	pub fn cdn_checksum_url(&self) -> String {
		format!("{}/nulnet.sha256", self.cdn_base().trim_end_matches('/'))
	}
}

#[derive(Debug, Deserialize)]
pub struct AgentConfig {
	pub data_dir: String,
	pub retention_days: u64,
	pub socket_path: String,
	#[serde(default)]
	pub allowed_keys: Vec<String>,
}

impl Default for AgentConfig {
	fn default() -> Self {
		if cfg!(debug_assertions) {
			Self {
				data_dir: "./data".to_string(),
				retention_days: 5,
				socket_path: "./nulnet.sock".to_string(),
				allowed_keys: vec![],
			}
		} else {
			Self {
				data_dir: "/opt/nulnet/data".to_string(),
				retention_days: 5,
				socket_path: "/opt/nulnet/nulnet.sock".to_string(),
				allowed_keys: vec![],
			}
		}
	}
}

#[derive(Debug, Deserialize)]
pub struct TelemetryConfig {
	pub interval_seconds: u64,
}

impl Default for TelemetryConfig {
	fn default() -> Self {
		Self {
			interval_seconds: 30,
		}
	}
}

pub fn parse_config(content: &str) -> Result<Config, toml::de::Error> {
	toml::from_str(content)
}

pub fn load_config() -> Config {
	let path_str = if cfg!(debug_assertions) {
		"./config.development.toml"
	} else {
		"/opt/nulnet/config.toml"
	};

	let path = Path::new(path_str);
	if path.exists() {
		match fs::read_to_string(path) {
			Ok(content) => {
				match parse_config(&content) {
					Ok(config) => return config,
					Err(e) => {
						// A broken config is dangerous in production (e.g. empty allowed_keys
						// would lock out all clients). Hard-abort so the operator is alerted.
						if !cfg!(debug_assertions) {
							eprintln!("FATAL: Failed to parse {}: {}", path_str, e);
							std::process::exit(1);
						}
						tracing::warn!("Failed to parse {}, using defaults: {}", path_str, e);
					}
				}
			}
			Err(e) => {
				tracing::warn!("Failed to read {}, using defaults: {}", path_str, e);
			}
		}
	} else {
		tracing::info!("{} not found, using default configuration", path_str);
	}

	Config::default()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parse_config_defaults_without_update_section() {
		let content = r#"
[agent]
data_dir = "/opt/nulnet/data"
retention_days = 5
socket_path = "/opt/nulnet/nulnet.sock"
allowed_keys = []

[telemetry]
interval_seconds = 30
"#;
		let config = parse_config(content).unwrap();
		assert!(!config.update.uses_cdn());
	}

	#[test]
	fn parse_config_reads_cdn_when_set() {
		let content = r#"
[agent]
data_dir = "/opt/nulnet/data"
retention_days = 5
socket_path = "/opt/nulnet/nulnet.sock"
allowed_keys = []

[telemetry]
interval_seconds = 30

[update]
cdn = "https://cdn.example.com/nulnet"
"#;
		let config = parse_config(content).unwrap();
		assert!(config.update.uses_cdn());
		assert_eq!(
			config.update.cdn.as_deref(),
			Some("https://cdn.example.com/nulnet")
		);
	}
}
