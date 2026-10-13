//! The fixture corpus for the VM query API — ADR 0001 §4, App Flow #9.
//!
//! The corpus (`query_corpus.txt`, beside this file) names the modules a real
//! isolate installs and the key sets each must answer. This is the runner for
//! the entries tagged `script`: the modules `makepad-script` itself installs,
//! which a bare `ScriptVmBase` has. It runs with one command anyone can type —
//! `cargo test -p makepad-script` — because the fork has no CI and the crate is
//! not on crates.io, so a green local run that nobody wrote down is not a
//! promise. The widgets-tagged entries are checked by
//! `widgets/tests/vm_query_corpus.rs`, which reads this same file.
//!
//! What the runner proves, per entry:
//!
//! * the name answers with a module object (`module_object`),
//! * `object_keys` answers with exactly the corpus's key set,
//! * the object holds exactly the entry count the corpus records — a name is
//!   not always recoverable (a key registered in Rust with `id!` and never
//!   spelled by source is an id the VM cannot turn back into a string, and
//!   `object_keys` does not invent one), so the count is what keeps an entry
//!   the corpus cannot name a visible diff rather than a silent shrink,
//! * every entry here is a name — for these modules the crate interns its own
//!   names, so the count and the key set must agree exactly,
//! * and the module table holds exactly the modules the corpus names for this
//!   runner, so a module added or removed upstream is drift, not silence.

use makepad_script::query;
use makepad_script::*;

const CORPUS: &str = include_str!("query_corpus.txt");
const RUNNER: &str = "script";

fn test_vm() -> ScriptVm<'static> {
    let host = Box::leak(Box::new(ScriptVmHost::new((), ())));
    ScriptVm {
        host,
        bx: Box::new(ScriptVmBase::new()),
    }
}

struct Entry {
    name: String,
    runner: String,
    keys: Vec<String>,
    total: usize,
}

fn parse_corpus(text: &str) -> Vec<Entry> {
    text.lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next().unwrap_or_default().to_string();
            let runner = fields.next().unwrap_or_default().to_string();
            let keys = fields
                .next()
                .unwrap_or_default()
                .split(',')
                .filter(|key| !key.is_empty())
                .map(|key| key.to_string())
                .collect();
            let total = fields
                .next()
                .and_then(|total| total.parse().ok())
                .unwrap_or(0);
            Entry { name, runner, keys, total }
        })
        .collect()
}

#[test]
fn the_corpus_matches_the_modules_this_vm_installs() {
    let vm = test_vm();
    if std::env::var_os("QUERY_CORPUS_WRITE").is_some() {
        regenerate(&vm, CORPUS);
        return;
    }
    let entries: Vec<Entry> = parse_corpus(CORPUS)
        .into_iter()
        .filter(|entry| entry.runner == RUNNER)
        .collect();
    assert!(
        !entries.is_empty(),
        "the corpus names no {RUNNER} entries — it has been emptied, not updated"
    );

    for entry in &entries {
        let module = query::module_object(&vm.bx.heap, &entry.name)
            .unwrap_or_else(|| panic!("the corpus names {}, which this VM does not install", entry.name));
        let keys = query::object_keys(&vm.bx.heap, module);
        let total = vm.bx.heap.iter_len(module);
        assert_eq!(
            keys, entry.keys,
            "corpus drift for {}: object_keys answers with a different set \
             (if the module's members changed on purpose, regenerate with \
             QUERY_CORPUS_WRITE=1 and name the diff in the pull request)",
            entry.name
        );
        assert_eq!(
            total, entry.total,
            "corpus drift for {}: the object holds {} entries, not the {} the \
             corpus records (an entry the VM cannot name is still a diff)",
            entry.name, total, entry.total
        );
        assert_eq!(
            keys.len(),
            total,
            "{} has {} entries but only {} are names: an entry the VM cannot \
             name would silently shrink the corpus",
            entry.name,
            total,
            keys.len()
        );
    }

    let installed = vm.bx.heap.iter_len(vm.bx.heap.modules);
    assert_eq!(
        installed,
        entries.len(),
        "the module table holds {installed} modules but the corpus names {} for \
         this runner: a module was added or removed without the corpus moving",
        entries.len()
    );
}

/// Rewrite this runner's entries from the VM, leaving every other line — the
/// header, the comments and the other runner's entries — exactly as it stands.
/// Run the test again afterwards: a regenerated corpus is a diff a person reads
/// and names in the pull request, not a green checkmark.
fn regenerate(vm: &ScriptVm, corpus: &str) {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/query_corpus.txt");
    let mut out = String::new();
    for line in corpus.lines() {
        let trimmed = line.trim();
        let entry = parse_corpus(trimmed).into_iter().next();
        let rewritten = match entry {
            Some(entry) if entry.runner == RUNNER => {
                match query::module_object(&vm.bx.heap, &entry.name) {
                    Some(module) => format!(
                        "{}\t{}\t{}\t{}",
                        entry.name,
                        entry.runner,
                        query::object_keys(&vm.bx.heap, module).join(","),
                        vm.bx.heap.iter_len(module)
                    ),
                    // Not installed here any more: leave the line for the
                    // check to report, rather than silently dropping it.
                    None => line.to_string(),
                }
            }
            _ => line.to_string(),
        };
        out.push_str(&rewritten);
        out.push('\n');
    }
    std::fs::write(path, out).expect("write the regenerated corpus");
    println!("regenerated {path}");
}
