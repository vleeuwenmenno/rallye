//! Emulation tab: the original firmware from reset on the patched µCsim board model, plus one
//! isolated lookup. A session runs in bounded chunks; each chunk continues from the previous
//! chunk's checkpoint in a fresh simulator process, until Pause, Reset or a blocker. Under H2 the
//! inputs (a commanded crank-wheel speed and raw sensor values) may change between chunks without
//! a reset. Nothing here is an engine model: speed is commanded, never derived from throttle.

use super::*;
use crate::emu::{self, Board, Coord, Inputs, Outcome, Request, RunReq};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Instructions per Run chunk (one backend request, about 4 s virtual at the assumed 2 MHz E clock).
const CHUNK: u32 = emu::MAX_RUN;
/// PC samples kept for the timeline across chunks; older ones are dropped.
const TRAIL: usize = 1024;
/// Firmware-reading samples kept for the engine plots (64 per request, about 20 requests).
const HISTORY: usize = 1280;
/// The A/D channel the firmware trace makes the throttle candidate (AN2 → $2003 → $C6, see the docs).
const TPS_CANDIDATE: usize = 2;
const NO_PERIOD: u16 = 0xFFFF;

/// One firmware-reading sample for the engine plots.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Point {
    inst: u64,
    /// Commanded crank speed during the request (stimulus).
    commanded: u16,
    /// $D1:$D2 when the firmware has a valid period ($BB ≠ $FFFF).
    firmware: Option<u16>,
    /// Raw A/D input of AN2, and the firmware's validated AN2 value $C6.
    tps_in: u8,
    tps_fw: u8,
}

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
    history: VecDeque<Point>,
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
    /// Raw H2 input controls shown (the engine card covers crank speed and throttle).
    raw_inputs: bool,
    /// Crank speed the Start button commands; inputs.crank_rpm is 0 while stopped.
    crank_target: u16,
    /// Throttle stimulus A/D channel and (closed, open) raw endpoints (None: calibration defaults).
    tps_ch: usize,
    tps_ends: Option<(u8, u8)>,
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
            raw_inputs: false,
            crank_target: 900,
            tps_ch: TPS_CANDIDATE,
            tps_ends: None,
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

/// Display the actual selected raw stimulus, including raw edits and channel changes.
fn throttle_percent(raw: u8, (closed, open): (u8, u8)) -> Option<u8> {
    if closed == open || raw < closed.min(open) || raw > closed.max(open) {
        return None;
    }
    Some(((raw as f64 - closed as f64) * 100. / (open as f64 - closed as f64)).round() as u8)
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

/// Firmware speed and throttle readings of a request's samples, appended to the bounded history.
fn extend_history(history: &mut VecDeque<Point>, run: &emu::Run) {
    let (Some(bb), Some(d1), Some(c6)) = (run.probe(0xBB), run.probe(0xD1), run.probe(0xC6)) else { return };
    for t in &run.trace {
        if history.back().is_some_and(|p| p.inst >= t.inst) {
            continue; // a chunk's first sample repeats the previous chunk's last one
        }
        let word = |k: usize| u16::from_be_bytes([t.probe[k], t.probe[k + 1]]);
        let firmware = (word(bb) != NO_PERIOD).then(|| word(d1));
        history.push_back(Point { inst: t.inst, commanded: run.inputs.crank_rpm, firmware, tps_in: run.inputs.adc[TPS_CANDIDATE], tps_fw: t.probe[c6] });
    }
    while history.len() > HISTORY {
        history.pop_front();
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

    /// The session for the current bytes and profile, if any; never a stale one. Its inputs may
    /// differ from the current ones: they take effect at the next chunk boundary.
    fn emu_session(&self) -> Option<&Session> {
        let snap = self.emu_snap();
        self.emu.session.as_ref().filter(|s| Some(&s.snap) == snap.as_ref() && s.board == self.emu.board)
    }

    /// Run one backend request off the UI thread. A Run result is kept only if the session identity
    /// (target bytes, working bytes, profile) is unchanged and no newer request was issued. Inputs
    /// changed meanwhile do not invalidate it: the result belongs to the inputs it ran with, and the
    /// next chunk applies the new ones from its checkpoint on.
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
                if this.emu_snap().as_ref() != Some(&snap) || this.emu.board != board {
                    this.emu.reset_session();
                    this.msg = Some(("Emulation result discarded: the target, its working copy or the profile changed while it ran".into(), WARN));
                    return cx.notify();
                }
                this.emu_store(snap, board, inputs, r.map_err(|e| e.to_string()), lookup);
                // Continue a Run with the next chunk and the current inputs; Pause, a blocker or an error ends it.
                if this.emu.running && !lookup {
                    match continuation(this.emu_session()) {
                        Some(Some(c)) => {
                            let inputs = this.emu.effective_inputs();
                            this.emu_run(Request::Run(RunReq { board, budget: CHUNK, inputs, checkpoint: Some(c) }), cx)
                        }
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
                let keep = self.emu.session.take().filter(|s| s.snap == snap && s.board == board);
                let (mut trail, mut history, chunks) = keep.map_or((VecDeque::new(), VecDeque::new(), 0), |s| (s.trail, s.history, s.chunks));
                if let Ok(run) = &result {
                    extend_trail(&mut trail, &run.samples);
                    extend_history(&mut history, run);
                }
                self.emu.session = Some(Session { snap, board, inputs, result, trail, history, chunks: chunks + 1 });
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

    /// Change one H2 input. It takes effect at the next chunk boundary without a reset, except the
    /// EEPROM power-up fill, which only matters at the first EEPROM access and so resets the session.
    fn set_input(&mut self, field: Field, value: u8) {
        if *field.get(&mut self.emu.inputs) == value {
            return;
        }
        *field.get(&mut self.emu.inputs) = value;
        if matches!(field, Field::Fill) && (self.emu.session.is_some() || self.emu.busy) {
            self.emu.reset_session();
            self.emu.note = Some("EEPROM fill changed: it only applies at power-up, so the session was reset. Run starts again from reset.".into());
        }
    }

    fn set_crank(&mut self, rpm: u16) {
        self.emu.inputs.crank_rpm = rpm.min(emu::CRANK_MAX);
    }

    /// (closed, open) raw endpoints of the throttle stimulus: explicit, else the calibration bytes the
    /// firmware loads as its initial learned endpoints ($88FE → $0E, $88FD → $0D).
    fn tps_ends(&self) -> (u8, u8) {
        self.emu.tps_ends.unwrap_or_else(|| self.emu_target().map_or((0x40, 0xD0), |d| (d.bytes[0x88FE], d.bytes[0x88FD])))
    }

    /// Current input expressed between the selected endpoints, or unknown when outside them.
    fn tps_pct(&self) -> Option<u8> {
        throttle_percent(self.emu.inputs.adc[self.emu.tps_ch], self.tps_ends())
    }

    /// Write the requested percentage into the selected A/D channel.
    fn apply_tps(&mut self, pct: u8) {
        let (closed, open) = self.tps_ends();
        let raw = closed as i32 + ((open as f64 - closed as f64) * pct as f64 / 100.).round() as i32;
        self.set_input(Field::Adc(self.emu.tps_ch), raw.clamp(0, 255) as u8);
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
            .child(heading("H2 RAW INPUTS · RAW UNITS · HYPOTHETICAL DEFAULTS · PHYSICAL MEANING UNKNOWN UNLESS NOTED"))
            .child(line("A change applies at the next chunk boundary (the session continues; only the EEPROM fill resets it). The throttle control above writes its channel here. * = bit the firmware is seen to read.", WARN))
            .child(self.bit_row("i60-", "Board input byte $1060", 0xFF, &[(5, "branch at $A86A"), (1, "stored at $A035")], Field::In1060, cx))
            .child(self.bit_row("i80-", "Board input byte $1080", 0xFF, &[(4, "tested at $BB51/$BB61")], Field::In1080, cx))
            .child(self.bit_row("pa-", "PORTA input pins PA2, PA1 (PA7/PA0: crank)", 0x06, &[], Field::PortA, cx))
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
        root.child(head).child(self.engine_card(cx)).child(self.run_card(cx)).child(self.lookup_card(d, cx)).child(
            card()
                .child(heading("ENGINE OUTPUTS"))
                .child(line("Injector pulse width and ignition timing are NOT shown as such: no output pin, latch bit or SPI byte has been mapped to an injector or coil. There is no engine model: the crank speed is commanded, not produced by combustion, and throttle does not change it.", WARN))
                .child(line("Available instead: the firmware's own speed and throttle readings (engine-input card), raw PORTA output-compare pins with their last pulse widths, latch bit activity, SPI frames and SCI characters (run below), map reads in the isolated lookup.", MUTED)),
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
            .child(heading("FIRMWARE FROM RESET · ORIGINAL SOURCE BYTES · KEY ON"))
            .child(profiles)
            .child(controls)
            .child(div().h(px(18.)).text_sm().font_weight(FontWeight::BOLD).text_color(rgb(color)).child(state))
            .child(line(
                format!("Run executes {CHUNK} instructions per backend request and continues from that request's checkpoint (exact continuation, tested against one long run) until Pause, Reset or a blocker; there is no total cap. H2 input changes apply at the next request boundary. Execution stops before the first instruction that needs unmodelled hardware or uses an opcode without a conformance case. Session edits are not used, but any edit resets the session."),
                MUTED,
            ));
        match self.emu.board {
            Board::H1 => {
                c = c.child(div().p_2().rounded_md().border_1().border_color(rgb(WARN)).child(line("H1 is a working hypothesis, not a validated ECU board: $1040/$1050 writes are treated as write-only latches with unknown effect, EPROM-address writes of the byte already there are dropped, and the diagnostic serial line is idle. Any other external access still stops.", WARN)))
            }
            Board::H2 => {
                let toggle = btn("raw-inputs", if self.emu.raw_inputs { "Hide raw inputs" } else { "Show raw inputs ($1060, $1080, ports, A/D, EEPROM fill)" }, self.emu.raw_inputs).on_click(cx.listener(|this, _, _, cx| {
                    this.emu.raw_inputs = !this.emu.raw_inputs;
                    cx.notify();
                }));
                c = c.child(div().flex().child(toggle));
                if self.emu.raw_inputs {
                    c = c.child(self.inputs_panel(cx));
                }
            }
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
        let width = |c: u64| if c == 0 { "—".to_string() } else { format!("{:.2} ms", ms(c)) };
        let pins: String = (3..8).rev().map(|b| format!("PA{b}={} ({} changes, last high {}, low {})  ", (o.porta_pins >> b) & 1, o.porta_changes[b], width(o.porta_high[b]), width(o.porta_low[b]))).collect();
        let latch = |base: usize| (0..8).rev().map(|b| o.latch_changes[base + b].to_string()).collect::<Vec<_>>().join(" ");
        let bytes = |v: &[u8]| v.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ");
        let mut c = div()
            .flex()
            .flex_col()
            .gap_1()
            .flex_1()
            .child(heading("RAW OUTPUTS · MAPPING TO ACTUATORS UNKNOWN"))
            .child(kv("PORTA output pins", pins).font_family(MONO))
            .child(kv("PORTA latch", format!("${:02X}", o.porta_latch)).font_family(MONO))
            .child(kv("Latch bit changes b7…b0", format!("$1040: {} · $1050: {}", latch(0), latch(8))).font_family(MONO))
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

    /// A row of buttons that each run `on(this, value)`; `active` marks the current value.
    fn choice_row<T: Copy + PartialEq + 'static>(&self, id: &'static str, label: &str, items: &[(T, String)], active: Option<T>, on: fn(&mut Self, T), cx: &mut Context<Self>) -> Div {
        let mut row = div().flex().flex_wrap().items_center().gap_1().child(div().w(px(150.)).flex_none().text_xs().text_color(rgb(MUTED)).child(label.to_string()));
        for (k, (v, text)) in items.iter().enumerate() {
            let v = *v;
            row = row.child(btn(SharedString::from(format!("{id}{k}")), text.clone(), active == Some(v)).on_click(cx.listener(move |this, _, _, cx| {
                on(this, v);
                cx.notify();
            })));
        }
        row
    }

    /// Top-level engine inputs: commanded crank speed and a throttle stimulus, with the firmware's own
    /// readings next to them. Requested values are stimuli; measured values come from firmware RAM.
    fn engine_card(&self, cx: &mut Context<Self>) -> Div {
        let mut c = card().child(heading("ENGINE INPUTS · SIMULATED CRANK SIGNAL AND THROTTLE STIMULUS · NO ENGINE MODEL"));
        if self.emu.board != Board::H2 {
            return c.child(line("Crank and throttle inputs need the H2 simulation profile (select it below).", MUTED));
        }
        let i = self.emu.inputs;
        let running = i.crank_rpm > 0;
        let start = btn("crank-toggle", if running { "■ Stop crank" } else { "▶ Start crank" }, running).on_click(cx.listener(|this, _, _, cx| {
            let rpm = if this.emu.inputs.crank_rpm > 0 { 0 } else { this.emu.crank_target };
            this.set_crank(rpm);
            cx.notify();
        }));
        let target = self.emu.crank_target;
        let mut speed = div().flex().flex_wrap().items_center().gap_1().child(div().w(px(150.)).flex_none().text_xs().text_color(rgb(MUTED)).child("Commanded speed"));
        for (k, delta) in [-500i32, -100, -10, 10, 100, 500].into_iter().enumerate() {
            if k == 3 {
                speed = speed.child(div().w(px(80.)).flex().justify_center().font_family(MONO).text_sm().child(format!("{target} rpm")));
            }
            let text = if delta < 0 { format!("−{}", -delta) } else { format!("+{delta}") };
            speed = speed.child(btn(SharedString::from(format!("rpm{k}")), text, false).on_click(cx.listener(move |this, _, _, cx| {
                this.emu.crank_target = (this.emu.crank_target as i32 + delta).clamp(30, emu::CRANK_MAX as i32) as u16;
                if this.emu.inputs.crank_rpm > 0 {
                    this.set_crank(this.emu.crank_target);
                }
                cx.notify();
            })));
        }
        let presets: Vec<(u16, String)> = [(250, "250 cranking"), (900, "900 idle"), (3000, "3000"), (6000, "6000")].iter().map(|&(v, t)| (v, t.to_string())).collect();
        let speed_presets = self.choice_row("rpmp", "Presets (start crank)", &presets, running.then_some(i.crank_rpm), |this, v| {
            this.emu.crank_target = v;
            this.set_crank(v);
        }, cx);
        let pct = self.tps_pct();
        let mut throttle = div().flex().flex_wrap().items_center().gap_1().child(div().w(px(150.)).flex_none().text_xs().text_color(rgb(MUTED)).child("Throttle stimulus"));
        for (k, delta) in [-10i32, -1, 1, 10].into_iter().enumerate() {
            if k == 2 {
                throttle = throttle.child(div().w(px(80.)).flex().justify_center().font_family(MONO).text_sm().child(pct.map_or_else(|| "—".into(), |v| format!("{v} %"))));
            }
            let text = if delta < 0 { format!("−{}", -delta) } else { format!("+{delta}") };
            throttle = throttle.child(btn(SharedString::from(format!("tps{k}")), text, false).on_click(cx.listener(move |this, _, _, cx| {
                let pct = (this.tps_pct().unwrap_or(0) as i32 + delta).clamp(0, 100) as u8;
                this.apply_tps(pct);
                cx.notify();
            })));
        }
        let pcts: Vec<(u8, String)> = [0u8, 25, 50, 75, 100].iter().map(|&v| (v, format!("{v} %"))).collect();
        let tps_presets = self.choice_row("tpsp", "Throttle presets", &pcts, pct, |this, v| {
            this.apply_tps(v);
        }, cx);
        let channels: Vec<(usize, String)> = (0..8).map(|k| (k, if k == TPS_CANDIDATE { format!("AN{k} (candidate)") } else { format!("AN{k}") })).collect();
        let channel = self.choice_row("tpsch", "Throttle A/D channel", &channels, Some(self.emu.tps_ch), |this, k| {
            this.emu.tps_ch = k;
        }, cx);
        let (closed, open) = self.tps_ends();
        let mut ends = div().flex().flex_wrap().items_center().gap_1().child(div().w(px(150.)).flex_none().text_xs().text_color(rgb(MUTED)).child("Endpoints (raw)"));
        for (k, (label, delta, is_open)) in [("closed −", -1i32, false), ("closed +", 1, false), ("open −", -1, true), ("open +", 1, true)].into_iter().enumerate() {
            ends = ends.child(btn(SharedString::from(format!("tpse{k}")), label, false).on_click(cx.listener(move |this, _, _, cx| {
                let pct = this.tps_pct().unwrap_or(0);
                let (mut c0, mut o0) = this.tps_ends();
                let v = if is_open { &mut o0 } else { &mut c0 };
                *v = (*v as i32 + delta * 4).clamp(0, 255) as u8;
                this.emu.tps_ends = Some((c0, o0));
                this.apply_tps(pct);
                cx.notify();
            })));
        }
        ends = ends
            .child(div().font_family(MONO).text_xs().child(format!("closed {closed} (${closed:02X}) · open {open} (${open:02X}) · now writes AN{} = {}", self.emu.tps_ch, i.adc[self.emu.tps_ch])))
            .child(btn("tpse-cal", "calibration defaults", self.emu.tps_ends.is_none()).on_click(cx.listener(|this, _, _, cx| {
                this.emu.tps_ends = None;
                cx.notify();
            })));
        c = c
            .child(div().flex().items_center().gap_2().child(start).child(line(if running { format!("crank turning at {} rpm (commanded)", i.crank_rpm) } else { "crank stopped (0 rpm commanded)".into() }, if running { OK } else { MUTED })))
            .child(speed)
            .child(speed_presets)
            .child(throttle)
            .child(tps_presets)
            .child(channel)
            .child(ends)
            .child(line("Crank: a 60-2 wheel signal (58 teeth, gap after tooth 58) on PA7/PAI and PA0/IC3, falling edge active, as the firmware's own timer setup and tooth decoder imply; board wiring, sensor polarity and the 2 MHz E clock that converts rpm to timer counts are unverified. Speed and throttle are independent stimuli: throttle never changes speed.", WARN))
            .child(line("Throttle: AN2 is the candidate from the firmware trace (range check, closed/open flags and endpoint learning on its copy $2003 → $C6); not proven on hardware. Percent maps linearly between the endpoints, which default to the calibration's initial learned values ($88FE closed, $88FD open). A change applies to A/D conversions completed after the next chunk boundary.", WARN));
        let session = self.emu_session();
        if let Some(s) = session.filter(|s| s.inputs != self.emu.inputs) {
            let when = match continuation(Some(s)).flatten() {
                _ if self.emu.busy => "at the end of the request in flight".to_string(),
                Some(cp) => format!("from instruction {} when Run or Step continues", cp.inst),
                None => "only after Reset (the session cannot continue)".to_string(),
            };
            c = c.child(line(format!("Input change pending: applies {when}; the session is not reset."), ACCENT));
        }
        let Some(Session { result: Ok(r), history, .. }) = session else {
            return c.child(line("No firmware readings yet: press Run below. Readings come from the firmware's RAM after each request.", MUTED));
        };
        c.child(self.engine_readings(r)).child(self.engine_plots(history))
    }

    /// Firmware readings beside the stimulus, from RAM after the last instruction (see the docs for each address).
    fn engine_readings(&self, r: &emu::Run) -> Div {
        let b = |a: u16| r.ram(a).unwrap_or(0);
        let w = |a: u16| u16::from_be_bytes([b(a), b(a + 1)]);
        let period = w(0xBB);
        let fw = if period == NO_PERIOD { "— (no valid period: $BB = $FFFF)".to_string() } else { format!("{} rpm", w(0xD1)) };
        let regime = b(0xAE);
        let crank = r.crank.as_ref().map_or("—".into(), |k| {
            let tooth = k.tooth.map_or("gap".into(), |t| format!("tooth {t}"));
            format!("{} falling edges · {tooth} · level {} · PACNT ${:02X} · TIC3 {}", k.edges, k.level, k.pacnt, k.tic3.map_or("—".into(), |t| format!("${t:04X}")))
        });
        let aa = b(0xAA);
        let flags = format!("{}{}", if aa & 0x40 != 0 { "closed ($AA bit 6) " } else { "" }, if aa & 0x80 != 0 { "open ($AA bit 7)" } else { "" });
        let fault = b(0x1B);
        let ch = self.emu.tps_ch;
        let mut t = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(heading("REQUESTED (STIMULUS) VS MEASURED (FIRMWARE RAM) · AFTER INSTRUCTION ".to_string() + &r.executed.to_string()))
            .child(kv("Requested crank speed", format!("{} rpm", r.inputs.crank_rpm)).font_family(MONO))
            .child(kv("Firmware speed $D1", fw).font_family(MONO).text_color(rgb(if period == NO_PERIOD { MUTED } else { OK })))
            .child(kv("Firmware period $BB", if period == NO_PERIOD { "$FFFF (stopped or below about 229 rpm)".into() } else { format!("{period} timer counts per 30 teeth (= 180°)") }).font_family(MONO))
            .child(kv("Firmware tooth / regime", format!("tooth $D8 = {} · regime $AE = {regime:08b} (bit 0: low-speed/stall mode) · gap check $AD bit 4 = {} · sync errors $2093 = {}", b(0xD8), (b(0xAD) >> 4) & 1, r.ram(0x2093).unwrap_or(0))).font_family(MONO))
            .child(kv("Simulated crank signal", crank).font_family(MONO))
            .child(kv(&format!("Throttle input AN{ch}"), format!("{} raw (applied in this completed run)", r.inputs.adc[ch])).font_family(MONO))
            .child(kv("Firmware AN2 values", format!("copy $2003 = {} · validated $C6 = {} · processed $200F = {} · learned closed $0E = {} · open $0D = {} · {}", b(0x2003), b(0xC6), b(0x200F), b(0x0E), b(0x0D), if flags.is_empty() { "no endpoint flag" } else { flags.trim() })).font_family(MONO))
            .child(kv("AN2 fault flags $1B", if fault == 0 { "none".into() } else { format!("${fault:02X} · the firmware substitutes a default instead of the raw value (range $9284..$9285, step limit $928A); Reset clears it") }).font_family(MONO).text_color(rgb(if fault == 0 { TEXT } else { WARN })))
            .child(line("Firmware speed formula (code at $429B, not assumed): $D1:$D2 = 256·58594/$BB, i.e. 15,000,064/$BB; $BB is the TCNT time between teeth 20 and 50 ($9618). Reading it as rpm assumes the 2 MHz E clock and TMSK2 prescaler 4 (2 µs per count).", MUTED));
        if ch != TPS_CANDIDATE {
            t = t.child(line(format!("AN{ch} is selected: the firmware AN2 values above do not follow the throttle stimulus."), WARN));
        }
        t
    }

    /// Small live plots of the session's history: commanded vs firmware speed, throttle input vs $C6.
    fn engine_plots(&self, history: &VecDeque<Point>) -> Div {
        let rpm_max = history.iter().map(|p| p.commanded.max(p.firmware.unwrap_or(0))).max().unwrap_or(0).max(1000) as f32;
        let rpm = plot(history, rpm_max, &[(|p: &Point| Some(p.commanded as f32), ACCENT), (|p: &Point| p.firmware.map(|f| f as f32), OK)]);
        let tps = plot(history, 255., &[(|p: &Point| Some(p.tps_in as f32), ACCENT), (|p: &Point| Some(p.tps_fw as f32), OK)]);
        let first = history.front().map_or(0, |p| p.inst);
        let last = history.back().map_or(0, |p| p.inst);
        div()
            .flex()
            .gap_4()
            .items_start()
            .child(div().flex().flex_col().gap_1().child(heading(format!("SPEED · BLUE COMMANDED, GREEN FIRMWARE $D1 · 0–{rpm_max:.0} RPM"))).child(rpm))
            .child(div().flex().flex_col().gap_1().child(heading("THROTTLE AN2 · BLUE RAW INPUT, GREEN FIRMWARE $C6 · 0–255")).child(tps))
            .child(line(format!("{} samples, instructions {first}–{last}; gaps in green: no valid period", history.len()), MUTED))
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

type Series = (fn(&Point) -> Option<f32>, u32);

/// A small line plot of `series` over the history's instruction axis; a None value breaks the line.
fn plot(history: &VecDeque<Point>, y_max: f32, series: &[Series]) -> Div {
    const W: f32 = 340.;
    const H: f32 = 90.;
    let (first, last) = (history.front().map_or(0, |p| p.inst), history.back().map_or(1, |p| p.inst));
    let span = (last - first).max(1) as f32;
    type Runs = Vec<Vec<(f32, f32)>>;
    let lines: Vec<(Runs, u32)> = series
        .iter()
        .map(|&(f, color)| {
            let mut runs: Vec<Vec<(f32, f32)>> = vec![vec![]];
            for p in history {
                match f(p) {
                    Some(v) => runs.last_mut().unwrap().push((4. + (W - 8.) * (p.inst - first) as f32 / span, 4. + (H - 8.) * (1. - (v / y_max).clamp(0., 1.)))),
                    None if !runs.last().unwrap().is_empty() => runs.push(vec![]),
                    None => {}
                }
            }
            (runs, color)
        })
        .collect();
    let cv = canvas(
        |_, _, _| (),
        move |b, _, window, _| {
            let o = b.origin;
            for (runs, color) in &lines {
                for run in runs.iter().filter(|r| r.len() > 1) {
                    let mut pb = PathBuilder::stroke(px(1.5));
                    for (k, q) in run.iter().enumerate() {
                        let pt = point(o.x + px(q.0), o.y + px(q.1));
                        if k == 0 { pb.move_to(pt) } else { pb.line_to(pt) }
                    }
                    if let Ok(path) = pb.build() {
                        window.paint_path(path, rgb(*color));
                    }
                }
            }
        },
    )
    .size_full();
    div().w(px(W)).h(px(H)).rounded_md().bg(rgb(BG)).child(cv)
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
    #[test]
    fn throttle_display_tracks_raw_input_and_endpoint_direction() {
        assert_eq!(super::throttle_percent(128, (64, 208)), Some(44));
        assert_eq!(super::throttle_percent(64, (64, 208)), Some(0));
        assert_eq!(super::throttle_percent(208, (64, 208)), Some(100));
        assert_eq!(super::throttle_percent(128, (208, 64)), Some(56));
        assert_eq!(super::throttle_percent(0, (64, 208)), None);
        assert_eq!(super::throttle_percent(64, (64, 64)), None);
    }

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
        Session { snap: Snap::new("dump.bin".into(), "source-sha".into(), None), board: Board::H2, inputs: Inputs::default(), result, trail: VecDeque::new(), history: VecDeque::new(), chunks: 1 }
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
        let checkpoint = (!blocked).then(|| emu::Checkpoint { state: "s".into(), digest: "d".into(), inst: executed, inputs: Inputs::default() });
        emu::Run {
            board: Board::H2, board_text: String::new(), core: String::new(), budget: CHUNK, start: 0, executed, cycles: 0, regs,
            samples: vec![], blocker, milestones: vec![], regions: vec![], io: vec![], latches: vec![], events: vec![], events_total: 0,
            pages: vec![], cop_services: 0, cop_cycle: 0, tcnt: 0, rom_same_writes: 0, opcodes: (0, 0), cop_gap_max: 0, watch: vec![],
            interrupts: vec![], rti: 0, main_loop: emu::Loop { pc: 0xD4A2, count: 0, sp: None, gap_max: 0 }, outputs: emu::Outputs::default(),
            eeprom: None, ram_internal: vec![0; 256], ram_external: vec![0; 2048], inputs: Inputs::default(), inputs_changed: vec![],
            probes: vec![], trace: vec![], crank: None, checkpoint,
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
    fn engine_history_reads_firmware_bytes_not_inputs() {
        let mut run = run_at(2000, false);
        run.inputs = Inputs { crank_rpm: 900, adc: [0, 0, 0x40, 0, 0, 0, 0, 0], ..Inputs::default() };
        run.probes = vec![(0xBB, "p".into()), (0xBC, "p".into()), (0xD1, "r".into()), (0xD2, "r".into()), (0xC6, "t".into())];
        let sample = |inst, probe: [u8; 5]| emu::Sample { inst, cycles: inst * 4, edges: 0, probe: probe.to_vec() };
        // $BB = $FFFF: no firmware speed yet; then $BB = 16667, $D1 = 899 and $C6 lagging at $80.
        run.trace = vec![sample(1000, [0xFF, 0xFF, 0, 0, 0x80]), sample(2000, [0x41, 0x1B, 0x03, 0x83, 0x80])];
        let mut h = VecDeque::new();
        extend_history(&mut h, &run);
        extend_history(&mut h, &run); // the same samples again add nothing
        assert_eq!(h.iter().map(|p| (p.commanded, p.firmware, p.tps_in, p.tps_fw)).collect::<Vec<_>>(), vec![(900, None, 0x40, 0x80), (900, Some(899), 0x40, 0x80)]);
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
