//! Binary entrypoint: load config, start the runtime, serve, shut down gracefully.

use std::{net::SocketAddr, process::ExitCode, time::Duration};

use agentic_web_search::{AppState, Config, api, telemetry};
use axum::serve::ListenerExt;
use mimalloc::MiMalloc;
use tokio::net::TcpListener;

/// mimalloc: lower fragmentation and faster small allocations under heavy concurrency.
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() -> ExitCode {
    // `.env` is optional; real environment variables always win.
    let _ = dotenvy::dotenv();

    // `agentic-web-search healthcheck` — used by Docker HEALTHCHECK (distroless has no curl).
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        return healthcheck();
    }

    let cfg = match Config::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    telemetry::init(&cfg.log_format);

    let mut rt = tokio::runtime::Builder::new_multi_thread();
    rt.enable_all();
    if let Some(n) = cfg.worker_threads {
        rt.worker_threads(n);
    }
    let rt = match rt.build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to start runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    match rt.block_on(run(cfg)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "fatal");
            ExitCode::FAILURE
        }
    }
}

async fn run(cfg: Config) -> Result<(), Box<dyn std::error::Error>> {
    let addr = SocketAddr::new(cfg.host, cfg.port);
    let state = AppState::new(cfg)?;

    // Evict idle rate-limiter entries so memory stays bounded with many clients.
    let gc_state = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            gc_state.limiter.retain_recent();
            gc_state.limiter.shrink_to_fit();
        }
    });

    let app = api::router(state.clone());
    let listener = TcpListener::bind(addr).await?.tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    });
    tracing::info!(
        %addr,
        auth = state.cfg.require_auth,
        browser = ?state.search.browser().status(),
        "agentic-web-search listening"
    );

    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("draining complete, shutting down browser");
    state.search.browser().shutdown().await;
    Ok(())
}

fn healthcheck() -> ExitCode {
    use std::io::{Read, Write};

    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080u16);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let probe = || -> std::io::Result<bool> {
        let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
        s.set_read_timeout(Some(Duration::from_secs(2)))?;
        s.write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
        let mut head = [0u8; 12];
        s.read_exact(&mut head)?;
        Ok(head.ends_with(b"200"))
    };
    match probe() {
        Ok(true) => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    tracing::info!("shutdown signal received");
}
