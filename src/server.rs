//! MCP server: rmcp tool handler over the streamable HTTP transport, guarded
//! by a bearer-token auth middleware.
//!
//! The rmcp-provided [`StreamableHttpService`] is a tower service; it is
//! nested into an axum router at `/mcp` (rmcp's own integration tests use the
//! same pattern). [`bearer_auth`] wraps every request to `/mcp` with a
//! constant-time `Authorization: Bearer` check, so remote clients
//! authenticate before any MCP traffic — including the SSE stream.
//!
//! Tool handlers are thin: they render the services from [`crate::notes`],
//! [`crate::sync`], and (in later phases) [`crate::vault`] writes into text
//! content, capped at [`MAX_TOOL_OUTPUT`]. Per rmcp's guidance, expected
//! failures (note not found, bad path) are returned as tool-level error
//! results so the calling model sees the message; protocol errors are
//! reserved for infrastructure faults.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, schemars, tool, tool_router};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::notes::Notes;
use crate::search::SearchIndex;
use crate::sync::{self, SyncSupervisor};
use crate::vault::{Vault, VaultError};
use crate::writes::{DailyAction, DailyNoteSettings, PatchOp, WriteOutcome, Writes};

/// Cap on a tool's text response, protecting client context windows from
/// pathologically large vaults and CLI output.
pub const MAX_TOOL_OUTPUT: usize = 256 * 1024;

/// Shared state behind every MCP session, cheap to clone into the per-session
/// handler instances that [`StreamableHttpService::new`] creates from its
/// factory.
#[derive(Clone)]
pub struct ServerContext {
    notes: Arc<Notes>,
    writes: Arc<Writes>,
    search: Arc<SearchIndex>,
    ob_bin: Arc<str>,
    supervisor: Arc<SyncSupervisor>,
}

impl ServerContext {
    pub fn new(
        vault: Vault,
        search: Arc<SearchIndex>,
        daily: DailyNoteSettings,
        ob_bin: impl Into<Arc<str>>,
        supervisor: Arc<SyncSupervisor>,
    ) -> Self {
        Self {
            notes: Arc::new(Notes::open(vault.clone())),
            writes: Arc::new(Writes::open(vault, daily)),
            search,
            ob_bin: ob_bin.into(),
            supervisor,
        }
    }
}

/// Builds the full axum router: the MCP streamable-HTTP service at `/mcp`,
/// wrapped in the bearer-token middleware.
///
/// `allowed_hosts` configures rmcp's inbound `Host`-header validation. When
/// `None` it is disabled: this server is designed to sit behind a TLS
/// reverse proxy that forwards the public `Host`, and every request already
/// requires the bearer token. Pass explicit hosts to re-enable the
/// DNS-rebinding protection for direct exposure.
pub fn router(
    ctx: ServerContext,
    auth_token: &str,
    cancellation: CancellationToken,
    allowed_hosts: Option<Vec<String>>,
) -> Router {
    let service = mcp_service(ctx, cancellation, allowed_hosts);
    let token = AuthToken::new(auth_token);
    Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn_with_state(token, bearer_auth))
}

fn mcp_service(
    ctx: ServerContext,
    cancellation: CancellationToken,
    allowed_hosts: Option<Vec<String>>,
) -> StreamableHttpService<ObsidianHandler, LocalSessionManager> {
    let config = match allowed_hosts {
        Some(hosts) => StreamableHttpServerConfig::default().with_allowed_hosts(hosts),
        None => StreamableHttpServerConfig::default().disable_allowed_hosts(),
    }
    .with_cancellation_token(cancellation.child_token());
    StreamableHttpService::new(
        // One handler instance per MCP session; ServerContext is Arc-backed,
        // so this is a handful of pointer copies.
        move || Ok(ObsidianHandler { ctx: ctx.clone() }),
        LocalSessionManager::default().into(),
        config,
    )
}

/// The MCP tool handler. `#[tool_router(server_handler)]` generates the
/// router from the `#[tool]` methods below and emits the full
/// `ServerHandler` implementation.
#[derive(Clone)]
struct ObsidianHandler {
    ctx: ServerContext,
}

// Argument structs. Doc comments become the JSON-schema field descriptions
// the model client sees; every field is optional-or-defaulted so clients can
// omit arguments.

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[serde(default)]
struct ListNotesArgs {
    /// Only list files whose vault-relative path starts with this prefix
    /// (case-sensitive, unlike search_notes' prefix).
    prefix: Option<String>,
    /// Maximum number of files to return (1-1000). On large vaults the
    /// full listing is truncated at 256 KiB; results are sorted by path,
    /// so combine this with prefix to page.
    limit: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[serde(default)]
struct ReadNoteArgs {
    /// Vault-relative path of the note, or a bare note name (".md" is
    /// optional), e.g. "journal/2026-09-25.md" or "Journal/2026-09-25".
    path: String,
    /// Return only this heading's section: the heading line through any
    /// nested subsections, up to the next heading of the same or higher
    /// level (case-insensitive). Frontmatter is not included.
    heading: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[serde(default)]
struct LinkQueryArgs {
    /// Vault-relative path or bare note name (".md" is optional).
    path: String,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[serde(default)]
struct SearchNotesArgs {
    /// Search query. Bare terms are AND-combined; "quoted phrases",
    /// field:term (title:, aliases:, tags:, body:, frontmatter_raw:),
    /// -excluded, and * (all notes) are also supported.
    query: String,
    /// Only return notes whose vault-relative path starts with this
    /// prefix (case-insensitive), e.g. "journal/2026".
    prefix: Option<String>,
    /// Maximum number of results to return (1-100, default 20).
    limit: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[serde(default)]
struct WriteNoteArgs {
    /// Vault-relative path of the note to create or replace, including
    /// the extension (e.g. "ideas/new-idea.md").
    path: String,
    /// Full note content. When `frontmatter` is given, this is the body
    /// only — anything frontmatter-shaped in it would land in the body.
    content: String,
    /// Optional YAML frontmatter for the note, without the `---` fences
    /// (fenced input is accepted and normalized).
    /// Example: "title: My Note\ntags: [journal, idea]"
    frontmatter: Option<String>,
    /// Replace the note if it already exists (default: refuse, so a
    /// mistaken whole-file write cannot destroy existing work).
    overwrite: Option<bool>,
}

// No container-level `serde(default)`: `operation`, `path`, and `content`
// are required, and rmcp turns a missing field into a tool-level error
// naming it.
#[derive(Deserialize, schemars::JsonSchema)]
struct PatchNoteArgs {
    /// Vault-relative path of the note to edit, or a bare note name
    /// (".md" is optional).
    path: String,
    /// How to modify the note.
    operation: PatchOp,
    /// The text to add, or the section's new content for
    /// "replace_section" (empty content clears the section, keeping the
    /// heading line).
    content: String,
    /// Target heading (case-insensitive); required for
    /// "insert_at_heading" and "replace_section".
    heading: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[serde(default)]
struct MoveNoteArgs {
    /// Current vault-relative path of the note (or file); a bare note
    /// name also works (".md" is optional).
    from: String,
    /// New vault-relative path, exactly as it should be written,
    /// including the file extension.
    to: String,
    /// Replace the destination if it already exists (default: refuse).
    overwrite: Option<bool>,
    /// Rewrite wikilinks and markdown links across the vault that
    /// pointed at the old path (default: true), like Obsidian's own
    /// rename behavior.
    update_links: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[serde(default)]
struct DeleteNoteArgs {
    /// Vault-relative path of the note to delete, or a bare note name
    /// (".md" is optional).
    path: String,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[serde(default)]
struct DailyNoteArgs {
    /// What to do with the day's note (default: "get").
    action: Option<DailyAction>,
    /// The day as YYYY-MM-DD. Defaults to today (UTC); pass an explicit
    /// date when the exact day matters.
    date: Option<String>,
    /// Initial content for "create"; the text to append for "append"
    /// (required for append).
    content: Option<String>,
}

#[tool_router(server_handler)]
impl ObsidianHandler {
    #[tool(
        name = "list_notes",
        description = "List every file in the vault (notes and attachments), sorted by vault-relative path, one \"- path (bytes, mtime)\" line each. prefix narrows to a folder; use limit to page large vaults — a full listing is truncated at 256 KiB."
    )]
    async fn list_notes(
        &self,
        Parameters(args): Parameters<ListNotesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // Clamp rather than reject: absurd limits are a modeling artifact,
        // not a user decision worth a round trip.
        let limit = args.limit.map(|l| l.clamp(1, 1000));
        let entries = match self.ctx.notes.list(args.prefix.as_deref()) {
            Ok(entries) => match limit {
                Some(l) => entries.into_iter().take(l).collect(),
                None => entries,
            },
            Err(e) => return tool_error(e),
        };
        let mut out = String::new();
        for entry in entries {
            // Epoch seconds avoid a time-formatting dependency; clients
            // rarely need humanized mtimes.
            let mtime = entry
                .modified
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let _ = writeln!(
                out,
                "- {} ({} bytes, mtime {})",
                entry.path, entry.size, mtime
            );
        }
        if out.is_empty() {
            return text("No notes found.");
        }
        text(out)
    }

    #[tool(
        name = "read_note",
        description = "Read a note's full content (frontmatter included), or only one heading's section (the heading line through its subsections, up to the next heading of the same or higher level; frontmatter excluded). Accepts a vault-relative path or a bare note name (\".md\" optional); heading match is case-insensitive."
    )]
    async fn read_note(
        &self,
        Parameters(args): Parameters<ReadNoteArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let path = match resolve_note_path(self.ctx.notes.vault(), &args.path) {
            Ok(path) => path,
            Err(e) => return tool_error(e),
        };
        let read = match self.ctx.notes.read(&path, args.heading.as_deref()) {
            Ok(read) => read,
            Err(e) => return tool_error(e),
        };
        let mut out = String::new();
        // The frontmatter block from Notes::read already carries its `---`
        // fences (and trailing newline); push it verbatim or the fences end
        // up doubled.
        if let Some(frontmatter) = read.frontmatter {
            out.push_str(&frontmatter);
        }
        out.push_str(&read.body);
        text(out)
    }

    #[tool(
        name = "get_backlinks",
        description = "List the notes that link to a given note, one \"- source path (link display text)\" line per linking note. Accepts a bare note name (\".md\" optional)."
    )]
    async fn get_backlinks(
        &self,
        Parameters(args): Parameters<LinkQueryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let index = match self.ctx.notes.link_index() {
            Ok(index) => index,
            Err(e) => return tool_error(e),
        };
        let key = match resolve_note_path(self.ctx.notes.vault(), &args.path) {
            Ok(key) => key,
            Err(e) => return tool_error(e),
        };
        let backlinks = index.backlinks(&key);
        if backlinks.is_empty() {
            return text(format!("No notes link to {key}."));
        }
        let mut out = String::new();
        for link in backlinks {
            let _ = writeln!(out, "- {} ({})", link.source, link.display);
        }
        text(out)
    }

    #[tool(
        name = "get_outlinks",
        description = "List the links a given note points to, one \"- target path [link|embed] (display text)\" line per link. Accepts a bare note name (\".md\" optional)."
    )]
    async fn get_outlinks(
        &self,
        Parameters(args): Parameters<LinkQueryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let index = match self.ctx.notes.link_index() {
            Ok(index) => index,
            Err(e) => return tool_error(e),
        };
        let key = match resolve_note_path(self.ctx.notes.vault(), &args.path) {
            Ok(key) => key,
            Err(e) => return tool_error(e),
        };
        let outlinks = index.outlinks(&key);
        if outlinks.is_empty() {
            return text(format!("{key} has no outgoing links."));
        }
        let mut out = String::new();
        for link in outlinks {
            let kind = if link.embed { "embed" } else { "link" };
            let _ = writeln!(out, "- {} [{kind}] ({})", link.target, link.display);
        }
        text(out)
    }

    #[tool(
        name = "list_tags",
        description = "List all tags in the vault (frontmatter and body) with the number of notes carrying each, sorted by count then name."
    )]
    async fn list_tags(&self) -> Result<CallToolResult, ErrorData> {
        let tags = match self.ctx.notes.list_tags() {
            Ok(tags) => tags,
            Err(e) => return tool_error(e),
        };
        if tags.is_empty() {
            return text("No tags found.");
        }
        let mut out = String::new();
        for tag in tags {
            let _ = writeln!(out, "- #{} ({} notes)", tag.tag, tag.notes);
        }
        text(out)
    }

    #[tool(
        name = "search_notes",
        description = "Full-text search across the vault (note bodies, titles, aliases, tags, and frontmatter). Returns up to `limit` matches, ranked by relevance, as numbered entries with path, title, and a matching snippet — use read_note to open a hit."
    )]
    async fn search_notes(
        &self,
        Parameters(args): Parameters<SearchNotesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // Clamp rather than reject: absurd limits are a modeling artifact,
        // not a user decision worth a round trip.
        let limit = args.limit.unwrap_or(20).clamp(1, 100);
        let hits = match self
            .ctx
            .search
            .search(&args.query, args.prefix.as_deref(), limit)
        {
            Ok(hits) => hits,
            Err(e) => return tool_error(e),
        };
        if hits.is_empty() {
            return text("No notes match the query.");
        }
        let mut out = String::new();
        for (i, hit) in hits.iter().enumerate() {
            let _ = writeln!(out, "{}. {} (score {:.2})", i + 1, hit.path, hit.score);
            if !hit.title.is_empty() {
                let _ = writeln!(out, "   {}", hit.title);
            }
            if let Some(snippet) = &hit.snippet
                && !snippet.is_empty()
            {
                let _ = writeln!(out, "   {}", snippet);
            }
        }
        text(out)
    }

    #[tool(
        name = "write_note",
        description = "Create a note, or replace an existing one when overwrite=true. overwrite=true replaces the ENTIRE note; to edit part of an existing note, prefer patch_note (it preserves frontmatter and untouched sections). For today's journal entry, prefer daily_note."
    )]
    async fn write_note(
        &self,
        Parameters(args): Parameters<WriteNoteArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let outcome = match self.ctx.writes.write_note(
            &args.path,
            &args.content,
            args.frontmatter.as_deref(),
            args.overwrite.unwrap_or(false),
        ) {
            Ok(outcome) => outcome,
            Err(e) => return tool_error(e),
        };
        commit_write(&self.ctx, outcome)
    }

    #[tool(
        name = "patch_note",
        description = "Edit part of an existing note — append, prepend, insert under a heading, or replace a heading's section — preserving frontmatter and untouched sections. Prefer this over write_note for edits: write_note with overwrite=true replaces the whole file."
    )]
    async fn patch_note(
        &self,
        Parameters(args): Parameters<PatchNoteArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let path = match resolve_note_path(self.ctx.notes.vault(), &args.path) {
            Ok(path) => path,
            Err(e) => return tool_error(e),
        };
        let outcome = match self.ctx.writes.patch_note(
            &path,
            args.operation,
            &args.content,
            args.heading.as_deref(),
        ) {
            Ok(outcome) => outcome,
            Err(e) => return tool_error(e),
        };
        commit_write(&self.ctx, outcome)
    }

    #[tool(
        name = "move_note",
        description = "Move or rename a note (or any vault file). By default, wikilinks and markdown links across the vault that pointed at the old path are rewritten, like Obsidian's rename behavior. `from` may be a bare note name (\".md\" optional); `to` is the exact new path including the extension."
    )]
    async fn move_note(
        &self,
        Parameters(args): Parameters<MoveNoteArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let from = match resolve_note_path(self.ctx.notes.vault(), &args.from) {
            Ok(from) => from,
            Err(e) => return tool_error(e),
        };
        let outcome = match self.ctx.writes.move_note(
            &from,
            &args.to,
            args.overwrite.unwrap_or(false),
            args.update_links.unwrap_or(true),
        ) {
            Ok(outcome) => outcome,
            Err(e) => return tool_error(e),
        };
        commit_write(&self.ctx, outcome)
    }

    #[tool(
        name = "delete_note",
        description = "Delete a note by moving it to the vault's .trash folder — recoverable, nothing is permanently removed. The result reports the trash path the note landed at. Accepts a bare note name (\".md\" optional)."
    )]
    async fn delete_note(
        &self,
        Parameters(args): Parameters<DeleteNoteArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let path = match resolve_note_path(self.ctx.notes.vault(), &args.path) {
            Ok(path) => path,
            Err(e) => return tool_error(e),
        };
        let outcome = match self.ctx.writes.delete_note(&path) {
            Ok(outcome) => outcome,
            Err(e) => return tool_error(e),
        };
        commit_write(&self.ctx, outcome)
    }

    #[tool(
        name = "daily_note",
        description = "Read, create, or append to the daily note for a date (default today, UTC): \"get\" reads it (reporting the path even when it does not exist yet), \"create\" creates it (an existing note is left untouched), \"append\" appends, creating the note when missing. The folder and filename format follow the vault's own daily-notes config."
    )]
    async fn daily_note(
        &self,
        Parameters(args): Parameters<DailyNoteArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let action = args.action.unwrap_or(DailyAction::Get);
        let outcome =
            match self
                .ctx
                .writes
                .daily_note(action, args.date.as_deref(), args.content.as_deref())
            {
                Ok(outcome) => outcome,
                Err(e) => return tool_error(e),
            };
        commit_write(&self.ctx, outcome)
    }

    #[tool(
        name = "sync_status",
        description = "Report Obsidian Headless sync status: daemon supervision state plus `ob sync-status` output."
    )]
    async fn sync_status(&self) -> Result<CallToolResult, ErrorData> {
        let state = self.ctx.supervisor.state();
        let outcome = sync::sync_status(&self.ctx.ob_bin, self.ctx.notes.vault().root()).await;
        text(format_supervision("sync-status", &state, &outcome))
    }

    #[tool(
        name = "sync_now",
        description = "Run a one-shot `ob sync` now, pushing local changes and pulling remote ones. Reports the command output."
    )]
    async fn sync_now(&self) -> Result<CallToolResult, ErrorData> {
        let state = self.ctx.supervisor.state();
        let outcome = sync::sync_now(&self.ctx.ob_bin, self.ctx.notes.vault().root()).await;
        text(format_supervision("sync", &state, &outcome))
    }
}

/// Finishes a successful write: refreshes the search index for the
/// affected paths and renders the outcome's message.
///
/// The write already succeeded, so an index failure is a degraded-mode
/// warning appended to the message, not a tool error — the filesystem
/// watcher would also converge eventually, but doing it here makes a
/// write-then-search sequence deterministic without waiting out the
/// debounce window.
fn commit_write(ctx: &ServerContext, outcome: WriteOutcome) -> Result<CallToolResult, ErrorData> {
    let mut message = outcome.message;
    if !outcome.reindex.is_empty()
        && let Err(e) = ctx
            .search
            .reindex_paths(ctx.notes.vault(), &outcome.reindex)
    {
        let _ = writeln!(message, "[warning: search index update failed: {e}]");
    }
    text(message)
}

/// Renders supervision state plus one-shot `ob` output. The raw CLI text is
/// kept as-is: Headless is an open beta and its output format is not a
/// stable contract (see PLAN.md).
fn format_supervision(
    command: &str,
    state: &crate::sync::SupervisorState,
    outcome: &sync::SyncRunOutcome,
) -> String {
    let mut out = String::new();
    let running = if state.running { "running" } else { "stopped" };
    let _ = writeln!(out, "daemon: {running} (restarts: {})", state.restarts);
    if let Some(error) = &state.last_error {
        let _ = writeln!(out, "daemon error: {error}");
    }
    if let Some(exit) = state.last_exit_code {
        let _ = writeln!(out, "last daemon exit: {exit}");
    }
    let _ = writeln!(
        out,
        "ob {command}: {}",
        match (outcome.success, outcome.timed_out) {
            (true, _) => "ok".to_string(),
            (false, true) => "timed out".to_string(),
            (false, false) => format!("failed (exit {:?})", outcome.exit_code),
        }
    );
    out.push_str(outcome.best_output());
    out
}

/// Resolves a tool-supplied note reference to the real vault path: the
/// exact path if it exists, otherwise the `.md`-appended variant. Agents
/// habitually pass bare names ("Foo" for "Foo.md"), so every tool that
/// addresses an existing note accepts both — one contract across the
/// tool set instead of per-tool quirks.
///
/// A missing note is reported with a discovery hint (list_notes /
/// search_notes) so the caller can recover in one step; sandboxed-path
/// errors (traversal, dot-folders) propagate unchanged — only a genuine
/// miss gets the hint.
fn resolve_note_path(vault: &Vault, path: &str) -> std::result::Result<String, VaultError> {
    match vault.read(path) {
        Ok(_) => Ok(path.to_string()),
        Err(VaultError::NotFound(_)) => {
            if !path.ends_with(".md") {
                let with_ext = format!("{path}.md");
                if vault.read(&with_ext).is_ok() {
                    return Ok(with_ext);
                }
            }
            Err(VaultError::NotFound(format!(
                "{path}; use list_notes or search_notes to find the right path"
            )))
        }
        Err(e) => Err(e),
    }
}

fn text(content: impl Into<String>) -> Result<CallToolResult, ErrorData> {
    let capped = cap_output(content.into());
    Ok(CallToolResult::success(vec![ContentBlock::text(capped)]))
}

/// Tool-level error: the message reaches the calling model (unlike a
/// protocol `Err(ErrorData)`, which clients render opaquely).
fn tool_error(message: impl std::fmt::Display) -> Result<CallToolResult, ErrorData> {
    let capped = cap_output(message.to_string());
    Ok(CallToolResult::error(vec![ContentBlock::text(capped)]))
}

/// Truncates tool output past [`MAX_TOOL_OUTPUT`] on a char boundary so the
/// lossy conversion never splits a multibyte character mid-way.
fn cap_output(content: String) -> String {
    if content.len() <= MAX_TOOL_OUTPUT {
        return content;
    }
    let mut end = MAX_TOOL_OUTPUT;
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = String::from(&content[..end]);
    let _ = write!(
        truncated,
        "\n[output truncated at {MAX_TOOL_OUTPUT} bytes of {}]",
        content.len()
    );
    truncated
}

/// The expected bearer token, kept as raw bytes for comparison.
#[derive(Clone)]
struct AuthToken(Arc<[u8]>);

impl AuthToken {
    fn new(token: &str) -> Self {
        Self(token.as_bytes().to_vec().into())
    }
}

/// Rejects any request without a valid `Authorization: Bearer <token>`
/// header. Applies to all methods on the MCP route, including the SSE GET.
async fn bearer_auth(State(expected): State<AuthToken>, req: Request, next: Next) -> Response {
    let provided = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_bearer);
    let authorized = provided.is_some_and(|token| constant_time_eq(token.as_bytes(), &expected.0));
    if !authorized {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            [("www-authenticate", "Bearer")],
            "unauthorized",
        )
            .into_response();
    }
    next.run(req).await
}

/// Splits an `Authorization` header into a bearer token. The scheme is
/// case-insensitive per RFC 6750; the token is returned as-is.
fn parse_bearer(header: &str) -> Option<&str> {
    let (scheme, token) = header.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token)
}

/// Constant-time equality over the token bytes. The length check is a plain
/// comparison: token length is not secret, and folding it into the loop
/// would only serve to leak timing on obviously-malformed input. The byte
/// comparison never short-circuits, so a probing attacker learns nothing
/// about how many leading bytes matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_and_rejects() {
        assert!(constant_time_eq(b"secret-token", b"secret-token"));
        assert!(!constant_time_eq(b"secret-token", b"secret-tokenx"));
        assert!(!constant_time_eq(b"secret-token", b"secret-tokem"));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn parse_bearer_accepts_case_insensitive_scheme() {
        assert_eq!(parse_bearer("Bearer abc"), Some("abc"));
        assert_eq!(parse_bearer("bearer abc"), Some("abc"));
        assert_eq!(parse_bearer("BEARER abc"), Some("abc"));
        assert_eq!(parse_bearer("Basic abc"), None);
        assert_eq!(parse_bearer("Bearer"), None);
        assert_eq!(parse_bearer(""), None);
    }

    #[test]
    fn cap_output_truncates_on_char_boundary() {
        let short = "hello".to_string();
        assert_eq!(cap_output(short.clone()), short);

        // 1 byte over the cap, ASCII: cut to exactly the cap.
        let long = "a".repeat(MAX_TOOL_OUTPUT + 1);
        let capped = cap_output(long);
        assert!(capped.starts_with(&"a".repeat(MAX_TOOL_OUTPUT - 1)));
        assert!(capped.contains("output truncated"));

        // Multibyte boundary: the cut must not split a UTF-8 sequence.
        let multibyte = "é".repeat(MAX_TOOL_OUTPUT);
        let capped = cap_output(multibyte);
        assert!(capped.contains("output truncated"));
        assert!(capped.chars().all(|c| c != '\u{fffd}'));
    }

    #[test]
    fn parse_bearer_rejects_malformed_authorization_values() {
        // Whitespace stays part of the token, so padded guesses ("Bearer
        // tok ") or doubled separators ("Bearer  tok") fail the equality
        // check rather than being trimmed into a match.
        assert_eq!(parse_bearer("Bearer "), Some(""));
        assert_eq!(parse_bearer("Bearer  tok"), Some(" tok"));
        assert_eq!(parse_bearer("Bearer tok extra"), Some("tok extra"));
        // Tab-separated values are not bearer headers at all.
        assert_eq!(parse_bearer("Bearer\ttok"), None);
        assert_eq!(parse_bearer(" Bearer tok"), None);
    }

    #[test]
    fn empty_bearer_token_never_authenticates() {
        let expected = AuthToken::new("real-token");
        for provided in ["", " ", "real-token ", " real-token", "REAL-TOKEN"] {
            assert!(
                !constant_time_eq(provided.as_bytes(), &expected.0),
                "{provided:?} must not authenticate"
            );
        }
    }

    #[test]
    fn cap_output_at_exact_boundary() {
        // Exactly at the cap: unchanged, no truncation marker.
        let exact = "x".repeat(MAX_TOOL_OUTPUT);
        let capped = cap_output(exact.clone());
        assert_eq!(capped, exact);
        // One byte over: the marker appears and the original size is
        // reported.
        let over = format!("{}y", exact);
        let capped = cap_output(over);
        assert!(capped.contains("output truncated"));
        assert!(capped.contains(&format!("of {}", MAX_TOOL_OUTPUT + 1)));
    }

    #[test]
    fn format_supervision_renders_state_and_outcome() {
        use crate::sync::{SupervisorState, SyncRunOutcome};

        // Degraded: daemon up but with history, one-shot command timed
        // out; stdout empty, so stderr is the best output.
        let state = SupervisorState {
            running: true,
            restarts: 2,
            last_exit_code: Some(1),
            last_error: Some("ob exploded".into()),
            ..SupervisorState::default()
        };
        let outcome = SyncRunOutcome {
            success: false,
            exit_code: None,
            timed_out: true,
            stdout: String::new(),
            stderr: "gave up waiting".into(),
        };
        let text = format_supervision("sync", &state, &outcome);
        assert!(
            text.contains("daemon: running (restarts: 2)"),
            "got: {text}"
        );
        assert!(text.contains("daemon error: ob exploded"), "got: {text}");
        assert!(text.contains("last daemon exit: 1"), "got: {text}");
        assert!(text.contains("ob sync: timed out"), "got: {text}");
        assert!(text.contains("gave up waiting"), "got: {text}");

        // Healthy: stopped daemon, ok one-shot, stdout preferred over
        // stderr.
        let state = SupervisorState::default();
        let outcome = SyncRunOutcome {
            success: true,
            exit_code: Some(0),
            timed_out: false,
            stdout: "all synced".into(),
            stderr: "ignored".into(),
        };
        let text = format_supervision("sync-status", &state, &outcome);
        assert!(
            text.contains("daemon: stopped (restarts: 0)"),
            "got: {text}"
        );
        assert!(text.contains("ob sync-status: ok"), "got: {text}");
        assert!(text.contains("all synced"), "got: {text}");
        assert!(!text.contains("ignored"), "got: {text}");
    }

    #[test]
    fn resolve_note_path_accepts_bare_name_and_full_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("Foo.md"), "# Foo").expect("write");
        let vault = Vault::open(dir.path()).expect("vault");
        assert_eq!(
            resolve_note_path(&vault, "Foo").expect("bare name"),
            "Foo.md"
        );
        assert_eq!(
            resolve_note_path(&vault, "Foo.md").expect("full path"),
            "Foo.md"
        );

        // A miss carries the discovery hint; a sandbox violation keeps
        // its real error instead of looking like a missing note.
        let err = resolve_note_path(&vault, "Missing").expect_err("missing");
        assert!(err.to_string().contains("list_notes"), "got: {err}");
        let err = resolve_note_path(&vault, ".obsidian/app.json").expect_err("forbidden");
        assert!(matches!(err, VaultError::Forbidden(_)), "got: {err}");
    }
}
