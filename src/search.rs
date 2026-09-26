//! Full-text search over the vault, backed by a tantivy index that is kept
//! fresh by a `notify` filesystem watcher.
//!
//! The index lives outside the vault by default (see
//! [`crate::config::Config::resolve_index_dir`]): Obsidian Sync replicates
//! dot-folders, so an in-vault index would sync to every device on every
//! write.
//!
//! Freshness model: [`SearchIndex::build_all`] wipes and rebuilds the index
//! at startup — after downtime, `ob` may have pulled changes with nobody
//! watching, so a full rebuild is the only correct state. From then on,
//! the watcher thread applies incremental updates (delete-by-path-term +
//! re-add) for live changes. Local writes and remote sync pulls both
//! surface as filesystem events, so one pipeline covers both sources.
//!
//! The reader uses [`ReloadPolicy::Manual`] and is reloaded immediately
//! after every commit this process makes, so searches always reflect all
//! committed updates without the tens-of-milliseconds lag of the
//! automatic policy (which would make freshness nondeterministic in
//! tests).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify::{Event, RecursiveMode, Watcher as _};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, RegexQuery};
use tantivy::schema::{FAST, Field, STORED, STRING, Schema, TEXT, Value as _};
use tantivy::snippet::SnippetGenerator;
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, Score, TantivyDocument, Term, doc};
use thiserror::Error;

use crate::markdown;
use crate::vault::Vault;

/// Per-writer indexing arena. One writer thread, so this is the total
/// writer memory footprint. Tantivy enforces a 15 MB minimum per thread,
/// so this sits just above it (PLAN.md's original 10 MB estimate is
/// below what the library accepts).
const INDEX_ARENA_BYTES: usize = 16 * 1024 * 1024;

/// Default quiet window for batching filesystem events into one index
/// commit. Exposed so `main` and tests can pick their own.
pub const WATCH_DEBOUNCE: Duration = Duration::from_secs(1);

/// Cap on how long a single batch of events can keep extending. A large
/// `ob sync` pull produces a continuous stream of events; without a cap,
/// "wait until quiet" would never fire and changes would never be indexed.
const WATCH_MAX_WINDOW: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
pub enum SearchError {
    #[error(transparent)]
    Tantivy(#[from] tantivy::TantivyError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("filesystem watcher error: {0}")]
    Watcher(#[from] notify::Error),
    #[error("invalid query: {0}")]
    Query(String),
    #[error("incompatible index: {0}")]
    Incompatible(String),
    #[error(transparent)]
    Vault(#[from] crate::vault::VaultError),
}

pub type Result<T> = std::result::Result<T, SearchError>;

/// One search result. `snippet` is the best-matching fragment of the body,
/// generated from the stored body field (missing only if the query matches
/// no body terms, e.g. a pure title/tag query).
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub path: String,
    pub title: String,
    pub score: Score,
    pub snippet: Option<String>,
}

/// Tantivy field handles for the fixed schema below.
#[derive(Debug, Clone, Copy)]
struct Fields {
    path: Field,
    path_lower: Field,
    title: Field,
    aliases: Field,
    tags: Field,
    frontmatter_raw: Field,
    body: Field,
}

/// The fixed index schema (see PLAN.md):
/// - `path` — the unique document key, needed for delete-by-term on
///   incremental updates; FAST so future features can filter on it cheaply.
/// - `path_lower` — lowercased copy for case-insensitive prefix filtering;
///   tantivy's regex queries run on fst automata and do not support the
///   `(?i)` flag, so case folding is done at indexing time instead.
/// - `title`, `aliases`, `tags` — Obsidian's structured search dimensions;
///   tags/aliases are added as one value per entry.
/// - `frontmatter_raw` — the verbatim YAML block, so arbitrary per-note
///   fields stay searchable without schema-per-vault gymnastics.
/// - `body` — the note body, stored so snippets can be generated without
///   re-reading the file (the indexed copy is always at least as fresh as
///   any snippet that could be read from disk).
fn build_schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field("path", STRING | FAST | STORED);
    builder.add_text_field("path_lower", STRING);
    builder.add_text_field("title", TEXT | STORED);
    builder.add_text_field("aliases", TEXT);
    builder.add_text_field("tags", TEXT);
    builder.add_text_field("frontmatter_raw", TEXT);
    builder.add_text_field("body", TEXT | STORED);
    builder.build()
}

/// Field handles for a schema built by [`build_schema`].
fn fields_of(schema: &Schema) -> Fields {
    // `expect` is fine: the schema is built two lines before with these
    // exact field names.
    Fields {
        path: schema.get_field("path").expect("schema field path"),
        path_lower: schema
            .get_field("path_lower")
            .expect("schema field path_lower"),
        title: schema.get_field("title").expect("schema field title"),
        aliases: schema.get_field("aliases").expect("schema field aliases"),
        tags: schema.get_field("tags").expect("schema field tags"),
        frontmatter_raw: schema
            .get_field("frontmatter_raw")
            .expect("schema field frontmatter_raw"),
        body: schema.get_field("body").expect("schema field body"),
    }
}

/// The open tantivy index plus its reader and writer.
///
/// The writer sits behind a [`Mutex`] because tantivy's `commit` needs
/// `&mut` while both the watcher thread and the MCP tools need `&self`
/// access. The lock is only held across the (batch, commit, reload) of a
/// single update, never during indexing of individual documents in a
/// multi-way contention scenario — one update wins, the next follows.
pub struct SearchIndex {
    index: Index,
    reader: IndexReader,
    writer: Mutex<IndexWriter<TantivyDocument>>,
    fields: Fields,
    /// Canonicalized index directory, used to exclude the index itself
    /// from watcher events when it is configured inside the vault.
    index_dir: PathBuf,
}

impl SearchIndex {
    /// Opens (or creates) the index at `index_dir`.
    ///
    /// An existing index is verified against the current code's schema; a
    /// mismatch (index built by a different server version) is an error
    /// for an administrator to resolve, not something to silently rebuild.
    pub fn open(index_dir: &Path) -> Result<Self> {
        let schema = build_schema();
        let index = if index_dir.join("meta.json").exists() {
            let existing = Index::open_in_dir(index_dir)?;
            if existing.schema() != schema {
                return Err(SearchError::Incompatible(format!(
                    "index at {} was built with a different schema; \
                     remove the directory to force a rebuild",
                    index_dir.display()
                )));
            }
            existing
        } else {
            std::fs::create_dir_all(index_dir)?;
            Index::create_in_dir(index_dir, schema.clone())?
        };

        let writer = index.writer(INDEX_ARENA_BYTES)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(Self {
            index,
            reader,
            writer: Mutex::new(writer),
            fields: fields_of(&schema),
            index_dir: index_dir.canonicalize()?,
        })
    }

    /// Wipes the index and re-adds every Markdown note. Returns the number
    /// of notes indexed. Called at startup (see module docs).
    pub fn build_all(&self, vault: &Vault) -> Result<usize> {
        let entries = vault.list(None)?;
        let mut writer = self.lock_writer();
        writer.delete_all_documents()?;
        let mut count = 0usize;
        for entry in &entries {
            if !entry.path.ends_with(".md") {
                continue;
            }
            // A note that vanished between listing and reading is not an
            // indexing failure; the watcher (or the next build) catches up.
            let Ok(raw) = vault.read(&entry.path) else {
                continue;
            };
            let content = String::from_utf8_lossy(&raw);
            writer.add_document(self.document_for(&entry.path, &content))?;
            count += 1;
        }
        writer.commit()?;
        self.reader.reload()?;
        Ok(count)
    }

    /// Incrementally updates the index for a set of vault-relative paths:
    /// each existing note is deleted and re-added with its current content,
    /// a missing note is only deleted. One commit for the whole batch.
    pub fn reindex_paths<P: AsRef<str>>(
        &self,
        vault: &Vault,
        paths: impl IntoIterator<Item = P>,
    ) -> Result<()> {
        let mut writer = self.lock_writer();
        for path in paths {
            let path = path.as_ref();
            writer.delete_term(Term::from_field_text(self.fields.path, path));
            // Only Markdown notes are indexed; other files (and paths the
            // sandbox rejects) simply drop out of the index.
            if path.ends_with(".md")
                && let Ok(raw) = vault.read(path)
            {
                let content = String::from_utf8_lossy(&raw);
                writer.add_document(self.document_for(path, &content))?;
            }
        }
        writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    /// Builds the index document for one note from its raw text.
    fn document_for(&self, path: &str, raw: &str) -> TantivyDocument {
        let (frontmatter, body) = markdown::split_frontmatter(raw);
        let yaml = frontmatter.and_then(markdown::parse_yaml);

        // Title: first H1, else the filename stem. Obsidian's search falls
        // back the same way.
        let title = markdown::parse_headings(body)
            .first()
            .map(|h| h.text.clone())
            .unwrap_or_else(|| {
                path.rsplit('/')
                    .next()
                    .unwrap_or(path)
                    .trim_end_matches(".md")
                    .to_string()
            });

        let mut document = doc!(
            self.fields.path => path,
            self.fields.path_lower => path.to_lowercase(),
            self.fields.title => title,
            self.fields.body => body,
        );
        if let Some(frontmatter) = frontmatter {
            document.add_field_value(self.fields.frontmatter_raw, frontmatter);
        }
        if let Some(yaml) = &yaml {
            for alias in markdown::frontmatter_string_list(yaml, "aliases") {
                document.add_field_value(self.fields.aliases, alias.as_str());
            }
            for tag in markdown::frontmatter_string_list(yaml, "tags") {
                document.add_field_value(self.fields.tags, tag.as_str());
            }
        }
        for tag in markdown::parse_body_tags(body) {
            document.add_field_value(self.fields.tags, tag.as_str());
        }
        document
    }

    /// Runs a query. `query` uses tantivy's syntax: bare terms (AND-combined
    /// by default), quoted phrases, `field:term`, `-excluded`, `*` for all
    /// notes. `path_prefix` restricts results to notes under a
    /// vault-relative path prefix (case-insensitive); `limit` bounds the
    /// number of hits.
    pub fn search(
        &self,
        query: &str,
        path_prefix: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let searcher = self.reader.searcher();
        let mut parser = QueryParser::for_index(
            &self.index,
            // `path` is excluded from the default fields: the explicit
            // prefix filter covers path narrowing, and matching on path
            // text would skew results for queries like "notes".
            vec![
                self.fields.title,
                self.fields.aliases,
                self.fields.tags,
                self.fields.frontmatter_raw,
                self.fields.body,
            ],
        );
        // Multi-term queries read better as AND for note search: "rust
        // error handling" should require all three concepts.
        parser.set_conjunction_by_default();
        // Title matches are what the user is usually looking for.
        parser.set_field_boost(self.fields.title, 2.0);
        let user_query = parser
            .parse_query(query)
            .map_err(|e| SearchError::Query(e.to_string()))?;

        // The prefix filter is a regex over the untokenized `path_lower`
        // field. tantivy regexes compile to fst automata: anchors (^/$)
        // are unsupported because matching is implicitly anchored, so a
        // prefix is expressed as `prefix.*`. Case folding happens at
        // indexing time (path_lower) rather than via a (?i) flag.
        let query: Box<dyn Query> = match path_prefix.filter(|p| !p.is_empty()) {
            Some(prefix) => Box::new(BooleanQuery::new(vec![
                (Occur::Must, user_query),
                (
                    Occur::Must,
                    Box::new(RegexQuery::from_pattern(
                        &format!("{}.*", regex::escape(&prefix.to_lowercase())),
                        self.fields.path_lower,
                    )?),
                ),
            ])),
            None => user_query,
        };

        let top = searcher.search(query.as_ref(), &TopDocs::with_limit(limit).order_by_score())?;
        let snippet_generator =
            SnippetGenerator::create(&searcher, query.as_ref(), self.fields.body).ok();
        let mut hits = Vec::with_capacity(top.len());
        for (score, address) in top {
            let document: TantivyDocument = searcher.doc(address)?;
            let field_str = |field: Field| {
                document
                    .get_first(field)
                    .and_then(|value| value.as_str().map(str::to_string))
            };
            hits.push(SearchHit {
                path: field_str(self.fields.path).unwrap_or_default(),
                title: field_str(self.fields.title).unwrap_or_default(),
                score,
                snippet: snippet_generator
                    .as_ref()
                    .map(|generator| generator.snippet_from_doc(&document))
                    .map(|snippet| snippet.fragment().trim().to_string()),
            });
        }
        Ok(hits)
    }

    /// Acquires the writer, recovering from a poisoned lock: an indexing
    /// panic must not take down every future tool call, and the tantivy
    /// writer itself is not left in an inconsistent state by our code
    /// (all mutations go through its own error-returning API).
    fn lock_writer(&self) -> std::sync::MutexGuard<'_, IndexWriter<TantivyDocument>> {
        self.writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Starts the filesystem watcher thread that keeps the index fresh.
    ///
    /// `debounce` is the quiet window that batches a burst of events into
    /// one commit. The returned handle stops the thread on [`WatcherHandle::stop`].
    pub fn spawn_watcher(
        self: &Arc<Self>,
        vault: &Vault,
        debounce: Duration,
    ) -> Result<WatcherHandle> {
        let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
        let mut watcher = notify::recommended_watcher(tx.clone())?;
        watcher.watch(vault.root(), RecursiveMode::Recursive)?;

        // If the index lives inside the vault (user-set INDEX_DIR), events
        // on the index itself must not re-enter the indexing pipeline.
        let in_vault_index_prefix = self
            .index_dir
            .strip_prefix(vault.root())
            .ok()
            .map(|rel| rel.to_string_lossy().into_owned());

        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let search = Arc::clone(self);
            let vault = vault.clone();
            let stop = Arc::clone(&stop);
            // The notify watcher is moved into the thread: dropping it is
            // what stops the OS-level watch.
            std::thread::Builder::new()
                .name("search-watcher".into())
                .spawn(move || {
                    let _watcher = watcher;
                    watch_loop(search, vault, in_vault_index_prefix, rx, debounce, &stop);
                })?
        };
        Ok(WatcherHandle {
            stop,
            tx,
            handle: Some(thread),
        })
    }
}

/// The watcher thread body: batch filesystem events until quiet, then
/// re-index the affected paths in one commit.
fn watch_loop(
    search: Arc<SearchIndex>,
    vault: Vault,
    in_vault_index_prefix: Option<String>,
    rx: Receiver<notify::Result<Event>>,
    debounce: Duration,
    stop: &AtomicBool,
) {
    loop {
        // Block until the first event; on timeout poll the stop flag.
        let first = match rx.recv_timeout(debounce) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        // `WatcherHandle::stop` wakes this recv with a dummy message;
        // checking here makes shutdown prompt instead of debounce-bound.
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let mut pending: BTreeSet<String> = BTreeSet::new();
        collect_event(first, vault.root(), &in_vault_index_prefix, &mut pending);

        // Keep consuming until the channel is quiet for one full window,
        // but never extend past WATCH_MAX_WINDOW from the first event (see
        // the constant's docs).
        let deadline = Instant::now() + WATCH_MAX_WINDOW;
        while Instant::now() < deadline {
            match rx.recv_timeout(debounce) {
                Ok(result) => {
                    collect_event(result, vault.root(), &in_vault_index_prefix, &mut pending);
                }
                // Quiet window elapsed: the burst is over.
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if let Err(error) = search.reindex_paths(&vault, &pending) {
            tracing::warn!(error = %error, notes = pending.len(), "incremental re-index failed");
        }
    }
}

/// Extracts the vault-relative note paths of interest from one watcher
/// event, skipping non-Markdown files, dot-folders (`.obsidian`, `.trash`,
/// `.git`), and — when applicable — the index directory itself.
fn collect_event(
    result: notify::Result<Event>,
    vault_root: &Path,
    in_vault_index_prefix: &Option<String>,
    pending: &mut BTreeSet<String>,
) {
    let event = match result {
        Ok(event) => event,
        Err(error) => {
            tracing::warn!(error = %error, "filesystem watcher error");
            return;
        }
    };
    for path in &event.paths {
        let Some(rel) = path
            .strip_prefix(vault_root)
            .ok()
            .map(|rel| rel.to_string_lossy().into_owned())
        else {
            continue;
        };
        if !rel.ends_with(".md") {
            continue;
        }
        if rel.split('/').any(|component| component.starts_with('.')) {
            continue;
        }
        if let Some(prefix) = in_vault_index_prefix
            && rel.starts_with(prefix.as_str())
        {
            continue;
        }
        pending.insert(rel);
    }
}

/// Handle to the watcher thread. Dropping it without calling
/// [`WatcherHandle::stop`] leaks the thread (it exits on the next stop
/// flag check at the latest); `stop` joins deterministically.
pub struct WatcherHandle {
    stop: Arc<AtomicBool>,
    /// Clone of the event channel's sender: `stop` pushes a dummy message
    /// so the thread's blocking `recv_timeout` returns immediately and
    /// observes the stop flag without waiting out the debounce window.
    tx: mpsc::Sender<notify::Result<Event>>,
    handle: Option<JoinHandle<()>>,
}

impl WatcherHandle {
    /// Signals the watcher to stop and waits for the thread to exit. The
    /// dummy message makes the join prompt even with a long debounce;
    /// the stop flag is still checked on every receive, so a dropped or
    /// full channel cannot hang shutdown.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Err(notify::Error::generic("stopping")));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::Vault;

    /// Fixture: a vault plus an independent index directory (kept outside
    /// the vault, matching the production default).
    fn fixture() -> (
        Vault,
        tempfile::TempDir,
        Arc<SearchIndex>,
        tempfile::TempDir,
    ) {
        let vault_dir = tempfile::tempdir().expect("vault tempdir");
        let vault = Vault::open(vault_dir.path()).expect("vault opens");
        let index_dir = tempfile::tempdir().expect("index tempdir");
        let index = Arc::new(SearchIndex::open(index_dir.path()).expect("index opens"));
        (vault, vault_dir, index, index_dir)
    }

    fn seeded_index() -> (
        Vault,
        tempfile::TempDir,
        Arc<SearchIndex>,
        tempfile::TempDir,
    ) {
        let (vault, vault_dir, index, index_dir) = fixture();
        std::fs::create_dir_all(vault.root().join("posts")).expect("mkdir posts");
        std::fs::write(
            vault.root().join("posts/rust.md"),
            "---\naliases: [raust]\ntags: [lang]\nauthor: Reilly\n---\n\
             # Rust notes\n\nThe xylophone zebra essay.\n\nSee #bodytag.\n",
        )
        .expect("write rust.md");
        std::fs::write(
            vault.root().join("posts/other.md"),
            "# Other\n\nzebra only\n",
        )
        .expect("write other.md");
        std::fs::write(vault.root().join("plain.txt"), "xylophone but not a note")
            .expect("write plain.txt");
        index.build_all(&vault).expect("build succeeds");
        (vault, vault_dir, index, index_dir)
    }

    fn paths(index: &SearchIndex, query: &str) -> Vec<String> {
        index
            .search(query, None, 10)
            .expect("search succeeds")
            .into_iter()
            .map(|hit| hit.path)
            .collect()
    }

    #[test]
    fn search_covers_all_schema_fields() {
        let (_vault, _v, index, _i) = seeded_index();

        // Body, title (H1), frontmatter alias, frontmatter tag, body tag,
        // and raw frontmatter text are all searchable.
        assert_eq!(paths(&index, "xylophone"), ["posts/rust.md"]);
        assert_eq!(paths(&index, "\"rust notes\""), ["posts/rust.md"]);
        assert_eq!(paths(&index, "raust"), ["posts/rust.md"]);
        assert_eq!(paths(&index, "lang"), ["posts/rust.md"]);
        assert_eq!(paths(&index, "bodytag"), ["posts/rust.md"]);
        assert_eq!(paths(&index, "reilly"), ["posts/rust.md"]);

        // Non-Markdown files never enter the index.
        assert!(paths(&index, "not a note").is_empty());

        // Snippets come from the stored body field; the title from the H1.
        let hits = index.search("xylophone", None, 10).expect("search");
        assert!(
            hits[0]
                .snippet
                .as_deref()
                .is_some_and(|s| s.contains("xylophone"))
        );
        assert_eq!(hits[0].title, "Rust notes");
    }

    #[test]
    fn title_falls_back_to_file_stem() {
        let (vault, _v, index, _i) = fixture();
        std::fs::write(vault.root().join("no-heading.md"), "no heading here\n").expect("write");
        index.build_all(&vault).expect("build");
        let hits = index.search("heading", None, 10).expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "no-heading");
    }

    #[test]
    fn conjunction_phrases_and_exclusions() {
        let (vault, _v, index, _i) = seeded_index();
        std::fs::write(vault.root().join("posts/both.md"), "xylophone zebra\n").expect("write");
        index
            .reindex_paths(&vault, ["posts/both.md"])
            .expect("reindex");

        // Bare terms are AND-combined by default: other.md has only
        // "zebra" and is excluded, while rust.md has both terms.
        let mut both_terms = paths(&index, "xylophone zebra");
        both_terms.sort();
        assert_eq!(both_terms, ["posts/both.md", "posts/rust.md"]);
        // Quoted terms are a phrase: order and adjacency matter. Both
        // matching notes contain the adjacent phrase.
        assert!(paths(&index, "\"zebra essay xylophone\"").is_empty());
        let mut phrase = paths(&index, "\"xylophone zebra\"");
        phrase.sort();
        assert_eq!(phrase, ["posts/both.md", "posts/rust.md"]);
        // Exclusion narrows: rust.md is the only note containing "essay".
        let mut excluded = paths(&index, "zebra -essay");
        excluded.sort();
        assert_eq!(excluded, ["posts/both.md", "posts/other.md"]);
        // Invalid queries surface a Query error for the tool layer.
        assert!(matches!(
            index.search("nosuchfield:x", None, 10),
            Err(SearchError::Query(_))
        ));
    }

    #[test]
    fn prefix_filter_and_limit() {
        let (_vault, _v, index, _i) = seeded_index();

        let hits = index.search("zebra", Some("posts"), 10).expect("search");
        let mut found: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        found.sort();
        assert_eq!(found, ["posts/other.md", "posts/rust.md"]);

        // Prefix matching is case-insensitive and anchors at the start.
        let hits = index
            .search("zebra", Some("POSTS/other"), 10)
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "posts/other.md");

        let hits = index
            .search("zebra", Some("nothing/here"), 10)
            .expect("search");
        assert!(hits.is_empty());

        let hits = index.search("zebra", None, 1).expect("search");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn reindex_paths_handles_add_update_remove() {
        let (vault, _v, index, _i) = fixture();
        index.build_all(&vault).expect("build empty");
        assert!(paths(&index, "elephant").is_empty());

        let note = vault.root().join("new.md");
        std::fs::write(&note, "elephant\n").expect("write");
        index
            .reindex_paths(&vault, ["new.md"])
            .expect("reindex add");
        assert_eq!(paths(&index, "elephant"), ["new.md"]);

        std::fs::write(&note, "giraffe\n").expect("overwrite");
        index
            .reindex_paths(&vault, ["new.md"])
            .expect("reindex update");
        assert!(paths(&index, "elephant").is_empty());
        assert_eq!(paths(&index, "giraffe"), ["new.md"]);

        std::fs::remove_file(&note).expect("remove");
        index
            .reindex_paths(&vault, ["new.md"])
            .expect("reindex remove");
        assert!(paths(&index, "giraffe").is_empty());
    }

    #[test]
    fn index_persists_across_reopen() {
        let (vault, _vault_dir, index, index_dir) = seeded_index();
        assert_eq!(paths(&index, "xylophone"), ["posts/rust.md"]);
        // Drop everything (releasing tantivy's writer lock file), then
        // reopen the same directory.
        drop(index);
        drop(vault);
        let index = SearchIndex::open(index_dir.path()).expect("reopen index");
        assert_eq!(paths(&index, "xylophone"), ["posts/rust.md"]);
    }

    #[test]
    fn foreign_schema_is_rejected() {
        let index_dir = tempfile::tempdir().expect("index tempdir");
        // Build an index with a schema that is not ours.
        let mut builder = Schema::builder();
        builder.add_text_field("whatever", TEXT);
        Index::create_in_dir(index_dir.path(), builder.build()).expect("create foreign index");

        let error = SearchIndex::open(index_dir.path())
            .err()
            .expect("must reject");
        assert!(matches!(error, SearchError::Incompatible(_)));
    }

    /// Polls `check` until it returns true or the deadline passes; returns
    /// the final answer so failures report the observed (stale) state.
    fn wait_for(deadline: Duration, mut check: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + deadline;
        loop {
            let done = check();
            if done || Instant::now() >= end {
                return done;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn watcher_reindexes_add_update_and_remove() {
        let (vault, _v, index, _i) = fixture();
        index.build_all(&vault).expect("build");
        // Generous debounce keeps the test's event batching small; the
        // wait_for deadlines absorb scheduling jitter under parallel load.
        let watcher = index
            .spawn_watcher(&vault, Duration::from_millis(50))
            .expect("watcher starts");

        let note = vault.root().join("watched.md");
        std::fs::write(&note, "saxophone solo\n").expect("write");
        assert!(
            wait_for(Duration::from_secs(10), || !paths(&index, "saxophone")
                .is_empty()),
            "new note should be indexed"
        );

        std::fs::write(&note, "trombone solo\n").expect("overwrite");
        assert!(
            wait_for(Duration::from_secs(10), || !paths(&index, "trombone")
                .is_empty()),
            "updated note should be re-indexed"
        );

        std::fs::remove_file(&note).expect("remove");
        assert!(
            wait_for(Duration::from_secs(10), || paths(&index, "trombone")
                .is_empty()),
            "deleted note should leave the index"
        );

        watcher.stop();
    }

    #[test]
    fn watcher_ignores_dot_folders() {
        let (vault, _v, index, _i) = fixture();
        index.build_all(&vault).expect("build");
        let watcher = index
            .spawn_watcher(&vault, Duration::from_millis(50))
            .expect("watcher starts");

        let hidden = vault.root().join(".obsidian");
        std::fs::create_dir_all(&hidden).expect("mkdir");
        std::fs::write(hidden.join("config.md"), "hidden xylophone\n").expect("write");

        // Give the watcher time to (wrongly) process the event if it were
        // going to; then confirm nothing was indexed.
        std::thread::sleep(Duration::from_millis(300));
        assert!(paths(&index, "xylophone").is_empty());

        watcher.stop();
    }

    #[test]
    fn watcher_stop_joins_promptly() {
        let (vault, _v, index, _i) = fixture();
        index.build_all(&vault).expect("build");
        let watcher = index
            .spawn_watcher(&vault, Duration::from_secs(5))
            .expect("watcher");
        // With a 5s debounce the thread is blocked in recv_timeout; stop
        // must still join within a fraction of that window.
        let start = Instant::now();
        watcher.stop();
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "stop took too long"
        );
    }

    #[test]
    fn prefix_filter_is_literal_not_regex() {
        let (vault, _v, index, _i) = fixture();
        for dir in ["a(b", "ab"] {
            std::fs::create_dir_all(vault.root().join(dir)).expect("mkdir");
            std::fs::write(
                vault.root().join(format!("{dir}/note.md")),
                "# Note\n\nxylophone\n",
            )
            .expect("write");
        }
        index.build_all(&vault).expect("build");

        // Regex metacharacters in a prefix match literally (never panic,
        // never widen the filter to other notes).
        let hits = index.search("xylophone", Some("a(b"), 10).expect("search");
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, ["a(b/note.md"]);
        // A traversal-shaped prefix is just a literal that matches nothing.
        let hits = index.search("xylophone", Some("../"), 10).expect("search");
        assert!(hits.is_empty());
    }

    #[test]
    fn hostile_queries_fail_gracefully() {
        let (_vault, _v, index, _i) = seeded_index();
        // Unbalanced or unclosed syntax is a query error the tool layer
        // reports to the caller, never a panic.
        for hostile in ["(((", "\"unclosed"] {
            assert!(
                matches!(index.search(hostile, None, 10), Err(SearchError::Query(_))),
                "{hostile:?} should be a query error"
            );
        }
        // An empty query is valid and simply matches nothing.
        assert_eq!(paths(&index, ""), Vec::<String>::new());
    }

    #[test]
    fn unicode_terms_are_searchable() {
        let (vault, _v, index, _i) = fixture();
        std::fs::write(
            vault.root().join("accents.md"),
            "# Café\n\nA naïve façade discussion.\n",
        )
        .expect("write");
        index.build_all(&vault).expect("build");
        // The query tokenizer and the indexing tokenizer must agree on
        // non-ASCII terms.
        assert_eq!(paths(&index, "café"), ["accents.md"]);
        assert_eq!(paths(&index, "naïve"), ["accents.md"]);
        // Case folding covers non-ASCII, not just ASCII.
        assert_eq!(paths(&index, "CAFÉ"), ["accents.md"]);
        // An accented query must not match the unaccented word: diacritics
        // are significant, so "facade" does not retrieve "façade".
        assert!(paths(&index, "facade").is_empty());
    }

    #[test]
    fn collect_event_filters_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize");
        let event = |path: &str| {
            let mut event = Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Any));
            event.paths.push(root.join(path));
            Ok(event)
        };
        let mut pending = BTreeSet::new();
        collect_event(event("a/b/note.md"), &root, &None, &mut pending);
        collect_event(event("image.png"), &root, &None, &mut pending);
        collect_event(event(".obsidian/app.json"), &root, &None, &mut pending);
        collect_event(event("sub/.trash/old.md"), &root, &None, &mut pending);
        // A path outside the vault root (e.g. a sibling directory) is
        // skipped: it can never match a vault-relative note path.
        let outside = dir.path().parent().expect("parent").join("sibling.md");
        let mut outside_event =
            Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Any));
        outside_event.paths.push(outside);
        collect_event(Ok(outside_event), &root, &None, &mut pending);
        let mut found: Vec<&str> = pending.iter().map(String::as_str).collect();
        found.sort();
        assert_eq!(found, ["a/b/note.md"]);

        // Index-dir guard: a note inside the configured index dir is skipped.
        let mut pending = BTreeSet::new();
        collect_event(
            event(".mcp-obsidian-index/x.md"),
            &root,
            &Some(".mcp-obsidian-index".to_string()),
            &mut pending,
        );
        assert!(pending.is_empty());
    }
}
