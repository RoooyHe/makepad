//! The stability promise for the query API's public surface — ADR 0001 §4,
//! App Flow #9.
//!
//! `makepad-script` is at 1.0.0 with no CI on this fork and no crates.io
//! release, so "semver" is a discipline this repository keeps by hand. Two
//! mechanisms, and nothing that needs a CI runner:
//!
//! * **The compiler is the gate.** The signatures below are bound to consts.
//!   Removing, renaming or reshaping either function — moving it out of
//!   `query`, changing a parameter or a return type — stops this file
//!   compiling, so the break cannot land silently.
//! * **The checked-in surface file is the review artifact.** It must keep
//!   describing the API, so an intentional change updates the file and the
//!   pull request names the diff. Together with the fixture corpus
//!   (`tests/query_corpus.txt`), that is the equivalent of a semver check for
//!   a crate nobody publishes.
//!
//! Within 1.x the rule is additive only: new functions may appear. Changing
//! the order `object_keys` returns is breaking too — the corpus pins it.

use makepad_script::query;
use makepad_script::*;

// The promised surface, spelled so the compiler checks it.
const OBJECT_KEYS: fn(&ScriptHeap, ScriptObject) -> Vec<String> = query::object_keys;
const MODULE_OBJECT: fn(&ScriptHeap, &str) -> Option<ScriptObject> = query::module_object;

/// The same surface as the checked-in text a reviewer reads.
const SURFACE: &str = "makepad_script::query::object_keys(heap: &ScriptHeap, object: ScriptObject) -> Vec<String>\nmakepad_script::query::module_object(heap: &ScriptHeap, name: &str) -> Option<ScriptObject>";

fn test_vm() -> ScriptVm<'static> {
    let host = Box::leak(Box::new(ScriptVmHost::new((), ())));
    ScriptVm {
        host,
        bx: Box::new(ScriptVmBase::new()),
    }
}

#[test]
fn the_promised_surface_is_reachable_exactly_as_spelled() {
    // The runtime half of the gate: the consts compile (else this file does
    // not) and the two functions answer through them.
    let vm = test_vm();
    assert!(MODULE_OBJECT(&vm.bx.heap, "mod.std").is_some());
    assert!(MODULE_OBJECT(&vm.bx.heap, "nope").is_none());
    let std = MODULE_OBJECT(&vm.bx.heap, "mod.std").unwrap();
    assert!(OBJECT_KEYS(&vm.bx.heap, std).contains(&"print".to_string()));
}

#[test]
fn the_checked_in_surface_file_describes_the_api() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/query_api_surface.txt");
    let checked_in = std::fs::read_to_string(path).expect("query_api_surface.txt is checked in beside this test");
    let promised: Vec<&str> = checked_in
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    assert_eq!(
        promised.join("\n"),
        SURFACE,
        "the checked-in surface file must describe the API: update it, and name \
         the change in the pull request, whenever the surface above changes"
    );
}
