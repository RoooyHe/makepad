//! Completion for the Splash DSL, answered from the live registry.
//!
//! A suggestion has to be a name that exists (App Flow issue #1, story 14): a
//! wrong suggestion is worse than no suggestion, so nothing here is a
//! hand-written list. Every name is read out of the VM an app itself runs in,
//! through the two functions ADR 0001 fixes the count of —
//! [`query::object_keys`] for an object's own names, [`query::module_object`]
//! for the object a dotted name points at — with the prototype chain walked
//! here rather than in the VM (ADR 0001 §2) and a property's type resolved
//! with the heap's own type table.
//!
//! The other half is the cursor, and it is the DSL's own punctuation: a `{`
//! after a name is a constructor, a `:` starts a value, a `.` asks a receiver
//! for its names. Nothing is evaluated, no instruction budget is charged and
//! no trap can be raised — the one path a language server must not fail on is
//! the one it takes while the person is typing (ADR 0001 §3).
//!
//! What this deliberately does not answer: a name that only exists at runtime.
//! A local's object, a value the body computed, a service name built by
//! string concatenation — none of them is in the registry, and inventing a
//! guess is the failure mode the whole design exists to avoid.

use crate::makepad_script::heap::ScriptHeap;
use crate::makepad_script::query;
use crate::makepad_script::value::ScriptObject;
use crate::makepad_script::LiveId;
use crate::widget_async::{CxSplashVmExt, SplashVmId};

/// The module table's widget families: what a `use mod.prelude.widgets.*`
/// import brings in, minus the prelude header's own names.
const WIDGETS: &str = "widgets";

/// The prelude an app's body is evaluated in. A type an app writes bare
/// (`Size`, `Flow`, `MouseCursor`) lives here, spread in from the module that
/// defines it.
const PRELUDE: &str = "prelude.widgets";

/// LSP's completion item kinds, the three this provider produces.
const KIND_CLASS: u64 = 7;
const KIND_ENUM_MEMBER: u64 = 13;
const KIND_PROPERTY: u64 = 10;

/// One suggestion: a name the live registry really has, and where it was read
/// from. `detail` is the registry path, shown beside the name in the editor
/// (issue #1, story 17).
#[derive(Debug)]
pub struct Suggestion {
    pub label: String,
    pub kind: u64,
    pub detail: String,
}

/// What the cursor is asking for, decided from the text alone.
///
/// `frames` is the stack of braces the cursor sits inside, innermost last, as
/// the scan read them; the registry turns it into the object whose names are
/// wanted. It is carried rather than resolved during the scan so that the text
/// half stays a pure function of the text.
#[derive(Debug)]
pub enum Question {
    /// A constructor name: the widget families.
    Widgets,
    /// A name inside a widget's braces: the properties its type accepts.
    Properties { frames: Vec<Frame> },
    /// A value after `name:`: the values the property's type accepts.
    Values { frames: Vec<Frame>, property: String },
    /// A name after `.`: the names the receiver holds.
    Members { receiver: String, frames: Vec<Frame> },
    /// Nothing here.
    Nothing,
}

/// One open brace the cursor is inside.
#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    /// `Label{` — a widget constructor.
    Widget(String),
    /// `draw_bg +: {` or `padding: Inset{` — a property's own block, whose
    /// type the registry resolves.
    Property(String),
    /// A block that is neither: `let x = {`, `fn f(){`, `for … {`.
    Other,
}

/// The DSL's keywords, the same list the editor's grammar highlights. An item
/// that begins with one is a declaration, and the name in it is a binding.
const KEYWORDS: [&str; 13] = [
    "if", "else", "for", "in", "while", "return", "break", "continue", "do", "use", "mod", "let", "fn",
];

// ---------------------------------------------------------------------------
// The cursor
// ---------------------------------------------------------------------------

/// What the cursor at `line`/`character` (LSP's 0-based, UTF-16) is asking for.
pub fn question(text: &str, line: usize, character: usize) -> Question {
    let Some(end) = cursor_offset(text, line, character) else {
        return Question::Nothing;
    };
    let scan = Scan::read(text, end);
    if scan.in_string || scan.in_comment {
        // Inside a literal or a comment there is no name to complete.
        return Question::Nothing;
    }
    if scan.after_dot {
        let receiver = trailing_path(&scan.run);
        if receiver.is_empty() {
            return Question::Nothing;
        }
        return Question::Members { receiver, frames: scan.frames };
    }
    // A value position: the run carries a `:` and the cursor is past it.
    if let Some(property) = value_property(&scan.run) {
        return Question::Values { frames: scan.frames, property };
    }
    // A binding's name, before its `=`: `let sta`, `fn lo`.
    if scan.declaration && !scan.run.contains('=') && !scan.run.contains('(') {
        return Question::Nothing;
    }
    if scan.frames.is_empty() {
        // The top level: a name here is the app's root widget, or the value a
        // binding is being given.
        if trailing_identifier(&scan.run).is_none() && !scan.run.is_empty() {
            return Question::Nothing;
        }
        return Question::Widgets;
    }
    Question::Properties { frames: scan.frames }
}

/// The cursor's byte offset in `text`.
///
/// LSP counts characters in UTF-16 and the DSL's lines hold CJK, so the column
/// is a UTF-16 count and not an index. A column past the end of its line reads
/// as the end of that line: an editor asks for a completion with the cursor at
/// the end of what has been typed, which is where a line ends.
fn cursor_offset(text: &str, line: usize, character: usize) -> Option<usize> {
    let mut at = 0usize;
    for (index, text_line) in text.split('\n').enumerate() {
        if index == line {
            let mut units = 0usize;
            for (byte, c) in text_line.char_indices() {
                if units >= character {
                    return Some(at + byte);
                }
                units += c.len_utf16();
            }
            return Some(at + text_line.len());
        }
        at += text_line.len() + 1;
    }
    None
}

/// The text up to the cursor, read as the DSL's punctuation says to read it.
struct Scan {
    /// The open braces the cursor is inside, innermost last.
    frames: Vec<Frame>,
    /// The item the cursor is in, since the last boundary. Whitespace is not
    /// kept: it separates items but says nothing about them.
    run: String,
    /// The first token of the statement the cursor is in, which is where a
    /// keyword shows up. It outlives the item: `let x = …` is one statement
    /// and `x` is its second item.
    first_word: String,
    /// Whether the statement has begun, so the first token of the next one is
    /// recognised as a first token.
    begun: bool,
    /// Whether the statement began with a keyword.
    declaration: bool,
    /// Inside an unterminated string literal.
    in_string: bool,
    /// Inside a `//` comment.
    in_comment: bool,
    /// The cursor sits right after a `.`.
    after_dot: bool,
    /// The last significant character read.
    last: Option<char>,
    /// Whether that character was part of an identifier, which is what tells a
    /// new token from a continuation.
    prev_ident: bool,
}

impl Scan {
    fn read(text: &str, end: usize) -> Self {
        let mut scan = Scan {
            frames: Vec::new(),
            run: String::new(),
            first_word: String::new(),
            begun: false,
            declaration: false,
            in_string: false,
            in_comment: false,
            after_dot: false,
            last: None,
            prev_ident: false,
        };
        let mut at = 0usize;
        while at < end {
            let Some(c) = text[at..].chars().next() else { break };
            at += c.len_utf8();
            if scan.in_comment {
                if c == '\n' {
                    scan.in_comment = false;
                    scan.boundary();
                }
                continue;
            }
            if scan.in_string {
                if c == '\\' {
                    // An escape: the character after it is part of the literal,
                    // so a `\"` cannot end it.
                    if let Some(next) = text[at..].chars().next() {
                        at += next.len_utf8();
                    }
                    continue;
                }
                if c == '"' {
                    scan.in_string = false;
                    // A finished value ends its item: `text: "x" Button{` is a
                    // sibling, not a continuation.
                    scan.boundary();
                }
                continue;
            }
            match c {
                '/' if text[at - 1..].starts_with("//") => scan.in_comment = true,
                '"' => scan.in_string = true,
                '\n' => scan.boundary(),
                '{' => {
                    let frame = scan.frame_for_brace();
                    scan.frames.push(frame);
                    scan.boundary();
                }
                '}' => {
                    scan.frames.pop();
                    scan.boundary();
                }
                ',' | ';' => scan.boundary(),
                c if c.is_whitespace() => scan.prev_ident = false,
                c if is_ident(c) => scan.ident(c),
                c => {
                    scan.run.push(c);
                    scan.last = Some(c);
                    scan.prev_ident = false;
                }
            }
        }
        scan.after_dot = scan.last == Some('.');
        scan
    }

    /// A boundary ends the item being read.
    fn boundary(&mut self) {
        self.run.clear();
        self.first_word.clear();
        self.begun = false;
        self.declaration = false;
        self.last = None;
        self.prev_ident = false;
    }

    /// One identifier character, which is where a new item can begin.
    fn ident(&mut self, c: char) {
        if !self.begun {
            // The statement's first token: it is where a keyword shows up, and
            // it is tested as it grows, so `le` already reads as `let`.
            self.first_word.push(c);
            self.declaration = KEYWORDS.iter().any(|keyword| keyword.starts_with(&self.first_word));
            self.begun = true;
        } else if !self.prev_ident && !self.run.is_empty() && self.run.ends_with(is_ident) {
            // A new token, and the item so far ends with a name: a name
            // follows a name, so this is a new item. `width: Fill height: Fit`
            // — `height` is not a longer word.
            self.run.clear();
        }
        self.run.push(c);
        self.last = Some(c);
        self.prev_ident = true;
    }

    /// What the `{` just read opens.
    fn frame_for_brace(&self) -> Frame {
        let run = self.run.trim_end_matches(|c: char| c == '+' || c == ' ');
        // `draw_bg +: {` — a property's own block.
        if run.ends_with(':') {
            return Frame::Property(property_before(run).unwrap_or_default());
        }
        let Some(name) = trailing_identifier(run) else {
            return Frame::Other;
        };
        let before = run[..run.len() - name.len()].trim_end();
        if before.is_empty() {
            // `Label{`
            return Frame::Widget(name);
        }
        if before.ends_with('=') {
            // `let x = Label{`, `title := Label{`
            return Frame::Widget(name);
        }
        if before.ends_with(':') {
            // `padding: Inset{` — the block is the property's own type.
            return Frame::Property(property_before(before).unwrap_or_default());
        }
        Frame::Other
    }
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The identifier a run ends with, if any.
fn trailing_identifier(run: &str) -> Option<String> {
    let trimmed = run.trim_end();
    let start = trimmed.rfind(|c: char| !is_ident(c)).map(|at| at + 1).unwrap_or(0);
    let name = &trimmed[start..];
    (!name.is_empty() && name.starts_with(|c: char| c.is_alphabetic() || c == '_')).then(|| name.to_owned())
}

/// The property a value position belongs to: the identifier before the run's
/// last `:`. A `:=` is a binding, not a property, so it is not one.
fn value_property(run: &str) -> Option<String> {
    let (before, _) = run.rsplit_once(':')?;
    if before.ends_with('=') {
        return None;
    }
    trailing_identifier(before.trim_end_matches('+'))
}

/// The property a `{` belongs to, from a run that ends at the colon.
fn property_before(run: &str) -> Option<String> {
    let (before, _) = run.rsplit_once(':')?;
    trailing_identifier(before.trim_end_matches('+'))
}

/// The dotted path that ends a run at a `.`.
fn trailing_path(run: &str) -> String {
    let trimmed = run.trim_end().trim_end_matches('.');
    let start = trimmed.rfind(|c: char| !(is_ident(c) || c == '.')).map(|at| at + 1).unwrap_or(0);
    trimmed[start..].to_owned()
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// The live registry: one isolate's VM, the same namespace an app's body is
/// evaluated in — the widget families, `sys`, `host`, the jailed `fs` — read
/// through the query API.
///
/// The isolate is the point. A host's own VM has the widget families and `sys`
/// but not `host`; an isolate has `host` and the jailed `fs` but not `sys`,
/// because the isolate setup installs neither ([`crate::splash::register_agent_module`]
/// is a host's own step — measured: an isolate's injected-global table is empty
/// without it). So the registry takes an isolate and installs what a host
/// installs, which is the namespace an app really runs in.
pub struct Registry<'a> {
    cx: &'a mut crate::Cx,
    vm: SplashVmId,
}

impl<'a> Registry<'a> {
    /// The isolate to read: the namespace an app runs in, allocated once and
    /// only ever read.
    ///
    /// Network on, the same a check asks for: whether an app may use the
    /// network is a declaration question, and the names are the same either
    /// way. Then what a host installs on its own VM — `sys` and `agent` are
    /// injected globals, and the isolate setup installs neither, so an isolate
    /// without this cannot name `sys.weather`.
    pub fn alloc(cx: &mut crate::Cx) -> SplashVmId {
        let vm = cx.alloc_splash_vm_with_network(true);
        cx.with_script_vm_id(vm, crate::splash::register_agent_module);
        vm
    }

    /// A registry over an isolate of its own, for a caller that does not keep
    /// one.
    pub fn new(cx: &'a mut crate::Cx) -> Self {
        let vm = Registry::alloc(cx);
        Registry { cx, vm }
    }

    /// A view of an isolate that already exists: the server allocates one at
    /// start-up and hands each request a view of it.
    pub fn at(cx: &'a mut crate::Cx, vm: SplashVmId) -> Self {
        Registry { cx, vm }
    }

    /// The isolate this reads, so its owner can free it.
    pub fn vm(&self) -> SplashVmId {
        self.vm
    }

    fn with_heap<R>(&mut self, f: impl FnOnce(&ScriptHeap) -> R) -> R {
        self.cx.with_script_vm_id(self.vm, |vm| f(&vm.bx.heap))
    }

    /// The object a dotted name points at.
    fn object(&mut self, name: &str) -> Option<ScriptObject> {
        self.with_heap(|heap| query::module_object(heap, name))
    }

    /// The names of one object, each labelled with `where` it came from.
    fn names_of_object(&mut self, object: ScriptObject, where_: &str, kind: u64) -> Vec<Suggestion> {
        self.with_heap(|heap| query::object_keys(heap, object))
            .into_iter()
            .map(|label| Suggestion { kind, detail: format!("{where_}.{label}"), label })
            .collect()
    }

    /// Every widget family the registry holds.
    ///
    /// The widget table holds the families and, beside them, the types the
    /// widget modules also register — enums (`Flow`, `Align`), api objects
    /// (`MenuItem`) and draw shaders. A family is the one whose prototype
    /// chain reaches an object the VM has a property table for, which is what
    /// makes `Label{ width: … }` legal and `Flow{ width: … }` not. A family
    /// with no property at all is therefore not in the list: the registry
    /// exposes no other marker, and a missed name is the cheaper error.
    pub fn widgets(&mut self) -> Vec<Suggestion> {
        let Some(table) = self.object(WIDGETS) else {
            return Vec::new();
        };
        let names = self.with_heap(|heap| query::object_keys(heap, table));
        let mut out = Vec::new();
        for name in names {
            let Some(object) = self.object(&format!("{WIDGETS}.{name}")) else {
                continue;
            };
            if self.with_heap(|heap| has_properties(heap, object)) {
                out.push(Suggestion { kind: KIND_CLASS, detail: format!("{WIDGETS}.{name}"), label: name });
            }
        }
        out
    }

    /// The properties a type accepts.
    ///
    /// The VM keeps them in the type's property table, not on the object: a
    /// widget's registration block sets only the fields it changes, so
    /// `object_keys` on `mod.widgets.Label` answers four names while the type
    /// answers twenty-two, and the twenty-two are the ones `Label{ … }`
    /// accepts — including the ones it inherits, which the table carries
    /// because the derive macro merges the base's fields into it. That is
    /// where inheritance lives in this VM; the object's own `proto` chain stops
    /// at the widget's own base and never reaches `View`.
    pub fn properties(&mut self, frames: &[Frame]) -> Vec<Suggestion> {
        let Some((owner, props)) = self.frame_properties(frames) else {
            return Vec::new();
        };
        props
            .into_iter()
            .map(|label| Suggestion { kind: KIND_PROPERTY, detail: format!("{owner}.{label}"), label })
            .collect()
    }

    /// The values a property accepts: the keys of the object its type points
    /// at. An enum's variants are that object's keys, which is why a value list
    /// and a member list are the same question.
    pub fn values(&mut self, frames: &[Frame], property: &str) -> Vec<Suggestion> {
        let Some(owner) = self.frame_object(frames) else {
            return Vec::new();
        };
        let Some(type_id) = self.with_heap(|heap| {
            heap.field_type_from_type_check(owner, LiveId::from_str(property))
        }) else {
            return Vec::new();
        };
        let name = self.with_heap(|heap| {
            heap.type_name_by_id(type_id).and_then(|name| name.as_string(|s| s.map(|s| s.to_owned())))
        });
        let Some(values) = self.with_heap(|heap| heap.type_default_for_id(type_id)) else {
            return Vec::new();
        };
        // A type with no registered name is still a type: say so rather than
        // inventing a path.
        let name = name.unwrap_or_else(|| "the property's type".to_owned());
        self.names_of_object(values, &name, KIND_ENUM_MEMBER)
    }

    /// The names a receiver holds after a `.`.
    ///
    /// A dotted receiver is a module path and nothing else. A single name is
    /// looked up in the order an app's own `use` would find it: a module, then
    /// a widget family, then a type the prelude puts in scope, then a global a
    /// host injects, then a property of the widget being written.
    pub fn members(&mut self, receiver: &str, frames: &[Frame]) -> Vec<Suggestion> {
        if !receiver.contains('.') {
            for path in [receiver.to_owned(), format!("{WIDGETS}.{receiver}"), format!("{PRELUDE}.{receiver}")] {
                if let Some(object) = self.object(&path) {
                    return self.names_of_object(object, &path, KIND_PROPERTY);
                }
            }
            if let Some(object) = self.injected(receiver) {
                return self.names_of_object(object, receiver, KIND_PROPERTY);
            }
        } else if let Some(object) = self.object(receiver) {
            return self.names_of_object(object, receiver, KIND_PROPERTY);
        }
        // A property of the widget under the cursor: `draw_bg.` asks the
        // property's own type for its names. A type's names are its property
        // table plus the keys of its default object — an enum's variants are
        // the object's keys and a struct's fields are the table's, so neither
        // kind is missed.
        let Some(owner) = self.frame_object(frames) else {
            return Vec::new();
        };
        let Some(type_id) = self.with_heap(|heap| {
            heap.field_type_from_type_check(owner, LiveId::from_str(receiver))
        }) else {
            return Vec::new();
        };
        let name = self.with_heap(|heap| {
            heap.type_name_by_id(type_id).and_then(|name| name.as_string(|s| s.map(|s| s.to_owned())))
        })
        .unwrap_or_else(|| receiver.to_owned());
        let table = self.with_heap(|heap| {
            heap.registered_type(type_id).map(|check| {
                check
                    .props
                    .iter_ordered()
                    .filter_map(|(id, _)| id.as_string(|s| s.map(|s| s.to_owned())))
                    .collect::<Vec<String>>()
            })
        });
        let mut out: Vec<Suggestion> = table
            .unwrap_or_default()
            .into_iter()
            .map(|label| Suggestion { kind: KIND_PROPERTY, detail: format!("{name}.{label}"), label })
            .collect();
        if let Some(values) = self.with_heap(|heap| heap.type_default_for_id(type_id)) {
            for suggestion in self.names_of_object(values, &name, KIND_PROPERTY) {
                if !out.iter().any(|kept| kept.label == suggestion.label) {
                    out.push(suggestion);
                }
            }
        }
        out
    }

    /// A global a host injects (`sys`, `agent`): not a module, so the module
    /// table has nothing to say about it.
    fn injected(&mut self, name: &str) -> Option<ScriptObject> {
        self.cx.with_script_vm_id(self.vm, |vm| {
            vm.bx.injected_globals.get(&LiveId::from_str(name)).and_then(|value| value.as_object())
        })
    }

    /// The object the innermost frame names: a widget family, or the type a
    /// property points at.
    fn frame_object(&mut self, frames: &[Frame]) -> Option<ScriptObject> {
        let (last, parents) = frames.split_last()?;
        match last {
            Frame::Widget(name) => self
                .object(&format!("{WIDGETS}.{name}"))
                .or_else(|| self.object(&format!("{PRELUDE}.{name}"))),
            Frame::Property(property) => {
                let owner = self.frame_object(parents)?;
                let type_id = self.with_heap(|heap| {
                    heap.field_type_from_type_check(owner, LiveId::from_str(property))
                })?;
                self.with_heap(|heap| heap.type_default_for_id(type_id))
            }
            Frame::Other => None,
        }
    }

    /// The property table the innermost frame's type carries, and the name to
    /// show beside each of its entries.
    ///
    /// A widget's own table carries what it inherits, so one lookup answers
    /// the whole question. A property's block is answered from the type the
    /// field is registered with: `padding: Inset{` answers `Inset`'s table,
    /// which is the four an app writes. A draw field is registered as its
    /// shader (`draw_bg` is a `DrawQuad`), so its block answers the shader's
    /// own table — a subset of what the VM accepts there, because a `+:` merge
    /// adds keys the type never declared. Measured: `draw_bg +: { color: … }`
    /// is accepted while `color` is in no table. The subset is the safe
    /// direction: nothing suggested is a name the registry does not hold.
    fn frame_properties(&mut self, frames: &[Frame]) -> Option<(String, Vec<String>)> {
        let (last, parents) = frames.split_last()?;
        match last {
            Frame::Widget(name) => {
                let object = self.frame_object(frames)?;
                let owner = self
                    .with_heap(|heap| {
                        heap.object_type_name_in_chain(object).and_then(|name| name.as_string(|s| s.map(|s| s.to_owned())))
                    })
                    .unwrap_or_else(|| name.clone());
                Some((owner, self.with_heap(|heap| properties_of(heap, object))?))
            }
            Frame::Property(property) => {
                let owner = self.frame_object(parents)?;
                let type_id = self.with_heap(|heap| {
                    heap.field_type_from_type_check(owner, LiveId::from_str(property))
                })?;
                let name = self.with_heap(|heap| {
                    heap.type_name_by_id(type_id).and_then(|name| name.as_string(|s| s.map(|s| s.to_owned())))
                })?;
                let props = self.with_heap(|heap| {
                    heap.registered_type(type_id).map(|check| {
                        check
                            .props
                            .iter_ordered()
                            .filter_map(|(id, _)| id.as_string(|s| s.map(|s| s.to_owned())))
                            .collect::<Vec<String>>()
                    })
                })?;
                Some((name, props))
            }
            Frame::Other => None,
        }
    }
}

/// Whether an object's prototype chain reaches one the VM has a property table
/// for.
fn has_properties(heap: &ScriptHeap, object: ScriptObject) -> bool {
    let mut at = Some(object);
    while let Some(current) = at {
        if let Some(type_id) = heap.object_type_id(current) {
            if heap.registered_type(type_id).is_some_and(|check| check.props.iter_ordered().next().is_some()) {
                return true;
            }
        }
        at = heap.proto(current).as_object();
    }
    false
}

/// The property table of the first typed object in a prototype chain, as
/// names. An entry whose name the VM cannot recover is skipped, for the same
/// reason [`query::object_keys`] skips one.
fn properties_of(heap: &ScriptHeap, object: ScriptObject) -> Option<Vec<String>> {
    let mut at = Some(object);
    while let Some(current) = at {
        if let Some(type_id) = heap.object_type_id(current) {
            if let Some(check) = heap.registered_type(type_id) {
                let names: Vec<String> = check
                    .props
                    .iter_ordered()
                    .filter_map(|(id, _)| id.as_string(|s| s.map(|s| s.to_owned())))
                    .collect();
                if !names.is_empty() {
                    return Some(names);
                }
            }
        }
        at = heap.proto(current).as_object();
    }
    None
}

/// Every suggestion for a cursor, in the order an editor should show them.
pub fn complete(registry: &mut Registry, text: &str, line: usize, character: usize) -> Vec<Suggestion> {
    match question(text, line, character) {
        Question::Widgets => registry.widgets(),
        Question::Properties { frames } => {
            let mut out = registry.properties(&frames);
            // Inside a widget's braces an item can be a property or a child
            // widget, and the DSL does not say which until the `{` arrives:
            // `Label{ text: … }` and `Label{ Label{ … } }` are both a name at
            // the same place. So both are offered, and each says which
            // registry path it came from. A block whose type the registry
            // knows — a property's own block, or an object literal — takes
            // properties alone, because a widget is not a name there.
            if frames.last().is_some_and(|frame| matches!(frame, Frame::Widget(_))) {
                out.extend(registry.widgets());
            }
            out
        }
        Question::Values { frames, property } => registry.values(&frames, &property),
        Question::Members { receiver, frames } => registry.members(&receiver, &frames),
        Question::Nothing => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(text: &str, line: usize, character: usize) -> Question {
        question(text, line, character)
    }

    #[test]
    fn a_constructor_name_asks_for_widgets() {
        // The cursor between the name and its `{`: the name is a
        // constructor's, so the answer is the widget families.
        let text = "View{\n    Label{text: \"x\"}\n}\n";
        assert!(matches!(ask(text, 0, 4), Question::Widgets), "{:?}", ask(text, 0, 4));
    }

    #[test]
    fn a_name_inside_braces_asks_for_properties() {
        let text = "View{\n    Label{\n        text: \"x\"\n    }\n}\n";
        match ask(text, 2, 8) {
            Question::Properties { frames } => {
                assert_eq!(
                    frames,
                    vec![Frame::Widget("View".to_owned()), Frame::Widget("Label".to_owned())]
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_property_block_asks_its_own_type_for_properties() {
        let text = "Label{\n    draw_bg +: {\n        color: #xffffff\n    }\n}\n";
        match ask(text, 2, 8) {
            Question::Properties { frames } => {
                assert_eq!(
                    frames,
                    vec![Frame::Widget("Label".to_owned()), Frame::Property("draw_bg".to_owned())]
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_typed_block_is_its_propertys_own_type() {
        // `padding: Inset{` — the block is the property's type, not a widget.
        let text = "View{\n    padding: Inset{\n        left: 10\n    }\n}\n";
        match ask(text, 2, 8) {
            Question::Properties { frames } => {
                assert_eq!(
                    frames,
                    vec![Frame::Widget("View".to_owned()), Frame::Property("padding".to_owned())]
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_named_child_is_a_widget_not_a_property() {
        // `title := Label{` — the third binding form names a child widget.
        let text = "View{\n    title := Label{\n        text: \"x\"\n    }\n}\n";
        match ask(text, 2, 8) {
            Question::Properties { frames } => {
                assert_eq!(
                    frames,
                    vec![Frame::Widget("View".to_owned()), Frame::Widget("Label".to_owned())]
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_value_position_asks_for_the_propertys_values() {
        let text = "View{\n    width: Fill height: Fit\n}\n";
        match ask(text, 1, 27) {
            Question::Values { frames, property } => {
                assert_eq!(property, "height");
                assert_eq!(frames, vec![Frame::Widget("View".to_owned())]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_dot_asks_the_receiver_for_its_names() {
        let text = "View{\n    on_render: fn(){ sys. }\n}\n";
        match ask(text, 1, 25) {
            Question::Members { receiver, .. } => assert_eq!(receiver, "sys"),
            other => panic!("{other:?}"),
        }
        // A dotted receiver is a module path.
        let text = "use mod.prelude.widgets.\n";
        match ask(text, 0, 24) {
            Question::Members { receiver, .. } => assert_eq!(receiver, "mod.prelude.widgets"),
            other => panic!("{other:?}"),
        }
        // A property of the widget under the cursor.
        let text = "Label{\n    draw_bg.\n}\n";
        match ask(text, 1, 12) {
            Question::Members { receiver, frames } => {
                assert_eq!(receiver, "draw_bg");
                assert_eq!(frames, vec![Frame::Widget("Label".to_owned())]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_string_and_a_comment_are_not_name_positions() {
        let text = "Label{\n    text: \"|\"\n}\n";
        assert!(matches!(ask(text, 1, 12), Question::Nothing));
        let text = "Label{\n    // a note\n    text: \"x\"\n}\n";
        assert!(matches!(ask(text, 1, 10), Question::Nothing));
        // An escaped quote does not end the literal, so the cursor after it is
        // still inside the string.
        let text = "Label{\n    text: \"a\\\"b|\"\n}\n";
        assert!(matches!(ask(text, 1, 15), Question::Nothing));
    }

    #[test]
    fn a_declaration_is_not_a_widget_position() {
        // `let` opens a binding; the name after it is not a constructor.
        let text = "let state = {items: []}\nView{ width: Fill }\n";
        assert!(matches!(ask(text, 0, 8), Question::Nothing));
        let text = "fn load(){ }\n";
        assert!(matches!(ask(text, 0, 8), Question::Nothing));
        // But the value a binding is given can be a widget.
        let text = "let title = Label{text: \"x\"}\n";
        assert!(matches!(ask(text, 0, 16), Question::Widgets), "{:?}", ask(text, 0, 16));
    }

    #[test]
    fn a_plain_object_block_is_not_a_widget() {
        // `let x = {` opens an object, not a widget. A key inside it reads as a
        // value position for a name nothing in the registry types, which the
        // registry answers with nothing.
        let text = "let x = {\n    a: 1\n}\n";
        match ask(text, 1, 6) {
            Question::Values { frames, property } => {
                assert_eq!(property, "a");
                assert_eq!(frames, vec![Frame::Other]);
            }
            other => panic!("{other:?}"),
        }
        // And the object's own block is not a place a widget name goes.
        assert!(matches!(ask(text, 2, 0), Question::Properties { .. }));
    }

    #[test]
    fn the_top_level_asks_for_widgets() {
        let text = "View{ width: Fill }\n";
        // Between the name and its `{`, and at the very start of the file.
        assert!(matches!(ask(text, 0, 4), Question::Widgets));
        assert!(matches!(ask(text, 0, 0), Question::Widgets));
        // A binding's value, before the object it is being given.
        let text = "let state = {items: []}\n";
        assert!(matches!(ask(text, 0, 12), Question::Nothing));
    }

    #[test]
    fn a_cursor_past_the_end_of_its_line_reads_as_the_lines_end() {
        let text = "View{\n    width: Fill\n}\n";
        assert!(matches!(ask(text, 1, 200), Question::Values { .. }));
    }

    #[test]
    fn a_column_is_utf16_and_not_a_character_index() {
        // A column is a UTF-16 count, and a line with CJK in it is shorter in
        // characters than the editor's column says: reading the column as an
        // index would land in the wrong place.
        let text = "Label{\n    你好: 1\n}\n";
        // Six units in is past the two characters and the colon.
        assert!(matches!(ask(text, 1, 7), Question::Values { .. }));
        // The same line without the colon is a name position at the same
        // column, which is the point: the column, not the byte count.
        let text = "Label{\n    你好\n}\n";
        assert!(matches!(ask(text, 1, 6), Question::Properties { .. }));
    }
}

/// The registry half, driven against a real isolate. These build one each,
/// which is what the language server does once at start-up.
mod registry_tests {
    use super::*;

    fn registry() -> Registry<'static> {
        // Leaked on purpose: a test's isolate is freed when the process is.
        let cx: &'static mut crate::Cx = Box::leak(Box::new(crate::Cx::new(Box::new(|_, _| {}))));
        cx.with_vm(crate::script_mod);
        Registry::new(cx)
    }

    fn labels(registry: &mut Registry, text: &str, line: usize, character: usize) -> Vec<String> {
        complete(registry, text, line, character)
            .into_iter()
            .map(|suggestion| suggestion.label)
            .collect()
    }

    #[test]
    fn the_widget_families_are_the_registrys_own() {
        let mut registry = registry();
        let names = registry.widgets();
        let labels: Vec<&str> = names.iter().map(|s| s.label.as_str()).collect();
        for widget in ["View", "Label", "Button", "ScrollBar"] {
            assert!(labels.contains(&widget), "{widget} is a widget family: {labels:?}");
        }
        // The prelude header's own names are not widget families, and the
        // registry says so by not listing them here.
        for not_a_widget in ["std", "math", "draw", "theme", "pod", "shader"] {
            assert!(!labels.contains(&not_a_widget), "{not_a_widget} is not a widget: {labels:?}");
        }
        assert!(names.iter().all(|s| s.detail.starts_with("widgets.")), "{:?}", names[0]);
    }

    #[test]
    fn a_widgets_properties_include_what_it_inherits() {
        let mut registry = registry();
        let frames = vec![Frame::Widget("Label".to_owned())];
        let found = registry.properties(&frames);
        let names: Vec<&str> = found.iter().map(|s| s.label.as_str()).collect();
        println!("Label properties: {names:?}");
        assert!(names.contains(&"text"), "Label's own property: {names:?}");
        // `width` comes from View, several prototypes up the chain.
        assert!(names.contains(&"width"), "inherited: {names:?}");
        assert!(names.contains(&"height"), "inherited: {names:?}");
        assert!(names.contains(&"visible"), "inherited: {names:?}");
        // A name appears once, at the object that owns it.
        assert_eq!(names.iter().filter(|n| **n == "width").count(), 1, "{names:?}");
    }

    #[test]
    fn a_propertys_own_block_has_its_types_properties() {
        let mut registry = registry();
        let frames = vec![Frame::Widget("View".to_owned()), Frame::Property("padding".to_owned())];
        let found = registry.properties(&frames);
        let names: Vec<&str> = found.iter().map(|s| s.label.as_str()).collect();
        println!("padding block properties: {names:?}");
        for edge in ["left", "top", "right", "bottom"] {
            assert!(names.contains(&edge), "{edge} is an Inset: {names:?}");
        }
        assert!(found.iter().all(|s| s.detail.starts_with("Inset.")), "{:?}", found[0]);
    }

    #[test]
    fn a_value_position_lists_the_types_own_values() {
        let mut registry = registry();
        let frames = vec![Frame::Widget("View".to_owned())];
        let found = registry.values(&frames, "width");
        let names: Vec<&str> = found.iter().map(|s| s.label.as_str()).collect();
        println!("width values: {names:?}");
        for value in ["Fill", "Fit", "Fixed"] {
            assert!(names.contains(&value), "{value} is a Size: {names:?}");
        }
        assert!(found.iter().all(|s| s.detail.starts_with("Size.")), "{:?}", found[0]);
    }

    #[test]
    fn a_module_receiver_lists_its_members() {
        let mut registry = registry();
        for module in ["sys", "host", "mod", "fs", "net", "std"] {
            let found = registry.members(module, &[]);
            println!("{module} members: {:?}", found.iter().map(|s| s.label.clone()).take(30).collect::<Vec<_>>());
            assert!(!found.is_empty(), "{module} has members");
            assert!(found.iter().all(|s| s.detail.starts_with(&format!("{module}."))), "{:?}", found[0]);
        }
    }

    #[test]
    fn a_type_receiver_lists_its_variants() {
        let mut registry = registry();
        let found = registry.members("Size", &[]);
        let names: Vec<&str> = found.iter().map(|s| s.label.as_str()).collect();
        assert!(names.contains(&"Fill"), "{names:?}");
    }

    #[test]
    fn a_widgets_property_receiver_lists_the_types_names() {
        // `draw_bg.` asks the property's own type, not the widget.
        let mut registry = registry();
        let frames = vec![Frame::Widget("View".to_owned())];
        let found = registry.members("draw_bg", &frames);
        let names: Vec<&str> = found.iter().map(|s| s.label.as_str()).collect();
        println!("draw_bg members: {names:?}");
        assert!(names.contains(&"rect_pos"), "{names:?}");
    }

    #[test]
    fn a_whole_path_from_a_cursor_to_names() {
        let mut registry = registry();
        // A name inside Label's braces: its properties, and the widget families
        // a child would come from.
        let text = "View{\n    width: Fill\n    Label{\n        text: \"x\"\n    }\n}\n";
        let names = labels(&mut registry, text, 3, 8);
        assert!(names.contains(&"text".to_owned()), "{names:?}");
        assert!(names.contains(&"width".to_owned()), "inherited: {names:?}");
        assert!(names.contains(&"Label".to_owned()), "a child widget: {names:?}");
        // A value position: the type's own values, and nothing else.
        let names = labels(&mut registry, text, 1, 15);
        assert!(names.contains(&"Fill".to_owned()), "{names:?}");
        assert!(!names.contains(&"Label".to_owned()), "no widgets in a value list: {names:?}");
        // A property's own block: the type the field is registered with.
        let text = "View{\n    padding: Inset{\n        left: 10\n    }\n}\n";
        let names = labels(&mut registry, text, 2, 8);
        assert!(names.contains(&"left".to_owned()), "{names:?}");
        assert!(!names.contains(&"Label".to_owned()), "no widgets in a property block: {names:?}");
        // A plain object block: nothing at all.
        let text = "let x = {\n    a: 1\n}\n";
        assert!(labels(&mut registry, text, 1, 6).is_empty());
    }
}
