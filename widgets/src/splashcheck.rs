//! `splashcheck`: static semantic analysis of a user-written Splash app.
//!
//! An app's `main.splash` is a program, so almost every fault it can have is
//! decidable without running it: [`validate_splash_body_records`] evaluates the
//! body in a throwaway isolate — the same VM, the same widget registry, the
//! same host bridge a real host installs — and returns every error with its
//! line, column and a “Did you mean” list. What it cannot see from inside the
//! VM, this module adds from the bundle: which host services the app asks for
//! against what its manifest declares.
//!
//! Three properties, each learned from a tool that got it wrong:
//!
//! * **It is advisory.** It never decides admission; the store's gate does
//!   that. `ok`/`warn`/`ERR` lines, exit 0/1/2, and a baseline so a legacy app
//!   can adopt the tool without a wall of red.
//! * **It reports the app's own positions.** The VM reports locations in the
//!   evaluated source, which is the body inside a harness prefix;
//!   [`body_location`] maps them back.
//! * **It says what it could not check.** A skipped check is a note, never a
//!   silent pass.
//!
//! The boundaries, stated rather than discovered:
//!
//! * Only top-level code is evaluated. A `fn` nothing calls at the top level
//!   is compiled but never run, so a fault inside it is not reported — which
//!   is also why the capability check below reads the source text rather than
//!   the VM's request queue: a call inside a button's `fn` is never reached,
//!   and a checker that missed it would miss the capability the app needs.
//! * `host.request` calls queue and are never answered, so a callback's body
//!   does not run and a fault inside one is not reported.
//! * The widget set is whatever this crate was built with. A host that links
//!   `makepad-widgets` with `features = ["maps"]` has `MapView`; a checker
//!   built without it reports `MapView not found` for a card that is fine on
//!   that host. Build the checker with the features of the host it checks for.
//! * Validation always runs with the network module available. Whether the app
//!   may *use* it is a declaration question, and it is answered by the
//!   capability check below; answering it twice — once as a VM error and once
//!   as a finding — would only double-report.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::splash::{body_location, validate_splash_body_records};

/// The tool's name, in output and errors.
pub const TOOL_NAME: &str = "splashcheck";
/// The baseline file's name, looked for in an app's bundle root.
pub const BASELINE_FILE: &str = "splashcheck-baseline.json";
/// The manifest file that marks a directory as an app bundle.
const MANIFEST_FILE: &str = "manifest.json";

/// What to check. Everything, always: a checker with tiers is a checker whose
/// default nobody trusts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tier {
    /// The validator's errors, mapped back to the app's own lines.
    Eval,
    /// The host services the app asks for against what it declares.
    Capability,
    /// Both.
    All,
}

impl Tier {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "eval" => Some(Self::Eval),
            "capability" => Some(Self::Capability),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Eval => "eval",
            Self::Capability => "capability",
            Self::All => "all",
        }
    }

    fn runs_eval(self) -> bool {
        matches!(self, Self::Eval | Self::All)
    }

    fn runs_capability(self) -> bool {
        matches!(self, Self::Capability | Self::All)
    }
}

/// A finding's severity. A clean file has no findings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Severity {
    Warn,
    Error,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// One finding, positioned in the app's own file.
#[derive(Clone, Debug)]
pub struct Finding {
    pub severity: Severity,
    /// A stable code, so a reader can match findings without parsing prose:
    /// `splash.eval`, `splash.capability`, `splash.known`, `splash.io`.
    pub code: &'static str,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub message: String,
}

/// What an app's manifest declares that this module reads.
#[derive(Clone, Debug, Default)]
pub struct Declarations {
    /// The manifest's `capabilities`, as written.
    pub capabilities: Vec<String>,
    /// Whether the manifest declares the network at all: a `net` capability or
    /// any `network.hosts` entry.
    pub declares_net: bool,
}

/// One app file's report.
#[derive(Clone, Debug)]
pub struct FileReport {
    pub path: PathBuf,
    pub bytes: usize,
    pub declarations: Declarations,
    /// Whether a manifest was found above the file at all.
    pub manifest_found: bool,
    pub findings: Vec<Finding>,
    pub notes: Vec<String>,
    /// A problem with the inputs rather than the app: a manifest that will not
    /// parse. Exits 2 (a usage problem), not 1.
    pub fatal: Option<String>,
}

impl FileReport {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            bytes: 0,
            declarations: Declarations::default(),
            manifest_found: false,
            findings: Vec::new(),
            notes: Vec::new(),
            fatal: None,
        }
    }

    fn push(&mut self, severity: Severity, code: &'static str, line: Option<usize>, column: Option<usize>, message: impl Into<String>) {
        self.findings.push(Finding { severity, code, line, column, message: message.into() });
    }

    fn error(&mut self, code: &'static str, line: Option<usize>, column: Option<usize>, message: impl Into<String>) {
        self.push(Severity::Error, code, line, column, message);
    }

    /// The file's status: the worst severity it carries.
    pub fn severity(&self) -> Option<Severity> {
        if self.findings.iter().any(|f| f.severity == Severity::Error) {
            Some(Severity::Error)
        } else if !self.findings.is_empty() {
            Some(Severity::Warn)
        } else {
            None
        }
    }
}

/// A parsed command line.
#[derive(Clone, Debug)]
pub struct Invocation {
    pub paths: Vec<PathBuf>,
    pub tier: Tier,
    pub baseline: Option<PathBuf>,
    pub write_baseline: bool,
    pub json: bool,
}

/// What [`parse`] produced.
#[derive(Debug)]
pub enum Parsed {
    Run(Invocation),
    Help,
    Version,
}

/// The usage text. Doubles as `--help`.
pub fn usage() -> String {
    format!(
        "\
{TOOL_NAME} — static semantic analysis of a user-written Splash app

usage: {TOOL_NAME} [options] <path...>

  A path is a .splash file or a directory to scan (recursively, skipping .git
  and target). A file is checked against the bundle it belongs to: the nearest
  manifest.json above it supplies the capabilities the app declares.

options:
  --tier eval|capability|all   what to check (default: all)
                                  eval        the VM's errors, at the app's
                                               own lines
                                  capability  the host services the app asks
                                               for against what it declares
  --baseline FILE              findings already known and accepted, as JSON.
                                Default: {BASELINE_FILE} in the app's bundle
                                root, when it exists. A known finding is a
                                warning; a new one is an error.
  --write-baseline             write the current findings as the baseline, so
                                a legacy app can adopt the tool without a wall
                                of red. Needs one bundle, or --baseline.
  --json                       one JSON report on stdout; human output
                                otherwise
  --version, --help

exit status:
  0  nothing new that stops the app
  1  at least one finding that is not in the baseline
  2  a usage or I/O problem: unknown flag, missing path, unreadable file, a
     manifest that will not parse

{TOOL_NAME} is advisory. It never decides admission; the store's gate does that.
It evaluates top-level code only: a fn nothing calls at the top level, and a
host.request callback, are compiled but never run.
"
    )
}

/// Parses a command line. `Err` carries the message to print before the usage.
pub fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut paths = Vec::new();
    let mut tier = Tier::All;
    let mut baseline = None;
    let mut write_baseline = false;
    let mut json = false;
    let mut flags_done = false;

    let take_value = |args: &[String], at: &mut usize, flag: &str| -> Result<String, String> {
        *at += 1;
        match args.get(*at) {
            Some(value) => Ok(value.clone()),
            None => Err(format!("{flag} needs a value")),
        }
    };

    let mut at = 0;
    while at < args.len() {
        let arg = &args[at];
        if flags_done || !arg.starts_with("--") || arg == "--" {
            if arg == "--" {
                flags_done = true;
            } else {
                paths.push(PathBuf::from(arg));
            }
            at += 1;
            continue;
        }
        match arg.as_str() {
            "--help" | "-h" => return Ok(Parsed::Help),
            "--version" | "-V" => return Ok(Parsed::Version),
            "--write-baseline" => write_baseline = true,
            "--json" => json = true,
            "--tier" => {
                let value = take_value(args, &mut at, "--tier")?;
                tier = Tier::parse(&value).ok_or_else(|| format!("unknown tier {value:?}: eval, capability or all"))?;
            }
            "--baseline" => baseline = Some(PathBuf::from(take_value(args, &mut at, "--baseline")?)),
            other => return Err(format!("unknown flag {other}")),
        }
        at += 1;
    }

    if paths.is_empty() {
        return Err("no app, bundle or directory given".to_owned());
    }
    Ok(Parsed::Run(Invocation { paths, tier, baseline, write_baseline, json }))
}

/// Runs the check and returns the exit status. `cx` is the host context the
/// validator evaluates in — built once by the caller and reused, because the
/// widget registry it carries is the expensive part.
pub fn run(inv: &Invocation, cx: &mut crate::Cx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let files = match discover(&inv.paths) {
        Ok(files) => files,
        Err(message) => {
            let _ = writeln!(err, "{TOOL_NAME}: {message}");
            return 2;
        }
    };

    let mut reports: Vec<FileReport> = files
        .iter()
        .map(|path| check_file(path, inv, cx))
        .collect();

    // The baseline, per bundle: a legacy app adopts the tool without a wall of
    // red, and a finding that is already known is a warning rather than a
    // failure. Position-independent on purpose — a line that moves is the same
    // finding.
    let baseline_paths = resolve_baselines(&reports, inv);
    for (report, baseline_path) in reports.iter_mut().zip(baseline_paths.iter()) {
        let Some(baseline_path) = baseline_path else { continue };
        match Baseline::load(baseline_path) {
            Ok(baseline) => {
                let name = file_key(&report.path);
                for finding in &mut report.findings {
                    if baseline.knows(&name, &finding.message) {
                        finding.severity = Severity::Warn;
                        finding.code = "splash.known";
                    }
                }
            }
            Err(message) => {
                report.fatal = Some(format!("{}: {message}", baseline_path.display()));
            }
        }
    }

    if inv.write_baseline {
        if let Err(message) = write_baselines(&reports, &baseline_paths, inv, out) {
            let _ = writeln!(err, "{TOOL_NAME}: {message}");
            return 2;
        }
    }

    if inv.json {
        print_json(&reports, inv, out);
    } else {
        print_human(&reports, out);
    }

    if let Some(fatal) = reports.iter().find_map(|report| report.fatal.clone()) {
        let _ = writeln!(err, "{TOOL_NAME}: {fatal}");
        return 2;
    }
    if reports
        .iter()
        .flat_map(|report| &report.findings)
        .any(|finding| finding.severity == Severity::Error)
    {
        1
    } else {
        0
    }
}

/// Collects the files to check: explicit files as given, directories scanned.
fn discover(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut found = Vec::new();
    for path in paths {
        let metadata = fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if metadata.is_file() {
            found.push(path.clone());
        } else if metadata.is_dir() {
            scan_dir(path, &mut found);
        } else {
            return Err(format!("{}: not a file or directory", path.display()));
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

fn scan_dir(dir: &Path, found: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if matches!(name.as_str(), ".git" | "target") {
                continue;
            }
            scan_dir(&path, found);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("splash") {
            found.push(path);
        }
    }
}

/// Checks one app file. [`run`] is the command-line entry; this is the one-file
/// form a host embedding the checker would call — which is what the language
/// server does, once per document.
pub fn check_file(path: &Path, inv: &Invocation, cx: &mut crate::Cx) -> FileReport {
    let mut report = FileReport::new(path);
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(e) => {
            report.error("splash.io", None, None, format!("cannot read: {e}"));
            return report;
        }
    };
    check_source(path, &source, inv, cx)
}

/// Check a source that is already in hand — an editor's buffer, or a caller
/// that read the file itself. `path` is only used to find the app's bundle, so
/// an unsaved document in a bundle on disk is still checked against that
/// bundle's declarations, which is the whole point for an editor.
pub fn check_source(path: &Path, source: &str, inv: &Invocation, cx: &mut crate::Cx) -> FileReport {
    let mut report = FileReport::new(path);
    report.bytes = source.len();

    // The bundle this file belongs to. Without one there is nothing to check
    // the app's declarations against, which is said rather than skipped
    // silently.
    let bundle_root = find_bundle_root(path);
    match &bundle_root {
        Some(root) => match read_declarations(root) {
            Ok(declarations) => {
                report.declarations = declarations;
                report.manifest_found = true;
            }
            Err(message) => {
                report.fatal = Some(message);
                return report;
            }
        },
        None => report
            .notes
            .push("no manifest.json above this file: capability checks skipped".to_owned()),
    }

    if inv.tier.runs_eval() {
        // Always with the network module available: whether the app may use it
        // is a declaration question, answered below. See the module docs.
        let records = validate_splash_body_records(cx, &source, true);
        for record in &records {
            let (line, mut column) = match &record.loc {
                Some(loc) => body_location(loc.line, loc.col, true),
                None => (1, 1),
            };
            // The VM's column is the token the interpreter was at, which is
            // often the property's key or the enclosing statement rather than
            // the name the message names. When the message names something
            // that is on the line, its own column is the better one — and it
            // is the one an editor should put the squiggle on.
            if let Some(name) = named_in_message(&record.message) {
                if let Some(found) = find_word(&line_text(&source, line), &name) {
                    column = found + 1;
                }
            }
            report.error("splash.eval", Some(line), Some(column), record.message.clone());
        }
    }

    if inv.tier.runs_capability() && report.manifest_found {
        for finding in capability_findings(&source, &report.declarations) {
            report.findings.push(finding);
        }
    }

    report
}

/// Every host service the source asks for, against what the manifest declares.
///
/// A textual scan, not the VM's request queue: the validator evaluates
/// top-level code only, so a `host.request` inside a `fn` a button invokes is
/// never reached. The queue would report a subset and call it the truth.
fn capability_findings(source: &str, declarations: &Declarations) -> Vec<Finding> {
    let mut findings = Vec::new();
    let declared: BTreeSet<&str> = declarations.capabilities.iter().map(String::as_str).collect();
    let calls = requested_services(source);
    let computed = calls.iter().filter(|call| !call.literal).count();
    for call in calls.iter().filter(|call| call.literal) {
        let family = call.service.split('.').next().unwrap_or(&call.service);
        if !declared.contains(family) {
            findings.push(Finding {
                severity: Severity::Error,
                code: "splash.capability",
                line: Some(call.line),
                column: Some(call.column),
                message: format!(
                    "host.request({:?}) needs the {:?} capability, which the manifest does not declare (it declares {})",
                    call.service,
                    family,
                    if declarations.capabilities.is_empty() {
                        "nothing".to_owned()
                    } else {
                        declarations.capabilities.join(", ")
                    }
                ),
            });
        }
    }
    if computed > 0 {
        findings.push(Finding {
            severity: Severity::Warn,
            code: "splash.capability",
            line: None,
            column: None,
            message: format!(
                "{computed} host.request call(s) pass a computed service name, which cannot be checked statically"
            ),
        });
    }
    if uses_net(source) && !declarations.declares_net {
        findings.push(Finding {
            severity: Severity::Error,
            code: "splash.capability",
            line: None,
            column: None,
            message: "the app uses the network module but the manifest declares neither a `net` capability nor any `network.hosts`"
                .to_owned(),
        });
    }
    findings
}

/// The 1-based line of a 1-based line number, or empty.
fn line_text(source: &str, line: usize) -> String {
    source.lines().nth(line.saturating_sub(1)).unwrap_or("").to_owned()
}

/// The name a validator message is about, when it names one.
///
/// The VM's messages are prose, and the one thing they all carry is the name:
/// `property X not defined on type`, `variable X not found in scope`, `field X
/// not found in type-check`, `no getter for field X on type …`. Taking it is
/// what lets the column point at the name rather than at whatever token the
/// interpreter happened to be at.
pub fn named_in_message(message: &str) -> Option<String> {
    for keyword in ["property ", "variable ", "field ", "name "] {
        if let Some(at) = message.find(keyword) {
            let rest = &message[at + keyword.len()..];
            let end = rest.find(|c: char| c.is_whitespace() || c == ':' || c == '.' || c == ',')?;
            let name = &rest[..end];
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                return Some(name.to_owned());
            }
        }
    }
    None
}

/// The names a validator message offers instead of the one it refused.
///
/// The VM appends `Did you mean: A(obj123) or B(fn45), C(nil) (+12 more)` to
/// most of its errors. Parsing it is what turns a finding into a fix: an
/// editor can offer to replace the name, which is the difference between a red
/// squiggle and a resolved one.
pub fn suggestions(message: &str) -> Vec<String> {
    let Some(at) = message.find("Did you mean: ") else {
        return Vec::new();
    };
    let tail = &message[at + "Did you mean: ".len()..];
    // The VM truncates its list and says so: "(+12 more)".
    let tail = match tail.find(" (+") {
        Some(end) => &tail[..end],
        None => tail,
    };
    let mut out = Vec::new();
    for token in tail.split(" or ").flat_map(|part| part.split(", ")) {
        // Each suggestion is a name with its kind in parentheses:
        // `width(obj653)`, `pow(fn280)`, `abs_pos(nil)`.
        let name = token.split('(').next().unwrap_or("").trim();
        if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            out.push(name.to_owned());
        } else {
            // A shape we do not recognise: stop rather than offer nonsense.
            return Vec::new();
        }
    }
    out.dedup();
    out
}

/// The 0-based **character** column of a whole-word occurrence of `name` in
/// `line`.
pub(crate) fn find_word(line: &str, name: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut from = 0usize;
    while let Some(at) = line[from..].find(name) {
        let at = from + at;
        let before_ok = at == 0 || !is_ident_byte(bytes[at - 1]);
        let after = at + name.len();
        let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
        if before_ok && after_ok {
            return Some(line[..at].chars().count());
        }
        from = at + 1;
    }
    None
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// One `host.request("service", …)` in the source.
#[derive(Clone, Debug)]
pub struct ServiceCall {
    pub line: usize,
    pub column: usize,
    pub service: String,
    /// Whether the service was a literal. A computed service name cannot be
    /// checked statically and is skipped rather than guessed at.
    pub literal: bool,
}

/// Every `host.request(...)` in the source, with the position of the service
/// name. The first argument is read across whitespace and newlines, because a
/// call written one argument per line is ordinary.
pub fn requested_services(source: &str) -> Vec<ServiceCall> {
    let mut out = Vec::new();
    let mut search = 0usize;
    while let Some(at) = source[search..].find("host.request") {
        let start = search + at;
        let mut index = start + "host.request".len();
        // Skip whitespace between the name and the argument list.
        while let Some(c) = source[index..].chars().next() {
            if c.is_whitespace() {
                index += c.len_utf8();
            } else {
                break;
            }
        }
        if source[index..].starts_with('(') {
            index += 1;
            while let Some(c) = source[index..].chars().next() {
                if c.is_whitespace() {
                    index += c.len_utf8();
                } else {
                    break;
                }
            }
            let (line, column) = line_column(source, index);
            if source[index..].starts_with('"') {
                let name_start = index + 1;
                if let Some(close) = source[name_start..].find('"') {
                    let after = &source[name_start + close + 1..];
                    if after.trim_start().starts_with('+') {
                        // A concatenation: the literal is only part of the
                        // name, so there is no name to check.
                        out.push(ServiceCall { line, column, service: String::new(), literal: false });
                    } else {
                        let service = source[name_start..name_start + close].to_owned();
                        out.push(ServiceCall { line, column, service, literal: true });
                    }
                }
            } else {
                // A computed service name: recorded so a caller can say it was
                // skipped, never guessed at.
                out.push(ServiceCall { line, column, service: String::new(), literal: false });
            }
        }
        search = start + "host.request".len();
    }
    out
}

/// Whether the source reaches for the network module at all.
fn uses_net(source: &str) -> bool {
    source.contains("net.http_request")
        || source.contains("net.socket_stream")
        || source.contains("net.http_server")
        || source.contains("net.open_url")
}

/// The 1-based line of a byte offset, and the 1-based **character** column.
///
/// Character, not byte: a column an editor or a person reads must not jump by
/// three when the line before the fault holds one Chinese character.
fn line_column(source: &str, at: usize) -> (usize, usize) {
    let at = at.min(source.len());
    let before = &source[..at];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map(|n| n + 1).unwrap_or(0);
    (line, before[line_start..].chars().count() + 1)
}

/// The nearest `manifest.json` at or above a file.
fn find_bundle_root(file: &Path) -> Option<PathBuf> {
    let start = file.parent()?;
    for ancestor in start.ancestors() {
        if ancestor.join(MANIFEST_FILE).is_file() {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

fn read_declarations(root: &Path) -> Result<Declarations, String> {
    let path = root.join(MANIFEST_FILE);
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let capabilities: Vec<String> = value
        .get("capabilities")
        .and_then(|c| c.as_array())
        .map(|list| list.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    let hosts = value
        .get("network")
        .and_then(|n| n.get("hosts"))
        .and_then(|h| h.as_array())
        .map(|list| list.len())
        .unwrap_or(0);
    Ok(Declarations {
        declares_net: capabilities.iter().any(|c| c == "net") || hosts > 0,
        capabilities,
    })
}

/// Findings a reader has already accepted. Keyed by file name and message, not
/// by position: a finding that moved is the same finding.
#[derive(Clone, Debug, Default)]
pub struct Baseline {
    entries: BTreeSet<(String, String)>,
}

impl Baseline {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let mut entries = BTreeSet::new();
        if let Some(list) = value.get("known").and_then(|k| k.as_array()) {
            for entry in list {
                if let (Some(file), Some(message)) = (entry.get("file").and_then(|f| f.as_str()), entry.get("message").and_then(|m| m.as_str())) {
                    entries.insert((file.to_owned(), message.to_owned()));
                }
            }
        }
        Ok(Self { entries })
    }

    /// Write the reports' findings as a baseline.
    pub fn write(path: &Path, reports: &[FileReport]) -> Result<(), String> {
        let mut known = Vec::new();
        for report in reports {
            let name = file_key(&report.path);
            for finding in &report.findings {
                known.push(serde_json::json!({"file": name, "message": finding.message}));
            }
        }
        known.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
        known.dedup();
        let document = serde_json::json!({"tool": TOOL_NAME, "known": known});
        fs::write(path, format!("{}\n", serde_json::to_string_pretty(&document).map_err(|e| e.to_string())?))
            .map_err(|e| e.to_string())
    }

    pub fn knows(&self, file: &str, message: &str) -> bool {
        self.entries.contains(&(file.to_owned(), message.to_owned()))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A file's key in a baseline: its name, not its whole path, so a baseline
/// survives a checkout moving.
fn file_key(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

/// One baseline path per report, or `None` where there is nothing to load:
/// the flag when given, else the bundle root's own file when it exists.
fn resolve_baselines(reports: &[FileReport], inv: &Invocation) -> Vec<Option<PathBuf>> {
    if let Some(path) = &inv.baseline {
        return reports.iter().map(|_| Some(path.clone())).collect();
    }
    reports
        .iter()
        .map(|report| {
            let root = find_bundle_root(&report.path)?;
            let candidate = root.join(BASELINE_FILE);
            candidate.is_file().then_some(candidate)
        })
        .collect()
}

fn write_baselines(
    reports: &[FileReport],
    baseline_paths: &[Option<PathBuf>],
    inv: &Invocation,
    out: &mut dyn Write,
) -> Result<(), String> {
    let target = if let Some(path) = &inv.baseline {
        path.clone()
    } else {
        // One bundle, or the flag: with several bundles there is no single
        // honest place to put one file.
        let roots: BTreeSet<PathBuf> = reports.iter().filter_map(|r| find_bundle_root(&r.path)).collect();
        if roots.len() != 1 {
            return Err("--write-baseline with several bundles needs --baseline FILE".to_owned());
        }
        let root = roots.into_iter().next().expect("one root");
        baseline_paths
            .iter()
            .flatten()
            .find(|p| p.parent() == Some(root.as_path()))
            .cloned()
            .unwrap_or_else(|| root.join(BASELINE_FILE))
    };
    Baseline::write(&target, reports)?;
    let _ = writeln!(out, "{TOOL_NAME}: wrote {} known finding(s) to {}", Baseline::default().len().max(count_findings(reports)), target.display());
    Ok(())
}

fn count_findings(reports: &[FileReport]) -> usize {
    reports.iter().map(|r| r.findings.len()).sum()
}

fn print_human(reports: &[FileReport], out: &mut dyn Write) {
    let mut ok = 0usize;
    let mut warn = 0usize;
    let mut error = 0usize;
    for report in reports {
        let status = match report.severity() {
            Some(Severity::Error) => {
                error += 1;
                "ERR "
            }
            Some(Severity::Warn) => {
                warn += 1;
                "warn"
            }
            None if report.fatal.is_some() => {
                error += 1;
                "ERR "
            }
            None => {
                ok += 1;
                "ok  "
            }
        };
        let name = report.path.display();
        if let Some(fatal) = &report.fatal {
            let _ = writeln!(out, "  {status} {name} — {fatal}");
            continue;
        }
        let net = if report.declarations.declares_net { ", net declared" } else { "" };
        let _ = writeln!(out, "  {status} {name} — {} bytes{net}", report.bytes);
        for finding in &report.findings {
            let at = match (finding.line, finding.column) {
                (Some(line), Some(column)) => format!("{line}:{column} "),
                (Some(line), None) => format!("{line} "),
                _ => String::new(),
            };
            let _ = writeln!(out, "        {}{}", at, finding.message);
        }
        for note in &report.notes {
            let _ = writeln!(out, "        note: {note}");
        }
    }
    let _ = writeln!(out, "{TOOL_NAME}: {ok} ok, {warn} warn, {error} error");
}

fn print_json(reports: &[FileReport], inv: &Invocation, out: &mut dyn Write) {
    let files: Vec<serde_json::Value> = reports
        .iter()
        .map(|report| {
            let findings: Vec<serde_json::Value> = report
                .findings
                .iter()
                .map(|finding| {
                    serde_json::json!({
                        "severity": finding.severity.as_str(),
                        "code": finding.code,
                        "line": finding.line,
                        "column": finding.column,
                        "message": finding.message,
                    })
                })
                .collect();
            serde_json::json!({
                "path": report.path.display().to_string(),
                "bytes": report.bytes,
                "manifest_found": report.manifest_found,
                "declares_net": report.declarations.declares_net,
                "capabilities": report.declarations.capabilities,
                "findings": findings,
                "notes": report.notes,
                "fatal": report.fatal,
            })
        })
        .collect();
    let (ok, warn, error) = tally(reports);
    let report = serde_json::json!({
        "tool": TOOL_NAME,
        "version": env!("CARGO_PKG_VERSION"),
        "tier": inv.tier.as_str(),
        "files": files,
        "summary": {"ok": ok, "warn": warn, "error": error},
    });
    let _ = writeln!(out, "{}", serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".to_owned()));
}

fn tally(reports: &[FileReport]) -> (usize, usize, usize) {
    let mut ok = 0usize;
    let mut warn = 0usize;
    let mut error = 0usize;
    for report in reports {
        match report.severity() {
            Some(Severity::Error) => error += 1,
            Some(Severity::Warn) => warn += 1,
            None if report.fatal.is_some() => error += 1,
            None => ok += 1,
        }
    }
    (ok, warn, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clean app: one state, one view, no host calls.
    const CLEAN: &str = "\
let state = {items: []}

fn add(text) {
    state.items.push(text)
}

View{
    width: Fill height: Fit
    flow: Down
    Label{text: \"Items\"}
    for item in state.items {
        Label{text: item}
    }
}
";

    /// The same app, reading a name nothing declares.
    const UNDECLARED: &str = "\
View{
    width: Fill height: Fit
    Label{text: nope}
}
";

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, text).expect("write fixture");
        path
    }

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("splashcheck-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn invocation(paths: Vec<PathBuf>) -> Invocation {
        Invocation { paths, tier: Tier::All, baseline: None, write_baseline: false, json: false }
    }

    /// A host context the validator can evaluate in: no window, no draw. The
    /// widget registry is the expensive part, so a caller builds one and reuses
    /// it; a test pays it once per test.
    fn context() -> crate::Cx {
        let mut cx = crate::Cx::new(Box::new(|_, _| {}));
        cx.with_vm(crate::script_mod);
        cx
    }

    #[test]
    fn a_clean_app_has_no_findings() {
        let dir = tempdir("clean");
        let app = write(&dir, "main.splash", CLEAN);
        let mut cx = context();
        let report = check_file(&app, &invocation(vec![app.clone()]), &mut cx);
        assert!(report.findings.is_empty(), "{report:?}");
        assert!(report.fatal.is_none(), "{report:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_undeclared_name_is_reported_at_the_apps_own_line() {
        let dir = tempdir("undeclared");
        // `nope` is on the third line of the body, at column 17.
        let app = write(&dir, "main.splash", UNDECLARED);
        let mut cx = context();
        let report = check_file(&app, &invocation(vec![app.clone()]), &mut cx);
        let finding = report
            .findings
            .iter()
            .find(|f| f.code == "splash.eval")
            .expect("an eval finding");
        // The VM reports 0-based lines in the prefixed source; the mapping
        // brings it back to the app's own line, and the message's name refines
        // the column to the name itself rather than the interpreter's token.
        assert_eq!(finding.line, Some(3), "{finding:?}");
        assert_eq!(finding.column, Some(17), "{finding:?}");
        assert!(finding.message.contains("nope"), "{}", finding.message);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_location_in_the_harness_prefix_is_clamped() {
        // ScriptLoc is 0-based, and the no-net prefix is three lines: a raw
        // line below three is the harness's own.
        assert_eq!(crate::splash::body_location(0, 0, false), (1, 1));
        assert_eq!(crate::splash::body_location(2, 9, false), (1, 10));
        // The body's first line shares the prefix's last line, so its column
        // loses the prefix tail's width ("View{height:Fit, " is 17 wide).
        assert_eq!(crate::splash::body_location(3, 5, false), (1, 1), "inside the prefix tail");
        assert_eq!(crate::splash::body_location(3, 20, false), (1, 4));
        // Past the shared line, only the line moves.
        assert_eq!(crate::splash::body_location(4, 0, false), (2, 1));
        assert_eq!(crate::splash::body_location(4, 7, false), (2, 8));
        // The net prefix is four lines, so the same raw line is one line later.
        assert_eq!(crate::splash::body_location(4, 0, true), (1, 1));
        assert_eq!(crate::splash::body_location(7, 0, true), (4, 1));
    }

    #[test]
    fn a_column_is_refined_to_the_name_the_message_names() {
        assert_eq!(named_in_message("property nosuchprop not defined on type. Did you mean: width"), Some("nosuchprop".to_owned()));
        assert_eq!(named_in_message("variable undefined_name not found in scope"), Some("undefined_name".to_owned()));
        assert_eq!(named_in_message("no getter for field Deep on type NotFound"), Some("Deep".to_owned()));
        assert_eq!(named_in_message("script evaluated to an error"), None);
        // The VM's "Did you mean" tail, parsed into the names it offers.
        let message = "property nosuchprop not defined on type. Did you mean: distribute(nil) or abs_pos(nil), width(obj653), height(obj656), aspect(nil) (+55 more)";
        assert_eq!(suggestions(message), vec!["distribute", "abs_pos", "width", "height", "aspect"]);
        let message = "variable MapView not found in scope. Did you mean: MathView(obj5322) or View(obj1436)";
        assert_eq!(suggestions(message), vec!["MathView", "View"]);
        // A tail we do not recognise yields nothing rather than nonsense.
        assert!(suggestions("property x not defined on type").is_empty());
        assert!(suggestions("Did you mean: \"quoted\"").is_empty());
        // Whole words only: `text` must not match inside `text_style`.
        let line = "    Label{text: \"ok\" text_style: 1}";
        assert_eq!(find_word(line, "text"), Some(10));
        assert_eq!(find_word(line, "style"), None);
        // Characters, not bytes: the column must not jump past a CJK line.
        // Two CJK characters are two columns and six bytes.
        let cjk = "    Label{text: \"你好\" nope}";
        assert_eq!(cjk.find("nope"), Some(25), "the byte offset");
        assert_eq!(find_word(cjk, "nope"), Some(21), "the character column");
    }

    #[test]
    fn a_host_service_the_manifest_does_not_declare_is_a_finding() {
        let dir = tempdir("capability");
        fs::create_dir_all(&dir).expect("mkdir");
        write(&dir, "manifest.json", r#"{"schema":1,"id":"dev.example.app","capabilities":["storage"]}"#);
        let app = write(
            &dir,
            "main.splash",
            "View{ width: Fill height: Fit Label{text: \"x\"} }\nfn load(){ host.request(\"news.list\", {}, fn(r){}) }\n",
        );
        let mut cx = context();
        let report = check_file(&app, &invocation(vec![app.clone()]), &mut cx);
        let finding = report
            .findings
            .iter()
            .find(|f| f.code == "splash.capability")
            .expect("a capability finding");
        assert_eq!(finding.line, Some(2), "the call's own line: {finding:?}");
        assert!(finding.message.contains("news"), "{}", finding.message);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_declared_host_service_is_not_a_finding() {
        let dir = tempdir("declared");
        fs::create_dir_all(&dir).expect("mkdir");
        write(&dir, "manifest.json", r#"{"schema":1,"id":"dev.example.app","capabilities":["storage","news"]}"#);
        let app = write(
            &dir,
            "main.splash",
            "View{ width: Fill height: Fit Label{text: \"x\"} }\nfn load(){ host.request(\"news.list\", {}, fn(r){}) }\n",
        );
        let mut cx = context();
        let report = check_file(&app, &invocation(vec![app.clone()]), &mut cx);
        assert!(report.findings.is_empty(), "{report:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_capability_scan_reads_the_call_however_it_is_written() {
        let source = "host.request(\"a.b\", {})\nhost.request( \"c.d\" , {})\nhost.request(\n  \"e.f\",\n  {},\n)\nhost.request(service, {})\n";
        let calls = requested_services(source);
        let services: Vec<&str> = calls.iter().filter(|c| c.literal).map(|c| c.service.as_str()).collect();
        assert_eq!(services, vec!["a.b", "c.d", "e.f"]);
        assert_eq!(calls.iter().filter(|c| !c.literal).count(), 1, "a computed service is skipped, not guessed");
        assert_eq!(calls[2].line, 4, "a call written one argument per line keeps its position");
        // A computed service is recorded so a checker can say it was skipped,
        // and is never checked against the manifest: there is no name to check.
        assert!(!calls.iter().any(|c| c.literal && c.service.is_empty()));
    }

    #[test]
    fn a_network_use_the_manifest_does_not_declare_is_a_finding() {
        let dir = tempdir("net");
        fs::create_dir_all(&dir).expect("mkdir");
        write(&dir, "manifest.json", r#"{"schema":1,"id":"dev.example.app","capabilities":["storage"]}"#);
        let app = write(
            &dir,
            "main.splash",
            "fn load(){ net.http_request({url: \"https://example.invalid/x\"}, fn(r){}) }\nView{ width: Fill height: Fit Label{text: \"x\"} }\n",
        );
        let mut cx = context();
        let report = check_file(&app, &invocation(vec![app.clone()]), &mut cx);
        assert!(
            report.findings.iter().any(|f| f.message.contains("network module")),
            "{report:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_without_a_manifest_says_so() {
        let dir = tempdir("no-manifest");
        let app = write(&dir, "main.splash", CLEAN);
        let mut cx = context();
        let report = check_file(&app, &invocation(vec![app.clone()]), &mut cx);
        assert!(report.findings.is_empty(), "{report:?}");
        assert_eq!(report.notes, vec!["no manifest.json above this file: capability checks skipped".to_owned()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_manifest_that_will_not_parse_is_a_usage_problem() {
        let dir = tempdir("bad-manifest");
        fs::create_dir_all(&dir).expect("mkdir");
        write(&dir, "manifest.json", "{not json");
        let app = write(&dir, "main.splash", CLEAN);
        let mut cx = context();
        let report = check_file(&app, &invocation(vec![app.clone()]), &mut cx);
        assert!(report.fatal.is_some(), "{report:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_scan_finds_apps_and_skips_build_output() {
        let dir = tempdir("scan");
        fs::create_dir_all(dir.join("target/debug")).expect("mkdir");
        fs::create_dir_all(dir.join("nested")).expect("mkdir");
        write(&dir, "main.splash", CLEAN);
        write(&dir, "nested/other.splash", CLEAN);
        write(&dir, "target/debug/generated.splash", CLEAN);
        let found = discover(&[dir.clone()]).expect("discover");
        assert_eq!(found.len(), 2, "{found:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_baseline_turns_a_finding_into_a_warning() {
        let dir = tempdir("baseline");
        fs::create_dir_all(&dir).expect("mkdir");
        write(&dir, "manifest.json", r#"{"schema":1,"id":"dev.example.app","capabilities":["storage"]}"#);
        let app = write(&dir, "main.splash", UNDECLARED);
        let baseline_path = write(&dir, BASELINE_FILE, "{\"tool\":\"splashcheck\",\"known\":[]}");
        // Adopt: write what is there now, then it is known.
        let mut cx = context();
        let reports = vec![check_file(&app, &invocation(vec![app.clone()]), &mut cx)];
        // Two findings: the name, and the cascade it causes in the property
        // that was going to hold it.
        assert_eq!(reports[0].findings.len(), 2, "{reports:?}");
        Baseline::write(&baseline_path, &reports).expect("write baseline");
        let baseline = Baseline::load(&baseline_path).expect("load baseline");
        assert_eq!(baseline.len(), 2);
        assert!(baseline.knows("main.splash", &reports[0].findings[0].message));
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut inv = invocation(vec![app.clone()]);
        inv.baseline = Some(baseline_path.clone());
        let status = run(&inv, &mut cx, &mut out, &mut err);
        let stdout = String::from_utf8(out).expect("utf8");
        // Every finding is known now, so each is a warning and the run passes.
        assert_eq!(status, 0, "{stdout}");
        assert!(stdout.contains("warn"), "{stdout}");
        assert!(stdout.contains("0 ok, 1 warn, 0 error"), "{stdout}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_finding_against_a_baseline_fails() {
        let dir = tempdir("new-finding");
        fs::create_dir_all(&dir).expect("mkdir");
        write(&dir, "manifest.json", r#"{"schema":1,"id":"dev.example.app","capabilities":["storage"]}"#);
        let app = write(&dir, "main.splash", UNDECLARED);
        let baseline_path = write(&dir, BASELINE_FILE, "{\"tool\":\"splashcheck\",\"known\":[{\"file\":\"main.splash\",\"message\":\"some other finding\"}]}");
        let mut cx = context();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut inv = invocation(vec![app.clone()]);
        inv.baseline = Some(baseline_path.clone());
        let status = run(&inv, &mut cx, &mut out, &mut err);
        assert_eq!(status, 1, "{}", String::from_utf8_lossy(&out));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_errors_name_the_problem() {
        for args in [
            vec!["--tier".to_owned(), "everything".to_owned(), "main.splash".to_owned()],
            vec!["--baseline".to_owned()],
            vec!["--nonsense".to_owned(), "main.splash".to_owned()],
            vec![],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
        assert!(matches!(parse(&["--help".to_owned()]), Ok(Parsed::Help)));
        assert!(matches!(parse(&["--version".to_owned()]), Ok(Parsed::Version)));
        match parse(&["--tier".to_owned(), "eval".to_owned(), "--".to_owned(), "main.splash".to_owned()]) {
            Ok(Parsed::Run(inv)) => {
                assert_eq!(inv.tier, Tier::Eval);
                assert_eq!(inv.paths, vec![PathBuf::from("main.splash")]);
            }
            _ => panic!("a run"),
        }
    }

    #[test]
    fn the_json_report_is_the_contract() {
        let dir = tempdir("json");
        fs::create_dir_all(&dir).expect("mkdir");
        write(&dir, "manifest.json", r#"{"schema":1,"id":"dev.example.app","capabilities":["storage"],"network":{"hosts":["example.invalid"]}}"#);
        let app = write(&dir, "main.splash", UNDECLARED);
        let mut cx = context();
        let mut inv = invocation(vec![app.clone()]);
        inv.json = true;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let status = run(&inv, &mut cx, &mut out, &mut err);
        assert_eq!(status, 1);
        let value: serde_json::Value = serde_json::from_slice(&out).expect("json report");
        assert_eq!(value["tool"], "splashcheck");
        assert_eq!(value["tier"], "all");
        let file = &value["files"][0];
        assert_eq!(file["manifest_found"], true);
        assert_eq!(file["declares_net"], true);
        assert_eq!(file["capabilities"][0], "storage");
        let finding = &file["findings"][0];
        assert_eq!(finding["severity"], "error");
        assert_eq!(finding["code"], "splash.eval");
        assert!(finding["line"].is_u64() && finding["column"].is_u64());
        assert!(finding["message"].is_string());
        assert_eq!(value["summary"]["error"], 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
