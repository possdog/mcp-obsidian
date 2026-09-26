use clap::Parser;
use mcp_obsidian::config::Config;
use mcp_obsidian::sync::SyncSupervisor;
use mcp_obsidian::vault::Vault;

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

    // The sync daemon runs as a supervised child: `ob login` and
    // `ob sync-setup` happen out-of-band on the host before startup.
    let supervisor = SyncSupervisor::start(&config.ob_bin, vault.root());

    tracing::info!(
        vault = %vault.root().display(),
        index_dir = %index_dir.display(),
        bind = %config.bind_addr,
        "mcp-obsidian started (phase 3: sync supervision; MCP handler arrives in phase 4)"
    );

    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::error!(error = %e, "failed to listen for ctrl-c");
        std::process::exit(1);
    }
    tracing::info!("ctrl-c received; shutting down");
    supervisor.stop().await;
}
