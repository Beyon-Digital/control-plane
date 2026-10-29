#!/usr/bin/env python3
"""Deterministic App Server adapter for UI previews; never executes tools or inference."""
import json
import sys
import uuid

RESPONSE = r'''**Simulator preview**

A reconnect should retain the last event cursor:

```swift
let cursor = events.last?.sequence ?? 0
print("Resume after event \(cursor)")
```

This response comes from a local test adapter.'''

def emit(value):
    print(json.dumps({"jsonrpc": "2.0", **value}), flush=True)

if "--version" in sys.argv:
    print("codex UI-fixture")
    raise SystemExit(0)

for line in sys.stdin:
    request = json.loads(line)
    method, identifier = request.get("method"), request.get("id")
    if method == "initialize":
        emit({"id": identifier, "result": {}})
    elif method in ("thread/start", "thread/resume"):
        thread = request.get("params", {}).get("threadId", "fixture-" + uuid.uuid4().hex)
        emit({"id": identifier, "result": {"thread": {"id": thread}}})
    elif method == "turn/start":
        emit({"id": identifier, "result": {"turn": {"id": "fixture-turn"}}})
        emit({"method": "item/agentMessage/delta", "params": {"delta": RESPONSE, "itemId": "fixture-message"}})
        emit({"method": "turn/completed", "params": {"turn": {"id": "fixture-turn", "status": "completed"}}})
    elif identifier is not None:
        emit({"id": identifier, "error": {"code": -32601, "message": "Unsupported UI fixture request"}})
