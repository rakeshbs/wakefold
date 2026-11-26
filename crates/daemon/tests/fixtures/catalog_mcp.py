"""Small MCP server used by the daemon's stdio integration test."""

import json
import os
import sys


PRICE_SCHEMA = {
    "type": "object",
    "properties": {"sku": {"type": "string"}},
    "required": ["sku"],
    "additionalProperties": False,
}
PRICES = {"WF-318": 350, "WF-319": 425}

assert "OPENAI_API_KEY" not in os.environ
assert "WAKEFOLD_DAEMON_TEST_CONFIG" not in os.environ

initialized = False
for line in sys.stdin:
    request = json.loads(line)
    method = request["method"]
    params = request.get("params") or {}
    if method == "notifications/initialized":
        initialized = True
    if "id" not in request:
        continue

    if method == "initialize":
        result = {
            "protocolVersion": "2025-11-25",
            "serverInfo": {"name": "wakefold-test-catalog", "version": "1"},
            "capabilities": {"tools": {}},
        }
    elif method == "tools/list":
        assert initialized
        if "cursor" not in params:
            result = {
                "tools": [{"name": "private_notes", "inputSchema": {"type": "object"}}],
                "nextCursor": "prices",
            }
        else:
            assert params["cursor"] == "prices"
            result = {
                "tools": [
                    {
                        "name": "lookup.price",
                        "description": "Look up the catalog price in cents.",
                        "inputSchema": PRICE_SCHEMA,
                    }
                ]
            }
    elif method == "tools/call":
        assert initialized
        assert params["name"] == "lookup.price"
        sku = params["arguments"]["sku"]
        with open("catalog-calls.jsonl", "a", encoding="utf-8") as calls:
            calls.write(json.dumps({"pid": os.getpid(), "sku": sku}) + "\n")
        result = {
            "content": [{"type": "text", "text": f"{sku} costs {PRICES[sku]} cents."}],
            "structuredContent": {"sku": sku, "price_cents": PRICES[sku]},
            "_meta": {"server_trace": "client-only"},
        }
    elif method == "ping":
        result = {}
    else:
        raise AssertionError(f"Unexpected MCP method {method}")

    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
