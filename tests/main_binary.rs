//! Binary-level tests: the real `mcp-obsidian` executable wired end to
//! end — env config, vault open, index build, auth, a JSON-RPC round
//! trip, and graceful SIGTERM shutdown (what systemd/Docker send).
//! Unix-only: the ob stub needs the executable bit and the shutdown path
//! under test is SIGTERM.

#![cfg(unix)]

use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use tokio::process::{Child, Command};

const TOKEN: &str = "binary-test-token";
const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"probe","version":"1.0"}}}"#;

fn set_executable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .expect("stub metadata")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("chmod stub");
}

/// The same ob stub as the http tests use: one-shot subcommands answer
/// and exit; `sync --continuous` (what the supervisor runs) stays alive.
fn ob_stub(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("ob-stub");
    std::fs::write(
        &path,
        "#!/bin/sh\ncase \"$1\" in\n  sync-status) echo 'status: synced (stub)'; exit 0;;\n  sync)\n    case \"$2\" in\n      --continuous) ;;\n      *) echo 'synced (stub)'; exit 0;;\n    esac\n    ;;\nesac\nexec sleep 300\n",
    )
    .expect("write ob stub");
    set_executable(&path);
    path
}

/// A ready-to-serve vault fixture with one note.
fn vault(dir: &std::path::Path) {
    std::fs::write(
        dir.join("hello.md"),
        "---\ntitle: Hello\n---\n# Hello\n\nbinary test marker.\n",
    )
    .expect("write hello.md");
}

/// Allocates a free localhost port by binding, reading, and dropping.
fn free_addr() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    format!("127.0.0.1:{port}")
}

fn spawn_server(
    vault_path: &std::path::Path,
    index_dir: &std::path::Path,
    ob: &std::path::Path,
    bind_addr: &str,
    auth_token: Option<&str>,
) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mcp-obsidian"));
    command
        .env("VAULT_PATH", vault_path)
        .env("INDEX_DIR", index_dir)
        .env("BIND_ADDR", bind_addr)
        .env("OB_BIN", ob)
        // Never inherit a token from the test environment: the missing-
        // token case must fail on its own merits.
        .env_remove("AUTH_TOKEN")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(token) = auth_token {
        command.env("AUTH_TOKEN", token);
    }
    command.spawn().expect("spawn mcp-obsidian")
}

async fn wait_listening(addr: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if TcpStream::connect(addr).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// One raw JSON-RPC POST to /mcp with the given bearer token and body.
/// Returns the response status and body text.
async fn rpc(
    addr: &str,
    token: &str,
    body: &str,
    session: Option<&str>,
) -> (reqwest::StatusCode, String, Option<String>) {
    let mut request = reqwest::Client::new()
        .post(format!("http://{addr}/mcp"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body.to_owned());
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    let response = request.send().await.expect("rpc post");
    let session = response
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    (status, text, session)
}

/// The real binary, configured purely through the environment (as under
/// systemd/Docker), serves MCP traffic and shuts down cleanly on SIGTERM.
#[tokio::test]
async fn binary_serves_and_shuts_down_cleanly() {
    let vault_dir = tempfile::tempdir().expect("vault tempdir");
    vault(vault_dir.path());
    let index_dir = tempfile::tempdir().expect("index tempdir");
    let ob = ob_stub(vault_dir.path());
    let addr = free_addr();

    let mut child = spawn_server(vault_dir.path(), index_dir.path(), &ob, &addr, Some(TOKEN));
    assert!(
        wait_listening(&addr, Duration::from_secs(15)).await,
        "server did not start listening"
    );

    // Auth is wired: no token is refused even at the binary level.
    let (status, _, _) = rpc(&addr, "", INIT, None).await;
    assert_eq!(status, 401);

    // A full initialize/tools-list round trip proves the whole stack:
    // config parsing, vault open, index build, router, auth, and the
    // tool registry all came up together.
    let (status, body, session) = rpc(&addr, TOKEN, INIT, None).await;
    assert_eq!(status, 200, "initialize failed: {body}");
    let session = session.expect("initialize must assign a session id");

    let (status, _, _) = rpc(
        &addr,
        TOKEN,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        Some(&session),
    )
    .await;
    assert_eq!(status, 202, "initialized notification failed: {status:?}");

    let (status, body, _) = rpc(
        &addr,
        TOKEN,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        Some(&session),
    )
    .await;
    assert_eq!(status, 200, "tools/list failed: {body}");
    assert!(
        body.contains("read_note") && body.contains("write_note"),
        "tool registry missing tools: {body}"
    );

    // The server must still be alive before the signal: an early exit
    // would make the shutdown assertions vacuous.
    assert!(
        child.try_wait().expect("try_wait on server").is_none(),
        "server must still be running before SIGTERM"
    );

    // SIGTERM (systemctl stop / docker stop): graceful exit, code 0.
    let kill_status = Command::new("kill")
        .arg(child.id().expect("child pid").to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .expect("kill command runs");
    assert!(kill_status.success(), "kill(1) must succeed");
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("server exits within 15s of SIGTERM")
        .expect("wait on server");
    assert!(
        status.success(),
        "graceful shutdown must exit 0, got {status}"
    );

    // The port is released again (the process really is gone).
    assert!(TcpStream::connect(&addr).await.is_err());
}

/// The listener is never reachable without a token: a missing AUTH_TOKEN
/// is a startup failure, not an open mode.
#[tokio::test]
async fn binary_refuses_to_start_without_a_token() {
    let vault_dir = tempfile::tempdir().expect("vault tempdir");
    vault(vault_dir.path());
    let index_dir = tempfile::tempdir().expect("index tempdir");
    let ob = ob_stub(vault_dir.path());

    let mut child = spawn_server(vault_dir.path(), index_dir.path(), &ob, "127.0.0.1:0", None);
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("exits promptly without AUTH_TOKEN")
        .expect("wait on server");
    assert_eq!(
        status.code(),
        Some(1),
        "missing AUTH_TOKEN must be exit code 1"
    );
}

/// A bad vault path is a startup failure with a distinct exit code, not
/// a per-request error.
#[tokio::test]
async fn binary_fails_fast_on_a_missing_vault() {
    let vault_dir = tempfile::tempdir().expect("tempdir");
    let missing = vault_dir.path().join("no-such-vault");
    let index_dir = tempfile::tempdir().expect("index tempdir");
    let ob = ob_stub(vault_dir.path());

    let mut child = spawn_server(&missing, index_dir.path(), &ob, "127.0.0.1:0", Some(TOKEN));
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("exits promptly on a missing vault")
        .expect("wait on server");
    assert_eq!(status.code(), Some(1), "missing vault must be exit code 1");
}
