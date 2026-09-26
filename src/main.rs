use clap::Parser;
use mcp_obsidian::config::Config;
use mcp_obsidian::vault::Vault;

fn main() {
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

    // Phase 1 scope: configuration and the sandboxed vault layer. The MCP
    // handler, sync supervisor, and search index arrive in phases 2-5.
    tracing::info!(
        vault = %vault.root().display(),
        index_dir = %index_dir.display(),
        bind = %config.bind_addr,
        "mcp-obsidian started (phase 1: vault layer only)"
    );
}
