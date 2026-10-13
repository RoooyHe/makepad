//! The VM query API: `object_keys` and `module_object` — the two functions
//! ADR 0001 fixes and App Flow #9 ships.
//!
//! What these tests pin down, in the order the acceptance criteria name them:
//!
//! * `object_keys` answers "what names does this object have" — its own
//!   entries, each name once, sorted, without values, and without walking the
//!   prototype chain.
//! * `module_object` answers "give me the object called X" — a module object by
//!   (dotted) name, and `None` for a name the VM does not have.
//! * Neither runs script, mutates the heap or traps: they are the reads an
//!   editor performs per keystroke, and the one place a language server must
//!   not produce an error is the path it takes while the person is typing.
//! * The prototype chain is NOT in the API, and IS walkable from outside with
//!   the accessors that already exist — the last test is the proof ADR 0001 §2
//!   asks for, and the reason leaving the walk out is safe rather than tidy.

use makepad_script::query;
use makepad_script::*;

fn test_vm() -> ScriptVm<'static> {
    let host = Box::leak(Box::new(ScriptVmHost::new((), ())));
    ScriptVm {
        host,
        bx: Box::new(ScriptVmBase::new()),
    }
}

fn eval_value(vm: &mut ScriptVm, code: &str) -> ScriptValue {
    vm.with_instruction_limit(500_000, |vm| {
        vm.eval(ScriptMod {
            file: "query_api".into(),
            code: code.into(),
            ..Default::default()
        })
    })
}

fn eval_object(vm: &mut ScriptVm, code: &str) -> ScriptObject {
    eval_value(vm, code)
        .as_object()
        .unwrap_or_else(|| panic!("expected an object from {code:?}"))
}

// ---------------------------------------------------------------- object_keys

#[test]
fn own_keys_come_back_sorted_with_each_name_once() {
    let mut vm = test_vm();
    let object = eval_object(&mut vm, "let o = {b: 1, a: 2, c: 3}\no");
    assert_eq!(
        query::object_keys(&vm.bx.heap, object),
        vec!["a", "b", "c"],
        "own keys, each name once, in the deterministic (sorted) order"
    );
}

#[test]
fn a_name_stored_as_a_string_key_is_a_key_too() {
    let mut vm = test_vm();
    let object = eval_object(&mut vm, "let o = {\"a b\": 1, plain: 2}\no");
    assert_eq!(
        query::object_keys(&vm.bx.heap, object),
        vec!["a b", "plain"],
        "a key is a name whether the VM holds it as an id or as a string"
    );
}

#[test]
fn own_keys_do_not_walk_the_prototype_chain() {
    let mut vm = test_vm();
    // `view{text: 3}` is a fresh object whose prototype is `view`: its own keys
    // stop at `text`, exactly as ADR 0001 §2 fixes them.
    let object = eval_object(&mut vm, "let view = {draw_bg: 1, cursor: 2}\nview{text: 3}");
    assert_eq!(
        query::object_keys(&vm.bx.heap, object),
        vec!["text"],
        "inherited names are the consumer's business, not the VM's"
    );
}

#[test]
fn a_key_that_is_not_a_name_is_not_returned() {
    let mut vm = test_vm();
    // A numeric key has no name to return; it is skipped rather than invented.
    let object = eval_object(&mut vm, "let o = {1: 2}\no");
    assert_eq!(
        query::object_keys(&vm.bx.heap, object),
        Vec::<String>::new(),
        "an entry with no recoverable name is not a key"
    );
}

#[test]
fn querying_runs_no_script_and_reports_no_error() {
    let mut vm = test_vm();
    let object = eval_object(&mut vm, "let o = {a: 1}\no");
    let module = query::module_object(&vm.bx.heap, "std").unwrap();
    let _ = query::object_keys(&vm.bx.heap, object);
    let _ = query::object_keys(&vm.bx.heap, module);
    let _ = query::module_object(&vm.bx.heap, "nope");
    assert!(
        vm.take_errors().is_empty(),
        "a query is a read: it evaluates nothing, so it cannot raise"
    );
}

// -------------------------------------------------------------- module_object

#[test]
fn a_module_the_vm_has_comes_back_by_name() {
    let vm = test_vm();
    let std = query::module_object(&vm.bx.heap, "std").expect("mod.std is installed");
    assert!(
        query::object_keys(&vm.bx.heap, std).contains(&"print".to_string()),
        "the module object is the one the script sees: its keys are the stdlib's"
    );
}

#[test]
fn the_leading_mod_is_the_module_table_itself() {
    let vm = test_vm();
    assert_eq!(
        query::module_object(&vm.bx.heap, "mod").map(|o| o.index()),
        Some(vm.bx.heap.modules.index()),
        "`mod` names the module table, the way it does in a script"
    );
    assert_eq!(
        query::module_object(&vm.bx.heap, "mod.std").map(|o| o.index()),
        query::module_object(&vm.bx.heap, "std").map(|o| o.index()),
        "with or without the leading `mod`, the same object answers"
    );
}

#[test]
fn a_dotted_name_walks_keys_into_the_object() {
    let mut vm = test_vm();
    eval_value(&mut vm, "mod.mytest = {inner: {a: 1}}\n1");
    let inner = query::module_object(&vm.bx.heap, "mod.mytest.inner")
        .expect("a dotted name walks the module table's keys");
    assert_eq!(query::object_keys(&vm.bx.heap, inner), vec!["a"]);
    assert!(query::module_object(&vm.bx.heap, "mod.mytest").is_some());
    assert!(
        query::module_object(&vm.bx.heap, "mod.mytest.nope").is_none(),
        "the walk stops at the first key that is missing or not an object"
    );
    assert!(
        query::module_object(&vm.bx.heap, "mod.nope.inner").is_none(),
        "a missing first segment is nil, not a trap"
    );
}

#[test]
fn module_object_returns_the_object_the_script_sees() {
    let mut vm = test_vm();
    let assigned = eval_object(&mut vm, "mod.mytest = {a: 1}\nmod.mytest");
    assert_eq!(
        query::module_object(&vm.bx.heap, "mod.mytest").map(|o| o.index()),
        Some(assigned.index()),
        "the object the name answers with is the object the script assigned"
    );
}

#[test]
fn a_name_the_vm_does_not_have_is_none_never_a_trap() {
    let mut vm = test_vm();
    for name in ["", "mod.nope", "nope", "nope.deeper", "mod.", "mod..std", "std.print.x"] {
        assert!(
            query::module_object(&vm.bx.heap, name).is_none(),
            "{name:?} must be nil, not a panic"
        );
    }
    assert!(vm.take_errors().is_empty());
}

// ------------------------------------------------- the prototype chain, walked

#[test]
fn the_prototype_chain_is_walkable_from_outside_with_the_existing_accessors() {
    let mut vm = test_vm();
    // A widget's inherited properties live on the prototype: `draw_bg` comes
    // from the view, `text` is the widget's own. The API does not walk the
    // chain; the consumer does, with `heap.proto` (the object's own proto
    // field) and `object_keys`. This test is ADR 0001 §2's fourth criterion.
    let widget = eval_object(&mut vm, "let view = {draw_bg: 1, cursor: 2}\nview{text: 3}");
    assert_eq!(query::object_keys(&vm.bx.heap, widget), vec!["text"]);

    let mut names = Vec::new();
    let mut current = Some(widget);
    while let Some(object) = current {
        names.extend(query::object_keys(&vm.bx.heap, object));
        current = vm.bx.heap.proto(object).as_object();
    }
    names.sort();
    names.dedup();
    assert_eq!(
        names,
        vec!["cursor", "draw_bg", "text"],
        "the consumer reaches every inherited name with the accessors that already exist"
    );

    // The other accessor the ADR names walks the same chain root to leaf: the
    // closure sees the root's map first and the object's own map last.
    let mut map_lengths = Vec::new();
    vm.proto_map_iter_mut_with(widget, &mut |_vm, map| map_lengths.push(map.len()));
    assert_eq!(
        map_lengths,
        vec![2, 1],
        "proto_map_iter_mut_with visits root to leaf: the view's two keys, then the widget's one"
    );
}

#[test]
fn a_shadowed_name_is_the_consumers_policy_not_the_vms() {
    let mut vm = test_vm();
    // The widget declares its own draw_bg; the prototype still has one. The API
    // reports both objects' own keys and leaves which one wins to the consumer
    // — that is the policy ADR 0001 §2 keeps out of the VM.
    let widget = eval_object(&mut vm, "let view = {draw_bg: 1}\nview{draw_bg: 2}");
    let mut seen = Vec::new();
    let mut current = Some(widget);
    while let Some(object) = current {
        seen.extend(query::object_keys(&vm.bx.heap, object));
        current = vm.bx.heap.proto(object).as_object();
    }
    assert_eq!(seen, vec!["draw_bg", "draw_bg"], "both are reported; deduplicating is the consumer's call");
}
