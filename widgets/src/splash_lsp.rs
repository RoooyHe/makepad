//! `splash-lsp`: the `splashcheck` core as a Language Server.
//!
//! An editor is where a card or an app is written, so it is where a finding is
//! worth the most: the same analysis, at the moment of typing. This module is
//! the protocol half — documents, diagnostics, code actions — and it owns the
//! one expensive thing, the host context the validator evaluates in, built
//! once and reused.
//!
//! The binary (`src/bin/splash-lsp.rs`) is the transport half: LSP framing on
//! stdio, and a worker that checks a document once it has stopped changing.
//! Nothing here knows about threads or pipes, which is what makes the whole
//! session testable: [`Server::handle`] takes a message and returns the
//! messages to write back.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::splashcheck::{self, FileReport, Severity};

/// How long after the last edit a document is checked. An editor sends a
/// didChange per keystroke, and checking a 130 KB app takes ~200 ms: doing it
/// per keystroke would lag the editor and hold a core for nothing.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(300);

/// One open document.
pub struct Document {
    pub text: String,
    pub version: i64,
    /// When the text last changed, and has not been checked since.
    pub dirty_since: Option<Instant>,
}

/// The server: the open documents and the context they are checked in.
pub struct Server {
    cx: crate::Cx,
    documents: BTreeMap<String, Document>,
    /// The diagnostics last published for a document. A code action needs the
    /// finding's own message — its suggestions — and re-checking the document
    /// to answer a keypress would be both slow and racy.
    published: BTreeMap<String, Vec<Value>>,
    debounce: Duration,
    shutdown: bool,
}

impl Server {
    pub fn new(debounce: Duration) -> Self {
        let mut cx = crate::Cx::new(Box::new(|_, _| {}));
        cx.with_vm(crate::script_mod);
        Self { cx, documents: BTreeMap::new(), published: BTreeMap::new(), debounce, shutdown: false }
    }

    /// Whether an `exit` notification arrived, and the process should end.
    pub fn shutdown(&self) -> bool {
        self.shutdown
    }

    /// Handle one message, returning the messages to write back.
    pub fn handle(&mut self, message: &Value) -> Vec<Value> {
        let method = message.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => vec![response(id, initialize_result())],
            "shutdown" => {
                self.shutdown = true;
                vec![response(id, Value::Null)]
            }
            "exit" => {
                self.shutdown = true;
                Vec::new()
            }
            "initialized" => Vec::new(),
            "textDocument/didOpen" => {
                if let Some(document) = params.get("textDocument") {
                    let uri = string_field(document, "uri");
                    let text = string_field(document, "text");
                    let version = document.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
                    self.documents.insert(uri, Document { text, version, dirty_since: Some(Instant::now()) });
                }
                Vec::new()
            }
            "textDocument/didChange" => {
                if let Some(document) = params.get("textDocument") {
                    let uri = string_field(document, "uri");
                    let version = document.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
                    if let Some(changes) = params.get("contentChanges").and_then(|c| c.as_array()) {
                        // Full sync: the last change carries the whole text.
                        if let Some(text) = changes.last().and_then(|c| c.get("text")).and_then(|t| t.as_str()) {
                            if let Some(document) = self.documents.get_mut(&uri) {
                                document.text = text.to_owned();
                                document.version = version;
                                document.dirty_since = Some(Instant::now());
                            }
                        }
                    }
                }
                Vec::new()
            }
            "textDocument/didSave" => {
                if let Some(uri) = params.get("textDocument").map(|d| string_field(d, "uri")).filter(|u| !u.is_empty()) {
                    if let Some(document) = self.documents.get_mut(&uri) {
                        document.dirty_since = Some(Instant::now());
                    }
                }
                Vec::new()
            }
            "textDocument/didClose" => {
                let uri = string_field(&params.get("textDocument").cloned().unwrap_or(Value::Null), "uri");
                self.documents.remove(&uri);
                self.published.remove(&uri);
                vec![publish(uri, Vec::new())]
            }
            "textDocument/codeAction" => {
                let uri = string_field(&params.get("textDocument").cloned().unwrap_or(Value::Null), "uri");
                vec![response(id, Value::Array(self.code_actions(&uri, &params)))]
            }
            "textDocument/definition" => {
                let uri = string_field(&params.get("textDocument").cloned().unwrap_or(Value::Null), "uri");
                vec![response(id, self.definitions(&uri, &params))]
            }
            "textDocument/documentSymbol" => {
                let uri = string_field(&params.get("textDocument").cloned().unwrap_or(Value::Null), "uri");
                vec![response(id, self.document_symbols(&uri))]
            }
            other => {
                // A request we do not implement is answered; a notification we
                // do not know is dropped, as the protocol requires.
                match id {
                    Some(id) => vec![error(id, -32601, format!("method not found: {other}"))],
                    None => Vec::new(),
                }
            }
        }
    }

    /// Check every document whose debounce has elapsed, returning the
    /// messages to send.
    pub fn poll(&mut self) -> Vec<Value> {
        let now = Instant::now();
        let ready: Vec<String> = self
            .documents
            .iter()
            .filter(|(_, document)| document.dirty_since.is_some_and(|since| now.duration_since(since) >= self.debounce))
            .map(|(uri, _)| uri.clone())
            .collect();
        let mut out = Vec::new();
        for uri in ready {
            out.extend(self.check(&uri));
        }
        out
    }

    /// Check one document and publish its findings.
    ///
    /// Returns the messages to send: the diagnostics, and — when the check had
    /// something to say about itself rather than about the code — a log line.
    /// A note is not a position in the app, and an editor that showed one as a
    /// squiggle would be showing the tool's own business in the user's file.
    fn check(&mut self, uri: &str) -> Vec<Value> {
        let Some(document) = self.documents.get_mut(uri) else {
            return vec![publish(uri.to_owned(), Vec::new())];
        };
        document.dirty_since = None;
        let text = document.text.clone();
        let Some(path) = uri_to_path(uri) else {
            return vec![publish(uri.to_owned(), Vec::new())];
        };
        let invocation = splashcheck::Invocation {
            paths: vec![path.clone()],
            tier: splashcheck::Tier::All,
            baseline: None,
            write_baseline: false,
            json: false,
        };
        let report = splashcheck::check_source(&path, &text, &invocation, &mut self.cx);
        let diagnostics = diagnostics(&report, &text);
        self.published.insert(uri.to_owned(), diagnostics.clone());
        let mut out = vec![publish(uri.to_owned(), diagnostics)];
        for note in &report.notes {
            out.push(json!({
                "jsonrpc": "2.0",
                "method": "window/logMessage",
                "params": {"type": 3, "message": format!("{}: {note}", uri)},
            }));
        }
        out
    }

    /// The quick fixes a document offers at a position.
    ///
    /// Only the ones that are an edit of the document itself: replacing a name
    /// with one of the checker's suggestions. A capability the manifest does
    /// not declare is a finding about another file, and offering to edit it
    /// blind is how a tool makes a mess.
    fn code_actions(&self, uri: &str, params: &Value) -> Vec<Value> {
        let Some(document) = self.documents.get(uri) else {
            return Vec::new();
        };
        let Some(range) = params.get("range") else {
            return Vec::new();
        };
        let line = range.get("start").and_then(|s| s.get("line")).and_then(|l| l.as_u64()).unwrap_or(0) as usize;
        let character = range.get("start").and_then(|s| s.get("character")).and_then(|c| c.as_u64()).unwrap_or(0) as usize;
        let Some(text) = document.text.lines().nth(line) else {
            return Vec::new();
        };
        // The word under the cursor, so the edit range is the name and not the
        // whole line.
        let Some((word, word_start)) = word_at(text, character) else {
            return Vec::new();
        };
        // The finding that covers it: its message carries the suggestions.
        let suggestions = self
            .published
            .get(uri)
            .into_iter()
            .flatten()
            .filter(|diagnostic| diagnostic["range"]["start"]["line"].as_u64() == Some(line as u64))
            .filter(|diagnostic| {
                diagnostic["range"]["start"]["character"].as_u64().is_some_and(|at| at <= character as u64)
                    && diagnostic["range"]["end"]["character"].as_u64().is_some_and(|end| end >= character as u64)
            })
            .filter_map(|diagnostic| diagnostic["message"].as_str())
            .filter(|message| splashcheck::named_in_message(message).as_deref() == Some(word.as_str()))
            .flat_map(splashcheck::suggestions)
            .take(3)
            .collect::<Vec<String>>();
        let mut actions = Vec::new();
        for suggestion in suggestions {
            let edit = json!({
                "changes": {
                    uri: [{
                        "range": {
                            "start": {"line": line, "character": word_start},
                            "end": {"line": line, "character": word_start + word.chars().count()},
                        },
                        "newText": suggestion,
                    }]
                }
            });
            actions.push(json!({
                "title": format!("Replace with '{suggestion}'"),
                "kind": "quickfix",
                "edit": edit,
            }));
        }
        actions
    }

    /// Where a name is declared, for go-to-definition.
    ///
    /// A textual scan, and honestly so: the VM holds the real scopes but
    /// exposes no "where is this name declared" query, and a DSL whose only
    /// binding forms are `let`, `fn` and `:=` is one where the text is nearly
    /// the truth. Every declaration of the name is returned, because a `let`
    /// inside a block shadows and the reader can pick.
    fn definitions(&self, uri: &str, params: &Value) -> Value {
        let Some(document) = self.documents.get(uri) else {
            return Value::Null;
        };
        let Some(range) = params.get("range") else {
 return Value::Null;
        };
        let line = range.get("start").and_then(|s| s.get("line")).and_then(|l| l.as_u64()).unwrap_or(0) as usize;
        let character = range.get("start").and_then(|s| s.get("character")).and_then(|c| c.as_u64()).unwrap_or(0) as usize;
        let Some(text) = document.text.lines().nth(line) else {
            return Value::Null;
        };
        let Some((word, _)) = word_at(text, character) else {
            return Value::Null;
        };
        let found: Vec<Value> = declarations(&document.text)
            .into_iter()
            .filter(|symbol| symbol.name == word)
            .map(|symbol| {
                json!({
                    "uri": uri,
                    "range": {
                        "start": {"line": symbol.line, "character": symbol.column},
                        "end": {"line": symbol.line, "character": symbol.column + symbol.name.chars().count()},
                    }
                })
            })
            .collect();
        if found.is_empty() {
            Value::Null
        } else {
            Value::Array(found)
        }
    }

    /// The document's declarations, for the outline view.
    fn document_symbols(&self, uri: &str) -> Value {
        let Some(document) = self.documents.get(uri) else {
            return Value::Array(Vec::new());
        };
        Value::Array(
            declarations(&document.text)
                .into_iter()
                .map(|symbol| {
                    json!({
                        "name": symbol.name,
                        "kind": symbol.kind,
                        "location": {
                            "uri": uri,
                            "range": {
                                "start": {"line": symbol.line, "character": symbol.column},
                                "end": {"line": symbol.line, "character": symbol.column + symbol.name.chars().count()},
                            }
                        },
                    })
                })
                .collect::<Vec<Value>>(),
        )
    }
}

/// What a declaration is, in LSP's symbol kinds.
pub const SYMBOL_FUNCTION: u64 = 12;
pub const SYMBOL_VARIABLE: u64 = 13;

/// One binding a document declares.
#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    /// 0-based, as LSP wants.
    pub line: usize,
    /// 0-based character offset.
    pub column: usize,
    pub kind: u64,
}

/// Every binding a document declares: `let name = …`, `fn name(…)`, and
/// `name := …` (a named child or property, the DSL's third binding form).
///
/// A `for` binder is deliberately not one: it is scoped to its own loop, and
/// offering to jump to it from outside would be wrong.
pub fn declarations(source: &str) -> Vec<Symbol> {
    let mut out = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let start = line.len() - line.trim_start().len();
        let indent = line[..start].chars().count();
        let trimmed = &line[start..];
        for (kind, keyword) in [(SYMBOL_VARIABLE, "let"), (SYMBOL_FUNCTION, "fn")] {
            let Some(rest) = trimmed.strip_prefix(keyword) else { continue };
            // A keyword is a whole word: `letter` does not begin with `let`.
            if !rest.starts_with(|c: char| c.is_whitespace()) {
                continue;
            }
            let after = rest.trim_start();
            let column = indent + keyword.chars().count() + (rest.chars().count() - after.chars().count());
            if let Some(name) = leading_identifier(after) {
                out.push(Symbol { name, line: index, column, kind });
            }
        }
        // `name := …`, whether it starts the line or sits inside a constructor.
        if let Some(at) = line.find(":=") {
            if let Some((name, column)) = trailing_identifier(&line[..at]) {
                out.push(Symbol { name, line: index, column, kind: SYMBOL_VARIABLE });
            }
        }
    }
    out
}

/// The identifier at the start of `text`.
fn leading_identifier(text: &str) -> Option<String> {
    let end = text.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(text.len());
    let name = &text[..end];
    (!name.is_empty() && name.starts_with(|c: char| c.is_alphabetic() || c == '_')).then(|| name.to_owned())
}

/// The identifier that ends `text` — the name before a `:=` — and its column.
fn trailing_identifier(text: &str) -> Option<(String, usize)> {
    let trimmed = text.trim_end();
    let start = trimmed.rfind(|c: char| !(c.is_alphanumeric() || c == '_')).map(|at| at + 1).unwrap_or(0);
    let name = &trimmed[start..];
    (!name.is_empty() && name.starts_with(|c: char| c.is_alphabetic() || c == '_'))
        .then(|| (name.to_owned(), trimmed[..start].chars().count()))
}

fn initialize_result() -> Value {
    json!({
        "capabilities": {
            "textDocumentSync": 1,
            "codeActionProvider": true,
            "definitionProvider": true,
            "documentSymbolProvider": true,
            "serverInfo": {"name": "splash-lsp", "version": env!("CARGO_PKG_VERSION")},
        }
    })
}

fn response(id: Option<Value>, result: Value) -> Value {
    match id {
        Some(id) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        None => Value::Null,
    }
}

fn error(id: Value, code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn publish(uri: String, diagnostics: Vec<Value>) -> Value {
    json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": uri, "diagnostics": diagnostics}})
}

/// A report's findings as LSP diagnostics.
///
/// The range is the name the message names, when it is on the line — the same
/// refinement the command line makes, and for the same reason: the VM's own
/// column is the token the interpreter was at. Positions are UTF-16, as LSP
/// requires, which is not the same as characters when a line holds CJK.
fn diagnostics(report: &FileReport, text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for finding in &report.findings {
        let Some(line) = finding.line else {
            // A whole-file finding: the first line, so it is seen.
            out.push(json!({
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                "severity": severity(finding.severity),
                "code": finding.code,
                "source": "splashcheck",
                "message": finding.message,
            }));
            continue;
        };
        let line_index = line.saturating_sub(1);
        let line_text = text.lines().nth(line_index).unwrap_or("");
        let (start, end) = match finding.column.zip(splashcheck::named_in_message(&finding.message)) {
            Some((_column, name)) => match splashcheck::find_word(line_text, &name) {
                Some(at) => (at, at + name.chars().count()),
                None => (0, line_text.chars().count()),
            },
            _ => (0, line_text.chars().count()),
        };
        out.push(json!({
            "range": {
                "start": {"line": line_index, "character": utf16_prefix(line_text, start)},
                "end": {"line": line_index, "character": utf16_prefix(line_text, end)},
            },
            "severity": severity(finding.severity),
            "code": finding.code,
            "source": "splashcheck",
            "message": finding.message,
        }));
    }
    if let Some(fatal) = &report.fatal {
        out.push(json!({
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
            "severity": 1,
            "source": "splashcheck",
            "message": fatal,
        }));
    }
    out
}

fn severity(severity: Severity) -> u64 {
    match severity {
        Severity::Error => 1,
        Severity::Warn => 2,
    }
}

/// The UTF-16 length of the first `chars` characters of a line.
fn utf16_prefix(line: &str, chars: usize) -> usize {
    line.chars().take(chars).map(char::len_utf16).sum()
}

/// The identifier at a character offset, and where it starts.
fn word_at(line: &str, character: usize) -> Option<(String, usize)> {
    let mut start = None;
    let mut end = 0usize;
    for (at, c) in line.chars().enumerate() {
        let is_ident = c.is_alphanumeric() || c == '_';
        if is_ident && start.is_none() {
            start = Some(at);
        }
        if !is_ident && start.is_some() {
            if at > character {
                end = at;
                break;
            }
            start = None;
        }
    }
    let start = start?;
    let end = if end > start { end } else { line.chars().count() };
    let word: String = line.chars().skip(start).take(end - start).collect();
    (!word.is_empty()).then_some((word, start))
}

fn string_field(value: &Value, field: &str) -> String {
    value.get(field).and_then(|v| v.as_str()).unwrap_or("").to_owned()
}

/// A `file://` URI as a path, percent-decoded.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // file:///C:/… on Windows, file:///home/… elsewhere: the host is empty in
    // both, and what follows is the path.
    let path = rest.strip_prefix('/').unwrap_or(rest);
    let decoded = percent_decode(path);
    Some(PathBuf::from(decoded))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' && at + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[at + 1..at + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(byte) = hex {
                out.push(byte);
                at += 3;
                continue;
            }
        }
        out.push(bytes[at]);
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        Server::new(Duration::ZERO)
    }

    fn open(server: &mut Server, uri: &str, text: &str) {
        let message = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {"textDocument": {"uri": uri, "languageId": "splash", "version": 1, "text": text}},
        });
        server.handle(&message);
    }

    fn diagnostics_of(server: &mut Server, uri: &str) -> Vec<Value> {
        // The debounce is zero, so the next poll checks it. Notes come back as
        // log messages, which are not diagnostics.
        server
            .poll()
            .into_iter()
            .filter(|message| message.get("method").and_then(|m| m.as_str()) == Some("textDocument/publishDiagnostics"))
            .filter(|message| message.get("params").and_then(|p| p.get("uri")).and_then(|u| u.as_str()) == Some(uri))
            .flat_map(|message| message.get("params").and_then(|p| p.get("diagnostics")).and_then(|d| d.as_array()).cloned().unwrap_or_default())
            .collect()
    }

    #[test]
    fn a_session_answers_initialize_and_shuts_down() {
        let mut server = server();
        let replies = server.handle(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}));
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["id"], 1);
        assert_eq!(replies[0]["result"]["capabilities"]["textDocumentSync"], 1);
        let replies = server.handle(&json!({"jsonrpc":"2.0","id":2,"method":"shutdown"}));
        assert!(replies[0]["result"].is_null());
        assert!(server.shutdown());
    }

    #[test]
    fn a_broken_app_publishes_a_positioned_diagnostic() {
        let mut server = server();
        let uri = "file:///C:/app/bundle/main.splash";
        open(&mut server, uri, "View{\n    width: Fill height: Fit\n    Label{text: nope}\n}\n");
        let diagnostics = diagnostics_of(&mut server, uri);
        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        let first = &diagnostics[0];
        // Line 2 zero-based, and the name's own column — not the interpreter's.
        assert_eq!(first["range"]["start"]["line"], 2);
        assert_eq!(first["range"]["start"]["character"], 16);
        assert_eq!(first["severity"], 1);
        assert_eq!(first["source"], "splashcheck");
        assert!(first["message"].as_str().unwrap().contains("nope"), "{}", first["message"]);
    }

    #[test]
    fn a_clean_app_publishes_nothing() {
        let mut server = server();
        let uri = "file:///C:/app/bundle/main.splash";
        open(&mut server, uri, "View{ width: Fill height: Fit Label{text: \"x\"} }\n");
        let diagnostics = diagnostics_of(&mut server, uri);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn a_closed_document_is_cleared() {
        let mut server = server();
        let uri = "file:///C:/app/bundle/main.splash";
        open(&mut server, uri, "View{ Label{text: nope} }\n");
        assert!(!diagnostics_of(&mut server, uri).is_empty());
        let replies = server.handle(&json!({
            "jsonrpc":"2.0","method":"textDocument/didClose",
            "params":{"textDocument":{"uri":uri}}
        }));
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["method"], "textDocument/publishDiagnostics");
        assert!(replies[0]["params"]["diagnostics"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_change_marks_the_document_dirty_and_a_repoll_checks_it() {
        let mut server = server();
        let uri = "file:///C:/app/bundle/main.splash";
        open(&mut server, uri, "View{ Label{text: \"ok\"} }\n");
        assert!(diagnostics_of(&mut server, uri).is_empty());
        server.handle(&json!({
            "jsonrpc":"2.0","method":"textDocument/didChange",
            "params":{
                "textDocument":{"uri":uri,"version":2},
                "contentChanges":[{"text":"View{ Label{text: nope} }\n"}]
            }
        }));
        let diagnostics = diagnostics_of(&mut server, uri);
        assert!(!diagnostics.is_empty(), "the changed text is checked");
        assert_eq!(diagnostics[0]["range"]["start"]["line"], 0);
    }

    #[test]
    fn a_cjk_column_is_utf16() {
        // The app's own lines hold Chinese, so a character column and a UTF-16
        // offset are not the same thing and an editor needs the second.
        assert_eq!(utf16_prefix("你好", 1), 1);
        assert_eq!(utf16_prefix("你好", 2), 2);
        assert_eq!(utf16_prefix("a𝄞b", 2), 3, "an astral character is two UTF-16 units");
        let line = "    Label{text: \"你好\" nope}";
        assert_eq!(splashcheck::find_word(line, "nope"), Some(21));
        assert_eq!(utf16_prefix(line, 24), 24);
    }

    #[test]
    fn a_suggestion_becomes_a_quick_fix() {
        let mut server = server();
        let uri = "file:///C:/app/bundle/main.splash";
        open(&mut server, uri, "View{\n    NoSuchWidget{text: \"x\"}\n}\n");
        // The cursor on the unknown name, after the diagnostics exist.
        let _ = diagnostics_of(&mut server, uri);
        let replies = server.handle(&json!({
            "jsonrpc":"2.0","id":7,"method":"textDocument/codeAction",
            "params":{
                "textDocument":{"uri":uri},
                "range":{"start":{"line":1,"character":4}},
                "context":{"diagnostics":[]}
            }
        }));
        let actions = replies[0]["result"].as_array().cloned().unwrap_or_default();
        assert!(!actions.is_empty(), "{actions:?}");
        let action = &actions[0];
        assert!(action["title"].as_str().unwrap().starts_with("Replace with '"), "{}", action["title"]);
        let edit = &action["edit"]["changes"][uri][0];
        assert_eq!(edit["range"]["start"]["line"], 1);
        assert_eq!(edit["range"]["start"]["character"], 4);
        assert_eq!(edit["range"]["end"]["character"], 16, "the whole name is replaced: 4 + NoSuchWidget");
    }

    #[test]
    fn declarations_finds_every_binding_form() {
        let source = "let state = {items: []}\nfn load(){ }\n\nView{\n    title := Label{text: \"x\"}\n    Card{body.text: state}\n}\n";
        let found = declarations(source);
        let named: Vec<(&str, usize, usize, u64)> =
            found.iter().map(|s| (s.name.as_str(), s.line, s.column, s.kind)).collect();
        assert_eq!(
            named,
            vec![
                ("state", 0, 4, SYMBOL_VARIABLE),
                ("load", 1, 3, SYMBOL_FUNCTION),
                ("title", 4, 4, SYMBOL_VARIABLE),
            ],
            "let, fn and :=, each at its own 0-based line and character"
        );
    }

    #[test]
    fn a_for_binder_is_not_a_declaration() {
        // It is scoped to its own loop; offering to jump to it from outside
        // would be wrong.
        let source = "for item in items { Label{text: item} }\nlet items = []\n";
        let found = declarations(source);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].name, "items");
    }

    #[test]
    fn a_keyword_is_a_whole_word() {
        let source = "let letters = 1\nlet fnord = 2\n";
        let found = declarations(source);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].name, "letters");
        assert_eq!(found[1].name, "fnord");
    }

    #[test]
    fn a_definition_request_returns_the_declaration() {
        let mut server = server();
        let uri = "file:///C:/app/bundle/main.splash";
        open(
            &mut server,
            uri,
            "let state = {items: []}\n\nfn load(){ }\n\nView{\n    width: Fill\n    on_render: fn(){ load() }\n    body := Label{text: \"x\"}\n}\n",
        );
        // The cursor on `load`, inside the handler on line 6 (0-based).
        let replies = server.handle(&json!({
            "jsonrpc":"2.0","id":11,"method":"textDocument/definition",
            "params":{"textDocument":{"uri":uri},"range":{"start":{"line":6,"character":22}}}
        }));
        let locations = replies[0]["result"].as_array().expect("a location array");
        assert_eq!(locations.len(), 1, "{locations:?}");
        assert_eq!(locations[0]["uri"], uri);
        assert_eq!(locations[0]["range"]["start"]["line"], 2, "the fn's own line");
        assert_eq!(locations[0]["range"]["start"]["character"], 3);
    }

    #[test]
    fn a_definition_for_a_name_nothing_declares_is_null() {
        let mut server = server();
        let uri = "file:///C:/app/bundle/main.splash";
        open(&mut server, uri, "View{ Label{text: \"x\"} }\n");
        let replies = server.handle(&json!({
            "jsonrpc":"2.0","id":12,"method":"textDocument/definition",
            "params":{"textDocument":{"uri":uri},"range":{"start":{"line":0,"character":7}}}
        }));
        assert!(replies[0]["result"].is_null(), "{}", replies[0]);
    }

    #[test]
    fn the_outline_lists_the_declarations() {
        let mut server = server();
        let uri = "file:///C:/app/bundle/main.splash";
        open(&mut server, uri, "let a = 1\nfn b(){ }\nView{ c := Label{text: \"x\"} }\n");
        let replies = server.handle(&json!({
            "jsonrpc":"2.0","id":13,"method":"textDocument/documentSymbol",
            "params":{"textDocument":{"uri":uri}}
        }));
        let symbols = replies[0]["result"].as_array().expect("symbols");
        let names: Vec<&str> = symbols.iter().map(|s| s["name"].as_str().expect("name")).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
        assert_eq!(symbols[0]["kind"], SYMBOL_VARIABLE);
        assert_eq!(symbols[1]["kind"], SYMBOL_FUNCTION);
    }

    #[test]
    fn a_file_uri_is_a_path() {
        assert_eq!(uri_to_path("file:///C:/app/bundle/main.splash"), Some(PathBuf::from("C:/app/bundle/main.splash")));
        assert_eq!(uri_to_path("file:///home/me/app.splash"), Some(PathBuf::from("home/me/app.splash")));
        // A space in a directory name arrives percent-encoded.
        assert_eq!(uri_to_path("file:///C:/my%20apps/main.splash"), Some(PathBuf::from("C:/my apps/main.splash")));
    }

    #[test]
    fn an_unknown_method_is_answered_only_when_it_is_a_request() {
        let mut server = server();
        let replies = server.handle(&json!({"jsonrpc":"2.0","id":9,"method":"textDocument/hover","params":{}}));
        assert_eq!(replies[0]["error"]["code"], -32601);
        let replies = server.handle(&json!({"jsonrpc":"2.0","method":"$/setTrace","params":{}}));
        assert!(replies.is_empty());
    }
}
