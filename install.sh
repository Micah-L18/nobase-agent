#!/bin/bash
# ── No-Base Agent Installer ──────────────────────────────────
# Downloads the latest release from GitHub and configures the agent.
#
# Usage:
#   curl -sSL https://raw.githubusercontent.com/Micah-L18/nobase-agent/main/install.sh | sudo bash -s -- \
#     --token TOKEN --gateway wss://gateway.nobase.dev --backend https://api.nobase.dev
#
# Options:
#   --token    Registration token (required, from NoBase dashboard)
#   --gateway  Gateway WebSocket URL (required, e.g. wss://gateway.nobase.dev)
#   --backend  Backend API URL (required, e.g. https://api.nobase.dev)
#   --version  Specific release tag to install (optional, defaults to latest)

set -euo pipefail

# ── Parse arguments ──────────────────────────────────────────
TOKEN=""
GATEWAY=""
BACKEND=""
VERSION="latest"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --token)   TOKEN="$2";   shift 2 ;;
    --gateway) GATEWAY="$2"; shift 2 ;;
    --backend) BACKEND="$2"; shift 2 ;;
    --version) VERSION="$2"; shift 2 ;;
    --help|-h)
      echo "Usage: curl -sSL <URL>/install.sh | sudo bash -s -- --token TOKEN --gateway URL --backend URL"
      echo ""
      echo "Options:"
      echo "  --token    Registration token from NoBase dashboard (required)"
      echo "  --gateway  Gateway WebSocket URL (required)"
      echo "  --backend  Backend API URL (required)"
      echo "  --version  Specific release tag (optional, defaults to latest)"
      exit 0
      ;;
    *) echo "Unknown argument: $1"; exit 1 ;;
  esac
done

echo ""
echo "  ╔═══════════════════════════════════════╗"
echo "  ║   No-Base Agent Installer              ║"
echo "  ╚═══════════════════════════════════════╝"
echo ""

# ── Validate required arguments ──────────────────────────────
ERRORS=0
if [[ -z "$TOKEN" ]]; then
  echo "  ✗ Error: --token is required"
  ERRORS=1
fi
if [[ -z "$GATEWAY" ]]; then
  echo "  ✗ Error: --gateway is required"
  ERRORS=1
fi
if [[ -z "$BACKEND" ]]; then
  echo "  ✗ Error: --backend is required"
  ERRORS=1
fi
if [[ $ERRORS -ne 0 ]]; then
  echo ""
  echo "  Usage: curl -sSL <URL>/install.sh | sudo bash -s -- \\"
  echo "    --token TOKEN --gateway wss://gateway.example.com --backend https://api.example.com"
  exit 1
fi

# ── Must run as root ─────────────────────────────────────────
if [[ "$(id -u)" -ne 0 ]]; then
  echo "  ✗ Error: This script must be run as root (use sudo)"
  exit 1
fi

# ── Detect platform ─────────────────────────────────────────
# Use statically-linked musl builds for maximum compatibility across Linux distros
ARCH=$(uname -m)
case "$ARCH" in
  x86_64|amd64)  ARCH="x86_64"; PLATFORM="linux-x86_64-static"  ;;
  aarch64|arm64)  ARCH="aarch64"; PLATFORM="linux-arm64-static" ;;
  *) echo "  ✗ Unsupported architecture: $ARCH"; exit 1 ;;
esac

OS=$(uname -s | tr '[:upper:]' '[:lower:]')
if [[ "$OS" != "linux" ]]; then
  echo "  ✗ Unsupported OS: $OS (only Linux is supported)"
  exit 1
fi

REPO="Micah-L18/nobase-agent"
BINARY_NAME="no-base-${PLATFORM}"
BINARY_PATH="/usr/local/bin/no-base"

# Normalize gateway URL: ensure it uses the /agent/ws path (not /backend/ws)
GATEWAY=$(echo "$GATEWAY" | sed 's|/backend/ws|/agent/ws|')
# If no path specified, append /agent/ws
if ! echo "$GATEWAY" | grep -q '/agent/ws'; then
  GATEWAY="${GATEWAY%/}/agent/ws"
fi

echo "  OS:       $OS"
echo "  Arch:     $ARCH"
echo "  Gateway:  $GATEWAY"
echo "  Backend:  $BACKEND"
echo ""

# ── Resolve download URL ────────────────────────────────────
if [[ "$VERSION" == "latest" ]]; then
  DOWNLOAD_URL="https://github.com/${REPO}/releases/latest/download/${BINARY_NAME}"
  CHECKSUM_URL="https://github.com/${REPO}/releases/latest/download/${BINARY_NAME}.sha256"
else
  DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${VERSION}/${BINARY_NAME}"
  CHECKSUM_URL="https://github.com/${REPO}/releases/download/${VERSION}/${BINARY_NAME}.sha256"
fi

# ── [1/5] Download agent binary ─────────────────────────────
echo "[1/5] Downloading agent binary..."

HTTP_CODE=$(curl -fsSL -w "%{http_code}" -o /tmp/no-base "$DOWNLOAD_URL" 2>/dev/null) || {
  echo "  ✗ Failed to download binary from:"
  echo "    $DOWNLOAD_URL"
  echo ""
  echo "  Make sure a release exists with the asset: ${BINARY_NAME}"
  echo "  Check: https://github.com/${REPO}/releases"
  rm -f /tmp/no-base
  exit 1
}

echo "  ✓ Binary downloaded"

# ── Verify SHA256 checksum ───────────────────────────────────
echo "  Verifying binary integrity..."
EXPECTED_SHA256=$(curl -fsSL "$CHECKSUM_URL" 2>/dev/null | awk '{print $1}') || true

if [[ -n "$EXPECTED_SHA256" ]]; then
  if command -v sha256sum &>/dev/null; then
    ACTUAL_SHA256=$(sha256sum /tmp/no-base | awk '{print $1}')
  elif command -v shasum &>/dev/null; then
    ACTUAL_SHA256=$(shasum -a 256 /tmp/no-base | awk '{print $1}')
  else
    ACTUAL_SHA256=""
    echo "  ⚠ No sha256sum/shasum available, skipping verification"
  fi

  if [[ -n "$ACTUAL_SHA256" ]]; then
    if [[ "$EXPECTED_SHA256" != "$ACTUAL_SHA256" ]]; then
      echo "  ✗ Checksum verification failed!"
      echo "    Expected: $EXPECTED_SHA256"
      echo "    Actual:   $ACTUAL_SHA256"
      echo "    The binary may have been corrupted or tampered with."
      rm -f /tmp/no-base
      exit 1
    fi
    echo "  ✓ SHA256 checksum verified"
  fi
else
  echo "  ⚠ No checksum available, skipping verification"
fi

chmod +x /tmp/no-base
mv /tmp/no-base "$BINARY_PATH"
echo "  ✓ Binary installed to $BINARY_PATH"

# ── [2/5] Write configuration ───────────────────────────────
echo "[2/5] Writing configuration..."
mkdir -p /etc/no-base

cat > /etc/no-base/agent.toml <<TOML
[connection]
gateway_url = "${GATEWAY}"
backend_url = "${BACKEND}"
api_key = ""
registration_token = "${TOKEN}"
server_id = ""

[heartbeat]
interval_secs = 30
reconnect_max_delay_secs = 300

[metrics]
push_interval_secs = 30
include_docker = true
include_gpu = true

[security]
allowed_commands = []
allowed_paths = []
max_file_size_mb = 100

[logging]
level = "info"
file = "/var/log/no-base.log"
max_size_mb = 50
max_files = 3
TOML

chmod 600 /etc/no-base/agent.toml
echo "  ✓ Config written to /etc/no-base/agent.toml"

# ── [3/5] Create systemd service ────────────────────────────
echo "[3/5] Creating systemd service..."

cat > /etc/systemd/system/no-base.service <<SERVICE
[Unit]
Description=No-Base Agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=${BINARY_PATH} run
Restart=always
RestartSec=5
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
SERVICE

systemctl daemon-reload
echo "  ✓ Systemd service created"

# ── [4/5] Create uninstall script ───────────────────────────
echo "[4/5] Creating uninstall script..."

cat > /usr/local/bin/no-base-uninstall <<'UNINSTALL'
#!/bin/bash
set -e

echo ""
echo "  Removing No-Base Agent..."
echo ""

if [[ "$(id -u)" -ne 0 ]]; then
  echo "Error: This script must be run as root (use sudo)"
  exit 1
fi

if systemctl is-active --quiet no-base 2>/dev/null; then
  systemctl stop no-base
  echo "  ✓ Agent stopped"
fi

if systemctl is-enabled --quiet no-base 2>/dev/null; then
  systemctl disable no-base --quiet
fi

rm -f /etc/systemd/system/no-base.service
systemctl daemon-reload
echo "  ✓ Systemd service removed"

rm -f /usr/local/bin/no-base
echo "  ✓ Binary removed"

rm -rf /etc/no-base
echo "  ✓ Configuration removed"

rm -f /var/log/no-base.log
rm -rf /var/lib/no-base
echo "  ✓ Logs and data removed"

rm -f /usr/local/bin/no-base-uninstall
echo "  ✓ Uninstall script removed"

echo ""
echo "  No-Base Agent has been completely removed."
echo ""
UNINSTALL

chmod +x /usr/local/bin/no-base-uninstall
echo "  ✓ Uninstall script created at /usr/local/bin/no-base-uninstall"

# ── [5/5] Start agent ──────────────────────────────────────
echo "[5/5] Starting agent..."
systemctl enable no-base --quiet
systemctl restart no-base

sleep 2

if systemctl is-active --quiet no-base; then
  echo "  ✓ Agent is running"
else
  echo "  ⚠ Agent may have failed to start. Check logs:"
  echo "    sudo no-base logs --lines 20"
fi

echo ""
echo "  ╔═══════════════════════════════════════╗"
echo "  ║   Installation Complete!               ║"
echo "  ╚═══════════════════════════════════════╝"
echo ""
echo "  Commands:"
echo "    no-base status     — check status"
echo "    no-base logs       — view logs (live)"
echo "    no-base restart    — restart agent"
echo "    no-base stop       — stop agent"
echo "    no-base uninstall  — remove agent"
echo ""
