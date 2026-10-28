#!/usr/bin/env python3
"""Manifeste des mises à jour automatiques (latest.json), signé ensuite avec minisign.

Nouvelle release :
    make-manifest.py new --version 5.10.0 --dir dist --percent 10 --full-after-hours 24 > latest.json
Pause, reprise ou déploiement complet d'une release publiée (empreintes inchangées) :
    make-manifest.py edit --from latest.json --action pause|resume|full > latest.json.new
"""
import argparse
import hashlib
import json
import pathlib
import re
import sys
import time

ASSET = re.compile(r"^(McpHub-.+\.(tar\.gz|zip)|mcphub\.mcpb|mcphub-client\.mjs)$")


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def new_manifest(args):
    if not re.fullmatch(r"\d+\.\d+\.\d+", args.version):
        sys.exit(f"invalid version: {args.version}")
    if not 0 <= args.percent <= 100:
        sys.exit("--percent must be between 0 and 100")
    files = sorted(p for p in pathlib.Path(args.dir).rglob("*") if p.is_file() and ASSET.match(p.name))
    assets = {}
    for path in files:
        if path.name in assets:
            sys.exit(f"duplicate asset: {path.name}")
        assets[path.name] = sha256(path)
    if not assets:
        sys.exit(f"no release asset found in {args.dir}")
    now = int(time.time())
    return {
        "version": args.version,
        "published_at": now,
        "rollout": {"percent": args.percent, "full_at": now + args.full_after_hours * 3600},
        "paused": False,
        "assets": assets,
    }


def edit_manifest(args):
    manifest = json.loads(pathlib.Path(args.source).read_text())
    if args.action == "pause":
        manifest["paused"] = True
    elif args.action == "resume":
        manifest["paused"] = False
    elif args.action == "full":
        manifest["paused"] = False
        manifest["rollout"]["full_at"] = int(time.time())
    return manifest


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    new = sub.add_parser("new")
    new.add_argument("--version", required=True)
    new.add_argument("--dir", required=True)
    new.add_argument("--percent", type=int, default=10)
    new.add_argument("--full-after-hours", type=int, default=24)
    edit = sub.add_parser("edit")
    edit.add_argument("--from", dest="source", required=True)
    edit.add_argument("--action", choices=["pause", "resume", "full"], required=True)
    args = parser.parse_args()
    manifest = new_manifest(args) if args.command == "new" else edit_manifest(args)
    json.dump(manifest, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
