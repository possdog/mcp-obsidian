//! Write-side note operations: create/overwrite, patch, move (with link
//! rewriting), trash-delete, and daily notes.
//!
//! All filesystem effects go through [`Vault`], so every safety property
//! of the sandbox (traversal rejection, dot-folder exclusion, atomic
//! writes, trash deletes) applies to writes exactly as it does to reads.
//! Patches are computed against frontmatter-split text and the original
//! frontmatter block is spliced back verbatim, so unknown YAML fields
//! survive every edit.
//!
//! Every operation returns a [`WriteOutcome`] carrying a message for the
//! calling model and the list of vault-relative paths whose search-index
//! entries must be refreshed. The server layer performs that refresh
//! synchronously; the filesystem watcher would catch up on its own, but
//! a write-then-search sequence from a client should not depend on the
//! debounce window.

use std::fmt::Write as _;
use std::ops::Range;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::markdown;
use crate::notes::LinkTables;
use crate::vault::{NoteEntry, Vault, VaultError};

pub type Result<T> = std::result::Result<T, VaultError>;

/// Default daily-note filename format, matching Obsidian's daily-notes
/// plugin. `YYYY`/`MM`/`DD` tokens are substituted; other text passes
/// through verbatim.
pub const DAILY_NOTE_FORMAT: &str = "YYYY-MM-DD.md";

/// Folder names checked (in this order) when auto-detecting the daily
/// notes folder in a vault without an Obsidian daily-notes config.
const DAILY_DIR_CANDIDATES: [&str; 3] = ["Daily Notes", "Daily", "Journal"];

/// How a [`Writes::patch_note`] call modifies the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchOp {
    /// Add content at the end of the body.
    Append,
    /// Add content at the start of the body (after frontmatter).
    Prepend,
    /// Insert content directly below a heading, above existing content.
    InsertAtHeading,
    /// Replace the content under a heading; the heading line itself is
    /// kept (it is the anchor the caller addressed).
    ReplaceSection,
}

/// What a [`Writes::daily_note`] call does with the day's note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DailyAction {
    /// Read the day's note (or report its absence).
    Get,
    /// Create the day's note; an existing note is left untouched.
    Create,
    /// Append to the day's note, creating it when missing.
    Append,
}

/// The result of one write operation.
#[derive(Debug, Clone)]
pub struct WriteOutcome {
    /// Human/model-facing summary of what happened.
    pub message: String,
    /// Vault-relative paths whose search-index entries must be refreshed
    /// (a moved note lists both its old and new paths, plus every note
    /// whose links were rewritten).
    pub reindex: Vec<String>,
}

/// Daily-note settings resolved from configuration; `None` fields mean
/// "auto-detect" (see [`Writes::resolve_daily_dir`] and
/// [`DAILY_NOTE_FORMAT`]).
#[derive(Debug, Clone, Default)]
pub struct DailyNoteSettings {
    pub dir: Option<String>,
    pub format: Option<String>,
}

/// A calendar date in UTC, the daily-note addressing unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Date {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

impl std::fmt::Display for Date {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// Write-side operations over a vault.
#[derive(Debug, Clone)]
pub struct Writes {
    vault: Vault,
    daily: DailyNoteSettings,
}

impl Writes {
    pub fn open(vault: Vault, daily: DailyNoteSettings) -> Self {
        Self { vault, daily }
    }

    pub fn vault(&self) -> &Vault {
        &self.vault
    }

    /// Creates or overwrites a note. With `frontmatter`, the content is
    /// treated as the body and the YAML block is prepended (fenced input
    /// is accepted and normalized); without it, `content` is written
    /// verbatim.
    ///
    /// An existing note is only replaced when `overwrite` is set: a
    /// whole-file write is the most destructive thing a client can do
    /// here, and a mistaken one must not silently destroy work.
    pub fn write_note(
        &self,
        path: &str,
        content: &str,
        frontmatter: Option<&str>,
        overwrite: bool,
    ) -> Result<WriteOutcome> {
        let existed = self.vault.read(path).is_ok();
        if existed && !overwrite {
            return Err(VaultError::Exists(format!(
                "{path} already exists (pass overwrite=true to replace it)"
            )));
        }
        let raw = match frontmatter.map(normalize_frontmatter) {
            Some(fm) if !fm.is_empty() => format!("---\n{fm}\n---\n{content}"),
            _ => content.to_string(),
        };
        self.vault.write(path, raw.as_bytes())?;
        let verb = if existed { "Updated" } else { "Created" };
        Ok(WriteOutcome {
            message: format!("{verb} {path} ({} bytes).", raw.len()),
            reindex: vec![path.to_string()],
        })
    }

    /// Applies a localized edit to a note. The original frontmatter block
    /// is preserved byte-for-byte; only the body changes.
    pub fn patch_note(
        &self,
        path: &str,
        op: PatchOp,
        content: &str,
        heading: Option<&str>,
    ) -> Result<WriteOutcome> {
        let raw = self.read_utf8(path)?;
        if content.trim().is_empty() && op != PatchOp::ReplaceSection {
            return Err(VaultError::InvalidPath(format!(
                "content is empty for {op:?}"
            )));
        }
        let heading = match op {
            PatchOp::InsertAtHeading | PatchOp::ReplaceSection => Some(
                heading
                    .ok_or_else(|| VaultError::InvalidPath(format!("{op:?} requires a heading")))?,
            ),
            _ => None,
        };
        let (frontmatter, body) = markdown::split_frontmatter(&raw);
        let new_body = match op {
            PatchOp::Append => join_blocks(body, content),
            PatchOp::Prepend => join_blocks(content, body),
            PatchOp::InsertAtHeading => {
                let heading = heading.expect("checked above");
                insert_at_heading(body, heading, content).ok_or_else(|| {
                    VaultError::NotFound(format!("heading {heading:?} in note {path}"))
                })?
            }
            PatchOp::ReplaceSection => {
                let heading = heading.expect("checked above");
                replace_section(body, heading, content).ok_or_else(|| {
                    VaultError::NotFound(format!("heading {heading:?} in note {path}"))
                })?
            }
        };
        let new_raw = match frontmatter {
            Some(fm) => format!("{fm}{new_body}"),
            None => new_body,
        };
        self.vault.write(path, new_raw.as_bytes())?;
        Ok(WriteOutcome {
            message: format!(
                "Patched {path} ({op:?}); note is now {} bytes.",
                new_raw.len()
            ),
            reindex: vec![path.to_string()],
        })
    }

    /// Moves or renames a note (or any vault file). When `update_links`
    /// is set, wikilinks and markdown links across the vault that pointed
    /// at the old path are rewritten, mirroring Obsidian's own move
    /// behavior (see [`Writes::rewrite_links`]).
    pub fn move_note(
        &self,
        from: &str,
        to: &str,
        overwrite: bool,
        update_links: bool,
    ) -> Result<WriteOutcome> {
        // Listed before the rename: the pre-move vault state is what the
        // existing link texts resolve against.
        let entries = self.vault.list(None)?;
        // Link texts may use different casing than the filesystem (the
        // resolution tables key on the listed, actual-case paths), so the
        // rewrite compares against the listed form.
        let from = entries
            .iter()
            .map(|e| e.path.as_str())
            .find(|p| p.eq_ignore_ascii_case(from))
            .unwrap_or(from);

        self.vault.rename(from, to, overwrite)?;
        let mut reindex = vec![from.to_string(), to.to_string()];
        let mut message = format!("Moved {from} to {to}.");
        if update_links {
            let changed = self.rewrite_links(&entries, from, to)?;
            if changed.is_empty() {
                message.push_str(" No links needed updating.");
            } else {
                message.push_str(&format!(
                    " Updated links in {} note(s): {}.",
                    changed.len(),
                    changed.join(", ")
                ));
                reindex.extend(changed);
            }
        } else {
            message.push_str(" Links were left untouched.");
        }
        Ok(WriteOutcome { message, reindex })
    }

    /// Rewrites vault links that break when `from` becomes `to`. Returns
    /// the paths of the notes whose text changed.
    ///
    /// Rules, mirroring Obsidian:
    /// - Wikilinks anywhere (including frontmatter values) that resolved
    ///   to `from` get a new target: the bare name when it is unambiguous
    ///   after the move, otherwise the full new path. Heading, block, and
    ///   alias parts are preserved; `[[note]]` links never gain a `.md`.
    /// - Markdown links are recomputed relative to their containing note
    ///   (and percent-encoded), anchors preserved.
    /// - The moved note itself keeps bare-name and vault-root wikilinks
    ///   (they are position-independent) but its own relative markdown
    ///   links — and self-links — are recomputed from the new directory.
    ///
    /// Resolution uses pre-move tables: link texts were written against
    /// the vault as it was. A second, post-move table (the listing with
    /// `from` replaced by `to`) decides bare-name ambiguity for the new
    /// location.
    fn rewrite_links(
        &self,
        pre_entries: &[NoteEntry],
        from: &str,
        to: &str,
    ) -> Result<Vec<String>> {
        let pre = LinkTables::build(pre_entries.iter().map(|e| e.path.clone()));
        let post = LinkTables::build(pre_entries.iter().map(|e| {
            if e.path == from {
                to.to_string()
            } else {
                e.path.clone()
            }
        }));

        let mut changed = Vec::new();
        for entry in pre_entries {
            if !entry.path.ends_with(".md") {
                continue;
            }
            // The moved note has already been renamed, so its content is
            // read (and written back) at the destination.
            let read_path = if entry.path == from {
                to
            } else {
                entry.path.as_str()
            };
            let Ok(raw) = self.read_utf8(read_path) else {
                continue;
            };
            let (_, body) = markdown::split_frontmatter(&raw);
            // Markdown-link spans are body-relative; wikilink spans are
            // raw-relative. Shifting by the frontmatter length puts both
            // into raw coordinates for splicing.
            let body_offset = raw.len() - body.len();
            let source_dir = dir_of(&entry.path);

            let mut edits: Vec<(Range<usize>, String)> = Vec::new();
            for link in markdown::parse_wikilinks(&raw) {
                if pre.resolve_wikilink(&link.target).as_deref() != Some(from) {
                    continue;
                }
                // "Did the written target carry the extension?" — this
                // also covers attachments, where the extension is the
                // filename's.
                let had_ext = link.target.trim().eq_ignore_ascii_case(from);
                let new_target = new_wikilink_target(&post, to, had_ext);
                if new_target.eq_ignore_ascii_case(link.target.trim()) {
                    continue;
                }
                edits.push((
                    link.span.clone(),
                    format_wikilink(
                        link.embed,
                        &new_target,
                        link.heading.as_deref(),
                        link.block.as_deref(),
                        link.alias.as_deref(),
                    ),
                ));
            }
            for link in markdown::parse_md_links(body) {
                if link.external {
                    continue;
                }
                let decoded = markdown::percent_decode(&link.target);
                let (target, base_dir) = if entry.path == from {
                    // Inside the moved note every relative link may break
                    // (the note changed directories), so all of them are
                    // recomputed from the new location; a link that
                    // resolved to the note itself now targets `to`.
                    let resolved = match pre.resolve_md_link(&source_dir, &decoded) {
                        Some(resolved) if resolved == from => to.to_string(),
                        Some(resolved) => resolved,
                        // Unresolvable before the move; leave it alone.
                        None => continue,
                    };
                    (resolved, dir_of(to))
                } else {
                    match pre.resolve_md_link(&source_dir, &decoded) {
                        Some(resolved) if resolved == from => (to.to_string(), source_dir.clone()),
                        _ => continue,
                    }
                };
                let new_rel = relative_path(&base_dir, &target);
                if decoded == new_rel {
                    continue;
                }
                edits.push((
                    shift(&link.span, body_offset),
                    format_md_link(
                        link.embed,
                        &link.text,
                        &percent_encode_path(&new_rel),
                        link.anchor.as_deref(),
                    ),
                ));
            }

            if edits.is_empty() {
                continue;
            }
            let new_raw = splice(&raw, edits);
            // Writing back through the sandbox keeps the atomic-write
            // guarantee; the byte-for-byte splice preserves frontmatter.
            self.vault.write(read_path, new_raw.as_bytes())?;
            changed.push(read_path.to_string());
        }
        Ok(changed)
    }

    /// Deletes a note by moving it to the vault's `.trash` (see
    /// [`crate::vault::Vault::delete`]). Nothing is ever unlinked.
    pub fn delete_note(&self, path: &str) -> Result<WriteOutcome> {
        let trashed = self.vault.delete(path)?;
        Ok(WriteOutcome {
            message: format!("Moved {path} to {trashed} (recoverable from the vault trash)."),
            reindex: vec![path.to_string()],
        })
    }

    /// Reads, creates, or appends to the daily note for a date.
    ///
    /// `date` defaults to today (UTC — the server may live in another
    /// timezone than the user; clients that care should pass the date).
    pub fn daily_note(
        &self,
        action: DailyAction,
        date: Option<&str>,
        content: Option<&str>,
    ) -> Result<WriteOutcome> {
        let date = match date {
            Some(raw) => parse_date(raw).ok_or_else(|| {
                VaultError::InvalidPath(format!("date must be YYYY-MM-DD, got {raw:?}"))
            })?,
            None => today_utc(),
        };
        let (dir, format) = self.resolve_daily_config()?;
        let path = format!("{dir}/{}", format_daily_filename(&format, date));

        match action {
            DailyAction::Get => match self.vault.read(&path) {
                Ok(bytes) => Ok(WriteOutcome {
                    message: format!("({path})\n\n{}", String::from_utf8_lossy(&bytes)),
                    reindex: Vec::new(),
                }),
                Err(VaultError::NotFound(_)) => Ok(WriteOutcome {
                    message: format!("No daily note for {date}; it would live at {path}."),
                    reindex: Vec::new(),
                }),
                Err(e) => Err(e),
            },
            DailyAction::Create => {
                if self.vault.read(&path).is_ok() {
                    return Ok(WriteOutcome {
                        message: format!("{path} already exists."),
                        reindex: Vec::new(),
                    });
                }
                self.vault.write(&path, content.unwrap_or("").as_bytes())?;
                Ok(WriteOutcome {
                    message: format!("Created {path}."),
                    reindex: vec![path],
                })
            }
            DailyAction::Append => {
                let content = content.ok_or_else(|| {
                    VaultError::InvalidPath("content is required for append".into())
                })?;
                if content.trim().is_empty() {
                    return Err(VaultError::InvalidPath(
                        "content is empty for daily append".into(),
                    ));
                }
                match self.vault.read(&path) {
                    Ok(bytes) => {
                        let raw = String::from_utf8(bytes)
                            .map_err(|_| VaultError::NotUtf8(path.clone()))?;
                        let (frontmatter, body) = markdown::split_frontmatter(&raw);
                        let new_body = join_blocks(body, content);
                        let new_raw = match frontmatter {
                            Some(fm) => format!("{fm}{new_body}"),
                            None => new_body,
                        };
                        self.vault.write(&path, new_raw.as_bytes())?;
                        Ok(WriteOutcome {
                            message: format!("Appended to {path}."),
                            reindex: vec![path],
                        })
                    }
                    Err(VaultError::NotFound(_)) => {
                        self.vault
                            .write(&path, ensure_trailing_newline(content).as_bytes())?;
                        Ok(WriteOutcome {
                            message: format!("Created {path} (it did not exist yet)."),
                            reindex: vec![path],
                        })
                    }
                    Err(e) => Err(e),
                }
            }
        }
    }

    /// Resolves the daily-notes folder and filename format, each field
    /// falling back independently:
    /// 1. an explicit `DAILY_NOTES_DIR` / `DAILY_NOTE_FORMAT` setting,
    /// 2. the vault's own `.obsidian/daily-notes.json` — the plugin's
    ///    configured values are the most accurate source for real vaults,
    /// 3. the first of the conventional [`DAILY_DIR_CANDIDATES`] folders
    ///    that exists in the vault, and
    /// 4. Obsidian's own defaults (`Daily Notes`, `YYYY-MM-DD.md`).
    ///
    /// Reading `.obsidian` here is a server-internal read that
    /// deliberately bypasses the tool sandbox: clients never touch the
    /// config folder, but honoring the vault's own settings is the point
    /// of auto-detection.
    fn resolve_daily_config(&self) -> Result<(String, String)> {
        let mut dir = self.daily.dir.clone();
        let mut format = self.daily.format.clone();
        if dir.is_none() || format.is_none() {
            let config = self.vault.root().join(".obsidian").join("daily-notes.json");
            if let Ok(bytes) = std::fs::read(&config)
                && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
            {
                if dir.is_none()
                    && let Some(folder) = value.get("folder").and_then(|v| v.as_str())
                {
                    let folder = folder.trim_matches('/');
                    if !folder.is_empty() {
                        dir = Some(folder.to_string());
                    }
                }
                if format.is_none()
                    && let Some(config_format) = value.get("format").and_then(|v| v.as_str())
                    && !config_format.is_empty()
                {
                    format = Some(config_format.to_string());
                }
            }
        }
        let dir = match dir {
            Some(dir) => {
                let dir = dir.trim_matches('/');
                if dir.is_empty() {
                    return Err(VaultError::InvalidPath(
                        "configured daily notes folder is empty".into(),
                    ));
                }
                dir.to_string()
            }
            None => DAILY_DIR_CANDIDATES
                .iter()
                .find(|candidate| self.vault.root().join(candidate).is_dir())
                .map(|candidate| candidate.to_string())
                .unwrap_or_else(|| DAILY_DIR_CANDIDATES[0].to_string()),
        };
        Ok((dir, format.unwrap_or_else(|| DAILY_NOTE_FORMAT.to_string())))
    }

    /// Reads a note and requires valid UTF-8: patches rewrite and store
    /// the text, so lossy conversion here would permanently replace
    /// invalid bytes with U+FFFD.
    fn read_utf8(&self, path: &str) -> Result<String> {
        let bytes = self.vault.read(path)?;
        String::from_utf8(bytes).map_err(|_| VaultError::NotUtf8(path.to_string()))
    }
}

/// Normalizes a frontmatter argument into bare YAML: surrounding `---`
/// fences are accepted and stripped (agents habitually paste the fenced
/// form), and whitespace is trimmed.
fn normalize_frontmatter(frontmatter: &str) -> String {
    let fm = frontmatter.trim();
    let fm = fm.strip_prefix("---").unwrap_or(fm);
    let fm = fm.strip_suffix("---").unwrap_or(fm);
    fm.trim_matches(['\n', '\r']).to_string()
}

/// Joins two text blocks with exactly one blank line between them and a
/// trailing newline, preserving each block's internal formatting. Empty
/// blocks vanish, so appending to an empty body yields just the content.
fn join_blocks(a: &str, b: &str) -> String {
    let a = a.trim_matches('\n');
    let b = b.trim_matches('\n');
    match (a.is_empty(), b.is_empty()) {
        (_, true) => format!("{a}\n"),
        (true, _) => format!("{b}\n"),
        (false, false) => format!("{a}\n\n{b}\n"),
    }
}

/// Ensures the text ends with a newline (notes conventionally do).
fn ensure_trailing_newline(s: &str) -> String {
    let mut out = s.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Inserts `content` directly below the heading line (case-insensitive
/// match), above any existing section content. Returns `None` when the
/// heading does not exist.
fn insert_at_heading(body: &str, heading: &str, content: &str) -> Option<String> {
    let found = find_heading(body, heading)?;
    Some(insert_at(body, found.span.end, content))
}

/// Replaces the content under the heading with `content`; the heading
/// line itself is kept, and empty content clears the section. Returns
/// `None` when the heading does not exist.
fn replace_section(body: &str, heading: &str, content: &str) -> Option<String> {
    let headings = markdown::parse_headings(body);
    let found = headings
        .iter()
        .find(|h| h.text.eq_ignore_ascii_case(heading.trim()))?;
    // Section extent: up to the next heading of the same or higher level,
    // exactly like `markdown::extract_section`.
    let section_end = headings
        .iter()
        .find(|h| h.span.start >= found.span.end && h.level <= found.level)
        .map(|h| h.span.start)
        .unwrap_or(body.len());
    let mut out = body[..found.span.end].to_string();
    if !content.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(content);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str(&body[section_end..]);
    Some(out)
}

/// Finds the first heading whose text case-insensitively matches.
fn find_heading(body: &str, heading: &str) -> Option<markdown::Heading> {
    markdown::parse_headings(body)
        .into_iter()
        .find(|h| h.text.eq_ignore_ascii_case(heading.trim()))
}

/// Writes `content` into `body` at byte offset `at` (always a heading
/// span end, i.e. a line boundary), keeping what follows. A heading at
/// the end of the file without its own newline gains one, so the inserted
/// content never lands on the heading's line.
fn insert_at(body: &str, at: usize, content: &str) -> String {
    let mut out = body[..at].to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(content);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&body[at..]);
    out
}

/// The new target text for a wikilink that pointed at the moved note:
/// the bare name when it is unambiguous after the move (Obsidian's
/// "shortest path when possible"), otherwise the full new path. Bare
/// `.md` links never gain the extension, and links that carried it keep
/// it.
fn new_wikilink_target(post: &LinkTables, to: &str, had_ext: bool) -> String {
    let file_name = to.rsplit('/').next().unwrap_or(to);
    let stem = to
        .strip_suffix(".md")
        .map(|p| p.rsplit('/').next().unwrap_or(p))
        .unwrap_or(file_name);
    let bare = if to.ends_with(".md") { stem } else { file_name };
    if post.basename_candidates(bare) == 1 {
        if had_ext && to.ends_with(".md") {
            file_name.to_string()
        } else {
            bare.to_string()
        }
    } else if had_ext || !to.ends_with(".md") {
        to.to_string()
    } else {
        to.strip_suffix(".md").unwrap_or(to).to_string()
    }
}

/// Rebuilds a wikilink token with a new target, preserving the embed
/// marker, heading/block references, and alias.
fn format_wikilink(
    embed: bool,
    target: &str,
    heading: Option<&str>,
    block: Option<&str>,
    alias: Option<&str>,
) -> String {
    let mut out = String::new();
    if embed {
        out.push('!');
    }
    out.push_str("[[");
    out.push_str(target);
    if let Some(heading) = heading {
        out.push('#');
        out.push_str(heading);
    }
    if let Some(block) = block {
        out.push_str("#^");
        out.push_str(block);
    }
    if let Some(alias) = alias {
        out.push('|');
        out.push_str(alias);
    }
    out.push_str("]]");
    out
}

/// Rebuilds a markdown link with a new target, preserving the embed
/// marker, link text, and anchor.
fn format_md_link(embed: bool, text: &str, target: &str, anchor: Option<&str>) -> String {
    let mut out = String::new();
    if embed {
        out.push('!');
    }
    out.push('[');
    out.push_str(text);
    out.push_str("](");
    out.push_str(target);
    if let Some(anchor) = anchor {
        out.push('#');
        out.push_str(anchor);
    }
    out.push(')');
    out
}

/// Percent-encodes a vault path for a markdown link, mirroring Obsidian:
/// unreserved characters plus the path separators `/` and `:` stay
/// verbatim, everything else (spaces, parentheses, non-ASCII, …) becomes
/// `%XX`.
fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                out.push(byte as char);
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// The vault-relative path from `from_dir` ("" = vault root) to
/// `to_path`, the form markdown links need. Case-sensitive: both sides
/// come from the same vault listing.
fn relative_path(from_dir: &str, to_path: &str) -> String {
    let src: Vec<&str> = if from_dir.is_empty() {
        Vec::new()
    } else {
        from_dir.split('/').collect()
    };
    let dst: Vec<&str> = to_path.split('/').collect();
    // The filename is never consumed as a common component.
    let max = src.len().min(dst.len().saturating_sub(1));
    let mut common = 0;
    while common < max && src[common] == dst[common] {
        common += 1;
    }
    let mut parts: Vec<&str> = vec![".."; src.len() - common];
    parts.extend(&dst[common..]);
    parts.join("/")
}

/// Directory part of a vault-relative path ("" for root-level notes).
fn dir_of(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => String::new(),
    }
}

/// Shifts a body-relative span into raw-note coordinates.
fn shift(span: &Range<usize>, offset: usize) -> Range<usize> {
    span.start + offset..span.end + offset
}

/// Applies span replacements to the raw note text, in span order. An
/// edit overlapping its predecessor is skipped: wikilink and markdown
/// link tokens can never legitimately overlap, so this only guards
/// against pathological inputs corrupting the note.
fn splice(raw: &str, mut edits: Vec<(Range<usize>, String)>) -> String {
    edits.sort_by_key(|(span, _)| span.start);
    let mut out = String::with_capacity(raw.len());
    let mut cursor = 0;
    for (span, replacement) in edits {
        if span.start < cursor {
            continue;
        }
        out.push_str(&raw[cursor..span.start]);
        out.push_str(&replacement);
        cursor = span.end;
    }
    out.push_str(&raw[cursor..]);
    out
}

/// Substitutes date tokens into a daily-note filename format. Only the
/// tokens Obsidian's daily-notes plugin uses for dates are supported;
/// anything else passes through verbatim.
fn format_daily_filename(format: &str, date: Date) -> String {
    format
        .replace("YYYY", &format!("{:04}", date.year))
        .replace("MM", &format!("{:02}", date.month))
        .replace("DD", &format!("{:02}", date.day))
}

/// Parses a strict `YYYY-MM-DD` date, rejecting impossible month/day
/// combinations (leap years included).
pub fn parse_date(input: &str) -> Option<Date> {
    let input = input.trim();
    let bytes = input.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let digit = |i: usize| (bytes[i] as char).to_digit(10);
    let year = (digit(0)? * 1000 + digit(1)? * 100 + digit(2)? * 10 + digit(3)?) as i32;
    let month = digit(5)? * 10 + digit(6)?;
    let day = digit(8)? * 10 + digit(9)?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    Some(Date { year, month, day })
}

/// Today's date in UTC. The server may run in a different timezone than
/// the user; daily-note clients that care pass an explicit date.
pub fn today_utc() -> Date {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    civil_from_days((secs / 86_400) as i64)
}

/// Converts days since the Unix epoch to a civil date (Howard Hinnant's
/// algorithm — branchless, valid across the whole practical range).
fn civil_from_days(days: i64) -> Date {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    Date {
        year: (year + i64::from(month <= 2)) as i32,
        month: month as u32,
        day: day as u32,
    }
}

fn is_leap_year(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writes() -> (Writes, tempfile::TempDir) {
        writes_with(DailyNoteSettings::default())
    }

    fn writes_with(daily: DailyNoteSettings) -> (Writes, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let vault = Vault::open(dir.path()).expect("vault opens");
        (Writes::open(vault, daily), dir)
    }

    fn read(w: &Writes, path: &str) -> String {
        String::from_utf8(w.vault.read(path).expect("read")).expect("utf8")
    }

    #[test]
    fn write_note_create_update_and_frontmatter() {
        let (w, _d) = writes();
        let out = w
            .write_note("notes/a.md", "body text\n", None, false)
            .expect("create");
        assert!(out.message.starts_with("Created notes/a.md"));
        assert_eq!(out.reindex, ["notes/a.md"]);

        // Refuses to clobber without the flag.
        let err = w.write_note("notes/a.md", "replaced\n", None, false);
        assert!(matches!(err, Err(VaultError::Exists(_))));
        assert!(w.write_note("notes/a.md", "replaced\n", None, true).is_ok());
        assert_eq!(read(&w, "notes/a.md"), "replaced\n");

        // Frontmatter: bare YAML and the fenced form both normalize to
        // the same output.
        w.write_note("fm1.md", "b1", Some("title: One\ntags: [x]"), false)
            .expect("fm");
        assert_eq!(read(&w, "fm1.md"), "---\ntitle: One\ntags: [x]\n---\nb1");
        w.write_note("fm2.md", "b2", Some("---\ntitle: Two\n---"), false)
            .expect("fenced fm");
        assert_eq!(read(&w, "fm2.md"), "---\ntitle: Two\n---\nb2");

        // Empty frontmatter is not written as an empty block.
        w.write_note("fm3.md", "b3", Some("   \n"), false)
            .expect("empty fm");
        assert_eq!(read(&w, "fm3.md"), "b3");
    }

    #[test]
    fn patch_note_operations_preserve_frontmatter() {
        let (w, _d) = writes();
        w.vault
            .write("n.md", b"---\ntitle: Keep\nunknown: field\n---\n# Top\ntop text\n\n## Log\nold entry\n\n## Other\nother\n")
            .expect("seed");

        w.patch_note("n.md", PatchOp::Append, "appended\n", None)
            .expect("append");
        let raw = read(&w, "n.md");
        assert!(raw.starts_with("---\ntitle: Keep\nunknown: field\n---\n"));
        assert!(raw.ends_with("other\n\nappended\n"));

        w.patch_note("n.md", PatchOp::Prepend, "prepended\n", None)
            .expect("prepend");
        let raw = read(&w, "n.md");
        assert!(raw.contains("---\nprepended\n\n# Top"), "got: {raw}");

        w.patch_note("n.md", PatchOp::InsertAtHeading, "newest\n", Some("log"))
            .expect("insert");
        let raw = read(&w, "n.md");
        assert!(raw.contains("## Log\nnewest\nold entry"), "got: {raw}");

        w.patch_note(
            "n.md",
            PatchOp::ReplaceSection,
            "replaced content\n",
            Some("LOG"),
        )
        .expect("replace");
        let raw = read(&w, "n.md");
        assert!(
            raw.contains("## Log\nreplaced content\n## Other"),
            "got: {raw}"
        );

        // Clearing a section keeps the heading line.
        w.patch_note("n.md", PatchOp::ReplaceSection, "", Some("log"))
            .expect("clear");
        let raw = read(&w, "n.md");
        assert!(raw.contains("## Log\n## Other"), "got: {raw}");

        // Missing heading and missing requirements are errors.
        assert!(matches!(
            w.patch_note("n.md", PatchOp::ReplaceSection, "x", Some("nope")),
            Err(VaultError::NotFound(_))
        ));
        assert!(matches!(
            w.patch_note("n.md", PatchOp::InsertAtHeading, "x", None),
            Err(VaultError::InvalidPath(_))
        ));
        assert!(matches!(
            w.patch_note("n.md", PatchOp::Append, "  ", None),
            Err(VaultError::InvalidPath(_))
        ));
    }

    #[test]
    fn patch_note_insert_into_last_heading_without_newline() {
        let (w, _d) = writes();
        w.write_note("tail.md", "## Log", None, false)
            .expect("create");
        w.patch_note("tail.md", PatchOp::InsertAtHeading, "entry\n", Some("log"))
            .expect("insert");
        assert_eq!(read(&w, "tail.md"), "## Log\nentry\n");
    }

    #[test]
    fn move_note_rewrites_wikilinks_and_md_links() {
        let (w, _d) = writes();
        w.write_note("projects/beta.md", "# Beta\n", None, false)
            .expect("beta");
        w.write_note(
            "linker.md",
            "[[beta]] [[beta#Head|al]] [[projects/beta]] [b](projects/beta.md#sec) ![[beta]]",
            None,
            false,
        )
        .expect("linker");

        let out = w
            .move_note("projects/beta.md", "renamed/gamma.md", false, true)
            .expect("move");
        assert!(
            out.message
                .starts_with("Moved projects/beta.md to renamed/gamma.md")
        );
        assert!(out.reindex.contains(&"linker.md".to_string()));
        // Old path (for index deletion) and new path are both listed.
        assert!(out.reindex.contains(&"projects/beta.md".to_string()));
        assert!(out.reindex.contains(&"renamed/gamma.md".to_string()));

        // "gamma" is unambiguous after the move, so every link collapses
        // to the shortest form Obsidian itself would write; heading,
        // alias, embed, and markdown-anchor parts are preserved.
        assert_eq!(
            read(&w, "linker.md"),
            "[[gamma]] [[gamma#Head|al]] [[gamma]] [b](renamed/gamma.md#sec) ![[gamma]]"
        );
    }

    #[test]
    fn move_note_ambiguous_name_uses_full_path() {
        let (w, _d) = writes();
        w.write_note("a/dup.md", "x", None, false).expect("dup a");
        w.write_note("b/dup.md", "x", None, false).expect("dup b");
        w.write_note("l.md", "[[dup]]\n", None, false)
            .expect("linker");
        // Both notes are named dup; the link resolved to the shortest
        // path (a/dup.md), which then moves somewhere it stays ambiguous.
        w.move_note("a/dup.md", "c/dup.md", false, true)
            .expect("move");
        let raw = read(&w, "l.md");
        assert!(raw.contains("[[c/dup]]"), "got: {raw}");
    }

    #[test]
    fn move_note_fixes_links_inside_the_moved_note() {
        let (w, _d) = writes();
        // Relative links from the note's own directory, plus a self-link.
        w.write_note(
            "sub/inner.md",
            "[o](other.md) [s](inner.md) [[inner]]\n",
            None,
            false,
        )
        .expect("inner");
        w.write_note("sub/other.md", "# Other\n", None, false)
            .expect("other");

        w.move_note("sub/inner.md", "deep/moved.md", false, true)
            .expect("move");
        let raw = read(&w, "deep/moved.md");
        assert!(raw.contains("[o](../sub/other.md)"), "got: {raw}");
        assert!(raw.contains("[s](moved.md)"), "got: {raw}");
        assert!(raw.contains("[[moved]]"), "got: {raw}");
    }

    #[test]
    fn move_note_without_link_update_leaves_links() {
        let (w, _d) = writes();
        w.write_note("old.md", "x", None, false).expect("old");
        w.write_note("l.md", "[[old]]\n", None, false)
            .expect("linker");
        let out = w.move_note("old.md", "new.md", false, false).expect("move");
        assert!(out.message.contains("Links were left untouched"));
        assert_eq!(read(&w, "l.md"), "[[old]]\n");
    }

    #[test]
    fn move_note_refuses_existing_target_and_missing_source() {
        let (w, _d) = writes();
        w.write_note("a.md", "one", None, false).expect("a");
        w.write_note("b.md", "two", None, false).expect("b");
        assert!(matches!(
            w.move_note("a.md", "b.md", false, true),
            Err(VaultError::Exists(_))
        ));
        assert!(matches!(
            w.move_note("ghost.md", "x.md", false, true),
            Err(VaultError::NotFound(_))
        ));
    }

    #[test]
    fn delete_note_moves_to_trash() {
        let (w, _d) = writes();
        w.write_note("gone.md", "bye\n", None, false)
            .expect("create");
        let out = w.delete_note("gone.md").expect("delete");
        assert!(out.message.contains(".trash/gone.md"));
        assert_eq!(out.reindex, ["gone.md"]);
        assert!(matches!(
            w.vault.read("gone.md"),
            Err(VaultError::NotFound(_))
        ));
        assert!(w.vault.list(None).expect("list").is_empty());
    }

    #[test]
    fn daily_note_create_get_append() {
        let (w, _d) = writes();
        // get on a missing day reports where it would live.
        let out = w
            .daily_note(DailyAction::Get, Some("2026-09-25"), None)
            .expect("get");
        assert!(out.message.contains("No daily note for 2026-09-25"));

        // create writes the note and reports the path.
        let out = w
            .daily_note(DailyAction::Create, Some("2026-09-25"), Some("morning\n"))
            .expect("create");
        assert_eq!(out.message, "Created Daily Notes/2026-09-25.md.");
        assert_eq!(out.reindex, ["Daily Notes/2026-09-25.md"]);

        // create is idempotent-friendly: an existing note is untouched.
        let out = w
            .daily_note(DailyAction::Create, Some("2026-09-25"), Some("other\n"))
            .expect("create again");
        assert!(out.message.contains("already exists"));
        assert_eq!(read(&w, "Daily Notes/2026-09-25.md"), "morning\n");

        // append joins with a blank line.
        w.daily_note(DailyAction::Append, Some("2026-09-25"), Some("evening\n"))
            .expect("append");
        assert_eq!(
            read(&w, "Daily Notes/2026-09-25.md"),
            "morning\n\nevening\n"
        );

        // get returns the content.
        let out = w
            .daily_note(DailyAction::Get, Some("2026-09-25"), None)
            .expect("get");
        assert!(out.message.contains("morning\n\nevening\n"));

        // append to a missing day creates it.
        w.daily_note(DailyAction::Append, Some("2026-09-26"), Some("fresh\n"))
            .expect("append create");
        assert_eq!(read(&w, "Daily Notes/2026-09-26.md"), "fresh\n");
    }

    #[test]
    fn daily_note_folder_and_format_resolution() {
        // Explicit config wins.
        let (w, _d) = writes_with(DailyNoteSettings {
            dir: Some("Journal".into()),
            format: Some("DD.MM.YYYY.md".into()),
        });
        w.daily_note(DailyAction::Create, Some("2026-09-25"), None)
            .expect("create");
        assert!(w.vault.read("Journal/25.09.2026.md").is_ok());

        // .obsidian/daily-notes.json is honored over conventions.
        let (w, d) = writes();
        std::fs::create_dir_all(d.path().join(".obsidian")).expect("mkdir");
        std::fs::write(
            d.path().join(".obsidian/daily-notes.json"),
            r#"{"folder": "My Days", "format": "YYYY/MM-DD.md"}"#,
        )
        .expect("config");
        w.daily_note(DailyAction::Create, Some("2026-09-25"), None)
            .expect("create");
        assert!(w.vault.read("My Days/2026/09-25.md").is_ok());

        // An existing conventional folder is picked when unconfigured.
        let (w, d) = writes();
        std::fs::create_dir_all(d.path().join("Journal")).expect("mkdir");
        w.daily_note(DailyAction::Create, Some("2026-09-25"), None)
            .expect("create");
        assert!(w.vault.read("Journal/2026-09-25.md").is_ok());
    }

    #[test]
    fn daily_note_validates_date_and_content() {
        let (w, _d) = writes();
        for bad in [
            "2026-13-01",
            "2026-00-10",
            "2026-09-32",
            "2026-02-30",
            "26-09-25",
            "2026-9-5",
            "garbage",
        ] {
            assert!(
                w.daily_note(DailyAction::Get, Some(bad), None).is_err(),
                "should reject {bad:?}"
            );
        }
        // Leap day is fine in a leap year, rejected otherwise.
        assert!(
            w.daily_note(DailyAction::Get, Some("2024-02-29"), None)
                .is_ok()
        );
        assert!(
            w.daily_note(DailyAction::Get, Some("2023-02-29"), None)
                .is_err()
        );
        // Append requires content.
        assert!(matches!(
            w.daily_note(DailyAction::Append, Some("2026-09-25"), None),
            Err(VaultError::InvalidPath(_))
        ));
        assert!(matches!(
            w.daily_note(DailyAction::Append, Some("2026-09-25"), Some("  ")),
            Err(VaultError::InvalidPath(_))
        ));
    }

    #[test]
    fn patch_rejects_non_utf8_notes() {
        let (w, d) = writes();
        std::fs::write(d.path().join("bin.md"), [0xff, 0xfe, 0x01]).expect("write");
        assert!(matches!(
            w.patch_note("bin.md", PatchOp::Append, "x", None),
            Err(VaultError::NotUtf8(_))
        ));
    }

    #[test]
    fn date_helpers() {
        assert_eq!(
            parse_date("2026-09-25"),
            Some(Date {
                year: 2026,
                month: 9,
                day: 25
            })
        );
        assert_eq!(
            parse_date(" 2026-09-25 "),
            Some(Date {
                year: 2026,
                month: 9,
                day: 25
            })
        );
        assert_eq!(
            parse_date("2024-02-29"),
            Some(Date {
                year: 2024,
                month: 2,
                day: 29
            })
        );
        assert_eq!(parse_date("2026-02-29"), None);
        assert_eq!(parse_date(""), None);

        // Known epoch-day conversions (computed by hand from the Unix
        // epoch): 0 = 1970-01-01, 10000 = 1997-05-19, 20721 = 2026-09-25.
        assert_eq!(
            civil_from_days(0),
            Date {
                year: 1970,
                month: 1,
                day: 1
            }
        );
        assert_eq!(
            civil_from_days(10_000),
            Date {
                year: 1997,
                month: 5,
                day: 19
            }
        );
        assert_eq!(
            civil_from_days(20_721),
            Date {
                year: 2026,
                month: 9,
                day: 25
            }
        );
        assert_eq!(
            Date {
                year: 2026,
                month: 9,
                day: 25
            }
            .to_string(),
            "2026-09-25"
        );
    }

    #[test]
    fn relative_and_encoded_paths() {
        assert_eq!(relative_path("", "a/b.md"), "a/b.md");
        assert_eq!(relative_path("a", "a/b.md"), "b.md");
        assert_eq!(relative_path("a/b", "c/d.md"), "../../c/d.md");
        assert_eq!(relative_path("a", "a/b/c.md"), "b/c.md");
        assert_eq!(relative_path("a/b", "a/c.md"), "../c.md");

        assert_eq!(percent_encode_path("a/b.md"), "a/b.md");
        assert_eq!(
            percent_encode_path("My Notes/Note (1).md"),
            "My%20Notes/Note%20%281%29.md"
        );
        assert_eq!(percent_encode_path("café.md"), "caf%C3%A9.md");
    }

    #[test]
    fn block_joining_and_splice() {
        assert_eq!(join_blocks("", "b"), "b\n");
        assert_eq!(join_blocks("a", ""), "a\n");
        assert_eq!(join_blocks("a", "b"), "a\n\nb\n");
        assert_eq!(join_blocks("\na\n\n", "\nb\n"), "a\n\nb\n");
        assert_eq!(ensure_trailing_newline("x"), "x\n");
        assert_eq!(ensure_trailing_newline("x\n"), "x\n");

        // Overlapping edits keep the earlier one; the rest splices in order.
        let spliced = splice("abcdef", vec![(3..4, "XY".into()), (0..2, "Z".into())]);
        assert_eq!(spliced, "ZcXYef");
    }

    #[test]
    fn frontmatter_normalization() {
        assert_eq!(normalize_frontmatter("---\ntitle: X\n---"), "title: X");
        assert_eq!(normalize_frontmatter("title: X\n"), "title: X");
        assert_eq!(normalize_frontmatter("  \n"), "");
    }
}
