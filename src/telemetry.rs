use serde::{Deserialize, Serialize};
use sysinfo::{CpuRefreshKind, Disks, System};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio::time::{timeout, Duration};

// ─── Snapshot ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct TelemetrySnapshot {
	pub timestamp: u64,
	pub os: Option<String>,
	pub cpu: Cpu,
	pub gpu: Vec<GpuInfo>,
	pub memory: MemoryStats,
	pub disk: Vec<DiskStats>,
	#[serde(default)]
	pub containers: Vec<DockerContainer>,
	pub uptime_seconds: u64,
	pub load_average: LoadAverage,
	/// Human-readable on-disk size of persisted telemetry JSON cache.
	#[serde(default = "default_cached_telemetry_size")]
	pub cached_telemetry_size: String,
}

fn default_cached_telemetry_size() -> String {
	"0 B".to_string()
}

impl TelemetrySnapshot {
	pub fn set_cached_telemetry_disk(&mut self, bytes: u64) {
		self.cached_telemetry_size = format_dynamic_size(bytes);
	}
}

// ─── Sub-types ───────────────────────────────────────────────────────────────

/// All CPU metrics grouped under a single key.
#[derive(Debug, Serialize, Deserialize)]
pub struct Cpu {
	/// Core usage 0–100.
	pub usage: f64,
	pub info: Option<CpuInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuInfo {
	/// Cleaned model name, e.g. "Intel Xeon E5-2680 v4".
	pub model: String,
	/// Physical (core) count.
	pub cores: u32,
	/// Logical (thread) count.
	pub threads: u32,
	/// Base frequency in MHz as reported by the OS.
	pub frequency_mhz: u64,
	/// Human-readable summary, e.g. "Intel Xeon E5-2680 v4 @ 4x 2.397GHz".
	pub display: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GpuInfo {
	pub name: String,
	/// GPU core usage 0–100.
	pub usage: f64,
	/// VRAM used, formatted (e.g. "4.73 GB").
	pub vram_used: String,
	/// VRAM total, formatted (e.g. "16.00 GB").
	pub vram_total: String,
	/// VRAM usage 0–100.
	pub vram_usage: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MemoryStats {
	/// Usage 0–100.
	pub usage: f64,
	pub used_gb: f64,
	pub total_gb: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DiskStats {
	pub name: String,
	/// Usage 0–100.
	pub usage: f64,
	pub used_size: String,
	pub total_size: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LoadAverage {
	pub one: f64,
	pub five: f64,
	pub fifteen: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerContainer {
	pub name: String,
	pub status: String,
	pub created: String,
	pub image: String,
	pub ports: String,
}

// ─── Collector ───────────────────────────────────────────────────────────────

pub struct TelemetryCollector {
	sys: System,
	disks: Disks,
	/// Collected once at startup — CPU topology and model don't change.
	cpu_info: Option<CpuInfo>,
	/// Collected once at startup — OS name doesn't change.
	os_info: Option<String>,
}

impl Default for TelemetryCollector {
	fn default() -> Self {
		Self::new()
	}
}

impl TelemetryCollector {
	pub fn new() -> Self {
		let mut sys = System::new();
		// Populate brand + frequency once; subsequent collect() calls only
		// refresh CPU usage which is much cheaper.
		sys.refresh_cpu_list(CpuRefreshKind::everything());
		sys.refresh_memory();

		let cpu_info = build_cpu_info(&sys);
		let os_info = collect_os_info();
		let disks = Disks::new_with_refreshed_list();

		Self { sys, disks, cpu_info, os_info }
	}

	pub async fn collect(&mut self) -> TelemetrySnapshot {
		self.sys.refresh_cpu_usage();
		self.sys.refresh_memory();
		self.disks.refresh(true);

		let cpu_usage = round2(self.sys.global_cpu_usage() as f64);
		let memory = build_memory_stats(&self.sys);
		let disk_stats = build_disk_stats(&self.disks);
		let load_avg = System::load_average();
		let gpu = collect_gpu_info().await;
		let containers = collect_docker_containers().await;

		TelemetrySnapshot {
			timestamp: SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.unwrap_or_default()
				.as_secs(),
			os: self.os_info.clone(),
			cpu: Cpu {
				usage: cpu_usage,
				info: self.cpu_info.clone(),
			},
			gpu,
			memory,
			disk: disk_stats,
			containers,
			uptime_seconds: System::uptime(),
			load_average: LoadAverage {
				one: load_avg.one,
				five: load_avg.five,
				fifteen: load_avg.fifteen,
			},
			cached_telemetry_size: format_dynamic_size(0),
		}
	}
}

// ─── Memory / disk builders ──────────────────────────────────────────────────

fn build_memory_stats(sys: &System) -> MemoryStats {
	let total_mem = sys.total_memory();
	let used_mem = sys.used_memory();
	let mem_usage = if total_mem > 0 {
		round2((used_mem as f64 / total_mem as f64) * 100.0)
	} else {
		0.0
	};
	MemoryStats {
		usage: mem_usage,
		used_gb: round2(bytes_to_gb(used_mem)),
		total_gb: round2(bytes_to_gb(total_mem)),
	}
}

fn build_disk_stats(disks: &Disks) -> Vec<DiskStats> {
	disks
		.list()
		.iter()
		.filter(|d| should_report_disk(d))
		.map(disk_to_stats)
		.collect()
}

fn disk_to_stats(disk: &sysinfo::Disk) -> DiskStats {
	let total = disk.total_space();
	let used = total.saturating_sub(disk.available_space());
	DiskStats {
		name: disk_display_name(disk),
		usage: round2((used as f64 / total as f64) * 100.0),
		used_size: format_dynamic_size(used),
		total_size: format_dynamic_size(total),
	}
}

// ─── CPU helpers ─────────────────────────────────────────────────────────────

fn build_cpu_info(sys: &System) -> Option<CpuInfo> {
	let cpus = sys.cpus();
	if cpus.is_empty() {
		return None;
	}
	let raw_brand = cpus[0].brand();
	if raw_brand.is_empty() {
		return None;
	}
	let model = clean_cpu_brand(raw_brand);
	let threads = cpus.len() as u32;
	let cores = System::physical_core_count().unwrap_or(threads as usize) as u32;
	let frequency_mhz = cpus[0].frequency();
	let display = format!(
		"{} @ {}x {:.3}GHz",
		model, cores,
		frequency_mhz as f64 / 1000.0,
	);
	Some(CpuInfo { model, cores, threads, frequency_mhz, display })
}

/// Strip Intel/AMD trademark noise and redundant words from brand strings.
fn clean_cpu_brand(brand: &str) -> String {
	let s = brand
		.replace("(R)", "")
		.replace("(TM)", "")
		.replace("(r)", "")
		.replace("(tm)", "")
		.replace("CPU ", "")
		.replace("Processor", "");
	let s: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
	// Strip trailing "@ X.XXGHz" — we report frequency from the live measurement.
	if let Some(idx) = s.find('@') {
		s[..idx].trim().to_string()
	} else {
		s.trim().to_string()
	}
}

// ─── OS helper ───────────────────────────────────────────────────────────────

fn collect_os_info() -> Option<String> {
	if let Ok(content) = std::fs::read_to_string("/etc/os-release")
		&& let Some(name) = parse_os_release(&content)
	{
		return Some(name);
	}
	parse_sysinfo_os()
}

fn parse_os_release(content: &str) -> Option<String> {
	for line in content.lines() {
		if let Some(val) = line.strip_prefix("PRETTY_NAME=") {
			return Some(val.trim_matches('"').to_string());
		}
	}
	None
}

fn parse_sysinfo_os() -> Option<String> {
	let name = System::name()?;
	let version = System::os_version().unwrap_or_default();
	if version.is_empty() {
		Some(name)
	} else {
		Some(format!("{} {}", name, version))
	}
}

// ─── GPU helpers ─────────────────────────────────────────────────────────────

/// Detect GPUs and their current utilization.
///
/// Queries `nvidia-smi` for NVIDIA GPUs, then `rocm-smi` for AMD.
/// Returns an empty vec when no compatible tool is found.
async fn collect_gpu_info() -> Vec<GpuInfo> {
	let gpus = collect_nvidia_gpus().await;
	if !gpus.is_empty() {
		return gpus;
	}
	collect_amd_gpus().await
}

async fn collect_nvidia_gpus() -> Vec<GpuInfo> {
	let result = timeout(
		Duration::from_secs(5),
		Command::new("nvidia-smi")
			.args([
				"--query-gpu=name,utilization.gpu,memory.used,memory.total",
				"--format=csv,noheader,nounits",
			])
			.stdout(Stdio::piped())
			.stderr(Stdio::null())
			.output(),
	)
	.await;

	let output = match result {
		Ok(Ok(o)) if o.status.success() => o,
		_ => return vec![],
	};

	String::from_utf8_lossy(&output.stdout)
		.lines()
		.filter_map(parse_nvidia_smi_line)
		.collect()
}

fn parse_nvidia_smi_line(line: &str) -> Option<GpuInfo> {
	let mut parts = line.splitn(4, ',').map(str::trim);
	let name = parts.next()?.to_string();
	let usage = parts.next()?.parse::<f64>().ok()?;
	let vram_used_mib = parts.next()?.parse::<u64>().ok()?;
	let vram_total_mib = parts.next()?.parse::<u64>().ok()?;
	let vram_usage = if vram_total_mib > 0 {
		round2((vram_used_mib as f64 / vram_total_mib as f64) * 100.0)
	} else {
		0.0
	};
	Some(GpuInfo {
		name,
		usage,
		vram_used: format_dynamic_size(vram_used_mib * 1024 * 1024),
		vram_total: format_dynamic_size(vram_total_mib * 1024 * 1024),
		vram_usage,
	})
}

async fn collect_amd_gpus() -> Vec<GpuInfo> {
	let result = timeout(
		Duration::from_secs(5),
		Command::new("rocm-smi")
			.args(["--showuse", "--showmemuse", "--showproductname", "--csv"])
			.stdout(Stdio::piped())
			.stderr(Stdio::null())
			.output(),
	)
	.await;

	let output = match result {
		Ok(Ok(o)) if o.status.success() => o,
		_ => return vec![],
	};

	parse_rocm_smi_csv(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `rocm-smi --csv` output.
/// Expected header contains: "card", "GPU use (%)", "GPU memory use (%)"
fn parse_rocm_smi_csv(text: &str) -> Vec<GpuInfo> {
	let mut lines = text.lines();
	let header = match lines.next() {
		Some(h) => h.to_lowercase(),
		None => return vec![],
	};

	let cols: Vec<&str> = header.split(',').collect();
	let find = |kw: &str| cols.iter().position(|c| c.contains(kw));

	let (Some(ni), Some(ui), Some(mi)) = (find("card"), find("gpu use"), find("gpu memory use"))
	else {
		return vec![];
	};

	lines
		.filter_map(|line| {
			let parts: Vec<&str> = line.split(',').collect();
			let name = parts.get(ni)?.trim().to_string();
			let usage = parts.get(ui)?.trim().parse::<f64>().ok()?;
			let vram_usage = round2(parts.get(mi)?.trim().parse::<f64>().ok()?);
			Some(GpuInfo {
				name,
				usage,
				vram_used: "N/A".to_string(),
				vram_total: "N/A".to_string(),
				vram_usage,
			})
		})
		.collect()
}

// ─── Docker helpers ──────────────────────────────────────────────────────────

/// One line from `docker ps -a --format '{{json .}}'`.
#[derive(Deserialize)]
struct DockerPsRow {
	#[serde(default, rename = "Names")]
	names: String,
	#[serde(default, rename = "Status")]
	status: String,
	#[serde(default, rename = "CreatedAt")]
	created_at: String,
	#[serde(default, rename = "Image")]
	image: String,
	#[serde(default, rename = "Ports")]
	ports: String,
}

async fn collect_docker_containers() -> Vec<DockerContainer> {
	let result = timeout(
		Duration::from_secs(5),
		Command::new("docker")
			.args(["ps", "-a", "--format", "{{json .}}"])
			.stdout(Stdio::piped())
			.stderr(Stdio::null())
			.output(),
	)
	.await;

	let output = match result {
		Ok(Ok(o)) if o.status.success() => o,
		_ => return vec![],
	};

	parse_docker_ps_json(&String::from_utf8_lossy(&output.stdout))
}

fn parse_docker_ps_json(text: &str) -> Vec<DockerContainer> {
	text.lines().filter_map(parse_docker_ps_line).collect()
}

fn parse_docker_ps_line(line: &str) -> Option<DockerContainer> {
	let trimmed = line.trim();
	if trimmed.is_empty() {
		return None;
	}
	let row: DockerPsRow = serde_json::from_str(trimmed).ok()?;
	if row.names.is_empty() {
		return None;
	}
	Some(DockerContainer {
		name: row.names,
		status: row.status,
		created: row.created_at,
		image: row.image,
		ports: empty_ports_placeholder(&row.ports),
	})
}

fn empty_ports_placeholder(ports: &str) -> String {
	if ports.is_empty() {
		"—".to_string()
	} else {
		ports.to_string()
	}
}

// ─── Disk helpers ────────────────────────────────────────────────────────────

fn should_report_disk(disk: &sysinfo::Disk) -> bool {
	let total = disk.total_space();
	if total == 0 {
		return false;
	}
	let name = disk.name().to_string_lossy();
	let fs = disk.file_system().to_string_lossy();
	if name.eq_ignore_ascii_case("overlay") || fs.eq_ignore_ascii_case("overlay") {
		return false;
	}
	let skip_fs = ["tmpfs", "devtmpfs", "squashfs", "efivarfs", "autofs"];
	skip_fs.iter().all(|&s| !fs.eq_ignore_ascii_case(s))
}

fn disk_display_name(disk: &sysinfo::Disk) -> String {
	let name = disk.name().to_string_lossy();
	if !name.is_empty() {
		return name.to_string();
	}
	disk.mount_point().to_string_lossy().to_string()
}

// ─── Numeric / size helpers ───────────────────────────────────────────────────

#[inline]
fn round2(v: f64) -> f64 {
	(v * 100.0).round() / 100.0
}

#[inline]
fn bytes_to_gb(bytes: u64) -> f64 {
	bytes as f64 / 1_073_741_824.0
}

fn format_dynamic_size(bytes: u64) -> String {
	const KB: f64 = 1024.0;
	const MB: f64 = KB * 1024.0;
	const GB: f64 = MB * 1024.0;
	const TB: f64 = GB * 1024.0;
	let b = bytes as f64;
	if bytes == 0 {
		return "0 B".to_string();
	}
	if b < KB {
		format!("{} B", bytes)
	} else if b < MB {
		format!("{:.2} KB", b / KB)
	} else if b < GB {
		format!("{:.2} MB", b / MB)
	} else if b < TB {
		format!("{:.2} GB", b / GB)
	} else {
		format!("{:.2} TB", b / TB)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn clean_cpu_brand_strips_trademark_noise() {
		let brand = "Intel(R) Xeon(R) CPU E5-2680 v4 @ 2.40GHz";
		assert_eq!(clean_cpu_brand(brand), "Intel Xeon E5-2680 v4");
	}

	#[test]
	fn format_dynamic_size_units() {
		assert_eq!(format_dynamic_size(0), "0 B");
		assert_eq!(format_dynamic_size(512), "512 B");
		assert_eq!(format_dynamic_size(2048), "2.00 KB");
	}

	#[test]
	fn parse_nvidia_smi_line_valid() {
		let gpu = parse_nvidia_smi_line(
			"Tesla V100-SXM2-16GB, 45, 4844, 16384",
		)
		.unwrap();
		assert_eq!(gpu.name, "Tesla V100-SXM2-16GB");
		assert_eq!(gpu.usage, 45.0);
		assert_eq!(gpu.vram_usage, 29.57);
	}

	#[test]
	fn parse_nvidia_smi_line_rejects_short_row() {
		assert!(parse_nvidia_smi_line("GPU only").is_none());
	}

	#[test]
	fn parse_rocm_smi_csv_extracts_rows() {
		let csv = "card,GPU use (%),GPU memory use (%)\n\
			gfx0,12.0,34.5\n";
		let gpus = parse_rocm_smi_csv(csv);
		assert_eq!(gpus.len(), 1);
		assert_eq!(gpus[0].name, "gfx0");
		assert_eq!(gpus[0].usage, 12.0);
		assert_eq!(gpus[0].vram_usage, 34.5);
	}

	#[test]
	fn parse_rocm_smi_csv_rejects_missing_columns() {
		assert!(parse_rocm_smi_csv("card,other\nx,1").is_empty());
	}

	#[test]
	fn parse_docker_ps_line_maps_fields() {
		let line = r#"{"Names":"web","Status":"Up 2 hours","CreatedAt":"2024-01-01 12:00:00 +0000 UTC","Image":"nginx:latest","Ports":"0.0.0.0:80->80/tcp"}"#;
		let c = parse_docker_ps_line(line).unwrap();
		assert_eq!(c.name, "web");
		assert_eq!(c.status, "Up 2 hours");
		assert_eq!(c.created, "2024-01-01 12:00:00 +0000 UTC");
		assert_eq!(c.image, "nginx:latest");
		assert_eq!(c.ports, "0.0.0.0:80->80/tcp");
	}

	#[test]
	fn parse_docker_ps_line_empty_ports_placeholder() {
		let line = r#"{"Names":"db","Status":"Exited (0) 1 day ago","CreatedAt":"2024-01-01","Image":"postgres:16","Ports":""}"#;
		let c = parse_docker_ps_line(line).unwrap();
		assert_eq!(c.ports, "—");
	}

	#[test]
	fn parse_docker_ps_json_skips_blank_lines() {
		let text = "\n\n";
		assert!(parse_docker_ps_json(text).is_empty());
	}

	#[test]
	fn empty_ports_placeholder_formats() {
		assert_eq!(super::empty_ports_placeholder(""), "—");
		assert_eq!(super::empty_ports_placeholder("8080/tcp"), "8080/tcp");
	}

	#[test]
	fn parse_os_release_reads_pretty_name() {
		let content = "NAME=\"Debian\"\nPRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\n";
		assert_eq!(
			parse_os_release(content),
			Some("Debian GNU/Linux 13 (trixie)".to_string()),
		);
	}

	#[test]
	fn parse_os_release_missing_returns_none() {
		assert!(parse_os_release("NAME=\"Debian\"\n").is_none());
	}
}
