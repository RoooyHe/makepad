//! The VM query API: two read-only functions an editor-facing consumer uses to
//! ask which names exist.
//!
//! ADR 0001 (App Flow) fixes the count and the semantics before the first
//! caller exists, because every addition to a shared crate's public surface is
//! a promise someone has to keep for as long as the crate is at that major
//! version — and this crate is at `1.0.0` with no CI and no crates.io release
//! to keep the promise for it. The two functions:
//!
//! 1. [`object_keys`] — enumerate an object's own keys: each name once, in a
//!    deterministic (sorted) order, without values. Not the prototype chain.
//! 2. [`module_object`] — get a module object by name; a name the VM does not
//!    have is `None`.
//!
//! Both are reads an editor performs per keystroke: neither mutates the heap,
//! runs script, charges an instruction budget, or traps. A query API that
//! inherited the default type getter's trap would turn a completion request
//! into a script error, and the one place a language server must not produce an
//! error is the path it takes while the person is typing.
//!
//! ## The stability promise
//!
//! Within `1.x` this surface is **additive only**: new functions may appear;
//! removing or reinterpreting either function, or changing the order
//! [`object_keys`] returns, is `2.0` and must be called out in the fork's
//! release notes. Because there is no CI and no crates.io, the promise is kept
//! by hand and gated by two things that run with one command anyone can type:
//!
//! * `tests/query_api_surface.rs` — the checked-in surface file stops the build
//!   if either function is removed, renamed or reshaped, so a breaking change
//!   cannot land silently.
//! * `tests/query_corpus.txt` with `tests/query_api_corpus.rs` — the fixture
//!   corpus of the real objects a VM installs, with the key sets each must
//!   answer. A change to a module's members is a visible diff in a checked-in
//!   file, which is the intended friction, and a pull request that changes
//!   either function names that diff.
//!
//! What is deliberately NOT here: prototype-chain traversal (the consumer walks
//! it with [`crate::vm::ScriptVm::proto_map_iter_mut_with`] or the object's own
//! `proto` field — see ADR 0001 §2), and every question that reduces to neither
//! function. If a third function is ever needed, the ADR is amended, not
//! quietly extended.

use crate::heap::*;
use crate::makepad_live_id::*;
use crate::suggest::key_to_string_opt;
use crate::value::*;

/// Enumerate an object's own keys.
///
/// The object's own entries — the ones its map and its vec hold — each name
/// once, sorted, without values. The prototype chain is not walked: a widget's
/// inherited properties are the consumer's business, reached with
/// [`crate::vm::ScriptVm::proto_map_iter_mut_with`] or
/// [`crate::heap::ScriptHeap::proto`].
///
/// The order is part of the contract: sorted, so it does not change when an
/// object grows past [`crate::value_map::ValueMap`]'s spill threshold, and a
/// golden fixture built on it does not flake for a reason no reader could see.
///
/// An entry whose key is not a recoverable name — a number, say — is skipped:
/// this answers with names, and a name the VM cannot recover (an id no source
/// ever spelled, which the VM's own "Did you mean" lists also cannot show) is
/// not invented here.
pub fn object_keys(heap: &ScriptHeap, object: ScriptObject) -> Vec<String> {
    let mut names = Vec::new();
    for kv in heap.vec_ref(object) {
        if let Some(name) = key_to_string_opt(heap, kv.key) {
            names.push(name);
        }
    }
    for (key, _) in heap.map_ref(object).iter() {
        if let Some(name) = key_to_string_opt(heap, *key) {
            names.push(name);
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Get a module object by name.
///
/// A name is a dotted path of keys rooted at the VM's module table — the same
/// names a script writes after `use`, with or without the leading `mod`
/// (`mod` is the table's own name, the way the global `mod` is the table):
/// `"sys"`, `"mod.sys"`, `"mod.prelude.widgets"`.
///
/// A name the VM does not have — an unknown module, a missing key along the
/// path, a segment that is not an object — is `None`. It is never a trap and
/// never a panic: the VM has no business deciding what an unknown name means,
/// and a completion request is not the place to find out.
pub fn module_object(heap: &ScriptHeap, name: &str) -> Option<ScriptObject> {
    let mut segments = name.split('.');
    // The module table is its own `mod`: a leading one starts at the table,
    // any other first segment is a module inside it.
    let mut current = heap.modules;
    if let Some(first) = segments.next() {
        if first != "mod" {
            current = segment_object(heap, current, first)?;
        }
    }
    for segment in segments {
        current = segment_object(heap, current, segment)?;
    }
    Some(current)
}

/// One segment of a dotted name: the object stored under that key, if any.
fn segment_object(heap: &ScriptHeap, object: ScriptObject, segment: &str) -> Option<ScriptObject> {
    heap.map_ref(object)
        .get(&LiveId::from_str(segment).into())
        .and_then(|entry| entry.value.as_object())
}
