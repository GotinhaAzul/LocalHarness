#!/usr/bin/env python3
"""Minimal MCP stdio server for LocalHarness integration tests.

Tools: echo, fail (isError result), slow (sleeps), crash (exits), env (reads
MCP_TEST_VALUE). tools/list is split over two pages to exercise cursors. A
non-JSON line on stdout and chatter on stderr check transport robustness.
"""
import json
import os
import sys
import time

TOOLS = [
    {
        "name": "echo",
        "description": "Echo the text back",
        "inputSchema": {
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
        "annotations": {"readOnlyHint": True},
    },
    {"name": "fail", "description": "Always fails", "inputSchema": {"type": "object"}},
    {"name": "slow", "description": "Sleeps", "inputSchema": {"type": "object"}},
    {"name": "crash", "description": "Exits the server", "inputSchema": {"type": "object"}},
    {"name": "env", "description": "Reads MCP_TEST_VALUE", "inputSchema": {"type": "object"}},
]


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def text(result_id, value, is_error=False):
    send({
        "jsonrpc": "2.0",
        "id": result_id,
        "result": {"content": [{"type": "text", "text": value}], "isError": is_error},
    })


print("this log line is not JSON-RPC", flush=True)
print("fixture server starting", file=sys.stderr, flush=True)

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    message = json.loads(line)
    method = message.get("method")
    msg_id = message.get("id")
    if msg_id is None:
        continue  # notification
    params = message.get("params") or {}
    if method == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "protocolVersion": params.get("protocolVersion", "2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fixture", "version": "1.0"},
            },
        })
    elif method == "tools/list":
        if params.get("cursor") == "page2":
            send({"jsonrpc": "2.0", "id": msg_id, "result": {"tools": TOOLS[2:]}})
        else:
            send({"jsonrpc": "2.0", "id": msg_id,
                  "result": {"tools": TOOLS[:2], "nextCursor": "page2"}})
    elif method == "tools/call":
        name = params.get("name")
        args = params.get("arguments") or {}
        if name == "echo":
            # Ask the client something first: it must answer ping and keep going.
            send({"jsonrpc": "2.0", "id": "srv-1", "method": "ping"})
            text(msg_id, "echo: " + args.get("text", ""))
        elif name == "fail":
            text(msg_id, "it broke", is_error=True)
        elif name == "slow":
            time.sleep(5)
            text(msg_id, "late")
        elif name == "crash":
            print("crashing on purpose", file=sys.stderr, flush=True)
            sys.exit(3)
        elif name == "env":
            text(msg_id, os.environ.get("MCP_TEST_VALUE", "<unset>"))
        else:
            send({"jsonrpc": "2.0", "id": msg_id,
                  "error": {"code": -32602, "message": "unknown tool " + str(name)}})
    elif method == "ping":
        send({"jsonrpc": "2.0", "id": msg_id, "result": {}})
    else:
        send({"jsonrpc": "2.0", "id": msg_id,
              "error": {"code": -32601, "message": "no method " + str(method)}})
