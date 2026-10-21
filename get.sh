#!/bin/bash
# McpHub quick install (macOS, Apple Silicon and Intel).
#   curl -fsSL https://raw.githubusercontent.com/Soflutionltd/McpHub/main/get.sh | bash
# Pin a version:  curl -fsSL .../get.sh | MCPHUB_VERSION=v5.8.0 bash
set -euo pipefail

REPO="Soflutionltd/McpHub"
VERSION="${MCPHUB_VERSION:-latest}"
ASSET="McpHub-macos-universal.tar.gz"
INSTALL_DIR="${MCPHUB_INSTALL_DIR:-$HOME/.local/bin}"

if [ "$(uname -s)" != "Darwin" ]; then
  echo "Prebuilt binaries are for macOS. Elsewhere: git clone https://github.com/$REPO && cd McpHub && cargo build --release"
  exit 1
fi

if [ "$VERSION" = "latest" ]; then
  BASE="https://github.com/$REPO/releases/latest/download"
else
  BASE="https://github.com/$REPO/releases/download/$VERSION"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "Downloading McpHub ($VERSION)..."
curl -fsSL "$BASE/$ASSET" -o "$TMP/$ASSET"
curl -fsSL "$BASE/$ASSET.sha256" -o "$TMP/$ASSET.sha256"

echo "Verifying checksum..."
(cd "$TMP" && shasum -a 256 -c "$ASSET.sha256" >/dev/null)

tar -xzf "$TMP/$ASSET" -C "$TMP"
mkdir -p "$INSTALL_DIR"
install -m 755 "$TMP/McpHub" "$INSTALL_DIR/McpHub"
xattr -d com.apple.quarantine "$INSTALL_DIR/McpHub" 2>/dev/null || true

echo ""
echo "McpHub installed: $INSTALL_DIR/McpHub"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "Add it to your PATH:  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.zshrc" ;;
esac
echo ""
echo "Next steps:"
echo "  1. McpHub add          # add your MCP servers (or edit ~/.McpHub/config.json)"
echo "  2. McpHub generate     # index every tool once"
echo "  3. McpHub install      # start the daemon at login"
echo "  4. Point your editor at http://127.0.0.1:24680/mcp (token in ~/.McpHub/auth-token)"
