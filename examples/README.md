# Examples

## `client.py`

A dependency-free Python client (standard library only) and a runnable demo of
every endpoint.

```bash
# Terminal 1 — start the server
cargo run --release

# Terminal 2 — run the demo
python3 examples/client.py
```

Point it at a deployed server with environment variables:

```bash
MONTY_SERVER_URL=https://your-app.up.railway.app \
MONTY_API_TOKEN=sk-your-token \
python3 examples/client.py
```

The demo covers:

1. A bare expression — `2 ** 10` → `1024`.
2. Named inputs — `sum(range(start, stop))`.
3. A Python exception surfaced as a `200` outcome (not an HTTP error).
4. A resource limit — a tight loop against a 50 ms timeout.
5. `compile` once, then run the snapshot.
6. **Code mode** — sandboxed Python orchestrating real host functions
   (`get_user`, `notify`) via a session. This is the interesting one: see
   `MontyClient.run_with_tools`, which auto-services `name_lookup` and
   `function_call` pauses by calling your registered Python callables.
7. `GET /v1/info` — the server's effective limits.

`MontyClient` itself (the top of the file) is small enough to copy into your own
project as a starting point.

### The code-mode loop, in brief

```python
session = client.create_session("get_user(uid)['name']", inputs={"uid": 1})
while session["status"] == "paused":
    pause = session["pause"]
    if pause["kind"] == "name_lookup":
        session = client.resume_session(sid, value={"$function": pause["name"]})
    elif pause["kind"] == "function_call":
        result = my_tools[pause["function"]](*pause["args"], **pause["kwargs"])
        session = client.resume_session(sid, return_value=result)
# session["status"] is now "completed" / "exception" / "limit_exceeded"
```

## Other languages

The API is plain JSON over HTTP, so a client in any language is short. The
shapes to know:

- **Request**: `{"code": "...", "inputs": {...}, "limits": {...}}`
- **Response**: always `200` for a valid request, with a `status` field
  (`completed` / `exception` / `limit_exceeded` / `compile_error`).
- **Sessions**: `POST /v1/sessions` → poll `status`; while `paused`, answer
  `pause.kind` with `POST /v1/sessions/{id}/resume`.

See the [API reference](../README.md#api-reference) for the full contract.
