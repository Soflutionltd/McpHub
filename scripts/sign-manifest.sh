#!/bin/sh
# Signe un manifeste de mise à jour avec la clé minisign de MINISIGN_SECRET_KEY, puis vérifie la
# signature avec la clé publique compilée dans src/update.rs : une clé qui ne correspond pas
# arrête la release au lieu de publier un manifeste que les démons refuseraient.
set -eu
manifest="$1"
: "${MINISIGN_SECRET_KEY:?MINISIGN_SECRET_KEY is not set}"
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
public_key=$(sed -n 's/^const UPDATE_PUBLIC_KEY: &str = "\(.*\)";$/\1/p' "$root/src/update.rs")
[ -n "$public_key" ] || { echo "UPDATE_PUBLIC_KEY not found in src/update.rs" >&2; exit 1; }
version=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['version'])" "$manifest")

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
umask 077
printf '%s\n' "$MINISIGN_SECRET_KEY" > "$work/key"
minisign -S -s "$work/key" -m "$manifest" -x "$manifest.minisig" -t "McpHub v$version update manifest" > /dev/null
minisign -V -P "$public_key" -m "$manifest" -x "$manifest.minisig" > /dev/null
echo "manifest for v$version signed and verified"
