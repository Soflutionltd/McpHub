"""Données déterministes des faux serveurs et réponses attendues des tâches.

Tout est généré avec une graine fixe : deux exécutions voient exactement les mêmes données.
"""

import random

SEED = 20261128
SLOW_SECS = 75  # au-delà du délai d'environ 60 s des clients MCP courants


def _rng(salt: str) -> random.Random:
    return random.Random(f"{SEED}-{salt}")


FIRST = ["Alice", "Bruno", "Chloe", "David", "Emma", "Farid", "Gaelle", "Hugo", "Ines", "Jules",
         "Karim", "Lea", "Marc", "Nina", "Oscar", "Paula", "Quentin", "Rose", "Simon", "Tess"]
LAST = ["Martin", "Bernard", "Dubois", "Thomas", "Robert", "Richard", "Petit", "Durand", "Leroy",
        "Moreau", "Simon", "Laurent", "Lefebvre", "Michel", "Garcia", "David", "Bertrand", "Roux"]


def customers() -> list[dict]:
    rng = _rng("customers")
    rows = []
    for i in range(60):
        first, last = rng.choice(FIRST), rng.choice(LAST)
        rows.append({
            "id": f"cus_{1000 + i}",
            "name": f"{first} {last} #{i}",
            "email": f"{first.lower()}.{last.lower()}{i}@example.com",
        })
    return rows


def invoices() -> list[dict]:
    rng = _rng("invoices")
    rows = []
    for n, cus in enumerate(customers()):
        for k in range(rng.randint(1, 4)):
            rows.append({
                "number": f"INV-{2026}-{n:03d}{k}",
                "customer_id": cus["id"],
                "amount_eur": rng.randint(40, 2400),
                "status": rng.choice(["paid", "paid", "open", "overdue"]),
            })
    return rows


def orders() -> list[dict]:
    rng = _rng("orders")
    return [{
        "id": f"ord_{5000 + i}",
        "status": rng.choice(["shipped", "pending", "delivered", "cancelled"]),
        "amount_eur": rng.randint(10, 900),
    } for i in range(80)]


def users() -> list[dict]:
    rng = _rng("users")
    return [{"email": c["email"], "user_id": f"usr_{i}", "org_id": f"org_{rng.randint(1, 15)}"}
            for i, c in enumerate(customers())]


def orgs() -> list[dict]:
    rng = _rng("orgs")
    return [{"org_id": f"org_{i}", "name": f"Org {i}", "plan_id": f"plan_{rng.randint(1, 5)}"}
            for i in range(1, 16)]


def plans() -> list[dict]:
    return [{"plan_id": f"plan_{i}", "name": n, "monthly_price_eur": p}
            for i, (n, p) in enumerate([("Starter", 19), ("Team", 49), ("Business", 129),
                                        ("Scale", 349), ("Enterprise", 990)], start=1)]


def logs() -> list[dict]:
    rng = _rng("logs")
    services = ["api", "billing", "auth", "search", "worker", "mailer"]
    levels = ["info"] * 12 + ["warn"] * 3 + ["error"]
    rows = []
    for i in range(1500):
        rows.append({
            "ts": f"2026-11-27T{i // 120:02d}:{(i // 2) % 60:02d}:{(i * 7) % 60:02d}Z",
            "service": rng.choice(services),
            "level": rng.choice(levels),
            "message": rng.choice([
                "request completed", "cache miss", "retrying upstream call",
                "slow query detected", "token refreshed", "connection reset by peer",
                "payload validated", "job enqueued",
            ]),
            "request_id": f"req_{rng.getrandbits(40):010x}",
        })
    return rows


def crm(account: str) -> list[dict]:
    rng = _rng(f"crm-{account}")
    return [{
        "customer_id": f"C-{100 + i}",
        "company": f"{account.capitalize()} client {i}",
        "account_manager": rng.choice(FIRST),
        "arr_eur": rng.randint(1000, 90000),
    } for i in range(30)]


def slow_report_total() -> int:
    return sum(o["amount_eur"] for o in orders() if o["status"] == "delivered")


# --- Tâches -------------------------------------------------------------------------------

def _task_lookup():
    cus = customers()[17]
    inv = [i for i in invoices() if i["customer_id"] == cus["id"]]
    latest = inv[-1]
    return {
        "id": "1-lookup",
        "title": "Recherche simple (2 appels)",
        "prompt": f"What is the number of the most recent invoice (the last one listed) for the "
                  f"customer named \"{cus['name']}\"? Answer with the invoice number only.",
        "expected": [latest["number"]],
    }


def _task_many_reads():
    ids = [o["id"] for o in orders()[:20]]
    total = sum(o["amount_eur"] for o in orders()[:20] if o["status"] == "shipped")
    return {
        "id": "2-many-reads",
        "title": "20 lectures independantes",
        "prompt": "Look up each of these 20 orders and give the total amount in EUR of the ones "
                  f"whose status is \"shipped\": {', '.join(ids)}. Answer with the number only.",
        "expected": [str(total)],
    }


def _task_chain():
    cus = customers()[42]
    user = next(u for u in users() if u["email"] == cus["email"])
    org = next(o for o in orgs() if o["org_id"] == user["org_id"])
    plan = next(p for p in plans() if p["plan_id"] == org["plan_id"])
    return {
        "id": "3-chain",
        "title": "Enchainement dependant (4 appels)",
        "prompt": f"The user with email {cus['email']} belongs to an organization. What is the "
                  "monthly price in EUR of that organization's plan? Answer with the number only.",
        "expected": [str(plan["monthly_price_eur"])],
    }


def _task_big_filter():
    n = sum(1 for r in logs() if r["service"] == "billing" and r["level"] == "error")
    return {
        "id": "4-big-filter",
        "title": "Grosse reponse a filtrer (1 500 lignes)",
        "prompt": "Using the logs of 2026-11-27, how many log entries have service \"billing\" "
                  "and level \"error\"? Answer with the number only.",
        "expected": [str(n)],
    }


def _task_homonyms():
    row = crm("beta")[23]
    return {
        "id": "5-homonyms",
        "title": "Deux comptes clients, outils homonymes",
        "prompt": "Two CRM accounts are connected: Acme and Beta, with the same tools. In the "
                  "BETA account only, who is the account manager of customer C-123? Answer with "
                  "the first name only.",
        "expected": [row["account_manager"]],
    }


def _task_slow():
    return {
        "id": "6-slow",
        "title": f"Appel long ({SLOW_SECS} s)",
        "prompt": "Run the monthly sales report for 2026-11 and tell me the total delivered "
                  "amount in EUR it returns. Answer with the number only.",
        "expected": [str(slow_report_total())],
    }


def tasks() -> list[dict]:
    return [_task_lookup(), _task_many_reads(), _task_chain(), _task_big_filter(),
            _task_homonyms(), _task_slow()]
