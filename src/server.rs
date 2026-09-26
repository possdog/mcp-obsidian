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
use crate::vault::Vault;
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
    /// Only list notes whose vault-relative path starts with this prefix.
    prefix: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[serde(default)]
struct ReadNoteArgs {
    /// Vault-relative path of the note (e.g. "journal/2026-09-25.md").
    path: String,
    /// Optional heading: return only that section instead of the whole note
    /// (case-insensitive).
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
    /// Vault-relative path of the note to create or replace.
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

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[serde(default)]
struct PatchNoteArgs {
    /// Vault-relative path of the note to edit.
    path: String,
    /// One of: "append", "prepend", "insert_at_heading",
    /// "replace_section".
    operation: String,
    /// The text to add. May be empty only for "replace_section", which
    /// then clears the section's content (the heading line is kept).
    content: String,
    /// Target heading (case-insensitive); required for
    /// "insert_at_heading" and "replace_section".
    heading: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[serde(default)]
struct MoveNoteArgs {
    /// Current vault-relative path of the note (or file).
    from: String,
    /// New vault-relative path.
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
    /// Vault-relative path of the note to delete.
    path: String,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[serde(default)]
struct DailyNoteArgs {
    /// One of: "get" (default), "create", "append".
    action: Option<String>,
    /// The day as YYYY-MM-DD. Defaults to today (UTC).
    date: Option<String>,
    /// Note content for "create"; the text to append for "append"
    /// (required for append).
    content: Option<String>,
}

#[tool_router(server_handler)]
impl ObsidianHandler {
    #[tool(
        name = "list_notes",
        description = "List notes in the vault with size and modification time, optionally under a path prefix."
    )]
    async fn list_notes(
        &self,
        Parameters(args): Parameters<ListNotesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let entries = match self.ctx.notes.list(args.prefix.as_deref()) {
            Ok(entries) => entries,
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
        description = "Read a note's full content (frontmatter included), or only one heading's section."
    )]
    async fn read_note(
        &self,
        Parameters(args): Parameters<ReadNoteArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let read = match self.ctx.notes.read(&args.path, args.heading.as_deref()) {
            Ok(read) => read,
            Err(e) => return tool_error(e),
        };
        let mut out = String::new();
        if let Some(frontmatter) = read.frontmatter {
            let _ = writeln!(out, "---\n{frontmatter}\n---");
        }
        out.push_str(&read.body);
        text(out)
    }

    #[tool(
        name = "get_backlinks",
        description = "List notes that link to a given note."
    )]
    async fn get_backlinks(
        &self,
        Parameters(args): Parameters<LinkQueryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let index = match self.ctx.notes.link_index() {
            Ok(index) => index,
            Err(e) => return tool_error(e),
        };
        let key = match note_key(&self.ctx.notes, &args.path) {
            Some(key) => key,
            None => return tool_error(format!("note not found: {}", args.path)),
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
        description = "List the links a given note points to."
    )]
    async fn get_outlinks(
        &self,
        Parameters(args): Parameters<LinkQueryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let index = match self.ctx.notes.link_index() {
            Ok(index) => index,
            Err(e) => return tool_error(e),
        };
        let key = match note_key(&self.ctx.notes, &args.path) {
            Some(key) => key,
            None => return tool_error(format!("note not found: {}", args.path)),
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
        description = "List all tags in the vault with the number of notes carrying each."
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
        description = "Full-text search across the vault (note bodies, titles, aliases, tags, and frontmatter). Returns matching paths ranked by relevance with snippets."
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
        description = "Create or overwrite a note, optionally with YAML frontmatter. Refuses to replace an existing note unless overwrite is true."
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
        description = "Edit part of a note: append, prepend, insert under a heading, or replace a heading's section. Existing frontmatter is preserved untouched."
    )]
    async fn patch_note(
        &self,
        Parameters(args): Parameters<PatchNoteArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let operation = match args.operation.as_str() {
            "append" => PatchOp::Append,
            "prepend" => PatchOp::Prepend,
            "insert_at_heading" => PatchOp::InsertAtHeading,
            "replace_section" => PatchOp::ReplaceSection,
            other => {
                return tool_error(format!(
                    "unknown operation {other:?}; expected append, prepend, insert_at_heading, or replace_section"
                ));
            }
        };
        let outcome = match self.ctx.writes.patch_note(
            &args.path,
            operation,
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
        description = "Move or rename a note (or any vault file), optionally updating wikilinks and markdown links across the vault that pointed at the old path."
    )]
    async fn move_note(
        &self,
        Parameters(args): Parameters<MoveNoteArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let outcome = match self.ctx.writes.move_note(
            &args.from,
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
        description = "Delete a note by moving it to the vault's .trash folder. Recoverable; nothing is permanently removed."
    )]
    async fn delete_note(
        &self,
        Parameters(args): Parameters<DeleteNoteArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let outcome = match self.ctx.writes.delete_note(&args.path) {
            Ok(outcome) => outcome,
            Err(e) => return tool_error(e),
        };
        commit_write(&self.ctx, outcome)
    }

    #[tool(
        name = "daily_note",
        description = "Work with daily notes: read the day's note, create it, or append to it (creating it when missing). The folder and filename format follow the vault's own daily-notes config."
    )]
    async fn daily_note(
        &self,
        Parameters(args): Parameters<DailyNoteArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let action = match args.action.as_deref().unwrap_or("get") {
            "get" => DailyAction::Get,
            "create" => DailyAction::Create,
            "append" => DailyAction::Append,
            other => {
                return tool_error(format!(
                    "unknown action {other:?}; expected get, create, or append"
                ));
            }
        };
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

/// Resolves a tool-supplied path to a link-graph key: the exact path if it
/// exists, otherwise the `.md`-appended variant. The link graph keys notes
/// by their real vault paths, but agents habitually say `read_note`-style
/// bare names ("Foo" for "Foo.md"), so the tools accept both.
fn note_key(notes: &Notes, path: &str) -> Option<String> {
    if notes.vault().read(path).is_ok() {
        return Some(path.to_string());
    }
    if !path.ends_with(".md") {
        let with_ext = format!("{path}.md");
        if notes.vault().read(&with_ext).is_ok() {
            return Some(with_ext);
        }
    }
    None
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
    fn note_key_accepts_bare_name_and_full_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("Foo.md"), "# Foo").expect("write");
        let notes = Notes::open(Vault::open(dir.path()).expect("vault"));
        assert_eq!(note_key(&notes, "Foo"), Some("Foo.md".to_string()));
        assert_eq!(note_key(&notes, "Foo.md"), Some("Foo.md".to_string()));
        assert_eq!(note_key(&notes, "Missing"), None);
    }
}
