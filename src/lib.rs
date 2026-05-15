//! `monty-server` — a production-ready HTTP API for the
//! [monty](https://github.com/pydantic/monty) sandboxed Python interpreter.
//!
//! The crate is split so each module has one clear job:
//!
//! * [`config`] — environment-driven configuration, loaded once at startup.
//! * [`state`] — the `Arc`-backed state shared across handlers.
//! * [`auth`] — bearer-token authentication and per-caller rate limiting.
//! * [`rate_limit`] — the token-bucket limiter itself.
//! * [`tracker`] — resource enforcement and stats for a single execution.
//! * [`limits`] — turning client-requested limits into enforced ones.
//! * [`convert`] — JSON ⇄ monty value and exception conversions.
//! * [`engine`] — the synchronous bridge to monty (`run`, `start`, `resume`).
//! * [`session`] — the in-memory store for iterative execution sessions.
//! * [`api`] — HTTP routing, middleware wiring, and request/response types.
//! * [`error`] — protocol-level (`4xx`/`5xx`) errors.
//!
//! The binary entry point in `main.rs` only loads config, builds [`AppState`],
//! starts the background reaper, and serves [`build_router`].

pub mod api;
pub mod auth;
pub mod config;
pub mod convert;
pub mod engine;
pub mod error;
pub mod limits;
pub mod rate_limit;
pub mod session;
pub mod state;
pub mod tracker;

pub use api::{REAPER_INTERVAL, build_router};
pub use config::{Config, ConfigError};
pub use state::AppState;
