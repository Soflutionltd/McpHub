#!/bin/sh
# Starts the McpHub binary that matches this machine (macOS universal, Linux x64 or arm64).
dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
case "$(uname -s)" in
  Darwin) bin="$dir/darwin/McpHub" ;;
  Linux)
    case "$(uname -m)" in
      aarch64|arm64) bin="$dir/linux-arm64/McpHub" ;;
      *) bin="$dir/linux-x64/McpHub" ;;
    esac ;;
  *) echo "McpHub: unsupported system $(uname -s)" >&2; exit 1 ;;
esac
[ -x "$bin" ] || chmod +x "$bin" 2>/dev/null
exec "$bin" "$@"
