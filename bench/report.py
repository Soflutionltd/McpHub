"""Rapport Markdown à partir d'un fichier de résultats bruts de run.py.

    bench/.venv/bin/python bench/report.py bench/results/raw-XXXX.jsonl > bench/results/report-XXXX.md
"""

import json
import statistics
import sys
from collections import defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import data


def p95(values: list[float]) -> float:
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, round(0.95 * (len(ordered) - 1)))]


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage : report.py <raw.jsonl>")
    lines = [json.loads(line) for line in Path(sys.argv[1]).read_text().splitlines() if line]
    meta = next((row for row in lines if row.get("meta")), {})
    rows = [row for row in lines if not row.get("meta")]
    groups: dict[tuple[str, str], list[dict]] = defaultdict(list)
    for row in rows:
        groups[(row["task"], row["mode"])].append(row)
    titles = {t["id"]: t["title"] for t in data.tasks()}

    out = [
        "# Banc d'essai McpHub : avec et sans McpHub",
        "",
        f"- Date : {meta.get('date')}",
        f"- Modèle : {meta.get('model')} (effort {meta.get('effort')}), SDK {meta.get('sdk')}",
        f"- McpHub : {meta.get('mcphub')}",
        f"- Délai client par appel d'outil : {meta.get('client_timeout_s')} s",
        f"- Tarifs utilisés ($/M tokens) : {meta.get('price_per_mtok_usd')}",
        f"- Exécutions : {len(rows)}",
        "",
        "Médianes par tâche et par mode. Coût calculé sur l'usage réel renvoyé par l'API.",
        "",
        ("| Tâche | Mode | Réussite | Coût médian | Tokens entrée (dont cache lu) | Tokens sortie "
         "| Temps médian | Temps p95 | Tours | Appels d'outils | Mémoire max |"),
        "|---|---|---|---|---|---|---|---|---|---|---|",
    ]
    totals = defaultdict(lambda: {"cost": 0.0, "ok": 0, "n": 0})
    for task in [t["id"] for t in data.tasks()]:
        for mode in ("direct", "mcphub"):
            g = groups.get((task, mode))
            if not g:
                continue
            ok = sum(r["success"] for r in g)
            totals[mode]["cost"] += sum(r["cost_usd"] for r in g)
            totals[mode]["ok"] += ok
            totals[mode]["n"] += len(g)
            tin = statistics.median(r["tokens"]["input"] + r["tokens"]["cache_read"]
                                    + r["tokens"]["cache_write"] for r in g)
            tcache = statistics.median(r["tokens"]["cache_read"] for r in g)
            out.append(
                f"| {titles.get(task, task)} | {mode} | {ok}/{len(g)} "
                f"| {statistics.median(r['cost_usd'] for r in g):.4f} $ "
                f"| {tin:,.0f} ({tcache:,.0f}) "
                f"| {statistics.median(r['tokens']['output'] for r in g):,.0f} "
                f"| {statistics.median(r['wall_s'] for r in g):.1f} s "
                f"| {p95([r['wall_s'] for r in g]):.1f} s "
                f"| {statistics.median(r['turns'] for r in g):.0f} "
                f"| {statistics.median(r['tool_calls'] for r in g):.0f} "
                f"| {max(r['peak_rss_mb'] for r in g):.0f} Mo |"
            )
    out += ["", "## Totaux", "", "| Mode | Réussite | Coût total |", "|---|---|---|"]
    for mode, t in totals.items():
        out.append(f"| {mode} | {t['ok']}/{t['n']} | {t['cost']:.2f} $ |")
    failures = [r for r in rows if not r["success"]]
    if failures:
        out += ["", "## Échecs", ""]
        for r in failures:
            out.append(f"- {r['task']} / {r['mode']} / rep {r['rep']} : arrêt `{r['stop']}`, "
                       f"réponse « {r['answer'][:120]} », attendu {r['expected']}"
                       + (f", erreur {r['error']}" if r["error"] else ""))
    print("\n".join(out))


if __name__ == "__main__":
    main()
