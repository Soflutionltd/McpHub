#!/bin/bash
# McpHub quick install: macOS (Apple Silicon and Intel) and Linux (x64 and arm64, servers included).
#   curl -fsSL https://raw.githubusercontent.com/Soflutionltd/McpHub/main/get.sh | bash
# Pin a version:  curl -fsSL .../get.sh | MCPHUB_VERSION=v5.9.0 bash
set -euo pipefail

REPO="Soflutionltd/McpHub"
VERSION="${MCPHUB_VERSION:-latest}"
INSTALL_DIR="${MCPHUB_INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)/$(uname -m)" in
  Darwin/*) ASSET="McpHub-macos-universal.tar.gz" ;;
  Linux/x86_64|Linux/amd64) ASSET="McpHub-linux-amd64.tar.gz" ;;
  Linux/aarch64|Linux/arm64) ASSET="McpHub-linux-arm64.tar.gz" ;;
  *)
    echo "No prebuilt binary for $(uname -s) $(uname -m). Build it: git clone https://github.com/$REPO && cd McpHub && cargo build --release"
    exit 1 ;;
esac

if [ "$VERSION" = "latest" ]; then
  BASE="https://github.com/$REPO/releases/latest/download"
else
  BASE="https://github.com/$REPO/releases/download/$VERSION"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "Downloading McpHub ($VERSION, $ASSET)..."
curl -fsSL "$BASE/$ASSET" -o "$TMP/$ASSET"
if ! curl -fsSL "$BASE/$ASSET.sha256" -o "$TMP/$ASSET.sha256"; then
  echo "No checksum published for $ASSET: refusing to install an unverified binary." >&2
  exit 1
fi
echo "Verifying checksum..."
if command -v sha256sum >/dev/null; then
  (cd "$TMP" && sha256sum -c "$ASSET.sha256" >/dev/null)
else
  (cd "$TMP" && shasum -a 256 -c "$ASSET.sha256" >/dev/null)
fi

tar -xzf "$TMP/$ASSET" -C "$TMP"
mkdir -p "$INSTALL_DIR"
install -m 755 "$TMP/McpHub" "$INSTALL_DIR/McpHub"
if [ "$(uname -s)" = "Darwin" ]; then
  xattr -d com.apple.quarantine "$INSTALL_DIR/McpHub" 2>/dev/null || true
fi

echo ""
echo "McpHub installed: $INSTALL_DIR/McpHub"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "Add it to your PATH:  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.$(basename "${SHELL:-bash}")rc" ;;
esac
if [ "$(uname -s)" = "Linux" ] && ! command -v bwrap >/dev/null; then
  echo ""
  echo "Sandbox: install bubblewrap so servers run isolated from your keys and secrets:"
  echo "  sudo apt install bubblewrap    # or: dnf install bubblewrap / apk add bubblewrap"
fi
echo ""
echo "Next steps:"
echo "  1. McpHub dashboard    # Library tab: add servers from the official MCP Registry"
echo "     or McpHub registry search postgres && McpHub registry install NAME"
echo "  2. McpHub install      # start the daemon at login (launchd on macOS, systemd --user on Linux)"
echo "  3. Point your agent at http://127.0.0.1:24680/mcp (token in ~/.McpHub/auth-token)"
echo "  Updates install themselves once the daemon runs (signed releases; opt out with \"autoUpdate\": false in settings)."
echo "  Moving from another machine?  McpHub sync export file --secrets   then   McpHub sync import file --secrets"
