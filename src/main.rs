use clap::Parser;
use mcp_obsidian::config::Config;
use mcp_obsidian::server::{self, ServerContext};
use mcp_obsidian::sync::SyncSupervisor;
use mcp_obsidian::vault::Vault;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::parse();

    // Fail fast on an unusable vault rather than erroring per tool call.
    let vault = match Vault::open(&config.vault_path) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "cannot open vault");
            std::process::exit(1);
        }
    };
    let index_dir = match config.resolve_index_dir(vault.root()) {
        Ok(dir) => dir,
        Err(msg) => {
            tracing::error!(error = msg, "cannot resolve index directory");
            std::process::exit(1);
        }
    };

    // The HTTP listener is remotely reachable, so it never starts without a
    // token; there is no "open mode" to accidentally deploy.
    let auth_token = match &config.auth_token {
        Some(token) => token,
        None => {
            tracing::error!("AUTH_TOKEN is required for the HTTP listener");
            std::process::exit(1);
        }
    };

    // The sync daemon runs as a supervised child: `ob login` and
    // `ob sync-setup` happen out-of-band on the host before startup.
    let root = vault.root().to_path_buf();
    let supervisor = std::sync::Arc::new(SyncSupervisor::start(&config.ob_bin, &root));
    let ctx = ServerContext::new(vault, config.ob_bin.as_str(), supervisor.clone());

    let cancellation = CancellationToken::new();
    let router = server::router(
        ctx,
        auth_token,
        cancellation.clone(),
        config.allowed_hosts.clone(),
    );
    if config.allowed_hosts.is_none() {
        tracing::warn!(
            "ALLOWED_HOSTS is unset: inbound Host validation is disabled \
             (fine behind a reverse proxy; set it if exposing directly)"
        );
    }

    let listener = match tokio::net::TcpListener::bind(&config.bind_addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, addr = %config.bind_addr, "cannot bind");
            std::process::exit(1);
        }
    };

    tracing::info!(
        vault = %root.display(),
        index_dir = %index_dir.display(),
        bind = %config.bind_addr,
        "mcp-obsidian started"
    );

    if let Err(e) = axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        tracing::error!(error = %e, "http server failed");
        std::process::exit(1);
    }

    cancellation.cancel();
    tracing::info!("http server stopped");
    let final_state = supervisor.stop().await;
    tracing::info!(restarts = final_state.restarts, "sync supervisor stopped");
}

/// Resolves on the first shutdown signal: SIGINT (Ctrl-C) or SIGTERM
/// (systemd/Docker stop).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut sigterm =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "cannot listen for SIGTERM");
                    // `loop` types this arm as `!`, coercing to the stream type;
                    // shutdown then only ever fires via ctrl-c.
                    loop {
                        std::future::pending::<()>().await;
                    }
                }
            };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "cannot listen for ctrl-c");
            std::future::pending::<()>().await;
        }
    }
    tracing::info!("shutdown signal received");
}
