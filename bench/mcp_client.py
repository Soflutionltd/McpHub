"""Client MCP minimal en stdio (JSON-RPC ligne à ligne), avec délai par appel comme un vrai client."""

import json
import os
import queue
import subprocess
import threading


class McpTimeout(Exception):
    pass


class StdioMcp:
    def __init__(self, cmd: list[str], env: dict[str, str] | None = None):
        self.proc = subprocess.Popen(
            cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            env=env, text=True, bufsize=1,
        )
        self._next_id = 0
        self._pending: dict[int, queue.Queue] = {}
        self._lock = threading.Lock()
        threading.Thread(target=self._reader, daemon=True).start()
        init = self.request("initialize", {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "mcphub-bench", "version": "1.0.0"},
        }, timeout=60)
        self.instructions = init.get("instructions") or ""
        self._send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def _reader(self) -> None:
        assert self.proc.stdout is not None
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            msg_id = msg.get("id")
            if msg_id is not None and "method" not in msg:
                with self._lock:
                    slot = self._pending.pop(msg_id, None)
                if slot is not None:
                    slot.put(msg)
            elif "method" in msg and msg_id is not None:
                # Requête du serveur (roots/list, ping…) : réponse vide polie.
                result = {"roots": []} if msg["method"] == "roots/list" else {}
                self._send({"jsonrpc": "2.0", "id": msg_id, "result": result})

    def _send(self, msg: dict) -> None:
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def request(self, method: str, params: dict, timeout: float) -> dict:
        with self._lock:
            self._next_id += 1
            msg_id = self._next_id
            slot: queue.Queue = queue.Queue(maxsize=1)
            self._pending[msg_id] = slot
        self._send({"jsonrpc": "2.0", "id": msg_id, "method": method, "params": params})
        try:
            msg = slot.get(timeout=timeout)
        except queue.Empty:
            with self._lock:
                self._pending.pop(msg_id, None)
            self._send({"jsonrpc": "2.0", "method": "notifications/cancelled",
                        "params": {"requestId": msg_id, "reason": "client timeout"}})
            raise McpTimeout(f"{method} timed out after {timeout:.0f}s")
        if "error" in msg:
            raise RuntimeError(msg["error"].get("message", str(msg["error"])))
        return msg.get("result") or {}

    def list_tools(self) -> list[dict]:
        tools, cursor = [], None
        while True:
            params = {"cursor": cursor} if cursor else {}
            res = self.request("tools/list", params, timeout=60)
            tools += res.get("tools", [])
            cursor = res.get("nextCursor")
            if not cursor:
                return tools

    def call(self, name: str, args: dict, timeout: float) -> tuple[str, bool]:
        """Renvoie (texte du résultat, erreur ?)."""
        res = self.request("tools/call", {"name": name, "arguments": args}, timeout=timeout)
        parts = []
        for block in res.get("content", []):
            if block.get("type") == "text":
                parts.append(block["text"])
            else:
                parts.append(json.dumps(block))
        return "\n".join(parts), bool(res.get("isError"))

    def close(self) -> None:
        try:
            if self.proc.stdin:
                self.proc.stdin.close()
            self.proc.wait(timeout=5)
        except (subprocess.TimeoutExpired, OSError):
            self.proc.kill()


def clean_env(extra: dict[str, str] | None = None) -> dict[str, str]:
    env = {"PATH": "/usr/bin:/bin:/opt/homebrew/bin", "LANG": "en_US.UTF-8"}
    if "TMPDIR" in os.environ:
        env["TMPDIR"] = os.environ["TMPDIR"]
    env.update(extra or {})
    return env
