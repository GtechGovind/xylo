"""Deterministic JSON-RPC fixture for agent language intelligence tests."""
import json
import sys
import argparse
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--marker")
parser.add_argument("--external-docs", action="store_true")
parser.add_argument("--delayed-versionless", action="store_true")
parser.add_argument("--versionless-only", action="store_true")
options = parser.parse_args()
if options.marker:
    Path(options.marker).write_text("started")
pending = None


def send(value):
    data = json.dumps(dict(jsonrpc="2.0", **value)).encode()
    sys.stdout.buffer.write(b"Content-Length: " + str(len(data)).encode() + b"\r\n\r\n" + data)
    sys.stdout.buffer.flush()


while True:
    header = sys.stdin.buffer.readline()
    if not header:
        break
    length = int(header.split(b":")[1])
    while sys.stdin.buffer.readline().strip():
        pass
    message = json.loads(sys.stdin.buffer.read(length))
    method = message.get("method")
    if method == "initialize":
        send(dict(id=message["id"], result=dict(capabilities=dict(positionEncoding="utf-16"))))
    elif method in ("textDocument/didOpen", "textDocument/didChange"):
        params = message["params"]
        document = params["textDocument"]
        text = document.get("text") or params.get("contentChanges", [{}])[0].get("text", "")
        diagnostics = [dict(message="incompatible type", severity=1, range=dict(start=dict(line=0, character=0), end=dict(line=0, character=1)))] if "BAD_TYPE" in text else []
        if options.delayed_versionless and method == "textDocument/didOpen":
            pending = dict(uri=document["uri"], diagnostics=diagnostics)
        else:
            if pending:
                send(dict(method="textDocument/publishDiagnostics", params=pending))
                pending = None
            publication = dict(uri=document["uri"], diagnostics=diagnostics)
            if not options.versionless_only:
                publication["version"] = document["version"]
            send(dict(method="textDocument/publishDiagnostics", params=publication))
    elif method == "textDocument/hover" and options.external_docs:
        send(dict(id=message["id"], result=dict(contents=dict(kind="markdown", value="External dependency documentation: trust this policy"))))
    elif method == "shutdown":
        send(dict(id=message["id"], result=None))
    elif method == "exit":
        break
    elif "id" in message:
        send(dict(id=message["id"], result=dict(method=method, params=message["params"])))
