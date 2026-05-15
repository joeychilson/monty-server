//! Binary entry point: load configuration, build shared state, start the
//! background reaper, and serve the API until a shutdown signal arrives.

use std::net::SocketAddr;
use std::process::ExitCode;

use monty_server::api::REAPER_INTERVAL;
use monty_server::state::AppState;
use monty_server::{Config, build_router};
use tokio::net::TcpListener;
use tokio::signal;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(%error, "failed to load configuration");
            return ExitCode::FAILURE;
        }
    };

    let bind = config.bind;
    let auth_mode = config.auth.describe();
    let state = AppState::new(config);

    spawn_reaper(state.clone());

    let app = build_router(state.clone());
    let listener = match TcpListener::bind(bind).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(%error, %bind, "failed to bind listener");
            return ExitCode::FAILURE;
        }
    };

    tracing::info!(%bind, auth = auth_mode, "monty-server listening");

    // `ConnectInfo` is required so the auth layer can read the peer address.
    let service = app.into_make_service_with_connect_info::<SocketAddr>();
    if let Err(error) = axum::serve(listener, service)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        tracing::error!(%error, "server error");
        return ExitCode::FAILURE;
    }

    tracing::info!("monty-server stopped");
    ExitCode::SUCCESS
}

/// Initializes structured logging. `RUST_LOG` controls verbosity (default
/// `info`); set `MONTY_LOG_FORMAT=json` for machine-readable output.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let json = std::env::var("MONTY_LOG_FORMAT").is_ok_and(|v| v.eq_ignore_ascii_case("json"));

    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if json {
        builder.json().init();
    } else {
        builder.init();
    }
}

/// Periodically evicts expired sessions and prunes idle rate-limit buckets, so
/// abandoned clients cannot leak memory.
fn spawn_reaper(state: AppState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(REAPER_INTERVAL);
        // The first tick fires immediately; skip it so we do not sweep an empty
        // store the instant the server starts.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let reaped = state.sessions.reap_expired();
            state.rate_limiter.prune_idle();
            if reaped > 0 {
                tracing::debug!(reaped, "evicted expired sessions");
            }
        }
    });
}

/// Resolves when the process receives `Ctrl-C` or (on Unix) `SIGTERM` — the
/// signal Railway and most orchestrators send to ask for a clean shutdown.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(error) => tracing::error!(%error, "failed to install SIGTERM handler"),
        }
    };

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }

    tracing::info!("shutdown signal received");
}
