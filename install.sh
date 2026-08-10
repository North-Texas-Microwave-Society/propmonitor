#!/bin/bash
# propmonitor — one-command installer
#
#   curl -sSL https://raw.githubusercontent.com/graham/propmonitor/main/install.sh | sudo bash
#
# Installs the latest propmonitor binary release + systemd service on
# Debian 13 (Trixie) / Raspberry Pi OS 64-bit / Ubuntu 24.04.
#
# Safe to re-run — it will skip already-installed packages and preserve
# your config.yaml.

set -euo pipefail

# ---------------------------------------------------------------------------
# Config — change these if you fork the repo
# ---------------------------------------------------------------------------
REPO="graham/propmonitor"
GITHUB_API="https://api.github.com/repos/${REPO}"
GITHUB_DL="https://github.com/${REPO}/releases/download"

SERVICE_NAME="propmonitor"
INSTALL_DIR="/usr/local/bin"
CONFIG_DIR="/etc/propmonitor"
SERVICE_USER="propmonitor"

# ---------------------------------------------------------------------------
# Colour helpers
# ---------------------------------------------------------------------------
if [[ -t 1 ]]; then
    BLD=$(tput bold)
    GRN=$(tput setaf 2)
    YLW=$(tput setaf 3)
    RED=$(tput setaf 1)
    CYN=$(tput setaf 6)
    RST=$(tput sgr0)
else
    BLD="" GRN="" YLW="" RED="" CYN="" RST=""
fi

info()  { echo "  ${GRN}[+]${RST} $*"; }
step()  { echo; echo "${BLD}${CYN}--- $* ---${RST}"; echo; }
warn()  { echo "  ${YLW}[!]${RST} $*"; }
die()   { echo "  ${RED}[ERROR]${RST} $*" >&2; exit 1; }
ask()   { local prompt="$1" default="$2" reply
          read -r -p "  ${prompt} [${default}]: " reply
          echo "${reply:-$default}"; }

# Escape a string for safe use in a sed s/// replacement (handles /, \, &).
escape_sed() { printf '%s\n' "$1" | sed 's/[\/&]/\\&/g'; }

# Write a YAML scalar line safely, quoting strings that need it.
yaml_set() {
    local file="$1" key="$2" value="$3"
    local escaped
    escaped=$(escape_sed "$value")
    sed -i "s/^${key}: .*/${key}: ${escaped}/" "$file"
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

ARCH=$(uname -m)
case "$ARCH" in
    x86_64)  ASSET="propmonitor-x86_64-linux"   ;;
    aarch64) ASSET="propmonitor-aarch64-linux"  ;;
    armv7l)  ASSET="propmonitor-armv7-linux"    ;;
    *)       die "Unsupported architecture: ${ARCH}. This installer supports x86_64, aarch64 (RPi 3/4/5), and armv7l (RPi 2/Zero)." ;;
esac

echo
echo "${BLD}=============================================="
echo "  propmonitor — SDR Beacon Monitor Installer"
echo "==============================================${RST}"
echo
info "Detected architecture: ${BLD}${ARCH}${RST} → asset ${BLD}${ASSET}${RST}"

# ---------------------------------------------------------------------------
# System dependencies
# ---------------------------------------------------------------------------
step "Installing system packages"
apt-get update -qq

# SoapySDR runtime + RTL-SDR driver module
# On Ubuntu, SoapySDR is in universe — enable it if needed
if command -v add-apt-repository &>/dev/null && ! grep -qr '^deb.*universe' /etc/apt/sources.list /etc/apt/sources.list.d/ 2>/dev/null; then
    add-apt-repository -y universe && apt-get update -qq || true
fi

PACKAGES=(
    libsoapysdr0.8
    soapysdr-module-rtlsdr
    curl
)

for pkg in "${PACKAGES[@]}"; do
    if dpkg -s "$pkg" &>/dev/null; then
        info "Already installed: ${pkg}"
    else
        apt-get install -y --no-install-recommends "$pkg"
        info "Installed: ${pkg}"
    fi
done

# ---------------------------------------------------------------------------
# Download latest release
# ---------------------------------------------------------------------------
step "Downloading propmonitor binary"

# Check connectivity and available releases
LATEST_TAG=$(curl -sSL --retry 3 --retry-delay 2 "${GITHUB_API}/releases/latest" | \
             python3 -c 'import json,sys; print(json.load(sys.stdin)["tag_name"])' 2>/dev/null || true)

if [[ -z "$LATEST_TAG" ]]; then
    die "Could not find any releases at ${GITHUB_API}/releases/latest.
  Make sure a release has been published first:
    git tag v0.1.0 && git push --tags
  Or build from source: cargo build --release"
fi

info "Latest release: ${BLD}${LATEST_TAG}${RST}"

TARBALL="${ASSET}.tar.gz"
DOWNLOAD_URL="${GITHUB_DL}/${LATEST_TAG}/${TARBALL}"

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

info "Downloading ${TARBALL}..."
if ! curl -sSL --retry 3 --retry-delay 2 -o "${TMPDIR}/${TARBALL}" "$DOWNLOAD_URL"; then
    die "Failed to download ${DOWNLOAD_URL}
  Check that the release has a ${ASSET} asset attached."
fi

info "Extracting..."
tar xzf "${TMPDIR}/${TARBALL}" -C "$TMPDIR"

# Find and install the binaries
BINARY_DIR="${TMPDIR}/propmonitor-"*
if [[ ! -f "${BINARY_DIR}/propmonitor" ]]; then
    die "Binary 'propmonitor' not found in the release tarball."
fi

install -m 755 "${BINARY_DIR}/propmonitor" "${INSTALL_DIR}/propmonitor"
info "Installed propmonitor → ${INSTALL_DIR}/propmonitor"

if [[ -f "${BINARY_DIR}/sdr_diag" ]]; then
    install -m 755 "${BINARY_DIR}/sdr_diag" "${INSTALL_DIR}/sdr_diag"
    info "Installed sdr_diag → ${INSTALL_DIR}/sdr_diag"
fi

# ---------------------------------------------------------------------------
# System user
# ---------------------------------------------------------------------------
step "Setting up service user"

if id "${SERVICE_USER}" &>/dev/null; then
    info "User '${SERVICE_USER}' already exists."
else
    useradd --system --no-create-home --shell /usr/sbin/nologin \
            --comment "propmonitor SDR monitor" "${SERVICE_USER}"
    info "Created system user '${SERVICE_USER}'."
fi

# Add to plugdev for USB access (RTL-SDR udev rule uses this group)
if getent group plugdev &>/dev/null; then
    if ! id -nG "${SERVICE_USER}" | grep -qw plugdev; then
        usermod -aG plugdev "${SERVICE_USER}"
        info "Added '${SERVICE_USER}' to plugdev group."
    else
        info "Already in plugdev group."
    fi
fi

# ---------------------------------------------------------------------------
# Config directory and config.yaml
# ---------------------------------------------------------------------------
step "Setting up configuration"

install -d -m 755 -o root -g root "${CONFIG_DIR}"

if [[ -f "${CONFIG_DIR}/config.yaml" ]]; then
    info "config.yaml already exists — leaving it alone."
    warn "To reconfigure, edit: ${CONFIG_DIR}/config.yaml"
else
    # Write a starter config. The operator fills in details.
    cat > "${CONFIG_DIR}/config.yaml" <<'YAML'
# propmonitor configuration
# Edit this file after install, then restart: sudo systemctl restart propmonitor
frequency: 28330000            # Hz — tuned center frequency
mode: beacon                   # usb | lsb | am | nfm | wfm | cw | beacon
driver: rtlsdr                 # SoapySDR driver args (e.g. "rtlsdr,serial=03340219")
sample_rate: 250000            # Hz — narrow rate gives waterfall ~244 Hz/bin
gain: auto                     # dB, or "auto" for AGC (omit line entirely for AGC too)
ppm: 0                         # 0 for TCXO devices, 1-2 for crystal
period_seconds: 60             # measurement integration window

beacon:
  offset_hz: 0
  bandwidth_hz: 50

http:
  bind: "0.0.0.0:5760"        # LAN-accessible on port 5760

# Uncomment and fill in to enable uploads to prop.w5isp.com
# microwaveprop:
#   enabled: true
#   gridsquare: ""             # Maidenhead grid square (e.g. "FN31pr") — REQUIRED for uploads
#   monitor_token: ""          # from https://prop.w5isp.com setup page
#   beacon_id: ""              # UUID of your beacon
YAML
    info "Wrote default config: ${CONFIG_DIR}/config.yaml"
fi

# ---------------------------------------------------------------------------
# Interactive configuration
# ---------------------------------------------------------------------------
echo
echo "  ${BLD}Quick configuration${RST}"
echo "  Press Enter to keep the default shown in [brackets]."
echo "  Edit ${CONFIG_DIR}/config.yaml later for full control."
echo

FREQ=$(ask     "Center frequency (Hz)"             28330000)
DRIVER=$(ask   "SDR driver / serial"               "rtlsdr")
MODE=$(ask     "Mode (usb|lsb|am|nfm|wfm|cw|beacon)" "beacon")
GAIN=$(ask     "Gain (dB, or 'auto')"              "auto")
PERIOD=$(ask   "Measurement period (seconds)"      "60")
PPM=$(ask      "PPM correction (0 for TCXO)"       "0")
GRID=$(ask     "Gridsquare for uploads (leave blank to skip)" "")
TOKEN=$(ask    "Monitor token from prop.w5isp.com (blank to skip)" "")
BEACON_ID=$(ask "Beacon UUID (blank to skip)"      "")

# Write back the user's choices (safely escaped)
yaml_set "${CONFIG_DIR}/config.yaml" "frequency"       "${FREQ}"
yaml_set "${CONFIG_DIR}/config.yaml" "driver"          "${DRIVER}"
yaml_set "${CONFIG_DIR}/config.yaml" "mode"            "${MODE}"
yaml_set "${CONFIG_DIR}/config.yaml" "gain"            "${GAIN}"
yaml_set "${CONFIG_DIR}/config.yaml" "period_seconds"  "${PERIOD}"
yaml_set "${CONFIG_DIR}/config.yaml" "ppm"             "${PPM}"

# If they provided upload credentials, uncomment the microwaveprop block
if [[ -n "${GRID}" || -n "${TOKEN}" || -n "${BEACON_ID}" ]]; then
    sed -i 's/^# microwaveprop:/microwaveprop:/' "${CONFIG_DIR}/config.yaml"
    sed -i 's/^#   enabled:/  enabled:/'         "${CONFIG_DIR}/config.yaml"
    local g_esc t_esc b_esc
    g_esc=$(escape_sed "${GRID}")
    t_esc=$(escape_sed "${TOKEN}")
    b_esc=$(escape_sed "${BEACON_ID}")
    sed -i 's/^#   gridsquare:.*$/  gridsquare: "'"${g_esc}"'"/' "${CONFIG_DIR}/config.yaml"
    sed -i 's/^#   monitor_token:.*$/  monitor_token: "'"${t_esc}"'"/' "${CONFIG_DIR}/config.yaml"
    sed -i 's/^#   beacon_id:.*$/  beacon_id: "'"${b_esc}"'"/'       "${CONFIG_DIR}/config.yaml"
    info "Upload credentials written to config."
fi

chown root:"${SERVICE_USER}" "${CONFIG_DIR}/config.yaml"
chmod 640 "${CONFIG_DIR}/config.yaml"

# ---------------------------------------------------------------------------
# Systemd service
# ---------------------------------------------------------------------------
step "Installing systemd service"

cat > /etc/systemd/system/propmonitor.service <<SERVICE
[Unit]
Description=propmonitor — SDR beacon signal monitor
After=network.target
Documentation=https://github.com/${REPO}

[Service]
Type=simple
User=${SERVICE_USER}
Group=${SERVICE_USER}
WorkingDirectory=${CONFIG_DIR}
ExecStart=${INSTALL_DIR}/propmonitor ${CONFIG_DIR}/config.yaml
Restart=always
RestartSec=10
StandardOutput=journal
StandardError=journal
SyslogIdentifier=propmonitor

# Basic hardening (SDR needs USB access, so we keep it light)
NoNewPrivileges=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
SERVICE

chmod 644 /etc/systemd/system/propmonitor.service
systemctl daemon-reload
info "Service unit installed."

# ---------------------------------------------------------------------------
# Start service
# ---------------------------------------------------------------------------
step "Starting propmonitor"

systemctl enable propmonitor.service
systemctl restart propmonitor.service

sleep 2  # give it a moment to start or fail

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo
echo "${BLD}=============================================="
echo "  Installation complete"
echo "==============================================${RST}"
echo
echo "  Binary    : ${INSTALL_DIR}/propmonitor"
echo "  Config    : ${CONFIG_DIR}/config.yaml"
echo "  Web UI    : http://$(hostname -I 2>/dev/null | awk '{print $1}' || echo "this-device"):5760"
echo

if systemctl is-active --quiet propmonitor.service; then
    echo "  ${GRN}Service is running.${RST}"
else
    echo "  ${YLW}Service is NOT running. Check the logs:${RST}"
    echo "    journalctl -u propmonitor -n 40 --no-pager"
    echo
    journalctl -u propmonitor -n 15 --no-pager 2>/dev/null || true
fi

echo
echo "  Useful commands:"
echo "    View logs       : sudo journalctl -u propmonitor -f"
echo "    Restart         : sudo systemctl restart propmonitor"
echo "    Stop            : sudo systemctl stop propmonitor"
echo "    Edit config     : sudo nano ${CONFIG_DIR}/config.yaml"
echo "    List SDR devices: ${INSTALL_DIR}/sdr_diag --list-devices"
echo "                      (or: SoapySDRUtil --find)"
echo "    Status          : systemctl status propmonitor"
echo

if systemctl is-active --quiet propmonitor.service; then
    echo "  ${GRN}Web UI → http://$(hostname -I 2>/dev/null | awk '{print $1}' || echo "localhost"):5760${RST}"
    echo
fi

warn "If you don't see the Web UI, check that the SDR is plugged in"
warn "and recognized:  SoapySDRUtil --find"
warn ""
warn "If you just added the service user to the plugdev group, you"
warn "may need to reboot for the udev rule to take effect."
echo
