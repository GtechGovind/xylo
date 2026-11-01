"""Deterministic JSON-RPC fixture for agent language intelligence tests."""
import json
import sys


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
        send(dict(method="textDocument/publishDiagnostics", params=dict(uri=document["uri"], version=document["version"], diagnostics=diagnostics)))
    elif method == "shutdown":
        send(dict(id=message["id"], result=None))
    elif method == "exit":
        break
    elif "id" in message:
        send(dict(id=message["id"], result=dict(method=method, params=message["params"])))
