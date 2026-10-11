use crate::{
    makepad_derive_widget::*,
    makepad_draw::*,
    view::View,
    widget::*,
    widget_async::{
        CxSplashVmExt, SplashVmId, MAIN_SPLASH_VM_ID, WIDGET_SCRIPT_INSTRUCTION_LIMIT,
    },
    widget_tree::CxWidgetExt,
};

// `vm.host.cx_mut()` — reach the host Cx from a script helper.
use crate::makepad_draw::makepad_platform::script::vm::ScriptVmCx;
use makepad_script::vm::ScriptErrorRecord;

// `sys.dataset` and `sys.news_digest`: they share this module's helpers.
#[path = "splash_dataset.rs"]
mod splash_dataset;
#[path = "splash_news.rs"]
mod splash_news;

script_mod! {
    use mod.prelude.widgets_internal.*
    use mod.widgets.*

    mod.widgets.SplashBase = #(Splash::register_widget(vm))

    mod.widgets.Splash = set_type_default() do mod.widgets.SplashBase{
        width: Fill height: Fit
    }
}

#[derive(Script, WidgetRef, WidgetRegister)]
pub struct Splash {
    #[uid]
    uid: WidgetUid,
    #[source]
    source: ScriptObjectRef,
    #[deref]
    pub view: View,
    #[live]
    body: ArcStringMut,
    #[live]
    allow_net: bool,
    /// The app's private storage directory — the root of its jailed `fs`
    /// module (see splash_storage.rs). None (the default) = every storage
    /// call errors, which is right for previews/validation-less contexts.
    #[rust]
    sandbox_dir: Option<std::path::PathBuf>,
    #[rust]
    vm_id: SplashVmId,
    /// Index of this Splash's script body in its isolate's bodies list, cached
    /// at eval time so host->script calls don't depend on re-deriving the
    /// pointer-based ScriptMod identity (the struct could in principle move
    /// between eval and a later call).
    #[rust]
    body_id: Option<u16>,
    /// Host-trusted identity carried on every `host.request` this isolate
    /// makes (see splash_host.rs). None = requests arrive untagged, which a
    /// policy-enforcing host treats as deniable — right for previews.
    #[rust]
    host_tag: Option<String>,
    /// Granted-capability names `host.capabilities()` reports. Informational
    /// for the script's UI; enforcement is the host's per-request decision.
    #[rust]
    host_caps: Vec<String>,
    /// Whether this isolate's surface may raise user prompts (true for a
    /// foreground app host, false for background surfaces like home-screen
    /// widget tiles). Rides on every host.request as `may_prompt`.
    #[rust(true)]
    host_prompts: bool,
    /// Whole-jail byte cap when the host has granted this app extra room.
    /// None leaves the storage default.
    #[rust]
    storage_quota: Option<u64>,
    /// ADR 0002: when Some, this isolate is ENFORCED — files stay in its
    /// jail, and the runtime's own reads of the person's data and devices
    /// need their grants in `host_caps`. Since the ruling of 8 October 2026
    /// the hosts and the capabilities are the app's declarations: no network
    /// path or `host.request` is refused by the runtime. None keeps the
    /// informational behaviour every existing host relied on.
    #[rust]
    policy_hosts: Option<Vec<String>>,
    /// The instruction budget the app declares (cumulative, unlike the
    /// per-evaluation cap): counted against, never enforced.
    #[rust]
    instruction_budget: Option<u64>,
    /// Heap ceiling for this isolate, applied to its script heap.
    #[rust]
    memory_bytes: Option<usize>,
    /// What to call this script in error messages. Set by the host to the
    /// mini-app's id; empty for previews and one-off evals.
    ///
    /// Script errors are logged as `{file}:{line}:{col} - {message}` with
    /// nothing else to go on, and every Splash app used to report an empty
    /// file — so an error from a generated app named neither the app nor a
    /// usable line, and finding the culprit meant grepping every installed
    /// script by hand.
    #[rust]
    debug_name: String,
    #[rust]
    stylesheet: Option<crate::desktop_style::StyleSheet>,
    #[rust]
    style_pending: bool,
    /// Script timers the body's top-level statements registered at its last
    /// run. A style reapply runs those statements again, so these are stopped
    /// first; timers a handler started later are left alone.
    #[rust]
    startup_timers: Vec<LiveId>,
    /// `mod.*` entries the body introduced (its persistent models, e.g.
    /// `mod.state`). A style reapply restores them after the body's re-run, as
    /// `!vm.is_reload()`-guarded application state survives a style reload.
    #[rust]
    body_modules: Vec<LiveId>,
    /// Per-frame pump for live-data / time-driven bodies — see `handle_event`.
    /// Armed by `eval_styled_body` when the body binds `sys.*` live data, uses a
    /// time-based shader, or reads `sys.simsecs`; `NextFrame::default()` (off)
    /// for static cards so they never repaint per frame.
    #[rust]
    anim_next_frame: NextFrame,
    #[rust]
    animating: bool,
    /// Data-fetch epoch recorded at the last eval. A LATER fetch completing
    /// bumps the global epoch (`script::res`), and the pump re-evaluates the
    /// body once so the "—" placeholders baked in at eval time become values.
    #[rust]
    last_data_epoch: u64,
    #[rust]
    last_sim_tick: u64,
    /// 1 Hz interval auto-started for bodies that define `fn tick()`.
    #[rust]
    tick_timer: Timer,
}

impl ScriptHook for Splash {
    fn on_after_apply(&mut self,vm:&mut ScriptVm,_apply:&Apply,_scope:&mut Scope,_value:ScriptValue) {
        let sheet=crate::desktop_style::current(vm);
        if sheet!=self.stylesheet {self.stylesheet=sheet;self.style_pending=true;}
    }
}

// `let fs = mod.fs` puts the jailed storage module (splash_storage.rs) in
// scope as a bare name — app scripts say `fs.read("/x")`, not `mod.fs.read`.
// A script reassigning `fs` only sabotages its own binding; the jail itself
// lives host-side.
/// Lines the no-net prefix occupies, which is the amount every reported
/// script line is ahead of the app's own file. Subtract it to get the line in
/// the `.splash` source; net-enabled apps carry one extra line
/// ([`SPLASH_NET_PREFIX_LINES`]).
///
/// It cannot be zero: the prefix would then have to share line 1 with the
/// app's first line, and a generated app's first line is its `// name:`
/// header — a comment, which would swallow the rest of the prefix.
pub const SPLASH_PREFIX_LINES: u32 = 3;
/// Line offset for net-enabled apps (their prefix adds `use mod.net`).
pub const SPLASH_NET_PREFIX_LINES: u32 = 4;

const SPLASH_PREFIX: &str =
    "use mod.prelude.widgets.*\nlet fs = mod.fs\nlet host = mod.host\nView{height:Fit, ";
const SPLASH_NET_PREFIX: &str =
    "use mod.prelude.widgets.*\nuse mod.net\nlet fs = mod.fs\nlet host = mod.host\nView{height:Fit, ";
const SPLASH_EVAL_INSTRUCTION_LIMIT: usize = 200_000;

impl Splash {
    /// Stable identity for the streaming script body, based on pointer address.
    fn self_id(&self) -> usize {
        self as *const Self as usize
    }

    /// Names this script for error reporting — see [`Self::debug_name`].
    pub fn set_debug_name(&mut self, name: &str) {
        self.debug_name = name.to_string();
    }

    /// Host-selected presentation for an embedded surface with its own isolate.
    /// The host's surrounding chrome can retain its own widget theme.
    pub fn set_stylesheet(&mut self, cx: &mut Cx, sheet: crate::desktop_style::StyleSheet) {
        if self.stylesheet.as_ref() != Some(&sheet) {
            self.stylesheet = Some(sheet);
            self.style_pending = true;
            self.view.redraw(cx);
        }
    }

    /// The body's identity within its isolate, carried in `ScriptMod`'s
    /// `module_path`.
    ///
    /// It used to live in `line`, which the VM adds to a script's real line
    /// when it reports an error (`ScriptCode::ip_to_loc`) — so every location
    /// came out as `real_line + a_pointer_address`, i.e. numbers like
    /// 1804943384. `module_path` is a free-form string nobody else reads for
    /// these bodies, so identity and line no longer fight over one field.
    fn body_key(&self) -> String {
        format!("splash#{}", self.self_id())
    }

    /// The name a script error reports. Falls back to something searchable
    /// rather than the empty string that made these untraceable.
    ///
    /// Errors read `splash:<name>:<line>:<col>`, where `<line>` is ahead of
    /// the app's own file by [`SPLASH_PREFIX_LINES`].
    fn source_label(&self) -> String {
        if self.debug_name.is_empty() {
            format!("splash:{}", self.self_id())
        } else {
            format!("splash:{}", self.debug_name)
        }
    }

    fn eval_body(&mut self, cx: &mut Cx) {self.eval_styled_body(cx,false);}

    fn eval_styled_body(&mut self,cx:&mut Cx,preserve:bool) {
        self.eval_styled_body_with_apply(cx, preserve, &Apply::ScriptReapply);
    }

    fn eval_styled_body_with_apply(&mut self, cx: &mut Cx, preserve: bool, apply: &Apply) {
        if self.body.as_ref().is_empty() {
            return;
        }

        if self.vm_id == MAIN_SPLASH_VM_ID {
            self.vm_id = cx.alloc_splash_vm_with_network(self.allow_net);
        }
        // (Re)bind this isolate's storage jail and host-bridge identity.
        // Keyed by heap so the script can neither read nor retarget them.
        let heap_key = cx.with_script_vm_id(self.vm_id, |vm| vm.bx.heap.heap_key());
        crate::splash_storage::set_root_for_heap(heap_key, self.sandbox_dir.clone());
        crate::splash_host::set_tag_for_heap(heap_key, self.host_tag.clone());
        crate::splash_host::set_caps_for_heap(heap_key, self.host_caps.clone());
        crate::splash_host::set_prompts_for_heap(heap_key, self.host_prompts);
        crate::splash_storage::set_quota_for_heap(heap_key, self.storage_quota);
        self.apply_policy(cx, heap_key);
        if !crate::splash_policy::may_run(heap_key) {
            log!("splash: {} is stopped: its instruction budget is spent", self.source_label());
            return;
        }

        let body_key = self.body_key();
        let body = self.body.as_ref();
        // Full code string: prefix + body (no closing - parser auto-closes)
        let prefix = if self.allow_net {
            SPLASH_NET_PREFIX
        } else {
            SPLASH_PREFIX
        };
        let code = format!("{}{}", prefix, body);

        // Identity is stable (same module_path each call) AND the location
        // fields are left alone, so `ip_to_loc` reports the script's own line
        // instead of one offset by a pointer address.
        let script_mod = ScriptMod {
            cargo_manifest_path: String::new(),
            module_path: self.body_key(),
            file: self.source_label(),
            line: 0,
            column: 0,
            code: String::new(),
            values: vec![],
        };

        let vm_id = self.vm_id;
        let sheet=self.stylesheet.clone();
        self.style_pending=false;
        // A style reapply runs the body's top-level statements again: only the
        // body defines the widget tree, and that tree has to be rebuilt on the
        // restyled `mod.widgets` protos (reload mode allocates a fresh widgets
        // module, so the existing instances' protos are stale). The re-run's
        // side effects are contained here, not repeated:
        // - timers the previous run registered are stopped first, so a
        //   top-level `start_interval` is re-armed rather than stacked;
        // - `mod.*` entries the body introduced (`mod.state`) are restored
        //   after the run, so the app's data survives the way
        //   `!vm.is_reload()`-guarded application state does.
        // Timers a handler started later and `let` bindings are not touched.
        if preserve {
            stop_script_timers(cx, &std::mem::take(&mut self.startup_timers));
        }
        let timers_before = isolate_timer_ids(cx, heap_key);
        let body_modules = std::mem::take(&mut self.body_modules);
        // A panicking body must not take the host down: contain it here (an
        // unwinding `with_script_vm_id` already restores the outer VM).
        let mut eval_out = None;
        crate::widget_async::contain_isolate_panic("app source eval", || {
        eval_out = Some(cx.with_script_vm_id(vm_id, |vm| {
            if let Some(sheet)=sheet {
                if crate::desktop_style::current(vm).as_ref()!=Some(&sheet) {
                    crate::desktop_style::install(vm,sheet);
                    // Trusted framework themes need their packaged font resolver.
                    // Preserve the jail and expose only the resolver during theme
                    // registration, removing it before evaluating the app body.
                    vm.with_reload(|vm| {
                        crate::script_eval!(vm, {mod.res = {crate_resource: mod.prelude.widgets.crate_resource}});
                        crate::widgets_mod(vm);
                        crate::desktop_style::apply_widgets(vm);
                        crate::script_eval!(vm, {mod.res = nil});
                        crate::widget_async::apply_splash_isolate_mods(vm);
                        crate::widget_async::forget_granted_isolate_mods(heap_key);
                    });
                }
            }
            // Vocabulary this app's grants entitle it to, now that its policy
            // is seated (see `register_splash_isolate_mod_for`).
            crate::widget_async::apply_granted_isolate_mods(vm);
            // Everything on `mod` that is not the body's own; whatever the run
            // adds beyond this is the body's.
            let mut known = module_keys(vm);
            known.retain(|key| !body_modules.contains(key));
            let saved = if preserve { snapshot_modules(vm, &body_modules) } else { Vec::new() };
            // Streaming evaluation silences console errors. Retain them so a
            // completed card cannot fail with an unexplained blank surface.
            let previous_errors = vm.bx.captured_errors.replace(Vec::new());
            let value = vm.with_instruction_limit(SPLASH_EVAL_INSTRUCTION_LIMIT, |vm| {
                if preserve {
                    vm.with_reload(|vm| vm.eval_with_append_source(script_mod, &code, NIL.into()))
                } else {
                    vm.eval_with_append_source(script_mod, &code, NIL.into())
                }
            });
            let evaluation_errors = vm.take_errors();
            vm.bx.captured_errors = previous_errors;
            if preserve {
                restore_modules(vm, saved);
            }
            if !crate::splash_policy::charge(heap_key, vm.last_limit_consumed() as u64) {
                log!("splash: an app spent its instruction budget evaluating its body and is stopped");
            }
            let body_modules: Vec<LiveId> = module_keys(vm)
                .into_iter()
                .filter(|key| !known.contains(key))
                .collect();
            let view = if !value.is_err() && !value.is_nil() {
                if preserve {
                    let walk=self.view.walk;
                    self.view.script_apply(vm,apply,&mut Scope::empty(),value);
                    self.view.walk=walk;
                    None
                } else {Some(View::script_from_value(vm, value))}
            } else {
                // A body that fails to evaluate leaves the Splash showing
                // its previous view — or nothing at all. Say so: a silent
                // blank widget is the hardest bug in this file to find.
                if value.is_err() {
                    for e in evaluation_errors {
                        log!("splash: {}", e);
                    }
                } else {
                    log!("splash: script body evaluated to nothing (no root view)");
                }
                None
            };
            (view, body_modules)
        }));
        });
        let (new_view, body_modules) = eval_out.unwrap_or((None, Vec::new()));
        self.body_modules = body_modules;
        self.startup_timers = isolate_timer_ids(cx, heap_key)
            .into_iter()
            .filter(|id| !timers_before.contains(id))
            .collect();

        let installed = new_view.is_some();
        if let Some(mut view) = new_view {
            // The HOST owns this widget's slot in its tree: `Splash{width: Fill
            // height: Fill}` is a promise about the space the Splash occupies,
            // and rebuilding the body from script must not silently take it
            // away. Without this the freshly-minted view arrives with the
            // Splash type-default walk (Fill/Fit) and a host asking for a
            // full-height Splash gets a zero-height one that draws nothing.
            view.walk = self.view.walk;
            // Cache the body index for host->script calls (call_script_fn etc).
            self.body_id = cx.with_script_vm_id(vm_id, |vm| {
                let bodies = vm.bx.code.bodies.borrow();
                bodies.iter().position(|body| match &body.source {
                    ScriptSource::Mod(m) => m.module_path == body_key,
                    _ => false,
                })
            })
            .map(|i| i as u16);
            self.view = view;
            // Make `ui` a global in this splash's VM so helper `fn`s inside the block can use
            // `ui.<id>.set_text(...)`, not just inline handlers. It points at the Splash widget
            // itself (not the wrapper view, which never becomes a widget-tree node since
            // `children()` forwards through it), so confined subtree lookups resolve.
            crate::widget_async::inject_splash_ui_handle(cx, self.vm_id, self.uid);
            cx.widget_tree_mark_dirty(self.uid);
        }
        // ---- live-data / animation bookkeeping ----
        // A body that binds async live data (sys.weather/…) evaluates against
        // "—" placeholders while its fetches are in flight; arm the per-frame
        // pump so `handle_event` can re-evaluate it once those land. `fn tick()`
        // bodies manage their own updates in place at 1 Hz and must never be
        // rebuilt underneath (a nav card would tear down its MapView on every
        // tile fetch), so they get the timer instead of the pump.
        let (has_tick, animating) = {
            let b = self.body.as_ref();
            let is_tick = b.contains("fn tick(") || b.contains("fn tick (");
            (
                is_tick,
                b.contains("draw_pass.time")
                    || b.contains("WeatherIcon")
                    || b.contains("sys.simsecs")
                    || (body_binds_live_data(b) && !is_tick),
            )
        };
        if !preserve {
            cx.stop_timer(self.tick_timer);
            if has_tick {
                self.tick_timer = cx.start_interval(1.0);
            }
        }
        self.animating = animating;
        self.anim_next_frame = if animating { cx.new_next_frame() } else { NextFrame::default() };
        // Record the epoch AT this eval so the pump only re-evaluates when a
        // LATER fetch completes. eval only reads cached data / fires pending
        // fetches — it never bumps the epoch — so this settles and cannot loop.
        self.last_data_epoch = cx.script_data_fetch_epoch();
        log!(
            "[SPLASH] eval: {} bytes preserve={} view={} animating={} tick={} epoch={}",
            self.body.as_ref().len(), preserve, installed, animating, has_tick, self.last_data_epoch
        );
    }

    /// Tears down this Splash's isolate (if any), returning it to the empty
    /// state a freshly-created Splash has. The isolate-minted view holds refs
    /// into the isolate heap, so it is REPLACED with a fresh empty view built
    /// in the main VM BEFORE the isolate is reclaimed; the reclamation then
    /// stops the isolate's timers and drops its storage-jail binding. A later
    /// non-empty `set_text` allocates a fresh isolate as usual.
    fn stop(&mut self, cx: &mut Cx) {
        if self.vm_id == MAIN_SPLASH_VM_ID {
            return; // nothing running
        }
        self.view = cx.with_vm(|vm| View::script_from_value(vm, NIL.into()));
        self.body_id = None;
        self.startup_timers.clear();
        self.body_modules.clear();
        crate::widget_async::mark_splash_isolate_dead(self.vm_id);
        self.vm_id = MAIN_SPLASH_VM_ID;
        // Reclaim now (Cx is in hand and nothing runs in the isolate) so the
        // timers stop immediately rather than lingering to the next pump.
        crate::widget_async::gc_dead_splash_isolates(cx);
        cx.widget_tree_mark_dirty(self.uid);
    }
}

/// The script timers whose callbacks live in the isolate heap `heap_key`.
fn isolate_timer_ids(cx: &Cx, heap_key: usize) -> Vec<LiveId> {
    cx.script_data
        .timers
        .timers
        .iter()
        .filter(|t| t.callback.heap_key() == heap_key)
        .map(|t| t.id)
        .collect()
}

/// Stops and drops the script timers with these ids.
fn stop_script_timers(cx: &mut Cx, ids: &[LiveId]) {
    if ids.is_empty() {
        return;
    }
    let stale: Vec<_> = cx
        .script_data
        .timers
        .timers
        .iter()
        .filter(|t| ids.contains(&t.id))
        .map(|t| (t.id, t.timer))
        .collect();
    for (id, timer) in stale {
        cx.stop_timer(timer);
        cx.script_data.timers.timers.retain(|t| t.id != id);
    }
}

/// The entries of the isolate's `mod` namespace.
fn module_keys(vm: &mut ScriptVm) -> Vec<LiveId> {
    let modules = vm.bx.heap.modules;
    vm.map_mut_with(modules, |_vm, map| {
        map.iter().filter_map(|(key, _)| key.as_id()).collect()
    })
}

/// The current values of these `mod` entries. Objects are rooted so a
/// collection during the body's re-run cannot free one only the snapshot
/// still refers to.
fn snapshot_modules(
    vm: &mut ScriptVm,
    keys: &[LiveId],
) -> Vec<(LiveId, ScriptValue, Option<ScriptObjectRef>)> {
    let modules = vm.bx.heap.modules;
    keys.iter()
        .map(|key| {
            let value = vm.bx.heap.value(modules, (*key).into(), NoTrap);
            let root = value.as_object().map(|obj| vm.bx.heap.new_object_ref(obj));
            (*key, value, root)
        })
        .collect()
}

fn restore_modules(vm: &mut ScriptVm, saved: Vec<(LiveId, ScriptValue, Option<ScriptObjectRef>)>) {
    let modules = vm.bx.heap.modules;
    for (key, value, _root) in saved {
        vm.bx.heap.set_value_def(modules, key.into(), value);
    }
}

/// Evaluates a Splash body in a throwaway isolate — the same prelude prefix,
/// instruction limit, and network gating the `Splash` widget itself uses — and
/// returns the formatted script errors, freeing the isolate afterwards. An
/// empty result means the body parses and its root expression evaluates to a
/// widget tree; runtime errors inside handlers can of course still occur later.
///
/// This is the widget's `eval_body` as a checked dry run: hosts that install
/// script source from outside (downloads, AI generation, user input) can
/// validate it — with real errors to show or feed back — before committing it
/// to a live `Splash`, whose own eval silently keeps the old view on failure.
///
/// Caveats (identical to installing the same source in a real `Splash`, so
/// validation adds no NEW exposure): the instruction limit bounds compute but
/// not heap growth, so a hostile script can still allocate aggressively within
/// its budget; and top-level side effects (e.g. `start_interval`) run and live
/// until the marked-dead isolate is reclaimed by `gc_dead_splash_isolates`.
/// Validate a card or app body without a live host, and return the errors as
/// text. [`validate_splash_body_records`] is the same check with each error's
/// location and message apart, which is what an editor or a linter needs:
/// `loc` carries the line and column, `message` is the text without its
/// location or the interpreter's origin.
///
/// The reported line is ahead of the body's own file by
/// [`SPLASH_PREFIX_LINES`] (or [`SPLASH_NET_PREFIX_LINES`] with `allow_net`),
/// because the body is evaluated inside the harness prefix. Subtract it to get
/// the line in the `.splash` source.
pub fn validate_splash_body(cx: &mut Cx, body: &str, allow_net: bool) -> Vec<String> {
    validate_splash_body_records(cx, body, allow_net)
        .into_iter()
        .map(|record| record.text)
        .collect()
}

/// Map a location in the evaluated source back to the body's own file.
///
/// Two adjustments, both of which an editor needs and neither of which it can
/// make for itself:
///
/// * `ScriptLoc`'s line and column are **0-based** — the tokenizer counts from
///   zero — so the app's own line is `line - the prefix's newlines + 1`.
/// * The body's FIRST line shares a physical line with the prefix's tail
///   (`View{height:Fit, `), so a column on it is ahead by that tail's width.
///
/// A location inside the prefix itself is the harness's own; it is reported as
/// 1:1 rather than as a zero or a negative.
///
/// The returned column is the VM's, which points at the token the interpreter
/// was at — often the property's key or the enclosing statement rather than
/// the name in the message. Callers that want the name's own column refine it
/// against the line's text; see `splashcheck`.
pub fn body_location(line: u32, column: u32, allow_net: bool) -> (usize, usize) {
    let (prefix, newlines) = if allow_net {
        (SPLASH_NET_PREFIX, SPLASH_NET_PREFIX_LINES)
    } else {
        (SPLASH_PREFIX, SPLASH_PREFIX_LINES)
    };
    if line < newlines {
        // Inside the prefix: the harness's own line, not the app's.
        return (1, column as usize + 1);
    }
    let body_line = (line - newlines + 1) as usize;
    let body_column = if line == newlines {
        // The body's first line continues the prefix's last one.
        let shared = prefix.rsplit('\n').next().unwrap_or("").chars().count() as u32;
        column.saturating_sub(shared) as usize + 1
    } else {
        column as usize + 1
    };
    (body_line, body_column.max(1))
}

/// [`validate_splash_body`] with each error's location and message apart.
pub fn validate_splash_body_records(cx: &mut Cx, body: &str, allow_net: bool) -> Vec<ScriptErrorRecord> {
    let vm_id = cx.alloc_splash_vm_with_network(allow_net);
    // Give the dry run a throwaway storage jail so top-level `fs.read` boot
    // loads validate instead of erroring "storage not available". The path is
    // unpredictable and created with an EXCLUSIVE mkdir (fails EEXIST on any
    // pre-existing entry incl. a planted symlink, so it never follows one out
    // of temp); on failure the jail is simply left unset (fs calls error, same
    // as a preview). Reclaimed below; per-vm so concurrent validations differ.
    let scratch = std::env::temp_dir().join(format!(
        "splash_validate_{}_{}",
        std::process::id(),
        vm_id.0,
    ));
    let heap_key = cx.with_script_vm_id(vm_id, |vm| vm.bx.heap.heap_key());
    // Clear a leftover from a crashed run (we own this exact name), then take
    // it exclusively.
    let _ = std::fs::remove_dir_all(&scratch);
    if std::fs::create_dir(&scratch).is_ok() {
        crate::splash_storage::set_root_for_heap(heap_key, Some(scratch.clone()));
    }
    let prefix = if allow_net {
        SPLASH_NET_PREFIX
    } else {
        SPLASH_PREFIX
    };
    let code = format!("{}{}", prefix, body);
    let script_mod = ScriptMod {
        cargo_manifest_path: String::new(),
        module_path: format!("splash-validate#{}", vm_id.0),
        // Named, and NOT via `line` — the validator's errors are shown to the
        // user (and fed back to the agent as repair input), so a location
        // offset by a vm id would be actively misleading there.
        file: "splash:validating".to_string(),
        line: 0,
        column: 0,
        code: String::new(),
        values: vec![],
    };
    let mut errors_out = vec![ScriptErrorRecord::from(
        "the script crashed its isolate during validation".to_string(),
    )];
    crate::widget_async::contain_isolate_panic("validation eval", || {
    errors_out = cx.with_script_vm_id(vm_id, |vm| {
        // Capture instead of logging: mid-eval errors otherwise go straight to
        // the error log (see `ScriptVm::take_errors`) and can't be returned.
        vm.bx.captured_errors = Some(Vec::new());
        let value = vm.with_instruction_limit(SPLASH_EVAL_INSTRUCTION_LIMIT, |vm| {
            vm.eval_with_append_source(script_mod, &code, NIL.into())
        });
        let mut errors = vm.take_error_records();
        if errors.is_empty() {
            if value.is_err() {
                errors.push(ScriptErrorRecord::from("script evaluated to an error".to_string()));
            } else if value.as_object().is_none() {
                errors.push(ScriptErrorRecord::from(
                    "script has no root widget (e.g. View{...}) as its final expression"
                        .to_string(),
                ));
            }
        }
        errors
    });
    });
    crate::widget_async::mark_splash_isolate_dead(vm_id);
    // Reclaim NOW (stops the isolate's top-level timers and drops its sandbox
    // root binding) so nothing can re-create the scratch dir after we remove
    // it; then delete last, and it stays deleted.
    crate::widget_async::gc_dead_splash_isolates(cx);
    let _ = std::fs::remove_dir_all(&scratch);
    errors_out
}

impl WidgetNode for Splash {
    fn widget_uid(&self) -> WidgetUid {
        self.uid
    }

    fn walk(&mut self, cx: &mut Cx) -> Walk {
        self.view.walk(cx)
    }

    fn area(&self) -> Area {
        self.view.area()
    }

    fn redraw(&mut self, cx: &mut Cx) {
        self.view.redraw(cx);
    }

    fn children(&self, visit: &mut dyn FnMut(LiveId, WidgetRef)) {
        self.view.children(visit);
    }
}

impl Drop for Splash {
    fn drop(&mut self) {
        // A Splash owns an isolate script VM. `Drop` has no `Cx`, so it can't free
        // the VM here; it just marks the id for reclamation. The isolate is torn
        // down later by `gc_dead_splash_isolates` (on the next isolate alloc, async
        // pump, or Splash event) while a `Cx` is available and nothing runs in it.
        crate::widget_async::mark_splash_isolate_dead(self.vm_id);
    }
}

impl Widget for Splash {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if self.allow_net {
            if let Event::NetworkResponses(responses) = event {
                crate::widget_async::handle_splash_network_responses(cx, self.vm_id, responses);
            }
        }
        // 1 Hz tick for `fn tick()` bodies: the script updates its widgets in
        // place (ui.<id>.set_*), which schedules no paint of its own.
        if self.tick_timer.is_event(event).is_some() {
            self.call_script_fn(cx, id!(tick), &[]);
            self.view.redraw(cx);
        }
        // Per-frame pump (see `anim_next_frame`): re-evaluate the body once per
        // data-fetch epoch change (live values replace the "—" placeholders),
        // once per whole sim-clock second for `sys.simsecs` bodies, and repaint
        // per frame only for genuinely time-based shaders.
        if self.animating && self.anim_next_frame.is_event(event).is_some() {
            let epoch = cx.script_data_fetch_epoch();
            let sim_tick = sim_clock_secs() as u64;
            let (sim_due, is_tick_card, needs_frame_anim, binds_live) = {
                let b = self.body.as_ref();
                (
                    sim_tick != self.last_sim_tick && b.contains("sys.simsecs"),
                    b.contains("fn tick(") || b.contains("fn tick ("),
                    b.contains("draw_pass.time") || b.contains("WeatherIcon"),
                    body_binds_live_data(b),
                )
            };
            if (epoch != self.last_data_epoch && binds_live && !is_tick_card) || sim_due {
                self.last_sim_tick = sim_tick;
                log!(
                    "[SPLASH] live re-eval: epoch {} -> {} sim_due={} ({} bytes)",
                    self.last_data_epoch, epoch, sim_due, self.body.as_ref().len()
                );
                // Full re-evaluation into a fresh view (what the fork did): the
                // body's `sys.*` calls now read the fetch cache instead of
                // returning placeholders.
                self.eval_styled_body(cx, false);
                cx.redraw_all();
            } else if needs_frame_anim {
                self.view.redraw(cx);
            }
            self.anim_next_frame = cx.new_next_frame();
        }
        self.view.handle_event(cx, event, scope);
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        if self.style_pending && self.vm_id!=MAIN_SPLASH_VM_ID {self.eval_styled_body(cx,true);}
        //let tree = self.view.widget_tree();
        //cx.with_vm(|vm| {
        //    log!("{}", tree.display(vm.heap()));
        //});
        self.view.draw_walk(cx, scope, walk)
    }

    fn text(&self) -> String {
        self.body.as_ref().to_string()
    }

    fn set_text(&mut self, cx: &mut Cx, v: &str) {
        if self.body.as_ref() != v {
            self.body.set(v);
            // Empty body = tear down the app: reclaim its isolate (stopping its
            // timers and dropping its storage jail) rather than leaving it
            // running behind a blank view. A reused Splash (e.g. a live-preview
            // widget) that goes back to empty must not leak its old isolate.
            if v.is_empty() {
                self.stop(cx);
            } else {
                self.eval_body(cx);
            }
            self.redraw(cx);
        }
    }
}

impl Splash {
    /// Calls a top-level `fn` defined in this Splash's script body, if the script
    /// defines one by that name. Runs inside the isolate under the standard entry
    /// budget and instruction limit. Returns whether the fn was found: hosts
    /// broadcasting optional hooks (e.g. size changes) can ignore it, while
    /// callers expecting the fn to exist can log a missing-hook diagnostic
    /// (distinguishing "script has no hook" from a typo'd name).
    pub fn call_script_fn(&mut self, cx: &mut Cx, name: LiveId, args: &[ScriptValue]) -> bool {
        let Some(scope) = self.body_scope(cx) else {
            return false;
        };
        let mut called = false;
        crate::widget_async::contain_isolate_panic("script hook call", || {
        called = cx.with_script_vm_id(self.vm_id, |vm| {
            let heap_key = vm.bx.heap.heap_key();
            if !crate::splash_policy::may_run(heap_key) {
                return false;
            }
            // NoTrap: this is an existence probe for an OPTIONAL hook. A
            // trapping lookup queues a NotFound into the error log even though
            // the miss is handled right here — every host broadcast (e.g.
            // on_app_resize) then spams "variable <raw id> not found" for
            // every script that simply doesn't define the hook.
            let fnval = vm.bx.heap.scope_value(scope, name, NoTrap);
            if fnval.is_nil() || fnval.is_err() {
                return false;
            }
            vm.with_instruction_limit(WIDGET_SCRIPT_INSTRUCTION_LIMIT, |vm| {
                vm.call(fnval, args);
            });
            crate::splash_policy::charge(heap_key, vm.last_limit_consumed() as u64);
            true
        });
        });
        called
    }

    /// Like [`Self::call_script_fn`], but with string arguments — those are
    /// heap values, so they must be minted inside this isolate's own heap
    /// right before the call (a cross-heap ScriptValue would resolve in the
    /// wrong arena). Used for host->script deliveries like IPC messages.
    pub fn call_script_fn_with_strings(&mut self, cx: &mut Cx, name: LiveId, args: &[&str]) -> bool {
        let Some(scope) = self.body_scope(cx) else {
            return false;
        };
        let mut called = false;
        crate::widget_async::contain_isolate_panic("script hook call", || {
        called = cx.with_script_vm_id(self.vm_id, |vm| {
            let heap_key = vm.bx.heap.heap_key();
            if !crate::splash_policy::may_run(heap_key) {
                return false;
            }
            let fnval = vm.bx.heap.scope_value(scope, name, NoTrap);
            if fnval.is_nil() || fnval.is_err() {
                return false;
            }
            let vals: Vec<ScriptValue> = args
                .iter()
                .map(|s| vm.bx.heap.new_string_from_str(s))
                .collect();
            vm.with_instruction_limit(WIDGET_SCRIPT_INSTRUCTION_LIMIT, |vm| {
                vm.call(fnval, &vals);
            });
            crate::splash_policy::charge(heap_key, vm.last_limit_consumed() as u64);
            true
        });
        });
        called
    }

    /// Sets whether this Splash's isolate gets the networking runtime. Must be
    /// called before the body is first evaluated (i.e. before `set_text`) — the
    /// VM is allocated with or without network on that first eval and isn't
    /// re-allocated afterwards.
    pub fn set_allow_net(&mut self, allow: bool) {
        self.allow_net = allow;
    }

    /// Assigns this app's private storage directory — the root its jailed
    /// `fs` module resolves against. Takes effect immediately when the
    /// isolate is live, and on the next eval otherwise; call BEFORE set_text
    /// so top-level `fs.read` boot loads see it.
    pub fn set_sandbox_dir(&mut self, cx: &mut Cx, dir: Option<std::path::PathBuf>) {
        self.sandbox_dir = dir.clone();
        if self.vm_id != MAIN_SPLASH_VM_ID {
            let heap_key = cx.with_script_vm_id(self.vm_id, |vm| vm.bx.heap.heap_key());
            crate::splash_storage::set_root_for_heap(heap_key, dir);
        }
    }

    /// Assigns the host-trusted identity for this app's `host.request` calls.
    /// Call BEFORE set_text so boot-time requests already carry it; takes
    /// effect immediately when the isolate is live.
    pub fn set_host_tag(&mut self, cx: &mut Cx, tag: Option<String>) {
        self.host_tag = tag.clone();
        if self.vm_id != MAIN_SPLASH_VM_ID {
            let heap_key = cx.with_script_vm_id(self.vm_id, |vm| vm.bx.heap.heap_key());
            crate::splash_host::set_tag_for_heap(heap_key, tag);
        }
    }

    /// Replaces the capability list `host.capabilities()` reports to this
    /// app. Informational (the host still decides every request); push it
    /// again whenever grants change so the script's UI can adapt.
    pub fn set_host_caps(&mut self, cx: &mut Cx, caps: Vec<String>) {
        self.host_caps = caps.clone();
        if self.vm_id != MAIN_SPLASH_VM_ID {
            let heap_key = cx.with_script_vm_id(self.vm_id, |vm| vm.bx.heap.heap_key());
            crate::splash_host::set_caps_for_heap(heap_key, caps);
        }
    }

    /// Raises this app's whole-jail storage cap (None = the default). Takes
    /// effect immediately; a lowered cap refuses further growth rather than
    /// deleting anything the app already wrote.
    pub fn set_storage_quota(&mut self, cx: &mut Cx, total_bytes: Option<u64>) {
        self.storage_quota = total_bytes;
        if self.vm_id != MAIN_SPLASH_VM_ID {
            let heap_key = cx.with_script_vm_id(self.vm_id, |vm| vm.bx.heap.heap_key());
            crate::splash_storage::set_quota_for_heap(heap_key, total_bytes);
        }
    }

    /// Marks whether this isolate's surface may raise user prompts; see
    /// `SplashHostRequest::may_prompt`. Defaults true.
    pub fn set_host_prompts(&mut self, cx: &mut Cx, may_prompt: bool) {
        self.host_prompts = may_prompt;
        if self.vm_id != MAIN_SPLASH_VM_ID {
            let heap_key = cx.with_script_vm_id(self.vm_id, |vm| vm.bx.heap.heap_key());
            crate::splash_host::set_prompts_for_heap(heap_key, may_prompt);
        }
    }

    /// Puts this isolate under an ENFORCED policy (ADR 0002): files stay in
    /// its jail, and the runtime's own reads of the person's data and devices
    /// need their grants. `hosts`, `instruction_budget` and the capabilities
    /// are what the app declares: since the ruling of 8 October 2026 none of
    /// them limits its network paths, its `host.request` calls or what it
    /// runs ([`crate::splash_policy`]). Call before set_text. Pass `None`
    /// hosts to return to the unenforced behaviour.
    pub fn set_policy(&mut self, cx: &mut Cx, hosts: Option<Vec<String>>, instruction_budget: Option<u64>) {
        self.policy_hosts = hosts;
        self.instruction_budget = instruction_budget;
        if self.vm_id != MAIN_SPLASH_VM_ID {
            let heap_key = cx.with_script_vm_id(self.vm_id, |vm| vm.bx.heap.heap_key());
            self.apply_policy(cx, heap_key);
        }
    }

    /// Caps this isolate's script heap. Applied when the isolate exists, and
    /// on the next eval otherwise. None removes the cap.
    pub fn set_memory_bytes(&mut self, cx: &mut Cx, bytes: Option<usize>) {
        self.memory_bytes = bytes;
        if self.vm_id != MAIN_SPLASH_VM_ID {
            cx.with_script_vm_id(self.vm_id, |vm| vm.bx.heap.set_max_heap_bytes(bytes));
        }
    }

    /// Instructions this isolate has run under its budget so far.
    pub fn instructions_used(&mut self, cx: &mut Cx) -> u64 {
        if self.vm_id == MAIN_SPLASH_VM_ID {
            return 0;
        }
        let heap_key = cx.with_script_vm_id(self.vm_id, |vm| vm.bx.heap.heap_key());
        crate::splash_policy::instructions_used(heap_key)
    }

    /// Push the policy fields onto the live isolate.
    fn apply_policy(&mut self, cx: &mut Cx, heap_key: usize) {
        if let Some(hosts) = &self.policy_hosts {
            crate::splash_policy::set_policy_for_heap(heap_key, self.host_caps.clone(), hosts.clone(), self.instruction_budget);
        }
        if let Some(bytes) = self.memory_bytes {
            cx.with_script_vm_id(self.vm_id, |vm| vm.bx.heap.set_max_heap_bytes(Some(bytes)));
        }
    }

    /// This Splash's live isolate heap identity (None while stopped) — the
    /// same key `SplashHostRequest::heap_key` carries, so hosts can relate a
    /// request to a specific widget (e.g. to skip an IPC sender's own isolate
    /// when fanning a message out).
    pub fn isolate_heap_key(&mut self, cx: &mut Cx) -> Option<usize> {
        if self.vm_id == MAIN_SPLASH_VM_ID {
            return None;
        }
        Some(cx.with_script_vm_id(self.vm_id, |vm| vm.bx.heap.heap_key()))
    }

    /// Sets (or replaces) a global visible to this Splash's script, like the
    /// injected `ui` handle. Useful for handing scripts host-provided context
    /// (configuration, sizes, capabilities) without re-evaluating the body.
    pub fn set_script_global(&mut self, cx: &mut Cx, key: LiveId, value: ScriptValue) {
        if self.vm_id == MAIN_SPLASH_VM_ID {
            return;
        }
        cx.with_script_vm_id(self.vm_id, |vm| {
            vm.set_injected_global(key, value);
        });
    }

    /// The scope object holding this Splash body's top-level definitions, via
    /// the body id cached at eval time (with a pointer-identity fallback for
    /// robustness).
    /// The scope the script's top-level names live in: the one its body
    /// ended in, since a `let`/`fn` that shadows a prelude name opens a
    /// child scope the module scope cannot see into.
    fn body_scope(&mut self, cx: &mut Cx) -> Option<ScriptObject> {
        if self.vm_id == MAIN_SPLASH_VM_ID {
            return None;
        }
        let body_key = self.body_key();
        let body_id = self.body_id;
        cx.with_script_vm_id(self.vm_id, |vm| {
            let bodies = vm.bx.code.bodies.borrow();
            let ended_in = |body: &ScriptBody| {
                body.end_scope.as_ref().unwrap_or(&body.scope).as_object()
            };
            if let Some(body) = body_id.and_then(|i| bodies.get(i as usize)) {
                return Some(ended_in(body));
            }
            bodies.iter().find_map(|body| match &body.source {
                ScriptSource::Mod(m) if m.module_path == body_key => Some(ended_in(body)),
                _ => None,
            })
        })
    }
}

impl SplashRef {
    /// See [`Splash::set_policy`].
    pub fn set_policy(&self, cx: &mut Cx, hosts: Option<Vec<String>>, instruction_budget: Option<u64>) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.set_policy(cx, hosts, instruction_budget);
        }
    }

    /// See [`Splash::set_memory_bytes`].
    pub fn set_memory_bytes(&self, cx: &mut Cx, bytes: Option<usize>) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.set_memory_bytes(cx, bytes);
        }
    }

    /// See [`Splash::instructions_used`].
    pub fn instructions_used(&self, cx: &mut Cx) -> u64 {
        self.borrow_mut().map(|mut inner| inner.instructions_used(cx)).unwrap_or(0)
    }

    pub fn set_text(&self, cx: &mut Cx, v: &str) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.set_text(cx, v);
        }
    }

    /// Reconcile a host-generated UI into its existing named widgets. This
    /// keeps focus, selection and scroll state while declarative data changes.
    /// Use set_text("") to tear down the instance and revoke its callbacks.
    pub fn reapply_text(&self, cx: &mut Cx, body: &str) {
        if let Some(mut inner) = self.borrow_mut() {
            if inner.vm_id == MAIN_SPLASH_VM_ID || body.is_empty() {
                inner.set_text(cx, body);
            } else if inner.body.as_ref() != body {
                inner.body.set(body);
                // This is new declarative source, not a stylesheet reapply.
                // Reload updates label data while reconciling named widgets;
                // their Rust focus/selection/scroll state remains in place.
                inner.eval_styled_body_with_apply(cx, true, &Apply::Reload);
                inner.redraw(cx);
            }
        }
    }

    /// See [`Splash::set_sandbox_dir`].
    pub fn set_sandbox_dir(&self, cx: &mut Cx, dir: Option<std::path::PathBuf>) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.set_sandbox_dir(cx, dir);
        }
    }

    /// See [`Splash::set_host_tag`].
    pub fn set_host_tag(&self, cx: &mut Cx, tag: Option<String>) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.set_host_tag(cx, tag);
        }
    }

    /// See [`Splash::set_host_caps`].
    pub fn set_host_caps(&self, cx: &mut Cx, caps: Vec<String>) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.set_host_caps(cx, caps);
        }
    }

    /// See [`Splash::set_storage_quota`].
    pub fn set_storage_quota(&self, cx: &mut Cx, total_bytes: Option<u64>) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.set_storage_quota(cx, total_bytes);
        }
    }

    /// See [`Splash::set_host_prompts`].
    pub fn set_host_prompts(&self, cx: &mut Cx, may_prompt: bool) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.set_host_prompts(cx, may_prompt);
        }
    }

    /// See [`Splash::isolate_heap_key`].
    pub fn isolate_heap_key(&self, cx: &mut Cx) -> Option<usize> {
        if let Some(mut inner) = self.borrow_mut() {
            inner.isolate_heap_key(cx)
        } else {
            None
        }
    }

    /// See [`Splash::call_script_fn`].
    pub fn call_script_fn(&self, cx: &mut Cx, name: LiveId, args: &[ScriptValue]) -> bool {
        if let Some(mut inner) = self.borrow_mut() {
            inner.call_script_fn(cx, name, args)
        } else {
            false
        }
    }

    /// See [`Splash::call_script_fn_with_strings`].
    pub fn call_script_fn_with_strings(&self, cx: &mut Cx, name: LiveId, args: &[&str]) -> bool {
        if let Some(mut inner) = self.borrow_mut() {
            inner.call_script_fn_with_strings(cx, name, args)
        } else {
            false
        }
    }

    /// See [`Splash::set_script_global`].
    pub fn set_script_global(&self, cx: &mut Cx, key: LiveId, value: ScriptValue) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.set_script_global(cx, key, value);
        }
    }
}

#[cfg(test)]
mod style_tests {
    use super::*;
    use crate::desktop_style::{self, DesktopStyle, StyleSheet};
    #[test]
    fn embedded_splash_restyles_its_isolate_without_replacing_edits() {
        let mut cx=Cx::new(Box::new(|_,_|{}));
        let mut splash=cx.with_vm(|vm| {
            crate::script_mod(vm);
            let value=vm.eval(makepad_script::script! {use mod.widgets.* Splash{}});
            Splash::script_from_value(vm,value)
        });
        splash.set_text(&mut cx,"field := TextInput{text: \"original\"}");
        let field=splash.view.children.iter().find(|(id,_)|*id==id!(field)).unwrap().1.clone();
        let uid=field.widget_uid();
        field.clone().set_text(&mut cx,"edited document");
        cx.with_vm(|vm| {
            desktop_style::install(vm,StyleSheet::load_with_appearance(DesktopStyle::Macos,true));
            let source=splash.script_source();
            splash.script_apply(vm,&Apply::ScriptReapply,&mut Scope::empty(),source.into());
        });
        splash.eval_styled_body(&mut cx,true);
        let field=splash.view.children.iter().find(|(id,_)|*id==id!(field)).unwrap().1.clone();
        assert_eq!(field.widget_uid(),uid);
        assert_eq!(field.text(),"edited document");
        cx.with_script_vm_id(splash.vm_id,|vm| {
            let theme=vm.module(id!(theme));
            assert_eq!(vm.bx.heap.value(theme,id!(color_bg_app).into(),NoTrap).as_color(),Some(0x28282aff));
        });
        splash.stop(&mut cx);
    }

    fn new_splash(cx: &mut Cx) -> Splash {
        cx.with_vm(|vm| {
            crate::script_mod(vm);
            let value = vm.eval(makepad_script::script! {use mod.widgets.* Splash{}});
            Splash::script_from_value(vm, value)
        })
    }

    /// What a host does on a style change: install the sheet, reapply the
    /// Splash (which notices the new sheet), then let it restyle its isolate.
    fn restyle(cx: &mut Cx, splash: &mut Splash, style: DesktopStyle, dark: bool) {
        cx.with_vm(|vm| {
            desktop_style::install(vm, StyleSheet::load_with_appearance(style, dark));
            let source = splash.script_source();
            splash.script_apply(vm, &Apply::ScriptReapply, &mut Scope::empty(), source.into());
        });
        splash.eval_styled_body(cx, true);
    }

    #[test]
    fn style_reapply_does_not_stack_startup_timers() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let mut splash = new_splash(&mut cx);
        splash.set_text(
            &mut cx,
            "mod.std.start_interval(60.0, || {})\nlabel := Label{text: \"tick\"}",
        );
        let heap = splash.isolate_heap_key(&mut cx).unwrap();
        let timers = |cx: &Cx| {
            cx.script_data
                .timers
                .timers
                .iter()
                .filter(|t| t.callback.heap_key() == heap)
                .count()
        };
        assert_eq!(timers(&cx), 1);
        assert_eq!(splash.startup_timers.len(), 1);
        restyle(&mut cx, &mut splash, DesktopStyle::Macos, true);
        assert_eq!(
            timers(&cx),
            1,
            "a style change re-arms the body's interval instead of stacking a second one"
        );
        restyle(&mut cx, &mut splash, DesktopStyle::Windows, false);
        assert_eq!(timers(&cx), 1);
        assert_eq!(splash.startup_timers.len(), 1);
        splash.stop(&mut cx);
        assert_eq!(timers(&cx), 0);
    }

    #[test]
    fn style_reapply_keeps_body_module_state() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let mut splash = new_splash(&mut cx);
        splash.set_text(
            &mut cx,
            "mod.state = {count: 0}\nfn bump() { mod.state.count += 1 }\nlabel := Label{text: \"x\"}",
        );
        let count = |cx: &mut Cx, splash: &mut Splash| {
            cx.with_script_vm_id(splash.vm_id, |vm| {
                let state = vm.module(id!(state));
                vm.bx.heap.value(state, id!(count).into(), NoTrap).as_f64()
            })
        };
        assert!(splash.call_script_fn(&mut cx, id!(bump), &[]));
        assert_eq!(count(&mut cx, &mut splash), Some(1.0));
        assert!(splash.body_modules.contains(&id!(state)), "{:?}", splash.body_modules);
        restyle(&mut cx, &mut splash, DesktopStyle::Macos, true);
        assert_eq!(
            count(&mut cx, &mut splash),
            Some(1.0),
            "the body's `mod.state` must survive a style change, not restart at its literal"
        );
        // The re-run's redefined `fn` works against the restored state.
        assert!(splash.call_script_fn(&mut cx, id!(bump), &[]));
        assert_eq!(count(&mut cx, &mut splash), Some(2.0));
        splash.stop(&mut cx);
    }
}

// ===========================================================================
// The sys.* engine: the agent module, every sys.* binding, and the helpers
// they share. Carried from the octos-one-framework lineage.
// ===========================================================================

#[derive(Clone, Debug, Default)]
pub enum SplashAction {
    Notify {
        event_id: String,
        payload: String,
    },
    #[default]
    None,
}

/// Web-Mercator "slippy map" tile (x, y) covering `lat`/`lon` at zoom `z`
/// (the standard OSM/XYZ scheme used by Carto and WAQI tile servers).
fn slippy_tile(lat: f64, lon: f64, z: u32) -> (i64, i64) {
    let n = (1u64 << z) as f64;
    let x = ((lon + 180.0) / 360.0 * n).floor();
    let y = ((1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * n).floor();
    let max = (1i64 << z) - 1;
    ((x as i64).clamp(0, max), (y as i64).clamp(0, max))
}

/// Fractional Web-Mercator tile coords plus the top-left tile of the 2x2
/// block that best CENTERS (lat, lon): the point's own tile joined with its
/// nearest neighbor in each axis, so the anchor always lands in the middle
/// half of the mosaic (offset fraction in [0.25, 0.75) per axis). Returns
/// (left_tile_x, top_tile_y, x_fraction_of_mosaic, y_fraction_of_mosaic).
/// Backs `sys.maptile` (URLs) and `sys.mappin` (pin offset) — the two MUST
/// agree, which is why the math lives in one place.
fn slippy_mosaic(lat: f64, lon: f64, z: u32) -> (i64, i64, f64, f64) {
    let n = (1u64 << z) as f64;
    let xf = (lon + 180.0) / 360.0 * n;
    let yf = (1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * n;
    let left = if xf.fract() >= 0.5 { xf.floor() } else { xf.floor() - 1.0 };
    let top = if yf.fract() >= 0.5 { yf.floor() } else { yf.floor() - 1.0 };
    let max = (1i64 << z) - 1;
    // Longitude WRAPS at the antimeridian: keep `left` raw (it may be -1 so
    // the anchor stays in the mosaic's middle band — a Fiji-class epicenter
    // used to get clamped and land at the pane's edge) and wrap the actual
    // tile x with rem_euclid when forming URLs. Latitude cannot wrap, so `top`
    // stays clamped.
    let left = left as i64;
    let top = (top as i64).clamp(0, (max - 1).max(0));
    ((left), (top), (xf - left as f64) / 2.0, (yf - top as f64) / 2.0)
}

/// Yesterday's civil date (UTC) as `YYYY-MM-DD` — the most recent day for
/// which NASA GIBS daily global mosaics are guaranteed complete. Days-to-date
/// via Howard Hinnant's `civil_from_days` (no chrono dep in this crate).
fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Howard Hinnant's `civil_from_days`: days-since-Unix-epoch → (year, month, day).
/// `pub(crate)` so the StockPlot widget can reuse it for date tick labels.
pub(crate) fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn gibs_latest_date() -> String {
    let (y, m, d) = civil_from_days((now_unix_secs() / 86_400) as i64 - 1); // yesterday
    format!("{y:04}-{m:02}-{d:02}")
}

/// A GIBS `TIME=` string for `minutes_ago` in the past, floored to a 10-minute
/// boundary (the geostationary AHI/ABI granule cadence). Format
/// `YYYY-MM-DDTHH:MM:00Z`.
fn gibs_datetime(minutes_ago: i64) -> String {
    let target = (now_unix_secs() as i64 - minutes_ago * 60).max(0);
    let target = (target / 600) * 600; // floor to 10 min
    let (y, m, d) = civil_from_days(target / 86_400);
    let sod = target % 86_400;
    let (hh, mm) = (sod / 3600, (sod % 3600) / 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:00Z")
}

#[path = "splash_news.rs"]
mod news_pipeline;
#[path = "splash_dataset.rs"]
mod composition_dataset;

pub fn register_agent_module(vm: &mut ScriptVm) {
    let agent = vm.new_module(id!(agent));
    vm.add_method(
        agent,
        id_lut!(notify),
        script_args_def!(event = NIL, payload = NIL),
        |vm, args| {
            let event_value = script_value!(vm, args.event);
            let payload_value = script_value!(vm, args.payload);

            let mut event_id = String::new();
            vm.bx.heap.cast_to_string(event_value, &mut event_id);

            let mut payload = String::new();
            vm.bx.heap.to_json_inner(payload_value, &mut payload);

            // A notify carries no sender, so the host acts on it as if its own
            // card sent it. An app under a policy may only post one when it
            // was granted `agent`; anything else would let an installed app
            // drive the host's agent.
            if let Err(reason) = crate::splash_policy::service_allowed(vm.bx.heap.heap_key(), "agent.notify") {
                log!("splash: refused agent.notify {event_id:?}: {reason}");
                return NIL;
            }
            Cx::post_action(SplashAction::Notify { event_id, payload });
            NIL
        },
    );
    vm.set_injected_global(id!(agent), agent.into());

    // `sys` module — Rust helpers callable from generated splash code.
    // Extend this with more methods (data fetchers, formatters, card builders)
    // and teach the LLM to call them in the A2App prompt.
    let sys = vm.new_module(id!(sys));
    crate::splash_l0::install(vm, sys);
    splash_dataset::install(vm, sys);
    splash_news::install(vm, sys);

    // sys.photo("tokyo skyline sunset") -> a full-screen 9:16 image URL for that
    // subject (pollinations.ai renders the prompt with an AI model, so the photo
    // always matches the subject). Centralises image sourcing in Rust so it can be
    // improved (curation, quality, providers) without touching the prompt.
    // Use as `Image{ src: http_resource(sys.photo("<q>")) }`.
    vm.add_method(
        sys,
        id_lut!(photo),
        script_args_def!(query = NIL, cond = NIL),
        |vm, args| {
            let query_value = script_value!(vm, args.query);
            let mut query = String::new();
            vm.bx.heap.cast_to_string(query_value, &mut query);
            // THE DAY, folded in. A card reading 92 % rain under a sunlit
            // backdrop is the page disagreeing with its own tiles, and the card
            // cannot fix that itself: the condition is a live fact, so it has to
            // arrive here rather than be written into the prompt by the model.
            //
            // Appended, never substituted — the SCENE is the card's words and
            // this only says what the sky is doing over them. A placeholder is
            // dropped rather than described: "—" is not weather.
            let cond_value = script_value!(vm, args.cond);
            let mut cond = String::new();
            vm.bx.heap.cast_to_string(cond_value, &mut cond);
            let cond = cond.trim();
            if !cond.is_empty()
                && cond != "\u{2014}"
                && cond != "n/a"
                && !cond.starts_with("$[")
                && !cond.eq_ignore_ascii_case("nil")
            {
                if !query.trim().is_empty() {
                    query.push_str(", ");
                }
                query.push_str(&cond.to_lowercase());
                query.push_str(" weather");
            }

            // AI-generated, always ON-TOPIC 9:16 portrait image. loremflickr
            // OR-matches comma tags, so a multi-word subject ("paris eiffel
            // tower sunny") returned unrelated photos (a cat statue). Pollinations
            // renders the full natural-language prompt, so the photo always
            // matches the subject and is high quality — the "nano banana"-style
            // AI source the app wants for beautiful full-screen backgrounds.
            let q = query.trim();
            // A SUBJECT THAT HAS NOT ARRIVED IS NOT A SUBJECT. `sys.photo(query:
            // place.name)` is resolved at draw time, and while the geocode is in
            // flight the name is the placeholder — so the card asked flux to
            // generate an image of "—", waited for it, and then asked again for
            // the real place once the name landed. Two generations per card, and
            // the first is a picture of nothing.
            //
            // No URL is the honest answer: the image is simply absent until the
            // subject exists, and the next redraw asks once, for the place.
            // Distinct from the EMPTY case below — an empty query is a card
            // deliberately asking for any handsome backdrop, and it gets one.
            if matches!(q, "—" | "n/a") || q.starts_with("$[") {
                return vm.bx.heap.new_string_from_str("");
            }
            let q = if q.is_empty() {
                "beautiful cinematic landscape scenery, golden hour"
            } else {
                q
            };
            // Percent-encode the prompt for a URL path segment (RFC 3986):
            // keep unreserved chars, encode everything else (incl. spaces) by
            // UTF-8 byte.
            let mut enc = String::with_capacity(q.len() * 3);
            let mut buf = [0u8; 4];
            for ch in q.chars() {
                if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~') {
                    enc.push(ch);
                } else {
                    for b in ch.encode_utf8(&mut buf).as_bytes() {
                        enc.push('%');
                        enc.push(char::from_digit((b >> 4) as u32, 16).unwrap().to_ascii_uppercase());
                        enc.push(char::from_digit((b & 0xF) as u32, 16).unwrap().to_ascii_uppercase());
                    }
                }
            }
            // 1080x1920 = 9:16. nologo strips the watermark; model=flux is fast
            // and photoreal.
            let url = format!(
                "https://image.pollinations.ai/prompt/{enc}?width=1080&height=1920&nologo=true&model=flux"
            );
            vm.bx.heap.new_string_from_str(&url)
        },
    );

    // sys.satellite(lat, lon) -> REAL satellite cloud imagery (卫星云图) for the city's
    // region: NASA GIBS WMS, MODIS Terra true-color corrected reflectance for yesterday
    // (UTC) — the most recent complete daily global mosaic. Actual clouds over actual
    // terrain, daylit everywhere, keyless, and a single GetMap call returns an
    // arbitrary-size image so a full-width pane needs no tile stitching. 2:1 aspect
    // (880x440 over a ~14°x7° box) — pair with `fit: ImageFit.CropToFill` in a wide pane.
    // Use as `Image{ src: http_resource(sys.satellite(LAT, LON)) fit: ImageFit.CropToFill }`.
    vm.add_method(
        sys,
        id_lut!(satellite),
        script_args_def!(lat = NIL, lon = NIL, zoom = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat).as_number().unwrap_or(35.68);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(139.65);
            // Optional slippy-style zoom (default 8 = the historical 14°-wide
            // frame; each +1 halves the span): z10 ≈ a metro close-up, z6 ≈ a
            // continental view. Omitting the arg keeps old cards identical.
            let z = script_value!(vm, args.zoom)
                .as_number()
                .unwrap_or(8.0)
                .clamp(3.0, 12.0);
            let half_lon = 7.0 * f64::powi(2.0, 8 - z as i32);
            let half_lat = half_lon / 2.0;
            // Keep the box inside the poles; wrap longitude edges.
            let lat = lat.clamp(-78.0, 78.0);
            let (min_lon, max_lon) = ((lon - half_lon).max(-180.0), (lon + half_lon).min(180.0));
            let (min_lat, max_lat) = (lat - half_lat, lat + half_lat);
            let date = gibs_latest_date();
            let url = format!(
                "https://gibs.earthdata.nasa.gov/wms/epsg4326/best/wms.cgi?SERVICE=WMS&VERSION=1.1.1&REQUEST=GetMap&LAYERS=MODIS_Terra_CorrectedReflectance_TrueColor&SRS=EPSG:4326&BBOX={min_lon},{min_lat},{max_lon},{max_lat}&WIDTH=880&HEIGHT=440&FORMAT=image/jpeg&TIME={date}"
            );
            vm.bx.heap.new_string_from_str(&url)
        },
    );

    // sys.satellite_ir(lat, lon, frames_ago) -> ONE frame of a geostationary
    // cloud-motion loop (卫星云图动画). Clean-IR brightness-temperature (clouds =
    // white on dark), which — unlike the once-daily MODIS still in sys.satellite —
    // updates every 10 min and is available day AND night, so cycling `frames_ago`
    // 0..N via a Splash `fn tick()` + `ui.<img>.set_src(...)` animates real cloud
    // movement. `frames_ago` 0 = newest (a fixed ~80 min latency floor so the
    // granule is published), each +1 steps 10 min further back. Satellite picked by
    // longitude: Himawari (Asia/Pacific) vs GOES-East (Americas/Atlantic); both are
    // GIBS "best", keyless, snap TIME to the nearest granule. Use as
    // `Image{ src: http_resource(sys.satellite_ir(LAT, LON, N)) fit: ImageFit.CropToFill }`.
    vm.add_method(
        sys,
        id_lut!(satellite_ir),
        script_args_def!(lat = NIL, lon = NIL, frames_ago = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat).as_number().unwrap_or(35.68);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(139.65);
            let frames_ago = script_value!(vm, args.frames_ago)
                .as_number()
                .unwrap_or(0.0)
                .clamp(0.0, 24.0) as i64;
            let lat = lat.clamp(-78.0, 78.0);
            let (min_lon, max_lon) = ((lon - 7.0).max(-180.0), (lon + 7.0).min(180.0));
            let (min_lat, max_lat) = (lat - 3.5, lat + 3.5);
            // ~80 min latency floor + 10 min per older frame.
            let dt = gibs_datetime(80 + frames_ago * 10);
            // Himawari sees Asia/Pacific; GOES-East the Americas/Atlantic.
            let layer = if lon >= 60.0 || lon < -140.0 {
                "Himawari_AHI_Band13_Clean_Infrared"
            } else {
                "GOES-East_ABI_Band13_Clean_Infrared"
            };
            let url = format!(
                "https://gibs.earthdata.nasa.gov/wms/epsg4326/best/wms.cgi?SERVICE=WMS&VERSION=1.1.1&REQUEST=GetMap&LAYERS={layer}&SRS=EPSG:4326&BBOX={min_lon},{min_lat},{max_lon},{max_lat}&WIDTH=880&HEIGHT=440&FORMAT=image/png&TIME={dt}"
            );
            vm.bx.heap.new_string_from_str(&url)
        },
    );

    // sys.basemap(lat, lon) -> a warm, LABELLED base-map tile (Carto "Voyager", no key) at
    // the city, meant to sit UNDER sys.airmap in an Overlay so the AQI colours have legible
    // geographic context. `voyager_labels_under` keeps place labels BENEATH the translucent
    // AQI markers so both read clearly. Zoom 8 frames the metro itself (not the whole
    // region — fewer, larger AQI badges on top), and the `@2x` retina tile (512px) stays
    // sharp in a full-width pane.
    vm.add_method(
        sys,
        id_lut!(basemap),
        script_args_def!(lat = NIL, lon = NIL, zoom = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat).as_number().unwrap_or(0.0);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(0.0);
            // Optional zoom (default 8 = the historical metro framing; omitting
            // the arg keeps old cards identical). Zoom by intent: region 6,
            // metro 8, city 10, district 12.
            let z = script_value!(vm, args.zoom)
                .as_number()
                .unwrap_or(8.0)
                .clamp(3.0, 17.0) as u32;
            let (x, y) = slippy_tile(lat, lon, z);
            let url = format!(
                "https://a.basemaps.cartocdn.com/rastertiles/voyager_labels_under/{z}/{x}/{y}@2x.png"
            );
            vm.bx.heap.new_string_from_str(&url)
        },
    );

    // sys.airmap(lat, lon) -> a LIVE air-quality colour overlay tile (WAQI, US-EPA AQI
    // scale). Mostly transparent except where AQI data exists, so stack it OVER
    // sys.basemap(lat, lon) at the SAME lat/lon in an Overlay (both use zoom 8 — one
    // zoom step in from the old 7 quarters the station-marker density, so the badges
    // read as a handful of legible chips instead of an overlapping pile).
    // Use as `View{ flow: Overlay Image{basemap} Image{airmap} }`.
    vm.add_method(
        sys,
        id_lut!(airmap),
        script_args_def!(lat = NIL, lon = NIL, zoom = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat).as_number().unwrap_or(0.0);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(0.0);
            // Optional zoom (default 8 keeps old cards identical). MUST match
            // the zoom of the sys.basemap it stacks over, tile for tile.
            let z = script_value!(vm, args.zoom)
                .as_number()
                .unwrap_or(8.0)
                .clamp(3.0, 17.0) as u32;
            let (x, y) = slippy_tile(lat, lon, z);
            let url = format!("https://tiles.waqi.info/tiles/usepa-aqi/{z}/{x}/{y}.png?token=_");
            vm.bx.heap.new_string_from_str(&url)
        },
    );

    // sys.maptile(lat, lon, zoom, "tl"|"tr"|"bl"|"br") -> one quadrant URL of a
    // CENTERED 2x2 Carto "Voyager" mosaic around (lat, lon). Unlike sys.basemap
    // (single tile, the point lands wherever it falls), the four quadrants are
    // chosen so the anchor sits in the middle half of the mosaic — pair with
    // sys.mappin for the 📍. Layout: two `flow: Right` rows of two square
    // Images inside a square pane; ALL FOUR calls must share the same lat/lon/
    // zoom. Zoom by intent: country 5, city 12, district 14, landmark 16.
    // Keyless Carto raster (fair use) — cards MUST caption the pane
    // "© OpenStreetMap contributors © CARTO". Subdomain rotates per quadrant so
    // the 4 GETs parallelize. Pure URL builder (feed to http_resource).
    vm.add_method(
        sys,
        id_lut!(maptile),
        script_args_def!(lat = NIL, lon = NIL, zoom = NIL, quad = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat)
                .as_number()
                .unwrap_or(0.0)
                .clamp(-85.0, 85.0);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(0.0);
            let z = script_value!(vm, args.zoom)
                .as_number()
                .unwrap_or(12.0)
                .clamp(3.0, 17.0) as u32;
            let quad_v = script_value!(vm, args.quad);
            let mut quad = String::new();
            vm.bx.heap.cast_to_string(quad_v, &mut quad);
            let (left, top, _, _) = slippy_mosaic(lat, lon, z);
            let (dx, dy, sub) = match quad.trim().to_ascii_lowercase().as_str() {
                "tr" => (1, 0, "b"),
                "bl" => (0, 1, "c"),
                "br" => (1, 1, "d"),
                _ => (0, 0, "a"), // "tl" and anything unrecognized
            };
            // Wrap tile x across the antimeridian (left may be -1 or n-1+1).
            let n = 1i64 << z;
            let url = format!(
                "https://{sub}.basemaps.cartocdn.com/rastertiles/voyager/{z}/{}/{}@2x.png",
                (left + dx).rem_euclid(n),
                top + dy
            );
            vm.bx.heap.new_string_from_str(&url)
        },
    );

    // sys.mappin(lat, lon, zoom, "x"|"y", size) -> the pin's offset IN DP from
    // the mosaic pane's top-left, for a SQUARE sys.maptile 2x2 pane `size` dp
    // wide. Same math as sys.maptile, so the pin lands exactly on the anchor.
    // Bind into layout like sys.stockbar heights — e.g. an Overlay child:
    //   View{ padding: Inset{ left: sys.mappin(LAT,LON,Z,"x",372) - 11
    //                         top:  sys.mappin(LAT,LON,Z,"y",372) - 22 }
    //         Label{ text: "📍" } }
    // (subtract half the glyph width / full height so the pin TIP marks the spot).
    // Pure math — no fetch, no sentinel.
    vm.add_method(
        sys,
        id_lut!(mappin),
        script_args_def!(lat = NIL, lon = NIL, zoom = NIL, axis = NIL, size = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat)
                .as_number()
                .unwrap_or(0.0)
                .clamp(-85.0, 85.0);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(0.0);
            let z = script_value!(vm, args.zoom)
                .as_number()
                .unwrap_or(12.0)
                .clamp(3.0, 17.0) as u32;
            let axis_v = script_value!(vm, args.axis);
            let mut axis = String::new();
            vm.bx.heap.cast_to_string(axis_v, &mut axis);
            let size = script_value!(vm, args.size).as_number().unwrap_or(372.0);
            let (_, _, fx, fy) = slippy_mosaic(lat, lon, z);
            let f = if axis.trim().eq_ignore_ascii_case("y") { fy } else { fx };
            ScriptValue::from_f64(f * size)
        },
    );

    // sys.geocode(name, "field") -> a LIVE geocoding lookup (open-meteo geocoding
    // API, keyless — same free tier as the forecast API). Resolves a city/town/
    // landmark NAME to facts, so cards never rely on invented coordinates:
    //   sys.geocode("kyoto", "name")    -> "Kyoto"
    //   sys.geocode("kyoto", "country") -> "Japan"
    //   sys.geocode("kyoto", "admin1")  -> "Kyoto"
    //   sys.geocode("kyoto", "lat")     -> "35.0211"   (string, for captions)
    //   sys.geocode("kyoto", "lon")     -> "135.7539"
    //   also: "timezone", "population". Returns "—" while loading. For the
    // NUMBERS that anchor sys.maptile/mappin/places, use sys.geocodenum.
    //
    // An EMPTY name resolves to WHERE THE DEVICE IS (reverse geocode of the
    // last GPS fix — see geocode_url); with no fix it keeps the placeholder.
    // Field translation for both response shapes lives in geocode_pluck.
    vm.add_method(
        sys,
        id_lut!(geocode),
        script_args_def!(name = NIL, field = NIL),
        |vm, args| {
            let name_v = script_value!(vm, args.name);
            let mut name = String::new();
            vm.bx.heap.cast_to_string(name_v, &mut name);
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let url = geocode_url(&name, vm.bx.heap.heap_key());
            let out = match vm.host.cx_mut().script_data_fetch(&url) {
                Some(bytes) => geocode_pluck(&bytes, field.trim())
                    .unwrap_or_else(|| "—".to_string()),
                None => vm.host.cx_mut().script_data_placeholder(&url),
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    // sys.fetch(url) -> the raw response body, or "" while it loads. This is the
    // ONLY Rust in the movie-detail path: a brokered HTTP primitive, nothing
    // movie- or Wikipedia-specific. The URL-building and JSON parsing are done by
    // the Splash data-source script that sys.wiki evaluates below.
    vm.add_method(
        sys,
        id_lut!(fetch),
        script_args_def!(url = NIL),
        |vm, args| {
            let url_v = script_value!(vm, args.url);
            let mut url = String::new();
            vm.bx.heap.cast_to_string(url_v, &mut url);
            let url = url.trim();
            if url.is_empty() {
                return vm.bx.heap.new_string_from_str("");
            }
            let out = match vm.host.cx_mut().script_data_fetch(url) {
                Some(bytes) => String::from_utf8_lossy(&bytes[..]).into_owned(),
                None => String::new(),
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    // sys.geocodenum(name, "lat"|"lon") -> the coordinate as a NUMBER, -9999
    // while the lookup loads / when the place is unknown — the anchor for a map
    // card. Guard the whole card body on it:
    //   let lat = sys.geocodenum("kyoto", "lat")
    //   let lon = sys.geocodenum("kyoto", "lon")
    //   if lat >= -9998 { <the card, using lat/lon everywhere> }
    // Shares sys.geocode's fetch (identical URL -> one request serves both).
    vm.add_method(
        sys,
        id_lut!(geocodenum),
        script_args_def!(name = NIL, field = NIL),
        |vm, args| {
            let name_v = script_value!(vm, args.name);
            let mut name = String::new();
            vm.bx.heap.cast_to_string(name_v, &mut name);
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let url = geocode_url(&name, vm.bx.heap.heap_key());
            let key = if field.trim() == "lon" { "lon" } else { "lat" };
            let n = vm
                .host
                .cx_mut()
                .script_data_fetch(&url)
                .and_then(|bytes| geocode_pluck(&bytes, key))
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(-9999.0);
            ScriptValue::from_f64(n)
        },
    );

    // sys.route(lat1, lon1, lat2, lon2, "field") -> LIVE driving route stats
    // between two points (OSRM public demo server, keyless):
    //   "km"  -> "5.5 km"     "min" -> "10 min"
    //   "distance" -> meters raw    "duration" -> seconds raw
    // Returns "—" while loading (the demo server has no SLA — cards must
    // tolerate the dash). Chain from sys.geocodenum for both endpoints.
    vm.add_method(
        sys,
        id_lut!(route),
        script_args_def!(
            lat1 = NIL,
            lon1 = NIL,
            lat2 = NIL,
            lon2 = NIL,
            field = NIL,
            vias = NIL
        ),
        |vm, args| {
            let lat1 = script_value!(vm, args.lat1).as_number().unwrap_or(0.0);
            let lon1 = script_value!(vm, args.lon1).as_number().unwrap_or(0.0);
            let lat2 = script_value!(vm, args.lat2).as_number().unwrap_or(0.0);
            let lon2 = script_value!(vm, args.lon2).as_number().unwrap_or(0.0);
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let vias_v = script_value!(vm, args.vias);
            let mut vias = String::new();
            vm.bx.heap.cast_to_string(vias_v, &mut vias);
            if !osrm_ready(lat1, lon1, lat2, lon2) || !osrm_vias_ready(&vias) {
                return vm.bx.heap.new_string_from_str("—");
            }
            let url = format!(
                "https://router.project-osrm.org/route/v1/driving/{}?overview=false",
                osrm_coords(lat1, lon1, lat2, lon2, &vias)
            );
            let out = match vm.host.cx_mut().script_data_fetch(&url) {
                Some(bytes) => match field.trim() {
                    "km" => json_pluck(&bytes, "routes.0.distance")
                        .and_then(|s| s.parse::<f64>().ok())
                        .map(|m| format!("{:.1} km", m / 1000.0))
                        .unwrap_or_else(|| "—".to_string()),
                    "min" => json_pluck(&bytes, "routes.0.duration")
                        .and_then(|s| s.parse::<f64>().ok())
                        .map(|s| format!("{:.0} min", s / 60.0))
                        .unwrap_or_else(|| "—".to_string()),
                    "duration" => json_pluck(&bytes, "routes.0.duration")
                        .unwrap_or_else(|| "—".to_string()),
                    _ => json_pluck(&bytes, "routes.0.distance")
                        .unwrap_or_else(|| "—".to_string()),
                },
                None => vm.host.cx_mut().script_data_placeholder(&url),
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    // sys.navroute(lat1, lon1, lat2, lon2, "field") -> LIVE turn-by-turn route
    // data (OSRM, keyless, ONE cached fetch shared with sys.navstep):
    //   "polyline" -> full route geometry (polyline5) — feed MapView.nav_polyline
    //   "km" -> "32.4 km"   "min" -> "33 min"   "" while loading.
    // Pair with MapView{ nav_mode: "3d" nav_polyline: sys.navroute(...) } for a
    // native, on-device Google-style live navigation card.
    vm.add_method(
        sys,
        id_lut!(navroute),
        script_args_def!(
            lat1 = NIL,
            lon1 = NIL,
            lat2 = NIL,
            lon2 = NIL,
            field = NIL,
            vias = NIL
        ),
        |vm, args| {
            let lat1 = script_value!(vm, args.lat1).as_number().unwrap_or(0.0);
            let lon1 = script_value!(vm, args.lon1).as_number().unwrap_or(0.0);
            let lat2 = script_value!(vm, args.lat2).as_number().unwrap_or(0.0);
            let lon2 = script_value!(vm, args.lon2).as_number().unwrap_or(0.0);
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let vias_v = script_value!(vm, args.vias);
            let mut vias = String::new();
            vm.bx.heap.cast_to_string(vias_v, &mut vias);
            if !osrm_ready(lat1, lon1, lat2, lon2) || !osrm_vias_ready(&vias) {
                return vm.bx.heap.new_string_from_str("—");
            }
            let url = navroute_url(lat1, lon1, lat2, lon2, &vias);
            let out = match nav_route_cached(vm, &url) {
                Some(route) => match field.trim() {
                    "polyline" => route.polyline.clone(),
                    "km" => format!("{:.1} km", route.total_m / 1000.0),
                    "min" => format!("{:.0} min", route.total_s / 60.0),
                    // walking (~5 km/h) / cycling (~15 km/h) estimates, formatted
                    "walk" => format!("{:.0} min", (route.total_m / 1000.0) * 12.0),
                    "bike" => format!("{:.0} min", (route.total_m / 1000.0) * 4.0),
                    _ => route.polyline.clone(),
                },
                None => nav_route_placeholder(vm.host.cx_mut(), &url),
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    // sys.navstep(lat1, lon1, lat2, lon2, progress_m, "field") -> the live
    // turn-by-turn banner data at `progress_m` meters into the route (drive
    // progress_m from sys.simsecs: e.g. `let d = sys.simsecs(92) * 15.2`):
    //   "instr" -> "Turn left onto South Market Street"  (the UPCOMING maneuver)
    //   "dist"  -> "850 m" / "1.2 km"  counting-down distance to it
    //   "arrow" -> its glyph ("⬅")   "next_arrow" -> the one after
    //   "lane0".."lane5" -> lane glyphs for the maneuver ("" past the end)
    //   "lane0hot".."lane5hot" -> "1" if that lane is the recommended one
    //   "rem" -> "31.4 km" remaining   "remmin" -> "34" minutes remaining
    // Returns "" while the route loads. Same fetch as sys.navroute.
    vm.add_method(
        sys,
        id_lut!(navstep),
        script_args_def!(
            lat1 = NIL,
            lon1 = NIL,
            lat2 = NIL,
            lon2 = NIL,
            progress = NIL,
            field = NIL,
            vias = NIL
        ),
        |vm, args| {
            let lat1 = script_value!(vm, args.lat1).as_number().unwrap_or(0.0);
            let lon1 = script_value!(vm, args.lon1).as_number().unwrap_or(0.0);
            let lat2 = script_value!(vm, args.lat2).as_number().unwrap_or(0.0);
            let lon2 = script_value!(vm, args.lon2).as_number().unwrap_or(0.0);
            let d = script_value!(vm, args.progress).as_number().unwrap_or(0.0);
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let vias_v = script_value!(vm, args.vias);
            let mut vias = String::new();
            vm.bx.heap.cast_to_string(vias_v, &mut vias);
            if !osrm_ready(lat1, lon1, lat2, lon2) || !osrm_vias_ready(&vias) {
                return vm.bx.heap.new_string_from_str("—");
            }
            let url = navroute_url(lat1, lon1, lat2, lon2, &vias);
            let out = match nav_route_cached(vm, &url) {
                Some(route) => nav_step_field(&route, d, field.trim()),
                None => nav_route_placeholder(vm.host.cx_mut(), &url),
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    #[cfg(feature = "maps")]
    // sys.navprog(lat1, lon1, lat2, lon2, at_lat, at_lon) -> how far along the
    // route the device ACTUALLY is, in meters, by projecting its fix onto the
    // route geometry. Feeds sys.navstep's `progress_m`. 0 while the route loads,
    // and 0 for a fix more than 500 m off the route (a device on another road has
    // not made progress along this one). Shares sys.navroute's cached fetch.
    //
    // This exists so a turn banner advances because the device moved. The nav app
    // fed `progress_m` a clock — `sys.navsecs(period) * 15.2` — which announced
    // turns for a vehicle travelling an assumed 34 mph whether or not anything
    // was moving.
    vm.add_method(
        sys,
        id_lut!(navprog),
        script_args_def!(
            lat1 = NIL,
            lon1 = NIL,
            lat2 = NIL,
            lon2 = NIL,
            at_lat = NIL,
            at_lon = NIL,
            vias = NIL
        ),
        |vm, args| {
            let lat1 = script_value!(vm, args.lat1).as_number().unwrap_or(0.0);
            let lon1 = script_value!(vm, args.lon1).as_number().unwrap_or(0.0);
            let lat2 = script_value!(vm, args.lat2).as_number().unwrap_or(0.0);
            let lon2 = script_value!(vm, args.lon2).as_number().unwrap_or(0.0);
            let at_lat = script_value!(vm, args.at_lat).as_number().unwrap_or(0.0);
            let at_lon = script_value!(vm, args.at_lon).as_number().unwrap_or(0.0);
            let vias_v = script_value!(vm, args.vias);
            let mut vias = String::new();
            vm.bx.heap.cast_to_string(vias_v, &mut vias);
            if !osrm_ready(lat1, lon1, lat2, lon2) || !osrm_vias_ready(&vias) {
                return vm.bx.heap.new_string_from_str("—");
            }
            let url = navroute_url(lat1, lon1, lat2, lon2, &vias);
            let n = match nav_route_cached(vm, &url) {
                Some(route) => nav_progress_m(&route, at_lat, at_lon),
                None => 0.0,
            };
            ScriptValue::from_f64(n)
        },
    );

    // sys.navstepnum(lat1, lon1, lat2, lon2, progress_m, "field") -> NUMBERS for
    // layout binding: "frac" -> trip fraction 0..1 (progress bars). -1 while
    // loading.
    vm.add_method(
        sys,
        id_lut!(navstepnum),
        script_args_def!(
            lat1 = NIL,
            lon1 = NIL,
            lat2 = NIL,
            lon2 = NIL,
            progress = NIL,
            field = NIL,
            vias = NIL
        ),
        |vm, args| {
            let lat1 = script_value!(vm, args.lat1).as_number().unwrap_or(0.0);
            let lon1 = script_value!(vm, args.lon1).as_number().unwrap_or(0.0);
            let lat2 = script_value!(vm, args.lat2).as_number().unwrap_or(0.0);
            let lon2 = script_value!(vm, args.lon2).as_number().unwrap_or(0.0);
            let d = script_value!(vm, args.progress).as_number().unwrap_or(0.0);
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let vias_v = script_value!(vm, args.vias);
            let mut vias = String::new();
            vm.bx.heap.cast_to_string(vias_v, &mut vias);
            if !osrm_ready(lat1, lon1, lat2, lon2) || !osrm_vias_ready(&vias) {
                return vm.bx.heap.new_string_from_str("—");
            }
            let url = navroute_url(lat1, lon1, lat2, lon2, &vias);
            let n = match nav_route_cached(vm, &url) {
                Some(route) => nav_step_num(&route, d, field.trim()),
                None => -1.0,
            };
            ScriptValue::from_f64(n)
        },
    );

    // sys.navsecs(period) -> the SAME looping clock as sys.simsecs, but it does
    // NOT arm the 1 Hz re-eval pump. For `fn tick()` cards that update named
    // widgets in place (ui.<id>.set_text) and must NEVER rebuild — e.g. the
    // live-navigation card, where a rebuild would tear the map widget down.
    vm.add_method(
        sys,
        id_lut!(navsecs),
        script_args_def!(period = NIL),
        |vm, args| {
            let period = script_value!(vm, args.period).as_number().unwrap_or(0.0);
            let secs = crate::splash::sim_clock_secs();
            let v = if period > 0.0 { secs % period } else { secs };
            ScriptValue::from_f64(v)
        },
    );

    // sys.simsecs(period) -> seconds since app start as a NUMBER, looping back
    // to 0 every `period` seconds (period <= 0 -> unbounded). THE animation
    // clock for cards: bind time windows to auto-advance content — a card that
    // calls it re-evaluates once per second (see the Splash pump):
    //   if sys.simsecs(70) >= 12 && sys.simsecs(70) < 15 { <frame/banner> }
    // Pure math — no fetch, no state, loops forever (replay for free).
    vm.add_method(
        sys,
        id_lut!(simsecs),
        script_args_def!(period = NIL),
        |vm, args| {
            let period = script_value!(vm, args.period).as_number().unwrap_or(0.0);
            let secs = crate::splash::sim_clock_secs();
            let v = if period > 0.0 { secs % period } else { secs };
            ScriptValue::from_f64(v)
        },
    );

    // sys.citytime(lat, lon, "field") -> the CURRENT wall-clock time AT A PLACE,
    // as a string. The DST-correct UTC offset comes from ONE cached open-meteo
    // fetch per lat/lon (`timezone=auto`); the time itself is the device clock
    // plus that offset, so a `fn tick()` card re-reading it every second costs
    // nothing after the first fetch.
    //   "hm"   -> "14:05"      "hms"     -> "14:05:09"
    //   "h12"  -> "2:05 PM"    "day"     -> "Mon"     "day_zh"  -> "周一"
    //   "date" -> "Aug 4"      "date_zh" -> "8月4日"
    //   "offset" -> "UTC+9" / "UTC+5:30" / "UTC-7"    "tz" -> "Asia/Tokyo"
    // Returns "—" while the offset fetch loads; the next tick fills it in.
    vm.add_method(
        sys,
        id_lut!(citytime),
        script_args_def!(lat = NIL, lon = NIL, field = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat).as_number().unwrap_or(0.0);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(0.0);
            let field_value = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_value, &mut field);
            let url = tz_offset_url(lat, lon);
            let value = match vm.host.cx_mut().script_data_fetch(&url) {
                Some(bytes) => match field.trim() {
                    "tz" => json_pluck(&bytes, "timezone").unwrap_or_else(|| "—".to_string()),
                    "abbr" => json_pluck(&bytes, "timezone_abbreviation")
                        .unwrap_or_else(|| "—".to_string()),
                    f => match tz_offset_secs(&bytes) {
                        Some(off) => format_city_time(now_unix_secs() as i64 + off, off, f),
                        None => "—".to_string(),
                    },
                },
                None => vm.host.cx_mut().script_data_placeholder(&url),
            };
            vm.bx.heap.new_string_from_str(&value)
        },
    );

    // sys.citytimenum(lat, lon, "field") -> the same place-local clock as a
    // NUMBER for script conditions (day/night row theming, analog hands):
    //   "hour" 0-23   "hour12" 1-12   "minute"   "second"   "offsecs"
    // -9999 while the offset fetch loads.
    vm.add_method(
        sys,
        id_lut!(citytimenum),
        script_args_def!(lat = NIL, lon = NIL, field = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat).as_number().unwrap_or(0.0);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(0.0);
            let field_value = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_value, &mut field);
            let url = tz_offset_url(lat, lon);
            let n = vm
                .host
                .cx_mut()
                .script_data_fetch(&url)
                .and_then(|bytes| tz_offset_secs(&bytes))
                .map(|off| {
                    let local = now_unix_secs() as i64 + off;
                    let (h, m, s) = hms_from_secs(local);
                    match field.trim() {
                        "hour" => h as f64,
                        "hour12" => {
                            let x = h % 12;
                            if x == 0 { 12.0 } else { x as f64 }
                        }
                        "minute" => m as f64,
                        "second" => s as f64,
                        "offsecs" => off as f64,
                        _ => -9999.0,
                    }
                })
                .unwrap_or(-9999.0);
            ScriptValue::from_f64(n)
        },
    );

    // sys.fx("FROM", "TO") -> the LIVE exchange rate as a display string
    // ("0.8669", "156.68") — ExchangeRate-API open endpoint, keyless.
    // ONE cached fetch per FROM currency covers every TO. "—" while loading.
    // For MATH use sys.fxnum; never write a literal rate into a card.
    vm.add_method(
        sys,
        id_lut!(fx),
        script_args_def!(from = NIL, to = NIL),
        |vm, args| {
            let from_value = script_value!(vm, args.from);
            let mut from = String::new();
            vm.bx.heap.cast_to_string(from_value, &mut from);
            let from = from.trim().to_ascii_uppercase();
            let to_value = script_value!(vm, args.to);
            let mut to = String::new();
            vm.bx.heap.cast_to_string(to_value, &mut to);
            let to = to.trim().to_ascii_uppercase();
            let value = if from == to {
                "1".to_string()
            } else {
                let url = fx_url(&from, &to);
                match vm.host.cx_mut().script_data_fetch(&url) {
                    Some(bytes) => json_pluck(&bytes, FX_RATE_PATH)
                        .and_then(|s| s.parse::<f64>().ok())
                        .map(format_fx_rate)
                        .unwrap_or_else(|| "—".to_string()),
                    None => vm.host.cx_mut().script_data_placeholder(&url),
                }
            };
            vm.bx.heap.new_string_from_str(&value)
        },
    );

    // sys.fxnum("FROM", "TO") -> the raw rate as a NUMBER for math:
    //   app.amt * sys.fxnum("USD", "EUR")
    // -9999 while loading — gate on `>= 0` before showing a converted amount.
    vm.add_method(
        sys,
        id_lut!(fxnum),
        script_args_def!(from = NIL, to = NIL),
        |vm, args| {
            let from_value = script_value!(vm, args.from);
            let mut from = String::new();
            vm.bx.heap.cast_to_string(from_value, &mut from);
            let from = from.trim().to_ascii_uppercase();
            let to_value = script_value!(vm, args.to);
            let mut to = String::new();
            vm.bx.heap.cast_to_string(to_value, &mut to);
            let to = to.trim().to_ascii_uppercase();
            let n = if from == to {
                1.0
            } else {
                let url = fx_url(&from, &to);
                vm.host
                    .cx_mut()
                    .script_data_fetch(&url)
                    .and_then(|bytes| json_pluck(&bytes, FX_RATE_PATH))
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(-9999.0)
            };
            ScriptValue::from_f64(n)
        },
    );

    // sys.fmtdur(secs) -> a duration/clock string: "04:35" under an hour,
    // "1:04:35" from an hour up. Negative clamps to "00:00". Pure math — the
    // timer/stopwatch formatter (the script engine has no floor or %).
    vm.add_method(
        sys,
        id_lut!(fmtdur),
        script_args_def!(secs = NIL),
        |vm, args| {
            let n = script_value!(vm, args.secs).as_number().unwrap_or(0.0);
            let t = if n.is_finite() && n > 0.0 { n.round() as i64 } else { 0 };
            let (h, m, s) = (t / 3600, (t / 60) % 60, t % 60);
            let out = if h > 0 {
                format!("{h}:{m:02}:{s:02}")
            } else {
                format!("{m:02}:{s:02}")
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    // sys.fmtnum(x, maxdecimals) -> x as a display string with AT MOST that many
    // decimals, trailing zeros trimmed: sys.fmtnum(42.0, 6) -> "42",
    // sys.fmtnum(0.866934, 4) -> "0.8669". THE display formatter for calculator
    // results and converted amounts — the script engine cannot round.
    vm.add_method(
        sys,
        id_lut!(fmtnum),
        script_args_def!(x = NIL, decimals = NIL),
        |vm, args| {
            let x = script_value!(vm, args.x).as_number().unwrap_or(0.0);
            let d = script_value!(vm, args.decimals)
                .as_number()
                .unwrap_or(2.0)
                .clamp(0.0, 9.0) as usize;
            let mut out = if x.is_finite() {
                format!("{x:.d$}")
            } else {
                "—".to_string()
            };
            if out.contains('.') {
                while out.ends_with('0') {
                    out.pop();
                }
                if out.ends_with('.') {
                    out.pop();
                }
            }
            if out == "-0" {
                out = "0".to_string();
            }
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    // sys.weather(lat, lon, "path") -> a LIVE value from the open-meteo forecast
    // API (temperature, humidity, wind, pressure, UV, 7-day highs/lows, sunrise/
    // sunset). `path` is dot-separated into the JSON; a numeric segment indexes an
    // array, e.g.:
    //   sys.weather(LAT, LON, "current.temperature_2m")     -> "27.3"
    //   sys.weather(LAT, LON, "current.relative_humidity_2m")-> "54"
    //   sys.weather(LAT, LON, "daily.temperature_2m_max.0")  -> "29.1"  (today)
    //   sys.weather(LAT, LON, "daily.sunrise.0")             -> "05:52" (HH:MM)
    // All fields for a given lat/lon share ONE cached fetch. Returns "—" while the
    // (async) request loads; the card auto-redraws when data arrives, so the value
    // fills in. THE LLM MUST CALL THIS FOR EVERY WEATHER NUMBER — never hardcode.
    vm.add_method(
        sys,
        id_lut!(weather),
        script_args_def!(lat = NIL, lon = NIL, path = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat).as_number().unwrap_or(0.0);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(0.0);
            let path_value = script_value!(vm, args.path);
            let mut path = String::new();
            vm.bx.heap.cast_to_string(path_value, &mut path);
            let url = format!(
                "https://api.open-meteo.com/v1/forecast?latitude={lat:.4}&longitude={lon:.4}\
&current=temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,wind_speed_10m,surface_pressure,is_day\
&daily=weather_code,temperature_2m_max,temperature_2m_min,sunrise,sunset,uv_index_max,precipitation_probability_max\
&timezone=auto&forecast_days=7"
            );
            let value = match vm.host.cx_mut().script_data_fetch(&url) {
                // Union of two changes: round the plucked value (whole-degree
                // display), and show the terminal-failure placeholder rather than
                // a bare "—" when the fetch itself gives up (#17). The em dash
                // stays for the case where the fetch SUCCEEDED but the path is
                // absent, which is a card bug, not a network one.
                Some(bytes) => json_pluck(&bytes, path.trim())
                    .map(|v| round_display(path.trim(), v))
                    .unwrap_or_else(|| "—".to_string()),
                None => vm.host.cx_mut().script_data_placeholder(&url),
            };
            vm.bx.heap.new_string_from_str(&value)
        },
    );

    // sys.airquality(lat, lon, "path") -> a LIVE value from the open-meteo air-
    // quality API. e.g. sys.airquality(LAT, LON, "current.us_aqi") -> "42",
    // "current.pm2_5", "current.pm10", "current.ozone". Same "—"/redraw semantics
    // as sys.weather. (The AQI *map tile* is sys.airmap; this is the number.)
    vm.add_method(
        sys,
        id_lut!(airquality),
        script_args_def!(lat = NIL, lon = NIL, path = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat).as_number().unwrap_or(0.0);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(0.0);
            let path_value = script_value!(vm, args.path);
            let mut path = String::new();
            vm.bx.heap.cast_to_string(path_value, &mut path);
            let url = format!(
                "https://air-quality-api.open-meteo.com/v1/air-quality?latitude={lat:.4}&longitude={lon:.4}\
&current=us_aqi,pm2_5,pm10,ozone,nitrogen_dioxide&timezone=auto"
            );
            let value = match vm.host.cx_mut().script_data_fetch(&url) {
                Some(bytes) => json_pluck(&bytes, path.trim()).unwrap_or_else(|| "—".to_string()),
                None => vm.host.cx_mut().script_data_placeholder(&url),
            };
            vm.bx.heap.new_string_from_str(&value)
        },
    );

    // sys.weathernum(lat, lon, "path") / sys.aqinum(lat, lon, "path") -> the SAME
    // live open-meteo values as sys.weather / sys.airquality, but as a NUMBER, so
    // script conditions can branch on LIVE data — the enabling primitive for
    // COMPOSED cards (pick activities by temperature/precipitation, gate
    // "go outside" on AQI, switch content on is_day). Returns -9999 while the
    // fetch loads or when the path is absent/non-numeric — guard with
    // `>= -9998`; the card re-evaluates when the fetch lands (same redraw
    // semantics as the string helpers). Shares the string helpers' fetch
    // (identical URL -> one request serves both).
    vm.add_method(
        sys,
        id_lut!(weathernum),
        script_args_def!(lat = NIL, lon = NIL, path = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat).as_number().unwrap_or(0.0);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(0.0);
            let path_value = script_value!(vm, args.path);
            let mut path = String::new();
            vm.bx.heap.cast_to_string(path_value, &mut path);
            let url = format!(
                "https://api.open-meteo.com/v1/forecast?latitude={lat:.4}&longitude={lon:.4}\
&current=temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,wind_speed_10m,surface_pressure,is_day\
&daily=weather_code,temperature_2m_max,temperature_2m_min,sunrise,sunset,uv_index_max,precipitation_probability_max\
&timezone=auto&forecast_days=7"
            );
            let n = vm
                .host
                .cx_mut()
                .script_data_fetch(&url)
                .and_then(|bytes| json_pluck(&bytes, path.trim()))
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(-9999.0);
            ScriptValue::from_f64(n)
        },
    );
    // sys.num(v) -> v as a NUMBER, for L1 arithmetic over live calls.
    //
    // Every other sys.* helper returns a STRING, because a string is what a card
    // renders and what concatenation composes: `"$" + sys.stock(…)` is how every
    // live value reaches the screen. L1's expression form needs the other thing.
    // `sys.cities(0,"feels") - sys.cities(0,"temp")` is string subtraction
    // without this, which the VM evaluates to NaN — measured on device, the card
    // drew "≈NaN°" in every row while every other value on the same row was
    // correct.
    //
    // ONE helper rather than a `*num` variant per capability. `geocodenum` and
    // `aqinum` exist because two specific fields were needed as numbers by other
    // calls; L1 can ask for arithmetic over ANY numeric field of ANY capability,
    // so the coercion belongs at the value rather than at the source.
    //
    // A value that is not a number yields NaN deliberately — an em dash while a
    // fetch is in flight, or a field the helper cannot answer. NOT zero: a zero
    // is a fabricated number, and arithmetic that quietly treats missing data as
    // nothing is the exact failure profile §4 exists to prevent.
    // Numeric coercion is installed with the pure L0 helpers above.


    vm.add_method(
        sys,
        id_lut!(aqinum),
        script_args_def!(lat = NIL, lon = NIL, path = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat).as_number().unwrap_or(0.0);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(0.0);
            let path_value = script_value!(vm, args.path);
            let mut path = String::new();
            vm.bx.heap.cast_to_string(path_value, &mut path);
            let url = format!(
                "https://air-quality-api.open-meteo.com/v1/air-quality?latitude={lat:.4}&longitude={lon:.4}\
&current=us_aqi,pm2_5,pm10,ozone,nitrogen_dioxide&timezone=auto"
            );
            let n = vm
                .host
                .cx_mut()
                .script_data_fetch(&url)
                .and_then(|bytes| json_pluck(&bytes, path.trim()))
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(-9999.0);
            ScriptValue::from_f64(n)
        },
    );

    // sys.weathercond(lat, lon, "path") -> the WeatherIcon index 0..7 for the WMO
    // weather code at `path` ("current.weather_code" or "daily.weather_code.N").
    //
    // This exists because the CONDITION IS LIVE DATA. A generated card used to
    // carry `draw_bg.cond: 2` — a number the model chose, for weather it had never
    // seen. It is the same class of invented value as a coordinate or a
    // temperature, and it fails the same way: a plausible icon that does not match
    // what the sky is doing, with nothing to catch it. `weather_code` is already in
    // the cached forecast, so the mapping belongs here.
    //
    // WMO 4677 code groups, per open-meteo's documentation.
    vm.add_method(
        sys,
        id_lut!(weathercond),
        script_args_def!(lat = NIL, lon = NIL, path = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat).as_number().unwrap_or(0.0);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(0.0);
            let path_value = script_value!(vm, args.path);
            let mut path = String::new();
            vm.bx.heap.cast_to_string(path_value, &mut path);
            let url = format!(
                "https://api.open-meteo.com/v1/forecast?latitude={lat:.4}&longitude={lon:.4}\
&current=temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,wind_speed_10m,surface_pressure,is_day\
&daily=weather_code,temperature_2m_max,temperature_2m_min,sunrise,sunset,uv_index_max,precipitation_probability_max\
&timezone=auto&forecast_days=7"
            );
            let code = vm
                .host
                .cx_mut()
                .script_data_fetch(&url)
                .and_then(|bytes| json_pluck(&bytes, path.trim()))
                .and_then(|s| s.parse::<f64>().ok())
                .map(|n| n as i64);
            // Partly cloudy while the fetch is in flight: the least wrong default,
            // and it changes to the real icon on the redraw.
            let idx = match code {
                Some(0) => 0,                                  // clear
                Some(1) | Some(2) => 1,                        // mainly clear / partly
                Some(3) => 2,                                  // overcast
                Some(45) | Some(48) => 7,                       // fog
                Some(51..=57) | Some(61..=67) | Some(80..=82) => 3, // drizzle / rain
                Some(71..=77) | Some(85) | Some(86) => 5,       // snow
                Some(95..=99) => 4,                            // thunderstorm
                _ => 1,
            };
            ScriptValue::from_f64(idx as f64)
        },
    );

    // sys.weatherword(lat, lon, "path", locale) -> the condition as DISPLAY TEXT
    // ("Partly Cloudy", "多云") for the live WMO code at `path`.
    //
    // The companion to sys.weathercond: the icon and the word must agree, and both
    // must come from the same live code. A card that carried the word itself could
    // say "Cloudy" over a rain icon, or over actual sunshine, and look fine.
    vm.add_method(
        sys,
        id_lut!(weatherword),
        script_args_def!(lat = NIL, lon = NIL, path = NIL, locale = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat).as_number().unwrap_or(0.0);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(0.0);
            let path_value = script_value!(vm, args.path);
            let mut path = String::new();
            vm.bx.heap.cast_to_string(path_value, &mut path);
            let loc_v = script_value!(vm, args.locale);
            let mut loc = String::new();
            vm.bx.heap.cast_to_string(loc_v, &mut loc);
            let zh = loc.trim().to_ascii_lowercase().starts_with("zh");
            let url = format!(
                "https://api.open-meteo.com/v1/forecast?latitude={lat:.4}&longitude={lon:.4}\
&current=temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,wind_speed_10m,surface_pressure,is_day\
&daily=weather_code,temperature_2m_max,temperature_2m_min,sunrise,sunset,uv_index_max,precipitation_probability_max\
&timezone=auto&forecast_days=7"
            );
            let code = vm
                .host
                .cx_mut()
                .script_data_fetch(&url)
                .and_then(|bytes| json_pluck(&bytes, path.trim()))
                .and_then(|s| s.parse::<f64>().ok())
                .map(|n| n as i64);
            let (en, cn) = match code {
                Some(0) => ("Clear", "晴"),
                Some(1) => ("Mainly Clear", "晴间多云"),
                Some(2) => ("Partly Cloudy", "局部多云"),
                Some(3) => ("Overcast", "阴"),
                Some(45) | Some(48) => ("Fog", "雾"),
                Some(51..=57) => ("Drizzle", "小雨"),
                Some(61..=67) => ("Rain", "雨"),
                Some(71..=77) => ("Snow", "雪"),
                Some(80..=82) => ("Showers", "阵雨"),
                Some(85) | Some(86) => ("Snow Showers", "阵雪"),
                Some(95..=99) => ("Thunderstorm", "雷暴"),
                // Nothing loaded yet — an em dash, consistent with sys.weather,
                // rather than a guess that later changes.
                _ => ("—", "—"),
            };
            vm.bx.heap.new_string_from_str(if zh { cn } else { en })
        },
    );

    // sys.dayname(lat, lon, n, locale) -> the weekday LABEL for forecast row n.
    //   sys.dayname(LAT, LON, 0, "en") -> "Now"
    //   sys.dayname(LAT, LON, 1, "en") -> "Thu"
    //   sys.dayname(LAT, LON, 1, "zh") -> "周四"
    //
    // A card must NEVER write weekday names as literal strings, for two reasons
    // that both bit us before this existed:
    //
    //   * The generating model does not reliably know the date. On Wed
    //     2026-07-29 it emitted "Today, Wed, Thu, …" — repeating today as
    //     tomorrow, so every row after the first was mislabelled. Nobody
    //     notices, because a wrong weekday looks exactly like a right one.
    //   * Cards are PERSISTED and re-served (a2app_cards/), so a literal is
    //     stale the next morning even when it was correct when generated.
    //
    // The date comes from `daily.time.n` in the SAME cached forecast the
    // temperatures come from, so the labels are local to the FORECAST'S place —
    // not to wherever the phone happens to be. Until that fetch lands we fall
    // back to the device clock, which is right unless the place is across a date
    // boundary, and self-corrects on the redraw when the real date arrives.
    vm.add_method(
        sys,
        id_lut!(dayname),
        script_args_def!(lat = NIL, lon = NIL, n = NIL, locale = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat).as_number().unwrap_or(0.0);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(0.0);
            let n = script_value!(vm, args.n).as_number().unwrap_or(0.0).max(0.0) as usize;
            let loc_v = script_value!(vm, args.locale);
            let mut loc = String::new();
            vm.bx.heap.cast_to_string(loc_v, &mut loc);
            let zh = loc.trim().to_ascii_lowercase().starts_with("zh");

            if n == 0 {
                // "Now", not "Today". The forecast's day column is a FIXED width
                // — the labels have to line up down the list — and "Today" does
                // not fit it, so row one wrapped to "Toda / y" beside six
                // three-letter weekdays. "Now" is also the more accurate word for
                // a row whose reading is current rather than forecast.
                let s = if zh { "现在" } else { "Now" };
                return vm.bx.heap.new_string_from_str(s);
            }

            let url = format!(
                "https://api.open-meteo.com/v1/forecast?latitude={lat:.4}&longitude={lon:.4}\
&current=temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,wind_speed_10m,surface_pressure,is_day\
&daily=weather_code,temperature_2m_max,temperature_2m_min,sunrise,sunset,uv_index_max,precipitation_probability_max\
&timezone=auto&forecast_days=7"
            );
            // Authoritative: the forecast's own local date for that row.
            let from_api = vm.host.cx_mut().script_data_fetch(&url).and_then(|bytes| {
                let date = json_pluck(&bytes, &format!("daily.time.{n}"))?;
                let mut it = date.trim().split('-');
                let y = it.next()?.parse::<i64>().ok()?;
                let m = it.next()?.parse::<u64>().ok()?;
                let d = it.next()?.parse::<u64>().ok()?;
                Some(weekday_from_days(days_from_civil(y, m, d)))
            });
            // Graceful: the device's own day, offset by n.
            let wd = from_api.unwrap_or_else(|| {
                weekday_from_days((now_unix_secs() / 86_400) as i64 + n as i64)
            });
            let s = if zh { DAY_ZH[wd] } else { DAY_EN[wd] };
            vm.bx.heap.new_string_from_str(s)
        },
    );

    // sys.weekmin(lat, lon) / sys.weekmax(lat, lon) -> the LOWEST low and HIGHEST
    // high across the 7-day forecast, for a TempBar's draw_bg.wmin / draw_bg.wmax.
    //
    // These exist because the card CANNOT know them. Every temperature on the card
    // is a live sys.weather call, so a generated card asking the model to name the
    // week's range is asking it to guess at numbers it has never seen — and it
    // guesses badly ("10 to 35" for a 27-39C week), which clamps every high to the
    // red end and pushes the whole week into the top of the scale.
    //
    // Shares the cached forecast fetch, so neither costs an extra request.
    // Falls back to a plausible temperate range if the fetch is still in flight,
    // rather than to 0/0 — a zero span would collapse every bar to one colour.
    vm.add_method(
        sys,
        id_lut!(weekmin),
        script_args_def!(lat = NIL, lon = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat).as_number().unwrap_or(0.0);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(0.0);
            let n = week_extreme(vm, lat, lon, "daily.temperature_2m_min", false).unwrap_or(0.0);
            ScriptValue::from_f64(n)
        },
    );
    vm.add_method(
        sys,
        id_lut!(weekmax),
        script_args_def!(lat = NIL, lon = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat).as_number().unwrap_or(0.0);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(0.0);
            let n = week_extreme(vm, lat, lon, "daily.temperature_2m_max", true).unwrap_or(30.0);
            ScriptValue::from_f64(n)
        },
    );

    // sys.moonphase("field") -> the CURRENT 月相 (moon phase), computed from the
    // device clock — no network, so it never shows a "—" placeholder.
    //   "name"         -> "Waxing Gibbous"  (one of the eight principal phases)
    //   "name_zh"      -> "盈凸月"           (the same phase, 八相 names)
    //   "illumination" -> "87"   (percent of the disc lit, 0-100)
    //   "phase"        -> "0.62" (position in the cycle, 0 new .. 0.5 full .. 1)
    // For the MoonPhase WIDGET uniform use sys.moonnum("phase"), which returns a
    // number: draw_bg.phase needs a float, and a string silently reads as 0.
    vm.add_method(
        sys,
        id_lut!(moonphase),
        script_args_def!(field = NIL),
        |vm, args| {
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let f = moon_phase_fraction();
            let value = match field.trim().to_ascii_lowercase().as_str() {
                "name" => moon_phase_name(f).to_string(),
                "name_zh" | "name_cn" => moon_phase_name_zh(f).to_string(),
                // Illuminated fraction is (1 - cos(2*pi*phase)) / 2: 0 at new,
                // 1 at full, and correctly non-linear in between.
                "illumination" | "illum" => {
                    let lit = (1.0 - (std::f64::consts::TAU * f).cos()) * 50.0;
                    format!("{}", lit.round() as i64)
                }
                _ => format!("{f:.2}"),
            };
            vm.bx.heap.new_string_from_str(&value)
        },
    );
    vm.add_method(
        sys,
        id_lut!(moonnum),
        script_args_def!(field = NIL),
        |vm, args| {
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let f = moon_phase_fraction();
            let n = match field.trim().to_ascii_lowercase().as_str() {
                "illumination" | "illum" => (1.0 - (std::f64::consts::TAU * f).cos()) * 50.0,
                _ => f,
            };
            ScriptValue::from_f64(n)
        },
    );

    // sys.daylight(lat, lon) -> fraction of DAYLIGHT elapsed: 0 at sunrise, 1 at
    // sunset, for the SunArc widget's draw_bg.progress. Before sunrise it is
    // negative and after sunset greater than 1, which the widget reads as night
    // and dims the sun rather than hiding it.
    //
    // Shares the cached sys.weather forecast fetch, so it costs no extra request.
    // Returns 0.5 while that fetch is in flight — the arc then shows a midday sun
    // for one redraw, which reads better than a sun jammed at the horizon.
    vm.add_method(
        sys,
        id_lut!(daylight),
        script_args_def!(lat = NIL, lon = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat).as_number().unwrap_or(0.0);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(0.0);
            let url = format!(
                "https://api.open-meteo.com/v1/forecast?latitude={lat:.4}&longitude={lon:.4}\
&current=temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,wind_speed_10m,surface_pressure,is_day\
&daily=weather_code,temperature_2m_max,temperature_2m_min,sunrise,sunset,uv_index_max,precipitation_probability_max\
&timezone=auto&forecast_days=7"
            );
            let progress = vm
                .host
                .cx_mut()
                .script_data_fetch(&url)
                .and_then(|bytes| {
                    let rise = hhmm_to_minutes(&json_pluck(&bytes, "daily.sunrise.0")?)?;
                    let set = hhmm_to_minutes(&json_pluck(&bytes, "daily.sunset.0")?)?;

                    // "Now" comes from the DEVICE CLOCK shifted by the city's UTC
                    // offset — NOT from `current.time` in the response. The
                    // forecast fetch is cached, so its timestamp is frozen at
                    // whenever the card last fetched; using it parks the sun
                    // wherever it stood then and drifts further all day. Sunrise,
                    // sunset and the offset are all stable for the day, so only
                    // the current instant must come from a live source.
                    let offset = json_pluck(&bytes, "utc_offset_seconds")?
                        .parse::<f64>()
                        .ok()?;
                    let local = (now_unix_secs() as f64 + offset).rem_euclid(86_400.0);
                    let now = local / 60.0;

                    let span = set - rise;
                    if span <= 0.0 {
                        // Polar day or night — no meaningful fraction.
                        return None;
                    }
                    Some((now - rise) / span)
                })
                .unwrap_or(0.5);
            ScriptValue::from_f64(progress)
        },
    );

    // sys.aqigrid(lat, lon, span, idx) -> the US-AQI at one cell of a 4x4 grid
    // covering `span` degrees centred on (lat, lon), for the AqiContour widget's
    // a0..a15 uniforms. `idx` is 0..15, row-major with the NORTH row first.
    //
    // All sixteen cells come from ONE multi-location open-meteo request, which
    // that API serves by accepting comma-separated coordinates and returning an
    // array of results. Because the URL is identical for every idx, the sixteen
    // card-side calls collapse to a single cached fetch.
    vm.add_method(
        sys,
        id_lut!(aqigrid),
        script_args_def!(lat = NIL, lon = NIL, span = NIL, idx = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat).as_number().unwrap_or(0.0);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(0.0);
            let span = script_value!(vm, args.span).as_number().unwrap_or(1.5);
            let idx = script_value!(vm, args.idx).as_number().unwrap_or(0.0) as usize;
            let idx = idx.min(15);

            // Row 0 is the NORTH edge, so latitude DECREASES with the row index —
            // matching how the shader walks self.pos.y downward.
            let step = span / 3.0;
            let mut lats = Vec::with_capacity(16);
            let mut lons = Vec::with_capacity(16);
            for r in 0..4 {
                for c in 0..4 {
                    lats.push(format!("{:.4}", lat + span / 2.0 - r as f64 * step));
                    lons.push(format!("{:.4}", lon - span / 2.0 + c as f64 * step));
                }
            }
            let url = format!(
                "https://air-quality-api.open-meteo.com/v1/air-quality?latitude={}&longitude={}\
&current=us_aqi&timezone=auto",
                lats.join(","),
                lons.join(",")
            );
            let n = vm
                .host
                .cx_mut()
                .script_data_fetch(&url)
                .and_then(|bytes| json_pluck(&bytes, &format!("{idx}.current.us_aqi")))
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            ScriptValue::from_f64(n)
        },
    );

    // sys.stock("AAPL", "key") -> a LIVE value from Yahoo Finance for that ticker.
    // Same "—"/redraw semantics as sys.weather. `key` (case-insensitive):
    //   price | prev | high | low | open | currency | name | symbol
    //   change    -> price − previous close, signed, e.g. "+1.99"
    //   changepct -> percent change, signed, e.g. "+0.63%"
    // e.g. sys.stock("AAPL", "price"), sys.stock("TSLA", "changepct").
    vm.add_method(
        sys,
        id_lut!(stock),
        script_args_def!(symbol = NIL, field = NIL),
        |vm, args| {
            let sym_v = script_value!(vm, args.symbol);
            let mut symbol = String::new();
            vm.bx.heap.cast_to_string(sym_v, &mut symbol);
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let sym = sanitize_ticker(&symbol);
            let url = format!(
                "https://query1.finance.yahoo.com/v8/finance/chart/{sym}?interval=1d&range=1d"
            );
            let out = match vm.host.cx_mut().script_data_fetch(&url) {
                None => vm.host.cx_mut().script_data_placeholder(&url),
                Some(bytes) => yahoo_chart_field(&bytes, &field),
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    // sys.stockbar("AAPL", index, count) -> the HEIGHT (dp, a NUMBER) of bar
    // `index` of `count` in a price sparkline (Yahoo closes, normalized so the
    // range's low→~8dp and high→~158dp). Draw the chart as a bottom-aligned
    // `flow: Right` row of thin `SolidView{ height: sys.stockbar("AAPL", N,
    // COUNT) }` bars (an area/line of the price path). Returns a small height
    // while the async fetch loads, then the card re-evaluates.
    // Optional 5th arg selects the series range: "1d" (default, 5-minute
    // intraday), "1w", "1m", "6m", "1y" — empty/unknown tokens fall back to
    // "1d" so an unset `{{state.range}}` still draws the intraday chart. The
    // close series is resampled to `count` bars, so the SAME bar row serves
    // every range. One fetch per symbol×range (URL-deduped).
    vm.add_method(
        sys,
        id_lut!(stockbar),
        script_args_def!(symbol = NIL, index = NIL, count = NIL, maxh = NIL, range = NIL),
        |vm, args| {
            let sym_v = script_value!(vm, args.symbol);
            let mut symbol = String::new();
            vm.bx.heap.cast_to_string(sym_v, &mut symbol);
            let index = script_value!(vm, args.index).as_number().unwrap_or(0.0).max(0.0) as usize;
            let count = (script_value!(vm, args.count).as_number().unwrap_or(40.0) as usize).max(2);
            // Optional 4th arg: the chart's pixel height, so the area fills it
            // exactly with no peak clipping. Defaults to 150 (legacy behavior).
            let maxh = script_value!(vm, args.maxh).as_number().filter(|v| v.is_finite() && *v > 8.0 && *v < 10_000.0).unwrap_or(150.0);
            let range_v = script_value!(vm, args.range);
            let mut range = String::new();
            vm.bx.heap.cast_to_string(range_v, &mut range);
            let url = yahoo_chart_url(&symbol, &range);
            let h = match vm.host.cx_mut().script_data_fetch(&url) {
                Some(bytes) => stock_bar_height(&bytes, index, count, maxh),
                None => 6.0,
            };
            ScriptValue::from_f64(h)
        },
    );

    // sys.stockrange("<TICKER>", "<RANGE>", "field") -> a range-aware scalar for
    // the SAME close series the chart bars draw (same URL → the one fetch is
    // shared). `sys.stock("high"/"low"/"change"/"changepct")` is DAY-only, so a
    // card whose chart switches range uses THIS for the Y-axis labels and the
    // change line. Range tokens as in sys.stockbar ("1d" default). Fields
    // (case-insensitive): "high" | "low" (range extremes, 2dp), "change" |
    // "changepct" (signed first→last close over the range), "up" ("1"/"0",
    // last >= first — for green/red styling). "—" while loading / on error.
    vm.add_method(
        sys,
        id_lut!(stockrange),
        script_args_def!(symbol = NIL, range = NIL, field = NIL),
        |vm, args| {
            let sym_v = script_value!(vm, args.symbol);
            let mut symbol = String::new();
            vm.bx.heap.cast_to_string(sym_v, &mut symbol);
            let range_v = script_value!(vm, args.range);
            let mut range = String::new();
            vm.bx.heap.cast_to_string(range_v, &mut range);
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let url = yahoo_chart_url(&symbol, &range);
            let out = match vm.host.cx_mut().script_data_fetch(&url) {
                None => vm.host.cx_mut().script_data_placeholder(&url),
                Some(bytes) => stock_range_field(&bytes, field.trim()),
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    // sys.movers(index, "field", symbols?) -> the LIVE movers list. With no
    // `symbols` this is Yahoo's day_gainers screener (no auth), index 0 = the
    // biggest % gainer today, up to 9. With a comma-separated `symbols` list it
    // ranks THAT universe instead -- the only way to answer "top AI movers",
    // since the screener has no theme. Fields
    // (case-insensitive): symbol, name, price, change (signed), changepct (signed %),
    // high, low, prev, open, 52wh, 52wl, vol, marketcap, currency, exchange.
    // ONE fetch (deduped by URL) serves all 10 rows × all fields. Use for a top-10
    // movers LIST card; tap a row to open the per-ticker detail (sys.stock/stockbar).
    vm.add_method(
        sys,
        id_lut!(movers),
        script_args_def!(index = NIL, field = NIL, symbols = NIL),
        |vm, args| {
            let index = script_value!(vm, args.index).as_number().unwrap_or(0.0).max(0.0) as i64;
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let symbols_v = script_value!(vm, args.symbols);
            let mut symbols = String::new();
            vm.bx.heap.cast_to_string(symbols_v, &mut symbols);
            let symbols = symbols.trim().to_string();

            // A NAMED universe: rank only these symbols. Yahoo's screener has no
            // `scrIds` for a theme, so "top 10 AI movers" could otherwise only be
            // answered by putting an AI title over market-wide gainers.
            //
            // Which companies are AI is world knowledge and comes from the card;
            // who among them moved is computed here. Every symbol is fetched (the
            // fetch dedupes by URL) and the whole set must be present before any
            // row resolves -- ranking half a universe would silently reorder the
            // list as the rest arrived.
            if !symbols.is_empty() {
                let syms: Vec<&str> = symbols
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect();
                let url_of = |sym: &str| {
                    format!(
                        "https://query1.finance.yahoo.com/v8/finance/chart/{sym}?range=1d&interval=1d"
                    )
                };
                let mut rows: Vec<(f64, String, std::rc::Rc<Vec<u8>>)> = Vec::new();
                for sym in &syms {
                    let u = url_of(sym);
                    match vm.host.cx_mut().script_data_fetch(&u) {
                        None => {
                            // A ticker that will never resolve is DROPPED from
                            // the universe; only one still loading holds the
                            // list back. All-or-nothing was wrong: one delisted
                            // or mistyped symbol among eighteen left every row
                            // an em dash permanently.
                            let ph = vm.host.cx_mut().script_data_placeholder(&u);
                            if ph == "n/a" {
                                continue;
                            }
                            return vm.bx.heap.new_string_from_str(&ph);
                        }
                        Some(bytes) => {
                            let pluck = |k: &str| {
                                json_pluck(&bytes, &format!("chart.result.0.meta.{k}"))
                            };
                            let price = pluck("regularMarketPrice").and_then(|v| v.parse::<f64>().ok());
                            let prev = pluck("chartPreviousClose")
                                .or_else(|| pluck("previousClose"))
                                .and_then(|v| v.parse::<f64>().ok());
                            if let (Some(p), Some(pv)) = (price, prev) {
                                if pv != 0.0 {
                                    rows.push(((p - pv) / pv * 100.0, (*sym).to_string(), bytes));
                                }
                            }
                        }
                    }
                }
                rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                let out = match rows.get(index.max(0) as usize) {
                    None => "—".to_string(),
                    Some((pct, sym, bytes)) => {
                        let m = |k: &str| json_pluck(bytes, &format!("chart.result.0.meta.{k}"));
                        let n = |k: &str| m(k).and_then(|v| v.parse::<f64>().ok());
                        let prev = n("chartPreviousClose").or_else(|| n("previousClose"));
                        match field.trim().to_ascii_lowercase().as_str() {
                            "symbol" => m("symbol").unwrap_or_else(|| sym.clone()),
                            "name" => m("shortName")
                                .or_else(|| m("longName"))
                                .unwrap_or_else(|| sym.clone()),
                            "price" => n("regularMarketPrice")
                                .map(|v| format!("{v:.2}"))
                                .unwrap_or_else(|| "—".into()),
                            "change" => match (n("regularMarketPrice"), prev) {
                                (Some(p), Some(pv)) => format!("{:+.2}", p - pv),
                                _ => "—".into(),
                            },
                            "changepct" | "changepercent" => format!("{pct:+.2}%"),
                            "currency" => m("currency").unwrap_or_else(|| "—".into()),
                            "exchange" => m("fullExchangeName")
                                .or_else(|| m("exchangeName"))
                                .unwrap_or_else(|| "—".into()),
                            other => m(other).unwrap_or_else(|| "—".into()),
                        }
                    }
                };
                return vm.bx.heap.new_string_from_str(&out);
            }

            let url = "https://query1.finance.yahoo.com/v1/finance/screener/predefined/saved?scrIds=day_gainers&count=10".to_string();
            let base = format!("finance.result.0.quotes.{index}");
            let out = match vm.host.cx_mut().script_data_fetch(&url) {
                None => vm.host.cx_mut().script_data_placeholder(&url),
                Some(bytes) => {
                    let raw = |k: &str| json_pluck(&bytes, &format!("{base}.{k}"));
                    let num = |k: &str| raw(k).and_then(|s| s.parse::<f64>().ok());
                    let money = |k: &str| num(k).map(|v| format!("{v:.2}")).unwrap_or_else(|| "—".into());
                    match field.trim().to_ascii_lowercase().as_str() {
                        "symbol" => raw("symbol").unwrap_or_else(|| "—".into()),
                        "name" => raw("displayName")
                            .or_else(|| raw("shortName"))
                            .or_else(|| raw("longName"))
                            .unwrap_or_else(|| "—".into()),
                        "price" => money("regularMarketPrice"),
                        "change" => num("regularMarketChange").map(|v| format!("{v:+.2}")).unwrap_or_else(|| "—".into()),
                        "changepct" | "changepercent" => num("regularMarketChangePercent").map(|v| format!("{v:+.2}%")).unwrap_or_else(|| "—".into()),
                        "high" => money("regularMarketDayHigh"),
                        "low" => money("regularMarketDayLow"),
                        "prev" | "prevclose" => money("regularMarketPreviousClose"),
                        "open" => money("regularMarketOpen"),
                        "52wh" | "yearhigh" => money("fiftyTwoWeekHigh"),
                        "52wl" | "yearlow" => money("fiftyTwoWeekLow"),
                        "currency" => raw("currency").unwrap_or_else(|| "—".into()),
                        "exchange" => raw("fullExchangeName").unwrap_or_else(|| "—".into()),
                        "vol" | "volume" => match num("regularMarketVolume") {
                            Some(v) if v >= 1e9 => format!("{:.2}B", v / 1e9),
                            Some(v) if v >= 1e6 => format!("{:.1}M", v / 1e6),
                            Some(v) if v >= 1e3 => format!("{:.1}K", v / 1e3),
                            Some(v) => format!("{v:.0}"),
                            None => "—".to_string(),
                        },
                        "marketcap" | "mktcap" | "cap" => match num("marketCap") {
                            Some(v) if v >= 1e12 => format!("{:.2}T", v / 1e12),
                            Some(v) if v >= 1e9 => format!("{:.1}B", v / 1e9),
                            Some(v) if v >= 1e6 => format!("{:.0}M", v / 1e6),
                            Some(v) => format!("{v:.0}"),
                            None => "—".to_string(),
                        },
                        other => raw(other).unwrap_or_else(|| "—".into()),
                    }
                }
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    news_pipeline::install(vm, sys);
    composition_dataset::install(vm, sys);

    // sys.news(index, "key") -> a LIVE Hacker News front-page story (index 0..).
    // Same "—"/redraw semantics. `key` (case-insensitive):
    //   title | url | author | points | comments
    // e.g. sys.news(0, "title"), sys.news(0, "points"), sys.news(1, "title").
    vm.add_method(
        sys,
        id_lut!(news),
        script_args_def!(index = NIL, field = NIL),
        |vm, args| {
            let idx = script_value!(vm, args.index).as_number().unwrap_or(0.0).max(0.0) as i64;
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let key = match field.trim().to_ascii_lowercase().as_str() {
                "title" => "title",
                "url" => "url",
                "author" | "by" => "author",
                "points" | "score" => "points",
                "comments" | "num_comments" => "num_comments",
                // The story's IDENTITY, which the catalog has always offered and
                // the lowering has always emitted. It fell into the `_` arm and
                // was answered with the TITLE — so a card storing "which story did
                // the user tap" stored its headline, and `sys.news_item` was then
                // asked to look up a story by a string that is not an id. A silent
                // substitution in an identity field is the worst kind: everything
                // downstream works on a plausible value that means something else.
                "id" | "objectid" => "objectID",
                _ => "title",
            };
            let url =
                "https://hn.algolia.com/api/v1/search?tags=front_page&hitsPerPage=12".to_string();
            let path = format!("hits.{idx}.{key}");
            let out = match vm.host.cx_mut().script_data_fetch(&url) {
                Some(bytes) => json_pluck(&bytes, &path).unwrap_or_else(|| "—".to_string()),
                None => vm.host.cx_mut().script_data_placeholder(&url),
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    // sys.quakes(index, "field") -> a REAL recent earthquake from the USGS
    // live feed (M2.5+, last 24 h, keyless), newest first: index 0 = the most
    // recent event. THE LLM MUST CALL THIS FOR EVERY DISPLAYED VALUE — an
    // invented magnitude/place destroys trust in the whole card. Fields
    // (case-insensitive):
    //   place -> "42 km SW of Ashkasham, Afghanistan"
    //   mag   -> "4.6"        (one decimal)
    //   depth -> "10 km"      (whole km)
    //   time  -> "2h ago"     (humanized age; "now" under a minute)
    //   lat|lon -> "36.5622"  (4 decimals — chain into sys.basemap)
    //   count -> total events in the feed (ignores `index`)
    // Returns "—" while the (async) fetch loads or the index is out of range;
    // the card re-evaluates when data lands (same redraw semantics as
    // sys.weather). ONE URL-deduped fetch serves all rows × fields of a card.
    vm.add_method(
        sys,
        id_lut!(quakes),
        script_args_def!(index = NIL, field = NIL),
        |vm, args| {
            let idx = script_value!(vm, args.index).as_number().unwrap_or(0.0).max(0.0) as i64;
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let out = match vm.host.cx_mut().script_data_fetch(QUAKES_FEED_URL) {
                Some(bytes) => quake_field(&bytes, idx, field.trim()),
                None => "—".to_string(),
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    // sys.quakesnum(index, "field") -> the same quake values as NUMBERS
    // (mag, depth, lat, lon, count) so scripts can branch on magnitude or
    // chain the epicenter into sys.basemap(lat, lon). Returns -9999 while the
    // fetch loads; guard with >= -9998 (same convention as sys.weathernum).
    vm.add_method(
        sys,
        id_lut!(quakesnum),
        script_args_def!(index = NIL, field = NIL),
        |vm, args| {
            let idx = script_value!(vm, args.index).as_number().unwrap_or(0.0).max(0.0) as i64;
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let n = match vm.host.cx_mut().script_data_fetch(QUAKES_FEED_URL) {
                Some(bytes) => quake_num(&bytes, idx, field.trim()),
                None => -9999.0,
            };
            ScriptValue::from_f64(n)
        },
    );

    // sys.newsitem(id, "key") -> one story of the SAME front page `sys.news`
    // reads, found by its `objectID` rather than by row. Keys as `sys.news`.
    //
    // The same fetch on purpose. A detail screen opened from a list must agree
    // with the row that was tapped, and a second endpoint can rank, expire or
    // paginate differently between the tap and the read — so the story under the
    // headline would quietly be a different story. It also costs no extra request.
    //
    // Resolved from the cached front page when the id is there (one shared
    // fetch serves every field of every visible story), and from items/{id}
    // when it is not — a topic's top story or an old bookmark is still a
    // story this capability can plainly fetch.
    vm.add_method(
        sys,
        id_lut!(newsitem),
        script_args_def!(id = NIL, field = NIL),
        |vm, args| {
            let id_v = script_value!(vm, args.id);
            let mut id = String::new();
            vm.bx.heap.cast_to_string(id_v, &mut id);
            let id = id.trim().to_string();
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let key = match field.trim().to_ascii_lowercase().as_str() {
                "url" => "url",
                "author" | "by" => "author",
                "points" | "score" => "points",
                "comments" | "num_comments" => "num_comments",
                "id" | "objectid" => "objectID",
                _ => "title",
            };
            let url =
                "https://hn.algolia.com/api/v1/search?tags=front_page&hitsPerPage=12".to_string();
            let mut located = false;
            let out = match vm.host.cx_mut().script_data_fetch(&url) {
                None => vm.host.cx_mut().script_data_placeholder(&url),
                Some(bytes) => {
                    let mut found = "\u{2014}".to_string();
                    if !id.is_empty() {
                        for row in 0..12 {
                            let at = json_pluck(&bytes, &format!("hits.{row}.objectID"));
                            if at.as_deref() == Some(id.as_str()) {
                                found = json_pluck(&bytes, &format!("hits.{row}.{key}"))
                                    .unwrap_or_else(|| "\u{2014}".to_string());
                                located = true;
                                break;
                            }
                        }
                    }
                    found
                }
            };
            // Not on the front page — a topic's top story or an old bookmark.
            // The items endpoint serves any id forever (the reading list is
            // built on it), so fall through to it instead of shrugging "\u{2014}"
            // at a story this capability can plainly still fetch.
            let out = if !located && !id.is_empty() {
                let item_url = format!("https://hn.algolia.com/api/v1/items/{id}");
                match vm.host.cx_mut().script_data_fetch(&item_url) {
                    None => vm.host.cx_mut().script_data_placeholder(&item_url),
                    Some(bytes) => match key {
                        // items/{id} has no flat comment count; count nodes.
                        "num_comments" => {
                            let n = String::from_utf8_lossy(&bytes)
                                .matches("\"type\":\"comment\"")
                                .count();
                            format!("{n}")
                        }
                        // The items endpoint names the identity "id".
                        "objectID" => json_pluck(&bytes, "id")
                            .unwrap_or_else(|| "\u{2014}".to_string()),
                        k => json_pluck(&bytes, k).unwrap_or_else(|| "\u{2014}".to_string()),
                    },
                }
            } else {
                out
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    // sys.locale("lang" | "temp_unit") -> what the DEVICE is set to.
    //
    // Six of the seven L0 exemplars reach for this — it is what seeds a units
    // toggle and picks a language — and nothing answered it, so every one of them
    // read an em dash and the toggle appeared to start blank. Published by the app
    // through `set_locale`, the same way the durable collections and the position
    // fix cross this boundary.
    vm.add_method(
        sys,
        id_lut!(locale),
        script_args_def!(field = NIL),
        |vm, args| {
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let out = locale_field(field.trim());
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    // sys.prefs("units" | "range") -> the user's own stored choice (§5.12,
    // read-only). A reference the user owns, held in the same `user.json` the
    // durable collections live in — never a fetched fact.
    vm.add_method(
        sys,
        id_lut!(prefs),
        script_args_def!(field = NIL),
        |vm, args| {
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let out = pref_at(vm.bx.heap.heap_key(), field.trim()).unwrap_or_else(|| "\u{2014}".to_string());
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    // sys.reading(index, "key") -> row `index` of the user's SAVED reading list
    // (§5.12). The store holds Algolia story IDS — identities the item endpoint
    // serves forever — so a bookmark outlives the front page it was found on:
    // this fetches https://hn.algolia.com/api/v1/items/{id} per saved id and
    // answers today's title/points, never a stored copy. Keys as sys.news,
    // plus "id". "—" while the fetch loads; one fetch per id, deduped. The item
    // endpoint serves no comment COUNT, so `comments` counts comment nodes in
    // the payload rather than pretending a field exists.
    vm.add_method(
        sys,
        id_lut!(reading),
        script_args_def!(index = NIL, field = NIL),
        |vm, args| {
            let index = script_value!(vm, args.index)
                .as_number()
                .unwrap_or(0.0)
                .max(0.0) as usize;
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let Some(id) = collection_at(vm.bx.heap.heap_key(), "reading", index) else {
                return vm.bx.heap.new_string_from_str("—");
            };
            let key = match field.trim().to_ascii_lowercase().as_str() {
                "id" => {
                    // The identity itself needs no fetch — it IS the store.
                    return vm.bx.heap.new_string_from_str(&id);
                }
                "url" => "url",
                "author" | "by" => "author",
                "points" | "score" => "points",
                "comments" | "num_comments" => "num_comments",
                _ => "title",
            };
            let url = format!("https://hn.algolia.com/api/v1/items/{id}");
            let out = match vm.host.cx_mut().script_data_fetch(&url) {
                None => vm.host.cx_mut().script_data_placeholder(&url),
                Some(bytes) => match key {
                    "num_comments" => {
                        let n = String::from_utf8_lossy(&bytes)
                            .matches("\"type\":\"comment\"")
                            .count();
                        format!("{n}")
                    }
                    k => json_pluck(&bytes, k).unwrap_or_else(|| "—".to_string()),
                },
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    // sys.video("lofi hip hop", index, "key") -> one YouTube search result.
    //
    // Keyless, and it has to be: every public Piped/Invidious instance the old
    // web card fell back to is dead or 401s (checked 2026-08-09), which is why
    // that card ended up asking the MODEL to remember video ids — ids that go
    // stale and cannot be checked. YouTube's own results page carries the same
    // data the app needs, and this fork already scrapes it for live ids
    // (refresh_youtube_live_ids), so this is the proven path rather than a new
    // dependency.
    //
    // Keys: id, title, channel, length, views, age, thumb (an image url) and
    // embed (a player url a card can hand to sys.link). "count" answers how
    // many results parsed, ignoring index.
    vm.add_method(
        sys,
        id_lut!(video),
        script_args_def!(query = NIL, index = NIL, field = NIL),
        |vm, args| {
            let mut query = String::new();
            let v = script_value!(vm, args.query);
            vm.bx.heap.cast_to_string(v, &mut query);
            let index = script_value!(vm, args.index)
                .as_number()
                .unwrap_or(0.0)
                .max(0.0) as usize;
            let mut field = String::new();
            let v = script_value!(vm, args.field);
            vm.bx.heap.cast_to_string(v, &mut field);
            let key = field.trim().to_ascii_lowercase();

            if query.trim().is_empty() {
                return vm.bx.heap.new_string_from_str("");
            }
            let url = yt_search_url(&query);
            let bytes = match vm.host.cx_mut().script_data_fetch(&url) {
                None => {
                    let ph = vm.host.cx_mut().script_data_placeholder(&url);
                    return vm.bx.heap.new_string_from_str(&ph);
                }
                Some(b) => b,
            };
            let hits = yt_results_for(&url, &bytes);
            if key == "count" {
                return vm.bx.heap.new_string_from_str(&format!("{}", hits.len()));
            }
            let Some(hit) = hits.get(index) else {
                // Past the last result: empty, so a row list stops here rather
                // than padding itself with rows that draw as nothing.
                return vm.bx.heap.new_string_from_str("");
            };
            let out = match key.as_str() {
                "title" => hit.title.clone(),
                "channel" => hit.channel.clone(),
                "length" => hit.length.clone(),
                "views" => hit.views.clone(),
                "age" => hit.age.clone(),
                "thumb" => format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", hit.id),
                // The player url, ready to open. A card cannot build one: L0
                // has no string concatenation, and that is deliberate.
                //
                // The WATCH page, not /embed/. An embed refuses with "Error
                // 153: video player configuration error" when it is loaded as
                // a top-level document — the iframe player wants a real page
                // origin, and the overlay has none. Measured on device.
                "embed" => format!("https://m.youtube.com/watch?v={}", hit.id),
                _ => hit.id.clone(),
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    // sys.videonum(query, 0, "count") -> how many results parsed. The shape
    // fetched_rows expects of a searchable list, beside sys.searchnum.
    vm.add_method(
        sys,
        id_lut!(videonum),
        script_args_def!(query = NIL, index = NIL, field = NIL),
        |vm, args| {
            let mut query = String::new();
            let v = script_value!(vm, args.query);
            vm.bx.heap.cast_to_string(v, &mut query);
            if query.trim().is_empty() {
                return vm.bx.heap.new_string_from_str("0");
            }
            let url = yt_search_url(&query);
            match vm.host.cx_mut().script_data_fetch(&url) {
                None => vm.bx.heap.new_string_from_str("0"),
                Some(b) => {
                    let n = yt_results_for(&url, &b).len();
                    vm.bx.heap.new_string_from_str(&format!("{n}"))
                }
            }
        },
    );

    // sys.indicator("CHN,IND", "NY.GDP.MKTP.KD.ZG", 30, index, "key") -> one
    // country's reading of a World Bank indicator. Rows are indexed in the
    // order the CARD listed its countries, so row 0 is the first one it named
    // — the same order IndicatorPlot assigns its legend colours, or the
    // number beside the chart would belong to the other line.
    //
    // Shares the chart's URL, so the whole card costs ONE request.
    // Keys: name (as the API spells it), latest, first, change (latest minus
    // first), min, max, year (of the latest reading), title (the indicator's
    // own name), count (how many countries answered; ignores index).
    vm.add_method(
        sys,
        id_lut!(indicator),
        script_args_def!(countries = NIL, indicator = NIL, years = NIL, index = NIL, field = NIL),
        |vm, args| {
            let mut countries = String::new();
            let v = script_value!(vm, args.countries);
            vm.bx.heap.cast_to_string(v, &mut countries);
            let mut indicator = String::new();
            let v = script_value!(vm, args.indicator);
            vm.bx.heap.cast_to_string(v, &mut indicator);
            let years = script_value!(vm, args.years).as_number().unwrap_or(30.0);
            let index = script_value!(vm, args.index)
                .as_number()
                .unwrap_or(0.0)
                .max(0.0) as usize;
            let mut field = String::new();
            let v = script_value!(vm, args.field);
            vm.bx.heap.cast_to_string(v, &mut field);

            let codes = wb_codes(&countries);
            let code = wb_indicator(&indicator);
            if codes.is_empty() || code.is_empty() {
                return vm.bx.heap.new_string_from_str("\u{2014}");
            }
            // The chart's own URL builder — one request serves both, and the
            // span anchoring can only drift if they disagree.
            let url = crate::matplot::indicator_plot::worldbank_url(&codes, &code, years);
            let bytes = match vm.host.cx_mut().script_data_fetch(&url) {
                None => {
                    let ph = vm.host.cx_mut().script_data_placeholder(&url);
                    return vm.bx.heap.new_string_from_str(&ph);
                }
                Some(b) => b,
            };
            let root: serde_json::Value = match serde_json::from_slice(&bytes) {
                Ok(v) => v,
                Err(_) => return vm.bx.heap.new_string_from_str("\u{2014}"),
            };
            let rows = match root.get(1).and_then(|r| r.as_array()) {
                Some(r) => r,
                None => return vm.bx.heap.new_string_from_str("\u{2014}"),
            };
            if field.trim() == "count" {
                let n = codes
                    .iter()
                    .filter(|c| {
                        rows.iter().any(|r| {
                            r.get("countryiso3code").and_then(|v| v.as_str()) == Some(c.as_str())
                        })
                    })
                    .count();
                return vm.bx.heap.new_string_from_str(&format!("{n}"));
            }
            let Some(want) = codes.get(index) else {
                return vm.bx.heap.new_string_from_str("");
            };
            // (year, value) for this country, ascending, nulls dropped.
            let mut pairs: Vec<(f64, f64)> = rows
                .iter()
                .filter(|r| {
                    r.get("countryiso3code").and_then(|v| v.as_str()) == Some(want.as_str())
                })
                .filter_map(|r| {
                    let y = r.get("date")?.as_str()?.parse::<f64>().ok()?;
                    let v = r.get("value")?.as_f64()?;
                    (y.is_finite() && v.is_finite()).then_some((y, v))
                })
                .collect();
            pairs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            if pairs.is_empty() {
                return vm.bx.heap.new_string_from_str("\u{2014}");
            }
            let out = match field.trim().to_ascii_lowercase().as_str() {
                "name" => rows
                    .iter()
                    .find(|r| {
                        r.get("countryiso3code").and_then(|v| v.as_str()) == Some(want.as_str())
                    })
                    .and_then(|r| r.pointer("/country/value"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(want)
                    .to_string(),
                "title" => rows
                    .first()
                    .and_then(|r| r.pointer("/indicator/value"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                "first" => fmt_reading(pairs[0].1),
                "min" => fmt_reading(
                    pairs.iter().map(|p| p.1).fold(f64::INFINITY, f64::min),
                ),
                "max" => fmt_reading(
                    pairs.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max),
                ),
                "year" => format!("{}", pairs[pairs.len() - 1].0 as i64),
                "change" => {
                    let d = pairs[pairs.len() - 1].1 - pairs[0].1;
                    format!("{}{}", if d >= 0.0 { "+" } else { "" }, fmt_reading(d))
                }
                _ => fmt_reading(pairs[pairs.len() - 1].1),
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    // sys.watchlist_has("NVDA") -> "1" when that ticker is in the user's
    // saved list, else "0". Synchronous — the store is published, not
    // fetched — and it is what lets a quote page show Add or Remove for
    // the stock it is looking at.
    vm.add_method(
        sys,
        id_lut!(watchlist_has),
        script_args_def!(ticker = NIL),
        |vm, args| {
            let ticker_v = script_value!(vm, args.ticker);
            let mut ticker = String::new();
            vm.bx.heap.cast_to_string(ticker_v, &mut ticker);
            let ticker = ticker.trim();
            let held = (0..collection_len_for(vm.bx.heap.heap_key(), "watchlist"))
                .filter_map(|i| collection_at(vm.bx.heap.heap_key(), "watchlist", i))
                .any(|t| t == ticker);
            vm.bx.heap.new_string_from_str(if held { "1" } else { "0" })
        },
    );

    // sys.topics(index, "key") -> row `index` of the user's FOLLOWED topics
    // (§5.12). The store holds only the topic word ("ai", "nba"); the `top_*`
    // keys are the first hit of a fresh Algolia search for that word, run when
    // the row is read — a followed topic surfaces whatever is hot NOW, never
    // the story that was hot when it was followed. "—" while the (deduped)
    // fetch loads, like every joined row in this file.
    vm.add_method(
        sys,
        id_lut!(topics),
        script_args_def!(index = NIL, field = NIL),
        |vm, args| {
            let index = script_value!(vm, args.index)
                .as_number()
                .unwrap_or(0.0)
                .max(0.0) as usize;
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let Some(name) = collection_at(vm.bx.heap.heap_key(), "topics", index) else {
                return vm.bx.heap.new_string_from_str("—");
            };
            // json_pluck walks a dot-path from the ROOT (it is not a text
            // scan) — the search response nests the hit, so every key starts
            // at hits.0. The flat items/{id} keys in sys.reading do not.
            let key = match field.trim().to_ascii_lowercase().as_str() {
                "name" => {
                    // The identity itself needs no fetch — it IS the store.
                    return vm.bx.heap.new_string_from_str(&name);
                }
                "top_id" => "hits.0.objectID",
                "top_points" => "hits.0.points",
                _ => "hits.0.title",
            };
            // Restricted attributes: the default response opens each hit
            // with _highlightResult, whose nested "title" would be the first
            // occurrence json_pluck finds. Scoped to the last 90 days so a
            // followed topic shows what is hot now, not 2019's biggest match.
            // Quantized to a day: the fetch layer dedupes by URL, and a
            // cutoff that moved every second would defeat it on every redraw.
            let cutoff = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
                .saturating_sub(90 * 24 * 3600)
                / 86400
                * 86400;
            let url = format!(
                "https://hn.algolia.com/api/v1/search?query={}&tags=story&hitsPerPage=1\
                 &attributesToRetrieve=title,points&attributesToHighlight=none\
                 &numericFilters=created_at_i%3E{cutoff}",
                name.replace(' ', "%20")
            );
            let out = match vm.host.cx_mut().script_data_fetch(&url) {
                None => vm.host.cx_mut().script_data_placeholder(&url),
                Some(bytes) => {
                    json_pluck(&bytes, key).unwrap_or_else(|| "—".to_string())
                }
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    // sys.link("url") -> the page open in the host's reader overlay, "" when
    // closed. Not a fetch: the overlay is host state, published like locale.
    vm.add_method(
        sys,
        id_lut!(link),
        script_args_def!(field = NIL),
        |vm, _args| {
            let url = if crate::splash_policy::profile_allowed(vm.bx.heap.heap_key()) {
                LINK.read().map(|s| s.clone()).unwrap_or_default()
            } else {
                String::new()
            };
            vm.bx.heap.new_string_from_str(&url)
        },
    );

    // sys.places(lat, lon, "category", index, "field") -> a REAL nearby venue
    // from OpenStreetMap (Overpass API, keyless): row `index` (0 = nearest) of
    // the named places within 4 km, sorted by distance. THE LLM MUST CALL THIS
    // FOR EVERY VENUE NAME/DISTANCE — an invented "Riverside Park" that isn't
    // actually there destroys trust in the whole card; these are real mapped
    // venues. Category tokens (case-insensitive): park, garden, trail, museum,
    // cafe, cinema, gym, library, pool, viewpoint, playground — unknown tokens
    // fall back to park. Fields (case-insensitive):
    //   name     -> "Ryland Park"
    //   distance -> "0.7 km"   (from the request point, one decimal)
    //   lat|lon  -> "37.3423"  (4 decimals — chain into sys.basemap/sys.photo)
    //   count    -> total places found (ignores `index`)
    // Returns "—" while the (async) fetch loads or when index/field is out of
    // range; the card re-evaluates when data lands (same redraw semantics as
    // sys.weather). Unnamed OSM elements are skipped and duplicate names
    // deduped (one park is often mapped as several ways), so every index is a
    // distinct, nameable venue. ONE fetch (URL-deduped) serves all rows ×
    // fields of a list card.
    // sys.poi(lat, lon, "The Bund", "name"|"kind") -> a NAMED landmark near a
    // coordinate, resolved live.
    //
    // Distinct from both neighbours. `sys.geocode` is a gazetteer of POPULATED
    // places and returns nothing for a landmark (four of five Shanghai
    // attractions came back empty). `sys.places` answers "what is tagged
    // tourism=attraction nearest here", which is a different question -- sorted
    // by distance it offers a game centre and a tourist information office ahead
    // of the Bund. This one takes the NAME the card recommends and proves it
    // against live data: resolved, or "—".
    //
    // Overpass, not Nominatim. Nominatim answers the same question and works
    // from a laptop, but returns 403 to this device for every request even with
    // the identifying UA its policy demands -- measured, five of five. A 403 is
    // permanent, so every row went terminal at once. Overpass is the host
    // `sys.places` already proves reachable from here.
    vm.add_method(
        sys,
        id_lut!(poi),
        script_args_def!(lat = NIL, lon = NIL, name = NIL, field = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat).as_number().unwrap_or(0.0);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(0.0);
            let n_v = script_value!(vm, args.name);
            let mut name = String::new();
            vm.bx.heap.cast_to_string(n_v, &mut name);
            let f_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(f_v, &mut field);
            // Escape the regex/quote metacharacters Overpass would choke on.
            let safe: String = name
                .trim()
                .chars()
                .filter(|c| c.is_alphanumeric() || c.is_whitespace() || *c == '\'')
                .collect();
            let around = format!("around:12000,{lat:.4},{lon:.4}");
            let q = format!(
                "[out:json][timeout:25];nwr[\"name\"~\"{safe}\",i]({around});out center 5;"
            );
            let url = format!("{OVERPASS_URL}?data={}", percent_encode_query(&q));
            let out = match vm.host.cx_mut().script_data_fetch(&url) {
                None => vm.host.cx_mut().script_data_placeholder(&url),
                Some(bytes) => {
                    // The first NAMED element; Overpass returns ways and nodes
                    // for one feature and only some carry the tags.
                    let mut found = "—".to_string();
                    for i in 0..5 {
                        let tag = |k: &str| {
                            json_pluck(&bytes, &format!("elements.{i}.tags.{k}"))
                                .filter(|v| !v.is_empty())
                        };
                        let Some(nm) = tag("name") else { continue };
                        found = match field.trim().to_ascii_lowercase().as_str() {
                            "name" => nm,
                            "kind" => tag("tourism")
                                .or_else(|| tag("historic"))
                                .or_else(|| tag("leisure"))
                                .or_else(|| tag("amenity"))
                                .unwrap_or_else(|| "—".into())
                                .replace('_', " "),
                            other => tag(other).unwrap_or_else(|| "—".into()),
                        };
                        break;
                    }
                    found
                }
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    vm.add_method(
        sys,
        id_lut!(places),
        script_args_def!(lat = NIL, lon = NIL, category = NIL, index = NIL, field = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat).as_number().unwrap_or(0.0);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(0.0);
            let cat_v = script_value!(vm, args.category);
            let mut category = String::new();
            vm.bx.heap.cast_to_string(cat_v, &mut category);
            let index = script_value!(vm, args.index).as_number().unwrap_or(0.0).max(0.0) as usize;
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let url = overpass_places_url(lat, lon, &category);
            let out = match vm.host.cx_mut().script_data_fetch(&url) {
                None => vm.host.cx_mut().script_data_placeholder(&url),
                Some(bytes) => places_field(&bytes, lat, lon, index, field.trim()),
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    // sys.placesnum(lat, lon, "category") -> how many places sys.places would
    // list, as a NUMBER, so script conditions can branch on LIVE availability
    // BEFORE laying out rows — skip the "parks" section when none are mapped,
    // cap a list at the real row count, fall back to another category.
    // Returns -9999 while the fetch loads / on a bad response — guard with
    // `>= 0` (the same sentinel convention as sys.weathernum); a genuine
    // "nothing nearby" is 0. Shares sys.places' fetch (identical URL -> ONE
    // request serves both helpers).
    vm.add_method(
        sys,
        id_lut!(placesnum),
        script_args_def!(lat = NIL, lon = NIL, category = NIL),
        |vm, args| {
            let lat = script_value!(vm, args.lat).as_number().unwrap_or(0.0);
            let lon = script_value!(vm, args.lon).as_number().unwrap_or(0.0);
            let cat_v = script_value!(vm, args.category);
            let mut category = String::new();
            vm.bx.heap.cast_to_string(cat_v, &mut category);
            let url = overpass_places_url(lat, lon, &category);
            let n = match vm.host.cx_mut().script_data_fetch(&url) {
                None => -9999.0,
                Some(bytes) => match places_parse(&bytes, lat, lon) {
                    Some(places) => places.len() as f64,
                    None => -9999.0,
                },
            };
            ScriptValue::from_f64(n)
        },
    );

    // sys.search("<free text>", index, "name"|"label"|"lat"|"lon") -> the i-th
    // RESULT of a free-text place/POI/address search (Photon, keyless). This is
    // the Google-Maps "search a location" step: bind rows to it to show tappable
    // search results, then a row's tap writes the picked lat/lon/name into state
    // and the card routes there. "" while loading / past the last hit.
    vm.add_method(
        sys,
        id_lut!(search),
        script_args_def!(query = NIL, index = NIL, field = NIL),
        |vm, args| {
            let q_v = script_value!(vm, args.query);
            let mut query = String::new();
            vm.bx.heap.cast_to_string(q_v, &mut query);
            let index = script_value!(vm, args.index).as_number().unwrap_or(0.0).max(0.0) as usize;
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            // An empty query is not a search — photon answers it 400, and the
            // one thing that can come back is a terminal failure for nothing.
            if query.trim().is_empty() {
                return vm.bx.heap.new_string_from_str("");
            }
            let url = search_url(query.trim());
            let out = match vm.host.cx_mut().script_data_fetch(&url) {
                None => String::new(),
                Some(bytes) => search_field(&bytes, index, field.trim()),
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    // sys.searchnum("<free text>", index, "count"|"lat"|"lon") -> NUMBERS for
    // the search hits: "count" (index ignored) = how many hits, to guard/cap the
    // results list; "lat"/"lon" = hit `index`'s coordinate as a NUMBER, to feed
    // straight into sys.navroute/sys.route (sys.search returns strings, for
    // display). -9999 while loading / bad response / out of range (guard `>= 0`;
    // 0 hits = 0). Shares sys.search's fetch (identical URL -> ONE request).
    vm.add_method(
        sys,
        id_lut!(searchnum),
        script_args_def!(query = NIL, index = NIL, field = NIL),
        |vm, args| {
            let q_v = script_value!(vm, args.query);
            let mut query = String::new();
            vm.bx.heap.cast_to_string(q_v, &mut query);
            let index = script_value!(vm, args.index).as_number().unwrap_or(0.0).max(0.0) as usize;
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            if query.trim().is_empty() {
                return ScriptValue::from_f64(-9999.0);
            }
            let url = search_url(query.trim());
            let n = match vm.host.cx_mut().script_data_fetch(&url) {
                None => -9999.0,
                Some(bytes) => match search_parse(&bytes) {
                    None => -9999.0,
                    Some(hits) => match field.trim().to_ascii_lowercase().as_str() {
                        "lat" => hits.get(index).map(|h| h.lat).unwrap_or(-9999.0),
                        "lon" => hits.get(index).map(|h| h.lon).unwrap_or(-9999.0),
                        _ => hits.len() as f64, // "count" / default
                    },
                },
            };
            ScriptValue::from_f64(n)
        },
    );

    // sys.navroutenum(lat1, lon1, lat2, lon2, "km"|"min", vias) -> the route's
    // distance (km) or driving duration (min) as a NUMBER, so a card can derive
    // other travel modes (walk ≈ km*12 min, bike ≈ km*4 min) and arrival math.
    // Shares sys.navroute's cached fetch. -1 while loading.
    vm.add_method(
        sys,
        id_lut!(navroutenum),
        script_args_def!(
            lat1 = NIL,
            lon1 = NIL,
            lat2 = NIL,
            lon2 = NIL,
            field = NIL,
            vias = NIL
        ),
        |vm, args| {
            let lat1 = script_value!(vm, args.lat1).as_number().unwrap_or(0.0);
            let lon1 = script_value!(vm, args.lon1).as_number().unwrap_or(0.0);
            let lat2 = script_value!(vm, args.lat2).as_number().unwrap_or(0.0);
            let lon2 = script_value!(vm, args.lon2).as_number().unwrap_or(0.0);
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let vias_v = script_value!(vm, args.vias);
            let mut vias = String::new();
            vm.bx.heap.cast_to_string(vias_v, &mut vias);
            if !osrm_ready(lat1, lon1, lat2, lon2) || !osrm_vias_ready(&vias) {
                return vm.bx.heap.new_string_from_str("—");
            }
            let url = navroute_url(lat1, lon1, lat2, lon2, &vias);
            let n = match nav_route_cached(vm, &url) {
                Some(route) => match field.trim() {
                    "min" => route.total_s / 60.0,
                    _ => route.total_m / 1000.0,
                },
                None => -1.0,
            };
            ScriptValue::from_f64(n)
        },
    );

    // sys.coord("lat,lon|name", field) -> a field of a place a card stored in
    // one scalar state key (a picked search result / waypoint / origin), so it
    // survives across screens and feeds routing. Format: "lat,lon" or
    // "lat,lon|Display Name".
    //   "lat"/"lon" -> NUMBER (feeds sys.navroute/sys.route lat/lon args)
    //   "latlon"    -> clean "lat,lon" STRING (feeds the navroute `vias` arg)
    //   "name"      -> the display name STRING (after the '|'), or ""
    // lat/lon are -9999 when unparseable (guard `>= -900`).
    vm.add_method(
        sys,
        id_lut!(coord),
        script_args_def!(s = NIL, field = NIL),
        |vm, args| {
            let s_v = script_value!(vm, args.s);
            let mut s = String::new();
            vm.bx.heap.cast_to_string(s_v, &mut s);
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let (coords, name) = match s.trim().split_once('|') {
                Some((c, n)) => (c.trim(), n.trim()),
                None => (s.trim(), ""),
            };
            let mut it = coords.split(',');
            let lat = it.next().and_then(|p| p.trim().parse::<f64>().ok());
            let lon = it.next().and_then(|p| p.trim().parse::<f64>().ok());
            match field.trim().to_ascii_lowercase().as_str() {
                "name" => vm.bx.heap.new_string_from_str(name),
                "latlon" => {
                    let out = match (lat, lon) {
                        (Some(a), Some(o)) => format!("{a},{o}"),
                        _ => String::new(),
                    };
                    vm.bx.heap.new_string_from_str(&out)
                }
                "lon" => ScriptValue::from_f64(lon.unwrap_or(-9999.0)),
                _ => ScriptValue::from_f64(lat.unwrap_or(-9999.0)),
            }
        },
    );

    // sys.gps("lat"|"lon"|"acc"|"ok") -> the device's last-known GPS fix, read
    // SYNCHRONOUSLY from the platform global (NO network fetch — so, unlike
    // sys.search/navroute, it must NOT gate the card via body_binds_live_data).
    // lat/lon/acc are numbers, -9999 when there is no fix yet; "ok" is 1 when a
    // fix exists else 0. Cards guard with `sys.gps("ok") >= 1` before trusting
    // lat/lon (same sentinel idiom as sys.coord). Fed by the Android
    // LocationListener through JNI onLocation -> makepad_platform::gps.
    vm.add_method(
        sys,
        id_lut!(gps),
        script_args_def!(field = NIL),
        |vm, args| {
            #[cfg(target_env = "ohos")]
            vm.host.cx_mut().ohos_request_gps();
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            // Without the `location` grant this reads as "no fix yet", the
            // state every card already handles.
            let fix = crate::splash_policy::gps_fix_for_heap(vm.bx.heap.heap_key());
            match field.trim().to_ascii_lowercase().as_str() {
                "ok" => ScriptValue::from_f64(if fix.is_some() { 1.0 } else { 0.0 }),
                "lon" => ScriptValue::from_f64(fix.map(|f| f.lon).unwrap_or(-9999.0)),
                "acc" => ScriptValue::from_f64(fix.map(|f| f.acc as f64).unwrap_or(-9999.0)),
                _ => ScriptValue::from_f64(fix.map(|f| f.lat).unwrap_or(-9999.0)),
            }
        },
    );

    // sys.watchlist(index, "field") -> a row of the user's SAVED list (§5.12).
    //
    // The ticker comes from the durable store; every other field is fetched
    // live, through the same resolver `sys.stock` uses. That split is the whole
    // design: the store holds references and the screen shows current values, so
    // a saved list can never render a stale price.
    //
    // `sys.watchlistnum()` is the row count, so a card can realize the right
    // number of rows before any fetch lands.
    vm.add_method(
        sys,
        id_lut!(watchlist),
        script_args_def!(index = NIL, field = NIL),
        |vm, args| {
            let index = script_value!(vm, args.index)
                .as_number()
                .unwrap_or(0.0)
                .max(0.0) as usize;
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let Some(ticker) = collection_at(vm.bx.heap.heap_key(), "watchlist", index) else {
                // Past the end of the list. Empty rather than an em dash: a row
                // that does not exist is not a row whose value failed to load.
                return vm.bx.heap.new_string_from_str("");
            };
            // The one field that needs no network — it IS what was stored.
            if matches!(field.trim(), "symbol" | "ticker") {
                return vm.bx.heap.new_string_from_str(&ticker);
            }
            let sym = sanitize_ticker(&ticker);
            let url = format!(
                "https://query1.finance.yahoo.com/v8/finance/chart/{sym}?interval=1d&range=1d"
            );
            let out = match vm.host.cx_mut().script_data_fetch(&url) {
                None => vm.host.cx_mut().script_data_placeholder(&url),
                Some(bytes) => yahoo_chart_field(&bytes, &field),
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    // sys.symbol_search("nvid", index, "field") -> a TICKER matching free text.
    //
    // The other half of a saved list: without it a user can only keep what the
    // movers list happens to show. Fields are `symbol`, `name`, `exchange`,
    // `kind` — the last because this returns equities, ETFs and crypto for the
    // same query, and a card that cannot say which is offering someone a coin
    // when they asked for a company.
    //
    // `longname` is null for many listings, verified against the live response,
    // so `name` falls back to the short one rather than rendering an em dash for
    // a company that plainly has a name.
    // sys.cities(index, "field") -> a row of the user's SAVED places (§5.12).
    //
    // The store holds a NAME and nothing else — the purest form of the rule that
    // a durable collection keeps references, never facts. Coordinates and every
    // reading are resolved here: geocode the name, then read the weather at what
    // comes back. Two chained fetches, each URL-cached, so a saved city costs one
    // geocode ever and one forecast per refresh.
    //
    // Storing `name|lat|lon` was the first design and is what the card cannot
    // produce: L0 has no way to build a composite value, which is the point of
    // it having no expression form. The language was right and the helper was
    // wrong.
    vm.add_method(
        sys,
        id_lut!(cities),
        script_args_def!(index = NIL, field = NIL, unit = NIL),
        |vm, args| {
            let index = script_value!(vm, args.index)
                .as_number()
                .unwrap_or(0.0)
                .max(0.0) as usize;
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let unit_value = script_value!(vm, args.unit);
            let mut unit = String::new();
            if unit_value.is_nil() {
                unit.push('c');
            } else {
                vm.bx.heap.cast_to_string(unit_value, &mut unit);
            }
            let Some(name) = collection_at(vm.bx.heap.heap_key(), "cities", index) else {
                return vm.bx.heap.new_string_from_str("");
            };
            // The one field that needs no network: it IS what was stored.
            if field.trim() == "name" {
                return vm.bx.heap.new_string_from_str(&name);
            }
            let geo = geocode_url(&name, vm.bx.heap.heap_key());
            let (lat, lon) = match vm.host.cx_mut().script_data_fetch(&geo) {
                None => {
                    let placeholder = vm.host.cx_mut().script_data_placeholder(&geo);
                    return vm.bx.heap.new_string_from_str(&placeholder);
                }
                Some(bytes) => {
                    let num = |k: &str| {
                        json_pluck(&bytes, k).and_then(|v| v.parse::<f64>().ok())
                    };
                    match (num("results.0.latitude"), num("results.0.longitude")) {
                        (Some(a), Some(o)) => (a, o),
                        // The name resolved to nothing. An em dash says so; a
                        // reading at 0,0 would be a plausible number for the
                        // Gulf of Guinea.
                        _ => return vm.bx.heap.new_string_from_str("\u{2014}"),
                    }
                }
            };
            if field.trim() == "lat" {
                return vm.bx.heap.new_string_from_str(&format!("{lat:.4}"));
            }
            if field.trim() == "lon" {
                return vm.bx.heap.new_string_from_str(&format!("{lon:.4}"));
            }
            // The card's field names, as open-meteo's own paths.
            let path = match field.trim() {
                "temp" => "current.temperature_2m",
                "cond" => "current.weather_code",
                "humidity" => "current.relative_humidity_2m",
                "wind" => "current.wind_speed_10m",
                "feels" => "current.apparent_temperature",
                "hi" => "daily.temperature_2m_max.0",
                "lo" => "daily.temperature_2m_min.0",
                other => other,
            };
            let url = format!(
                "https://api.open-meteo.com/v1/forecast?latitude={lat:.4}&longitude={lon:.4}\
&current=temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,wind_speed_10m\
&daily=temperature_2m_max,temperature_2m_min&timezone=auto&forecast_days=1"
            );
            let out = match vm.host.cx_mut().script_data_fetch(&url) {
                None => vm.host.cx_mut().script_data_placeholder(&url),
                Some(bytes) => city_weather_value(&bytes, field.trim(), path, unit.trim()),
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    vm.add_method(
        sys,
        id_lut!(citiesnum),
        script_args_def!(),
        |vm, _args| ScriptValue::from_f64(collection_len_for(vm.bx.heap.heap_key(), "cities") as f64),
    );

    vm.add_method(
        sys,
        id_lut!(symbol_search),
        script_args_def!(query = NIL, index = NIL, field = NIL),
        |vm, args| {
            let q_v = script_value!(vm, args.query);
            let mut query = String::new();
            vm.bx.heap.cast_to_string(q_v, &mut query);
            let index = script_value!(vm, args.index)
                .as_number()
                .unwrap_or(0.0)
                .max(0.0) as usize;
            let field_v = script_value!(vm, args.field);
            let mut field = String::new();
            vm.bx.heap.cast_to_string(field_v, &mut field);
            let q = query.trim();
            if q.is_empty() {
                // No query is not a failed search. Empty, so a card can branch
                // on it without an em dash claiming something went wrong.
                return vm.bx.heap.new_string_from_str("");
            }
            let url = format!(
                "https://query1.finance.yahoo.com/v1/finance/search?q={}&quotesCount=10&newsCount=0",
                percent_encode_query(q)
            );
            let base = format!("quotes.{index}");
            let out = match vm.host.cx_mut().script_data_fetch(&url) {
                None => vm.host.cx_mut().script_data_placeholder(&url),
                Some(bytes) => {
                    let raw = |k: &str| json_pluck(&bytes, &format!("{base}.{k}"));
                    match field.trim().to_ascii_lowercase().as_str() {
                        "symbol" | "ticker" => raw("symbol").unwrap_or_default(),
                        "name" => raw("longname")
                            .filter(|s| !s.is_empty() && s != "null")
                            .or_else(|| raw("shortname"))
                            .unwrap_or_default(),
                        "exchange" => raw("exchDisp").unwrap_or_default(),
                        "kind" => raw("typeDisp").unwrap_or_default(),
                        other => raw(other).unwrap_or_default(),
                    }
                }
            };
            vm.bx.heap.new_string_from_str(&out)
        },
    );

    vm.add_method(
        sys,
        id_lut!(symbol_searchnum),
        script_args_def!(query = NIL),
        |vm, args| {
            let q_v = script_value!(vm, args.query);
            let mut query = String::new();
            vm.bx.heap.cast_to_string(q_v, &mut query);
            let q = query.trim();
            if q.is_empty() {
                return ScriptValue::from_f64(0.0);
            }
            let url = format!(
                "https://query1.finance.yahoo.com/v1/finance/search?q={}&quotesCount=10&newsCount=0",
                percent_encode_query(q)
            );
            let n = vm
                .host
                .cx_mut()
                .script_data_fetch(&url)
                .and_then(|bytes| json_pluck(&bytes, "count"))
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            ScriptValue::from_f64(n)
        },
    );

    vm.add_method(
        sys,
        id_lut!(watchlistnum),
        script_args_def!(),
        |vm, _args| ScriptValue::from_f64(collection_len_for(vm.bx.heap.heap_key(), "watchlist") as f64),
    );

    vm.set_injected_global(id!(sys), sys.into());
}

/// The user's durable collections, as the store last reported them (§5.12).
///
/// A map because there is more than one: a watchlist of tickers, a list of
/// saved cities, whatever the next store-backed capability names. Keyed by the
/// capability's own name, so two cards binding `sys.watchlist` under different
/// local names reach the same list.
///
/// The APP writes this and this crate reads it — the same shape `sys.gps` uses
/// for the platform's location fix. The app owns the file and the schema, this
/// crate owns the fetching, and neither depends on the other. What crosses is a
/// list of REFERENCES: never a price, never a temperature.
static COLLECTIONS: std::sync::RwLock<Option<std::collections::BTreeMap<String, Vec<String>>>> =
    std::sync::RwLock::new(None);

/// The device's locale, and the user's stored single-value preferences.
///
/// Same contract as `COLLECTIONS`: the APP writes, this crate reads, and what
/// crosses is never a fetched fact. A locale is what the device is set to and a
/// preference is what the user chose — both are references, both are the user's.
static LOCALE: std::sync::RwLock<Option<(String, String)>> = std::sync::RwLock::new(None);
static PREFS: std::sync::RwLock<Option<std::collections::BTreeMap<String, String>>> =
    std::sync::RwLock::new(None);

/// Publish the device locale as `(lang, temp_unit)`.
pub fn set_locale(lang: &str, temp_unit: &str) {
    if let Ok(mut slot) = LOCALE.write() {
        *slot = Some((lang.to_owned(), temp_unit.to_owned()));
    }
}

/// Publish the user's single-value preferences.
pub fn set_prefs(map: std::collections::BTreeMap<String, String>) {
    if let Ok(mut slot) = PREFS.write() {
        *slot = Some(map);
    }
}

/// `en` and `c` until the app says otherwise.
///
/// A DEFAULT, not an em dash. The other capabilities answer "—" while a fetch is
/// in flight because a temperature nobody has measured must not be guessed; a
/// locale is not fetched and not a measurement, and a card that cannot read one
/// has no language to render its own labels in.
fn locale_field(field: &str) -> String {
    let held = LOCALE.read().ok().and_then(|l| l.clone());
    let (lang, unit) = held.unwrap_or_else(|| ("en".to_owned(), "c".to_owned()));
    match field {
        "temp_unit" => unit,
        _ => lang,
    }
}

fn pref_at(heap_key: usize, field: &str) -> Option<String> {
    if !crate::splash_policy::profile_allowed(heap_key) {
        return None;
    }
    PREFS.read().ok()?.as_ref()?.get(field).cloned()
}

/// The page currently open in the host's reader overlay — "" when closed.
/// Same contract as LOCALE/PREFS: the app writes, this crate reads.
static LINK: std::sync::RwLock<String> = std::sync::RwLock::new(String::new());

pub fn set_link(url: &str) {
    if let Ok(mut slot) = LINK.write() {
        *slot = url.to_owned();
    }
}

/// Publish the stored references. Called on load and after every write.
pub fn set_collections(map: std::collections::BTreeMap<String, Vec<String>>) {
    if let Ok(mut slot) = COLLECTIONS.write() {
        *slot = Some(map);
    }
}

/// How many entries a collection holds, for `sys.<name>num()` — so a card can
/// realize the right number of rows before any fetch lands.
pub fn collection_len(name: &str) -> usize {
    COLLECTIONS
        .read()
        .ok()
        .and_then(|m| m.as_ref().and_then(|m| m.get(name).map(|v| v.len())))
        .unwrap_or(0)
}

/// [`collection_len`] as a given isolate may see it: empty without the
/// `profile` grant, like a user who saved nothing.
fn collection_len_for(heap_key: usize, name: &str) -> usize {
    if crate::splash_policy::profile_allowed(heap_key) {
        collection_len(name)
    } else {
        0
    }
}

fn collection_at(heap_key: usize, name: &str, index: usize) -> Option<String> {
    if !crate::splash_policy::profile_allowed(heap_key) {
        return None;
    }
    COLLECTIONS
        .read()
        .ok()?
        .as_ref()?
        .get(name)?
        .get(index)
        .cloned()
}


/// One field of a Yahoo *chart* response, in this profile's own field names.
///
/// Lifted out of `sys.stock` so `sys.watchlist` answers identically. The two
/// differ only in where the ticker comes from — an argument, or the user's
/// stored list — and a second copy of this match is how they would drift into
/// answering the same question two different ways.
fn yahoo_chart_field(bytes: &[u8], field: &str) -> String {
    // Did Yahoo actually answer with a chart? A refusal still arrives as a
    // complete HTTP response with a JSON body, so the fetch layer hands it over
    // as success and every field below falls back to an em dash — which the host
    // reads as "still fetching" (`state_of_answer` in the app's l0_card.rs), not
    // as a failure. Measured on the 6T: Yahoo answered `429 Too Many Requests`
    // and the card sat on "Fetching the quote…" indefinitely, which is the one
    // thing §5.9's two states exist to keep apart. `n/a` is the failed sentinel,
    // so the card renders its own `.failed` copy instead.
    if json_pluck(bytes, "chart.result.0.meta.symbol").is_none() {
        return "n/a".into();
    }
    let m = |k: &str| format!("chart.result.0.meta.{k}");
                let num = |k: &str| json_pluck(&bytes, &m(k)).and_then(|s| s.parse::<f64>().ok());
                // Monetary fields formatted to a consistent 2 decimals (Yahoo
                // returns e.g. 201.5, which otherwise breaks the visual rhythm).
                let money = |k: &str| num(k).map(|v| format!("{v:.2}")).unwrap_or_else(|| "—".into());
                match field.trim().to_ascii_lowercase().as_str() {
                    "change" => match (num("regularMarketPrice"), num("chartPreviousClose")) {
                        (Some(p), Some(c)) => format!("{:+.2}", p - c),
                        _ => "—".to_string(),
                    },
                    // The same number with the currency INSIDE the sign.
                    //
                    // `signed_money` renders `+$7.13`, and a lowering can only
                    // prepend a prefix to what this returns — `"$" + "+7.13"` is
                    // `$+7.13`, which is wrong, so the card fell back to its
                    // seeded value instead. That put a fixture's `+$3.10` beside
                    // a live `+3.55%` on the same line, two numbers describing
                    // the same move and disagreeing. Composing it here is the
                    // only place the sign and the symbol can be ordered.
                    "changemoney" => match (num("regularMarketPrice"), num("chartPreviousClose")) {
                        (Some(p), Some(c)) => {
                            let d = p - c;
                            format!("{}${:.2}", if d < 0.0 { "-" } else { "+" }, d.abs())
                        }
                        _ => "—".to_string(),
                    },
                    "changepct" | "changepercent" => {
                        match (num("regularMarketPrice"), num("chartPreviousClose")) {
                            (Some(p), Some(c)) if c != 0.0 => format!("{:+.2}%", (p - c) / c * 100.0),
                            _ => "—".to_string(),
                        }
                    }
                    "price" => money("regularMarketPrice"),
                    "prev" | "prevclose" => money("chartPreviousClose"),
                    "high" => money("regularMarketDayHigh"),
                    "low" => money("regularMarketDayLow"),
                    // The session open is NOT in `meta`. Verified against the
                    // live response: `regularMarketOpen` and `previousClose`
                    // are both absent, while `regularMarketPrice`,
                    // `…DayHigh`, `…DayLow` and `chartPreviousClose` are
                    // present. Reading it from `meta` returned an em dash, so
                    // the card fell back to its seeded value and drew a
                    // fixture price beside two live ones — a $181 open under
                    // a $207 price on a +3% day, which does not add up and
                    // was the only thing on screen that said so.
                    //
                    // It IS in the bar series. This URL is fixed at
                    // `interval=1d&range=1d`, so there is exactly one bar and
                    // its open is today's; widening the range here would make
                    // `open.0` the first bar of the RANGE instead.
                    "open" => json_pluck(&bytes, "chart.result.0.indicators.quote.0.open.0")
                        .and_then(|s| s.parse::<f64>().ok())
                        .map(|v| format!("{v:.2}"))
                        .unwrap_or_else(|| "—".into()),
                    "currency" => json_pluck(&bytes, &m("currency")).unwrap_or_else(|| "—".into()),
                    "name" => json_pluck(&bytes, &m("longName"))
                        .or_else(|| json_pluck(&bytes, &m("shortName")))
                        .unwrap_or_else(|| "—".into()),
                    "symbol" => json_pluck(&bytes, &m("symbol")).unwrap_or_else(|| "—".into()),
                    "exchange" => json_pluck(&bytes, &m("fullExchangeName")).unwrap_or_else(|| "—".into()),
                    "52wh" | "yearhigh" => money("fiftyTwoWeekHigh"),
                    "52wl" | "yearlow" => money("fiftyTwoWeekLow"),
                    "vol" | "volume" => match num("regularMarketVolume") {
                        Some(v) if v >= 1e9 => format!("{:.2}B", v / 1e9),
                        Some(v) if v >= 1e6 => format!("{:.1}M", v / 1e6),
                        Some(v) if v >= 1e3 => format!("{:.1}K", v / 1e3),
                        Some(v) => format!("{v:.0}"),
                        None => "—".to_string(),
                    },
                    // Day-range position 0..100 (where price sits low→high), for a range bar.
                    "rangepct" => match (
                        num("regularMarketPrice"),
                        num("regularMarketDayLow"),
                        num("regularMarketDayHigh"),
                    ) {
                        (Some(p), Some(lo), Some(hi)) if hi > lo => {
                            format!("{:.0}", (((p - lo) / (hi - lo)) * 100.0).clamp(0.0, 100.0))
                        }
                        _ => "50".to_string(),
                    },
                    other => json_pluck(&bytes, other).unwrap_or_else(|| "—".into()),
                }
}

/// True if a Splash body calls any live-data helper (sys.weather/airquality/
/// stock/news/movers/places). Such cards must re-evaluate when their async
/// fetch lands (the value is baked into a Label at eval time), so we arm the
/// frame pump + watch the data-fetch epoch for them. Keep in sync with the
/// data `sys.*` helpers. (Substring matches also cover the -num variants:
/// "sys.weather" matches sys.weathernum, "sys.places" matches sys.placesnum.)
fn body_binds_live_data(body: &str) -> bool {
    body.contains("sys.weather")
        || body.contains("sys.airquality")
        // `sys.aqinum` shares no prefix with `sys.airquality` — without its own
        // check an aqinum-only card would never re-evaluate when its fetch lands.
        || body.contains("sys.aqinum")
        || body.contains("sys.stock")
        || body.contains("sys.news")
        || body.contains("sys.movers")
        // substring covers sys.quakesnum too (same trick as weather/weathernum)
        || body.contains("sys.quakes")
        || body.contains("sys.places")
        // covers sys.search + sys.searchnum — the search-results card must
        // re-evaluate once the free-text search fetch lands
        || body.contains("sys.search")
        // `sys.symbol_search` does NOT contain `sys.search` — the prefix trick
        // the rest of this list leans on does not reach it, and a search card
        // that never re-evaluates shows an empty result list forever.
        || body.contains("sys.symbol_search")
        // A saved row's values are fetched per ticker, so the list must
        // re-evaluate when those land (§5.12).
        || body.contains("sys.watchlist")
        // substring covers sys.geocodenum too (same trick as weather/weathernum)
        || body.contains("sys.geocode")
        || body.contains("sys.route")
        // covers sys.navroute/navstep/navstepnum — the nav card's body must
        // re-evaluate ONCE when the OSRM fetch lands (fills nav_polyline)
        || body.contains("sys.nav")
        // covers sys.citytimenum too — the clock card's body must re-evaluate
        // once the tz-offset fetch lands (fn tick() then keeps it current)
        || body.contains("sys.citytime")
        // covers sys.fxnum too (sys.fmtnum does NOT match this prefix)
        || body.contains("sys.fx")
}

/// Height (dp) of bar `index` of `count` for an intraday sparkline, from Yahoo's
/// 5-minute close series, normalized so the day's min→~8dp and max→`maxh`+8dp
/// (so a chart of pixel height `maxh`+8 shows the full range without clipping).
/// Map a card-facing range token to Yahoo chart-API `(range, interval)` params.
/// Tokens (case-insensitive): "1d" (default), "1w", "1m", "6m", "1y" — the five
/// chips an iOS-Stocks-style card shows. Empty/unknown tokens (e.g. an unset
/// `{{state.range}}` rendering as "") fall back to the intraday default so a
/// card is never blank because of a bad token.
fn yahoo_range_params(token: &str) -> (&'static str, &'static str) {
    match token.trim().to_ascii_lowercase().as_str() {
        "1w" | "5d" => ("5d", "30m"),
        "1m" | "1mo" => ("1mo", "1d"),
        "6m" | "6mo" => ("6mo", "1d"),
        "1y" => ("1y", "1wk"),
        _ => ("1d", "5m"),
    }
}

/// Sanitize a card-supplied ticker into a safe URL path segment. Card bodies
/// are LLM-generated (semi-trusted): a symbol containing `?`/`#`/`/`/`%`
/// would rewrite the request target (and split the fetch-dedup key), and a
/// `\0` — which Splash string literals CAN carry — reaches the Android HTTP
/// layer's `CString::new(url).unwrap()` and aborts the process. Keep only the
/// characters real Yahoo tickers use (letters, digits, `.` `-` `^` `=`),
/// uppercased, capped at 16.
pub(crate) fn sanitize_ticker(symbol: &str) -> String {
    let out: String = symbol
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '^' | '='))
        .map(|c| c.to_ascii_uppercase())
        .take(16)
        .collect();
    // A real ticker has at least one letter/digit. Reject a result that is
    // empty or all-punctuation (e.g. ".." — which a URL canonicalizer would
    // resolve as a path-traversal segment, rewriting the request target). The
    // callers treat "" as "no symbol" and skip the fetch.
    if out.chars().any(|c| c.is_ascii_alphanumeric()) {
        out
    } else {
        String::new()
    }
}

/// The ONE Yahoo chart-API URL for a symbol×range. `sys.stockbar`,
/// `sys.stockrange` and the `StockPlot` widget all build their URL here, so
/// they share a single `script_data_fetch` cache entry — one request per
/// symbol×range serves the plot, the bars and every scalar on the card.
pub(crate) fn yahoo_chart_url(symbol: &str, range: &str) -> String {
    let sym = sanitize_ticker(symbol);
    let (yr, yi) = yahoo_range_params(range);
    format!("https://query1.finance.yahoo.com/v8/finance/chart/{sym}?interval={yi}&range={yr}")
}

/// Range-aware scalar for `sys.stockrange`, computed from the SAME Yahoo close
/// series the chart bars draw: the range's high/low extremes and the range's
/// own first→last change. Returns "—" when the series is absent/short so the
/// card shows the standard loading placeholder.
fn stock_range_field(bytes: &[u8], field: &str) -> String {
    let root: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(_) => return "—".to_string(),
    };
    let vals: Vec<f64> = match root
        .pointer("/chart/result/0/indicators/quote/0/close")
        .and_then(|c| c.as_array())
    {
        Some(a) => a.iter().filter_map(|x| x.as_f64()).collect(),
        None => return "—".to_string(),
    };
    if vals.is_empty() {
        return "—".to_string();
    }
    let mn = vals.iter().cloned().fold(f64::INFINITY, f64::min);
    let mx = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let (first, last) = (vals[0], vals[vals.len() - 1]);
    match field.to_ascii_lowercase().as_str() {
        "high" => format!("{mx:.2}"),
        "low" => format!("{mn:.2}"),
        "change" => format!("{:+.2}", last - first),
        "changepct" | "changepercent" => {
            if first != 0.0 {
                format!("{:+.2}%", (last - first) / first * 100.0)
            } else {
                "—".to_string()
            }
        }
        "up" => (if last >= first { "1" } else { "0" }).to_string(),
        _ => "—".to_string(),
    }
}

fn stock_bar_height(bytes: &[u8], index: usize, count: usize, maxh: f64) -> f64 {
    let span = (maxh - 8.0).max(8.0);
    let root: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(_) => return 6.0,
    };
    let arr = match root
        .pointer("/chart/result/0/indicators/quote/0/close")
        .and_then(|c| c.as_array())
    {
        Some(a) => a,
        None => return 6.0,
    };
    let vals: Vec<f64> = arr.iter().filter_map(|x| x.as_f64()).collect();
    if vals.len() < 2 {
        return 6.0;
    }
    let mn = vals.iter().cloned().fold(f64::INFINITY, f64::min);
    let mx = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    if mx <= mn {
        return 24.0;
    }
    let denom = (count.max(2) - 1) as f64;
    let pos = ((index as f64 / denom) * (vals.len() - 1) as f64).round() as usize;
    let val = vals[pos.min(vals.len() - 1)];
    8.0 + (val - mn) / (mx - mn) * span
}

/// Extract a scalar from an open-meteo JSON body at a dot-path, formatted for
/// USGS live feed backing sys.quakes/sys.quakesnum: all M2.5+ earthquakes in
/// the last 24 h as GeoJSON, newest first (keyless, ~50-200 KB).
const QUAKES_FEED_URL: &str =
    "https://earthquake.usgs.gov/earthquakes/feed/v1.0/summary/2.5_day.geojson";

/// Pluck one DISPLAY field for quake `idx` out of the raw USGS GeoJSON bytes.
/// GeoJSON layout: features[i].properties.{mag,place,time(ms)} and
/// features[i].geometry.coordinates = [lon, lat, depth_km].
fn quake_field(bytes: &[u8], idx: i64, field: &str) -> String {
    let f = field.to_ascii_lowercase();
    let out = match f.as_str() {
        "mag" | "magnitude" => json_pluck(bytes, &format!("features.{idx}.properties.mag"))
            .and_then(|s| s.parse::<f64>().ok())
            .map(|m| format!("{m:.1}")),
        "depth" => json_pluck(bytes, &format!("features.{idx}.geometry.coordinates.2"))
            .and_then(|s| s.parse::<f64>().ok())
            .map(|d| format!("{d:.0} km")),
        "lat" => json_pluck(bytes, &format!("features.{idx}.geometry.coordinates.1"))
            .and_then(|s| s.parse::<f64>().ok())
            .map(|v| format!("{v:.4}")),
        "lon" => json_pluck(bytes, &format!("features.{idx}.geometry.coordinates.0"))
            .and_then(|s| s.parse::<f64>().ok())
            .map(|v| format!("{v:.4}")),
        // Humanized age from the epoch-ms event time ("now", "12m ago", "3h ago").
        "time" | "ago" => json_pluck(bytes, &format!("features.{idx}.properties.time"))
            .and_then(|s| s.parse::<f64>().ok())
            .map(|ms| {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as f64)
                    .unwrap_or(ms);
                let mins = ((now_ms - ms) / 60000.0).max(0.0) as i64;
                if mins < 1 {
                    "now".to_string()
                } else if mins < 60 {
                    format!("{mins}m ago")
                } else if mins < 48 * 60 {
                    format!("{}h ago", mins / 60)
                } else {
                    format!("{}d ago", mins / (60 * 24))
                }
            }),
        "count" => json_pluck(bytes, "metadata.count"),
        // Default (incl. "place"): the human-readable location string.
        _ => json_pluck(bytes, &format!("features.{idx}.properties.place")),
    };
    out.unwrap_or_else(|| "—".to_string())
}

/// The numeric twin behind sys.quakesnum. -9999 while absent (sentinel shared
/// with sys.weathernum so cards can guard with `>= -9998`).
fn quake_num(bytes: &[u8], idx: i64, field: &str) -> f64 {
    let f = field.to_ascii_lowercase();
    let path = match f.as_str() {
        "depth" => format!("features.{idx}.geometry.coordinates.2"),
        "lat" => format!("features.{idx}.geometry.coordinates.1"),
        "lon" => format!("features.{idx}.geometry.coordinates.0"),
        "time" => format!("features.{idx}.properties.time"),
        "count" => "metadata.count".to_string(),
        // Default (incl. "mag").
        _ => format!("features.{idx}.properties.mag"),
    };
    json_pluck(bytes, &path)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(-9999.0)
}

/// display. A numeric path segment indexes into an array; other segments are
/// object keys. Returns None if the path is absent or the leaf isn't a scalar.
/// ISO datetimes ("2026-07-13T05:52", as open-meteo returns for sunrise/sunset)
/// are shortened to "HH:MM".
fn json_pluck(bytes: &[u8], path: &str) -> Option<String> {
    let root: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let mut cur = &root;
    for seg in path.split('.') {
        cur = if let Ok(idx) = seg.parse::<usize>() {
            cur.get(idx)?
        } else {
            cur.get(seg)?
        };
    }
    let s = match cur {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => return None,
    };
    // open-meteo ISO datetime -> HH:MM (sunrise/sunset).
    if s.len() >= 16 && s.as_bytes().get(10) == Some(&b'T') {
        return Some(s[11..16].to_string());
    }
    Some(s)
}

/// Howard Hinnant's `days_from_civil` — the inverse of `civil_from_days`.
/// (year, month, day) → days since the Unix epoch.
fn days_from_civil(y: i64, m: u64, d: u64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// Weekday for a days-since-epoch count, 0 = Sunday. 1970-01-01 was a Thursday,
/// hence the +4.
fn weekday_from_days(z: i64) -> usize {
    (((z + 4) % 7 + 7) % 7) as usize
}

/// Abbreviated weekday names, index 0 = Sunday.
const DAY_EN: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const DAY_ZH: [&str; 7] = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];

/// Abbreviated month names, index 0 = January (for sys.citytime "date").
const MONTH_EN: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The minimal open-meteo request that still carries `utc_offset_seconds` +
/// `timezone` — sys.citytime's ONE cached fetch per place. Deliberately a
/// DIFFERENT URL from sys.weather's so a pure clock card downloads ~300 bytes,
/// not a 7-day forecast (they cache separately; a card using both fetches both).
fn tz_offset_url(lat: f64, lon: f64) -> String {
    format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat:.4}&longitude={lon:.4}\
&timezone=auto&forecast_days=1"
    )
}

/// The DST-correct UTC offset (seconds) out of an open-meteo response.
fn tz_offset_secs(bytes: &[u8]) -> Option<i64> {
    json_pluck(bytes, "utc_offset_seconds")
        .and_then(|s| s.parse::<f64>().ok())
        .map(|v| v as i64)
}

/// Split place-local unix seconds into (hour 0-23, minute, second).
fn hms_from_secs(local: i64) -> (i64, i64, i64) {
    let sod = local.rem_euclid(86_400);
    (sod / 3600, (sod / 60) % 60, sod % 60)
}

/// Render one sys.citytime field from place-local unix seconds + the offset.
fn format_city_time(local: i64, off: i64, field: &str) -> String {
    let (h, m, s) = hms_from_secs(local);
    let days = local.div_euclid(86_400);
    match field {
        "hms" => format!("{h:02}:{m:02}:{s:02}"),
        "h12" => {
            let x = h % 12;
            let h12 = if x == 0 { 12 } else { x };
            let ap = if h < 12 { "AM" } else { "PM" };
            format!("{h12}:{m:02} {ap}")
        }
        "day" => DAY_EN[weekday_from_days(days)].to_string(),
        "day_zh" => DAY_ZH[weekday_from_days(days)].to_string(),
        "date" => {
            let (_y, mo, d) = civil_from_days(days);
            format!("{} {}", MONTH_EN[(mo as usize).clamp(1, 12) - 1], d)
        }
        "date_zh" => {
            let (_y, mo, d) = civil_from_days(days);
            format!("{mo}月{d}日")
        }
        "offset" => {
            let sign = if off < 0 { "-" } else { "+" };
            let a = off.abs();
            let (oh, om) = (a / 3600, (a / 60) % 60);
            if om == 0 {
                format!("UTC{sign}{oh}")
            } else {
                format!("UTC{sign}{oh}:{om:02}")
            }
        }
        // "hm" and anything unrecognized: the clock's bread-and-butter form.
        _ => format!("{h:02}:{m:02}"),
    }
}

/// ONE cached FX fetch per currency PAIR — Yahoo's intraday FX chart, the
/// same host + browser UA the stock helpers already use (proven on-device).
/// NOTE: the Cloudflare-fronted keyless FX APIs (api.frankfurter.dev,
/// open.er-api.com) both HANG from the Android TLS stack (no response, no
/// error, no timeout) while working from desktop — do not switch back
/// without on-device proof.
fn fx_url(from: &str, to: &str) -> String {
    format!("https://query1.finance.yahoo.com/v8/finance/chart/{from}{to}=X")
}

/// Where the live rate lives in Yahoo's chart response.
const FX_RATE_PATH: &str = "chart.result.0.meta.regularMarketPrice";

/// Display precision for an FX rate: 156.68, 7.253, 0.8669.
fn format_fx_rate(r: f64) -> String {
    if r >= 100.0 {
        format!("{r:.2}")
    } else if r >= 10.0 {
        format!("{r:.3}")
    } else {
        format!("{r:.4}")
    }
}

/// Reduce a 7-element `daily.*` temperature array to its min or max.
///
/// `round_display` is deliberately NOT applied: this feeds a shader uniform, where
/// the extra precision is free and rounding the span would visibly quantise the
/// bar colours.
fn week_extreme(
    vm: &mut ScriptVm,
    lat: f64,
    lon: f64,
    path: &str,
    want_max: bool,
) -> Option<f64> {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat:.4}&longitude={lon:.4}\
&current=temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,wind_speed_10m,surface_pressure,is_day\
&daily=weather_code,temperature_2m_max,temperature_2m_min,sunrise,sunset,uv_index_max,precipitation_probability_max\
&timezone=auto&forecast_days=7"
    );
    let bytes = vm.host.cx_mut().script_data_fetch(&url)?;
    let mut acc: Option<f64> = None;
    for i in 0..7 {
        let Some(v) = json_pluck(&bytes, &format!("{path}.{i}")) else {
            continue;
        };
        let Ok(n) = v.parse::<f64>() else { continue };
        acc = Some(match acc {
            None => n,
            Some(a) if want_max => a.max(n),
            Some(a) => a.min(n),
        });
    }
    acc
}

/// Mean synodic month — new moon to new moon — in seconds.
const SYNODIC_SECS: f64 = 29.530_588_853 * 86_400.0;

/// A known new moon: 2000-01-06 18:14 UTC, the epoch conventionally used for
/// simple phase arithmetic.
const NEW_MOON_EPOCH: f64 = 947_182_440.0;

/// Position in the synodic cycle, 0..1: 0 new, 0.25 first quarter, 0.5 full,
/// 0.75 last quarter.
///
/// This is the MEAN cycle, not a full lunar theory: the true phase wanders by up
/// to about half a day because the Moon's orbit is elliptical. That is invisible
/// on a rendered disc and in an illumination percentage rounded to units, which
/// is all this feeds, and it avoids pulling an ephemeris into the widget crate.
fn moon_phase_fraction() -> f64 {
    let elapsed = now_unix_secs() as f64 - NEW_MOON_EPOCH;
    let f = (elapsed % SYNODIC_SECS) / SYNODIC_SECS;
    if f < 0.0 {
        f + 1.0
    } else {
        f
    }
}

/// The principal-phase name for a point in the cycle. The four exact phases (new,
/// quarters, full) name a narrow window around the instant; the rest of the cycle
/// is crescent or gibbous, waxing before full and waning after.
fn moon_phase_name(f: f64) -> &'static str {
    if f < 0.0335 || f >= 0.9665 {
        "New Moon"
    } else if f < 0.2165 {
        "Waxing Crescent"
    } else if f < 0.2835 {
        "First Quarter"
    } else if f < 0.4665 {
        "Waxing Gibbous"
    } else if f < 0.5335 {
        "Full Moon"
    } else if f < 0.7165 {
        "Waning Gibbous"
    } else if f < 0.7835 {
        "Last Quarter"
    } else {
        "Waning Crescent"
    }
}

/// The principal-phase name in Chinese — the traditional 八相 names, so a Chinese
/// card is not forced to print "Full Moon" in the middle of otherwise Chinese
/// text. Same boundaries as `moon_phase_name`.
fn moon_phase_name_zh(f: f64) -> &'static str {
    if f < 0.0335 || f >= 0.9665 {
        "新月"
    } else if f < 0.2165 {
        "蛾眉月"
    } else if f < 0.2835 {
        "上弦月"
    } else if f < 0.4665 {
        "盈凸月"
    } else if f < 0.5335 {
        "满月"
    } else if f < 0.7165 {
        "亏凸月"
    } else if f < 0.7835 {
        "下弦月"
    } else {
        "残月"
    }
}

/// "HH:MM" → minutes since midnight. json_pluck has already reduced open-meteo's
/// ISO local datetimes to this form.
fn hhmm_to_minutes(s: &str) -> Option<f64> {
    let (h, m) = s.trim().split_once(':')?;
    Some(h.trim().parse::<f64>().ok()? * 60.0 + m.trim().parse::<f64>().ok()?)
}

/// The last page parsed, by url.
///
/// A card asks for 8 fields of 12 rows: without this the 1.3 MB results page
/// is re-parsed 96 times per render, which on the OnePlus 6 blew the VM's
/// script time budget outright (`script time budget exceeded`) and the card
/// never rendered a row — it looked like a slow network and was not. The
/// bytes are already deduped by the fetch cache; this dedupes the WORK of
/// reading them.
static YT_PARSED: std::sync::RwLock<Option<(String, std::sync::Arc<Vec<YtHit>>)>> =
    std::sync::RwLock::new(None);

/// Parse `bytes` for `url`, reusing the last parse when the url is unchanged.
fn yt_results_for(url: &str, bytes: &[u8]) -> std::sync::Arc<Vec<YtHit>> {
    if let Ok(slot) = YT_PARSED.read() {
        if let Some((u, hits)) = slot.as_ref() {
            if u == url {
                return hits.clone();
            }
        }
    }
    let hits = std::sync::Arc::new(yt_parse_results(bytes));
    if let Ok(mut slot) = YT_PARSED.write() {
        *slot = Some((url.to_owned(), hits.clone()));
    }
    hits
}

/// One parsed YouTube search result.
struct YtHit {
    id: String,
    title: String,
    channel: String,
    length: String,
    views: String,
    age: String,
}

/// The results page for a query. One url per query, so the fetch cache serves
/// every row and every field of a card from a single request — the page is
/// ~1.7 MB, which is the price of the only keyless path that still works.
fn yt_search_url(query: &str) -> String {
    let mut q = String::new();
    for b in query.trim().as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                q.push(*b as char)
            }
            b' ' => q.push('+'),
            other => q.push_str(&format!("%{other:02X}")),
        }
    }
    format!("https://www.youtube.com/results?search_query={q}")
}

/// Pull `videoRenderer` blocks out of the results page.
///
/// A WINDOW after each id rather than a JSON parse: the page embeds several
/// megabytes of `ytInitialData` whose shape shifts between rollouts, and a
/// strict parse of the whole thing fails entirely when one key moves. Reading
/// a bounded window per hit degrades field by field instead — a missing
/// channel costs the channel, not the list.
fn yt_parse_results(bytes: &[u8]) -> Vec<YtHit> {
    let body = String::from_utf8_lossy(bytes);
    let mut out: Vec<YtHit> = Vec::new();
    let mut at = 0usize;
    const MARK: &str = "\"videoRenderer\":{\"videoId\":\"";
    while let Some(found) = body[at..].find(MARK) {
        let start = at + found + MARK.len();
        at = start;
        let Some(end) = body[start..].find('"') else { break };
        let id = &body[start..start + end];
        if id.len() != 11 || out.iter().any(|h| h.id == id) {
            continue;
        }
        let win = &body[start..(start + 2600).min(body.len())];
        let pick = |open: &str| -> String {
            win.find(open)
                .and_then(|i| {
                    let rest = &win[i + open.len()..];
                    rest.find('"').map(|j| yt_unescape(&rest[..j]))
                })
                .unwrap_or_default()
        };
        let title = pick("\"title\":{\"runs\":[{\"text\":\"");
        if title.is_empty() {
            continue;
        }
        out.push(YtHit {
            id: id.to_string(),
            title,
            channel: pick("\"longBylineText\":{\"runs\":[{\"text\":\""),
            // `lengthText` nests an accessibility label BEFORE its simpleText,
            // so the duration is found inside that object rather than by a
            // key that happens to follow it.
            //
            // A LIVE stream has no lengthText at all — it has no duration yet.
            // Answering "" for it read as a pending fetch to the host's
            // lifecycle probe, so a card whose first result was a livestream
            // said "Searching..." over a full list of results (measured: Lofi
            // Girl). "LIVE" is both true and not empty.
            length: {
                let d = yt_nested(win, "\"lengthText\":{", "\"simpleText\":\"");
                if d.is_empty() { "LIVE".to_string() } else { d }
            },
            views: pick("\"viewCountText\":{\"simpleText\":\""),
            age: pick("\"publishedTimeText\":{\"simpleText\":\""),
        });
        if out.len() >= 20 {
            break;
        }
    }
    out
}

/// A value inside a named object: find the object, then the key within the
/// bounded slice that follows it. Two steps because the page nests
/// accessibility text ahead of the human-readable value.
fn yt_nested(win: &str, object: &str, key: &str) -> String {
    let Some(i) = win.find(object) else {
        return String::new();
    };
    let scope = &win[i..(i + 400).min(win.len())];
    scope
        .find(key)
        .and_then(|j| {
            let rest = &scope[j + key.len()..];
            rest.find('"').map(|k| yt_unescape(&rest[..k]))
        })
        .unwrap_or_default()
}

/// The page's JSON escapes, as far as a title needs them.
fn yt_unescape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(ch) => out.push(ch),
                    None => out.push_str(&hex),
                }
            }
            Some('n') => out.push(' '),
            Some(other) => out.push(other),
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod yt_tests {
    use super::*;

    /// A trimmed shape of the real results page, with the two nestings that
    /// caught the first parser out: the duration behind an accessibility
    /// label, and a unicode escape in the title.
    const PAGE: &str = r#"{"contents":[
      {"videoRenderer":{"videoId":"kJQP7kiw5Fk","title":{"runs":[{"text":"Luis Fonsi & Daddy Yankee"}]},
       "longBylineText":{"runs":[{"text":"LuisFonsiVEVO"}]},
       "publishedTimeText":{"simpleText":"8 years ago"},
       "lengthText":{"accessibility":{"accessibilityData":{"label":"4 minutes, 41 seconds"}},"simpleText":"4:41"},
       "viewCountText":{"simpleText":"8,900,000,000 views"}}},
      {"videoRenderer":{"videoId":"n61ULEU7CO0","title":{"runs":[{"text":"Best of lofi"}]},
       "longBylineText":{"runs":[{"text":"Lofi Girl"}]},
       "lengthText":{"accessibility":{"accessibilityData":{"label":"6 hours"}},"simpleText":"6:10:58"}}}
    ]}"#;

    #[test]
    fn a_result_carries_every_field_the_card_shows() {
        let hits = yt_parse_results(PAGE.as_bytes());
        assert_eq!(hits.len(), 2, "both renderers parse");
        assert_eq!(hits[0].id, "kJQP7kiw5Fk");
        // The escape is decoded, not shown raw.
        assert_eq!(hits[0].title, "Luis Fonsi & Daddy Yankee");
        assert_eq!(hits[0].channel, "LuisFonsiVEVO");
        // The HUMAN duration, not the accessibility sentence that precedes it.
        assert_eq!(hits[0].length, "4:41");
        assert_eq!(hits[0].views, "8,900,000,000 views");
        assert_eq!(hits[0].age, "8 years ago");
        assert_eq!(hits[1].length, "6:10:58");
    }

    #[test]
    fn a_live_stream_says_live_rather_than_nothing() {
        // No lengthText: a live stream has no duration. Empty would read as
        // "still fetching" to the lifecycle probe.
        const LIVE: &str = r#"{"videoRenderer":{"videoId":"jfKfPfyJRdk","title":{"runs":[{"text":"lofi radio"}]},
          "longBylineText":{"runs":[{"text":"Lofi Girl"}]}}}"#;
        let hits = yt_parse_results(LIVE.as_bytes());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].length, "LIVE", "a live result must not answer empty");
    }

    #[test]
    fn a_missing_field_costs_only_that_field() {
        // The second hit has no viewCount and no age; it is still a result.
        let hits = yt_parse_results(PAGE.as_bytes());
        assert_eq!(hits[1].id, "n61ULEU7CO0");
        assert_eq!(hits[1].channel, "Lofi Girl");
        assert!(hits[1].views.is_empty());
    }

    #[test]
    fn a_query_cannot_write_a_url() {
        let u = yt_search_url("lofi hip hop");
        assert_eq!(u, "https://www.youtube.com/results?search_query=lofi+hip+hop");
        let evil = yt_search_url("a&b=c#d");
        assert!(!evil.contains('&') && !evil.contains('#'), "{evil}");
    }
}

/// ISO3 codes in the order the card listed them (mirrors IndicatorPlot's own
/// sanitizer — the row index a card asks for must mean the same country the
/// chart drew in that colour).
fn wb_codes(raw: &str) -> Vec<String> {
    raw.split(|c| c == ',' || c == ';' || c == ' ')
        .map(|s| s.trim())
        .filter(|s| s.len() == 3 && s.chars().all(|c| c.is_ascii_alphabetic()))
        .map(|s| s.to_ascii_uppercase())
        .take(5)
        .collect()
}

fn wb_indicator(raw: &str) -> String {
    let t = raw.trim();
    if !t.is_empty()
        && t.len() <= 32
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
    {
        t.to_ascii_uppercase()
    } else {
        String::new()
    }
}

/// A World Bank reading, at the precision it is worth reading: a growth rate
/// to a decimal, a GDP total as a magnitude.
fn fmt_reading(v: f64) -> String {
    let a = v.abs();
    if a >= 1e12 {
        format!("{:.2}T", v / 1e12)
    } else if a >= 1e9 {
        format!("{:.1}B", v / 1e9)
    } else if a >= 1e6 {
        format!("{:.1}M", v / 1e6)
    } else if a >= 1000.0 {
        format!("{:.0}", v)
    } else {
        format!("{:.1}", v)
    }
}

/// Convert city readings and feels-like differences from the shared weather response.
fn city_weather_value(bytes: &[u8], field: &str, path: &str, unit: &str) -> String {
    let number = |path| json_pluck(bytes, path)?.parse::<f64>().ok().filter(|n| n.is_finite());
    let celsius = match field {
        "feels_delta" => number("current.apparent_temperature")
            .zip(number("current.temperature_2m"))
            .map(|(feels, temp)| feels - temp),
        "temp" | "feels" | "hi" | "lo" => number(path),
        _ => return json_pluck(bytes, path)
            .map(|v| round_display(path, v)).unwrap_or_else(|| "—".into()),
    };
    celsius.and_then(|value| octoscript_node::units::temperature(value, unit, field == "feels_delta"))
        .map(|value| {
            if field == "feels_delta" { format!("{value:.1}") }
            else { format!("{value:.0}") }
        })
        .unwrap_or_else(|| "—".into())
}

/// Round a DISPLAY reading to a whole number.
///
/// open-meteo reports a decimal, so an untouched card reads "29.5°", "8.45" and
/// "5.0 km/h" where every real weather app shows "29°", "8" and "5 km/h". The
/// decimals are noise at this precision and they cost real layout: a hero
/// temperature is set 60-76pt, so ".5" is a wide, attention-grabbing appendage on
/// the largest element of the card; in a forecast row it pushes the lo/hi labels
/// wide enough to squeeze the TempBar between them; and in a detail tile ".45"
/// pads a value already close to overflowing its box.
///
/// a2app's weather spec has always PROMISED this for temperatures ("show
/// whole-degree temps — the helper rounds temperature paths automatically"); it
/// was documented but never implemented, so every generated card carried them.
///
/// Scoped BY PATH NAME to the three quantities a card renders as a headline
/// number. Everything else is left alone, deliberately:
///   - `sys.airquality` shares `json_pluck`, and an AQI or pm2_5 reading must not
///     be touched here;
///   - "daily.sunrise.0" has already become "05:52", which must not be parsed as
///     a number;
///   - humidity and precipitation probability are already integers.
/// Values that do not parse as a number pass through untouched — including the
/// "—" placeholder shown while the fetch is in flight.

fn round_display(path: &str, value: String) -> String {
    let rounds = path.contains("temperature")
        || path.contains("uv_index")
        || path.contains("wind_speed");
    if !rounds {
        return value;
    }
    match value.parse::<f64>() {
        Ok(n) => format!("{}", n.round() as i64),
        Err(_) => value,
    }
}

/// Overpass API endpoint (keyless, no auth). overpass-api.de is the primary
/// public instance; https://overpass.kumi.systems/api/interpreter is a
/// drop-in mirror should it ever rate-limit.
const OVERPASS_URL: &str = "https://overpass-api.de/api/interpreter";

/// Map a card-facing category token to the ONE OSM tag filter with the best
/// worldwide coverage for that idea. "trail" uses nature_reserve because real
/// trail routes are OSM *relations*, which need a far heavier query shape than
/// the node/way `around` filter used here. Unknown tokens fall back to park —
/// the most widely mapped leisure tag — so a novel word never yields an empty
/// card.
fn overpass_filter(category: &str) -> (&'static str, &'static str) {
    match category.trim().to_ascii_lowercase().as_str() {
        "garden" => ("leisure", "garden"),
        "trail" => ("leisure", "nature_reserve"),
        "museum" => ("tourism", "museum"),
        "cafe" => ("amenity", "cafe"),
        "cinema" => ("amenity", "cinema"),
        "gym" => ("leisure", "fitness_centre"),
        "library" => ("amenity", "library"),
        "pool" => ("leisure", "swimming_pool"),
        "viewpoint" => ("tourism", "viewpoint"),
        "playground" => ("leisure", "playground"),
        "attraction" => ("tourism", "attraction"),
        "restaurant" | "food" => ("amenity", "restaurant"),
        "hotel" => ("tourism", "hotel"),
        // Google-Maps "add a stop along the route" categories:
        "gas" | "fuel" => ("amenity", "fuel"),
        "coffee" => ("amenity", "cafe"),
        "store" | "supermarket" | "grocery" => ("shop", "supermarket"),
        "pharmacy" => ("amenity", "pharmacy"),
        "atm" | "bank" => ("amenity", "bank"),
        "ev" | "charging" => ("amenity", "charging_station"),
        "parking" => ("amenity", "parking"),
        _ => ("leisure", "park"), // "park" and any unknown token
    }
}

// --- turn-by-turn navigation support (sys.navroute / sys.navstep) ---

struct NavStepInfo {
    cum_start: f64, // meters from route start where this step begins
    distance: f64,  // step length, meters
    kind: String,   // maneuver.type
    modifier: String,
    name: String,
    lanes: Vec<(String, bool)>, // (indication, is-recommended) at the maneuver
}

struct ParsedNavRoute {
    polyline: String,
    total_m: f64,
    total_s: f64,
    steps: Vec<NavStepInfo>,
}

thread_local! {
    static NAV_ROUTE_CACHE: std::cell::RefCell<
        std::collections::HashMap<String, std::rc::Rc<ParsedNavRoute>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Build OSRM's `;`-joined `{lon},{lat}` coordinate path: origin, then any
/// intermediate waypoints, then destination. `vias` is a `"lat,lon;lat,lon"`
/// string (empty / non-coord segments are ignored) so old 2-point callers pass
/// "" and get the exact same 2-coord path.
fn osrm_coords(lat1: f64, lon1: f64, lat2: f64, lon2: f64, vias: &str) -> String {
    let mut coords = format!("{lon1:.5},{lat1:.5}");
    for seg in vias.split(';') {
        let mut it = seg.trim().split(',');
        if let (Some(la), Some(lo)) = (it.next(), it.next()) {
            if let (Ok(la), Ok(lo)) = (la.trim().parse::<f64>(), lo.trim().parse::<f64>()) {
                coords.push_str(&format!(";{lo:.5},{la:.5}"));
            }
        }
    }
    coords.push_str(&format!(";{lon2:.5},{lat2:.5}"));
    coords
}

/// One URL per (from, vias…, to) so sys.navroute, sys.navstep and the MapView
/// widget all share a single deduped OSRM fetch. The via list keys into the
/// cache for free (the full URL is the key).
/// Whether a trip's endpoints are REAL yet. `sys.searchnum` answers -9999
/// while its geocode is in flight (and forever for an empty query), and that
/// sentinel was going out on the wire as a coordinate: measured,
/// `route/v1/driving/-9999.00000,-9999.00000;…` → 400 — which is TERMINAL, so
/// every such call also burned a permanent failure for nothing. A route whose
/// endpoint is not a place yet is not a route to fetch; it is a route to wait
/// for, and "—" already means exactly that.
fn osrm_ready(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> bool {
    valid_route_point(lat1, lon1) && valid_route_point(lat2, lon2)
}

fn valid_route_point(lat: f64, lon: f64) -> bool {
    lat.is_finite() && lon.is_finite() && lat.abs() <= 90.0 && lon.abs() <= 180.0
}

fn osrm_vias_ready(vias: &str) -> bool {
    // NIL means no optional waypoint. Coordinates still loading must not become
    // a 400 request or silently turn a multi-leg journey into a direct route.
    if matches!(vias.trim(), "" | "nil" | "null") { return true; }
    vias.split(';').all(|pair| {
        let Some((lat, lon)) = pair.trim().split_once(',') else { return false };
        match (lat.trim().parse::<f64>(), lon.trim().parse::<f64>()) {
            (Ok(lat), Ok(lon)) => valid_route_point(lat, lon),
            _ => false,
        }
    })
}

fn navroute_url(lat1: f64, lon1: f64, lat2: f64, lon2: f64, vias: &str) -> String {
    format!(
        "https://router.project-osrm.org/route/v1/driving/{}?overview=full&steps=true",
        osrm_coords(lat1, lon1, lat2, lon2, vias)
    )
}

fn nav_route_cached(vm: &mut ScriptVm, url: &str) -> Option<std::rc::Rc<ParsedNavRoute>> {
    if let Some(hit) = NAV_ROUTE_CACHE.with(|c| c.borrow().get(url).cloned()) {
        return Some(hit);
    }
    let bytes = vm.host.cx_mut().script_data_fetch(url)?;
    let route = parse_nav_route(&bytes)?;
    let rc = std::rc::Rc::new(route);
    NAV_ROUTE_CACHE.with(|c| c.borrow_mut().insert(url.to_string(), rc.clone()));
    Some(rc)
}

/// Called only when no parsed route exists. Preserve a failed HTTP request's
/// lifecycle; a completed response without a usable route is a failure too.
fn nav_route_placeholder(cx: &Cx, url: &str) -> String {
    if matches!(cx.script_data.resources.get_data_fetch(url),
        Some(crate::makepad_draw::makepad_platform::script::res::DataFetch::Loaded(_))) {
        "n/a".to_owned()
    } else {
        cx.script_data_placeholder(url)
    }
}

/// One serde parse of the OSRM response -> compact step table.
fn parse_nav_route(bytes: &[u8]) -> Option<ParsedNavRoute> {
    let root: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let route = root.get("routes")?.get(0)?;
    let polyline = route.get("geometry")?.as_str()?.to_string();
    let total_m = route.get("distance")?.as_f64()?;
    let total_s = route.get("duration")?.as_f64()?;
    // Iterate ALL legs (a route with K waypoints has K+1 legs), concatenating
    // their steps with a CONTINUOUS `cum` accumulator so cum_start stays
    // "meters from the route start" across waypoints — otherwise the banner
    // would stop guiding after the first via-point.
    let legs = route.get("legs")?.as_array()?;
    let mut steps = Vec::new();
    let mut cum = 0.0_f64;
    for leg in legs {
      let Some(steps_v) = leg.get("steps").and_then(|v| v.as_array()) else {
          continue;
      };
      for s in steps_v {
        let distance = s.get("distance").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let man = s.get("maneuver");
        let kind = man
            .and_then(|m| m.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let modifier = man
            .and_then(|m| m.get("modifier"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let name = s
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let mut lanes = Vec::new();
        if let Some(lv) = s
            .get("intersections")
            .and_then(|v| v.get(0))
            .and_then(|v| v.get("lanes"))
            .and_then(|v| v.as_array())
        {
            for l in lv.iter().take(6) {
                let ind = l
                    .get("indications")
                    .and_then(|v| v.get(0))
                    .and_then(|v| v.as_str())
                    .unwrap_or("straight")
                    .to_string();
                let valid = l.get("valid").and_then(|v| v.as_bool()).unwrap_or(false);
                lanes.push((ind, valid));
            }
        }
        steps.push(NavStepInfo {
            cum_start: cum,
            distance,
            kind,
            modifier,
            name,
            lanes,
        });
        cum += distance;
      }
    }
    if steps.is_empty() {
        return None;
    }
    Some(ParsedNavRoute {
        polyline,
        total_m,
        total_s,
        steps,
    })
}

fn nav_fmt_dist(m: f64) -> String {
    if m >= 1000.0 {
        format!("{:.1} km", m / 1000.0)
    } else {
        format!("{:.0} m", m.max(0.0))
    }
}

fn nav_arrow(step: &NavStepInfo) -> String {
    let m = step.modifier.as_str();
    match step.kind.as_str() {
        "arrive" => "🏁",
        "depart" => "▶",
        "merge" => "⤴",
        "on ramp" => "↗",
        "off ramp" => "↘",
        "fork" => {
            if m.contains("left") {
                "↖"
            } else {
                "↗"
            }
        }
        "roundabout" | "rotary" => "➡",
        "exit roundabout" | "exit rotary" => "↗",
        _ => match m {
            "left" => "⬅",
            "right" => "➡",
            "slight left" => "↖",
            "slight right" => "↗",
            "sharp left" => "⬅",
            "sharp right" => "➡",
            "uturn" => "⟲",
            _ => "⬆",
        },
    }
    .to_string()
}

fn nav_lane_glyph(indication: &str) -> &'static str {
    match indication {
        "left" | "sharp left" => "⬅",
        "right" | "sharp right" => "➡",
        "slight left" => "↖",
        "slight right" => "↗",
        "uturn" => "⟲",
        _ => "⬆",
    }
}

fn nav_instr(step: &NavStepInfo) -> String {
    let road = if step.name.trim().is_empty() {
        "the road".to_string()
    } else {
        step.name.clone()
    };
    let m = step.modifier.as_str();
    match step.kind.as_str() {
        "depart" => format!("Head out on {road}"),
        "arrive" => "Arrived at destination".to_string(),
        "merge" => format!("Merge onto {road}"),
        "on ramp" => "Take the ramp".to_string(),
        "off ramp" => "Take the exit".to_string(),
        "fork" => format!(
            "Keep {} at the fork",
            if m.contains("left") { "left" } else { "right" }
        ),
        "roundabout" | "rotary" => "Enter the roundabout".to_string(),
        "exit roundabout" | "exit rotary" => format!("Exit onto {road}"),
        "new name" => format!("Continue onto {road}"),
        _ => {
            if m.is_empty() {
                "Continue".to_string()
            } else {
                format!("Turn {m} onto {road}")
            }
        }
    }
}

/// Lanes for a maneuver: OSRM's, or synthesized from the modifier when absent
/// (car-grade nav always shows a lane strip near a turn).
fn nav_lanes(step: &NavStepInfo) -> Vec<(String, bool)> {
    if !step.lanes.is_empty() {
        let m = step.modifier.as_str();
        let any_valid = step.lanes.iter().any(|(_, v)| *v);
        return step
            .lanes
            .iter()
            .enumerate()
            .map(|(i, (ind, valid))| {
                let hot = if any_valid {
                    *valid
                } else {
                    // no valid flags: highlight by side
                    if m.contains("right") {
                        i == step.lanes.len() - 1
                    } else {
                        i == 0
                    }
                };
                (ind.clone(), hot)
            })
            .collect();
    }
    let m = step.modifier.as_str();
    if m.contains("left") {
        vec![
            ("left".into(), true),
            ("straight".into(), false),
            ("straight".into(), false),
        ]
    } else if m.contains("right") {
        vec![
            ("straight".into(), false),
            ("straight".into(), false),
            ("right".into(), true),
        ]
    } else {
        vec![("straight".into(), true), ("straight".into(), false)]
    }
}

thread_local! {
    /// One decoded polyline, kept for `nav_progress_m`'s per-frame projection.
    static DECODED_ROUTE: std::cell::RefCell<(String, std::rc::Rc<Vec<(f64, f64)>>)> =
        std::cell::RefCell::new((String::new(), std::rc::Rc::new(Vec::new())));
}

/// The decoded points of a polyline, decoding only when it changes.
#[cfg(feature = "maps")]
fn decoded_route(polyline: &str) -> std::rc::Rc<Vec<(f64, f64)>> {
    DECODED_ROUTE.with(|c| {
        let mut slot = c.borrow_mut();
        if slot.0 != polyline {
            *slot = (
                polyline.to_owned(),
                std::rc::Rc::new(crate::map::decode_polyline5(polyline)),
            );
        }
        slot.1.clone()
    })
}

/// How far along the route a position actually is, in metres.
///
/// Gated on `maps` because it needs that module's polyline decoder, and because a
/// position along a route has nothing to report without a map to draw it on. This
/// crate builds with `--no-default-features` and the first version of this was not
/// gated, which broke that configuration.
#[cfg(feature = "maps")]
///
/// This is the number `sys.navstep` needs and the one nothing could measure. The
/// nav app drove it from a clock — `sys.navsecs(period) * 15.2`, a looping timer
/// times an assumed 34 mph — so the turn banner advanced whether or not the
/// device moved, and arrived at the destination on schedule from a parked car.
///
/// Here the fix is projected onto the route: every segment is tested, the closest
/// one wins, and the answer is the distance accumulated up to that projection. So
/// progress moves when the device does and not otherwise.
///
/// The CLOSEST segment rather than the first within a tolerance, because a route
/// that doubles back — a U-turn, a cloverleaf, a street driven twice — has two
/// segments near the same point, and picking the earlier one would rewind the
/// banner to a turn already taken.
fn nav_progress_m(route: &ParsedNavRoute, at_lat: f64, at_lon: f64) -> f64 {
    // The decoded geometry is MEMOISED, and that is not a micro-optimisation.
    //
    // This decoded the polyline on every call. Once the turn banner moved into
    // `fn tick()` — so it could update without rebuilding the card — that became a
    // 685-point decode sixty times a second, and the frame hitches it caused were
    // measured at 46-62 ms each: the big 327 ms stalls were gone and had been
    // replaced by constant small ones. Same total jank, different cause.
    //
    // Keyed by the polyline itself, so a re-route replaces the entry and nothing
    // has to invalidate it.
    let pts = decoded_route(&route.polyline);
    if pts.len() < 2 {
        return 0.0;
    }
    let (mut best_d, mut best_at) = (f64::MAX, 0.0);
    let mut cum = 0.0;
    for w in pts.windows(2) {
        let ((a_lat, a_lon), (b_lat, b_lon)) = (w[0], w[1]);
        let seg = crate::map::haversine_m(a_lat, a_lon, b_lat, b_lon);
        // Project onto the segment in a local planar frame. Over the tens of
        // metres a route segment spans, the error of treating degrees as flat is
        // far below a GPS fix's own accuracy.
        let kx = 111_320.0 * a_lat.to_radians().cos();
        let (vx, vy) = ((b_lon - a_lon) * kx, (b_lat - a_lat) * 110_540.0);
        let (wx, wy) = ((at_lon - a_lon) * kx, (at_lat - a_lat) * 110_540.0);
        let len2 = vx * vx + vy * vy;
        let t = if len2 > 0.0 {
            ((wx * vx + wy * vy) / len2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let (dx, dy) = (wx - vx * t, wy - vy * t);
        let off = (dx * dx + dy * dy).sqrt();
        if off < best_d {
            best_d = off;
            best_at = cum + seg * t;
        }
        cum += seg;
    }
    // A fix nowhere near the route is not progress along it. Half a kilometre is
    // wide enough for a parallel service road and a poor urban fix, and narrow
    // enough that a device on a different road entirely reports the start rather
    // than a confidently wrong position mid-route.
    if best_d > 500.0 {
        return 0.0;
    }
    best_at.clamp(0.0, route.total_m)
}

/// Numeric per-step fields for layout binding (see sys.navstepnum).
fn nav_step_num(route: &ParsedNavRoute, d: f64, field: &str) -> f64 {
    let mut si = 0usize;
    for (i, s) in route.steps.iter().enumerate() {
        if d >= s.cum_start {
            si = i;
        } else {
            break;
        }
    }
    match field {
        "frac" => {
            if route.total_m > 0.0 {
                (d / route.total_m).clamp(0.0, 1.0)
            } else {
                0.0
            }
        }
        // meters to the upcoming maneuver — gate lane strips on `< 320`
        "dist" => {
            let cur_end = route
                .steps
                .get(si)
                .map(|s| s.cum_start + s.distance)
                .unwrap_or(route.total_m);
            (cur_end - d).max(0.0)
        }
        _ => -1.0,
    }
}

fn nav_step_field(route: &ParsedNavRoute, d: f64, field: &str) -> String {
    // current step = the one whose span contains d
    let mut si = 0usize;
    for (i, s) in route.steps.iter().enumerate() {
        if d >= s.cum_start {
            si = i;
        } else {
            break;
        }
    }
    let cur_end = route
        .steps
        .get(si)
        .map(|s| s.cum_start + s.distance)
        .unwrap_or(route.total_m);
    let next = route.steps.get(si + 1);
    match field {
        "instr" => next
            .map(nav_instr)
            .unwrap_or_else(|| "Arrived at destination".to_string()),
        "dist" => nav_fmt_dist((cur_end - d).max(10.0)),
        "arrow" => next
            .map(nav_arrow)
            .unwrap_or_else(|| "🏁".to_string()),
        "next_arrow" => route
            .steps
            .get(si + 2)
            .map(nav_arrow)
            .unwrap_or_else(|| "🏁".to_string()),
        "road" => route
            .steps
            .get(si)
            .map(|s| s.name.clone())
            .unwrap_or_default(),
        "rem" => nav_fmt_dist((route.total_m - d).max(0.0)),
        "remmin" => {
            let frac = if route.total_m > 0.0 {
                (1.0 - d / route.total_m).clamp(0.0, 1.0)
            } else {
                0.0
            };
            format!("{:.0}", (route.total_s * frac / 60.0).ceil().max(1.0))
        }
        f if f.starts_with("lane") => {
            let hot_query = f.ends_with("hot");
            let idx: usize = f
                .trim_start_matches("lane")
                .trim_end_matches("hot")
                .parse()
                .unwrap_or(99);
            let lanes = next.map(nav_lanes).unwrap_or_default();
            match lanes.get(idx) {
                Some((ind, hot)) => {
                    if hot_query {
                        if *hot { "1".to_string() } else { "0".to_string() }
                    } else {
                        nav_lane_glyph(ind).to_string()
                    }
                }
                None => String::new(),
            }
        }
        _ => String::new(),
    }
}

/// The open-meteo geocoding lookup URL for a place name — one URL per name so
/// sys.geocode + sys.geocodenum share the same deduped fetch.
/// Build the geocoding request for a place NAME.
///
/// `language` is not cosmetic here — it gates whether the query MATCHES AT ALL.
/// open-meteo's index is searched per-language, so "上海" with `language=en`
/// returns an empty result set while `language=zh` returns Shanghai. A Chinese
/// card therefore geocoded nothing, every coordinate came back as the -9999
/// sentinel, and the whole card rendered "n/a" — with the place name and the
/// condition displaying perfectly, which made it look like a data outage rather
/// than a lookup failure.
///
/// So the language follows the SCRIPT OF THE QUERY. A card names the place in
/// whatever language it is written in — that is the whole point of the card
/// choosing one language — and resolving it is the framework's job, not the
/// card's. Detecting CJK by codepoint range is enough: the alternative is asking
/// the generating model to romanise names, which is another thing for it to get
/// silently wrong.
fn geocode_url(name: &str, heap_key: usize) -> String {
    let name = name.trim();
    // An EMPTY name means "where the device is" — the weather exemplar's
    // `state city { shape: text, initial: "" }` is documented as exactly
    // that — and querying open-meteo's search for the empty string asks for
    // nothing and answers it: a full page of "n/a°" with no failure message.
    // Resolve the blank from the device's last GPS fix instead, by REVERSE
    // geocoding (Photon; open-meteo's gazetteer has no reverse endpoint).
    // City-scale rounding (~1 km) keeps the URL cache-stable under GPS
    // jitter — the answer is a PLACE, and the place does not change every
    // 40 m the fix drifts. No name AND no fix falls through to the empty
    // search, which resolves to nothing and keeps the placeholder path:
    // never invent a place.
    if name.is_empty() {
        // The fix only for an app granted `location`: a blank name must not
        // become a way to learn where the device is without asking.
        if let Some(fix) = crate::splash_policy::gps_fix_for_heap(heap_key) {
            return format!(
                "https://photon.komoot.io/reverse?lat={:.2}&lon={:.2}&lang=en",
                fix.lat, fix.lon
            );
        }
    }
    let lang = if name.chars().any(is_cjk) { "zh" } else { "en" };
    format!(
        "https://geocoding-api.open-meteo.com/v1/search?name={}&count=1&language={lang}&format=json",
        percent_encode_query(name)
    )
}

/// Pluck a logical geocode field from whichever response shape the fetch
/// answered with: open-meteo search (`results.0.*`) for a NAMED place, or
/// Photon reverse GeoJSON (`features.0.*`) when an empty name resolved from
/// the device's GPS fix — see `geocode_url`. One translation point, so
/// `sys.geocode` and `sys.geocodenum` cannot disagree about what "lat" means.
fn geocode_pluck(bytes: &[u8], field: &str) -> Option<String> {
    let candidates: &[&str] = match field {
        "lat" => &["results.0.latitude", "features.0.geometry.coordinates.1"],
        "lon" => &["results.0.longitude", "features.0.geometry.coordinates.0"],
        // Reverse at device scale names a house or a POI; the CITY is the
        // honest display name for "where the device is". `state` fills in
        // for the rural case where Photon answers no city at all.
        "name" => &[
            "results.0.name",
            "features.0.properties.city",
            "features.0.properties.name",
            "features.0.properties.state",
        ],
        "country" => &["results.0.country", "features.0.properties.country"],
        "admin1" => &["results.0.admin1", "features.0.properties.state"],
        // Photon carries neither; the em dash is the honest answer for a
        // blank name resolved by reverse.
        "timezone" => &["results.0.timezone"],
        "population" => &["results.0.population"],
        other => {
            return json_pluck(bytes, &format!("results.0.{other}"))
                .or_else(|| json_pluck(bytes, &format!("features.0.properties.{other}")));
        }
    };
    candidates.iter().find_map(|p| json_pluck(bytes, p))
}

/// Is this codepoint CJK? Covers the unified ideographs (incl. extension A) and
/// the compatibility block — enough to tell a Chinese/Japanese place name from a
/// Latin one. Kana are deliberately included via the Hiragana/Katakana range so
/// "きょうと" also routes to a CJK-indexed lookup.
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{30FF}'   // Hiragana + Katakana
        | '\u{3400}'..='\u{4DBF}' // CJK ext A
        | '\u{4E00}'..='\u{9FFF}' // CJK unified
        | '\u{F900}'..='\u{FAFF}' // CJK compatibility
    )
}

/// Percent-encode a string for a URL query VALUE (RFC 3986): unreserved chars
/// pass through, everything else (space and Overpass QL's `[]();:,=`) becomes
/// %XX per UTF-8 byte. Local so the crate needs no url-encoding dependency
/// (same approach as the sys.photo prompt encoder).
fn percent_encode_query(s: &str) -> String {
    let mut enc = String::with_capacity(s.len() * 3);
    let mut buf = [0u8; 4];
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~') {
            enc.push(ch);
        } else {
            for b in ch.encode_utf8(&mut buf).as_bytes() {
                enc.push('%');
                enc.push(char::from_digit((b >> 4) as u32, 16).unwrap().to_ascii_uppercase());
                enc.push(char::from_digit((b & 0xF) as u32, 16).unwrap().to_ascii_uppercase());
            }
        }
    }
    enc
}

/// The Overpass query URL for named `category` places within 4 km of lat/lon:
/// nodes AND ways (most parks/pools are mapped as ways; `out center` gives
/// each way a single representative point), capped at 30 elements to keep the
/// response phone-sized. Lat/lon are fixed to 4 decimals (~11 m) so GPS jitter
/// doesn't defeat the URL-level fetch dedup — sys.places and sys.placesnum
/// build this SAME url, so one request serves both.
fn overpass_places_url(lat: f64, lon: f64, category: &str) -> String {
    let (key, value) = overpass_filter(category);
    let around = format!("around:4000,{lat:.4},{lon:.4}");
    let query = format!(
        "[out:json][timeout:25];(node[{key}={value}]({around});way[{key}={value}]({around}););out center 30;"
    );
    format!("{OVERPASS_URL}?data={}", percent_encode_query(&query))
}

/// A named OSM place from an Overpass response, with its distance from the
/// request point.
struct NearbyPlace {
    name: String,
    dist_km: f64,
    lat: f64,
    lon: f64,
}

/// Great-circle distance in km (haversine, mean Earth radius). Sub-1% error
/// is invisible at the one-decimal precision cards display.
fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (dlat, dlon) = ((lat2 - lat1).to_radians(), (lon2 - lon1).to_radians());
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    2.0 * 6371.0 * a.sqrt().min(1.0).asin()
}

/// Parse an Overpass `out center` body into named places sorted nearest-first.
/// Position: `lat`/`lon` for nodes, `center.lat`/`center.lon` for ways (what
/// `out center` emits for areas). Unnamed elements are skipped (a card can't
/// show "way 24680") and duplicate names deduped keeping the nearest — one
/// park is often mapped as several ways sharing a name. None when the body
/// isn't an Overpass JSON response (vs Some(empty) for a valid "nothing
/// nearby"), so callers can tell error from zero.
fn places_parse(bytes: &[u8], lat: f64, lon: f64) -> Option<Vec<NearbyPlace>> {
    let root: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let elements = root.get("elements")?.as_array()?;
    let mut out = Vec::new();
    for el in elements {
        let name = match el.pointer("/tags/name").and_then(|n| n.as_str()) {
            Some(n) if !n.trim().is_empty() => n.trim().to_string(),
            _ => continue,
        };
        let coord = |key: &str, center_ptr: &str| {
            el.get(key)
                .and_then(|v| v.as_f64())
                .or_else(|| el.pointer(center_ptr).and_then(|v| v.as_f64()))
        };
        let (plat, plon) = match (coord("lat", "/center/lat"), coord("lon", "/center/lon")) {
            (Some(a), Some(o)) => (a, o),
            _ => continue,
        };
        out.push(NearbyPlace {
            name,
            dist_km: haversine_km(lat, lon, plat, plon),
            lat: plat,
            lon: plon,
        });
    }
    out.sort_by(|a, b| a.dist_km.total_cmp(&b.dist_km));
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.name.clone()));
    Some(out)
}

/// Field lookup for `sys.places`: row `index` of the parsed nearest-first
/// list. "count" ignores `index` so a header can show it without a dummy row.
/// "—" for unknown fields, out-of-range indices, or a malformed body — the
/// same placeholder the other live helpers show while loading.
fn places_field(bytes: &[u8], lat: f64, lon: f64, index: usize, field: &str) -> String {
    let places = match places_parse(bytes, lat, lon) {
        Some(p) => p,
        None => return "—".to_string(),
    };
    let f = field.to_ascii_lowercase();
    if f == "count" {
        return places.len().to_string();
    }
    let p = match places.get(index) {
        Some(p) => p,
        None => return "—".to_string(),
    };
    match f.as_str() {
        "name" => p.name.clone(),
        "distance" | "dist" => format!("{:.1} km", p.dist_km),
        "lat" => format!("{:.4}", p.lat),
        "lon" => format!("{:.4}", p.lon),
        _ => "—".to_string(),
    }
}

/// Free-text place/POI/address search via Photon (komoot) — keyless, built for
/// search & autocomplete, returns a ranked list. Backs `sys.search`/`sys.searchnum`
/// so a card can show tappable search RESULTS (the Google-Maps "search a place"
/// step) — unlike `sys.geocode` (one place-name → facts) or `sys.places`
/// (category + radius). `lang=en`, capped to 8 hits.
fn search_url(query: &str) -> String {
    format!(
        "https://photon.komoot.io/api/?q={}&limit=8&lang=en",
        percent_encode_query(query)
    )
}

/// One search hit: a display name, a secondary label (city/region/country), a
/// human category ("Museum", "Cafe"…), and coordinates to route to.
struct SearchHit {
    id: String,
    name: String,
    label: String,
    cat: String,
    lat: f64,
    lon: f64,
}

/// Turn Photon's osm_key/osm_value into a friendly category label ("Art gallery",
/// "Cafe", "Park"…), Title-cased, so a result row reads like a Google category.
fn photon_category(key: &str, value: &str) -> String {
    let v = match value {
        "" => key,
        other => other,
    };
    let pretty: String = v
        .replace('_', " ")
        .split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    pretty
}

/// Parse a Photon GeoJSON body into ranked hits. `geometry.coordinates` is
/// `[lon, lat]`. The primary name is `properties.name`, else
/// `housenumber street`, else the locality; the label chains the admin parts
/// (street/city/state/country) so ambiguous names ("Springfield") are
/// distinguishable. None when the body isn't valid GeoJSON (vs Some(empty) for
/// "no matches"), so callers tell error from zero.
fn search_parse(bytes: &[u8]) -> Option<Vec<SearchHit>> {
    let root: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let feats = root.get("features")?.as_array()?;
    let mut out = Vec::new();
    for ft in feats {
        let p = ft.get("properties");
        let coords = ft.pointer("/geometry/coordinates").and_then(|c| c.as_array());
        let (lon, lat) = match coords.and_then(|c| {
            Some((c.first()?.as_f64()?, c.get(1)?.as_f64()?))
        }) {
            Some(v) => v,
            None => continue,
        };
        let field = |k: &str| {
            p.and_then(|p| p.get(k))
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        let name = field("name")
            .or_else(|| match (field("housenumber"), field("street")) {
                (Some(h), Some(s)) => Some(format!("{h} {s}")),
                (_, Some(s)) => Some(s),
                _ => None,
            })
            .or_else(|| field("city"))
            .unwrap_or_else(|| "Unnamed place".to_string());
        let mut parts: Vec<String> = Vec::new();
        for k in ["street", "city", "state", "country"] {
            if let Some(v) = field(k) {
                if v != name && !parts.contains(&v) {
                    parts.push(v);
                }
            }
        }
        let cat = photon_category(
            field("osm_key").as_deref().unwrap_or(""),
            field("osm_value").as_deref().unwrap_or(""),
        );
        let id = p.and_then(|p| Some(format!("{}:{}",
            p.get("osm_type")?.as_str()?, p.get("osm_id")?.as_u64()?)))
            .unwrap_or_else(|| format!("{lat:.7},{lon:.7}|{name}"));
        if out.iter().any(|hit: &SearchHit| hit.id == id) {
            continue;
        }
        out.push(SearchHit {
            id,
            name,
            label: parts.join(", "),
            cat,
            lat,
            lon,
        });
    }
    Some(out)
}

/// Field lookup for `sys.search`: hit `index` of the ranked list. "count"
/// ignores `index`. "—"/"" for out-of-range or a bad body.
fn search_field(bytes: &[u8], index: usize, field: &str) -> String {
    let hits = match search_parse(bytes) {
        Some(h) => h,
        None => return String::new(),
    };
    let f = field.to_ascii_lowercase();
    if f == "count" {
        return hits.len().to_string();
    }
    let h = match hits.get(index) {
        Some(h) => h,
        None => return String::new(),
    };
    match f.as_str() {
        "id" => h.id.clone(),
        "name" => h.name.clone(),
        // The text that FINDS THIS HIT AGAIN. A results list sets card state from
        // the row a user picked, and that state is searched again to route — so a
        // row that carries only its name sends "Stanford" back and gets the TOP
        // "Stanford", which is in California whichever one was tapped. Name plus
        // label is what tells the five apart, so it is what has to travel.
        "query" => {
            if h.label.trim().is_empty() {
                h.name.clone()
            } else {
                format!("{}, {}", h.name, h.label)
            }
        }
        "label" | "addr" | "address" => h.label.clone(),
        "cat" | "category" => h.cat.clone(),
        "lat" => format!("{:.5}", h.lat),
        "lon" => format!("{:.5}", h.lon),
        _ => String::new(),
    }
}

#[cfg(test)]
mod nav_search_tests {
    use super::*;

    #[test]
    fn rejected_routes_are_failed_instead_of_loading_forever() {
        let cx = Cx::new(Box::new(|_, _| {}));
        let url = "https://nav-test.invalid/route";
        let request = LiveId::unique();
        cx.script_data.resources.begin_data_fetch(url, request);
        assert_eq!(nav_route_placeholder(&cx, url), "—");
        cx.script_data.resources.fail_data_fetch_terminally(request);
        assert_eq!(nav_route_placeholder(&cx, url), "n/a");
        let request = LiveId::unique();
        cx.script_data.resources.begin_data_fetch(url, request);
        let body = br#"{"code":"NoRoute","routes":[]}"#.to_vec();
        assert!(parse_nav_route(&body).is_none());
        cx.script_data.resources.handle_data_fetch_response(request, body);
        assert_eq!(nav_route_placeholder(&cx, url), "n/a");
    }

    #[test]
    fn unresolved_waypoints_do_not_issue_routes() {
        assert!(osrm_vias_ready(""));
        assert!(osrm_vias_ready("nil"));
        assert!(osrm_vias_ready("37.3,-122.0;37.4,-121.9"));
        assert!(!osrm_vias_ready("-9999,-9999"));
        assert!(!osrm_vias_ready("37.3,-122.0;-9999,-9999"));
        assert!(!osrm_vias_ready("NaN,0"));
        assert!(!osrm_ready(91.0, 0.0, 37.0, -122.0));
    }

    #[test]
    fn shared_addresses_have_distinct_stable_ids_and_duplicate_features_collapse() {
        let bytes = br#"{"features":[
            {"properties":{"osm_type":"N","osm_id":1,"name":"Office","city":"Santa Clara"},"geometry":{"coordinates":[-121.96,37.37]}},
            {"properties":{"osm_type":"W","osm_id":1,"name":"Office","city":"Santa Clara"},"geometry":{"coordinates":[-121.97,37.38]}},
            {"properties":{"osm_type":"N","osm_id":1,"name":"Office","city":"Santa Clara"},"geometry":{"coordinates":[-121.96,37.37]}}
        ]}"#;
        let hits = search_parse(bytes).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].label, hits[1].label);
        assert_ne!(hits[0].id, hits[1].id);
        assert_eq!(search_field(bytes, 0, "id"), "N:1");
        assert_eq!(search_field(bytes, 1, "id"), "W:1");
    }
}

/// Monotonic seconds since first use — the clock `sys.simsecs` animates from.
/// Also the map nav sim clock (`crate::map::nav::sim_clock_secs` re-exports it).
pub(crate) fn sim_clock_secs() -> f64 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_secs_f64()
}

#[cfg(test)]
mod policy_gate_tests {
    use super::*;

    /// `sys.gps(...)`, `sys.watchlistnum()` and `agent.notify` run inside a
    /// real isolate: first unpoliced (the host's own card), then under a
    /// policy that lacks each grant, then with the grants.
    #[test]
    fn a_policed_isolate_needs_grants_for_location_the_users_lists_and_the_agent() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        cx.with_vm(crate::script_mod);
        crate::makepad_draw::makepad_platform::gps::set_gps_fix(52.37, 4.89, 10.0);
        set_collections(std::collections::BTreeMap::from([(
            "watchlist".to_string(),
            vec!["NVDA".to_string(), "AAPL".to_string()],
        )]));
        let vm_id = cx.alloc_splash_vm();
        let read = |cx: &mut Cx| {
            cx.with_script_vm_id(vm_id, |vm| {
                register_agent_module(vm);
                let ok = vm.eval(crate::makepad_script::script! { sys.gps("ok") }).as_number();
                let lists = vm.eval(crate::makepad_script::script! { sys.watchlistnum() }).as_number();
                (ok, lists)
            })
        };
        let heap = cx.with_script_vm_id(vm_id, |vm| vm.bx.heap.heap_key());

        assert_eq!(read(&mut cx), (Some(1.0), Some(2.0)), "the host's own card sees both");

        crate::splash_policy::set_policy_for_heap(heap, vec!["net".into()], vec![], None);
        assert_eq!(read(&mut cx), (Some(0.0), Some(0.0)), "no grant reads as no fix and no saved lists");
        assert!(crate::splash_policy::service_allowed(heap, "agent.notify").is_err());

        crate::splash_policy::set_policy_for_heap(heap, vec!["location".into(), "profile".into()], vec![], None);
        assert_eq!(read(&mut cx), (Some(1.0), Some(2.0)));

        crate::splash_policy::gc_policies(&[heap]);
        set_collections(Default::default());
        cx.free_splash_vm(vm_id);
    }
}
