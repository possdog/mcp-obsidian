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
            ob_bin.as_str(),
            supervisor.clone(),
        );
        let cancellation = CancellationToken::new();
        let router = server::router(ctx, TOKEN, cancellation.clone(), None);

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

    let result = client
        .call_tool(call("sync_status", serde_json::json!({})))
        .await
        .expect("sync_status call");
    let text = result_text(&result);
    assert!(text.contains("daemon: running"), "got: {text}");
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
