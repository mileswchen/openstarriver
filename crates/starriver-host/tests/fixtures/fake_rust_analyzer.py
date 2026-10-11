#!/usr/bin/env python3
"""Minimal fake rust-analyzer for e2e: answers initialize, documentSymbol,
definition/references/implementation/hover and shutdown over
Content-Length-framed stdio. Unknown requests get a null result so the
client never waits for a timeout."""
import json
import sys


def read_frame():
    headers = {}
    while True:
        line = sys.stdin.readline()
        if line in ("\r\n", "\n", ""):
            break
        key, _, value = line.partition(":")
        headers[key.strip().lower()] = value.strip()
    length = int(headers.get("content-length", 0))
    if length == 0:
        return None
    return json.loads(sys.stdin.read(length))


def write_frame(payload):
    body = json.dumps(payload).encode("utf-8")
    sys.stdout.write(f"Content-Length: {len(body)}\r\n\r\n")
    sys.stdout.write(body.decode("utf-8"))
    sys.stdout.flush()


def main():
    while True:
        msg = read_frame()
        if msg is None:
            break
        method = msg.get("method")
        msg_id = msg.get("id")
        if method == "initialize":
            write_frame({"jsonrpc": "2.0", "id": msg_id, "result": {
                "serverInfo": {"name": "fake", "version": "1"},
                "capabilities": {"documentSymbolProvider": True,
                                 "definitionProvider": True,
                                 "referencesProvider": True,
                                 "implementationProvider": True,
                                 "hoverProvider": True},
            }})
        elif method == "textDocument/documentSymbol":
            write_frame({"jsonrpc": "2.0", "id": msg_id, "result": [{
                "name": "main", "kind": 12,
                "range": {"start": {"line": 0, "character": 0},
                          "end": {"line": 3, "character": 0}},
                "selectionRange": {"start": {"line": 0, "character": 0},
                                   "end": {"line": 0, "character": 4}},
                "children": [],
            }]})
        elif method in ("textDocument/definition", "textDocument/references",
                        "textDocument/implementation"):
            uri = msg["params"]["textDocument"]["uri"]
            write_frame({"jsonrpc": "2.0", "id": msg_id, "result": [{
                "uri": uri,
                "range": {"start": {"line": 0, "character": 0},
                          "end": {"line": 0, "character": 4}},
            }]})
        elif method == "textDocument/hover":
            write_frame({"jsonrpc": "2.0", "id": msg_id, "result": {
                "contents": {"kind": "markdown", "value": "fn main()\n\nfake hover"},
            }})
        elif method == "shutdown":
            write_frame({"jsonrpc": "2.0", "id": msg_id, "result": None})
        elif method == "exit":
            break
        elif msg_id is not None:
            write_frame({"jsonrpc": "2.0", "id": msg_id, "result": None})


if __name__ == "__main__":
    main()
