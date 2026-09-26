use std::path::{Path, PathBuf};

use clap::Parser;

/// Server configuration. Every option has an equivalent environment variable
/// so the binary can run under systemd/Docker with zero CLI arguments.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "mcp-obsidian",
    version,
    about = "MCP server for Obsidian vaults, synced via Obsidian Headless"
)]
pub struct Config {
    /// Path to the vault root directory.
    #[arg(long, env = "VAULT_PATH")]
    pub vault_path: PathBuf,

    /// Directory for the search index.
    ///
    /// Defaults to a sibling of the vault (`<vault-parent>/.mcp-obsidian-index/<vault-name>`)
    /// rather than a folder inside the vault: Obsidian Sync replicates
    /// dot-folders, and an index inside the vault would be synced to every
    /// device on every write.
    #[arg(long, env = "INDEX_DIR")]
    pub index_dir: Option<PathBuf>,

    /// HTTP listen address for the MCP streamable-HTTP endpoint.
    #[arg(long, default_value = "127.0.0.1:8080", env = "BIND_ADDR")]
    pub bind_addr: String,

    /// Bearer token required from MCP clients. Mandatory once the HTTP
    /// listener is wired up (phase 4).
    #[arg(long, env = "AUTH_TOKEN")]
    pub auth_token: Option<String>,

    /// Path to the Obsidian Headless (`ob`) binary.
    #[arg(long, default_value = "ob", env = "OB_BIN")]
    pub ob_bin: String,

    /// Daily notes folder name (vault-relative). Auto-detected when unset.
    #[arg(long, env = "DAILY_NOTES_DIR")]
    pub daily_notes_dir: Option<String>,

    /// Daily note filename format (e.g. "YYYY-MM-DD.md"). Default when unset.
    #[arg(long, env = "DAILY_NOTE_FORMAT")]
    pub daily_note_format: Option<String>,
}

impl Config {
    /// Resolved search index directory, falling back to the sibling-of-vault
    /// default documented on [`Config::index_dir`].
    ///
    /// Returns an error if no fallback exists, which only happens when the
    /// vault sits at the filesystem root; such setups must set INDEX_DIR.
    pub fn resolve_index_dir(&self, vault_root: &Path) -> Result<PathBuf, &'static str> {
        if let Some(dir) = &self.index_dir {
            return Ok(dir.clone());
        }
        let vault_name = vault_root
            .file_name()
            .ok_or("vault is at the filesystem root; set INDEX_DIR explicitly")?;
        let parent = vault_root
            .parent()
            .ok_or("vault is at the filesystem root; set INDEX_DIR explicitly")?;
        Ok(parent.join(".mcp-obsidian-index").join(vault_name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_dir_defaults_to_sibling_of_vault() {
        let config = Config {
            vault_path: PathBuf::from("/srv/vaults/Knowledge"),
            index_dir: None,
            bind_addr: String::new(),
            auth_token: None,
            ob_bin: String::new(),
            daily_notes_dir: None,
            daily_note_format: None,
        };
        let resolved = config
            .resolve_index_dir(Path::new("/srv/vaults/Knowledge"))
            .expect("sibling fallback should resolve");
        assert_eq!(
            resolved,
            Path::new("/srv/vaults/.mcp-obsidian-index/Knowledge")
        );
    }

    #[test]
    fn index_dir_explicit_overrides_default() {
        let config = Config {
            vault_path: PathBuf::new(),
            index_dir: Some(PathBuf::from("/var/lib/mcp-obsidian/index")),
            bind_addr: String::new(),
            auth_token: None,
            ob_bin: String::new(),
            daily_notes_dir: None,
            daily_note_format: None,
        };
        let resolved = config
            .resolve_index_dir(Path::new("/srv/vaults/Knowledge"))
            .expect("explicit dir should be used");
        assert_eq!(resolved, Path::new("/var/lib/mcp-obsidian/index"));
    }
}
