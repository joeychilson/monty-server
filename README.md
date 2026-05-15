# monty-server

monty-server is an HTTP API in front of [**monty**](https://github.com/pydantic/monty),
Pydantic's sandboxed Python interpreter written in Rust.

monty runs an LLM-friendly subset of Python with **no filesystem, environment, or network
access**, microsecond startup times, and hard limits on memory, allocations, and execution
time. `monty-server` wraps that engine in a small, well-defined HTTP API so you can run
untrusted Python from any language — Python, JavaScript, Go, anything that speaks HTTP.

```bash
curl -s localhost:8080/v1/run \
  -H 'content-type: application/json' \
  -d '{"code": "x * 2 + 1", "inputs": {"x": 20}}'
# {"status":"completed","result":41,"stdout":"","stderr":"","stats":{...}}
```

[![Deploy on Railway](https://railway.com/button.svg)](https://railway.com/deploy/monty-server?referralCode=NhCCIt&utm_medium=integration&utm_source=template&utm_campaign=generic)
    
## Contents

- [Why](#why)
- [Quick start](#quick-start)
- [API reference](#api-reference)
  - [`POST /v1/run`](#post-v1run)
  - [`POST /v1/compile`](#post-v1compile)
  - [Sessions — `/v1/sessions`](#sessions--v1sessions)
  - [Metadata endpoints](#metadata-endpoints)
- [The value format](#the-value-format)
- [Errors](#errors)
- [Configuration](#configuration)
- [Security model](#security-model)
- [Deployment](#deployment)
- [Architecture](#architecture)
- [Limitations](#limitations)

## Why

Running model-generated code safely usually means containers, microVMs, or a remote
sandbox service — all heavy. monty is a Rust library with a microsecond cold start and a
strict sandbox baked in. `monty-server` keeps that lightness while giving you:

- **One simple HTTP surface** — `run` for fire-and-forget execution, `sessions` for
  "code mode" where the program calls *your* functions, `compile` for snapshot caching.
- **Predictable outcomes** — a well-formed request always returns `200` with a
  `status` field describing what the *code* did. `4xx`/`5xx` are reserved for problems
  with the *request*.
- **Production hardening** — bounded concurrency, per-token rate limiting, request
  timeouts and body limits, graceful shutdown, structured logging, a non-root container.

## Quick start

Requires Rust 1.95+ (pinned in `rust-toolchain.toml`).

```bash
cargo run --release
# monty-server listening on 0.0.0.0:8080 (auth: disabled)
```

With no `MONTY_API_TOKENS` set, the server runs **open** and rate limits per client IP —
fine for local use. For anything exposed, set tokens (see [Security model](#security-model)).

```bash
# Run code to completion
curl -s localhost:8080/v1/run -H 'content-type: application/json' \
  -d '{"code": "sum(range(n))", "inputs": {"n": 100}}'

# Discover the server's effective limits
curl -s localhost:8080/v1/info
```

A complete, dependency-free Python client and a demo of every endpoint —
including code mode — lives in [`examples/client.py`](examples/client.py):

```bash
python3 examples/client.py
```

## API reference

All request and response bodies are JSON. `Content-Type: application/json` is required on
`POST`s. When authentication is enabled, send `Authorization: Bearer <token>`.

### `POST /v1/run`

Compile (or load) a program and run it to completion. This is the stateless path: one
request, one result. Host functions are **not** available here — code that calls an
undefined function raises `NameError`; use [sessions](#sessions--v1sessions) for that.

**Request**

| Field      | Type              | Notes |
|------------|-------------------|-------|
| `code`     | string            | Python source. Provide this **or** `snapshot`. |
| `snapshot` | string (base64)   | A snapshot from `/v1/compile`. Provide this **or** `code`. |
| `filename` | string            | Script name shown in tracebacks. Default `main.py`. |
| `inputs`   | object or array   | With `code`: an object of `name → value`. With `snapshot`: a positional array. See [The value format](#the-value-format). |
| `limits`   | object            | Optional per-execution limits, clamped to the server maximum. |

`limits` fields: `max_duration_ms`, `max_memory_bytes`, `max_allocations`,
`max_recursion_depth` — all optional, all clamped.

**Response** — always `200` for a well-formed request, with a `status`:

```jsonc
// status: "completed"
{ "status": "completed", "result": 41, "stdout": "", "stderr": "",
  "stats": { "duration_ms": 0.04, "allocations": 3, "peak_memory_bytes": 512 } }

// status: "exception" — the code raised
{ "status": "exception",
  "exception": { "type": "ValueError", "message": "bad", "summary": "ValueError: bad",
                 "traceback": [ ... ], "traceback_text": "Traceback (most recent call last): ..." },
  "stdout": "", "stderr": "", "stats": { ... } }

// status: "limit_exceeded" — a resource limit tripped ("time" | "memory" | "allocations" | "recursion")
{ "status": "limit_exceeded", "limit": "time", "exception": { ... }, "stdout": "...", "stats": { ... } }

// status: "compile_error" — the source did not parse
{ "status": "compile_error", "exception": { ... } }
```

### `POST /v1/compile`

Parse Python source into a reusable **snapshot** — monty's compiled program state.
Compile once, then run the snapshot many times to skip re-parsing, or use it to validate
code without executing it.

**Request**: `code` (string, required), `filename` (string), `inputs` (array of input
*names* — values are supplied later by `/v1/run`).

**Response**:

```jsonc
{ "status": "compiled", "snapshot": "<base64>", "input_names": ["a", "b"] }
// or
{ "status": "compile_error", "exception": { ... } }
```

### Sessions — `/v1/sessions`

Sessions implement **iterative ("code mode") execution**: the program runs until it needs
something from the host — a function call, a name, an async result — then *pauses*. You
answer the pause with a `resume`, and it runs to the next one. This is what lets an LLM
write Python that calls your tools as ordinary functions.

A session is owned by the token that created it, allows one in-flight `resume` at a time,
and is evicted after its TTL.

#### `POST /v1/sessions` → `201 Created`

Same body as `/v1/run`, plus an optional `ttl_seconds` (clamped to the server max). Starts
the program; it runs until it finishes or pauses.

```jsonc
{ "session_id": "f1e2...", "expires_in_seconds": 300,
  "status": "paused",
  "pause": { "kind": "function_call", "call_id": 1, "function": "get_weather",
             "method_call": false, "args": ["London"], "kwargs": {} },
  "stdout": "", "stderr": "", "stats": { ... } }
```

A session's `status` is `paused`, `completed`, `exception`, or `limit_exceeded` — the last
three are terminal and shaped exactly like the `/v1/run` outcomes. A `compile_error`
returns `200` with no session created.

**Pause kinds:**

| `kind`            | Meaning | Answer it with |
|-------------------|---------|----------------|
| `function_call`   | The program called a host function. | `return`, `exception`, or `pending` |
| `name_lookup`     | The program referenced an undefined name. | `value` or `undefined` |
| `resolve_futures` | All async tasks are blocked on futures. | `futures` |

> **Host functions are two steps.** The first time a name like `get_weather` is used, the
> session pauses on `name_lookup`. Answer it with `{"value": {"$function": "get_weather"}}`
> to tell monty the name is a callable; the program then pauses on `function_call` with the
> actual arguments.

#### `POST /v1/sessions/{id}/resume`

Set **exactly one** field, matching the current pause kind:

| Field        | For pause kind | Meaning |
|--------------|----------------|---------|
| `return`     | `function_call` | The function's return value (any JSON value). |
| `exception`  | `function_call` | Raise instead: `{"type": "ValueError", "message": "..."}`. |
| `pending`    | `function_call` | `true` — the function is an async coroutine. |
| `value`      | `name_lookup`   | The resolved value for the name. |
| `undefined`  | `name_lookup`   | `true` — the name is undefined (raises `NameError`). |
| `futures`    | `resolve_futures` | `[{"call_id": 1, "return": ...}, {"call_id": 2, "exception": {...}}]` |

Returns the session's new state — same shape as create. A resume whose shape does not
match the pause is rejected `400` and leaves the session untouched.

#### `GET /v1/sessions/{id}`

Returns the current session state without resuming it.

#### `DELETE /v1/sessions/{id}` → `204 No Content`

Discards the session.

#### Worked example

```bash
# 1. Start a program that calls a host function `add`.
curl -s localhost:8080/v1/sessions -H 'content-type: application/json' \
  -d '{"code": "add(2, 3) * 10"}'
# -> {"session_id":"ABC","status":"paused","pause":{"kind":"name_lookup","name":"add"}, ...}

# 2. Tell monty `add` is a host function.
curl -s localhost:8080/v1/sessions/ABC/resume -H 'content-type: application/json' \
  -d '{"value": {"$function": "add"}}'
# -> {"status":"paused","pause":{"kind":"function_call","function":"add","args":[2,3]}, ...}

# 3. Answer the call.
curl -s localhost:8080/v1/sessions/ABC/resume -H 'content-type: application/json' \
  -d '{"return": 5}'
# -> {"status":"completed","result":50, ...}
```

### Metadata endpoints

| Endpoint      | Auth | Purpose |
|---------------|------|---------|
| `GET /`       | no   | Service banner and endpoint list. |
| `GET /health` | no   | Liveness probe (`{"status":"ok"}`). |
| `GET /v1/info`| yes  | Effective limits, auth mode, live session count, uptime. |

## The value format

Inputs and results use monty's **natural-form JSON**: JSON-native Python values map
directly, and a few non-native types use a single-key `{"$tag": ...}` object.

| JSON                              | Python |
|-----------------------------------|--------|
| `null`, `true`, `42`, `"s"`, `[...]`, `{...}` | `None`, `bool`, `int`, `str`, `list`, `dict` |
| `1.5`                             | `float` |
| `{"$tuple": [...]}`               | `tuple` |
| `{"$set": [...]}` / `{"$frozenset": [...]}` | `set` / `frozenset` |
| `{"$bytes": "base64"}` or `[ints]`| `bytes` |
| `{"$ellipsis": "..."}`            | `...` |
| `{"$float": "nan"\|"inf"\|"-inf"}`| non-finite `float` |
| `{"$dict": [[k, v], ...]}`        | `dict` with non-string keys |
| `{"$function": "name"}`           | a host function (to answer a `name_lookup`) |
| `{"$path": "p"}`                  | `pathlib.Path` |

Results can additionally contain `$exception`, `$dataclass`, `$namedtuple`, `$type`, and
date/time objects — see monty's docs. **Integer inputs are limited to the signed 64-bit
range**; larger integers are rejected rather than silently truncated.

## Errors

A `200` response means the server *ran your code* — inspect `status` for what happened.
A non-`2xx` response means a problem with the *request* and always has this shape:

```json
{ "error": { "code": "rate_limited", "message": "rate limit exceeded" } }
```

| Status | `code`              | Cause |
|--------|---------------------|-------|
| 400    | `bad_request`       | Malformed body, bad inputs, mismatched resume. |
| 401    | `unauthorized`      | Missing/invalid bearer token (when auth is enabled). |
| 404    | `session_not_found` | Unknown or expired session, or not owned by you. |
| 409    | `session_busy`      | A `resume` is already in flight for this session. |
| 429    | `rate_limited`      | Rate limit exceeded; see the `Retry-After` header. |
| 503    | `unavailable`       | Session capacity reached. |
| 408    | —                   | Request exceeded the server request timeout. |

## Configuration

All configuration is via environment variables; every one has a safe default. See
[`.env.example`](.env.example) for the full annotated list. The most important:

| Variable               | Default      | Purpose |
|------------------------|--------------|---------|
| `PORT`                 | `8080`       | Listen port (injected by Railway). |
| `MONTY_API_TOKENS`     | *(unset)*    | `name:token:rpm` entries, comma-separated. Unset ⇒ auth disabled. |
| `MONTY_RATE_LIMIT_RPM` | `120`        | Default per-caller rate (requests/minute). |
| `MONTY_MAX_CONCURRENCY`| CPU count    | Max simultaneous executions. |
| `MONTY_MAX_BODY_BYTES` | `1048576`    | Max request body size. |
| `MONTY_MAX_TIMEOUT_MS` | `30000`      | Ceiling for client-requested execution time. |
| `MONTY_MAX_MEMORY_BYTES`| `268435456` | Ceiling for client-requested memory. |
| `MONTY_SESSION_TTL_SECONDS` | `300`   | Idle session lifetime. |
| `MONTY_MAX_SESSIONS`   | `1000`       | Max live sessions. |
| `RUST_LOG`             | `info`       | Log verbosity. |

`GET /v1/info` reports the effective values at runtime.

## Security model

monty itself is the sandbox: the interpreted code **cannot** touch the filesystem,
environment, or network, and runs under hard memory/allocation/time limits. `monty-server`
adds the perimeter around it:

- **Authentication** — set `MONTY_API_TOKENS` and every `/v1` route requires a bearer
  token. Tokens are matched in constant configuration; each can carry its own rate limit.
  With no tokens set the server is *open* and falls back to per-IP rate limiting — only
  appropriate for trusted networks or local development.
- **Rate limiting** — a per-caller token bucket (per token, or per IP when open).
  Exhausted callers get `429` with `Retry-After`.
- **Bounded concurrency** — a semaphore caps simultaneous executions at
  `MONTY_MAX_CONCURRENCY`, so CPU and memory cannot be exhausted by request volume.
  monty runs on a blocking thread pool, never on the async runtime.
- **Always-on execution limits** — every execution runs under a resource tracker; client
  limits can only be *lower* than the server ceiling, never higher. There is no
  "unlimited" mode.
- **Request hardening** — body size limit, overall request timeout, panic catching, and a
  background reaper that evicts expired sessions and idle rate-limit state.
- **OS calls are denied** — if interpreted code attempts a filesystem/OS operation, the
  server answers it with `PermissionError` rather than exposing host resources.
- **Container** — the Docker image is `distroless/cc` and runs as a non-root user.

Behind a proxy (Railway, Cloudflare, nginx) the client IP is read from the first
`X-Forwarded-For` hop. Terminate TLS at the proxy.

## Deployment

### Prebuilt image

Each `v*` release tag is built and published to the GitHub Container Registry by
[`.github/workflows/docker.yml`](.github/workflows/docker.yml). Pushes to `main` do **not**
build an image — cut a release tag when you want a new one:

```bash
git tag v0.1.0 && git push origin v0.1.0
```

```bash
docker run -p 8080:8080 \
  -e MONTY_API_TOKENS="prod:sk-your-secret-token:600" \
  ghcr.io/<owner>/<repo>:latest
```

Tags published: the semver version (`0.1.0`, `0.1`), the commit SHA, and `latest` (the
most recent release). Pull requests build the image to catch Dockerfile breakage but do
not push.

### Docker (local build)

```bash
docker build -t monty-server .
docker run -p 8080:8080 \
  -e MONTY_API_TOKENS="prod:sk-your-secret-token:600" \
  monty-server
```

The multi-stage build compiles against Rust 1.95 and ships a `distroless/cc` runtime image
(~30 MB) running as non-root.

## Architecture

```
HTTP ─▶ trace ─▶ catch-panic ─▶ CORS ─▶ body-limit ─▶ timeout ─▶ [/v1: auth ─▶ rate-limit] ─▶ handler
                                                                                                 │
                                                                  spawn_blocking + semaphore ◀───┘
                                                                                                 │
                                                                                          monty engine
```

Source layout (each module has one job; see `src/lib.rs`):

| Module        | Responsibility |
|---------------|----------------|
| `config`      | Environment-driven configuration, loaded once. |
| `state`       | `Arc`-backed state shared across handlers. |
| `auth`        | Bearer-token authentication + rate-limit middleware. |
| `rate_limit`  | The token-bucket limiter. |
| `tracker`     | Thread-safe resource enforcement + stats for one execution. |
| `limits`      | Clamping client-requested limits to server bounds. |
| `convert`     | JSON ⇄ monty value/exception conversions. |
| `engine`      | The synchronous bridge to monty (`run` / `start` / `resume`). |
| `session`     | In-memory store for iterative-execution sessions. |
| `api`         | Routing, middleware wiring, request/response types. |
| `error`       | Protocol-level (`4xx`/`5xx`) errors. |

Why a custom resource tracker (`StatsTracker`) instead of `monty::LimitedTracker`: monty's
tracker is `!Sync` and its counters cannot be read after it is consumed. The server needs
to move tracker-bearing values across threads and report stats after a run finishes, so
`StatsTracker` is an `Arc` of atomic counters with the same enforcement logic.

## Limitations

- **Sessions are process-local.** The store is in-memory; horizontal scaling needs sticky
  routing (route a session's requests to the instance that created it). This is a
  deliberate trade — no database dependency for a feature most deployments run single-node.
- **Integer inputs are `i64`-bounded.** monty's public API exposes no way to construct an
  arbitrary-precision integer, so larger integer *inputs* are rejected. Results can still
  contain big integers.
- **monty's Python subset.** No class definitions, no `match` statements, no third-party
  libraries, and a reduced standard library. See the [monty README](https://github.com/pydantic/monty).

## License

MIT, matching monty.
