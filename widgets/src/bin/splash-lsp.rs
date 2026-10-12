//! `splash-lsp` — the `splashcheck` core as a Language Server, on stdio.
//!
//! Two threads, and the split is forced by the framework: makepad's `Cx` holds
//! `Rc`, `Cell` and raw pointers, so it is not `Send` — the context the
//! validator evaluates in cannot leave the thread that owns it. So the main
//! thread owns the [`Server`] and stdout, and the only other thread is a reader
//! that moves *bytes* (which are `Send`) from stdin into a channel.
//!
//! The main loop then does what a single-threaded editor server should: wait
//! for a message, and when none arrives within a tick, check whatever document
//! has stopped changing. A keystroke costs a message, not a validation.

use std::io::{Read, Write};
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use makepad_widgets::splash_lsp::{Server, DEFAULT_DEBOUNCE};

/// How often the main loop looks for a settled document when no message
/// arrives. The debounce is what decides when a check happens; this is only how
/// promptly it notices.
const TICK: Duration = Duration::from_millis(25);

/// Read one LSP message: headers, then a body of `Content-Length` bytes.
fn read_message(stdin: &mut impl Read) -> Option<String> {
    let mut content_length = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        let mut byte = [0u8; 1];
        let mut in_line = false;
        loop {
            match stdin.read(&mut byte) {
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
    stdin.read_exact(&mut body).ok()?;
    Some(String::from_utf8_lossy(&body).into_owned())
}

fn frame(message: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{}", message.len(), message)
}

fn main() -> ExitCode {
    // The debounce is configurable so a test can ask for immediate checks, and
    // so a machine that struggles with a 130 KB app can raise it.
    let debounce = std::env::var("SPLASH_LSP_DEBOUNCE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_DEBOUNCE);

    // The reader: bytes in, messages out. It holds no server state, which is
    // what lets it be a thread at all.
    let (messages, inbox) = mpsc::channel::<String>();
    thread::spawn(move || {
        let mut stdin = std::io::stdin();
        while let Some(body) = read_message(&mut stdin) {
            if messages.send(body).is_err() {
                break;
            }
        }
    });

    let mut server = Server::new(debounce);
    let mut stdout = std::io::stdout();
    let mut clean_exit = false;
    loop {
        match inbox.recv_timeout(TICK) {
            Ok(body) => {
                let Ok(message) = serde_json::from_str::<serde_json::Value>(&body) else {
                    continue;
                };
                for reply in server.handle(&message) {
                    let _ = stdout.write_all(frame(&reply.to_string()).as_bytes());
                }
                let _ = stdout.flush();
                clean_exit = server.shutdown();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                for notification in server.poll() {
                    let _ = stdout.write_all(frame(&notification.to_string()).as_bytes());
                }
                let _ = stdout.flush();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if clean_exit {
            break;
        }
    }
    // Per the protocol, `exit` after `shutdown` is a clean end; anything else
    // is not.
    if clean_exit {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
