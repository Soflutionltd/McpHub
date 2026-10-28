#!/bin/sh
# Pose le certificat Developer ID Application (équipe TB8CC687M3) dans l'environnement GitHub
# « release » (MACOS_CERT_P12, MACOS_CERT_PASSWORD) et en garde une copie dans 1Password.
# Seule cette identité part : les certificats Apple Development restent dans le Trousseau.
# À relancer au renouvellement du certificat. macOS demande d'autoriser l'export de la clé.
set -eu
repo="Soflutionltd/McpHub"
identity="Developer ID Application: Soflution ltd (TB8CC687M3)"
openssl=/usr/bin/openssl

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
umask 077

security find-certificate -c "$identity" -p > "$work/cert.pem"
security find-certificate -a -c "Developer ID Certification Authority" -p > "$work/chain.pem"
cert_key=$("$openssl" x509 -in "$work/cert.pem" -noout -pubkey)

EXPORT_PW=$("$openssl" rand -hex 24)
export EXPORT_PW
security export -k "$HOME/Library/Keychains/login.keychain-db" -t identities -f pkcs12 \
  -P "$EXPORT_PW" -o "$work/all.p12"
"$openssl" pkcs12 -in "$work/all.p12" -passin env:EXPORT_PW -nocerts -nodes 2>/dev/null \
  | awk -v dir="$work" '/-----BEGIN/{n++; f=dir "/key" n ".pem"} f{print > f} /-----END/{f=""}'
rm -f "$work/all.p12"

match=""
for key in "$work"/key*.pem; do
  if [ "$("$openssl" pkey -in "$key" -pubout 2>/dev/null)" = "$cert_key" ]; then
    match="$key"
  fi
done
[ -n "$match" ] || { echo "private key of \"$identity\" not found" >&2; exit 1; }

P12_PW=$("$openssl" rand -hex 24)
export P12_PW
"$openssl" pkcs12 -export -inkey "$match" -in "$work/cert.pem" -certfile "$work/chain.pem" \
  -name "$identity" -passout env:P12_PW -out "$work/developer-id.p12"
rm -f "$work"/key*.pem

base64 -i "$work/developer-id.p12" | tr -d '\n' | gh secret set MACOS_CERT_P12 --env release -R "$repo"
printf '%s' "$P12_PW" | gh secret set MACOS_CERT_PASSWORD --env release -R "$repo"

title="McpHub CI — certificat Developer ID (p12)"
op-sof document create "$work/developer-id.p12" --vault Soflution --title "$title" \
  --file-name developer-id.p12 --tags mcphub > /dev/null
python3 -c 'import json, os, sys; json.dump({"title": sys.argv[1], "category": "PASSWORD", "tags": ["mcphub"],
  "fields": [{"id": "password", "type": "CONCEALED", "purpose": "PASSWORD", "label": "password",
  "value": os.environ["P12_PW"]}]}, open(sys.argv[2], "w"))' "$title, mot de passe" "$work/item.json"
op-sof item create --vault Soflution --template "$work/item.json" > /dev/null
echo "MACOS_CERT_P12 and MACOS_CERT_PASSWORD set on $repo (environment release), copy in 1Password"
