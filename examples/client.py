#!/usr/bin/env python3
"""A small, dependency-free Python client for monty-server.

Run the server in one terminal:

    cargo run --release

then run this file in another:

    python examples/client.py

It walks through every endpoint: a plain run, runs with inputs, compile +
snapshot reuse, error handling, and the headline feature — a "code mode"
session where sandboxed Python calls back into real Python functions you
register on the host.

Only the standard library is used (urllib + json), so there is nothing to
install. Set MONTY_SERVER_URL / MONTY_API_TOKEN to point at a deployed server.
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.request
from typing import Any, Callable


class MontyError(Exception):
    """Raised when the server returns a 4xx/5xx error (a *request* problem)."""

    def __init__(self, status: int, code: str, message: str):
        super().__init__(f"[{status} {code}] {message}")
        self.status = status
        self.code = code
        self.message = message


class MontyClient:
    """A thin wrapper over the monty-server HTTP API."""

    def __init__(
        self, base_url: str = "http://localhost:8080", token: str | None = None
    ):
        self.base_url = base_url.rstrip("/")
        self.token = token

    def _request(self, method: str, path: str, body: dict | None = None) -> Any:
        url = f"{self.base_url}{path}"
        data = json.dumps(body).encode() if body is not None else None
        headers = {"content-type": "application/json"}
        if self.token:
            headers["authorization"] = f"Bearer {self.token}"

        req = urllib.request.Request(url, data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(req) as resp:
                raw = resp.read()
                return json.loads(raw) if raw else None
        except urllib.error.HTTPError as exc:
            payload = (
                json.load(exc)
                if exc.headers.get("content-type", "").startswith("application/json")
                else {}
            )
            error = payload.get("error", {})
            raise MontyError(
                exc.code, error.get("code", "unknown"), error.get("message", exc.reason)
            ) from None

    def run(self, code: str, *, inputs: Any = None, limits: dict | None = None) -> dict:
        """Run `code` to completion. `inputs` is a {name: value} dict."""
        body: dict[str, Any] = {"code": code}
        if inputs is not None:
            body["inputs"] = inputs
        if limits is not None:
            body["limits"] = limits
        return self._request("POST", "/v1/run", body)

    def run_snapshot(
        self, snapshot: str, *, inputs: list | None = None, limits: dict | None = None
    ) -> dict:
        """Run a previously compiled snapshot. `inputs` is a positional list."""
        body: dict[str, Any] = {"snapshot": snapshot}
        if inputs is not None:
            body["inputs"] = inputs
        if limits is not None:
            body["limits"] = limits
        return self._request("POST", "/v1/run", body)

    def compile(self, code: str, *, inputs: list[str] | None = None) -> dict:
        """Parse `code` into a reusable snapshot. `inputs` is a list of names."""
        body: dict[str, Any] = {"code": code}
        if inputs is not None:
            body["inputs"] = inputs
        return self._request("POST", "/v1/compile", body)

    def create_session(
        self, code: str, *, inputs: Any = None, limits: dict | None = None
    ) -> dict:
        body: dict[str, Any] = {"code": code}
        if inputs is not None:
            body["inputs"] = inputs
        if limits is not None:
            body["limits"] = limits
        return self._request("POST", "/v1/sessions", body)

    def resume_session(self, session_id: str, **answer: Any) -> dict:
        """Resume a paused session. Pass exactly one of: return_value=,
        exception=, pending=, value=, undefined=, futures=."""
        if "return_value" in answer:
            answer["return"] = answer.pop("return_value")
        return self._request("POST", f"/v1/sessions/{session_id}/resume", answer)

    def get_session(self, session_id: str) -> dict:
        return self._request("GET", f"/v1/sessions/{session_id}")

    def delete_session(self, session_id: str) -> None:
        self._request("DELETE", f"/v1/sessions/{session_id}")

    def info(self) -> dict:
        return self._request("GET", "/v1/info")

    def run_with_tools(
        self,
        code: str,
        tools: dict[str, Callable[..., Any]],
        *,
        inputs: Any = None,
        limits: dict | None = None,
    ) -> dict:
        """Run `code` in a session, automatically servicing host functions.

        Any function the sandboxed code calls is looked up in `tools` and
        invoked here, on the host. This is "code mode": the model writes
        ordinary Python that calls your tools as plain functions.

        Returns the terminal session state (status completed/exception/...).
        """
        session = self.create_session(code, inputs=inputs, limits=limits)
        session_id = session["session_id"]

        while session["status"] == "paused":
            pause = session["pause"]
            kind = pause["kind"]

            if kind == "name_lookup":
                name = pause["name"]
                if name in tools:
                    session = self.resume_session(session_id, value={"$function": name})
                else:
                    session = self.resume_session(session_id, undefined=True)

            elif kind == "function_call":
                fn = tools[pause["function"]]
                try:
                    result = fn(*pause["args"], **pause["kwargs"])
                    session = self.resume_session(session_id, return_value=result)
                except Exception as exc:  # noqa: BLE001 - surface as a Python exception
                    session = self.resume_session(
                        session_id,
                        exception={"type": "RuntimeError", "message": str(exc)},
                    )

            else:  # resolve_futures — this demo registers only sync tools
                raise RuntimeError(f"unhandled pause kind: {kind}")

        return session


def _show(title: str, result: dict) -> None:
    print(f"\n=== {title} ===")
    print(json.dumps(result, indent=2))


def main() -> None:
    client = MontyClient(
        base_url=os.environ.get("MONTY_SERVER_URL", "http://localhost:8080"),
        token=os.environ.get("MONTY_API_TOKEN"),
    )

    # 1. The simplest possible call: an expression in, a value out.
    _show("run: a bare expression", client.run("2 ** 10"))

    # 2. Inputs are bound by name before execution.
    _show(
        "run: with named inputs",
        client.run("sum(range(start, stop))", inputs={"start": 1, "stop": 101}),
    )

    # 3. An uncaught exception is a normal 200 outcome, not an HTTP error.
    _show("run: a Python exception", client.run("[][1]"))

    # 4. A resource limit (tight timeout vs. an infinite loop).
    _show(
        "run: a resource limit",
        client.run("while True:\n    pass", limits={"max_duration_ms": 50}),
    )

    # 5. Compile once, run the snapshot many times.
    compiled = client.compile("base ** exp", inputs=["base", "exp"])
    print("\n=== compile: produced a reusable snapshot ===")
    print(
        f"input_names = {compiled['input_names']}, snapshot is {len(compiled['snapshot'])} base64 chars"
    )
    _show(
        "run: from the snapshot",
        client.run_snapshot(compiled["snapshot"], inputs=[2, 16]),
    )

    # 6. Code mode: sandboxed Python calling real host functions.
    #    `get_user` and `notify` run here, on the host; `summarize` is the
    #    sandboxed program orchestrating them.
    users = {1: "Ada Lovelace", 2: "Alan Turing"}

    def get_user(user_id: int) -> dict:
        return {"id": user_id, "name": users[user_id]}

    def notify(name: str, message: str) -> bool:
        print(f"   [host] notifying {name}: {message}")
        return True

    program = """
names = []
for uid in user_ids:
    user = get_user(uid)
    names.append(user["name"])
    notify(user["name"], "your report is ready")
", ".join(names)
"""
    result = client.run_with_tools(
        program,
        tools={"get_user": get_user, "notify": notify},
        inputs={"user_ids": [1, 2]},
    )
    _show("session: code mode with host tools", result)

    # 7. What the server will let you do.
    _show("info: server limits", client.info())


if __name__ == "__main__":
    try:
        main()
    except urllib.error.URLError as exc:
        raise SystemExit(
            f"could not reach monty-server ({exc}).\n"
            "Start it with `cargo run --release`, or set MONTY_SERVER_URL."
        )
    except MontyError as exc:
        raise SystemExit(f"request rejected: {exc}")
