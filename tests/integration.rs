//! Black-box HTTP integration tests.
//!
//! Each test starts a real server on an ephemeral port with a hand-built
//! [`Config`] (so tests do not race on process environment) and drives it over
//! HTTP with `reqwest`, exercising the full middleware + handler + engine stack.

use std::net::SocketAddr;
use std::time::Duration;

use monty_server::config::{Auth, LimitBounds, RateRule, SessionConfig};
use monty_server::state::AppState;
use monty_server::{Config, build_router};
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// A permissive configuration suitable for tests: no auth, generous rate limit,
/// small-but-real execution limits.
fn test_config() -> Config {
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        auth: Auth::Disabled,
        default_rate: RateRule {
            per_minute: 100_000,
            burst: 100_000,
        },
        max_concurrency: 4,
        max_body_bytes: 1 << 20,
        request_timeout: Duration::from_secs(30),
        limits: LimitBounds {
            default_duration: Duration::from_secs(2),
            max_duration: Duration::from_secs(5),
            default_memory_bytes: 64 << 20,
            max_memory_bytes: 128 << 20,
            default_max_allocations: 5_000_000,
            max_allocations: 10_000_000,
            default_recursion_depth: 1_000,
            max_recursion_depth: 2_000,
        },
        sessions: SessionConfig {
            default_ttl: Duration::from_secs(60),
            max_ttl: Duration::from_secs(300),
            max_sessions: 100,
        },
        cors_origins: Vec::new(),
    }
}

/// Starts the server in the background and returns its base URL and a client.
async fn spawn() -> (String, reqwest::Client) {
    let state = AppState::new(test_config());
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    (format!("http://{addr}"), reqwest::Client::new())
}

/// POSTs `body` to `path` and returns the status code and parsed JSON body.
async fn post(client: &reqwest::Client, url: &str, body: Value) -> (reqwest::StatusCode, Value) {
    let response = client.post(url).json(&body).send().await.unwrap();
    let status = response.status();
    let json = response.json().await.unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn health_is_ok() {
    let (base, client) = spawn().await;
    let response = client.get(format!("{base}/health")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({ "status": "ok" })
    );
}

#[tokio::test]
async fn run_returns_result_with_inputs() {
    let (base, client) = spawn().await;
    let (status, body) = post(
        &client,
        &format!("{base}/v1/run"),
        json!({ "code": "x + 1", "inputs": { "x": 41 } }),
    )
    .await;

    assert_eq!(status, 200);
    assert_eq!(body["status"], "completed");
    assert_eq!(body["result"], 42);
}

#[tokio::test]
async fn run_captures_stdout() {
    let (base, client) = spawn().await;
    let (status, body) = post(
        &client,
        &format!("{base}/v1/run"),
        json!({ "code": "print('hello world')\nNone" }),
    )
    .await;

    assert_eq!(status, 200);
    assert_eq!(body["status"], "completed");
    assert!(
        body["stdout"].as_str().unwrap().contains("hello world"),
        "stdout was {:?}",
        body["stdout"]
    );
}

#[tokio::test]
async fn run_reports_python_exception() {
    let (base, client) = spawn().await;
    let (status, body) = post(
        &client,
        &format!("{base}/v1/run"),
        json!({ "code": "raise ValueError('boom')" }),
    )
    .await;

    assert_eq!(status, 200);
    assert_eq!(body["status"], "exception");
    assert_eq!(body["exception"]["type"], "ValueError");
    assert_eq!(body["exception"]["message"], "boom");
}

#[tokio::test]
async fn run_reports_compile_error() {
    let (base, client) = spawn().await;
    let (status, body) = post(&client, &format!("{base}/v1/run"), json!({ "code": "x +" })).await;

    assert_eq!(status, 200);
    assert_eq!(body["status"], "compile_error");
    assert!(body["exception"].is_object());
}

#[tokio::test]
async fn run_enforces_time_limit() {
    let (base, client) = spawn().await;
    let (status, body) = post(
        &client,
        &format!("{base}/v1/run"),
        json!({ "code": "while True:\n    pass", "limits": { "max_duration_ms": 50 } }),
    )
    .await;

    assert_eq!(status, 200);
    assert_eq!(body["status"], "limit_exceeded");
    assert_eq!(body["limit"], "time");
}

#[tokio::test]
async fn run_rejects_array_inputs_for_code() {
    let (base, client) = spawn().await;
    let (status, body) = post(
        &client,
        &format!("{base}/v1/run"),
        json!({ "code": "x", "inputs": [1, 2] }),
    )
    .await;

    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "bad_request");
}

#[tokio::test]
async fn compile_then_run_from_snapshot() {
    let (base, client) = spawn().await;

    let (status, compiled) = post(
        &client,
        &format!("{base}/v1/compile"),
        json!({ "code": "a * b", "inputs": ["a", "b"] }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(compiled["status"], "compiled");
    let snapshot = compiled["snapshot"].as_str().unwrap().to_owned();

    let (status, body) = post(
        &client,
        &format!("{base}/v1/run"),
        json!({ "snapshot": snapshot, "inputs": [6, 7] }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "completed");
    assert_eq!(body["result"], 42);
}

#[tokio::test]
async fn session_drives_host_function_to_completion() {
    let (base, client) = spawn().await;

    // `add` is undefined, so the program pauses for the host to supply it.
    let (status, mut body) = post(
        &client,
        &format!("{base}/v1/sessions"),
        json!({ "code": "add(2, 3)" }),
    )
    .await;
    assert_eq!(status, 201);
    let session_id = body["session_id"].as_str().unwrap().to_owned();
    let resume_url = format!("{base}/v1/sessions/{session_id}/resume");

    // Drive the session: resolve the `add` name to a host function, then answer
    // the resulting function call. A real client's host loop looks just like this.
    for _ in 0..5 {
        match body["status"].as_str().unwrap() {
            "paused" => {
                let answer = match body["pause"]["kind"].as_str().unwrap() {
                    "name_lookup" => {
                        assert_eq!(body["pause"]["name"], "add");
                        json!({ "value": { "$function": "add" } })
                    }
                    "function_call" => {
                        assert_eq!(body["pause"]["function"], "add");
                        assert_eq!(body["pause"]["args"], json!([2, 3]));
                        json!({ "return": 5 })
                    }
                    other => panic!("unexpected pause kind: {other}"),
                };
                (_, body) = post(&client, &resume_url, answer).await;
            }
            "completed" => {
                assert_eq!(body["result"], 5);
                return;
            }
            other => panic!("unexpected session status: {other} ({body})"),
        }
    }
    panic!("session did not complete within the expected number of steps");
}

#[tokio::test]
async fn session_not_found_is_404() {
    let (base, client) = spawn().await;
    let missing = "00000000-0000-4000-8000-000000000000";
    let response = client
        .get(format!("{base}/v1/sessions/{missing}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
}
