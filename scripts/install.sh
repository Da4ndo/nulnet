#!/usr/bin/env bash
# nulnet agent — standalone install (extracted from NOVA nova/setup.sh CDN path).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Da4ndo/nulnet/main/install.sh | bash
#   bash install.sh              # interactive
#   bash install.sh -y           # skip all confirmations (CI / automation)
#   bash install.sh --dry-run    # full checks + sudo auth; skip mutating commands
#
# Requires: curl, sha256sum or shasum, mktemp; privileged phase needs useradd
# or adduser, install, systemctl (Debian/Ubuntu style; Linux target for install).
# visudo is optional — used to validate sudoers when present.

set -euo pipefail

# Debian non-login shells often omit /usr/sbin (useradd, visudo, …).
export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:${PATH}"

ASSUME_YES=false
DRY_RUN=false
INSTALL_SUDOERS=""

for arg in "$@"; do
	case "$arg" in
		-y|--yes) ASSUME_YES=true ;;
		-n|--dry-run) DRY_RUN=true ;;
		-h|--help)
			cat <<'HELP'
Usage: install.sh [OPTIONS]

Options:
  -y, --yes       Skip confirmation prompts (CI / automation)
  -n, --dry-run   Run downloads, checks, and sudo auth; skip install writes
  -h, --help      Show this help

Environment:
  NULNET_INSTALL_ASSUME_YES=1   Same as -y
  NULNET_CDN_BASE=<url>         Use custom CDN; default is GitHub Releases (Da4ndo/nulnet)
  NULNET_INSTALL_SUDOERS=1|0    Install sudoers for self-update restart (skip prompt)
  NO_COLOR=1                    Disable ANSI colors
HELP
			exit 0
			;;
	esac
done

if [[ "${NULNET_INSTALL_ASSUME_YES:-}" == 1 ]]; then
	ASSUME_YES=true
fi

if [[ "${NULNET_INSTALL_SUDOERS:-}" == 1 ]]; then
	INSTALL_SUDOERS=1
elif [[ "${NULNET_INSTALL_SUDOERS:-}" == 0 ]]; then
	INSTALL_SUDOERS=0
fi

GITHUB_REPO="Da4ndo/nulnet"
CDN_BASE="${NULNET_CDN_BASE:-}"
NULNET_ROOT="/opt/nulnet"
NULNET_BIN="${NULNET_ROOT}/bin/nulnet"
NULNET_CONFIG="${NULNET_ROOT}/config.toml"
NULNET_SERVICE="/etc/systemd/system/nulnet.service"
NULNET_SUDOERS="/etc/sudoers.d/nulnet"

INVOKER="${SUDO_USER:-$USER}"

TMP_BIN=""
TMP_SVC=""

# --- colors ------------------------------------------------------------------

if [[ -t 1 ]] && [[ "${NO_COLOR:-}" != 1 ]]; then
	C_RESET=$'\033[0m'
	C_BOLD=$'\033[1m'
	C_DIM=$'\033[2m'
	C_CYAN=$'\033[36m'
	C_GREEN=$'\033[32m'
	C_YELLOW=$'\033[33m'
	C_RED=$'\033[31m'
	C_MAGENTA=$'\033[35m'
else
	C_RESET= C_BOLD= C_DIM= C_CYAN= C_GREEN= C_YELLOW= C_RED= C_MAGENTA=
fi

info()  { printf '%b\n' "${C_CYAN}→${C_RESET} $*"; }
ok()    { printf '%b\n' "${C_GREEN}✓${C_RESET} $*"; }
warn()  { printf '%b\n' "${C_YELLOW}⚠${C_RESET} $*"; }
skip()  { printf '%b\n' "${C_MAGENTA}⊘${C_RESET} ${C_DIM}dry-run — not run:${C_RESET} $*"; }
err()   { printf '%b\n' "${C_RED}✗${C_RESET} $*" >&2; }
hdr()   { printf '\n%b%b%s%b\n\n' "$C_BOLD" "$C_CYAN" "$*" "$C_RESET"; }
banner() {
	printf '%b\n' "${C_BOLD}${C_CYAN}"
	printf '  ╔══════════════════════════════════════════════════════════╗\n'
	printf '  ║  NULNET agent — installer%*s║\n' $((26 - ${#1})) "$1"
	printf '  ╚══════════════════════════════════════════════════════════╝\n'
	printf '%b\n' "$C_RESET"
}

is_root() { [[ "$(id -u)" -eq 0 ]]; }

has_useradd() { command -v useradd &>/dev/null; }
has_adduser() { command -v adduser &>/dev/null; }

can_create_users() {
	has_useradd || has_adduser
}

create_system_user_desc() {
	if has_useradd; then
		printf 'useradd --system --no-create-home --home-dir %s --shell /usr/sbin/nologin nulnet' \
			"$NULNET_ROOT"
	elif has_adduser; then
		printf 'adduser --system --no-create-home --home %s --shell /usr/sbin/nologin --disabled-password --gecos "" nulnet' \
			"$NULNET_ROOT"
	fi
}

answer_lower() {
	printf '%s' "$1" | tr '[:upper:]' '[:lower:]'
}

sha256_file() {
	local f="$1"
	if command -v sha256sum &>/dev/null; then
		sha256sum "$f" | awk '{print $1}'
	elif command -v shasum &>/dev/null; then
		shasum -a 256 "$f" | awk '{print $1}'
	else
		err "Need sha256sum or shasum for checksum verification"
		exit 1
	fi
}

uses_cdn() {
	[[ -n "${CDN_BASE// }" ]]
}

download_base() {
	if uses_cdn; then
		printf '%s' "${CDN_BASE%/}"
	else
		printf 'https://github.com/%s/releases/latest/download' "$GITHUB_REPO"
	fi
}

source_label() {
	if uses_cdn; then
		printf 'CDN'
	else
		printf 'GitHub Releases'
	fi
}

print_sudoers_snippet() {
	cat <<'SUDOERS'
# nulnet — self-update (no password)
nulnet ALL=(ALL) NOPASSWD: /usr/bin/systemctl restart nulnet
SUDOERS
}

# Run a command as root (current shell or sudo).
as_root() {
	if is_root; then
		"$@"
	else
		sudo "$@"
	fi
}

# Read-only root check — always runs (including dry-run).
inspect() {
	local label="$1"
	shift
	info "$label"
	as_root "$@"
}

# Mutating root action — skipped in dry-run.
mutate() {
	local label="$1"
	shift
	info "$label"
	if [[ "$DRY_RUN" == true ]]; then
		skip "$*"
		return 0
	fi
	as_root "$@"
}

prompt_yes_no() {
	local prompt="$1"
	local default_no="${2:-1}"
	local answer
	if [[ "$ASSUME_YES" == true ]]; then
		return 0
	fi
	local tty=/dev/tty
	if [[ ! -r "$tty" ]]; then
		err "No TTY for confirmation (e.g. piped stdin)."
		printf '%b\n' "${C_DIM}  Re-run attached to a terminal, or use -y / NULNET_INSTALL_ASSUME_YES=1.${C_RESET}" >&2
		return 1
	fi
	if [[ "$default_no" == 1 ]]; then
		printf '%b' "${C_BOLD}$prompt${C_RESET} ${C_DIM}[y/N]${C_RESET} "
		read -r answer <"$tty" || true
		case "$(answer_lower "$answer")" in
			y|yes) return 0 ;;
			*) return 1 ;;
		esac
	else
		printf '%b' "${C_BOLD}$prompt${C_RESET} ${C_DIM}[Y/n]${C_RESET} "
		read -r answer <"$tty" || true
		case "$(answer_lower "$answer")" in
			n|no) return 1 ;;
			*) return 0 ;;
		esac
	fi
}

prompt_sudoers() {
	if [[ -n "$INSTALL_SUDOERS" ]]; then
		return 0
	fi
	if [[ "$ASSUME_YES" == true ]]; then
		INSTALL_SUDOERS=1
		ok "Will install sudoers for self-update restart (-y)"
		return 0
	fi
	hdr "Sudoers for self-update"
	printf '%b\n' "${C_DIM}After a self-update, the agent runs:${C_RESET}"
	printf '%b\n' "  ${C_DIM}setsid sh -c \"sleep 3 && sudo systemctl restart nulnet\"${C_RESET}"
	printf '%b\n' "${C_DIM}That requires one NOPASSWD line in ${NULNET_SUDOERS}:${C_RESET}\n"
	printf '%b\n' "${C_BOLD}File: ${NULNET_SUDOERS}${C_RESET}"
	printf '%b\n' "${C_DIM}────────────────────────────────────────${C_RESET}"
	print_sudoers_snippet | while IFS= read -r line; do
		printf '  %b%s%b\n' "$C_GREEN" "$line" "$C_RESET"
	done
	printf '%b\n' "${C_DIM}────────────────────────────────────────${C_RESET}"
	printf '\n%b\n' "${C_YELLOW}Without this, the binary may update but the service will not restart.${C_RESET}"
	if prompt_yes_no "Install sudoers rules for passwordless self-update restart?" 0; then
		INSTALL_SUDOERS=1
	else
		INSTALL_SUDOERS=0
		warn "Skipping sudoers — you can add rules later or restart nulnet manually after updates."
	fi
}

show_intro() {
	local mode_label=""
	if [[ "$DRY_RUN" == true ]]; then
		mode_label=" (dry-run)"
	fi
	banner "$mode_label"

	cat <<EOF
${C_DIM}Telemetry host agent: Unix-socket API, Ed25519 auth, self-update from GitHub or CDN.${C_RESET}

  ${C_BOLD}Source:${C_RESET}  $(source_label)
  ${C_BOLD}URL:${C_RESET}    $(download_base)

${C_BOLD}What this installer will do${C_RESET}
  ${C_CYAN}0)${C_RESET} Install required system packages ${C_DIM}(curl, socat — root/sudo)${C_RESET}
  ${C_CYAN}1)${C_RESET} Download and verify the agent binary and systemd unit ${C_DIM}(no root)${C_RESET}
  ${C_CYAN}2)${C_RESET} Optionally configure sudoers for passwordless restart after self-update
  ${C_CYAN}3)${C_RESET} Install under ${NULNET_ROOT}, enable and start systemd ${C_DIM}(root/sudo)${C_RESET}

${C_BOLD}Paths touched in a real install${C_RESET}
  ${C_GREEN}+${C_RESET} user ${C_BOLD}nulnet${C_RESET}, ${NULNET_ROOT}/{bin,data,logs}
  ${C_GREEN}+${C_RESET} ${NULNET_BIN}, ${NULNET_SERVICE}
  ${C_GREEN}+${C_RESET} ${NULNET_CONFIG} ${C_DIM}(only if missing)${C_RESET}
  ${C_DIM}±${C_RESET} ${NULNET_SUDOERS} ${C_DIM}(if you opt in)${C_RESET}
EOF

	if [[ "$DRY_RUN" == true ]]; then
		printf '\n%b\n' "${C_MAGENTA}${C_BOLD}Dry-run:${C_RESET} ${C_MAGENTA}runs downloads, checks, and sudo authentication; skips commands that modify the system.${C_RESET}"
	fi
}

detect_pkg_manager() {
	if command -v apt-get &>/dev/null; then
		printf 'apt'
	elif command -v pacman &>/dev/null; then
		printf 'pacman'
	else
		printf ''
	fi
}

pkg_installed() {
	local pkg="$1"
	local mgr
	mgr="$(detect_pkg_manager)"
	case "$mgr" in
		apt)    dpkg -l "$pkg" 2>/dev/null | grep -q "^ii" ;;
		pacman) pacman -Q "$pkg" &>/dev/null ;;
		*)      return 1 ;;
	esac
}

phase_packages() {
	hdr "Phase 0 — system packages"

	local mgr
	mgr="$(detect_pkg_manager)"
	if [[ -z "$mgr" ]]; then
		warn "No supported package manager (apt/pacman) — skipping auto-install"
		warn "Ensure curl, socat, and sha256sum are installed manually."
		return 0
	fi

	local needed=("curl" "socat")
	if ! command -v sha256sum &>/dev/null && ! command -v shasum &>/dev/null; then
		needed+=("coreutils")
	fi

	local to_install=()
	for pkg in "${needed[@]}"; do
		if ! pkg_installed "$pkg" && ! command -v "$pkg" &>/dev/null; then
			to_install+=("$pkg")
		fi
	done

	if [[ "${#to_install[@]}" -eq 0 ]]; then
		ok "Required packages already installed (curl, socat, sha256sum)"
		return 0
	fi

	info "Installing missing packages: ${to_install[*]}"
	if [[ "$DRY_RUN" == true ]]; then
		skip "package install: ${to_install[*]}"
		return 0
	fi

	case "$mgr" in
		apt)
			as_root apt-get update -qq
			as_root DEBIAN_FRONTEND=noninteractive apt-get install -y "${to_install[@]}"
			;;
		pacman)
			as_root pacman -Sy --noconfirm "${to_install[@]}"
			;;
	esac
	ok "Packages installed: ${to_install[*]}"
}

check_commands() {
	local missing=()
	command -v curl &>/dev/null || missing+=("curl")
	if ! command -v sha256sum &>/dev/null && ! command -v shasum &>/dev/null; then
		missing+=("sha256sum or shasum")
	fi
	command -v mktemp &>/dev/null || missing+=("mktemp")
	if [[ "$DRY_RUN" != true ]]; then
		if ! can_create_users; then
			missing+=("useradd or adduser")
		fi
		for cmd in install systemctl; do
			command -v "$cmd" &>/dev/null || missing+=("$cmd")
		done
		if ! command -v visudo &>/dev/null; then
			warn "visudo not found — sudoers validation will be skipped on install"
		fi
	fi
	if [[ "${#missing[@]}" -gt 0 ]]; then
		err "Required command(s) not found: ${missing[*]}"
		info "Install them (e.g. apt-get install curl socat coreutils passwd sudo) and re-run."
		exit 1
	fi
	ok "Required tools present"
}

phase_download() {
	hdr "Phase 1 — download and verify"

	local base
	base="$(download_base)"
	local label
	label="$(source_label)"

	TMP_BIN="$(mktemp)"
	TMP_SVC="$(mktemp)"
	cleanup() { rm -f "${TMP_BIN:-}" "${TMP_SVC:-}"; }
	trap cleanup EXIT

	info "Downloading agent binary from ${label}…"
	if ! curl -fsSL --connect-timeout 10 --max-time 120 -o "$TMP_BIN" "${base}/nulnet"; then
		err "Failed to download nulnet binary from ${label}"
		exit 1
	fi
	ok "Downloaded $(wc -c <"$TMP_BIN" | tr -d ' ') bytes"

	EXPECTED_HASH="$(curl -fsSL --connect-timeout 10 --max-time 30 \
		"${base}/nulnet.sha256" 2>/dev/null || true)"
	EXPECTED_HASH="${EXPECTED_HASH//[$'\t\r\n ']}"

	if [[ -n "$EXPECTED_HASH" ]] && [[ "${#EXPECTED_HASH}" -eq 64 ]]; then
		ACTUAL_HASH="$(sha256_file "$TMP_BIN")"
		if [[ "$ACTUAL_HASH" != "$EXPECTED_HASH" ]]; then
			err "Checksum mismatch — aborting"
			printf '%b\n' "${C_DIM}  Expected: ${EXPECTED_HASH}${C_RESET}" >&2
			printf '%b\n' "${C_DIM}  Got:      ${ACTUAL_HASH}${C_RESET}" >&2
			exit 1
		fi
		ok "Checksum verified (SHA-256)"
	else
		warn "Could not fetch a 64-character checksum — skipping verification"
	fi

	info "Downloading systemd unit…"
	if ! curl -fsSL --connect-timeout 10 --max-time 30 -o "$TMP_SVC" \
		"${base}/nulnet.service"; then
		err "Failed to download nulnet.service from ${label}"
		exit 1
	fi
	ok "Downloaded systemd unit ($(wc -l <"$TMP_SVC" | tr -d ' ') lines)"

	if [[ "$DRY_RUN" == true ]]; then
		ok "Phase 1 complete (artifacts in temp files only)"
	else
		ok "Phase 1 complete — artifacts ready for install"
	fi
}

ensure_sudo_access() {
	if is_root; then
		ok "Running as root"
		return 0
	fi

	info "Elevating with sudo"
	printf '%b\n' "${C_DIM}  Phase 2 needs root to inspect and (on a real install) modify:
  • user/dirs under ${NULNET_ROOT}
  • binary, systemd unit, optional ${NULNET_SUDOERS}
  • group membership for '${INVOKER}'
  • systemctl enable & restart nulnet${C_RESET}"
	if [[ "$INSTALL_SUDOERS" == 1 ]]; then
		info "Sudoers install: ${C_BOLD}yes${C_RESET}"
	else
		warn "Sudoers install: ${C_BOLD}no${C_RESET}"
	fi
	echo

	local sudo_prompt="Continue with sudo?"
	if [[ "$DRY_RUN" == true ]]; then
		sudo_prompt="Continue with sudo for read-only checks? (no install writes)"
	fi

	if ! prompt_yes_no "$sudo_prompt" 0; then
		warn "Privileged phase skipped."
		exit 0
	fi

	echo
	info "Requesting sudo credentials…"
	if ! sudo -v; then
		err "sudo authentication failed"
		exit 1
	fi
	ok "sudo access granted"
}

write_default_config() {
	if [[ "$DRY_RUN" == true ]]; then
		skip "write default ${NULNET_CONFIG}"
		return 0
	fi
	info "Creating default ${NULNET_CONFIG}"
	if uses_cdn; then
		as_root tee "$NULNET_CONFIG" >/dev/null <<EOL
[agent]
data_dir = "/opt/nulnet/data"
retention_days = 5
socket_path = "/opt/nulnet/nulnet.sock"
allowed_keys = []

[telemetry]
interval_seconds = 30

[update]
cdn = "${CDN_BASE}"
EOL
	else
		as_root tee "$NULNET_CONFIG" >/dev/null <<'EOL'
[agent]
data_dir = "/opt/nulnet/data"
retention_days = 5
socket_path = "/opt/nulnet/nulnet.sock"
allowed_keys = []

[telemetry]
interval_seconds = 30
EOL
	fi
	as_root chown nulnet:nulnet "$NULNET_CONFIG"
	as_root chmod 640 "$NULNET_CONFIG"
	ok "Created ${NULNET_CONFIG}"
}

# Validate a sudoers file with visudo when available.
# Returns 0 on success or when validation is skipped (no visudo).
# Returns 1 when visudo reports invalid syntax.
validate_sudoers_path() {
	local file="$1"
	if ! command -v visudo &>/dev/null; then
		return 0
	fi
	if as_root visudo -c -f "$file" &>/dev/null; then
		return 0
	fi
	return 1
}

install_sudoers_file() {
	if [[ "$DRY_RUN" == true ]]; then
		info "Would write ${NULNET_SUDOERS}:"
		print_sudoers_snippet | while IFS= read -r line; do
			printf '    %b%s%b\n' "$C_GREEN" "$line" "$C_RESET"
		done
		skip "tee ${NULNET_SUDOERS}"
		return 0
	fi
	info "Writing ${NULNET_SUDOERS}"
	as_root tee "$NULNET_SUDOERS" >/dev/null <<'SUDOERS'
# nulnet — self-update (no password)
nulnet ALL=(ALL) NOPASSWD: /usr/bin/systemctl restart nulnet
SUDOERS
	as_root chmod 0440 "$NULNET_SUDOERS"
	if validate_sudoers_path "$NULNET_SUDOERS"; then
		if command -v visudo &>/dev/null; then
			ok "Sudoers configured at ${NULNET_SUDOERS}"
		else
			ok "Sudoers configured at ${NULNET_SUDOERS} (visudo not found — validation skipped)"
		fi
	else
		err "sudoers validation failed — removing ${NULNET_SUDOERS}"
		as_root rm -f "$NULNET_SUDOERS"
		exit 1
	fi
}

validate_sudoers_snippet() {
	local tmp
	tmp="$(mktemp)"
	print_sudoers_snippet >"$tmp"
	chmod 0440 "$tmp"
	if validate_sudoers_path "$tmp"; then
		if command -v visudo &>/dev/null; then
			ok "sudoers snippet validates with visudo"
		fi
	else
		err "sudoers snippet failed visudo -c"
		rm -f "$tmp"
		exit 1
	fi
	rm -f "$tmp"
}

privileged_install() {
	if [[ "$DRY_RUN" == true ]]; then
		hdr "Phase 2 — install checks (dry-run)"
	else
		hdr "Phase 2 — install"
	fi

	ensure_sudo_access
	phase_packages

	# --- system user ---
	if as_root id -u nulnet &>/dev/null; then
		ok "System user nulnet already exists"
	else
		if [[ "$DRY_RUN" == true ]]; then
			info "System user nulnet does not exist yet"
			skip "$(create_system_user_desc)"
		else
			if has_useradd; then
				mutate "Creating system user nulnet" \
					useradd --system --no-create-home --home-dir "$NULNET_ROOT" \
					--shell /usr/sbin/nologin nulnet
			else
				mutate "Creating system user nulnet" \
					adduser --system --no-create-home --home "$NULNET_ROOT" \
					--shell /usr/sbin/nologin --disabled-password --gecos "" nulnet
			fi
			ok "Created system user nulnet"
		fi
	fi

	# --- docker group ---
	if command -v getent &>/dev/null && getent group docker &>/dev/null; then
		if as_root id -u nulnet &>/dev/null; then
			if as_root id -nG nulnet 2>/dev/null | tr ' ' '\n' | grep -qx docker; then
				ok "User nulnet is already in group docker"
			else
				if command -v usermod &>/dev/null; then
					mutate "Adding nulnet to group docker" \
						usermod -aG docker nulnet
				elif has_adduser; then
					mutate "Adding nulnet to group docker" \
						adduser nulnet docker
				else
					mutate "Adding nulnet to group docker" \
						gpasswd -a nulnet docker
				fi
				if [[ "$DRY_RUN" != true ]]; then
					ok "Added nulnet to group docker"
				fi
			fi
		else
			info "User nulnet not created yet — would add to docker after user creation"
		fi
	else
		info "Docker group not present — skipping docker membership"
	fi

	# --- directories ---
	if as_root test -d "${NULNET_ROOT}/bin"; then
		ok "Directory ${NULNET_ROOT}/bin exists"
	else
		info "Directory ${NULNET_ROOT}/bin is missing"
	fi
	mutate "Creating ${NULNET_ROOT}/{bin,data,logs}" \
		mkdir -p "${NULNET_ROOT}/bin" "${NULNET_ROOT}/data" "${NULNET_ROOT}/logs"
	if [[ "$DRY_RUN" != true ]]; then
		ok "Directories ready under ${NULNET_ROOT}"
	fi

	# --- binary ---
	info "Staging binary from ${TMP_BIN}"
	if [[ -f "$TMP_BIN" ]]; then
		ok "Local artifact SHA-256: $(sha256_file "$TMP_BIN")"
	fi
	if as_root test -f "$NULNET_BIN"; then
		inspect "Current installed binary" test -x "$NULNET_BIN"
		ok "Existing binary at ${NULNET_BIN}"
	else
		info "No binary installed yet at ${NULNET_BIN}"
	fi
	mutate "Installing binary → ${NULNET_BIN}" \
		install -m 755 -o nulnet -g nulnet "$TMP_BIN" "$NULNET_BIN"

	# --- config ---
	if as_root test -f "$NULNET_CONFIG"; then
		ok "${NULNET_CONFIG} already exists — would not overwrite"
	else
		info "${NULNET_CONFIG} is missing"
		write_default_config
	fi

	mutate "chown -R nulnet:nulnet ${NULNET_ROOT}" \
		chown -R nulnet:nulnet "$NULNET_ROOT"
	mutate "chmod 750 ${NULNET_ROOT}" chmod 750 "$NULNET_ROOT"

	# --- invoker group ---
	if [[ -n "$INVOKER" ]] && id -u "$INVOKER" &>/dev/null; then
		if [[ "$(id -u "$INVOKER")" -ne 0 ]]; then
			if id -nG "$INVOKER" 2>/dev/null | tr ' ' '\n' | grep -qx nulnet; then
				ok "User '${INVOKER}' is already in group nulnet"
			else
				if command -v usermod &>/dev/null; then
					mutate "Adding '${INVOKER}' to group nulnet" \
						usermod -aG nulnet "$INVOKER"
				elif has_adduser; then
					mutate "Adding '${INVOKER}' to group nulnet" \
						adduser "$INVOKER" nulnet
				else
					mutate "Adding '${INVOKER}' to group nulnet" \
						gpasswd -a "$INVOKER" nulnet
				fi
				if [[ "$DRY_RUN" != true ]]; then
					ok "Added '${INVOKER}' to group nulnet"
				fi
			fi
		fi
	fi

	# --- sudoers ---
	if [[ "$INSTALL_SUDOERS" == 1 ]]; then
		if as_root test -f "$NULNET_SUDOERS"; then
			info "Existing ${NULNET_SUDOERS} present"
			inspect "Current sudoers file permissions" stat -c '%a %U:%G' "$NULNET_SUDOERS" 2>/dev/null \
				|| inspect "Current sudoers file" ls -la "$NULNET_SUDOERS"
		fi
		info "Validating sudoers snippet (temp file)…"
		validate_sudoers_snippet
		install_sudoers_file
	else
		info "Sudoers install declined — skipping ${NULNET_SUDOERS}"
	fi

	# --- systemd unit ---
	if as_root test -f "$NULNET_SERVICE"; then
		ok "Systemd unit already at ${NULNET_SERVICE}"
	else
		info "Systemd unit not installed yet"
	fi
	mutate "Installing unit → ${NULNET_SERVICE}" \
		install -m 644 "$TMP_SVC" "$NULNET_SERVICE"

	# --- service state (read-only in dry-run) ---
	if command -v systemctl &>/dev/null; then
		if as_root systemctl list-unit-files nulnet.service &>/dev/null 2>&1; then
			inspect "systemd unit file listing" \
				systemctl list-unit-files nulnet.service --no-pager 2>/dev/null || true
		fi
		if as_root systemctl is-active --quiet nulnet 2>/dev/null; then
			ok "Service nulnet is currently active"
		elif as_root systemctl is-enabled --quiet nulnet 2>/dev/null; then
			warn "Service nulnet is enabled but not active"
		else
			info "Service nulnet is not active (expected before first install)"
		fi
	fi

	mutate "systemctl daemon-reload" systemctl daemon-reload
	mutate "systemctl enable nulnet" systemctl enable nulnet
	mutate "systemctl restart nulnet" systemctl restart nulnet

	if [[ "$DRY_RUN" != true ]]; then
		if as_root systemctl is-active --quiet nulnet; then
			ok "nulnet service is active"
		else
			warn "nulnet may not be running — check: journalctl -u nulnet -n 50"
		fi
	fi
}

show_next_steps() {
	hdr "Next steps"
	printf '%b\n' "  ${C_CYAN}•${C_RESET} Add your nulctl public key to ${C_BOLD}allowed_keys${C_RESET} in ${NULNET_CONFIG}"
	printf '%b\n' "  ${C_CYAN}•${C_RESET} ${C_DIM}sudo systemctl restart nulnet${C_RESET}"
	printf '%b\n' "  ${C_CYAN}•${C_RESET} ${C_DIM}sudo journalctl -u nulnet -n 200${C_RESET}"
	printf '%b\n' "  ${C_CYAN}•${C_RESET} ${C_DIM}ls -la /opt/nulnet/nulnet.sock${C_RESET}"
	echo
}

finish_dry_run() {
	printf '\n%b\n' "${C_GREEN}${C_BOLD}Dry-run complete.${C_RESET} ${C_DIM}Downloads and checks succeeded; no system files were modified. Re-run without --dry-run to install.${C_RESET}"
	echo
}

# --- main --------------------------------------------------------------------

show_intro

if ! prompt_yes_no "Proceed with installation?"; then
	info "Aborted."
	exit 0
fi

check_commands
phase_download
prompt_sudoers
privileged_install

trap - EXIT
rm -f "$TMP_BIN" "$TMP_SVC"

if [[ "$DRY_RUN" == true ]]; then
	finish_dry_run
	exit 0
fi

show_next_steps
