//! Emulation tab: the original firmware from reset on the patched µCsim board model, plus one
//! isolated lookup. A session runs in bounded chunks; each chunk continues from the previous
//! chunk's checkpoint in a fresh simulator process, until Pause, Reset or a blocker. Nothing here
//! is an engine model: under H2 the engine is stopped and every input is a named raw value.

use super::*;
use crate::emu::{self, Board, Coord, Inputs, Outcome, Request, RunReq};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Instructions per Run chunk (one backend request, about 4 s virtual at the assumed 2 MHz E clock).
const CHUNK: u32 = emu::MAX_RUN;
/// PC samples kept for the timeline across chunks; older ones are dropped.
const TRAIL: usize = 1024;

/// Identity of the bytes a result was computed from. Results for any other snapshot are never shown.
#[derive(Clone, PartialEq)]
pub(super) struct Snap {
    path: PathBuf,
    sha: String,
    /// All working bytes: any session edit invalidates a shown result.
    working: Option<Vec<u8>>,
}

/// One emulation session: its identity (bytes, profile, inputs) and its latest state.
pub(super) struct Session {
    snap: Snap,
    board: Board,
    inputs: Inputs,
    result: Result<emu::Run, String>,
    /// (instruction, E cycle, PC) across all chunks, oldest first.
    trail: VecDeque<(u64, u64, u16)>,
    chunks: u64,
}

pub(super) struct Emu {
    /// Request generation: only the latest request may store a result.
    generation: u64,
    busy: bool,
    /// Run continues chunk after chunk until paused, reset, blocked or failed.
    running: bool,
    /// Kills the in-flight backend process when the session is reset or replaced.
    cancel: Arc<AtomicBool>,
    board: Board,
    inputs: Inputs,
    raw: bool,
    coord: Coord,
    session: Option<Session>,
    lookup: Option<(Snap, Result<emu::Lookup, String>)>,
    note: Option<String>,
}

impl Default for Emu {
    fn default() -> Self {
        Self {
            generation: 0,
            busy: false,
            running: false,
            cancel: Arc::new(AtomicBool::new(false)),
            board: Board::H2,
            inputs: Inputs::default(),
            raw: false,
            coord: Coord { row: 7, col: 3, row_frac: 192, col_frac: 64 },
            session: None,
            lookup: None,
            note: None,
        }
    }
}

impl Emu {
    /// Drop the session: cancel any in-flight request and forget its result.
    fn reset_session(&mut self) {
        self.generation += 1;
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        (self.busy, self.running) = (false, false);
        self.session = None;
    }

    pub(super) fn invalidate(&mut self) {
        self.reset_session();
        self.lookup = None;
        self.note = None;
    }

    /// The inputs a session of this profile uses (strict and H1 take none).
    fn effective_inputs(&self) -> Inputs {
        if self.board == Board::H2 { self.inputs } else { Inputs::default() }
    }
}

impl Snap {
    fn new(path: PathBuf, sha: String, working: Option<&[u8]>) -> Self {
        Self { path, sha, working: working.map(<[u8]>::to_vec) }
    }
}

/// An H2 input a control edits.
#[derive(Clone, Copy)]
enum Field {
    In1060,
    In1080,
    PortA,
    PortD,
    Fill,
    Adc(usize),
}

impl Field {
    fn get(self, i: &mut Inputs) -> &mut u8 {
        match self {
            Field::In1060 => &mut i.in1060,
            Field::In1080 => &mut i.in1080,
            Field::PortA => &mut i.porta_in,
            Field::PortD => &mut i.portd_in,
            Field::Fill => &mut i.eeprom_fill,
            Field::Adc(k) => &mut i.adc[k],
        }
    }
}

const MONO: &str = "JetBrainsMono Nerd Font";
const OK: u32 = 0x5cc98a;

fn regs_text(r: &emu::Regs) -> String {
    format!("PC ${:04X}  SP ${:04X}  A ${:02X}  B ${:02X}  X ${:04X}  Y ${:04X}  CC {:08b} (SXHINZVC)", r.pc, r.sp, r.a, r.b, r.x, r.y, r.cc)
}

fn line(t: impl Into<SharedString>, color: u32) -> Div {
    div().text_xs().text_color(rgb(color)).child(t.into())
}

fn chip(t: impl Into<SharedString>, fg: u32, border: u32) -> Div {
    div().px_2().py(px(2.)).rounded_md().border_1().border_color(rgb(border)).bg(rgb(PANEL2)).text_xs().text_color(rgb(fg)).child(t.into())
}

/// Log-scaled 0..1 for activity counts.
fn level(n: u64, max: u64) -> f32 {
    if n == 0 || max == 0 { 0. } else { ((n as f32).ln_1p() / (max as f32).ln_1p()).clamp(0.05, 1.) }
}

fn ms(cycles: u64) -> f64 {
    cycles as f64 * 1000. / emu::E_CLOCK_HZ as f64
}

/// The checkpoint a session continues from, if it can continue at all.
fn continuation(s: Option<&Session>) -> Option<Option<emu::Checkpoint>> {
    match s {
        None => Some(None),
        Some(Session { result: Ok(r), .. }) => r.checkpoint.clone().map(Some),
        Some(_) => None,
    }
}

/// Append a chunk's samples to the bounded trail (its first sample repeats the previous last one).
fn extend_trail(trail: &mut VecDeque<(u64, u64, u16)>, samples: &[(u64, u64, u16)]) {
    for &s in samples {
        if trail.back() != Some(&s) {
            trail.push_back(s);
        }
    }
    while trail.len() > TRAIL {
        trail.pop_front();
    }
}

impl Rallye {
    fn emu_target(&self) -> Option<&Dump> {
        self.inv.as_ref().ok()?.dumps.get(self.target)
    }

    fn emu_snap(&self) -> Option<Snap> {
        let d = self.emu_target()?;
        Some(Snap::new(d.path.clone(), d.sha256.clone(), Some(self.bytes_of(d))))
    }

    /// The session for the current bytes, profile and inputs, if any; never a stale one.
    fn emu_session(&self) -> Option<&Session> {
        let snap = self.emu_snap();
        let inputs = self.emu.effective_inputs();
        self.emu.session.as_ref().filter(|s| Some(&s.snap) == snap.as_ref() && s.board == self.emu.board && s.inputs == inputs)
    }

    /// Run one backend request off the UI thread. A Run result is kept only if the session identity
    /// (target bytes, working bytes, profile, inputs) is unchanged and no newer request was issued.
    fn emu_run(&mut self, req: Request, cx: &mut Context<Self>) {
        let (Some(d), Some(snap)) = (self.emu_target(), self.emu_snap()) else { return };
        if self.emu.busy {
            return;
        }
        let lookup = matches!(req, Request::Lookup(_));
        let (board, inputs) = (self.emu.board, self.emu.effective_inputs());
        let (original, sha) = (d.bytes.clone(), d.sha256.clone());
        let edits = if lookup { emu::table_edits(&d.bytes, self.bytes_of(d)) } else { Ok(vec![]) };
        let (sim, edits) = match edits.and_then(|e| emu::simulator().map(|s| (s, e))) {
            Ok(v) => v,
            Err(e) => {
                self.emu.running = false;
                return self.emu_store(snap, board, inputs, Err(e), lookup);
            }
        };
        self.emu.generation += 1;
        let generation = self.emu.generation;
        self.emu.busy = true;
        let cancel = self.emu.cancel.clone();
        let task = cx.background_executor().spawn(async move { emu::run(&req, &sim, &original, &sha, &edits, &cancel) });
        cx.spawn(async move |this, cx| {
            let r = task.await;
            this.update(cx, |this, cx| {
                if this.emu.generation != generation {
                    return; // reset, replaced or cancelled meanwhile
                }
                this.emu.busy = false;
                if this.emu_snap().as_ref() != Some(&snap) || this.emu.board != board || this.emu.effective_inputs() != inputs {
                    this.emu.reset_session();
                    this.msg = Some(("Emulation result discarded: the target, its working copy, the profile or the inputs changed while it ran".into(), WARN));
                    return cx.notify();
                }
                this.emu_store(snap, board, inputs, r.map_err(|e| e.to_string()), lookup);
                // Continue a Run with the next chunk; Pause, a blocker or an error ends it.
                if this.emu.running && !lookup {
                    match continuation(this.emu_session()) {
                        Some(Some(c)) => this.emu_run(Request::Run(RunReq { board, budget: CHUNK, inputs, checkpoint: Some(c) }), cx),
                        _ => this.emu.running = false,
                    }
                }
                cx.notify();
            })
            .ok(); // window closed: nothing to update
        })
        .detach();
    }

    fn emu_store(&mut self, snap: Snap, board: Board, inputs: Inputs, r: Result<Outcome, String>, lookup: bool) {
        match (r, lookup) {
            (Ok(Outcome::Lookup(l)), _) => self.emu.lookup = Some((snap, Ok(l))),
            (Err(e), true) => self.emu.lookup = Some((snap, Err(e))),
            (r, _) => {
                let result = r.and_then(|o| match o {
                    Outcome::Run(x) => Ok(*x),
                    Outcome::Lookup(_) => Err("unexpected lookup response".into()),
                });
                if result.is_err() {
                    self.emu.running = false;
                }
                let keep = self.emu.session.take().filter(|s| s.snap == snap && s.board == board && s.inputs == inputs);
                let (mut trail, chunks) = keep.map_or((VecDeque::new(), 0), |s| (s.trail, s.chunks));
                if let Ok(run) = &result {
                    extend_trail(&mut trail, &run.samples);
                }
                self.emu.session = Some(Session { snap, board, inputs, result, trail, chunks: chunks + 1 });
            }
        }
    }

    /// Start a Run (continuous) or a single Step from the session's checkpoint (or from reset).
    fn emu_continue(&mut self, budget: u32, running: bool, cx: &mut Context<Self>) {
        if let Some(checkpoint) = continuation(self.emu_session()) {
            self.emu.running = running;
            let (board, inputs) = (self.emu.board, self.emu.effective_inputs());
            self.emu_run(Request::Run(RunReq { board, budget, inputs, checkpoint }), cx);
        }
    }

    fn emu_btn(&self, id: &'static str, label: &'static str, enabled: bool, on: fn(&mut Self, &mut Context<Self>), cx: &mut Context<Self>) -> Stateful<Div> {
        let b = btn(id, label, false);
        if enabled {
            b.on_click(cx.listener(move |this, _, _, cx| {
                on(this, cx);
                cx.notify();
            }))
        } else {
            b.opacity(0.4)
        }
    }

    /// − / + controls for one descriptor byte, clamped to 0..=max.
    fn stepper(&self, id: &'static str, label: &'static str, value: u8, max: u8, field: fn(&mut Coord) -> &mut u8, cx: &mut Context<Self>) -> Div {
        let deltas: &[(i32, &str)] = if max > 30 { &[(-16, "−16"), (-1, "−1"), (1, "+1"), (16, "+16")] } else { &[(-1, "−1"), (1, "+1")] };
        let mut row = div().flex().items_center().gap_1().child(div().w(px(190.)).flex_none().text_xs().text_color(rgb(MUTED)).child(label));
        for (k, &(delta, text)) in deltas.iter().enumerate() {
            if k == deltas.len() / 2 {
                row = row.child(div().w(px(44.)).flex().justify_center().font_family(MONO).text_sm().child(value.to_string()));
            }
            row = row.child(btn(SharedString::from(format!("{id}{k}")), text, false).on_click(cx.listener(move |this, _, _, cx| {
                let v = field(&mut this.emu.coord);
                *v = (*v as i32 + delta).clamp(0, max as i32) as u8;
                cx.notify();
            })));
        }
        row
    }

    /// Change one H2 input. Inputs are part of the session identity, so a running session is reset.
    fn set_input(&mut self, field: Field, value: u8) {
        if *field.get(&mut self.emu.inputs) == value {
            return;
        }
        *field.get(&mut self.emu.inputs) = value;
        if self.emu.session.is_some() || self.emu.busy {
            self.emu.reset_session();
            self.emu.note = Some("Inputs changed: they are part of the session identity, so the session was reset. Run starts again from reset with the new inputs.".into());
        }
    }

    /// One byte input as eight bit toggles (bit 7 left); `used` marks bits the firmware is known to read.
    fn bit_row(&self, id: &'static str, label: &'static str, bits: u8, used: &[(u8, &'static str)], field: Field, cx: &mut Context<Self>) -> Div {
        let value = *field.get(&mut self.emu.inputs.clone());
        let mut row = div().flex().items_center().gap_1().child(div().w(px(210.)).flex_none().text_xs().text_color(rgb(MUTED)).child(label));
        for b in (0..8u8).rev() {
            if bits & (1 << b) == 0 {
                row = row.child(div().w(px(26.)).text_xs().text_color(rgb(MUTED)).flex().justify_center().child("·"));
                continue;
            }
            let on = value & (1 << b) != 0;
            let mark = if used.iter().any(|u| u.0 == b) { "*" } else { "" };
            row = row.child(btn(SharedString::from(format!("{id}{b}")), format!("{b}{mark}={}", on as u8), on).w(px(40.)).on_click(cx.listener(move |this, _, _, cx| {
                this.set_input(field, value ^ (1 << b));
                cx.notify();
            })));
        }
        let notes: Vec<String> = used.iter().map(|(b, w)| format!("bit {b}: {w}")).collect();
        row.child(div().font_family(MONO).text_xs().child(format!("${value:02X}"))).child(line(notes.join(" · "), MUTED))
    }

    fn byte_stepper(&self, id: SharedString, label: String, value: u8, field: Field, cx: &mut Context<Self>) -> Div {
        let mut row = div().flex().items_center().gap_1().child(div().w(px(210.)).flex_none().text_xs().text_color(rgb(MUTED)).child(label));
        for (k, delta) in [-16i32, -1, 1, 16].into_iter().enumerate() {
            if k == 2 {
                row = row.child(div().w(px(70.)).flex().justify_center().font_family(MONO).text_sm().child(format!("{value} ${value:02X}")));
            }
            let text = if delta < 0 { format!("−{}", -delta) } else { format!("+{delta}") };
            row = row.child(btn(SharedString::from(format!("{id}-{k}")), text, false).on_click(cx.listener(move |this, _, _, cx| {
                this.set_input(field, (value as i32 + delta).clamp(0, 255) as u8);
                cx.notify();
            })));
        }
        row
    }

    fn inputs_panel(&self, cx: &mut Context<Self>) -> Div {
        let i = self.emu.inputs;
        let mut adc = div().flex().flex_col().gap_1();
        for k in 0..8 {
            adc = adc.child(self.byte_stepper(SharedString::from(format!("adc{k}")), format!("A/D channel AN{k} · raw result"), i.adc[k], Field::Adc(k), cx));
        }
        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(WARN))
            .child(heading("H2 SIMULATED INPUTS · RAW UNITS · HYPOTHETICAL DEFAULTS · PHYSICAL MEANING OF EVERY INPUT UNKNOWN"))
            .child(line("Constant for the whole session (the engine is stopped: no crank or input-capture edges). Changing any input resets the session. * = bit the firmware is seen to read.", WARN))
            .child(self.bit_row("i60-", "Board input byte $1060", 0xFF, &[(5, "branch at $A86A"), (1, "stored at $A035")], Field::In1060, cx))
            .child(self.bit_row("i80-", "Board input byte $1080", 0xFF, &[(4, "tested at $BB51/$BB61")], Field::In1080, cx))
            .child(self.bit_row("pa-", "PORTA input pins PA7, PA2–PA0", 0x87, &[], Field::PortA, cx))
            .child(self.bit_row("pd-", "PORTD input pins PD5–PD0", 0x3F, &[(5, "SPI SS level (low = mode fault)")], Field::PortD, cx))
            .child(adc)
            .child(self.byte_stepper("eefill".into(), "Internal EEPROM power-up fill byte".into(), i.eeprom_fill, Field::Fill, cx))
            .child(line("Default $FF = erased EEPROM. The real EEPROM content is not in the dump; the firmware treats $B7FF ≠ $AA as uninitialised and rewrites it (simulated only).", MUTED))
    }

    pub(super) fn emulation(&self, cx: &mut Context<Self>) -> Div {
        let root = div().flex().flex_col().gap_3();
        let Some(d) = self.emu_target() else {
            return root.child(card().child(line("No target dump loaded. Use Browse… or Open folder….", WARN)));
        };
        let profile = emu::profile(&d.sha256);
        let mut head = card()
            .child(div().text_lg().font_weight(FontWeight::BOLD).child("Firmware emulation · research model, not an engine simulation"))
            .child(kv("Target", d.name.clone()))
            .child(kv("Source SHA-256", d.sha256.clone()))
            .child(kv("Profile", profile.map_or("unsupported source hash".to_string(), |p| format!("{p} · reviewed hash"))));
        let Some(_) = profile else {
            return root.child(head.child(line(
                "Emulation disabled for this file (fail closed): only the reviewed XSI2 and mild-remap source hashes are supported. Code addresses differ between firmware revisions.",
                ERR,
            )));
        };
        head = head.child(kv("Backend", emu::simulator().map_or_else(|e| e, |p| format!("µCsim {}", p.display()))));
        root.child(head).child(self.run_card(cx)).child(self.lookup_card(d, cx)).child(
            card()
                .child(heading("ENGINE OUTPUTS"))
                .child(line("Engine stopped. RPM, injector pulse width and ignition timing are NOT simulated: there is no crank signal, and no output pin or SPI byte has been mapped to an injector or coil.", WARN))
                .child(line("Available instead: raw PORTA output-compare pins, SPI frames and SCI characters the firmware produces with the engine stopped (see the run above), map reads in the isolated lookup below.", MUTED)),
        )
    }

    fn run_card(&self, cx: &mut Context<Self>) -> Div {
        let session = self.emu_session();
        let can_continue = continuation(session).is_some();
        let (busy, running) = (self.emu.busy, self.emu.running);
        let mut profiles = div().flex().flex_wrap().gap_2().items_center().child(div().text_xs().text_color(rgb(MUTED)).child("Board profile"));
        for (id, label, b) in [
            ("bp-strict", "Strict (stop at unknown hardware)", Board::Strict),
            ("bp-h1", "H1 hypothesis (unverified board assumptions)", Board::H1),
            ("bp-h2", "H2 simulation (H1 + named hypotheses + supplied inputs)", Board::H2),
        ] {
            profiles = profiles.child(btn(id, label, self.emu.board == b).on_click(cx.listener(move |this, _, _, cx| {
                if this.emu.board != b {
                    this.emu.board = b;
                    this.emu.reset_session(); // a session belongs to one profile
                    this.emu.note = None;
                }
                cx.notify();
            })));
        }
        // Fixed button positions: availability shows only through opacity, so Pause never moves.
        let controls = div()
            .flex()
            .flex_wrap()
            .gap_2()
            .child(self.emu_btn("run", "▶ Run", !running && !busy && can_continue, |this, cx| this.emu_continue(CHUNK, true, cx), cx))
            .child(self.emu_btn("pause", "‖ Pause", running, |this, _| this.emu.running = false, cx))
            .child(self.emu_btn("step", "Step 1 instruction", !running && !busy && can_continue, |this, cx| this.emu_continue(1, false, cx), cx))
            .child(self.emu_btn("reset", "Reset", session.is_some() || busy || self.emu.session.is_some(), |this, _| {
                this.emu.reset_session();
                this.emu.note = None;
            }, cx));
        let (state, color) = match session.map(|s| &s.result) {
            _ if running => ("RUNNING · continuing chunk after chunk from each checkpoint", ACCENT),
            _ if busy => ("EXECUTING ONE REQUEST", ACCENT),
            None => ("NOT STARTED · press Run (starts from reset)", MUTED),
            Some(Err(_)) => ("STOPPED ON ERROR · press Reset", ERR),
            Some(Ok(r)) if r.blocker.is_some() => ("BLOCKED · next instruction needs unmodelled hardware", ERR),
            Some(Ok(_)) => ("PAUSED · Run continues from the checkpoint", WARN),
        };
        let mut c = card()
            .child(heading("FIRMWARE FROM RESET · ORIGINAL SOURCE BYTES · KEY ON, ENGINE STOPPED"))
            .child(profiles)
            .child(controls)
            .child(div().h(px(18.)).text_sm().font_weight(FontWeight::BOLD).text_color(rgb(color)).child(state))
            .child(line(
                format!("Run executes {CHUNK} instructions per backend request and continues from that request's checkpoint (exact continuation, tested against one long run) until Pause, Reset or a blocker; there is no total cap. Execution stops before the first instruction that needs unmodelled hardware or uses an opcode without a conformance case. Session edits are not used, but any edit resets the session."),
                MUTED,
            ));
        match self.emu.board {
            Board::H1 => {
                c = c.child(div().p_2().rounded_md().border_1().border_color(rgb(WARN)).child(line("H1 is a working hypothesis, not a validated ECU board: $1040/$1050 writes are treated as write-only latches with unknown effect, EPROM-address writes of the byte already there are dropped, and the diagnostic serial line is idle. Any other external access still stops.", WARN)))
            }
            Board::H2 => c = c.child(self.inputs_panel(cx)),
            Board::Strict => {}
        }
        if let Some(n) = &self.emu.note {
            c = c.child(line(n.clone(), WARN));
        }
        let Some(s) = session else {
            if self.emu.session.is_some() {
                return c.child(line("The last session belonged to other bytes, another profile or other inputs; Run starts again from reset.", MUTED));
            }
            return c;
        };
        let r = match &s.result {
            Err(e) => return c.child(line(e.clone(), ERR)),
            Ok(r) => r,
        };
        c.child(self.status_strip(r, s))
            .child(self.scheduler(r))
            .child(self.timeline(r, s))
            .child(self.memory_map(r))
            .child(div().flex().gap_4().items_start().child(self.page_grid(r)).child(self.io_table(r)))
            .child(div().flex().gap_4().items_start().child(self.outputs(r)).child(self.memory(r)))
            .child(self.events(r, cx))
    }

    /// Main-loop, timer-interrupt and watchdog evidence of the engine-stopped scheduler.
    fn scheduler(&self, r: &emu::Run) -> Div {
        let entries: u64 = r.interrupts.iter().map(|i| i.count).sum();
        let mut t = div().flex().flex_col().font_family(MONO).text_xs();
        t = t.child(div().text_color(rgb(MUTED)).child(format!("{:<6} {:<7} {:>10} {:>14}", "source", "vector", "entries", "last at")));
        for i in &r.interrupts {
            t = t.child(div().child(format!("{:<6} ${:04X}   {:>10} {:>11.1} ms", i.name, i.vector, i.count, ms(i.last_cycle))));
        }
        if r.interrupts.is_empty() {
            t = t.child(line("no interrupt taken yet", MUTED));
        }
        let mut w = div().flex().flex_col().font_family(MONO).text_xs();
        for (a, name, n) in &r.watch {
            w = w.child(div().child(format!("${a:04X} {n:>10}  {name}")));
        }
        let l = &r.main_loop;
        let sp = l.sp.map_or("—".into(), |(a, b)| if a == b { format!("${a:04X} (stable)") } else { format!("${a:04X}–${b:04X}") });
        // OPTION CR1:CR0 scales the COP timeout of 2^15 E cycles by 1, 4, 16 or 64 (manual 5.3.2).
        let cop_timeout = r.io.iter().find(|i| i.addr == 0x1039).map(|o| (1u64 << 15) << (2 * (o.value & 3)));
        let cop = match cop_timeout {
            Some(t) => format!("{} services · longest gap {:.1} ms · timeout {:.1} ms if enabled (OPTION CR) · CONFIG NOCOP unknown, not enforced", r.cop_services, ms(r.cop_gap_max), ms(t)),
            None => format!("{} services", r.cop_services),
        };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(heading("SCHEDULER · MAIN LOOP, INTERRUPTS AND WATCHDOG (ENGINE STOPPED; NOT EVIDENCE OF A RUNNING ENGINE)"))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_3()
                    .child(chip(format!("main loop ${:04X}: {} passes", l.pc, l.count), if l.count > 1 { OK } else { MUTED }, BORDER))
                    .child(chip(format!("SP at loop head: {sp}"), TEXT, BORDER))
                    .child(chip(format!("longest pass {:.2} ms", ms(l.gap_max)), TEXT, BORDER))
                    .child(chip(format!("interrupts: {entries} entries, {} RTI returns", r.rti), if r.rti > 0 { OK } else { MUTED }, BORDER)),
            )
            .child(div().flex().gap_4().items_start().child(div().flex().flex_col().child(heading("INTERRUPT SOURCES (HC11 PRIORITY ORDER)")).child(t)).child(div().flex().flex_col().child(heading("EXECUTIONS AT STATIC LANDMARKS")).child(w)))
            .child(kv("COP watchdog", cop))
            .child(line("Interrupt entry stacks PC, Y, X, A, B, CCR (9 bytes), sets I and takes 12 E cycles; RTI restores them. Conformance cases cover entry, return, repeated requests, CLI delay and HPRIO priority.", MUTED))
    }

    /// Raw output activity; which actuator (if any) each signal drives is unknown.
    fn outputs(&self, r: &emu::Run) -> Div {
        let o = &r.outputs;
        let pins: String = (3..8).rev().map(|b| format!("PA{b}={} ({} changes)  ", (o.porta_pins >> b) & 1, o.porta_changes[b])).collect();
        let bytes = |v: &[u8]| v.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ");
        let mut c = div()
            .flex()
            .flex_col()
            .gap_1()
            .flex_1()
            .child(heading("RAW OUTPUTS · MAPPING TO ACTUATORS UNKNOWN"))
            .child(kv("PORTA output pins", pins).font_family(MONO))
            .child(kv("PORTA latch", format!("${:02X}", o.porta_latch)).font_family(MONO))
            .child(kv("SPI bytes sent", format!("{} · last: {}", o.spi_bytes, bytes(&o.spi_last))).font_family(MONO))
            .child(kv("SCI characters sent", format!("{} · last: {}", o.sci_bytes, bytes(&o.sci_last))).font_family(MONO))
            .child(line("SPI/SCI received data is unknown in every profile; the firmware may only load it and discard it, otherwise the run stops.", MUTED));
        if let Some((init, ops, _)) = &r.eeprom {
            c = c.child(kv("Internal EEPROM", format!("{} · {ops} program/erase operations (simulated, 10 ms VPP checked)", if *init { "in use" } else { "not accessed yet" })));
        }
        c
    }

    /// Internal RAM, and the H2 EEPROM, as value heat grids (16 bytes per row).
    fn memory(&self, r: &emu::Run) -> Div {
        let grid = |bytes: &[u8], cols: usize| {
            let mut g = div().flex().flex_col().gap(px(1.));
            for row in bytes.chunks(cols) {
                let mut line_ = div().flex().gap(px(1.));
                for &v in row {
                    line_ = line_.child(div().w(px(8.)).h(px(8.)).bg(if v == 0 { rgb(PANEL2).into() } else { heat(v as f32 / 255.) }));
                }
                g = g.child(line_);
            }
            g
        };
        let mut c = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(heading("MEMORY · VALUE HEAT, 16 OR 64 BYTES PER ROW (DARK = $00)"))
            .child(div().flex().gap_3().items_start().child(div().flex().flex_col().child(line("internal RAM $0000–$00FF", MUTED)).child(grid(&r.ram_internal, 16))).child(div().flex().flex_col().child(line("external RAM $2000–$27FF", MUTED)).child(grid(&r.ram_external, 64))));
        if let Some((_, _, ee)) = &r.eeprom {
            c = c.child(line("EEPROM $B600–$B7FF (H2, simulated)", MUTED)).child(grid(ee, 64));
        }
        c
    }

    /// Boot stage chips, blocker and headline counters.
    fn status_strip(&self, r: &emu::Run, s: &Session) -> Div {
        let last = r.milestones.iter().rposition(|m| m.2.is_some());
        let mut stages = div().flex().flex_wrap().gap_1().items_center();
        for (k, (addr, name, inst)) in r.milestones.iter().enumerate() {
            let label = format!("${addr:04X} {name}");
            stages = stages.child(match (inst, Some(k) == last) {
                (Some(_), true) => chip(label, TEXT, ACCENT).font_weight(FontWeight::BOLD),
                (Some(_), false) => chip(label, OK, BORDER),
                (None, _) => chip(label, MUTED, BORDER).opacity(0.5),
            });
            if Some(k) == last && r.blocker.is_some() {
                stages = stages.child(chip("■ BLOCKED", ERR, ERR).font_weight(FontWeight::BOLD));
            }
        }
        let mut st = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_xs().font_family(MONO).child(format!(
                "{} instr · {} E cycles · {:.1} s virtual (assumed 2 MHz E) · PC ${:04X} · {} request(s) in this session, last from instruction {}",
                r.executed,
                r.cycles,
                ms(r.cycles) / 1000.,
                r.regs.pc,
                s.chunks,
                r.start
            )))
            .child(stages)
            .child(line("Landmarks come from static disassembly; reaching one only means its first instruction executed.", MUTED));
        if let Some(b) = &r.blocker {
            let pending = match (b.address, b.value) {
                (Some(a), Some(v)) => format!("{} ${a:04X} ← ${v:02X}", b.kind),
                (Some(a), None) => format!("{} ${a:04X}", b.kind),
                _ => b.kind.clone(),
            };
            st = st.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(ERR))
                    .child(div().text_sm().text_color(rgb(ERR)).font_weight(FontWeight::BOLD).child(format!("Next instruction not executed: {pending}")))
                    .child(div().font_family(MONO).text_xs().child(format!("${:04X}  {}  {}", b.pc, b.bytes, b.text)))
                    .child(line(b.reason.clone(), TEXT))
                    .child(line("µCsim disassembly text is display only; the bytes are authoritative.", MUTED)),
            );
        }
        st.child(kv("Registers", regs_text(&r.regs)).font_family(MONO))
    }

    /// PC against instruction count from the session's samples, over address-region bands.
    fn timeline(&self, r: &emu::Run, s: &Session) -> Div {
        const W: f32 = 720.;
        const H: f32 = 150.;
        const L: f32 = 52.;
        let (pw, ph) = (W - L - 8., H - 22.);
        let first = s.trail.front().map_or(0, |t| t.0);
        let span = (r.executed - first).max(1) as f32;
        let y = |pc: u16| 4. + ph * (1. - pc as f32 / 65535.);
        let x = |i: u64| L + pw * i.saturating_sub(first) as f32 / span;
        let pts: Vec<(f32, f32)> = s.trail.iter().map(|&(i, _, pc)| (x(i), y(pc))).collect();
        let bands: Vec<f32> = r.regions.iter().skip(1).map(|g| y(g.lo)).collect();
        let marks: Vec<f32> = r.milestones.iter().filter_map(|m| m.2).filter(|&i| i >= first).map(x).collect();
        let blocked = r.blocker.as_ref().map(|b| (x(r.executed), y(b.pc)));
        let cv = canvas(
            |_, _, _| (),
            move |b, _, window, _| {
                let o = b.origin;
                let pt = |q: (f32, f32)| point(o.x + px(q.0), o.y + px(q.1));
                let seg = |window: &mut Window, a: (f32, f32), c: (f32, f32), w: f32, color: Hsla| {
                    let mut pb = PathBuilder::stroke(px(w));
                    pb.move_to(pt(a));
                    pb.line_to(pt(c));
                    if let Ok(path) = pb.build() {
                        window.paint_path(path, color);
                    }
                };
                for &by in &bands {
                    seg(window, (L, by), (L + pw, by), 1., rgb(BORDER).into());
                }
                for &mx in &marks {
                    seg(window, (mx, 4.), (mx, 4. + ph), 1., hsla(0.38, 0.5, 0.45, 0.35));
                }
                if pts.len() > 1 {
                    let mut pb = PathBuilder::stroke(px(1.5));
                    for (k, q) in pts.iter().enumerate() {
                        if k == 0 { pb.move_to(pt(*q)) } else { pb.line_to(pt(*q)) }
                    }
                    if let Ok(path) = pb.build() {
                        window.paint_path(path, rgb(ACCENT));
                    }
                }
                if let Some((bx, by)) = blocked {
                    seg(window, (bx - 5., by - 5.), (bx + 5., by + 5.), 2., rgb(ERR).into());
                    seg(window, (bx - 5., by + 5.), (bx + 5., by - 5.), 2., rgb(ERR).into());
                }
            },
        )
        .size_full();
        let lbl = |x: f32, yy: f32, w: f32, t: String| div().absolute().left(px(x)).top(px(yy)).w(px(w)).text_size(px(10.)).text_color(rgb(MUTED)).child(t);
        let mut area = div().relative().w(px(W)).h(px(H)).rounded_md().bg(rgb(BG)).child(cv);
        // Uniform address ticks stay legible where several small regions share the bottom band.
        for address in [0xFFFF, 0xC000, 0x8000, 0x4000, 0x0000] {
            area = area.child(lbl(2., (y(address) - 6.).max(0.), L - 4., format!("${address:04X}")));
        }
        area = area.child(lbl(L, ph + 6., 160., format!("instruction {first}"))).child(lbl(W - 160., ph + 6., 158., format!("instruction {}", r.executed)));
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(heading(format!("PC TIMELINE · LAST {} SAMPLES OF THE ACTUAL RUN (64 PER REQUEST) · GREEN: LANDMARKS · RED ×: BLOCKED INSTRUCTION", s.trail.len())))
            .child(area)
    }

    /// The board model's address regions with fetch/read/write counts; the blocked region is red.
    fn memory_map(&self, r: &emu::Run) -> Div {
        let max = r.regions.iter().map(|g| g.fetch + g.read + g.write).max().unwrap_or(0);
        let hit = r.blocker.as_ref().and_then(|b| b.address);
        let mut row = div().flex().gap_1();
        for g in &r.regions {
            let total = g.fetch + g.read + g.write;
            let blocked = hit.is_some_and(|a| (g.lo..=g.hi).contains(&a));
            let mut cell = div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .p_1()
                .rounded_md()
                .border_1()
                .border_color(rgb(if blocked { ERR } else { BORDER }))
                .bg(if total > 0 { heat(level(total, max)) } else { rgb(PANEL2).into() })
                .child(div().text_xs().font_weight(FontWeight::BOLD).child(g.name.clone()))
                .child(div().text_size(px(10.)).font_family(MONO).child(format!("${:04X}–${:04X}", g.lo, g.hi)))
                .child(div().text_size(px(10.)).font_family(MONO).child(format!("F {}  R {}  W {}", g.fetch, g.read, g.write)));
            if blocked {
                cell = cell.child(div().text_size(px(10.)).text_color(rgb(ERR)).font_weight(FontWeight::BOLD).child(format!("BLOCKED ${:04X}", hit.unwrap_or_default())));
            }
            row = row.child(cell);
        }
        let mut notes = div().flex().flex_col();
        if r.rom_same_writes > 0 {
            notes = notes.child(line(format!("H1: {} EPROM-address writes of the byte already there were dropped (copy loop $8000–$9338).", r.rom_same_writes), WARN));
        }
        for (a, v) in &r.latches {
            notes = notes.child(line(format!("H1 latch ${a:04X} = ${v:02X} · function unknown, no effect modelled", ), WARN));
        }
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(heading("MEMORY MAP · BOARD MODEL REGIONS · F = OPCODE/OPERAND FETCH, R = DATA READ, W = DATA WRITE"))
            .child(row)
            .child(notes)
    }

    /// Instructions started per 256-byte page, 16 × 16 (row = high nibble of the page).
    fn page_grid(&self, r: &emu::Run) -> Div {
        let max = r.pages.iter().map(|p| p.1).max().unwrap_or(0);
        let counts: HashMap<u8, u64> = r.pages.iter().copied().collect();
        let blocked_page = r.blocker.as_ref().map(|b| (b.pc >> 8) as u8);
        let mut columns = div().flex().gap(px(1.)).child(div().w(px(18.)));
        for lo in 0..16u8 {
            columns = columns.child(div().w(px(12.)).text_size(px(9.)).text_color(rgb(MUTED)).child(format!("{lo:X}")));
        }
        let mut grid = div().flex().flex_col().gap(px(1.)).child(columns);
        for hi in 0..16u8 {
            let mut row = div().flex().gap(px(1.)).child(div().w(px(18.)).text_size(px(9.)).text_color(rgb(MUTED)).child(format!("{hi:X}·")));
            for lo in 0..16u8 {
                let p = hi << 4 | lo;
                let n = counts.get(&p).copied().unwrap_or(0);
                let mut cell = div().w(px(12.)).h(px(12.)).bg(if n > 0 { heat(level(n, max)) } else { rgb(PANEL2).into() });
                if Some(p) == blocked_page {
                    cell = cell.border_1().border_color(rgb(ERR));
                }
                row = row.child(cell);
            }
            grid = grid.child(row);
        }
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(heading("CODE ACTIVITY · INSTRUCTIONS PER 256-BYTE PAGE"))
            .child(grid)
            .child(line("Row B, column 8 = $B800–$B8FF · colour shows instruction count", MUTED))
            .child(line(format!("{} opcodes executed, {} have conformance cases", r.opcodes.0, r.opcodes.1), MUTED))
    }

    /// Modelled HC11 registers the firmware touched; the blocking access is listed first in red.
    fn io_table(&self, r: &emu::Run) -> Div {
        let mut t = div().flex().flex_col().font_family(MONO).text_xs();
        t = t.child(div().text_color(rgb(MUTED)).child(format!("{:<6} {:<8} {:>4} {:>10} {:>6} {:>6}", "addr", "name", "val", "bits", "reads", "writes")));
        if let Some(b) = r.blocker.as_ref().filter(|b| b.address.is_some()) {
            t = t.child(div().text_color(rgb(ERR)).font_weight(FontWeight::BOLD).child(format!("${:04X} {:<8} not modelled: {}", b.address.unwrap_or_default(), "", b.kind)));
        }
        for io in &r.io {
            let bits: String = (0..8).rev().map(|bit| {
                if io.unknown & (1 << bit) != 0 { '?' } else if io.value & (1 << bit) != 0 { '1' } else { '0' }
            }).collect();
            let value = if io.unknown == 0 { format!("${:02X}", io.value) } else { " ? ".into() };
            t = t.child(div().child(format!("${:04X} {:<8} {value} {bits:>10} {:>6} {:>6}", io.addr, io.name, io.reads, io.writes)));
        }
        let since = r.cycles.saturating_sub(r.cop_cycle);
        div()
            .flex()
            .flex_col()
            .gap_1()
            .flex_1()
            .child(heading("HC11 REGISTERS · VALUES AT THE CURRENT INSTRUCTION"))
            .child(t)
            .child(line("? = unknown input bit copied into the output latch", MUTED))
            .child(kv("TCNT (free-running)", format!("${:04X} · prescaler from TMSK2", r.tcnt)))
            .child(kv("COP watchdog", format!("{} service(s) ($55/$AA); last {:.1} ms ago virtual", r.cop_services, ms(since))))
    }

    /// Register-block events, latest first; the full list and samples are in the collapsible raw view.
    fn events(&self, r: &emu::Run, cx: &mut Context<Self>) -> Div {
        let toggle = btn("raw", if self.emu.raw { "Hide raw details" } else { "Show raw details" }, self.emu.raw).on_click(cx.listener(|this, _, _, cx| {
            this.emu.raw = !this.emu.raw;
            cx.notify();
        }));
        let shown = if self.emu.raw { r.events.len() } else { 12 };
        let mut list = div().flex().flex_col().font_family(MONO).text_xs();
        for e in r.events.iter().rev().take(shown) {
            let (what, color) = match e.kind {
                'W' => ("write", TEXT),
                'R' => ("read ", TEXT),
                'L' => ("latch", WARN),
                'C' => ("COP  ", OK),
                'I' => ("IRQ  ", ACCENT),
                'T' => ("RTI →", ACCENT),
                'O' => ("pins ", WARN),
                'S' => ("SPI →", WARN),
                'X' => ("SCI →", WARN),
                _ => ("EEPRM", WARN),
            };
            let name = emu_reg_name(r, e.addr);
            let port_rmw = e.addr == 0x1008 && r.events.iter().any(|other| other.inst == e.inst && other.addr == e.addr && other.kind == 'R');
            let value = if port_rmw { "input bits unknown".into() } else { format!("${:02X}", e.value) };
            list = list.child(div().text_color(rgb(color)).child(format!("#{:<7} {:>9} cyc  ${:04X}  {what} ${:04X} {:<7} {value}", e.inst, e.cycle, e.pc, e.addr, name)));
        }
        if r.events.is_empty() {
            list = list.child(line("none", MUTED));
        }
        let mut c = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().flex().gap_2().items_center().child(heading(format!("PERIPHERAL EVENTS · {} TOTAL, LATEST 256 KEPT, LATEST FIRST", r.events_total))).child(toggle))
            .child(list);
        if self.emu.raw {
            let mut samples = div().flex().flex_col().font_family(MONO).text_xs();
            for (i, cyc, pc) in &r.samples {
                samples = samples.child(div().child(format!("#{i:<7} {cyc:>9} cyc  PC ${pc:04X}")));
            }
            c = c
                .child(kv("Board model", r.board_text.clone()))
                .child(kv("CPU core", r.core.clone()))
                .child(kv("Last request", format!("{} instructions requested from instruction {}, {} executed in total", r.budget, r.start, r.executed)))
                .child(heading("PC SAMPLES"))
                .child(samples);
        }
        c
    }

    fn lookup_card(&self, d: &Dump, cx: &mut Context<Self>) -> Div {
        let co = self.emu.coord;
        let source = match emu::table_edits(&d.bytes, self.bytes_of(d)) {
            Ok(e) if e.is_empty() => line("Uses the original source bytes (no session edits).", MUTED),
            Ok(e) => line(format!("Uses the session working copy: {} edited byte(s), all inside 0x91AB..0x9282; everything else is the original.", e.len()), ACCENT),
            Err(e) => line(e, ERR),
        };
        let mut c = card()
            .child(heading("ISOLATED LOOKUP · ORIGINAL ROUTINE $B1C5..$B217 · TABLE $91AB, STRIDE 9"))
            .child(line("Inputs are table coordinates and byte fractions (n/256) in the routine's RAM descriptor. They are not RPM, throttle, load or any sensor unit.", WARN))
            .child(self.stepper("row", "Row index (0–22)", co.row, Coord::ROW_MAX, |c| &mut c.row, cx))
            .child(self.stepper("col", "Column index (0–7)", co.col, Coord::COL_MAX, |c| &mut c.col, cx))
            .child(self.stepper("rf", "Row fraction (/256)", co.row_frac, 255, |c| &mut c.row_frac, cx))
            .child(self.stepper("cf", "Column fraction (/256)", co.col_frac, 255, |c| &mut c.col_frac, cx))
            .child(div().flex().gap_2().items_center().child(self.emu_btn("lookup", "Run lookup", !self.emu.busy, |this, cx| this.emu_run(Request::Lookup(this.emu.coord), cx), cx)).child(source));
        let snap = self.emu_snap();
        let cur = self.emu.lookup.as_ref().filter(|(s, _)| Some(s) == snap.as_ref()).map(|(_, r)| r);
        if cur.is_none() && self.emu.lookup.is_some() {
            c = c.child(line("The last lookup used other bytes (target or working copy changed); run it again.", MUTED));
        }
        let l = match cur {
            None => return c,
            Some(Err(e)) => return c.child(line(e.clone(), ERR)),
            Some(Ok(l)) => l,
        };
        let k = l.coord;
        let names = ["[r, c]", "[r, c+1]", "[r+1, c]", "[r+1, c+1]"];
        let mut cells = div().flex().flex_col().font_family(MONO).text_xs();
        for (n, (a, v)) in names.iter().zip(l.cells) {
            cells = cells.child(div().child(format!("{n:<11} ${a:04X} = {v:>3} (${v:02X})")));
        }
        c.child(div().text_lg().font_weight(FontWeight::BOLD).child(format!("Result A = {} (${:02X}) · raw unsigned byte", l.result, l.result)))
            .child(div().flex().gap_4().items_start().child(four_cells(l)).child(cells))
            .child(kv("For", format!("row {} + {}/256, column {} + {}/256", k.row, k.row_frac, k.col, k.col_frac)))
            .child(kv("Execution", format!("{} instructions; stopped before RTS at $B218 · result checked against the recovered arithmetic", l.instructions)))
            .child(kv("Registers", regs_text(&l.regs)).font_family(MONO))
            .child(kv("Source SHA-256", d.sha256.clone()))
            .child(kv("Working SHA-256", if l.edits > 0 { format!("{} · {} session edit(s) in the table", l.working_sha, l.edits) } else { format!("{} · original, no edits", l.working_sha) }))
    }
}

fn emu_reg_name(r: &emu::Run, addr: u16) -> String {
    r.io.iter().find(|i| i.addr == addr).map_or_else(|| if addr == 0x103A { "COPRST".into() } else { String::new() }, |i| i.name.clone())
}

/// The four neighbouring cells as a 2 × 2 heat grid; the dot marks the interpolation point
/// between the cell centres, and the swatch shows the result on the same colour scale.
fn four_cells(l: &emu::Lookup) -> Div {
    const S: f32 = 72.;
    let cell = |v: u8| {
        div().w(px(S)).h(px(S)).flex().items_center().justify_center().bg(heat(v as f32 / 255.)).text_sm().font_family(MONO).text_color(rgb(TEXT)).child(v.to_string())
    };
    let v = |k: usize| l.cells[k].1;
    let (fx, fy) = (l.coord.col_frac as f32 / 256., l.coord.row_frac as f32 / 256.);
    let dot = div()
        .absolute()
        .left(px(S / 2. + fx * S - 6.))
        .top(px(S / 2. + fy * S - 6.))
        .w(px(12.))
        .h(px(12.))
        .rounded_full()
        .border_2()
        .border_color(rgb(TEXT))
        .bg(heat(l.result as f32 / 255.));
    let grid = div()
        .relative()
        .w(px(2. * S))
        .h(px(2. * S))
        .child(div().flex().child(cell(v(0))).child(cell(v(1))))
        .child(div().flex().child(cell(v(2))).child(cell(v(3))))
        .child(dot);
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(heading("FOUR-CELL INTERPOLATION"))
        .child(grid)
        .child(div().flex().items_center().gap_2().child(div().w(px(16.)).h(px(16.)).bg(heat(l.result as f32 / 255.))).child(line(format!("result {} · rows down, columns right", l.result), MUTED)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_identity_includes_every_working_byte() {
        let original = vec![0; 65536];
        let snapshot = |bytes: &[u8]| Snap::new("dump.bin".into(), "source-sha".into(), Some(bytes));
        let before = snapshot(&original);
        let mut working = original.clone();
        working[0x802A] = 1;
        assert!(snapshot(&working) != before);
        assert!(emu::table_edits(&original, &working).is_err());
        working[0x802A] = 0;
        assert!(snapshot(&working) == before);
    }

    fn session(result: Result<emu::Run, String>) -> Session {
        Session { snap: Snap::new("dump.bin".into(), "source-sha".into(), None), board: Board::H2, inputs: Inputs::default(), result, trail: VecDeque::new(), chunks: 1 }
    }

    #[test]
    fn invalidation_cancels_inflight_requests_and_drops_results() {
        let mut emu = Emu { busy: true, running: true, ..Emu::default() };
        let (request_generation, cancel) = (emu.generation, emu.cancel.clone());
        emu.session = Some(session(Err("old run error".into())));
        emu.lookup = Some((Snap::new("dump.bin".into(), "source-sha".into(), None), Err("old lookup error".into())));
        emu.invalidate();
        assert_ne!(request_generation, emu.generation);
        assert!(cancel.load(Ordering::Relaxed), "the in-flight backend process is told to stop");
        assert!(!emu.cancel.load(Ordering::Relaxed), "the next request gets a fresh flag");
        assert!(!emu.busy && !emu.running && emu.session.is_none() && emu.lookup.is_none());
    }

    fn run_at(executed: u64, blocked: bool) -> emu::Run {
        let regs = emu::Regs { pc: 0, sp: 0, a: 0, b: 0, x: 0, y: 0, cc: 0 };
        let blocker = blocked.then(|| emu::Blocker { pc: 0, bytes: String::new(), text: String::new(), kind: "read".into(), address: Some(0x1060), value: None, reason: String::new() });
        let checkpoint = (!blocked).then(|| emu::Checkpoint { state: "s".into(), digest: "d".into(), inst: executed });
        emu::Run {
            board: Board::H2, board_text: String::new(), core: String::new(), budget: CHUNK, start: 0, executed, cycles: 0, regs,
            samples: vec![], blocker, milestones: vec![], regions: vec![], io: vec![], latches: vec![], events: vec![], events_total: 0,
            pages: vec![], cop_services: 0, cop_cycle: 0, tcnt: 0, rom_same_writes: 0, opcodes: (0, 0), cop_gap_max: 0, watch: vec![],
            interrupts: vec![], rti: 0, main_loop: emu::Loop { pc: 0xD4A2, count: 0, sp: None, gap_max: 0 }, outputs: emu::Outputs::default(),
            eeprom: None, ram_internal: vec![0; 256], ram_external: vec![0; 2048], checkpoint,
        }
    }

    #[test]
    fn run_continues_from_checkpoints_until_blocked_or_failed() {
        assert_eq!(continuation(None), Some(None)); // a new session starts at reset
        let s = session(Ok(run_at(4_000_000, false)));
        assert_eq!(continuation(Some(&s)).flatten().map(|c| c.inst), Some(4_000_000)); // no total cap
        assert_eq!(continuation(Some(&session(Ok(run_at(68_202, true))))), None);
        assert_eq!(continuation(Some(&session(Err("backend failed".into())))), None);
    }

    #[test]
    fn timeline_trail_is_bounded_and_continuous() {
        let mut trail = VecDeque::new();
        extend_trail(&mut trail, &[(0, 0, 0xB800), (10, 40, 0xB81B)]);
        extend_trail(&mut trail, &[(10, 40, 0xB81B), (20, 80, 0xB830)]); // next chunk repeats its start sample
        assert_eq!(trail.len(), 3);
        let many: Vec<_> = (0..3 * TRAIL as u64).map(|i| (i, i, 0)).collect();
        extend_trail(&mut trail, &many);
        assert_eq!((trail.len(), trail.back().map(|t| t.0)), (TRAIL, Some(3 * TRAIL as u64 - 1)));
    }
}
