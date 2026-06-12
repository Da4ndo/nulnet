pub const GITHUB_REPO: &str = "Da4ndo/nulnet";

/// Release artifact basename for the running CPU (e.g. `nulnet`, `nulnet-aarch64`).
pub fn binary_artifact_name() -> Result<&'static str, String> {
	match std::env::consts::ARCH {
		"x86_64" => Ok("nulnet"),
		"aarch64" => Ok("nulnet-aarch64"),
		arch => Err(format!(
			"unsupported CPU architecture for prebuilt binaries: {arch}"
		)),
	}
}

pub fn checksum_artifact_name() -> Result<String, String> {
	Ok(format!("{}.sha256", binary_artifact_name()?))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn binary_artifact_name_matches_host() {
		let name = binary_artifact_name().unwrap();
		match std::env::consts::ARCH {
			"x86_64" => assert_eq!(name, "nulnet"),
			"aarch64" => assert_eq!(name, "nulnet-aarch64"),
			_ => {}
		}
	}
}
