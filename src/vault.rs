//! Sandboxed access to the vault directory.
//!
//! Every filesystem operation the MCP tools perform goes through [`Vault`].
//! Its job is to make it impossible for a client to touch anything outside
//! the vault: traversal sequences, absolute paths, and symlinks are rejected
//! during path resolution, and dot-folders (`.obsidian`, `.trash`, `.git`)
//! are invisible to listing and rejected for direct access.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use thiserror::Error;

/// Name of the vault's trash folder, mirroring Obsidian's own behavior:
/// deleted notes are moved here rather than unlinked, so a client mistake
/// is always recoverable (and before `ob` syncs, sync version history
/// provides a second safety net).
pub const TRASH_DIR: &str = ".trash";

#[derive(Debug, Error)]
pub enum VaultError {
    /// The requested path is malformed or attempts to escape the vault.
    #[error("invalid vault path: {0}")]
    InvalidPath(String),
    /// The path resolves somewhere the tools must not touch (dot-folders).
    #[error("path is not accessible: {0}")]
    Forbidden(String),
    #[error("note not found: {0}")]
    NotFound(String),
    #[error("path already exists: {0}")]
    Exists(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, VaultError>;

/// A single note returned by [`Vault::list`].
#[derive(Debug, Clone)]
pub struct NoteEntry {
    /// Vault-relative path, always using `/` separators.
    pub path: String,
    pub size: u64,
    pub modified: SystemTime,
}

/// A vault opened at a canonical root directory.
#[derive(Debug, Clone)]
pub struct Vault {
    root: PathBuf,
}

impl Vault {
    /// Opens the vault, canonicalizing the root. The root must already exist
    /// and be a directory; the server never creates vaults (that is
    /// `ob sync-setup`'s job).
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        if !root.is_dir() {
            return Err(VaultError::InvalidPath(format!(
                "vault path is not a directory: {}",
                root.display()
            )));
        }
        let canonical = std::fs::canonicalize(root)?;
        Ok(Self { root: canonical })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves a vault-relative path into an absolute one, refusing
    /// anything that could leave the vault.
    ///
    /// `must_exist` controls the last-component check: reads want the file
    /// to exist, writes want the target to be free (or to overwrite it).
    ///
    /// The rules, in order:
    /// 1. Empty paths, NUL bytes, absolute paths, and `\` separators are
    ///    rejected outright (Obsidian uses `/`; on Unix `\` in a filename
    ///    is legal and would only cause confusion).
    /// 2. Components are normalized lexically: `.` and empty segments are
    ///    dropped, and *any* `..` is rejected rather than resolved — a
    ///    client has no legitimate reason to traverse upward inside a vault.
    /// 3. Dot-prefixed components (`.obsidian`, `.trash`, `.git`, …) are
    ///    rejected everywhere, matching Obsidian's treatment of dot-folders
    ///    as hidden and keeping tools away from configuration and trash.
    /// 4. Every component on the path is checked with `symlink_metadata`
    ///    before being appended, so a symlink planted anywhere inside the
    ///    vault cannot redirect a resolved path outside the root. `..` was
    ///    already rejected, so the walk can never leave the vault even when
    ///    intermediate directories do not exist yet (write case).
    fn resolve(&self, rel: &str, must_exist: bool) -> Result<PathBuf> {
        if rel.is_empty() {
            return Err(VaultError::InvalidPath("path is empty".into()));
        }
        if rel.contains('\0') {
            return Err(VaultError::InvalidPath("path contains NUL".into()));
        }
        if rel.contains('\\') || Path::new(rel).is_absolute() {
            return Err(VaultError::InvalidPath(format!(
                "path must be vault-relative with / separators: {rel}"
            )));
        }

        let mut normalized: Vec<&str> = Vec::new();
        for segment in rel.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    return Err(VaultError::InvalidPath(format!(
                        "parent segments are not allowed: {rel}"
                    )));
                }
                s if s.starts_with('.') => {
                    return Err(VaultError::Forbidden(format!(
                        "dot-folders are not accessible: {rel}"
                    )));
                }
                s => normalized.push(s),
            }
        }
        if normalized.is_empty() {
            return Err(VaultError::InvalidPath(format!(
                "path has no segments: {rel}"
            )));
        }

        let mut resolved = self.root.clone();
        for (idx, segment) in normalized.iter().enumerate() {
            resolved.push(segment);
            // symlink_metadata does not follow the link, so this detects
            // symlinks without ever reading through them. Missing entries
            // are fine only when the caller does not require existence
            // (writes into fresh folders).
            match fs::symlink_metadata(&resolved) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(VaultError::Forbidden(format!(
                        "symlinks are not accessible: {rel}"
                    )));
                }
                Ok(_) => {}
                // A non-directory in the middle of the path (e.g. writing
                // "note.md/child") is a path-shape error, not an IO failure.
                Err(e) if e.kind() == io::ErrorKind::NotADirectory => {
                    return Err(VaultError::InvalidPath(format!(
                        "a path segment is not a directory: {rel}"
                    )));
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    if must_exist {
                        return Err(VaultError::NotFound(rel.to_string()));
                    }
                    // The first missing component means nothing deeper can
                    // exist either (no symlink chain to worry about), so the
                    // remaining segments are appended verbatim.
                    for rest in &normalized[idx + 1..] {
                        resolved.push(rest);
                    }
                    return Ok(resolved);
                }
                Err(e) => return Err(VaultError::Io(e)),
            }
        }

        if must_exist && !resolved.exists() {
            return Err(VaultError::NotFound(rel.to_string()));
        }
        Ok(resolved)
    }

    /// Lists notes in the vault, optionally restricted to a path prefix.
    ///
    /// Hidden entries (anything starting with `.`) are skipped at every
    /// depth, so `.obsidian`, `.trash`, and friends never show up. Symlinks
    /// are skipped for the same reason `resolve` rejects them.
    pub fn list(&self, prefix: Option<&str>) -> Result<Vec<NoteEntry>> {
        let prefix = prefix
            .map(|p| p.trim_start_matches('/'))
            .filter(|p| !p.is_empty());
        if let Some(p) = prefix {
            // Validate through the same sandbox rules so a prefix cannot
            // probe hidden folders; the prefix need not exist (empty result).
            self.resolve(p, false)?;
        }

        let start = match prefix {
            Some(p) => self.resolve(p, false)?,
            None => self.root.clone(),
        };

        let mut entries = Vec::new();
        let walker = walkdir::WalkDir::new(&start)
            .follow_links(false)
            .into_iter()
            // filter_entry prunes whole subtrees: hidden folders are never
            // descended into, so .obsidian/.trash contents cannot appear.
            // The start directory itself is exempt — it is always validated
            // by resolve() first (and vault roots may legitimately start
            // with a dot, e.g. macOS tempdirs during tests).
            .filter_entry(|e| e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.'));
        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                // A race with `ob` pulling files can remove a file mid-walk;
                // skipping is safer than failing the whole listing.
                Err(_) => continue,
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(&self.root)
                .unwrap_or_else(|_| entry.path());
            let rel = rel.to_string_lossy().replace('\\', "/");
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            entries.push(NoteEntry {
                path: rel,
                size: meta.len(),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            });
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }

    /// Reads a note, returning its raw bytes.
    pub fn read(&self, rel: &str) -> Result<Vec<u8>> {
        let path = self.resolve(rel, true)?;
        fs::read(&path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => VaultError::NotFound(rel.to_string()),
            _ => VaultError::Io(e),
        })
    }

    /// Writes a note atomically: the content lands in a temp file inside the
    /// target directory, is fsynced, then renamed into place.
    ///
    /// This matters because `ob sync --continuous` watches the vault: a
    /// crash mid-write must never leave a truncated note for `ob` to pick
    /// up and replicate to every device. Parent directories are created as
    /// needed.
    pub fn write(&self, rel: &str, contents: &[u8]) -> Result<()> {
        let path = self.resolve(rel, false)?;
        if path.exists() && !path.is_file() {
            return Err(VaultError::Exists(format!("target is a directory: {rel}")));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut tmp = tempfile::NamedTempFile::new_in(path.parent().unwrap_or(&self.root))?;
        use std::io::Write;
        tmp.write_all(contents)?;
        // Sync before the rename so the renamed file never contains a
        // partially flushed buffer.
        tmp.as_file().sync_all()?;
        // persist() renames the temp file into place; PersistError wraps the
        // underlying io::Error plus the lost temp path, whose content is
        // irrelevant here since the caller can simply retry the write.
        tmp.persist(&path).map_err(|e| VaultError::Io(e.error))?;
        Ok(())
    }

    /// Moves or renames a note. Existing targets are only overwritten when
    /// `overwrite` is set; `move_note` will prompt the caller to decide.
    pub fn rename(&self, from: &str, to: &str, overwrite: bool) -> Result<()> {
        let from_path = self.resolve(from, true)?;
        if !from_path.is_file() {
            return Err(VaultError::InvalidPath(format!(
                "source is not a file: {from}"
            )));
        }
        let to_path = self.resolve(to, false)?;
        if !overwrite && to_path.exists() {
            return Err(VaultError::Exists(to.to_string()));
        }
        if let Some(parent) = to_path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Both paths are inside the same vault, hence the same filesystem,
        // so a plain rename cannot hit EXDEV.
        fs::rename(&from_path, &to_path)?;
        Ok(())
    }

    /// Deletes a note by moving it into the vault's `.trash` folder.
    /// Nothing is ever unlinked, so every client-initiated "delete" is
    /// reversible by hand (or by a future restore tool).
    pub fn delete(&self, rel: &str) -> Result<String> {
        let path = self.resolve(rel, true)?;
        if !path.is_file() {
            return Err(VaultError::InvalidPath(format!("not a file: {rel}")));
        }
        let trash = self.root.join(TRASH_DIR);
        fs::create_dir_all(&trash)?;

        let file_name = path
            .file_name()
            .ok_or_else(|| VaultError::InvalidPath(rel.to_string()))?
            .to_string_lossy()
            .into_owned();
        // Collision handling: deleting two notes with the same filename
        // must not clobber the first trash entry.
        let mut target = trash.join(&file_name);
        if target.exists() {
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| file_name.clone());
            let ext = path
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default();
            let stamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            target = trash.join(format!("{stem}.{stamp}{ext}"));
        }
        fs::rename(&path, &target)?;
        let trashed = target
            .strip_prefix(&self.root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| TRASH_DIR.into());
        Ok(trashed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn vault() -> (Vault, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let v = Vault::open(dir.path()).expect("vault opens");
        (v, dir)
    }

    fn write(v: &Vault, rel: &str, contents: &str) {
        v.write(rel, contents.as_bytes()).expect("write succeeds");
    }

    #[test]
    fn open_rejects_missing_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope");
        assert!(Vault::open(&missing).is_err());
    }

    #[test]
    fn resolve_rejects_traversal_and_absolute() {
        let (v, _d) = vault();
        for bad in [
            "../escape.md",
            "a/../../escape.md",
            "a/../b/../..",
            "/etc/passwd",
            "",
            "a//b/..",
            ".",
        ] {
            assert!(v.resolve(bad, false).is_err(), "should reject: {bad:?}");
        }
    }

    #[test]
    fn resolve_rejects_dot_folders() {
        let (v, _d) = vault();
        for bad in [".obsidian/app.json", "sub/.hidden/note.md", ".trash/x.md"] {
            assert!(v.resolve(bad, false).is_err(), "should reject: {bad:?}");
        }
    }

    #[test]
    fn resolve_rejects_symlink_escape() {
        let (v, dir) = vault();
        let outside = tempfile::tempdir().expect("tempdir");
        fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        // Symlink planted inside the vault pointing outside it.
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        assert!(v.resolve("link/secret.txt", true).is_err());
        assert!(v.resolve("link", true).is_err());
    }

    #[test]
    fn resolve_accepts_normal_paths() {
        let (v, _d) = vault();
        write(&v, "Notes/Deep/note.md", "hi");
        assert!(v.resolve("Notes/Deep/note.md", true).is_ok());
        assert!(v.resolve("Notes/Deep/note.md", false).is_ok());
        // Missing path: fine for writes, NotFound for reads.
        assert!(matches!(
            v.resolve("Notes/Deep/other.md", true),
            Err(VaultError::NotFound(_))
        ));
        assert!(v.resolve("Notes/Deep/other.md", false).is_ok());
    }

    #[test]
    fn read_write_roundtrip_and_no_temp_leftovers() {
        let (v, dir) = vault();
        write(&v, "Notes/note.md", "# hello");
        assert_eq!(v.read("Notes/note.md").unwrap(), b"# hello");
        write(&v, "Notes/note.md", "# updated");
        assert_eq!(v.read("Notes/note.md").unwrap(), b"# updated");
        // Atomic write must not leave temp files behind.
        let leftovers: Vec<_> = fs::read_dir(dir.path().join("Notes"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp leftovers: {leftovers:?}");
    }

    #[test]
    fn read_missing_is_not_found() {
        let (v, _d) = vault();
        assert!(matches!(v.read("nope.md"), Err(VaultError::NotFound(_))));
    }

    #[test]
    fn list_skips_hidden_and_sorts() {
        let (v, dir) = vault();
        write(&v, "b/note2.md", "x");
        write(&v, "a/note1.md", "x");
        // Write the hidden fixture with plain fs: the sandbox itself must
        // refuse to create files under .obsidian.
        fs::create_dir_all(dir.path().join(".obsidian")).unwrap();
        fs::write(dir.path().join(".obsidian/app.json"), "{}").unwrap();
        let entries = v.list(None).unwrap();
        let paths: Vec<_> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["a/note1.md", "b/note2.md"]);
        // Prefix filter
        let entries = v.list(Some("a")).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "a/note1.md");
        // Prefix probing hidden folders is rejected
        assert!(v.list(Some(".obsidian")).is_err());
    }

    #[test]
    fn rename_moves_and_respects_overwrite() {
        let (v, _d) = vault();
        write(&v, "a.md", "one");
        write(&v, "b.md", "two");
        v.rename("a.md", "moved/c.md", false).unwrap();
        assert!(matches!(v.read("a.md"), Err(VaultError::NotFound(_))));
        assert_eq!(v.read("moved/c.md").unwrap(), b"one");
        // Refuses to clobber without overwrite=true
        assert!(matches!(
            v.rename("moved/c.md", "b.md", false),
            Err(VaultError::Exists(_))
        ));
        v.rename("moved/c.md", "b.md", true).unwrap();
        assert_eq!(v.read("b.md").unwrap(), b"one");
        assert!(v.rename("ghost.md", "x.md", false).is_err());
    }

    #[test]
    fn delete_moves_to_trash_with_collision_handling() {
        let (v, dir) = vault();
        write(&v, "n.md", "first");
        let trashed1 = v.delete("n.md").unwrap();
        assert_eq!(trashed1, ".trash/n.md");
        assert!(matches!(v.read("n.md"), Err(VaultError::NotFound(_))));
        assert!(
            v.list(None).unwrap().is_empty(),
            "trash must be hidden from listing"
        );
        assert!(fs::read(dir.path().join(".trash/n.md")).is_ok());

        // Deleting another note with the same filename must not clobber.
        write(&v, "sub/n.md", "second");
        let trashed2 = v.delete("sub/n.md").unwrap();
        assert_ne!(trashed1, trashed2, "collision should get a unique name");
        assert_eq!(fs::read(dir.path().join(".trash/n.md")).unwrap(), b"first");
    }
}
