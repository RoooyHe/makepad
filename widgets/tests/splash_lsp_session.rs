//! A whole LSP session over a real pipe: the framing, the threads and the
//! debounce that the unit tests cannot see.
//!
//! The unit tests drive `Server` directly; this drives the binary the way an
//! editor does, over stdin and stdout, which is the only way to know the
//! protocol half works.

use std::io::{BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// A temp app: a manifest that declares one capability, and a source that asks
/// for another — so both the eval tier and the capability tier have something
/// to say.
const MANIFEST: &str = r#"{"schema":1,"id":"dev.example.lsp","capabilities":["storage"]}"#;
const SOURCE: &str = "View{\n    width: Fill height: Fit\n    Label{text: nope}\n}\nfn load(){ host.request(\"news.list\", {}, fn(r){}) }\n";

fn frame(body: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{}", body.len(), body)
}

/// Read one framed message from a stream, or `None` at EOF.
fn read_message(reader: &mut impl Read) -> Option<String> {
    let mut content_length = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        let mut byte = [0u8; 1];
        let mut in_line = false;
        loop {
            match reader.read(&mut byte) {
                Ok(0) => return None,
                Ok(_) => {
                    in_line = true;
                    if byte[0] == b'\n' {
                        break;
                    }
                    if byte[0] != b'\r' {
                        line.push(byte[0]);
                    }
                }
                Err(_) => return None,
            }
        }
        if !in_line {
            return None;
        }
        if line.is_empty() {
            break;
        }
        let text = String::from_utf8_lossy(&line);
        if let Some((name, value)) = text.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
    }
    let mut body = vec![0u8; content_length?];
    reader.read_exact(&mut body).ok()?;
    Some(String::from_utf8_lossy(&body).into_owned())
}

/// A session: the server process, and a channel of everything it says.
struct Session {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    messages: mpsc::Receiver<String>,
}

impl Session {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_splash-lsp"))
            .env("SPLASH_LSP_DEBOUNCE_MS", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn splash-lsp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, rx) = mpsc::channel();
        // A reader thread, so a wait is a wait and not a hang.
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Some(message) = read_message(&mut reader) {
                if tx.send(message).is_err() {
                    break;
                }
            }
        });
        Self { child, stdin, messages: rx }
    }

    fn send(&mut self, body: &str) {
        self.stdin.write_all(frame(body).as_bytes()).expect("write");
        let _ = self.stdin.flush();
    }

    /// The next message, or None if none arrives in time.
    fn next(&self, within: Duration) -> Option<String> {
        self.messages.recv_timeout(within).ok()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn temp_app(name: &str) -> (PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("splash-lsp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("manifest.json"), MANIFEST).expect("manifest");
    std::fs::write(dir.join("main.splash"), SOURCE).expect("source");
    // A file URI for a Windows or Unix path.
    let uri = format!("file:///{}", dir.join("main.splash").display().to_string().replace('\\', "/"));
    (dir, uri)
}

#[test]
fn a_session_answers_initialize_and_publishes_findings() {
    let (dir, uri) = temp_app("session");
    let mut session = Session::spawn();

    session.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
    let reply = session.next(Duration::from_secs(10)).expect("the initialize reply");
    assert!(reply.contains("textDocumentSync"), "{reply}");
    assert!(reply.contains("splash-lsp"), "{reply}");

    session.send(
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {"textDocument": {"uri": uri, "languageId": "splash", "version": 1, "text": SOURCE}},
        })
        .to_string(),
    );

    // The debounce is zero, so a publishDiagnostics arrives without an edit.
    let mut diagnostics = None;
    for _ in 0..40 {
        match session.next(Duration::from_secs(5)) {
            Some(message) if message.contains("publishDiagnostics") => {
                diagnostics = Some(message);
                break;
            }
            Some(_) => continue,
            None => break,
        }
    }
    let diagnostics = diagnostics.expect("a publishDiagnostics notification");
    let value: serde_json::Value = serde_json::from_str(&diagnostics).expect("the notification is JSON");
    let list = value["params"]["diagnostics"].as_array().expect("a diagnostics array");
    let messages: Vec<&str> = list.iter().filter_map(|d| d["message"].as_str()).collect();

    // The eval tier: the undeclared name, at its own line and column.
    let name = list
        .iter()
        .find(|d| d["message"].as_str().is_some_and(|m| m.contains("nope not found in scope")))
        .expect("the name finding");
    assert_eq!(name["range"]["start"]["line"], 2, "{name:?}");
    assert_eq!(name["range"]["start"]["character"], 16, "{name:?}");
    assert_eq!(name["range"]["end"]["character"], 20, "the name's own range");
    assert_eq!(name["severity"], 1);
    assert_eq!(name["source"], "splashcheck");

    // The capability tier: a host service the manifest does not declare.
    assert!(
        messages.iter().any(|m| m.contains("host.request(\"news.list\")") && m.contains("needs the")),
        "{messages:?}"
    );
    // And the tool's own note is a log line, not a squiggle.
    assert!(!messages.iter().any(|m| m.contains("capability checks skipped")), "{messages:?}");

    session.send(
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/codeAction",
            "params": {
                "textDocument": {"uri": uri},
                "range": {"start": {"line": 2, "character": 17}},
                "context": {"diagnostics": []},
            },
        })
        .to_string(),
    );
    let reply = session.next(Duration::from_secs(10)).expect("the code action reply");
    assert!(reply.contains("Replace with '"), "{reply}");
    assert!(reply.contains("\"newText\""), "{reply}");

    session.send(r#"{"jsonrpc":"2.0","id":3,"method":"shutdown"}"#);
    let reply = session.next(Duration::from_secs(10)).expect("the shutdown reply");
    assert!(reply.contains("\"result\":null"), "{reply}");
    session.send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    let status = session.child.wait_timeout(Duration::from_secs(10));
    assert!(status.is_some(), "the process exits on `exit`");

    let _ = std::fs::remove_dir_all(&dir);
}

/// `wait_timeout` is not in std; this is the small version of it.
trait WaitTimeout {
    fn wait_timeout(&mut self, within: Duration) -> Option<()>;
}

impl WaitTimeout for std::process::Child {
    fn wait_timeout(&mut self, within: Duration) -> Option<()> {
        let start = std::time::Instant::now();
        loop {
            if self.try_wait().ok()?.is_some() {
                return Some(());
            }
            if start.elapsed() >= within {
                return None;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}
