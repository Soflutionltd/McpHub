#!/bin/bash
# McpHub install script
set -e

INSTALL_DIR="$HOME/.local/bin"
mkdir -p "$INSTALL_DIR"

echo "Building McpHub (release)..."
cargo build --release

echo "Installing binary..."
cp target/release/McpHub "$INSTALL_DIR/McpHub"
# Signature stable si un certificat Developer ID est present (macOS garde alors ses autorisations).
SIGN_ID=$(security find-identity -v -p codesigning 2>/dev/null | grep -m1 "Developer ID Application" | sed -E 's/.*"(.*)"/\1/' || true)
if [ -n "$SIGN_ID" ]; then
  codesign --force --sign "$SIGN_ID" --identifier com.soflution.mcphub --options runtime --timestamp "$INSTALL_DIR/McpHub"
else
  codesign --force --sign - --identifier com.soflution.mcphub "$INSTALL_DIR/McpHub"
fi

echo "✓ McpHub installed to $INSTALL_DIR/McpHub"
echo ""

CACHE_FILE="$HOME/.McpHub/schema-cache.json"
if [ ! -f "$CACHE_FILE" ]; then
    echo "Cache not found. Generating cache..."
    "$INSTALL_DIR/McpHub" generate
else
    echo "✓ Cache already exists."
fi

echo ""
read -p "Do you want to install the auto-start daemon (McpHub install)? [y/N] " -n 1 -r
echo
if [[ $REPLY =~ ^[Yy]$ ]]; then
    "$INSTALL_DIR/McpHub" install
fi

echo ""
echo "========================================="
echo "✅ McpHub installation complete!"
echo "========================================="
echo "Add this to your Cursor MCP settings (~/.cursor/mcp.json):"
echo ""

AUTH_TOKEN=""
if [ -f "$HOME/.McpHub/auth-token" ]; then
    AUTH_TOKEN=$(cat "$HOME/.McpHub/auth-token")
fi

cat <<EOF
{
  "mcpServers": {
    "McpHub": {
      "url": "http://127.0.0.1:24680/sse",
      "headers": {
        "Authorization": "Bearer ${AUTH_TOKEN:-<your-token>}"
      }
    }
  }
}
EOF
echo ""
echo "Done. Restart Cursor to pick up the new settings."
