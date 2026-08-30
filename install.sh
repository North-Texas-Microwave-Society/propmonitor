#!/bin/bash
# propmonitor — one-command installer
#
#   curl -sSL https://raw.githubusercontent.com/graham/propmonitor/main/install.sh | sudo bash
#
# Installs the latest propmonitor binary release + systemd service on a
# clean Debian 13 (Trixie) / Debian 12 (Bookworm) / Ubuntu 24.04 LTS /
# Raspberry Pi OS 64-bit box. Everything propmonitor needs at runtime is
# pulled in here; the machine does not need a prior SDR setup.
#
# Safe to re-run: existing config values are used as the prompt defaults,
# so pressing Enter through the questions leaves your settings untouched.
#
# Non-interactive (CI, cloud-init, Ansible):
#   curl -sSL .../install.sh | sudo PROPMONITOR_NONINTERACTIVE=1 bash
# accepts every default without prompting.

set -euo pipefail

# ---------------------------------------------------------------------------
# Config — change these if you fork the repo
#
# Development happens on the private Forgejo instance; that repo push-mirrors
# to the public GitHub one, whose Actions build and publish the release
# binaries. So downloads always come from GitHub, never from Forgejo.
# ---------------------------------------------------------------------------
REPO="North-Texas-Microwave-Society/propmonitor"
GITHUB_API="https://api.github.com/repos/${REPO}"
GITHUB_DL="https://github.com/${REPO}/releases/download"

SERVICE_NAME="propmonitor"
INSTALL_DIR="/usr/local/bin"
CONFIG_DIR="/etc/propmonitor"
SERVICE_USER="propmonitor"
BLACKLIST_FILE="/etc/modprobe.d/propmonitor-rtlsdr.conf"

# ---------------------------------------------------------------------------
# Colour helpers
#
# `tput` needs both a tty and a usable TERM. Under `curl | sudo bash`
# stdout is still the terminal, but TERM can be unset in a cron/cloud-init
# context — so every call is failure-tolerant. Without the guards, `set -e`
# would abort the install before it printed a single line.
# ---------------------------------------------------------------------------
if [[ -t 1 && -n "${TERM:-}" && "${TERM}" != "dumb" ]]; then
    BLD=$(tput bold     2>/dev/null || true)
    GRN=$(tput setaf 2  2>/dev/null || true)
    YLW=$(tput setaf 3  2>/dev/null || true)
    RED=$(tput setaf 1  2>/dev/null || true)
    CYN=$(tput setaf 6  2>/dev/null || true)
    RST=$(tput sgr0     2>/dev/null || true)
else
    BLD="" GRN="" YLW="" RED="" CYN="" RST=""
fi

info()  { echo "  ${GRN}[+]${RST} $*"; }
step()  { echo; echo "${BLD}${CYN}--- $* ---${RST}"; echo; }
warn()  { echo "  ${YLW}[!]${RST} $*"; }
die()   { echo "  ${RED}[ERROR]${RST} $*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Interactive input
#
# `curl … | sudo bash` hands the script its own source text on stdin, so a
# bare `read` consumes script bytes and then hits EOF — which under
# `set -e` kills the install partway through. Prompts therefore read from
# the controlling terminal on fd 3, and fall back to defaults when there
# isn't one.
# ---------------------------------------------------------------------------
INTERACTIVE=0
if [[ "${PROPMONITOR_NONINTERACTIVE:-0}" != "1" ]] && [[ -e /dev/tty ]] &&
   (exec 3</dev/tty) 2>/dev/null; then
    exec 3</dev/tty
    INTERACTIVE=1
fi

# ask PROMPT DEFAULT [VALIDATOR]
# The prompt goes to stderr so command substitution captures only the answer.
ask() {
    local prompt="$1" default="$2" validator="${3:-}" reply
    while :; do
        reply=""
        if (( INTERACTIVE )); then
            printf '  %s [%s]: ' "$prompt" "$default" >&2
            IFS= read -r reply <&3 || reply=""
        fi
        reply="${reply:-$default}"
        if [[ -z "$validator" ]] || "$validator" "$reply"; then
            printf '%s' "$reply"
            return 0
        fi
        if (( ! INTERACTIVE )); then
            die "Default value '${reply}' for '${prompt}' is invalid; fix the script."
        fi
        echo "      ${YLW}Invalid value: ${reply}${RST}" >&2
    done
}

is_number()   { [[ "$1" =~ ^-?[0-9]+([.][0-9]+)?$ ]]; }
is_positive() { is_number "$1" && [[ "${1%%.*}" -gt 0 ]] 2>/dev/null; }
is_gain()     { [[ "$1" == "auto" ]] || is_number "$1"; }
is_period()   { [[ "$1" =~ ^[0-9]+$ ]] && (( 10#$1 >= 5 )); }
is_mode()     { case "$1" in usb|lsb|am|nfm|wfm|cw|beacon) return 0 ;; *) return 1 ;; esac; }
# The in-tree YAML reader has no escape syntax for a double quote inside a
# double-quoted scalar, so reject the one character that would corrupt the file.
is_plain()    { [[ "$1" != *'"'* ]]; }
is_grid()     { [[ -z "$1" ]] || { is_plain "$1" && (( ${#1} >= 4 && ${#1} <= 20 )); }; }

# ---------------------------------------------------------------------------
# YAML get/set
#
# Values are passed through the environment rather than interpolated into a
# sed script: an SDR serial or an auth token may contain `/`, `&` or `\`,
# all of which are active characters in a sed replacement. `index(…) == 1`
# is a literal prefix test, so no key is treated as a regex either.
# The KEY argument carries its own indentation ("  monitor_token").
# ---------------------------------------------------------------------------
yaml_get() {
    local file="$1"
    [[ -f "$file" ]] || return 0
    YKEY="$2" awk '
        BEGIN { key = ENVIRON["YKEY"] }
        index($0, key ": ") == 1 || index($0, key ":") == 1 {
            v = substr($0, length(key) + 2)
            sub(/[ \t]+#.*$/, "", v)
            gsub(/^[ \t]+|[ \t]+$/, "", v)
            if (v ~ /^".*"$/) v = substr(v, 2, length(v) - 2)
            print v
            exit
        }
    ' "$file"
}

# yaml_put FILE MATCH_REGEX NEW_LINE
#
# Replaces the first line matching MATCH_REGEX with NEW_LINE. When nothing
# matches, NEW_LINE is inserted just before the first nested block (`beacon:`,
# `http:`, …) rather than appended at EOF: a top-level key appended after
# those blocks sits inside the region that the microwaveprop rewrite scans,
# and would be silently swallowed on the next run.
yaml_put() {
    local file="$1" tmp
    tmp=$(mktemp "${file}.XXXXXX")
    YRE="$2" YLINE="$3" awk '
        BEGIN { re = ENVIRON["YRE"]; line = ENVIRON["YLINE"] }
        !done && $0 ~ re { print line; done = 1; next }
        # A top-level key with no value opens a nested block: end of scalars.
        !done && /^[A-Za-z_][A-Za-z0-9_]*:[ \t]*(#.*)?$/ {
            print line; done = 1; print; next
        }
        { print }
        END { if (!done) print line }
    ' "$file" > "$tmp"
    # Copy contents rather than rename: keeps the original owner and mode.
    cat "$tmp" > "$file"
    rm -f "$tmp"
}

# yaml_set FILE KEY VALUE — every key passed here is a hardcoded identifier,
# so anchoring it as a regex is safe.
yaml_set() { yaml_put "$1" "^$2:" "$2: $3"; }

# `gain` is read with a float parse (src/config.rs), so AGC is expressed by
# the key being *absent* — never by a literal `gain: auto`, which makes the
# daemon refuse to start and crash-loop. Toggling therefore rewrites the same
# line between its commented and active forms, matching either one.
GAIN_LINE_RE='^#?[ \t]*gain:'
yaml_set_gain() {
    local file="$1" value="$2"
    if [[ "$value" == "auto" ]]; then
        yaml_put "$file" "$GAIN_LINE_RE" \
                 "# gain: 10                     # dB. Uncomment to disable AGC."
    else
        yaml_put "$file" "$GAIN_LINE_RE" "gain: ${value}"
    fi
}

# Delete the `microwaveprop:` section — active or commented out, including the
# header comment the installer writes above it — so the section can be
# regenerated from scratch rather than patched with fragile uncommenting seds.
# Without swallowing the header too, re-running duplicates that comment line.
yaml_drop_microwaveprop() {
    local file="$1" tmp
    tmp=$(mktemp "${file}.XXXXXX")
    awk '
        /^#[ \t]*Uploads to prop\.w5isp\.com/ { skip = 1; next }
        /^#?[ \t]*microwaveprop:/             { skip = 1; next }
        skip && /^#?[ \t]+[^ \t]/             { next }
        skip && /^[ \t]*$/                    { next }
        { skip = 0; print }
    ' "$file" > "$tmp"
    # `$(cat)` drops trailing newlines, so repeated runs cannot accumulate
    # blank lines where the section used to be.
    printf '%s\n' "$(cat "$tmp")" > "$file"
    rm -f "$tmp"
}

# ---------------------------------------------------------------------------
# Pre-flight checks
# ---------------------------------------------------------------------------
if [[ $EUID -ne 0 ]]; then
    echo "${RED}This script must be run as root.${RST}"
    echo
    echo "  ${BLD}curl -sSL https://raw.githubusercontent.com/${REPO}/main/install.sh | sudo bash${RST}"
    echo
    exit 1
fi

command -v apt-get >/dev/null 2>&1 ||
    die "No apt-get found. This installer supports Debian 12/13, Ubuntu 24.04 LTS,
  and Raspberry Pi OS 64-bit. On other distributions, install SoapySDR plus its
  RTL-SDR module and build from source: https://github.com/${REPO}#local-development"

command -v systemctl >/dev/null 2>&1 ||
    die "No systemd found. propmonitor installs as a systemd service."

OS_PRETTY=$(. /etc/os-release 2>/dev/null && echo "${PRETTY_NAME:-unknown}")
OS_FAMILY=$(. /etc/os-release 2>/dev/null && echo "${ID:-} ${ID_LIKE:-}")
case "$OS_FAMILY" in
    *debian*|*ubuntu*|*raspbian*) : ;;
    *) warn "Unrecognised distribution '${OS_PRETTY}'. Continuing, but only" \
            "Debian/Ubuntu derivatives are tested." ;;
esac

ARCH=$(uname -m)
case "$ARCH" in
    x86_64)  ASSET="propmonitor-x86_64-linux"   ;;
    aarch64) ASSET="propmonitor-aarch64-linux"  ;;
    armv7l)  ASSET="propmonitor-armv7-linux"    ;;
    *)       die "Unsupported architecture: ${ARCH}.
  Prebuilt binaries exist for x86_64, aarch64 (Raspberry Pi 3/4/5 on a 64-bit
  OS) and armv7l (Raspberry Pi 2/3/4 on a 32-bit OS). Older armv6 boards
  (Pi 1, Pi Zero W) must build from source." ;;
esac

echo
echo "${BLD}=============================================="
echo "  propmonitor — SDR Beacon Monitor Installer"
echo "==============================================${RST}"
echo
info "System       : ${BLD}${OS_PRETTY}${RST}"
info "Architecture : ${BLD}${ARCH}${RST} → asset ${BLD}${ASSET}${RST}"
(( INTERACTIVE )) || info "Mode         : ${BLD}non-interactive${RST} (accepting all defaults)"

# ---------------------------------------------------------------------------
# System dependencies
# ---------------------------------------------------------------------------
step "Installing system packages"

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq

# Candidate version present in the configured sources?
pkg_available() {
    local cand
    cand=$(apt-cache policy "$1" 2>/dev/null | awk '/Candidate:/ { print $2; exit }')
    [[ -n "$cand" && "$cand" != "(none)" ]]
}

# On Ubuntu the SoapySDR stack lives in `universe`. Probe for the package
# rather than grepping sources.list: Ubuntu 24.04 uses the deb822
# `/etc/apt/sources.list.d/ubuntu.sources` format, which a `^deb ` grep
# never matches, and Debian has no `universe` component at all.
if ! pkg_available soapysdr-module-rtlsdr; then
    if command -v add-apt-repository >/dev/null 2>&1; then
        info "Enabling the 'universe' component..."
        add-apt-repository -y universe >/dev/null
        apt-get update -qq
    fi
fi

pkg_available soapysdr-module-rtlsdr ||
    die "Package 'soapysdr-module-rtlsdr' is not available from your apt sources.
  On Ubuntu it lives in the 'universe' component:
      sudo add-apt-repository universe && sudo apt-get update
  On Debian make sure the 'main' component is enabled in /etc/apt/sources.list.d/."

# Required: the install is broken without these.
REQUIRED_PACKAGES=(
    # HTTPS trust store. reqwest is built against rustls-native-certs, which
    # reads /etc/ssl/certs — with no CA bundle, every upload to
    # prop.w5isp.com fails the TLS handshake. Minimal Debian images and most
    # container bases ship without it.
    ca-certificates
    curl
    # Pulls in libsoapysdr0.x AND the librtlsdr runtime transitively. The
    # librtlsdr package is deliberately NOT named here: its SONAME is baked
    # into the package name and differs per distribution (librtlsdr0 on
    # Debian 13, librtlsdr2 on Ubuntu 24.04), so naming it breaks one of the
    # two. Depending on the module keeps this correct everywhere — and the
    # udev rules ride along with the library either way.
    soapysdr-module-rtlsdr
)

# Diagnostics. Referenced by the troubleshooting output, but the service runs
# without them, so an archive that lacks one must not abort the install.
OPTIONAL_PACKAGES=(
    soapysdr-tools   # SoapySDRUtil --find
    rtl-sdr          # rtl_test / rtl_eeprom
)

install_packages() {
    local strict="$1"; shift
    local pkg missing=()
    for pkg in "$@"; do
        if dpkg-query -W -f='${Status}' "$pkg" 2>/dev/null | grep -q "ok installed"; then
            info "Already installed: ${pkg}"
        elif pkg_available "$pkg"; then
            missing+=("$pkg")
        elif [[ "$strict" == "required" ]]; then
            die "Required package '${pkg}' is not available from your apt sources."
        else
            warn "Optional package '${pkg}' is unavailable here — skipping."
        fi
    done
    if (( ${#missing[@]} )); then
        info "Installing: ${missing[*]}"
        apt-get install -y --no-install-recommends "${missing[@]}"
    fi
}

install_packages required "${REQUIRED_PACKAGES[@]}"
install_packages optional "${OPTIONAL_PACKAGES[@]}"

# The dongle is only reachable by a non-root service user because librtlsdr
# ships a udev rule granting MODE=0660 GROUP=plugdev. Assert it actually
# landed rather than trusting the dependency chain silently.
if ! compgen -G '/usr/lib/udev/rules.d/*rtlsdr*.rules' >/dev/null &&
   ! compgen -G '/lib/udev/rules.d/*rtlsdr*.rules' >/dev/null; then
    warn "No librtlsdr udev rule found. The dongle may only be usable as root."
    warn "Install your distribution's librtlsdr runtime package manually."
fi

# ---------------------------------------------------------------------------
# RTL-SDR kernel driver conflict
#
# An RTL-SDR dongle enumerates as a DVB-T receiver, so the in-tree
# dvb_usb_rtl28xxu driver binds it on plug-in and holds the USB interface.
# librtlsdr then fails with "usb_claim_interface error -6" and propmonitor
# never sees a device. Neither Debian nor Ubuntu ships a blacklist for this
# (verified: the `rtl-sdr` package installs binaries and man pages only), so
# a clean install needs one written here.
# ---------------------------------------------------------------------------
step "Blacklisting the conflicting DVB-T kernel driver"

cat > "$BLACKLIST_FILE" <<'MODPROBE'
# Installed by propmonitor.
#
# RTL2832U dongles are DVB-T receivers being used as SDRs. If the kernel
# DVB driver claims the USB interface first, librtlsdr/SoapySDR fail with
# "usb_claim_interface error -6". Blacklisting the DVB stack leaves the
# device free for userspace.
#
# Note: only the DVB modules are blacklisted. The similarly named rtl8xxxu
# module is a Realtek Wi-Fi driver and is deliberately left alone.
blacklist dvb_usb_rtl28xxu
blacklist rtl2832_sdr
blacklist rtl2832
blacklist rtl2830
MODPROBE
chmod 644 "$BLACKLIST_FILE"
info "Wrote ${BLACKLIST_FILE}"

# The blacklist only affects future autoloads, so evict anything already
# holding the device. Order matters: dependants before the modules they use.
for mod in dvb_usb_rtl28xxu rtl2832_sdr rtl2832 rtl2830; do
    if lsmod 2>/dev/null | awk '{ print $1 }' | grep -qx "$mod"; then
        if modprobe -r "$mod" 2>/dev/null; then
            info "Unloaded kernel module: ${mod}"
        else
            warn "Could not unload '${mod}' (in use). Reboot to release the dongle."
        fi
    fi
done

# ---------------------------------------------------------------------------
# Download latest release
# ---------------------------------------------------------------------------
step "Downloading propmonitor binary"

# Parsed with sed rather than python3/jq: neither is guaranteed on a minimal
# Debian install, and pulling in an interpreter just to read one string is
# not worth it. GitHub returns pretty-printed JSON, one key per line.
LATEST_TAG=$(curl -fsSL --retry 3 --retry-delay 2 \
                  -H 'Accept: application/vnd.github+json' \
                  "${GITHUB_API}/releases/latest" 2>/dev/null |
             sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
             head -n1 || true)

if [[ -z "$LATEST_TAG" ]]; then
    die "Could not find a published release at ${GITHUB_API}/releases/latest.
  Either the repository has no release yet, or this machine cannot reach
  api.github.com. To build from source instead, see:
      https://github.com/${REPO}#local-development"
fi

info "Latest release: ${BLD}${LATEST_TAG}${RST}"

TARBALL="${ASSET}.tar.gz"
DOWNLOAD_URL="${GITHUB_DL}/${LATEST_TAG}/${TARBALL}"

WORKDIR=$(mktemp -d)
trap 'rm -rf "$WORKDIR"' EXIT

info "Downloading ${TARBALL}..."
curl -fsSL --retry 3 --retry-delay 2 -o "${WORKDIR}/${TARBALL}" "$DOWNLOAD_URL" ||
    die "Failed to download ${DOWNLOAD_URL}
  Check that release ${LATEST_TAG} has a ${TARBALL} asset attached."

info "Extracting..."
mkdir -p "${WORKDIR}/extract"
tar xzf "${WORKDIR}/${TARBALL}" -C "${WORKDIR}/extract"

# Locate the binaries by searching, not by reconstructing the archive's
# directory name. The release workflow names that directory after the Rust
# target triple (propmonitor-x86_64-unknown-linux-gnu), which does not match
# the asset name — so any hardcoded path here silently rots.
find_payload() { find "${WORKDIR}/extract" -type f -name "$1" -print -quit; }

BIN_PROPMONITOR=$(find_payload propmonitor)
[[ -n "$BIN_PROPMONITOR" ]] ||
    die "Binary 'propmonitor' not found inside ${TARBALL}."

install -m 755 "$BIN_PROPMONITOR" "${INSTALL_DIR}/propmonitor"
info "Installed propmonitor → ${INSTALL_DIR}/propmonitor"

# Dynamic-linker smoke test, run before anything depends on the binary.
#
# The release binaries are built on Ubuntu 24.04 and link against
# GLIBC_2.38. Distributions older than that — Debian 12 (glibc 2.36),
# Raspberry Pi OS bookworm (2.36), Ubuntu 22.04 (2.35) — cannot start the
# executable at all. Without this check the failure surfaces only as a
# systemd crash-loop with a linker error buried in the journal.
#
# Pointing it at a path that does not exist makes it fail during config load,
# long before it opens the SDR or binds a port, so the probe is instant and
# has no side effects. Reading the message is also self-adjusting: it reports
# whatever the build actually requires, with nothing hardcoded here.
LINK_PROBE=$("${INSTALL_DIR}/propmonitor" /nonexistent/preflight.yaml 2>&1 || true)
case "$LINK_PROBE" in
    *GLIBC_*|*"error while loading shared libraries"*)
        rm -f "${INSTALL_DIR}/propmonitor"
        die "The prebuilt binary cannot run on this system:
      ${LINK_PROBE}

  This release needs glibc 2.38 or newer. Supported with prebuilt binaries:
      Debian 13 (trixie), Ubuntu 24.04 LTS, Raspberry Pi OS 64-bit (trixie).
  Older releases — Debian 12, Raspberry Pi OS bookworm, Ubuntu 22.04 — must
  build from source instead:
      https://github.com/${REPO}#local-development"
    ;;
esac

BIN_SDR_DIAG=$(find_payload sdr_diag)
if [[ -n "$BIN_SDR_DIAG" ]]; then
    install -m 755 "$BIN_SDR_DIAG" "${INSTALL_DIR}/sdr_diag"
    info "Installed sdr_diag → ${INSTALL_DIR}/sdr_diag"
fi

# ---------------------------------------------------------------------------
# System user and device access
# ---------------------------------------------------------------------------
step "Setting up service user and device permissions"

if id "${SERVICE_USER}" &>/dev/null; then
    info "User '${SERVICE_USER}' already exists."
else
    useradd --system --user-group --no-create-home \
            --shell /usr/sbin/nologin \
            --comment "propmonitor SDR monitor" "${SERVICE_USER}"
    info "Created system user '${SERVICE_USER}'."
fi

# librtlsdr's udev rule grants MODE=0660 GROUP=plugdev on the dongle, so
# membership of plugdev is what lets a non-root daemon open it. `plugdev` is
# not part of Debian's base group set — minimal server images frequently
# lack it, in which case udev silently skips the GROUP assignment and the
# node stays root-only. Create it before reloading the rules.
groupadd -f plugdev
if id -nG "${SERVICE_USER}" | tr ' ' '\n' | grep -qx plugdev; then
    info "'${SERVICE_USER}' is already in the plugdev group."
else
    usermod -aG plugdev "${SERVICE_USER}"
    info "Added '${SERVICE_USER}' to the plugdev group."
fi

# Re-apply the rules to devices that are already plugged in, so the install
# does not need a reboot to hand the dongle over.
if command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload-rules 2>/dev/null || true
    udevadm trigger --subsystem-match=usb --action=add 2>/dev/null || true
    udevadm settle --timeout=10 2>/dev/null || true
    info "Reloaded udev rules and re-triggered USB devices."
fi

# ---------------------------------------------------------------------------
# Config directory and config.yaml
# ---------------------------------------------------------------------------
step "Setting up configuration"

# Owned by root, group-writable by the service user. propmonitor rewrites
# config.yaml when settings are saved from the web UI, and the atomic
# write creates `config.yaml.tmp` in this directory first — so the
# service group needs write on the DIRECTORY, not just the file. The
# setgid bit keeps that tmp file in the service group.
install -d -m 2770 -o root -g "${SERVICE_USER}" "${CONFIG_DIR}"

CONFIG="${CONFIG_DIR}/config.yaml"
FRESH_CONFIG=0

if [[ -f "$CONFIG" ]]; then
    info "Existing config found — its values become the defaults below."
else
    FRESH_CONFIG=1
    # `gain` is deliberately absent, not set to "auto": the parser reads it
    # with a float parse (src/config.rs), so a literal `gain: auto` makes the
    # daemon refuse to start and crash-loop. Absence *is* the AGC setting.
    cat > "$CONFIG" <<'YAML'
# propmonitor configuration
# Edit, then restart: sudo systemctl restart propmonitor
frequency: 28330000            # Hz — tuned center frequency
mode: beacon                   # usb | lsb | am | nfm | wfm | cw | beacon
driver: "rtlsdr"               # SoapySDR args, e.g. "rtlsdr,serial=03340219"
sample_rate: 250000            # Hz — minimum 250000
# gain: 10                     # dB. Comment this line out to use AGC.
ppm: 0                         # 0 for TCXO devices, 1-2 for plain crystal
period_seconds: 60             # measurement integration window, minimum 5

beacon:                        # used when mode == beacon
  offset_hz: 0                 # passband center, relative to `frequency`
  bandwidth_hz: 50             # narrow window for a tight CW carrier

http:
  bind: "0.0.0.0:5760"         # LAN-accessible on port 5760

# Uploads to prop.w5isp.com. All three fields are required.
# microwaveprop:
#   enabled: true
#   gridsquare: ""             # Maidenhead grid square, e.g. "FN31pr"
#   monitor_token: ""          # from https://prop.w5isp.com setup page
#   beacon_id: ""              # UUID of the beacon being monitored
YAML
    info "Wrote default config: ${CONFIG}"
fi

# ---------------------------------------------------------------------------
# Interactive configuration
# ---------------------------------------------------------------------------
echo
echo "  ${BLD}Quick configuration${RST}"
echo "  Press Enter to keep the value shown in [brackets]."
echo "  Edit ${CONFIG} later for full control."
echo

# Seeding each prompt from the current file is what makes a re-run safe:
# holding Enter rewrites every key with the value it already had.
CUR_GAIN=$(yaml_get "$CONFIG" "gain"); CUR_GAIN="${CUR_GAIN:-auto}"

FREQ=$(ask   "Center frequency (Hz)"                 "$(yaml_get "$CONFIG" frequency)"      is_positive)
DRIVER=$(ask "SDR driver / serial"                   "$(yaml_get "$CONFIG" driver)"         is_plain)
MODE=$(ask   "Mode (usb|lsb|am|nfm|wfm|cw|beacon)"   "$(yaml_get "$CONFIG" mode)"           is_mode)
GAIN=$(ask   "Gain (dB, or 'auto')"                  "$CUR_GAIN"                            is_gain)
PERIOD=$(ask "Measurement period (seconds, min 5)"   "$(yaml_get "$CONFIG" period_seconds)" is_period)
PPM=$(ask    "PPM correction (0 for TCXO)"           "$(yaml_get "$CONFIG" ppm)"            is_number)

yaml_set "$CONFIG" "frequency"      "${FREQ}"
yaml_set "$CONFIG" "driver"         "\"${DRIVER}\""
yaml_set "$CONFIG" "mode"           "${MODE}"
yaml_set "$CONFIG" "period_seconds" "${PERIOD}"
yaml_set "$CONFIG" "ppm"            "${PPM}"

yaml_set_gain "$CONFIG" "${GAIN}"

# --- uploads ---------------------------------------------------------------
echo
echo "  ${BLD}Beacon reporting to prop.w5isp.com${RST} (optional — Enter to skip)"
echo

GRID=$(ask      "Gridsquare (4-20 chars)"     "$(yaml_get "$CONFIG" "  gridsquare")"    is_grid)
TOKEN=$(ask     "Monitor token"               "$(yaml_get "$CONFIG" "  monitor_token")" is_plain)
BEACON_ID=$(ask "Beacon UUID"                 "$(yaml_get "$CONFIG" "  beacon_id")"     is_plain)

yaml_drop_microwaveprop "$CONFIG"

if [[ -n "$GRID" && -n "$TOKEN" && -n "$BEACON_ID" ]]; then
    # Only enable with all three present. A half-filled block still parses,
    # so the daemon would start and then push rejected uploads forever.
    {
        echo
        echo "microwaveprop:"
        echo "  enabled: true"
        echo "  gridsquare: \"${GRID}\""
        echo "  monitor_token: \"${TOKEN}\""
        echo "  beacon_id: \"${BEACON_ID}\""
    } >> "$CONFIG"
    info "Uploads enabled for gridsquare ${GRID}."
else
    {
        echo
        echo "# Uploads to prop.w5isp.com. All three fields are required."
        echo "# microwaveprop:"
        echo "#   enabled: true"
        echo "#   gridsquare: \"${GRID}\""
        echo "#   monitor_token: \"${TOKEN}\""
        echo "#   beacon_id: \"${BEACON_ID}\""
    } >> "$CONFIG"
    if [[ -n "$GRID$TOKEN$BEACON_ID" ]]; then
        warn "Uploads need gridsquare + token + beacon UUID; got only some."
        warn "Left disabled. Fill in the commented block in ${CONFIG}."
    else
        info "Uploads left disabled (UI-only install)."
    fi
fi

# 660, not 640: the service user rewrites this file when settings are
# saved from the web UI. Group-only read keeps monitor_token off-limits
# to other local users.
chown root:"${SERVICE_USER}" "$CONFIG"
chmod 660 "$CONFIG"

# ---------------------------------------------------------------------------
# Systemd service
# ---------------------------------------------------------------------------
step "Installing systemd service"

cat > "/etc/systemd/system/${SERVICE_NAME}.service" <<SERVICE
[Unit]
Description=propmonitor — SDR beacon signal monitor
Documentation=https://github.com/${REPO}
# network-online, not just network.target: the uploader has to resolve
# and reach prop.w5isp.com. network.target is reached before DHCP/DNS
# are usable, so plain After=network.target means the first upload
# attempts after a boot fail and sit in the retry queue for no reason.
Wants=network-online.target
After=network-online.target
# Never permanently give up. The default start-rate limit (5 starts in
# 10 s) latches the unit into "failed" if the SDR dongle is missing at
# boot; an unattended monitor must keep retrying so that plugging the
# dongle in later is enough to recover without a manual systemctl call.
StartLimitIntervalSec=0

[Service]
Type=simple
User=${SERVICE_USER}
Group=${SERVICE_USER}
# The dongle's device node is GROUP=plugdev MODE=0660 via librtlsdr's udev
# rule. Naming the group here means the unit does not silently depend on
# the supplementary groups cached in the user database.
SupplementaryGroups=plugdev
WorkingDirectory=${CONFIG_DIR}
ExecStart=${INSTALL_DIR}/propmonitor ${CONFIG_DIR}/config.yaml
Restart=always
RestartSec=10
StandardOutput=journal
StandardError=journal
SyslogIdentifier=propmonitor

# Hardening. Deliberately does NOT set PrivateDevices=yes or
# DeviceAllow: SoapySDR needs raw USB access to the dongle via
# /dev/bus/usb, and PrivateDevices hides it.
NoNewPrivileges=yes
PrivateTmp=yes
ProtectHome=yes
ProtectSystem=strict
# config.yaml is rewritten in place when settings are saved from the
# web UI, so this one path stays writable under ProtectSystem=strict.
ReadWritePaths=${CONFIG_DIR}
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictSUIDSGID=yes
RestrictNamespaces=yes
RestrictRealtime=yes
LockPersonality=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK

[Install]
WantedBy=multi-user.target
SERVICE

chmod 644 "/etc/systemd/system/${SERVICE_NAME}.service"
systemctl daemon-reload
info "Service unit installed."

# ---------------------------------------------------------------------------
# Start service
# ---------------------------------------------------------------------------
step "Starting propmonitor"

systemctl enable "${SERVICE_NAME}.service" >/dev/null
systemctl restart "${SERVICE_NAME}.service"

PORT=$(yaml_get "$CONFIG" "  bind" | sed -n 's/.*:\([0-9]\{1,5\}\)$/\1/p')
PORT="${PORT:-5760}"

# Poll the real endpoint instead of sleeping a fixed interval: `is-active`
# goes green the moment the process execs, well before the listener binds.
HEALTHY=0
for _ in $(seq 1 20); do
    if curl -fsS -m 2 "http://127.0.0.1:${PORT}/api/status" >/dev/null 2>&1; then
        HEALTHY=1
        break
    fi
    systemctl is-active --quiet "${SERVICE_NAME}.service" || break
    sleep 1
done

host_ip() {
    local ip
    ip=$(hostname -I 2>/dev/null | awk '{ print $1 }')
    [[ -n "$ip" ]] || ip=$(ip -4 route get 1.1.1.1 2>/dev/null |
                           sed -n 's/.*[[:space:]]src[[:space:]]\([0-9.]*\).*/\1/p')
    printf '%s' "${ip:-127.0.0.1}"
}
IP=$(host_ip)

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo
echo "${BLD}=============================================="
echo "  Installation complete"
echo "==============================================${RST}"
echo
echo "  Binary    : ${INSTALL_DIR}/propmonitor"
echo "  Config    : ${CONFIG}"
echo "  Web UI    : http://${IP}:${PORT}"
echo

if (( HEALTHY )); then
    echo "  ${GRN}Service is running and answering on port ${PORT}.${RST}"
elif systemctl is-active --quiet "${SERVICE_NAME}.service"; then
    echo "  ${YLW}Service is running but did not answer on port ${PORT} in 20 s.${RST}"
    echo "    journalctl -u ${SERVICE_NAME} -n 40 --no-pager"
else
    echo "  ${RED}Service failed to start. Last log lines:${RST}"
    echo
    journalctl -u "${SERVICE_NAME}" -n 20 --no-pager 2>/dev/null || true
fi

echo
echo "  Useful commands:"
echo "    View logs       : sudo journalctl -u ${SERVICE_NAME} -f"
echo "    Restart         : sudo systemctl restart ${SERVICE_NAME}"
echo "    Stop            : sudo systemctl stop ${SERVICE_NAME}"
echo "    Edit config     : sudo nano ${CONFIG}"
echo "    Re-run setup    : sudo bash install.sh"
echo "    List SDR devices: SoapySDRUtil --find"
echo "    Test the dongle : rtl_test -t"
echo "    Status          : systemctl status ${SERVICE_NAME}"
echo

if (( ! HEALTHY )); then
    warn "Troubleshooting:"
    warn "  1. Is the dongle plugged in?   SoapySDRUtil --find"
    warn "  2. Kernel driver still bound?  lsmod | grep dvb_usb_rtl28xxu"
    warn "     (a reboot completes the blacklist written by this installer)"
    warn "  3. Config rejected?            journalctl -u ${SERVICE_NAME} -n 40"
    echo
fi

if (( FRESH_CONFIG )); then
    info "This was a first install. Review ${CONFIG} before leaving it unattended."
fi
