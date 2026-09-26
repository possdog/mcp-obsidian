//! End-to-end tests: a real rmcp streamable-HTTP client against the full axum
//! router (MCP service + bearer auth), and raw HTTP probes of the auth
//! middleware. The `ob` binary is a stub shell script; the vault is a
//! tempdir fixture.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mcp_obsidian::search::{SearchIndex, WatcherHandle};
use mcp_obsidian::server::{self, ServerContext};
use mcp_obsidian::sync::{SupervisorOptions, SyncSupervisor};
use mcp_obsidian::vault::Vault;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "test-token-abc123";

struct TestServer {
    addr: SocketAddr,
    cancellation: CancellationToken,
    supervisor: Arc<SyncSupervisor>,
    watcher: WatcherHandle,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    // Keep the fixtures alive for the server's lifetime.
    _vault: tempfile::TempDir,
    _index: tempfile::TempDir,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with_hosts(None).await
    }

    /// Like [`TestServer::start`], but with explicit `allowed_hosts` for
    /// rmcp's inbound Host-header validation (DNS-rebinding guard).
    async fn start_with_hosts(allowed_hosts: Option<Vec<String>>) -> Self {
        let vault = tempfile::tempdir().expect("vault tempdir");
        std::fs::write(
            vault.path().join("hello.md"),
            "---\ntitle: Hello\ntags: [greeting]\n---\n# Hello\n\nSee [[world]].\n\n## Notes\n\nSome notes with #sample tag.\n",
        )
        .expect("write hello");
        std::fs::write(vault.path().join("world.md"), "# World\n\nBack at you.\n")
            .expect("write world");

        // Stub ob: one-shot commands answer and exit; `sync --continuous`
        // (what the supervisor runs) stays alive. The daemon and the
        // one-shot `sync` share the `sync` subcommand, so dispatch also
        // checks $2: without that, the "daemon: running" assertion races
        // the supervisor's backoff loop.
        let ob = vault.path().join("ob-stub");
        std::fs::write(
            &ob,
            "#!/bin/sh\ncase \"$1\" in\n  sync-status) echo 'status: synced (stub)'; exit 0;;\n  sync)\n    case \"$2\" in\n      --continuous) ;;\n      *) echo 'synced (stub)'; exit 0;;\n    esac\n    ;;\nesac\nexec sleep 300\n",
        )
        .expect("write stub");
        set_executable(&ob);

        let vault_root = vault.path().to_path_buf();
        let ob_bin = ob.to_str().expect("utf-8 path").to_owned();

        let supervisor = Arc::new(SyncSupervisor::start_with(
            &ob_bin,
            &vault_root,
            SupervisorOptions {
                initial_backoff: std::time::Duration::from_millis(50),
                max_backoff: std::time::Duration::from_millis(200),
                healthy_run: std::time::Duration::from_secs(600),
            },
        ));

        // Search index: independent tempdir (never inside the vault, which
        // would both trip the watcher and get synced), rebuilt at startup
        // exactly like the real server does, then kept fresh by its own
        // watcher. The debounce is short so the live-re-index test below
        // does not need to wait out the production window.
        let index_dir = tempfile::tempdir().expect("index tempdir");
        let search = Arc::new(SearchIndex::open(index_dir.path()).expect("index opens"));
        search
            .build_all(&Vault::open(&vault_root).expect("vault"))
            .expect("index builds");
        let watcher = search
            .spawn_watcher(
                &Vault::open(&vault_root).expect("vault"),
                Duration::from_millis(100),
            )
            .expect("watcher starts");

        let ctx = ServerContext::new(
            Vault::open(&vault_root).expect("vault"),
            search,
            mcp_obsidian::writes::DailyNoteSettings::default(),
            ob_bin.as_str(),
            supervisor.clone(),
        );
        let cancellation = CancellationToken::new();
        let router = server::router(ctx, TOKEN, cancellation.clone(), allowed_hosts);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let shutdown = cancellation.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move { shutdown.cancelled().await })
                .await
        });

        Self {
            addr,
            cancellation,
            supervisor,
            watcher,
            task,
            _vault: vault,
            _index: index_dir,
        }
    }

    async fn stop(self) {
        self.cancellation.cancel();
        self.task
            .await
            .expect("server task")
            .expect("server served cleanly");
        self.supervisor.stop().await;
        self.watcher.stop();
    }
}

#[cfg(unix)]
fn set_executable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .expect("stub metadata")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("chmod stub");
}

#[cfg(not(unix))]
fn set_executable(_path: &std::path::Path) {}

fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.as_str(),
            _ => "",
        })
        .collect()
}

fn call(name: &str, arguments: serde_json::Value) -> CallToolRequestParams {
    // CallToolRequestParams is #[non_exhaustive], so build via Default and
    // public-field mutation rather than a struct literal.
    let mut params = CallToolRequestParams::default();
    params.name = name.to_owned().into();
    params.arguments = serde_json::from_value(arguments).expect("valid arguments object");
    params
}

/// True when a call either failed at the protocol level or came back as a
/// tool-level error result: the model sees a failure either way, and a
/// hostile argument must never quietly succeed.
fn failed(result: Result<CallToolResult, impl std::fmt::Debug>) -> bool {
    match result {
        Err(_) => true,
        Ok(result) => result.is_error == Some(true),
    }
}

/// Connects an rmcp client. The error is boxed: rmcp's
/// `ClientInitializeError` is large enough to trip `clippy::result_large_err`,
/// and boxing costs nothing in a test helper.
async fn connect(
    addr: &SocketAddr,
    token: &str,
) -> Result<
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    Box<rmcp::service::ClientInitializeError>,
> {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
            .auth_header(token),
    );
    ().serve(transport).await.map_err(Box::new)
}

#[tokio::test]
async fn client_round_trip_over_streamable_http() {
    let server = TestServer::start().await;
    let client = connect(&server.addr, TOKEN)
        .await
        .expect("client initialize");

    let tools = client.list_all_tools().await.expect("list tools");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    for expected in [
        "list_notes",
        "read_note",
        "search_notes",
        "get_backlinks",
        "get_outlinks",
        "list_tags",
        "write_note",
        "patch_note",
        "move_note",
        "delete_note",
        "daily_note",
        "sync_status",
        "sync_now",
    ] {
        assert!(
            names.contains(&expected),
            "tool {expected} missing; got {names:?}"
        );
    }

    let result = client
        .call_tool(call("list_notes", serde_json::json!({})))
        .await
        .expect("list_notes call");
    assert_ne!(result.is_error, Some(true));
    let text = result_text(&result);
    assert!(text.contains("hello.md"), "got: {text}");
    assert!(text.contains("world.md"), "got: {text}");

    // Heading-filtered read.
    let result = client
        .call_tool(call(
            "read_note",
            serde_json::json!({ "path": "hello.md", "heading": "notes" }),
        ))
        .await
        .expect("read_note call");
    let text = result_text(&result);
    assert!(text.contains("Some notes with #sample tag."), "got: {text}");
    assert!(!text.contains("# Hello"), "got: {text}");

    // Link graph, by bare name (no .md).
    let result = client
        .call_tool(call(
            "get_backlinks",
            serde_json::json!({ "path": "world" }),
        ))
        .await
        .expect("get_backlinks call");
    let text = result_text(&result);
    assert!(text.contains("hello.md"), "got: {text}");

    let result = client
        .call_tool(call(
            "get_outlinks",
            serde_json::json!({ "path": "hello.md" }),
        ))
        .await
        .expect("get_outlinks call");
    let text = result_text(&result);
    assert!(text.contains("world.md"), "got: {text}");

    let result = client
        .call_tool(call("list_tags", serde_json::json!({})))
        .await
        .expect("list_tags call");
    let text = result_text(&result);
    assert!(text.contains("#greeting"), "got: {text}");
    assert!(text.contains("#sample"), "got: {text}");

    // Full-text search: the frontmatter tag "greeting" is indexed.
    let result = client
        .call_tool(call(
            "search_notes",
            serde_json::json!({ "query": "greeting" }),
        ))
        .await
        .expect("search_notes call");
    assert_ne!(result.is_error, Some(true));
    let text = result_text(&result);
    assert!(text.contains("hello.md"), "got: {text}");
    assert!(text.contains("Hello"), "title missing; got: {text}");

    // Prefix filter with no match.
    let result = client
        .call_tool(call(
            "search_notes",
            serde_json::json!({ "query": "*", "prefix": "no/such/dir" }),
        ))
        .await
        .expect("search_notes call");
    assert!(
        result_text(&result).contains("No notes match"),
        "got: {}",
        result_text(&result)
    );

    // Live re-indexing: a note written after startup is found once the
    // watcher's debounce window has passed.
    std::fs::write(
        server._vault.path().join("searchable.md"),
        "# Searchable\n\nA unique xylophone passage.\n",
    )
    .expect("write searchable");
    let found = {
        let client = &client;
        let mut found = false;
        for _ in 0..50 {
            let result = client
                .call_tool(call(
                    "search_notes",
                    serde_json::json!({ "query": "xylophone" }),
                ))
                .await
                .expect("search_notes call");
            if result_text(&result).contains("searchable.md") {
                found = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        found
    };
    assert!(found, "watcher should index the new note within 5s");

    // Invalid query syntax is a tool-level error the model can act on.
    let result = client
        .call_tool(call(
            "search_notes",
            serde_json::json!({ "query": "nosuchfield:x" }),
        ))
        .await
        .expect("search_notes call");
    assert_eq!(result.is_error, Some(true));
    assert!(
        result_text(&result).contains("invalid query"),
        "got: {}",
        result_text(&result)
    );

    // The supervisor spawns the daemon on a tokio task; under parallel
    // test load that task may not have polled yet when this call runs.
    // The tool must report a running daemon once startup settles, so
    // poll for up to 5s instead of asserting on the first try.
    let mut text = String::new();
    let mut running = false;
    for _ in 0..50 {
        let result = client
            .call_tool(call("sync_status", serde_json::json!({})))
            .await
            .expect("sync_status call");
        text = result_text(&result);
        if text.contains("daemon: running") {
            running = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(running, "daemon never reported running; got: {text}");
    assert!(text.contains("status: synced (stub)"), "got: {text}");

    // Tool-level errors surface as is_error results with a message.
    let result = client
        .call_tool(call("read_note", serde_json::json!({ "path": "nope.md" })))
        .await
        .expect("read_note call");
    assert_eq!(result.is_error, Some(true));
    assert!(
        result_text(&result).contains("not found"),
        "got: {}",
        result_text(&result)
    );

    client.cancel().await.expect("client cancel");
    server.stop().await;
}

#[tokio::test]
async fn write_tools_round_trip() {
    let server = TestServer::start().await;
    let client = connect(&server.addr, TOKEN)
        .await
        .expect("client initialize");

    // Create via the tool; the note is immediately searchable because the
    // write path refreshes the index synchronously (no debounce wait).
    let result = client
        .call_tool(call(
            "write_note",
            serde_json::json!({
                "path": "projects/alpha.md",
                "content": "# Alpha\n\nThe xylophone plan.\n"
            }),
        ))
        .await
        .expect("write_note call");
    assert_ne!(result.is_error, Some(true));
    assert!(
        result_text(&result).contains("Created projects/alpha.md"),
        "got: {}",
        result_text(&result)
    );
    let result = client
        .call_tool(call(
            "search_notes",
            serde_json::json!({ "query": "xylophone" }),
        ))
        .await
        .expect("search call");
    assert!(
        result_text(&result).contains("projects/alpha.md"),
        "got: {}",
        result_text(&result)
    );

    // Overwrite protection is a tool-level error.
    let result = client
        .call_tool(call(
            "write_note",
            serde_json::json!({ "path": "projects/alpha.md", "content": "x" }),
        ))
        .await
        .expect("write_note call");
    assert_eq!(result.is_error, Some(true));
    assert!(
        result_text(&result).contains("overwrite"),
        "got: {}",
        result_text(&result)
    );

    // Frontmatter + patch: the frontmatter block survives the append.
    let result = client
        .call_tool(call(
            "write_note",
            serde_json::json!({
                "path": "projects/beta.md",
                "content": "body\n",
                "frontmatter": "title: Beta\ntags: [beta]"
            }),
        ))
        .await
        .expect("write_note call");
    assert_ne!(result.is_error, Some(true));
    let result = client
        .call_tool(call(
            "patch_note",
            serde_json::json!({
                "path": "projects/beta.md",
                "operation": "append",
                "content": "added"
            }),
        ))
        .await
        .expect("patch_note call");
    assert_ne!(result.is_error, Some(true));
    let result = client
        .call_tool(call(
            "read_note",
            serde_json::json!({ "path": "projects/beta.md" }),
        ))
        .await
        .expect("read_note call");
    let text = result_text(&result);
    // The frontmatter block is returned verbatim: fences exactly once.
    assert!(
        text.starts_with("---\ntitle: Beta\n"),
        "frontmatter must render once, got: {text}"
    );
    assert!(
        !text.starts_with("---\n---\n"),
        "frontmatter fences must not be doubled, got: {text}"
    );
    assert!(
        text.contains("title: Beta") && text.contains("body") && text.contains("added"),
        "got: {text}"
    );

    // Move with link rewriting: the linker references beta as a wikilink
    // and as a relative markdown link.
    let result = client
        .call_tool(call(
            "write_note",
            serde_json::json!({
                "path": "projects/linker.md",
                "content": "See [[beta]] and [b](beta.md).\n"
            }),
        ))
        .await
        .expect("write_note call");
    assert_ne!(result.is_error, Some(true));
    let result = client
        .call_tool(call(
            "move_note",
            serde_json::json!({ "from": "projects/beta.md", "to": "renamed/gamma.md" }),
        ))
        .await
        .expect("move_note call");
    let text = result_text(&result);
    assert!(
        text.contains("Moved projects/beta.md to renamed/gamma.md"),
        "got: {text}"
    );
    assert!(text.contains("projects/linker.md"), "got: {text}");
    let result = client
        .call_tool(call(
            "read_note",
            serde_json::json!({ "path": "projects/linker.md" }),
        ))
        .await
        .expect("read_note call");
    let text = result_text(&result);
    assert!(text.contains("[[gamma]]"), "got: {text}");
    assert!(text.contains("(../renamed/gamma.md)"), "got: {text}");
    let result = client
        .call_tool(call(
            "get_backlinks",
            serde_json::json!({ "path": "renamed/gamma.md" }),
        ))
        .await
        .expect("get_backlinks call");
    assert!(
        result_text(&result).contains("projects/linker.md"),
        "got: {}",
        result_text(&result)
    );

    // Delete goes to .trash and the note leaves the index.
    let result = client
        .call_tool(call(
            "delete_note",
            serde_json::json!({ "path": "projects/alpha.md" }),
        ))
        .await
        .expect("delete_note call");
    assert!(
        result_text(&result).contains(".trash/alpha.md"),
        "got: {}",
        result_text(&result)
    );
    let result = client
        .call_tool(call(
            "search_notes",
            serde_json::json!({ "query": "xylophone" }),
        ))
        .await
        .expect("search call");
    assert!(
        result_text(&result).contains("No notes match"),
        "got: {}",
        result_text(&result)
    );

    // Daily notes: create, get, append.
    let result = client
        .call_tool(call(
            "daily_note",
            serde_json::json!({
                "action": "create",
                "date": "2026-09-25",
                "content": "daily marker text\n"
            }),
        ))
        .await
        .expect("daily_note call");
    assert!(
        result_text(&result).contains("Created Daily Notes/2026-09-25.md"),
        "got: {}",
        result_text(&result)
    );
    let result = client
        .call_tool(call(
            "daily_note",
            serde_json::json!({ "action": "get", "date": "2026-09-25" }),
        ))
        .await
        .expect("daily_note call");
    assert!(
        result_text(&result).contains("daily marker text"),
        "got: {}",
        result_text(&result)
    );
    let result = client
        .call_tool(call(
            "daily_note",
            serde_json::json!({
                "action": "append",
                "date": "2026-09-25",
                "content": "second line"
            }),
        ))
        .await
        .expect("daily_note call");
    assert_ne!(result.is_error, Some(true));
    let result = client
        .call_tool(call(
            "daily_note",
            serde_json::json!({ "action": "get", "date": "2026-09-25" }),
        ))
        .await
        .expect("daily_note call");
    let text = result_text(&result);
    assert!(
        text.contains("daily marker text") && text.contains("second line"),
        "got: {text}"
    );

    client.cancel().await.expect("client cancel");
    server.stop().await;
}

#[tokio::test]
async fn bearer_token_is_enforced() {
    let server = TestServer::start().await;
    let base = format!("http://{}/mcp", server.addr);

    let client = reqwest::Client::new();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"probe","version":"1.0"}}}"#;

    // No token -> 401.
    let resp = client
        .post(&base)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body)
        .send()
        .await
        .expect("no-token post");
    assert_eq!(resp.status(), 401);
    assert_eq!(
        resp.headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok()),
        Some("Bearer")
    );

    // Wrong token -> 401.
    let resp = client
        .post(&base)
        .header("authorization", "Bearer wrong")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body)
        .send()
        .await
        .expect("wrong-token post");
    assert_eq!(resp.status(), 401);

    // Correct token passes the middleware (200 with an initialize result).
    // Dropping the response closes the SSE connection so the server's
    // graceful shutdown in `stop` does not wait on it.
    let resp = client
        .post(&base)
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body)
        .send()
        .await
        .expect("good-token post");
    assert_eq!(resp.status(), 200);
    drop(resp);

    server.stop().await;
}

#[tokio::test]
async fn wrong_token_client_cannot_initialize() {
    let server = TestServer::start().await;
    let result = connect(&server.addr, "not-the-token").await;
    assert!(result.is_err(), "initialize should fail with 401");
    server.stop().await;
}

/// Hostile tool arguments over the wire: path traversal, absolute paths,
/// dot-folders, NUL, backslashes, and symlink redirection must all come
/// back as tool errors, with the vault and the filesystem outside it
/// untouched.
#[tokio::test]
async fn malicious_paths_are_rejected_end_to_end() {
    let server = TestServer::start().await;
    let client = connect(&server.addr, TOKEN)
        .await
        .expect("client initialize");

    let escape_probe = "mcp-obsidian-escape-probe.md";
    let outside = server._vault.path().parent().unwrap().join(escape_probe);

    // Reads: traversal, absolute, dot-folder, and embedded-NUL paths.
    for bad in [
        "../hello.md",
        "hello.md/../../hello.md",
        "/etc/passwd",
        ".obsidian/app.json",
        ".trash/hello.md",
        "hello\0.md",
        "a\\hello.md",
    ] {
        let result = client
            .call_tool(call("read_note", serde_json::json!({ "path": bad })))
            .await
            .expect("read_note call");
        assert_eq!(result.is_error, Some(true), "read_note must reject {bad:?}");
    }

    // List prefixes cannot probe hidden folders or escape upward.
    for bad in ["..", "../", ".obsidian", ".trash/x"] {
        let result = client
            .call_tool(call("list_notes", serde_json::json!({ "prefix": bad })))
            .await
            .expect("list_notes call");
        assert_eq!(
            result.is_error,
            Some(true),
            "list_notes must reject {bad:?}"
        );
    }

    // Writes refuse the same shapes everywhere.
    for bad in [
        "../evil.md",
        "/tmp/evil.md",
        ".obsidian/app.json",
        ".trash/hello.md",
        "evil\0.md",
        "",
    ] {
        let result = client
            .call_tool(call(
                "write_note",
                serde_json::json!({ "path": bad, "content": "x" }),
            ))
            .await
            .expect("write_note call");
        assert_eq!(
            result.is_error,
            Some(true),
            "write_note must reject {bad:?}"
        );

        let result = client
            .call_tool(call(
                "move_note",
                serde_json::json!({ "from": "hello.md", "to": bad }),
            ))
            .await
            .expect("move_note call");
        assert_eq!(result.is_error, Some(true), "move_note must reject {bad:?}");

        let result = client
            .call_tool(call("delete_note", serde_json::json!({ "path": bad })))
            .await
            .expect("delete_note call");
        assert_eq!(
            result.is_error,
            Some(true),
            "delete_note must reject {bad:?}"
        );
    }

    // The daily-note date is parsed strictly, so path syntax smuggled in a
    // date never reaches the filesystem.
    for bad in ["2026-09-25/../evil", "2026/09/25", "../../../2026-09-25"] {
        let result = client
            .call_tool(call(
                "daily_note",
                serde_json::json!({ "action": "create", "date": bad, "content": "x" }),
            ))
            .await
            .expect("daily_note call");
        assert_eq!(
            result.is_error,
            Some(true),
            "daily_note must reject {bad:?}"
        );
    }

    // A symlink planted inside the vault pointing at a file outside it.
    #[cfg(unix)]
    {
        let secret = tempfile::tempdir().expect("secret tempdir");
        std::fs::write(secret.path().join("secret.md"), "secrets").expect("write secret");
        std::os::unix::fs::symlink(
            secret.path().join("secret.md"),
            server._vault.path().join("planted-link.md"),
        )
        .expect("plant symlink");
        let result = client
            .call_tool(call(
                "read_note",
                serde_json::json!({ "path": "planted-link.md" }),
            ))
            .await
            .expect("read_note call");
        assert_eq!(
            result.is_error,
            Some(true),
            "read_note must refuse a planted symlink"
        );
    }

    // Nothing escaped and the existing notes are intact.
    assert!(
        !outside.exists(),
        "no file may be created outside the vault"
    );
    let result = client
        .call_tool(call("read_note", serde_json::json!({ "path": "hello.md" })))
        .await
        .expect("read_note call");
    assert_ne!(result.is_error, Some(true));
    assert!(
        result_text(&result).contains("# Hello"),
        "got: {}",
        result_text(&result)
    );

    client.cancel().await.expect("client cancel");
    server.stop().await;
}

/// Raw HTTP probes of the bearer-token middleware beyond the plain
/// wrong-token case: other methods, padded schemes, and hostile header
/// values must all be refused before any MCP handling.
#[tokio::test]
async fn auth_middleware_rejects_probes() {
    let server = TestServer::start().await;
    let base = format!("http://{}/mcp", server.addr);
    let client = reqwest::Client::new();

    let probe = |method: reqwest::Method, auth: Option<String>, body: Option<&'static str>| {
        let mut request = client
            .request(method, &base)
            .header("accept", "application/json, text/event-stream");
        if let Some(auth) = auth {
            request = request.header("authorization", auth);
        }
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .body(body);
        }
        request
    };

    const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"probe","version":"1.0"}}}"#;

    // Every method requires the token, including the SSE GET.
    let resp = probe(reqwest::Method::GET, None, None)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    assert_eq!(
        resp.headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok()),
        Some("Bearer")
    );
    let resp = probe(reqwest::Method::DELETE, Some("Bearer wrong".into()), None)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // Padded, doubled, and scheme-less headers must not authenticate.
    // (A *trailing* space after the token is not probed here: HTTP field
    // values have their surrounding whitespace stripped on the wire per
    // RFC 9110, so "Bearer tok " never reaches the middleware as anything
    // but "Bearer tok". The middleware-level behavior — whitespace is
    // significant, no trimming — is pinned by parse_bearer's unit tests.)
    for auth in [
        "Bearer ".to_string(),
        format!(" {TOKEN}"),
        format!("  {TOKEN}"),
        format!("Bearer  {TOKEN}"),
        TOKEN.to_string(),
        "Basic dGVzdA==".to_string(),
        format!("Bearer{}", TOKEN),
        "x".repeat(100_000),
    ] {
        let resp = probe(reqwest::Method::POST, Some(auth), Some(INIT))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "probe header must be refused");
    }

    // The scheme itself is case-insensitive per RFC 6750.
    for scheme in ["bearer", "BEARER"] {
        let resp = probe(
            reqwest::Method::POST,
            Some(format!("{scheme} {TOKEN}")),
            Some(INIT),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(resp.status(), 200, "{scheme} is a valid scheme");
        // Dropping the response closes the SSE stream for shutdown.
        drop(resp);
    }

    server.stop().await;
}

/// Malformed and hostile tool calls: unknown tools, wrong-typed arguments,
/// unknown enum values, and unknown operations are reported as failures
/// without wedging the session; benign extra arguments are tolerated.
#[tokio::test]
async fn malformed_tool_calls_are_reported_not_fatal() {
    let server = TestServer::start().await;
    let client = connect(&server.addr, TOKEN)
        .await
        .expect("client initialize");

    // Unknown tool: a protocol error, never a silent success.
    let result = client
        .call_tool(call("no_such_tool", serde_json::json!({})))
        .await;
    assert!(failed(result), "unknown tool must fail");

    // Wrong-typed argument (path must be a string).
    let result = client
        .call_tool(call("read_note", serde_json::json!({ "path": 123 })))
        .await;
    assert!(failed(result), "non-string path must fail");

    // Missing required argument: empty path is a tool-level error.
    let result = client
        .call_tool(call("read_note", serde_json::json!({})))
        .await
        .expect("read_note call");
    assert_eq!(result.is_error, Some(true));
    assert!(
        result_text(&result).contains("path is empty"),
        "got: {}",
        result_text(&result)
    );

    // Unknown enum values surface as actionable tool errors.
    let result = client
        .call_tool(call(
            "patch_note",
            serde_json::json!({ "path": "hello.md", "operation": "rm -rf /", "content": "x" }),
        ))
        .await
        .expect("patch_note call");
    assert_eq!(result.is_error, Some(true));
    assert!(
        result_text(&result).contains("unknown operation"),
        "got: {}",
        result_text(&result)
    );
    let result = client
        .call_tool(call(
            "daily_note",
            serde_json::json!({ "action": "explode", "date": "2026-09-25" }),
        ))
        .await
        .expect("daily_note call");
    assert_eq!(result.is_error, Some(true));
    assert!(
        result_text(&result).contains("unknown action"),
        "got: {}",
        result_text(&result)
    );

    // A hostile operation value must not have modified the note.
    let result = client
        .call_tool(call("read_note", serde_json::json!({ "path": "hello.md" })))
        .await
        .expect("read_note call");
    assert_ne!(result.is_error, Some(true));
    assert!(result_text(&result).contains("# Hello"));

    // Unknown extra arguments are tolerated (serde ignores them), and the
    // session stays usable afterwards.
    let result = client
        .call_tool(call(
            "read_note",
            serde_json::json!({ "path": "hello.md", "surprise": "argument" }),
        ))
        .await
        .expect("read_note call");
    assert_ne!(result.is_error, Some(true));
    let result = client
        .call_tool(call("list_notes", serde_json::json!({})))
        .await
        .expect("list_notes call");
    assert_ne!(result.is_error, Some(true));

    client.cancel().await.expect("client cancel");
    server.stop().await;
}

/// Resource-exhaustion probes from the client side: absurd search limits
/// are clamped, and a pathologically large note cannot blow up a client
/// context window (tool output is capped).
#[tokio::test]
async fn hostile_sizes_are_capped() {
    let server = TestServer::start().await;
    let client = connect(&server.addr, TOKEN)
        .await
        .expect("client initialize");

    // limit=0 (and absurd limits) clamp into 1..=100 rather than being
    // passed to the collector raw: 0 becomes 1.
    let result = client
        .call_tool(call(
            "search_notes",
            serde_json::json!({ "query": "*", "limit": 0 }),
        ))
        .await
        .expect("search_notes call");
    assert_ne!(result.is_error, Some(true));
    let text = result_text(&result);
    let hits = text
        .lines()
        .filter(|l| l.starts_with(char::is_numeric))
        .count();
    assert_eq!(hits, 1, "limit 0 must clamp to 1 result, got: {text}");

    // An absurdly large limit must neither error nor silently drop
    // results: it returns every note the fixture has (compared against
    // list_notes, so the test does not hard-code the fixture size; the
    // `.md` filter mirrors search, which indexes only notes — list_notes
    // also shows non-note files like the ob-stub fixture).
    let result = client
        .call_tool(call("list_notes", serde_json::json!({})))
        .await
        .expect("list_notes call");
    let total = result_text(&result)
        .lines()
        .filter(|l| l.starts_with("- ") && l.contains(".md"))
        .count();
    let result = client
        .call_tool(call(
            "search_notes",
            serde_json::json!({ "query": "*", "limit": 1_000_000 }),
        ))
        .await
        .expect("search_notes call");
    assert_ne!(result.is_error, Some(true));
    let text = result_text(&result);
    let hits = text
        .lines()
        .filter(|l| l.starts_with(char::is_numeric))
        .count();
    assert_eq!(hits, total, "a huge limit must return every note");

    // A 300 KB note writes fine, but reading it back is truncated at the
    // tool-output cap instead of flooding the caller.
    let big = "a".repeat(300_000);
    let result = client
        .call_tool(call(
            "write_note",
            serde_json::json!({ "path": "big.md", "content": big }),
        ))
        .await
        .expect("write_note call");
    assert_ne!(result.is_error, Some(true));
    let result = client
        .call_tool(call("read_note", serde_json::json!({ "path": "big.md" })))
        .await
        .expect("read_note call");
    assert_ne!(result.is_error, Some(true));
    let text = result_text(&result);
    assert!(
        text.contains("output truncated at"),
        "read of a huge note must be capped, got {} bytes",
        text.len()
    );
    // The truncated response must still be a prefix of the real content
    // and report the true original size — not an arbitrary short
    // response that merely mentions truncation.
    assert!(
        text.starts_with(&"a".repeat(1_000)),
        "truncated output must start with the note's content"
    );
    assert!(
        text.contains("of 300000"),
        "the marker must report the true size, got: {text:?}"
    );
    assert!(text.len() < 300_000, "got {} bytes", text.len());

    client.cancel().await.expect("client cancel");
    server.stop().await;
}

/// Non-ASCII and spaced paths round-trip through every layer: JSON, the
/// sandbox, the filesystem, listing, and search.
#[tokio::test]
async fn unicode_and_spaced_paths_round_trip() {
    let server = TestServer::start().await;
    let client = connect(&server.addr, TOKEN)
        .await
        .expect("client initialize");

    let path = "café/naïve note (draft).md";
    let result = client
        .call_tool(call(
            "write_note",
            serde_json::json!({ "path": path, "content": "zebra unicorn\n" }),
        ))
        .await
        .expect("write_note call");
    assert_ne!(result.is_error, Some(true));

    let result = client
        .call_tool(call("read_note", serde_json::json!({ "path": path })))
        .await
        .expect("read_note call");
    assert_ne!(result.is_error, Some(true));
    assert!(result_text(&result).contains("zebra unicorn"));

    let result = client
        .call_tool(call("list_notes", serde_json::json!({ "prefix": "café" })))
        .await
        .expect("list_notes call");
    assert!(
        result_text(&result).contains("naïve note (draft).md"),
        "got: {}",
        result_text(&result)
    );

    // The write path refreshes the index synchronously, so search sees it.
    let result = client
        .call_tool(call(
            "search_notes",
            serde_json::json!({ "query": "zebra" }),
        ))
        .await
        .expect("search_notes call");
    assert!(
        result_text(&result).contains(path),
        "got: {}",
        result_text(&result)
    );

    client.cancel().await.expect("client cancel");
    server.stop().await;
}

/// rmcp's inbound Host-header validation (the DNS-rebinding guard): with
/// `allowed_hosts` configured, only the listed hosts may reach the MCP
/// endpoint — even with a valid bearer token.
#[tokio::test]
async fn allowed_hosts_guard_rejects_foreign_hosts() {
    let server = TestServer::start_with_hosts(Some(vec!["vault.example".into()])).await;
    let base = format!("http://{}/mcp", server.addr);
    let client = reqwest::Client::new();

    const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"probe","version":"1.0"}}}"#;

    let post = |host: &str| {
        client
            .post(&base)
            .header("host", host)
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(INIT)
    };

    // A listed host reaches the endpoint.
    let resp = post("vault.example").send().await.expect("allowed host");
    assert_eq!(
        resp.status(),
        200,
        "the configured host must be served (got {:?})",
        resp.status()
    );
    drop(resp);

    // A foreign Host header — the DNS-rebinding payload, where a browser
    // resolves attacker.example to this server's IP — is refused even
    // with a valid token.
    for hostile in ["evil.example", "localhost", "vault.example.evil.io"] {
        let resp = post(hostile).send().await.expect("foreign host");
        assert!(
            resp.status().is_client_error() || resp.status().is_server_error(),
            "host {hostile:?} must be rejected with an HTTP error, got {:?}",
            resp.status()
        );
    }

    server.stop().await;
}

/// Hostile HTTP bodies with a *valid* token: oversized payloads,
/// non-JSON, and JSON that is not a JSON-RPC message are all rejected
/// without wedging the server — a subsequent initialize still succeeds.
#[tokio::test]
async fn hostile_http_bodies_are_rejected_without_wedging() {
    let server = TestServer::start().await;
    let base = format!("http://{}/mcp", server.addr);
    let client = reqwest::Client::new();

    const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"probe","version":"1.0"}}}"#;

    let hostile_bodies: Vec<(&str, reqwest::Body)> = vec![
        ("non-json", reqwest::Body::from("this is not json at all")),
        ("json but not rpc", reqwest::Body::from(r#"{"foo": 1}"#)),
        ("empty", reqwest::Body::from("")),
        (
            "10 MB of bytes",
            reqwest::Body::from("a".repeat(10 * 1024 * 1024)),
        ),
    ];
    for (label, body) in hostile_bodies {
        let resp = client
            .post(&base)
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(body)
            .send()
            .await
            .expect("hostile body post");
        assert!(
            resp.status().is_client_error() || resp.status().is_server_error(),
            "{label} must be rejected with an HTTP error, got {:?}",
            resp.status()
        );
        drop(resp);
    }

    // None of the probes may have wedged the server.
    let resp = client
        .post(&base)
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(INIT)
        .send()
        .await
        .expect("recovery post");
    assert_eq!(resp.status(), 200);
    drop(resp);

    server.stop().await;
}

/// Two concurrent MCP sessions share one vault: a write through one is
/// immediately visible through the other, and cancelling one session
/// leaves the other fully functional.
#[tokio::test]
async fn concurrent_sessions_share_the_vault() {
    let server = TestServer::start().await;
    let alpha = connect(&server.addr, TOKEN).await.expect("alpha init");
    let beta = connect(&server.addr, TOKEN).await.expect("beta init");

    // Alpha writes; beta sees it without any restart or resync.
    let result = alpha
        .call_tool(call(
            "write_note",
            serde_json::json!({ "path": "shared.md", "content": "from alpha\n" }),
        ))
        .await
        .expect("alpha write");
    assert_ne!(result.is_error, Some(true));
    let result = beta
        .call_tool(call(
            "read_note",
            serde_json::json!({ "path": "shared.md" }),
        ))
        .await
        .expect("beta read");
    assert_ne!(result.is_error, Some(true));
    assert!(result_text(&result).contains("from alpha"));

    // Beta deletes; alpha sees the note gone.
    let result = beta
        .call_tool(call(
            "delete_note",
            serde_json::json!({ "path": "shared.md" }),
        ))
        .await
        .expect("beta delete");
    assert_ne!(result.is_error, Some(true));
    let result = alpha
        .call_tool(call(
            "read_note",
            serde_json::json!({ "path": "shared.md" }),
        ))
        .await
        .expect("alpha read");
    assert_eq!(result.is_error, Some(true));

    // Cancelling one session does not disturb the other.
    alpha.cancel().await.expect("alpha cancel");
    let result = beta
        .call_tool(call("list_notes", serde_json::json!({})))
        .await
        .expect("beta list after alpha cancel");
    assert_ne!(result.is_error, Some(true));
    assert!(
        !result_text(&result).contains("shared.md"),
        "got: {}",
        result_text(&result)
    );

    beta.cancel().await.expect("beta cancel");
    server.stop().await;
}
