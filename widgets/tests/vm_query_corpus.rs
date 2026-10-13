//! The widgets-tagged half of the VM query API fixture corpus — ADR 0001 §4,
//! App Flow #9.
//!
//! A real Splash isolate installs more than `makepad-script` itself does: the
//! widget registry (`mod.prelude.widgets`, `mod.widgets`), the host doorway
//! (`mod.host`), the jailed per-app storage (`mod.fs`), the `sys` module, the
//! agent module, and the Cx-free runtime's `net`. Those are the names a
//! completion provider asks about most, so the corpus covers them here, on the
//! isolate the host really builds — the same one `Splash` widgets run in.
//!
//! This runner reads the same corpus file as
//! `platform/script/tests/query_api_corpus.rs` (which checks the `script`
//! entries with `cargo test -p makepad-script`); this one runs with
//! `cargo test -p makepad-widgets`. Regenerate its entries with
//! `QUERY_CORPUS_WRITE=1`.

use makepad_widgets::makepad_script::{query, ScriptVm};
use makepad_widgets::{Cx, CxSplashVmExt};

const CORPUS: &str = include_str!("../../platform/script/tests/query_corpus.txt");
const RUNNER: &str = "widgets";
const CORPUS_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../platform/script/tests/query_corpus.txt"
);

/// The isolate modules the corpus names. ADR 0001 §4 lists `mod.prelude.widgets`,
/// `mod.host`, `mod.fs`, `mod.sys` and `mod.net` for a real isolate (`mod.std`
/// is the script crate's entry, checked there); the widget registry and the
/// agent module complete the set a completion provider asks about.
const ISOLATE_MODULES: &[&str] = &[
    "mod.agent",
    "mod.fs",
    "mod.host",
    "mod.net",
    "mod.prelude.widgets",
    "mod.sys",
    "mod.widgets",
];

/// Run `f` inside a real Splash isolate: the VM the host allocates for a
/// widget, with the widget registry, the host doorway, the jailed storage and
/// the runtime modules installed. `sys` and `agent` are registered per isolate
/// by the host (`splash::register_agent_module`), the way the hosts that embed
/// a Splash do — see the isolate test in `widgets/src/splash.rs`.
fn with_isolate<R>(f: impl FnOnce(&mut ScriptVm) -> R) -> R {
    let mut cx = Cx::new(Box::new(|_, _| {}));
    let vm_id = cx.alloc_splash_vm_with_network(true);
    cx.with_script_vm_id(vm_id, |vm| {
        makepad_widgets::splash::register_agent_module(vm);
        f(vm)
    })
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
fn the_corpus_matches_the_modules_a_real_isolate_installs() {
    with_isolate(|vm| {
        if std::env::var_os("QUERY_CORPUS_WRITE").is_some() {
            regenerate(vm, CORPUS);
            return;
        }

        let entries: Vec<Entry> = parse_corpus(CORPUS)
            .into_iter()
            .filter(|entry| entry.runner == RUNNER)
            .collect();

        for name in ISOLATE_MODULES {
            assert!(
                entries.iter().any(|entry| entry.name == *name),
                "the corpus no longer names {name}: the ADR's module list is the \
                 contract, and a runner that checks fewer names is a corpus that \
                 shrank without anyone saying so"
            );
        }

        for entry in &entries {
            let module = query::module_object(&vm.bx.heap, &entry.name)
                .unwrap_or_else(|| panic!("the corpus names {}, which this isolate does not install", entry.name));
            let keys = query::object_keys(&vm.bx.heap, module);
            let total = vm.bx.heap.iter_len(module);
            assert_eq!(
                keys, entry.keys,
                "corpus drift for {}: object_keys answers with a different set \
                 (if the module's members changed on purpose, regenerate with \
                 QUERY_CORPUS_WRITE=1 and name the diff in the pull request)",
                entry.name
            );
            // The total is recorded because a name is not always recoverable —
            // a key registered in Rust with `id!` and never spelled by source is
            // an id the VM cannot name, and `object_keys` does not invent one.
            // Recording the count keeps such an entry a visible diff.
            assert_eq!(
                total, entry.total,
                "corpus drift for {}: the object holds {} entries, not the {} the \
                 corpus records",
                entry.name, total, entry.total
            );
        }
    });
}

/// Rewrite this runner's entries from the isolate, leaving every other line —
/// the header, the comments and the script runner's entries — exactly as it
/// stands. Run the test again afterwards: a regenerated corpus is a diff a
/// person reads and names in the pull request, not a green checkmark.
fn regenerate(vm: &ScriptVm, corpus: &str) {
    let mut out = String::new();
    for line in corpus.lines() {
        let entry = parse_corpus(line).into_iter().next();
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
                    // Not installed any more: leave the line for the check to
                    // report, rather than silently dropping it.
                    None => line.to_string(),
                }
            }
            _ => line.to_string(),
        };
        out.push_str(&rewritten);
        out.push('\n');
    }
    std::fs::write(CORPUS_PATH, out).expect("write the regenerated corpus");
    println!("regenerated {CORPUS_PATH}");
}
