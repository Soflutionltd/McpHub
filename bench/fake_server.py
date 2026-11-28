"""Faux serveur MCP en stdio, déterministe, sans dépendance.

Usage : python3 fake_server.py <shop|logs|crm_acme|crm_beta|noise_*>
"""

import json
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import data

KIND = sys.argv[1] if len(sys.argv) > 1 else "shop"


def schema(props: dict[str, str], required: list[str] | None = None) -> dict:
    return {
        "type": "object",
        "properties": {k: {"type": "string", "description": v} for k, v in props.items()},
        "required": required if required is not None else list(props),
    }


def _find(rows, key, value):
    for row in rows:
        if row[key] == value:
            return row
    raise LookupError(f"no record with {key}={value}")


SHOP = {
    "find_customer": ("Find a customer by exact name. Returns id, name and email.",
                      schema({"name": "Exact customer name"}),
                      lambda a: _find(data.customers(), "name", a["name"])),
    "list_invoices": ("List the invoices of a customer, oldest first.",
                      schema({"customer_id": "Customer id, e.g. cus_1000"}),
                      lambda a: [i for i in data.invoices() if i["customer_id"] == a["customer_id"]]),
    "get_order": ("Get one order by id: status and amount.",
                  schema({"order_id": "Order id, e.g. ord_5000"}),
                  lambda a: _find(data.orders(), "id", a["order_id"])),
    "get_user": ("Get the user account for an email: user id and organization id.",
                 schema({"email": "User email"}),
                 lambda a: _find(data.users(), "email", a["email"])),
    "get_org": ("Get an organization by id: name and plan id.",
                schema({"org_id": "Organization id"}),
                lambda a: _find(data.orgs(), "org_id", a["org_id"])),
    "get_plan": ("Get a pricing plan by id: name and monthly price.",
                 schema({"plan_id": "Plan id"}),
                 lambda a: _find(data.plans(), "plan_id", a["plan_id"])),
    "monthly_sales_report": ("Compute the monthly sales report (slow: about a minute or more).",
                             schema({"month": "Month, YYYY-MM"}),
                             lambda a: (time.sleep(data.SLOW_SECS),
                                        {"month": a["month"],
                                         "delivered_total_eur": data.slow_report_total()})[1]),
    "list_products": ("List the product catalog.", schema({}, []),
                      lambda a: [{"sku": f"SKU-{i}", "price_eur": 10 + i} for i in range(5)]),
}

LOGS = {
    "list_logs": ("List all log entries of a given day (every service and level).",
                  schema({"date": "Day, YYYY-MM-DD"}),
                  lambda a: data.logs()),
    "get_log": ("Get one log entry by request id.", schema({"request_id": "Request id"}),
                lambda a: _find(data.logs(), "request_id", a["request_id"])),
}


def crm_tools(account: str) -> dict:
    return {
        "get_customer": ("Get a CRM customer by id: company, account manager, ARR.",
                         schema({"customer_id": "CRM customer id, e.g. C-100"}),
                         lambda a: _find(data.crm(account), "customer_id", a["customer_id"])),
        "list_customers": ("List CRM customers (id and company).", schema({}, []),
                           lambda a: [{"customer_id": r["customer_id"], "company": r["company"]}
                                      for r in data.crm(account)]),
        "search_customers": ("Search CRM customers by company name.",
                             schema({"query": "Text to search in company names"}),
                             lambda a: [r for r in data.crm(account)
                                        if a["query"].lower() in r["company"].lower()]),
    }


# Serveurs de bruit : des outils plausibles qu'une vraie configuration charge aussi.
NOISE_DOMAINS = {
    "noise_calendar": ["list_events", "get_event", "create_event", "update_event", "delete_event",
                       "list_calendars", "find_free_slot", "respond_to_invite", "list_attendees",
                       "set_reminder", "get_timezone", "search_events"],
    "noise_docs": ["search_docs", "get_doc", "create_doc", "update_doc", "delete_doc",
                   "list_folders", "move_doc", "share_doc", "list_comments", "add_comment",
                   "export_doc", "get_revisions"],
    "noise_tickets": ["list_tickets", "get_ticket", "create_ticket", "update_ticket",
                      "close_ticket", "assign_ticket", "add_ticket_note", "list_queues",
                      "search_tickets", "merge_tickets", "get_sla", "list_tags"],
    "noise_analytics": ["run_query", "list_dashboards", "get_dashboard", "list_metrics",
                        "get_metric", "create_alert", "list_alerts", "export_csv",
                        "get_funnel", "get_retention", "list_segments", "get_cohort"],
}


def noise_tools(kind: str) -> dict:
    out = {}
    for name in NOISE_DOMAINS[kind]:
        domain = kind.removeprefix("noise_")
        out[name] = (
            (f"{name.replace('_', ' ').capitalize()} in the {domain} workspace. Supports "
             "pagination, filtering by owner, date range and labels; returns JSON records."),
            {
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Record id when relevant"},
                    "query": {"type": "string", "description": "Free text filter"},
                    "owner": {"type": "string", "description": "Owner email"},
                    "from_date": {"type": "string", "description": "Start date, ISO 8601"},
                    "to_date": {"type": "string", "description": "End date, ISO 8601"},
                    "labels": {"type": "array", "items": {"type": "string"},
                               "description": "Labels to filter on"},
                    "page_size": {"type": "integer", "description": "Results per page (max 100)"},
                    "cursor": {"type": "string", "description": "Pagination cursor"},
                },
                "required": [],
            },
            lambda a, n=name: {"tool": n, "records": [], "next_cursor": None},
        )
    return out


def tools_for(kind: str) -> dict:
    if kind == "shop":
        return SHOP
    if kind == "logs":
        return LOGS
    if kind.startswith("crm_"):
        return crm_tools(kind.removeprefix("crm_"))
    if kind in NOISE_DOMAINS:
        return noise_tools(kind)
    raise SystemExit(f"unknown server kind: {kind}")


TOOLS = tools_for(KIND)


def reply(msg_id, result=None, error=None):
    out = {"jsonrpc": "2.0", "id": msg_id}
    if error is not None:
        out["error"] = error
    else:
        out["result"] = result
    sys.stdout.write(json.dumps(out) + "\n")
    sys.stdout.flush()


def handle(msg: dict) -> None:
    method, msg_id = msg.get("method"), msg.get("id")
    if msg_id is None:
        return  # notification
    if method == "initialize":
        reply(msg_id, {
            "protocolVersion": msg.get("params", {}).get("protocolVersion", "2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": KIND, "version": "1.0.0"},
        })
    elif method == "tools/list":
        reply(msg_id, {"tools": [{"name": n, "description": d, "inputSchema": s}
                                 for n, (d, s, _) in TOOLS.items()]})
    elif method == "tools/call":
        params = msg.get("params", {})
        name, args = params.get("name"), params.get("arguments") or {}
        if name not in TOOLS:
            reply(msg_id, error={"code": -32602, "message": f"unknown tool {name}"})
            return
        try:
            value = TOOLS[name][2](args)
            reply(msg_id, {"content": [{"type": "text", "text": json.dumps(value)}]})
        except (LookupError, KeyError) as exc:
            reply(msg_id, {"content": [{"type": "text", "text": f"error: {exc}"}], "isError": True})
    elif method == "ping":
        reply(msg_id, {})
    else:
        reply(msg_id, error={"code": -32601, "message": f"method not found: {method}"})


def main() -> None:
    for line in sys.stdin:
        line = line.strip()
        if line:
            handle(json.loads(line))


if __name__ == "__main__":
    main()
