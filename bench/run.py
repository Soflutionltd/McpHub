"""Banc d'essai McpHub : même tâche, même modèle, avec et sans McpHub.

    bench/.venv/bin/python bench/run.py --reps 5 --jobs 4
    bench/.venv/bin/python bench/run.py --tasks 1-lookup --modes mcphub --reps 1

La clé API est lue dans ANTHROPIC_API_KEY (voir bench/README.md pour la lire depuis 1Password).
Chaque exécution démarre des processus neufs ; les résultats bruts vont dans bench/results/.
"""

import argparse
import datetime as dt
import json
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import anthropic

BENCH = Path(__file__).resolve().parent
sys.path.insert(0, str(BENCH))
import data
from mcp_client import McpTimeout, StdioMcp, clean_env

MODEL = "claude-sonnet-5-5"
# Tarifs Anthropic, Claude Sonnet 5.5, en $ par million de tokens (lus le 2026-11-28 dans la
# documentation officielle ; écriture en cache : 1,25 × l'entrée, durée de vie de 5 minutes).
PRICE = {"input": 2.00, "output": 10.00, "cache_read": 0.10, "cache_write": 2.50}
CLIENT_TIMEOUT = 60  # délai d'un appel d'outil côté client (ordre de grandeur des apps Claude)
MAX_TURNS = 30
SERVERS = ["shop", "logs", "crm_acme", "crm_beta",
           "noise_calendar", "noise_docs", "noise_tickets", "noise_analytics"]
SYSTEM = ("You are an assistant with tools connected through MCP. Use the tools to answer. "
          "When you have the answer, reply with the answer only.")


# Copie des faux serveurs hors du dépôt : McpHub ignore tout serveur dont le chemin contient
# « McpHub » (protection contre une boucle sur lui-même), et le dépôt s'appelle McpHub.
SERVER_DIR = Path(tempfile.mkdtemp(prefix="bench-servers-"))
for _name in ("fake_server.py", "data.py"):
    shutil.copy(BENCH / _name, SERVER_DIR / _name)
SERVER_PYTHON = shutil.which("python3", path="/opt/homebrew/bin:/usr/local/bin:/usr/bin") or "python3"


def server_cmd(kind: str) -> list[str]:
    return [SERVER_PYTHON, "-I", str(SERVER_DIR / "fake_server.py"), kind]


# --- Connexion aux outils -----------------------------------------------------------------

class Direct:
    """Sans McpHub : chaque serveur branché directement, tous ses outils envoyés au modèle."""

    def __init__(self):
        self.clients: dict[str, StdioMcp] = {}
        self.tools: list[dict] = []
        self.route: dict[str, tuple[str, str]] = {}
        for kind in SERVERS:
            client = StdioMcp(server_cmd(kind), clean_env())
            self.clients[kind] = client
            for tool in client.list_tools():
                name = f"{kind}__{tool['name']}"
                self.route[name] = (kind, tool["name"])
                self.tools.append({
                    "name": name,
                    "description": f"[{kind}] {tool.get('description', '')}",
                    "input_schema": tool.get("inputSchema") or {"type": "object"},
                })
        self.instructions = ""

    def call(self, name: str, args: dict) -> tuple[str, bool]:
        kind, tool = self.route[name]
        return self.clients[kind].call(tool, args, timeout=CLIENT_TIMEOUT)

    def pids(self) -> list[int]:
        return [c.proc.pid for c in self.clients.values()]

    def close(self) -> None:
        for client in self.clients.values():
            client.close()


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Hub:
    """Avec McpHub, chemin réel d'un utilisateur : client Node en stdio, démon McpHub derrière.

    Le démon tourne isolé (dossier, port et coffre temporaires) : rien de la vraie
    installation n'est lu ni modifié.
    """

    def __init__(self, binary: str, client_js: str):
        self.root = Path(tempfile.mkdtemp(prefix="mcphub-bench-"))
        hub = self.root / "hub"
        (self.root / "home").mkdir()
        hub.mkdir()
        config = {
            "servers": {kind: {"command": server_cmd(kind)[0], "args": server_cmd(kind)[1:],
                               "sandbox": False} for kind in SERVERS},
            "settings": {"preload": "all"},
        }
        (hub / "config.json").write_text(json.dumps(config, indent=2))
        (hub / "secrets.env").write_text("")
        port = free_port()
        env = clean_env({
            "HOME": str(self.root / "home"),
            "MCPHUB_HOME": str(hub),
            "MCPHUB_PORT": str(port),
            "MCPHUB_VAULT_BACKEND": "file",
            "MCPHUB_AUTO_UPDATE": "0",
        })
        self.daemon = subprocess.Popen([binary, "serve"], env=env, stdin=subprocess.DEVNULL,
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 30
        while not port_open(port):
            if time.monotonic() > deadline or self.daemon.poll() is not None:
                self.daemon.kill()
                raise RuntimeError("le démon McpHub de test n'a pas démarré")
            time.sleep(0.1)
        node = shutil.which("node") or "node"
        env["PATH"] = f"{Path(node).parent}:{env['PATH']}"
        env["MCPHUB_BINARY"] = binary
        self.client = StdioMcp([node, client_js], env)
        self.tools = [{
            "name": t["name"],
            "description": t.get("description", ""),
            "input_schema": t.get("inputSchema") or {"type": "object"},
        } for t in self.client.list_tools()]
        self.instructions = self.client.instructions

    def call(self, name: str, args: dict) -> tuple[str, bool]:
        return self.client.call(name, args, timeout=CLIENT_TIMEOUT)

    def pids(self) -> list[int]:
        return [self.client.proc.pid, self.daemon.pid]

    def close(self) -> None:
        self.client.close()
        self.daemon.terminate()
        try:
            self.daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.daemon.kill()
        shutil.rmtree(self.root, ignore_errors=True)


def port_open(port: int) -> bool:
    with socket.socket() as s:
        s.settimeout(0.2)
        return s.connect_ex(("127.0.0.1", port)) == 0


def tree_rss_mb(roots: list[int]) -> float:
    """Mémoire résidente totale des processus et de leurs descendants, en Mo."""
    out = subprocess.run(["ps", "-A", "-o", "pid=,ppid=,rss="], check=True,
                         capture_output=True, text=True)
    children: dict[int, list[int]] = {}
    rss: dict[int, int] = {}
    for line in out.stdout.splitlines():
        pid, ppid, kb = (int(x) for x in line.split())
        children.setdefault(ppid, []).append(pid)
        rss[pid] = kb
    total, stack = 0, list(roots)
    while stack:
        pid = stack.pop()
        total += rss.get(pid, 0)
        stack += children.get(pid, [])
    return round(total / 1024, 1)


# --- Une exécution ------------------------------------------------------------------------

def normalize(text: str) -> str:
    return re.sub(r"(?<=\d)[,   ](?=\d{3}\b)", "", text)


def succeeded(answer: str, expected: list[str]) -> bool:
    text = normalize(answer)
    return all(re.search(rf"(?<![\w-]){re.escape(e)}(?![\w-])", text) for e in expected)


def run_once(api: anthropic.Anthropic, task: dict, mode: str, rep: int, binary: str,
             client_js: str, effort: str) -> dict:
    t_setup = time.monotonic()
    conn = Direct() if mode == "direct" else Hub(binary, client_js)
    setup_s = time.monotonic() - t_setup
    system = SYSTEM + (f"\n\n<mcp_server_instructions>\n{conn.instructions}\n"
                       "</mcp_server_instructions>" if conn.instructions else "")
    messages: list = [{"role": "user", "content": task["prompt"]}]
    usage = {"input": 0, "output": 0, "cache_read": 0, "cache_write": 0}
    tool_calls, tool_errors, timeouts, turns = 0, 0, 0, 0
    answer, stop, error, peak_rss = "", "", None, 0.0
    started = time.monotonic()
    try:
        while turns < MAX_TURNS:
            turns += 1
            resp = api.messages.create(
                model=MODEL, max_tokens=16000, system=system, tools=conn.tools,
                messages=messages, cache_control={"type": "ephemeral"},
                output_config={"effort": effort},
            )
            u = resp.usage
            usage["input"] += u.input_tokens
            usage["output"] += u.output_tokens
            usage["cache_read"] += u.cache_read_input_tokens or 0
            usage["cache_write"] += u.cache_creation_input_tokens or 0
            stop = resp.stop_reason or ""
            messages.append({"role": "assistant", "content": resp.content})
            if stop != "tool_use":
                answer = "".join(b.text for b in resp.content if b.type == "text")
                break
            results = []
            for block in resp.content:
                if block.type != "tool_use":
                    continue
                tool_calls += 1
                try:
                    text, is_err = conn.call(block.name, dict(block.input))
                except McpTimeout:
                    timeouts += 1
                    text, is_err = "MCP error -32001: Request timed out", True
                except (RuntimeError, KeyError) as exc:
                    text, is_err = f"MCP error: {exc}", True
                tool_errors += int(is_err)
                results.append({"type": "tool_result", "tool_use_id": block.id,
                                "content": text, "is_error": is_err})
            peak_rss = max(peak_rss, tree_rss_mb(conn.pids()))
            messages.append({"role": "user", "content": results})
        else:
            stop = "max_turns"
    except anthropic.APIError as exc:
        error = f"{type(exc).__name__}: {exc}"
    finally:
        peak_rss = max(peak_rss, tree_rss_mb(conn.pids()))
        conn.close()
    cost = sum(usage[k] * PRICE[k] for k in usage) / 1e6
    return {
        "task": task["id"], "mode": mode, "rep": rep, "model": MODEL, "effort": effort,
        "success": error is None and succeeded(answer, task["expected"]),
        "answer": answer.strip()[:300], "expected": task["expected"], "stop": stop,
        "error": error, "turns": turns, "tool_calls": tool_calls, "tool_errors": tool_errors,
        "client_timeouts": timeouts, "tools_exposed": len(conn.tools),
        "tokens": usage, "cost_usd": round(cost, 5),
        "wall_s": round(time.monotonic() - started, 2), "setup_s": round(setup_s, 2),
        "peak_rss_mb": peak_rss,
    }


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("--reps", type=int, default=5)
    p.add_argument("--jobs", type=int, default=4)
    p.add_argument("--tasks", default="all", help="ids séparés par des virgules")
    p.add_argument("--modes", default="direct,mcphub")
    p.add_argument("--effort", default="medium")
    p.add_argument("--mcphub", default=shutil.which("McpHub") or "McpHub")
    p.add_argument("--client-js", default=str(Path.home() / "Cursor/App/MCP/McpHub/client/index.js"))
    p.add_argument("--out", default=None)
    args = p.parse_args()

    tasks = [t for t in data.tasks() if args.tasks == "all" or t["id"] in args.tasks.split(",")]
    modes = args.modes.split(",")
    version = subprocess.run([args.mcphub, "--version"], check=True, capture_output=True,
                             text=True).stdout
    out = Path(args.out) if args.out else (
        BENCH / "results" / f"raw-{dt.datetime.now():%Y%m%d-%H%M%S}.jsonl")
    out.parent.mkdir(exist_ok=True)
    meta = {"meta": True, "date": dt.datetime.now().isoformat(timespec="seconds"),
            "model": MODEL, "effort": args.effort, "mcphub": version.strip(),
            "sdk": anthropic.__version__, "client_timeout_s": CLIENT_TIMEOUT,
            "servers": SERVERS, "price_per_mtok_usd": PRICE}
    out.write_text(json.dumps(meta) + "\n")

    api = anthropic.Anthropic(max_retries=6)
    jobs = [(t, m, r) for r in range(args.reps) for t in tasks for m in modes]
    lock = threading.Lock()

    def work(job):
        task, mode, rep = job
        row = run_once(api, task, mode, rep, args.mcphub, args.client_js, args.effort)
        with lock:
            with out.open("a") as f:
                f.write(json.dumps(row) + "\n")
            print(f"{row['task']:<14} {row['mode']:<7} rep{rep} "
                  f"{'OK ' if row['success'] else 'KO '} {row['cost_usd']:.4f}$ "
                  f"{row['wall_s']:.1f}s {row['error'] or ''}", flush=True)

    with ThreadPoolExecutor(max_workers=args.jobs) as pool:
        list(pool.map(work, jobs))
    print(f"\nRésultats bruts : {out}")


if __name__ == "__main__":
    main()
