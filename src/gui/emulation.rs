//! Emulation tab: the original firmware from reset on the patched µCsim board model, plus one
//! isolated lookup. A session runs in bounded chunks; each chunk continues from the previous
//! chunk's checkpoint in a fresh simulator process, until Pause, Reset or a blocker. Under H2 the
//! inputs (a commanded crank-wheel speed and raw sensor values) may change between chunks without
//! a reset. Nothing here is an engine model: speed is commanded, never derived from throttle.
//!
//! Layout: a fixed toolbar (Run/Pause/Step/Reset, profile), one scrolling pane chosen in the
//! sidebar, and a fixed status bar.

use super::*;
use crate::emu::{self, Board, Coord, Inputs, Outcome, Request, RunReq};
use gpui::{DispatchPhase, relative};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
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
    /// E cycles since reset when the sample was taken.
    cycles: u64,
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

/// The view shown below the toolbar, chosen in the sidebar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Pane {
    Overview,
    Inputs,
    Cpu,
    Memory,
    Io,
    Lookup,
    Notes,
}

const PANES: [(Pane, &str); 7] = [
    (Pane::Overview, "Overview"),
    (Pane::Inputs, "Inputs & calibration"),
    (Pane::Cpu, "CPU & scheduler"),
    (Pane::Memory, "Memory"),
    (Pane::Io, "I/O & events"),
    (Pane::Lookup, "Map lookup"),
    (Pane::Notes, "Model & caveats"),
];

/// A slider-controlled value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Knob {
    Rpm,
    /// Raw steps from the closed endpoint towards the open one on the selected channel.
    Throttle,
    Closed,
    Open,
    Adc(usize),
    /// Lookup descriptor byte: 0 row, 1 column, 2 row fraction, 3 column fraction.
    Coord(u8),
}

/// Inclusive integer range of a knob with its arrow-key step and page step.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Range {
    lo: i32,
    hi: i32,
    step: i32,
    page: i32,
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
    /// The inputs of the Run request in flight (None: none in flight).
    inflight: Option<Inputs>,
    /// Show every kept event and the raw PC samples.
    raw: bool,
    pane: Pane,
    /// Knob being dragged with the left button.
    drag: Option<Knob>,
    focus: RefCell<HashMap<Knob, FocusHandle>>,
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
            inflight: None,
            raw: false,
            pane: Pane::Overview,
            drag: None,
            focus: RefCell::default(),
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
        (self.busy, self.running, self.inflight) = (false, false, None);
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

/// Throttle knob position: raw steps from the closed endpoint towards the open one (clamped),
/// and the number of steps; works for sensors whose open endpoint is below the closed one.
fn throttle_steps(raw: u8, (closed, open): (u8, u8)) -> (i32, i32) {
    let n = (open as i32 - closed as i32).abs();
    let v = if open >= closed { raw as i32 - closed as i32 } else { closed as i32 - raw as i32 };
    (v.clamp(0, n), n)
}

/// Raw A/D value for a throttle knob position.
fn throttle_raw(steps: i32, (closed, open): (u8, u8)) -> u8 {
    (if open >= closed { closed as i32 + steps } else { closed as i32 - steps }).clamp(0, 255) as u8
}

/// Knob value under pointer position `x` on a track starting at `left`, snapped to the step.
fn knob_at(x: f32, left: f32, width: f32, r: Range) -> i32 {
    let t = if width > 0. { ((x - left) / width).clamp(0., 1.) } else { 0. };
    let v = r.lo as f32 + t * (r.hi - r.lo) as f32;
    (((v / r.step as f32).round() as i32) * r.step).clamp(r.lo, r.hi)
}

/// New knob value for a key press (arrows: one step, Shift or Page keys: a page), or None.
fn knob_key(key: &str, shift: bool, v: i32, r: Range) -> Option<i32> {
    let d = if shift { r.page } else { r.step };
    let n = match key {
        "left" | "down" => v - d,
        "right" | "up" => v + d,
        "pagedown" => v - r.page,
        "pageup" => v + r.page,
        "home" => r.lo,
        "end" => r.hi,
        _ => return None,
    };
    Some(n.clamp(r.lo, r.hi))
}

const MONO: &str = "JetBrainsMono Nerd Font";
const OK: u32 = 0x5cc98a;
/// Left gutter of a chart, holding the value ticks.
const GUTTER: f32 = 56.;

fn line(t: impl Into<SharedString>, color: u32) -> Div {
    div().text_xs().text_color(rgb(color)).child(t.into())
}

fn para(t: impl Into<SharedString>, color: u32) -> Div {
    div().text_sm().text_color(rgb(color)).child(t.into())
}

fn chip(t: impl Into<SharedString>, fg: u32, border: u32) -> Div {
    div().px_2().py(px(2.)).rounded_md().border_1().border_color(rgb(border)).bg(rgb(PANEL2)).text_xs().text_color(rgb(fg)).child(t.into())
}

/// A titled card that may shrink with its container.
fn panel(title: impl Into<SharedString>) -> Div {
    card().gap_2().min_w_0().child(div().text_sm().font_weight(FontWeight::BOLD).text_color(rgb(TEXT)).child(title.into()))
}

/// A labelled reading with a large value; `sub` explains where it comes from.
fn tile(label: &str, value: impl Into<SharedString>, sub: impl Into<SharedString>, color: u32) -> Div {
    div()
        .flex_1()
        .min_w(px(170.))
        .flex()
        .flex_col()
        .gap(px(2.))
        .p_3()
        .rounded_md()
        .bg(rgb(PANEL2))
        .border_1()
        .border_color(rgb(BORDER))
        .child(line(label.to_string(), MUTED))
        .child(div().text_xl().font_family(MONO).text_color(rgb(color)).child(value.into()))
        .child(line(sub, MUTED))
}

/// A monospaced table; columns are (header, width in characters, right-aligned), width 0 = as
/// wide as its text. It scrolls sideways inside its own box instead of pushing the page wider,
/// and never clips a cell.
fn table(id: impl Into<SharedString>, cols: &[(&str, usize, bool)], rows: Vec<(Vec<String>, u32)>) -> Stateful<Div> {
    let total: usize = cols.iter().filter(|c| c.1 > 0).map(|c| c.1 * 8 + 16).sum();
    let row = |cells: Vec<String>, color: u32| {
        let mut r = div().flex().text_color(rgb(color));
        for (c, t) in cols.iter().zip(cells) {
            let cell = div().flex_none().flex().whitespace_nowrap();
            let cell = if c.1 > 0 { cell.min_w(px(c.1 as f32 * 8. + 16.)) } else { cell };
            r = r.child(if c.2 { cell.justify_end().pr_3() } else { cell.pr_3() }.child(t));
        }
        r
    };
    let mut t = div().flex().flex_col().min_w(px(total as f32)).font_family(MONO).text_size(px(13.)).child(
        row(cols.iter().map(|c| c.0.to_string()).collect(), MUTED).pb_1().border_b_1().border_color(rgb(BORDER)),
    );
    for (cells, color) in rows {
        t = t.child(row(cells, color));
    }
    div().id(id.into()).w_full().overflow_x_scroll().child(t)
}

fn regs_text(r: &emu::Regs) -> String {
    format!("PC ${:04X}  SP ${:04X}  A ${:02X}  B ${:02X}  X ${:04X}  Y ${:04X}  CC {:08b}", r.pc, r.sp, r.a, r.b, r.x, r.y, r.cc)
}

/// Log-scaled 0..1 for activity counts.
fn level(n: u64, max: u64) -> f32 {
    if n == 0 || max == 0 { 0. } else { ((n as f32).ln_1p() / (max as f32).ln_1p()).clamp(0.05, 1.) }
}

fn ms(cycles: u64) -> f64 {
    cycles as f64 * 1000. / emu::E_CLOCK_HZ as f64
}

fn secs(cycles: u64) -> f64 {
    cycles as f64 / emu::E_CLOCK_HZ as f64
}

/// 12345678 → "12,345,678".
fn grouped(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (k, c) in s.chars().enumerate() {
        if k > 0 && (s.len() - k).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
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
        history.push_back(Point { inst: t.inst, cycles: t.cycles, commanded: run.inputs.crank_rpm, firmware, tps_in: run.inputs.adc[TPS_CANDIDATE], tps_fw: t.probe[c6] });
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

/// One chart axis: its value range and labelled ticks.
struct Axis {
    lo: f64,
    hi: f64,
    ticks: Vec<(f64, String)>,
}

/// The smallest 1, 2 or 5 × 10^k at least `raw`.
fn nice_step(raw: f64) -> f64 {
    let raw = raw.max(1e-9);
    let mag = 10f64.powf(raw.log10().floor());
    [1., 2., 5., 10.].into_iter().map(|m| m * mag).find(|&s| s >= raw * (1. - 1e-9)).unwrap_or(10. * mag)
}

impl Axis {
    /// About `n` ticks at round multiples inside lo..=hi; an empty range becomes lo..lo+1.
    fn nice(lo: f64, hi: f64, n: usize, fmt: impl Fn(f64, f64) -> String) -> Self {
        let hi = if hi > lo { hi } else { lo + 1. };
        let step = nice_step((hi - lo) / n.max(1) as f64);
        let (k0, k1) = ((lo / step - 1e-9).ceil() as i64, (hi / step + 1e-9).floor() as i64);
        let ticks = (k0..=k1).map(|k| k as f64 * step).map(|t| (t, fmt(t, step))).collect();
        Self { lo, hi, ticks }
    }

    /// 0 at `lo` through 1 at `hi`, clamped.
    fn frac(&self, v: f64) -> f32 {
        ((v - self.lo) / (self.hi - self.lo)).clamp(0., 1.) as f32
    }
}

/// Value axis from 0 to the round tick at or above `max` (at least `floor`).
fn zero_axis(max: f64, floor: f64) -> Axis {
    let max = max.max(floor);
    let step = nice_step(max / 4.);
    Axis::nice(0., (max / step).ceil() * step, 4, |v, _| format!("{v:.0}"))
}

/// Elapsed virtual seconds axis.
fn time_axis(lo: f64, hi: f64) -> Axis {
    Axis::nice(lo, hi, 6, |v, step| if step < 1. { format!("{v:.1}") } else { format!("{v:.0}") })
}

/// Executed-instruction axis.
fn inst_axis(lo: f64, hi: f64) -> Axis {
    Axis::nice(lo, hi, 5, |v, step| if step >= 1e6 { format!("{:.0}M", v / 1e6) } else if step >= 1e5 { format!("{:.1}M", v / 1e6) } else if step >= 1e3 { format!("{:.0}k", v / 1e3) } else { format!("{v:.0}") })
}

/// One line of a chart; separate runs are not joined (a gap means no valid reading).
struct Series {
    name: &'static str,
    color: u32,
    runs: Vec<Vec<(f64, f64)>>,
}

/// Split samples into runs at None (unknown) values.
fn runs<T>(items: impl Iterator<Item = T>, f: impl Fn(&T) -> (f64, Option<f64>)) -> Vec<Vec<(f64, f64)>> {
    let mut out: Vec<Vec<(f64, f64)>> = vec![vec![]];
    for it in items {
        match f(&it) {
            (x, Some(y)) => out.last_mut().unwrap().push((x, y)),
            (_, None) if !out.last().unwrap().is_empty() => out.push(vec![]),
            _ => {}
        }
    }
    out.retain(|r| !r.is_empty());
    out
}

/// X/Y line chart filling the available width: value ticks on the left, X ticks below, grid,
/// legend, optional vertical marks. Single samples draw as dots; nothing is interpolated beyond
/// straight segments between consecutive actual samples.
fn chart(title: &str, caption: String, x: Axis, y: Axis, series: Vec<Series>, marks: Vec<(f64, u32)>, height: f32) -> Div {
    let has_data = series.iter().any(|s| !s.runs.is_empty());
    type Runs = Vec<Vec<(f32, f32)>>;
    let lines: Vec<(Runs, u32)> =
        series.iter().map(|s| (s.runs.iter().map(|r| r.iter().map(|&(a, b)| (x.frac(a), y.frac(b))).collect()).collect(), s.color)).collect();
    let (xt, yt): (Vec<f32>, Vec<f32>) = (x.ticks.iter().map(|t| x.frac(t.0)).collect(), y.ticks.iter().map(|t| y.frac(t.0)).collect());
    let mk: Vec<(f32, u32)> = marks.iter().map(|&(v, c)| (x.frac(v), c)).collect();
    let (xg, yg) = (xt.clone(), yt.clone());
    let cv = canvas(
        |_, _, _| (),
        move |b, _, window, _| {
            let (o, w, h) = (b.origin, f32::from(b.size.width), f32::from(b.size.height));
            let pt = |q: (f32, f32)| point(o.x + px(q.0 * w), o.y + px((1. - q.1) * h));
            let seg = |window: &mut Window, a: (f32, f32), c: (f32, f32), color: Hsla, width: f32| {
                let mut pb = PathBuilder::stroke(px(width));
                pb.move_to(pt(a));
                pb.line_to(pt(c));
                if let Ok(path) = pb.build() {
                    window.paint_path(path, color);
                }
            };
            for &f in &yg {
                seg(window, (0., f), (1., f), rgb(BORDER).into(), 1.);
            }
            for &f in &xg {
                seg(window, (f, 0.), (f, 1.), hsla(0., 0., 0.3, 0.35), 1.);
            }
            for &(f, c) in &mk {
                let mut color: Hsla = rgb(c).into();
                color.a = 0.45;
                seg(window, (f, 0.), (f, 1.), color, 1.);
            }
            for (runs, color) in &lines {
                for run in runs {
                    if run.len() == 1 {
                        let c = pt(run[0]);
                        let mut f = PathBuilder::fill();
                        f.move_to(point(c.x - px(3.), c.y - px(3.)));
                        f.line_to(point(c.x + px(3.), c.y - px(3.)));
                        f.line_to(point(c.x + px(3.), c.y + px(3.)));
                        f.line_to(point(c.x - px(3.), c.y + px(3.)));
                        f.close();
                        if let Ok(path) = f.build() {
                            window.paint_path(path, rgb(*color));
                        }
                        continue;
                    }
                    let mut pb = PathBuilder::stroke(px(1.75));
                    for (k, q) in run.iter().enumerate() {
                        if k == 0 { pb.move_to(pt(*q)) } else { pb.line_to(pt(*q)) }
                    }
                    if let Ok(path) = pb.build() {
                        window.paint_path(path, rgb(*color));
                    }
                }
            }
        },
    )
    .size_full();
    let mut legend = div().flex().flex_wrap().items_center().gap_x_4().gap_y_1().child(div().flex_1().min_w(px(120.)).text_sm().font_weight(FontWeight::BOLD).child(title.to_string()));
    for s in &series {
        legend = legend.child(div().flex().items_center().gap_1().text_xs().text_color(rgb(TEXT)).child(div().w(px(14.)).h(px(3.)).rounded_sm().bg(rgb(s.color))).child(s.name));
    }
    let mut ylab = div().relative().w(px(GUTTER)).h(px(height)).flex_none();
    for (t, f) in y.ticks.iter().zip(&yt) {
        ylab = ylab.child(div().absolute().right(px(8.)).top(relative(1. - f)).mt(px(-8.)).text_xs().font_family(MONO).text_color(rgb(MUTED)).child(t.1.clone()));
    }
    let mut plot = div().relative().flex_1().min_w_0().h(px(height)).rounded_sm().bg(rgb(BG)).border_1().border_color(rgb(BORDER)).child(cv);
    if !has_data {
        plot = plot.child(div().absolute().size_full().flex().items_center().justify_center().text_sm().text_color(rgb(MUTED)).child("No samples yet"));
    }
    let mut xlab = div().relative().flex_1().min_w_0().h(px(18.));
    for (t, f) in x.ticks.iter().zip(&xt) {
        xlab = xlab.child(div().absolute().left(relative(*f)).ml(px(-40.)).w(px(80.)).top(px(2.)).flex().justify_center().text_xs().font_family(MONO).text_color(rgb(MUTED)).child(t.1.clone()));
    }
    div()
        .flex()
        .flex_col()
        .gap_1()
        .min_w_0()
        .child(legend)
        .child(div().flex().pt_2().child(ylab).child(plot).child(div().w(px(28.)).flex_none()))
        .child(div().flex().child(div().w(px(GUTTER)).flex_none()).child(xlab).child(div().w(px(28.)).flex_none()))
        .child(div().pl(px(GUTTER)).text_xs().text_color(rgb(MUTED)).child(caption))
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

    /// The latest successful run of the current session.
    fn emu_last(&self) -> Option<(&Session, &emu::Run)> {
        self.emu_session().and_then(|s| s.result.as_ref().ok().map(|r| (s, r)))
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
        self.emu.inflight = (!lookup).then_some(inputs);
        let cancel = self.emu.cancel.clone();
        let task = cx.background_executor().spawn(async move { emu::run(&req, &sim, &original, &sha, &edits, &cancel) });
        cx.spawn(async move |this, cx| {
            let r = task.await;
            this.update(cx, |this, cx| {
                if this.emu.generation != generation {
                    return; // reset, replaced or cancelled meanwhile
                }
                (this.emu.busy, this.emu.inflight) = (false, None);
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
        let b = btn(id, label, false).text_sm().px_3();
        if enabled {
            b.on_click(cx.listener(move |this, _, _, cx| {
                on(this, cx);
                cx.notify();
            }))
        } else {
            b.opacity(0.4).cursor_default()
        }
    }

    fn go(&self, id: &'static str, label: &'static str, pane: Pane, cx: &mut Context<Self>) -> Stateful<Div> {
        btn(id, label, false).on_click(cx.listener(move |this, _, _, cx| {
            this.emu.pane = pane;
            cx.notify();
        }))
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

    /// A knob's current value and range.
    fn knob_get(&self, k: Knob) -> (i32, Range) {
        let byte = Range { lo: 0, hi: 255, step: 1, page: 16 };
        match k {
            Knob::Rpm => (self.emu.inputs.crank_rpm as i32, Range { lo: 0, hi: emu::CRANK_MAX as i32, step: 10, page: 250 }),
            Knob::Throttle => {
                let (v, n) = throttle_steps(self.emu.inputs.adc[self.emu.tps_ch], self.tps_ends());
                (v, Range { lo: 0, hi: n, step: 1, page: (n / 10).max(1) })
            }
            Knob::Closed => (self.tps_ends().0 as i32, byte),
            Knob::Open => (self.tps_ends().1 as i32, byte),
            Knob::Adc(ch) => (self.emu.inputs.adc[ch] as i32, byte),
            Knob::Coord(i) => {
                let c = self.emu.coord;
                match i {
                    0 => (c.row as i32, Range { lo: 0, hi: Coord::ROW_MAX as i32, step: 1, page: 4 }),
                    1 => (c.col as i32, Range { lo: 0, hi: Coord::COL_MAX as i32, step: 1, page: 2 }),
                    2 => (c.row_frac as i32, byte),
                    _ => (c.col_frac as i32, byte),
                }
            }
        }
    }

    fn knob_set(&mut self, k: Knob, v: i32) {
        let byte = v.clamp(0, 255) as u8;
        match k {
            Knob::Rpm => {
                let rpm = v.clamp(0, emu::CRANK_MAX as i32) as u16;
                self.set_crank(rpm);
                if rpm > 0 {
                    self.emu.crank_target = rpm;
                }
            }
            Knob::Throttle => self.set_input(Field::Adc(self.emu.tps_ch), throttle_raw(v, self.tps_ends())),
            Knob::Closed | Knob::Open => {
                // Keep the requested percentage: the raw input follows the moved endpoint.
                let pct = self.tps_pct().unwrap_or(0);
                let (mut c, mut o) = self.tps_ends();
                *(if k == Knob::Open { &mut o } else { &mut c }) = byte;
                self.emu.tps_ends = Some((c, o));
                self.apply_tps(pct);
            }
            Knob::Adc(ch) => self.set_input(Field::Adc(ch), byte),
            Knob::Coord(i) => {
                let c = &mut self.emu.coord;
                *match i {
                    0 => &mut c.row,
                    1 => &mut c.col,
                    2 => &mut c.row_frac,
                    _ => &mut c.col_frac,
                } = byte;
            }
        }
    }

    /// Horizontal analog slider: drag (also outside the track until release), click to jump, or
    /// focus it and use arrows (Shift: larger step), Page Up/Down, Home and End.
    fn knob(&self, k: Knob, label: impl Into<SharedString>, readout: impl Into<SharedString>, window: &Window, cx: &mut Context<Self>) -> Div {
        let (v, r) = self.knob_get(k);
        let enabled = r.hi > r.lo;
        let frac = if enabled { (v - r.lo) as f32 / (r.hi - r.lo) as f32 } else { 0. };
        let focus = self.emu.focus.borrow_mut().entry(k).or_insert_with(|| cx.focus_handle()).clone();
        let (focused, dragging) = (focus.is_focused(window), self.emu.drag == Some(k));
        let track_bounds = Rc::new(Cell::new(None::<Bounds<Pixels>>));
        let weak = cx.entity().downgrade();
        let tb = track_bounds.clone();
        // Measures the track; while this knob is dragged it also follows the pointer anywhere in the
        // window and ends on release, a release outside the window, or window deactivation.
        let probe = canvas(
            move |b, _, _| tb.set(Some(b)),
            move |b, _, window, _| {
                if !dragging {
                    return;
                }
                let w = weak.clone();
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    let held = e.pressed_button == Some(MouseButton::Left) && window.is_window_active();
                    w.update(cx, |this, cx| {
                        if held {
                            let (_, r) = this.knob_get(k);
                            this.knob_set(k, knob_at(f32::from(e.position.x), f32::from(b.origin.x), f32::from(b.size.width), r));
                        } else {
                            this.emu.drag = None;
                        }
                        cx.notify();
                    })
                    .ok();
                });
                window.on_mouse_event(move |e: &MouseUpEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble && e.button == MouseButton::Left {
                        weak.update(cx, |this, cx| {
                            this.emu.drag = None;
                            cx.notify();
                        })
                        .ok();
                    }
                });
            },
        )
        .absolute()
        .size_full();
        let accent = if enabled { ACCENT } else { BORDER };
        let inner = div()
            .relative()
            .h_full()
            .child(probe)
            .child(div().absolute().top(px(13.)).left_0().w_full().h(px(6.)).rounded_full().bg(rgb(PANEL2)).border_1().border_color(rgb(BORDER)))
            .child(div().absolute().top(px(13.)).left_0().w(relative(frac)).h(px(6.)).rounded_full().bg(rgb(accent)))
            .child(
                div()
                    .absolute()
                    .top(px(5.))
                    .left(relative(frac))
                    .ml(px(-11.))
                    .w(px(22.))
                    .h(px(22.))
                    .rounded_full()
                    .bg(rgb(if enabled { TEXT } else { MUTED }))
                    .border_2()
                    .border_color(rgb(if focused || dragging { ACCENT } else { PANEL })),
            );
        let mut track = div()
            .id(SharedString::from(format!("knob-{k:?}")))
            .flex_1()
            .min_w(px(140.))
            .h(px(32.))
            .px(px(11.))
            .rounded_md()
            .border_1()
            .border_color(if focused { rgb(ACCENT).into() } else { hsla(0., 0., 0., 0.) })
            .track_focus(&focus)
            .child(inner);
        if enabled {
            track = track
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                    window.focus(&focus);
                    if let Some(b) = track_bounds.get() {
                        let (_, r) = this.knob_get(k);
                        this.knob_set(k, knob_at(f32::from(e.position.x), f32::from(b.origin.x), f32::from(b.size.width), r));
                    }
                    this.emu.drag = Some(k);
                    cx.stop_propagation();
                    cx.notify();
                }))
                .on_key_down(cx.listener(move |this, e: &KeyDownEvent, _, cx| {
                    let (v, r) = this.knob_get(k);
                    if let Some(n) = knob_key(&e.keystroke.key, e.keystroke.modifiers.shift, v, r) {
                        this.knob_set(k, n);
                        cx.stop_propagation();
                        cx.notify();
                    }
                }));
        }
        div()
            .flex()
            .items_center()
            .gap_3()
            .min_w_0()
            .child(div().w(px(120.)).flex_none().text_sm().text_color(rgb(MUTED)).child(label.into()))
            .child(track)
            .child(div().w(px(132.)).flex_none().flex().justify_end().text_base().font_family(MONO).child(readout.into()))
    }

    /// One byte input as eight bit toggles (bit 7 left); `used` marks bits the firmware is known to read.
    fn bit_row(&self, id: &'static str, label: &'static str, bits: u8, used: &[(u8, &'static str)], field: Field, cx: &mut Context<Self>) -> Div {
        let value = *field.get(&mut self.emu.inputs.clone());
        let mut toggles = div().flex().gap_1().flex_none();
        for b in (0..8u8).rev() {
            if bits & (1 << b) == 0 {
                toggles = toggles.child(div().w(px(30.)).text_xs().text_color(rgb(MUTED)).flex().justify_center().child("·"));
                continue;
            }
            let on = value & (1 << b) != 0;
            let mark = if used.iter().any(|u| u.0 == b) { "*" } else { "" };
            toggles = toggles.child(btn(SharedString::from(format!("{id}{b}")), format!("{b}{mark}"), on).w(px(30.)).flex().justify_center().on_click(cx.listener(move |this, _, _, cx| {
                this.set_input(field, value ^ (1 << b));
                cx.notify();
            })));
        }
        let notes: Vec<String> = used.iter().map(|(b, w)| format!("bit {b}: {w}")).collect();
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_x_3()
            .gap_y_1()
            .child(div().w(px(200.)).flex_none().text_sm().text_color(rgb(MUTED)).child(label))
            .child(toggles)
            .child(div().w(px(40.)).font_family(MONO).text_sm().child(format!("${value:02X}")))
            .child(line(notes.join(" · "), MUTED))
    }

    // ---- frame -------------------------------------------------------------------------------

    /// Sidebar navigation for the Emulation tab.
    pub(super) fn emu_nav(&self, cx: &mut Context<Self>) -> Div {
        let blocked = self.emu_last().is_some_and(|(_, r)| r.blocker.is_some());
        let mut list = div().flex().flex_col().py_2();
        for (pane, name) in PANES {
            let sel = self.emu.pane == pane;
            let badge = match pane {
                Pane::Cpu if blocked => Some(("blocked", ERR)),
                Pane::Notes => Some(("hypotheses", WARN)),
                _ => None,
            };
            list = list.child(
                div()
                    .id(SharedString::from(format!("pane-{pane:?}")))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py(px(7.))
                    .cursor_pointer()
                    .border_l_2()
                    .border_color(rgb(if sel { ACCENT } else { PANEL }))
                    .bg(rgb(if sel { 0x1d3550 } else { PANEL }))
                    .hover(|s| s.bg(rgb(if sel { 0x1d3550 } else { PANEL2 })))
                    .text_sm()
                    .child(div().flex_1().child(name))
                    .children(badge.map(|(t, c)| line(t, c)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.emu.pane = pane;
                        cx.notify();
                    })),
            );
        }
        div().flex().flex_col().child(div().px_3().pt_2().child(heading("EMULATION"))).child(list)
    }

    pub(super) fn emulation(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let mut alerts = div().flex().flex_col().gap_2();
        let profile = self.emu_target().map(|d| emu::profile(&d.sha256));
        let usable = matches!(profile, Some(Some(_)));
        match profile {
            None => alerts = alerts.child(card().child(para("No target dump loaded. Use Browse… or Open folder….", WARN))),
            Some(None) => {
                alerts = alerts.child(card().border_color(rgb(ERR)).child(para(
                    "Emulation is disabled for this file: only the reviewed XSI2 and mild-remap source hashes are supported, because code addresses differ between firmware revisions.",
                    ERR,
                )))
            }
            Some(Some(_)) => {
                if let Err(e) = emu::simulator() {
                    alerts = alerts.child(card().border_color(rgb(ERR)).child(para(e, ERR)));
                }
                if let Some(n) = &self.emu.note {
                    alerts = alerts.child(para(n.clone(), WARN));
                }
                match self.emu_session().map(|s| &s.result) {
                    Some(Err(e)) => alerts = alerts.child(card().border_color(rgb(ERR)).child(para("Run stopped on an error. Press Reset to start again.", ERR)).child(line(e.clone(), TEXT))),
                    Some(Ok(r)) => {
                        if let Some(b) = &r.blocker {
                            alerts = alerts.child(self.blocker_box(r, b, cx));
                        }
                    }
                    None if self.emu.session.is_some() => alerts = alerts.child(para("The last session belonged to other bytes or another profile; Run starts again from reset.", MUTED)),
                    None => {}
                }
            }
        }
        let body = if !usable {
            div()
        } else {
            match self.emu.pane {
                Pane::Overview => self.pane_overview(window, cx),
                Pane::Inputs => self.pane_inputs(window, cx),
                Pane::Cpu => self.pane_cpu(),
                Pane::Memory => self.pane_memory(),
                Pane::Io => self.pane_io(cx),
                Pane::Lookup => self.pane_lookup(window, cx),
                Pane::Notes => self.pane_notes(cx),
            }
        };
        div()
            .flex()
            .flex_col()
            .min_h_0()
            .child(self.toolbar(usable, cx))
            .child(alerts.px_4())
            .child(div().id("emu-scroll").flex_1().min_h_0().overflow_y_scroll().child(div().flex().flex_col().gap_3().p_4().child(body)))
            .child(self.status_bar())
    }

    fn state(&self) -> (&'static str, u32) {
        match self.emu_session().map(|s| &s.result) {
            _ if self.emu.running => ("Running", ACCENT),
            _ if self.emu.busy => ("Executing", ACCENT),
            None => ("Not started", MUTED),
            Some(Err(_)) => ("Error", ERR),
            Some(Ok(r)) if r.blocker.is_some() => ("Blocked", ERR),
            Some(Ok(_)) => ("Paused", WARN),
        }
    }

    fn toolbar(&self, usable: bool, cx: &mut Context<Self>) -> Div {
        let session = self.emu_session();
        let can_continue = usable && continuation(session).is_some();
        let (busy, running) = (self.emu.busy, self.emu.running);
        let (state, color) = self.state();
        // Fixed button positions: availability shows only through opacity, so Pause never moves.
        let mut bar = div()
            .flex()
            .flex_none()
            .flex_wrap()
            .items_center()
            .gap_2()
            .px_4()
            .py_2()
            .bg(rgb(PANEL))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(self.emu_btn("run", "▶ Run", !running && !busy && can_continue, |this, cx| this.emu_continue(CHUNK, true, cx), cx))
            .child(self.emu_btn("pause", "‖ Pause", running, |this, _| this.emu.running = false, cx))
            .child(self.emu_btn("step", "Step", !running && !busy && can_continue, |this, cx| this.emu_continue(1, false, cx), cx))
            .child(self.emu_btn("reset", "Reset", session.is_some() || busy || self.emu.session.is_some(), |this, _| {
                this.emu.reset_session();
                this.emu.note = None;
            }, cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .ml_2()
                    .w(px(120.))
                    .child(div().w(px(10.)).h(px(10.)).rounded_full().bg(rgb(color)))
                    .child(div().text_sm().font_weight(FontWeight::BOLD).text_color(rgb(color)).child(state)),
            )
            .child(div().flex_1())
            .child(line("Profile", MUTED));
        for (id, label, b) in [("bp-strict", "Strict", Board::Strict), ("bp-h1", "H1", Board::H1), ("bp-h2", "H2 simulation", Board::H2)] {
            bar = bar.child(btn(id, label, self.emu.board == b).text_sm().on_click(cx.listener(move |this, _, _, cx| {
                if this.emu.board != b {
                    this.emu.board = b;
                    this.emu.reset_session(); // a session belongs to one profile
                    this.emu.note = None;
                }
                cx.notify();
            })));
        }
        bar.child(btn("hyp", "Hypothesis model ⓘ", self.emu.pane == Pane::Notes).text_sm().text_color(rgb(WARN)).on_click(cx.listener(|this, _, _, cx| {
            this.emu.pane = Pane::Notes;
            cx.notify();
        })))
    }

    fn status_bar(&self) -> Div {
        let mut items: Vec<String> = vec![];
        if let Some((s, r)) = self.emu_last() {
            items.push(format!("{} chunk(s)", s.chunks));
            items.push(format!("{} instructions", grouped(r.executed)));
            items.push(format!("{:.2} s virtual (2 MHz E assumed)", secs(r.cycles)));
            items.push(format!("PC ${:04X}", r.regs.pc));
        } else {
            items.push("No session".into());
        }
        if self.emu.board == Board::H2 && self.emu_session().is_some_and(|s| s.inputs != self.emu.inputs) {
            items.push(if self.emu.inflight == Some(self.emu.inputs) { "inputs: running now".into() } else { "inputs: pending next chunk".into() });
        }
        let backend = match emu::simulator() {
            Ok(_) => ("µCsim ready", MUTED),
            Err(_) => ("µCsim missing", ERR),
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_4()
            .px_4()
            .py_1()
            .bg(rgb(PANEL))
            .border_t_1()
            .border_color(rgb(BORDER))
            .text_xs()
            .text_color(rgb(MUTED))
            .child(div().flex_1().min_w_0().truncate().child(items.join("  ·  ")))
            .child(div().flex_none().text_color(rgb(backend.1)).child(backend.0))
    }

    fn blocker_box(&self, r: &emu::Run, b: &emu::Blocker, cx: &mut Context<Self>) -> Div {
        let pending = match (b.address, b.value) {
            (Some(a), Some(v)) => format!("{} ${a:04X} ← ${v:02X}", b.kind),
            (Some(a), None) => format!("{} ${a:04X}", b.kind),
            _ => b.kind.clone(),
        };
        card()
            .border_color(rgb(ERR))
            .gap_1()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_3()
                    .child(div().flex_1().min_w(px(240.)).text_base().font_weight(FontWeight::BOLD).text_color(rgb(ERR)).child(format!("Blocked after {} instructions: {pending}", grouped(r.executed))))
                    .child(self.go("blk-cpu", "CPU details", Pane::Cpu, cx)),
            )
            .child(div().font_family(MONO).text_sm().child(format!("${:04X}  {}  {}", b.pc, b.bytes, b.text)))
            .child(para(b.reason.clone(), TEXT))
            .child(line("The instruction was not executed. µCsim disassembly text is display only; the bytes are authoritative.", MUTED))
    }

    // ---- overview ----------------------------------------------------------------------------

    fn pane_overview(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let mut root = div().flex().flex_col().gap_3();
        let last = self.emu_last();
        if self.emu.board == Board::H2 {
            root = root.child(self.controls(window, cx)).child(self.signals(last.map(|x| x.1)));
        } else {
            root = root.child(
                panel("Engine inputs").child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(para("Crank and throttle inputs need the H2 simulation profile.", MUTED))
                        .child(btn("to-h2", "Use H2 simulation", false).text_sm().on_click(cx.listener(|this, _, _, cx| {
                            this.emu.board = Board::H2;
                            this.emu.reset_session();
                            cx.notify();
                        }))),
                ),
            );
        }
        if let Some((s, r)) = last {
            root = root.child(self.status_tiles(r));
            if self.emu.board == Board::H2 {
                root = root.child(self.engine_charts(&s.history)).child(self.engine_detail(r));
            }
        } else {
            root = root.child(panel("Firmware readings").child(para("No readings yet. Press Run: the firmware starts from reset and readings come from its RAM after each chunk.", MUTED)));
        }
        root
    }

    /// Crank speed and throttle sliders. Each change is a request; the firmware sees it only from
    /// the next chunk boundary on (see the signals table).
    fn controls(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let i = self.emu.inputs;
        let turning = i.crank_rpm > 0;
        let start = btn("crank-toggle", if turning { "■ Stop crank" } else { "▶ Start crank" }, turning).text_sm().w(px(120.)).flex().justify_center().on_click(cx.listener(|this, _, _, cx| {
            let rpm = if this.emu.inputs.crank_rpm > 0 { 0 } else { this.emu.crank_target };
            this.set_crank(rpm);
            cx.notify();
        }));
        let (closed, open) = self.tps_ends();
        let ch = self.emu.tps_ch;
        let raw = i.adc[ch];
        let pct = self.tps_pct().map_or("—".to_string(), |p| format!("{p} %"));
        let rpm_text = if turning { format!("{} rpm", i.crank_rpm) } else { "stopped".into() };
        panel("Engine inputs")
            .child(div().flex().flex_wrap().items_center().gap_3().child(div().flex_1().min_w(px(300.)).child(self.knob(Knob::Rpm, "Crank speed", rpm_text, window, cx))).child(start))
            .child(self.knob(Knob::Throttle, format!("Throttle (AN{ch})"), pct, window, cx))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x_3()
                    .gap_y_1()
                    .child(line(format!("Throttle writes raw {raw} (${raw:02X}) to AN{ch}; 0–100 % spans raw {closed}–{open}{}.", if self.emu.tps_ends.is_none() { " (calibration endpoints)" } else { "" }), MUTED))
                    .child(self.go("to-inputs", "Calibration…", Pane::Inputs, cx))
                    .child(line("No engine model: crank speed is commanded; throttle never changes it.", WARN))
                    .child(self.go("to-notes", "Why?", Pane::Notes, cx)),
            )
    }

    /// Requested now, applied in the last completed chunk, and what the firmware read.
    fn signals(&self, r: Option<&emu::Run>) -> Div {
        let req = self.emu.inputs;
        let ch = self.emu.tps_ch;
        let ends = self.tps_ends();
        let can_continue = continuation(self.emu_session()).flatten().is_some() || self.emu_session().is_none();
        // Where a requested value is on its way to the firmware.
        let status = |same: bool, inflight_same: bool| -> (String, u32) {
            if same {
                ("applied".into(), OK)
            } else if self.emu.busy && inflight_same {
                ("in the running chunk".into(), ACCENT)
            } else if self.emu.running || self.emu.busy {
                ("waits for the next chunk".into(), WARN)
            } else if can_continue {
                ("applies when Run continues".into(), WARN)
            } else {
                ("applies after Reset".into(), WARN)
            }
        };
        let tps = |raw: u8| throttle_percent(raw, ends).map_or(format!("raw {raw} (outside endpoints)"), |p| format!("{p} % · raw {raw}"));
        let b = |a: u16| r.and_then(|r| r.ram(a)).unwrap_or(0);
        let period = u16::from_be_bytes([b(0xBB), b(0xBC)]);
        let fw_rpm = match r {
            None => ("—".to_string(), MUTED),
            Some(_) if period == NO_PERIOD => ("no valid period ($BB = $FFFF)".into(), MUTED),
            Some(_) => (format!("{} rpm ($D1)", u16::from_be_bytes([b(0xD1), b(0xD2)])), OK),
        };
        let fw_tps = r.map_or(("—".to_string(), MUTED), |_| (format!("raw {} ($C6, validated AN2)", b(0xC6)), OK));
        let (rpm_s, rpm_c) = match r {
            None => ("press Run".into(), MUTED),
            Some(r) => status(r.inputs.crank_rpm == req.crank_rpm, self.emu.inflight.is_some_and(|f| f.crank_rpm == req.crank_rpm)),
        };
        let (tps_s, tps_c) = match r {
            None => ("press Run".into(), MUTED),
            Some(r) => status(r.inputs.adc[ch] == req.adc[ch], self.emu.inflight.is_some_and(|f| f.adc[ch] == req.adc[ch])),
        };
        let applied = |f: &dyn Fn(&emu::Run) -> String| r.map_or("—".to_string(), f);
        let rows = vec![
            (vec!["Crank speed".into(), format!("{} rpm", req.crank_rpm), applied(&|r| format!("{} rpm", r.inputs.crank_rpm)), fw_rpm.0, rpm_s], (rpm_c, fw_rpm.1)),
            (vec![format!("Throttle AN{ch}"), tps(req.adc[ch]), applied(&|r| tps(r.inputs.adc[ch])), if ch == TPS_CANDIDATE { fw_tps.0 } else { "not read as throttle".into() }, tps_s], (tps_c, fw_tps.1)),
        ];
        let widths = [130., 170., 170., 250.];
        let head = ["Signal", "Requested now", "Applied (last chunk)", "Firmware reading", "Status"];
        let cell = |k: usize| if k < 4 { div().w(px(widths[k])).flex_none() } else { div().flex_1().min_w(px(150.)) };
        let mut t = div().flex().flex_col().gap_1().min_w(px(870.));
        let mut h = div().flex().text_xs().text_color(rgb(MUTED)).pb_1().border_b_1().border_color(rgb(BORDER));
        for (k, name) in head.iter().enumerate() {
            h = h.child(cell(k).child(*name));
        }
        t = t.child(h);
        for (cells, (sc, fc)) in rows {
            let mut row = div().flex().items_center().text_sm();
            for (k, c) in cells.into_iter().enumerate() {
                let color = match k {
                    0 => MUTED,
                    3 => fc,
                    4 => sc,
                    _ => TEXT,
                };
                row = row.child(cell(k).text_color(rgb(color)).when(k > 0 && k < 4, |d| d.font_family(MONO)).child(c));
            }
            t = t.child(row);
        }
        panel("Requested → applied → firmware")
            .child(div().id("signals").w_full().overflow_x_scroll().child(t))
            .child(line(format!("A change reaches the firmware at a chunk boundary (every {} instructions, about {:.0} s virtual). Firmware readings are RAM values after the last completed chunk.", grouped(CHUNK as u64), secs(CHUNK as u64 * 2)), MUTED))
    }

    /// Faults and key state at a glance.
    fn status_tiles(&self, r: &emu::Run) -> Div {
        let b = |a: u16| r.ram(a).unwrap_or(0);
        let fault = b(0x1B);
        let sync = r.ram(0x2093).unwrap_or(0);
        let regime = b(0xAE);
        let stage = r.milestones.iter().rev().find(|m| m.2.is_some()).map_or("—".to_string(), |m| m.1.clone());
        let aa = b(0xAA);
        let flags = match (aa & 0x40 != 0, aa & 0x80 != 0) {
            (true, true) => "closed + open flags",
            (true, false) => "closed flag ($AA b6)",
            (false, true) => "open flag ($AA b7)",
            _ => "no endpoint flag",
        };
        let mut row = div().flex().flex_wrap().gap_3();
        if self.emu.board == Board::H2 {
            row = row
                .child(tile("Throttle fault $1B", if fault == 0 { "none".into() } else { format!("${fault:02X}") }, if fault == 0 { "AN2 range and step checks pass".into() } else { "latched: firmware uses a default; Reset clears".to_string() }, if fault == 0 { OK } else { WARN }))
                .child(tile("Learned endpoints", format!("{} / {}", b(0x0E), b(0x0D)), format!("closed $0E / open $0D · {flags}"), TEXT))
                .child(tile("Crank decoding", format!("tooth {}", b(0xD8)), format!("sync errors $2093 = {sync} · {}", if regime & 1 != 0 { "low-speed/stall mode" } else { "normal regime" }), if sync == 0 { TEXT } else { WARN }));
        }
        row.child(tile("Boot stage", stage, "last static landmark reached", TEXT))
            .child(tile("Main loop", grouped(r.main_loop.count), format!("passes at ${:04X} · longest {:.2} ms", r.main_loop.pc, ms(r.main_loop.gap_max)), if r.main_loop.count > 1 { OK } else { MUTED }))
    }

    /// Commanded vs firmware speed, and AN2 raw input vs firmware $C6, over virtual time.
    fn engine_charts(&self, history: &VecDeque<Point>) -> Div {
        let (t0, t1) = (history.front().map_or(0., |p| secs(p.cycles)), history.back().map_or(1., |p| secs(p.cycles)));
        let x = || time_axis(t0, t1);
        let rpm_max = history.iter().map(|p| p.commanded.max(p.firmware.unwrap_or(0))).max().unwrap_or(0) as f64;
        let rpm = chart(
            "Crank speed",
            "Y: rpm · X: virtual seconds since reset (2 MHz E assumed) · gaps: no valid period".into(),
            x(),
            zero_axis(rpm_max * 1.05, 1000.),
            vec![
                Series { name: "commanded (applied)", color: ACCENT, runs: runs(history.iter(), |p| (secs(p.cycles), Some(p.commanded as f64))) },
                Series { name: "firmware $D1", color: OK, runs: runs(history.iter(), |p| (secs(p.cycles), p.firmware.map(f64::from))) },
            ],
            vec![],
            220.,
        );
        let raw_axis = Axis { lo: 0., hi: 255., ticks: [0., 64., 128., 192., 255.].map(|v| (v, format!("{v:.0}"))).to_vec() };
        let tps = chart(
            "Throttle (AN2)",
            "Y: raw A/D count · X: virtual seconds since reset (2 MHz E assumed)".into(),
            x(),
            raw_axis,
            vec![
                Series { name: "AN2 input (applied)", color: ACCENT, runs: runs(history.iter(), |p| (secs(p.cycles), Some(p.tps_in as f64))) },
                Series { name: "firmware $C6", color: OK, runs: runs(history.iter(), |p| (secs(p.cycles), Some(p.tps_fw as f64))) },
            ],
            vec![],
            220.,
        );
        let n = history.len();
        div()
            .flex()
            .flex_wrap()
            .gap_3()
            .child(card().flex_1().min_w(px(420.)).child(rpm))
            .child(card().flex_1().min_w(px(420.)).child(tps))
            .child(div().w_full().child(line(format!("{n} samples (64 per chunk, latest {HISTORY} kept). Lines join consecutive actual samples only."), MUTED)))
    }

    /// Every firmware RAM value behind the overview, after the last instruction (see the docs per address).
    fn engine_detail(&self, r: &emu::Run) -> Div {
        let b = |a: u16| r.ram(a).unwrap_or(0);
        let w = |a: u16| u16::from_be_bytes([b(a), b(a + 1)]);
        let period = w(0xBB);
        let aa = b(0xAA);
        let fault = b(0x1B);
        let crank = r.crank.as_ref().map_or("—".into(), |k| {
            let tooth = k.tooth.map_or("gap".into(), |t| format!("tooth {t}"));
            format!("{} falling edges · {tooth} · level {} · PACNT ${:02X} · TIC3 {}", grouped(k.edges), k.level, k.pacnt, k.tic3.map_or("—".into(), |t| format!("${t:04X}")))
        });
        let row = |name: &str, addr: &str, value: String| (vec![name.to_string(), addr.to_string(), value], TEXT);
        let rows = vec![
            row("Speed", "$D1:$D2", if period == NO_PERIOD { "— (no valid period)".into() } else { format!("{} rpm", w(0xD1)) }),
            row("Period, teeth 20→50", "$BB:$BC", if period == NO_PERIOD { "$FFFF · stopped or below about 229 rpm".into() } else { format!("{period} timer counts (180°)") }),
            row("Tooth counter", "$D8", b(0xD8).to_string()),
            row("Regime", "$AE", format!("{:08b} · bit 0: low-speed/stall mode", b(0xAE))),
            row("Gap check", "$AD bit 4", ((b(0xAD) >> 4) & 1).to_string()),
            row("Sync errors", "$2093", r.ram(0x2093).unwrap_or(0).to_string()),
            row("Simulated crank signal", "PA7/PA0", crank),
            row("AN2 copy", "$2003", b(0x2003).to_string()),
            row("AN2 validated", "$C6", b(0xC6).to_string()),
            row("AN2 processed", "$200F", b(0x200F).to_string()),
            row("Learned closed / open", "$0E / $0D", format!("{} / {}", b(0x0E), b(0x0D))),
            row("Endpoint flags", "$AA", format!("{}{}", if aa & 0x40 != 0 { "closed (bit 6) " } else { "" }, if aa & 0x80 != 0 { "open (bit 7)" } else if aa & 0x40 == 0 { "none" } else { "" })),
            row("AN2 fault flags", "$1B", if fault == 0 { "none".into() } else { format!("${fault:02X} · default substituted (range $9284..$9285, step limit $928A)") }),
        ];
        panel(format!("Firmware RAM after instruction {}", grouped(r.executed)))
            .child(table("detail", &[("reading", 24, false), ("address", 11, false), ("value", 0, false)], rows))
            .child(line("Speed formula (code at $429B): $D1:$D2 = 15,000,064 / $BB; reading it as rpm assumes the 2 MHz E clock and TMSK2 prescaler 4.", MUTED))
    }

    // ---- inputs ------------------------------------------------------------------------------

    fn pane_inputs(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        if self.emu.board != Board::H2 {
            return div().child(panel("Inputs").child(para("Inputs exist only in the H2 simulation profile. Strict and H1 run with no inputs.", MUTED)));
        }
        let (closed, open) = self.tps_ends();
        let mut chans = div().flex().flex_wrap().items_center().gap_1().child(div().w(px(120.)).flex_none().text_sm().text_color(rgb(MUTED)).child("A/D channel"));
        for k in 0..8 {
            let label = if k == TPS_CANDIDATE { format!("AN{k} (candidate)") } else { format!("AN{k}") };
            chans = chans.child(btn(SharedString::from(format!("tpsch{k}")), label, self.emu.tps_ch == k).text_sm().on_click(cx.listener(move |this, _, _, cx| {
                this.emu.tps_ch = k;
                cx.notify();
            })));
        }
        let calib = panel("Throttle calibration")
            .child(chans)
            .child(self.knob(Knob::Closed, "Closed (0 %)", format!("{closed} ${closed:02X}"), window, cx))
            .child(self.knob(Knob::Open, "Open (100 %)", format!("{open} ${open:02X}"), window, cx))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_3()
                    .child(btn("tpse-cal", "Calibration defaults", self.emu.tps_ends.is_none()).text_sm().on_click(cx.listener(|this, _, _, cx| {
                        this.emu.tps_ends = None;
                        cx.notify();
                    })))
                    .child(line("Defaults are the calibration's initial learned endpoints ($88FE closed, $88FD open). Moving an endpoint keeps the throttle percentage, so the raw input moves too.", MUTED)),
            );
        let i = self.emu.inputs;
        let mut adc = div().flex().flex_col().gap_1();
        for k in 0..8 {
            let v = i.adc[k];
            adc = adc.child(self.knob(Knob::Adc(k), format!("AN{k}{}", if k == self.emu.tps_ch { " (throttle)" } else { "" }), format!("{v} ${v:02X}"), window, cx));
        }
        let fill = i.eeprom_fill;
        let mut fill_row = div().flex().flex_wrap().items_center().gap_1().child(div().w(px(200.)).flex_none().text_sm().text_color(rgb(MUTED)).child("EEPROM power-up fill"));
        for (k, delta) in [-16i32, -1, 1, 16].into_iter().enumerate() {
            if k == 2 {
                fill_row = fill_row.child(div().w(px(80.)).flex().justify_center().font_family(MONO).text_sm().child(format!("{fill} ${fill:02X}")));
            }
            let text = if delta < 0 { format!("−{}", -delta) } else { format!("+{delta}") };
            fill_row = fill_row.child(btn(SharedString::from(format!("eefill-{k}")), text, false).on_click(cx.listener(move |this, _, _, cx| {
                this.set_input(Field::Fill, (fill as i32 + delta).clamp(0, 255) as u8);
                cx.notify();
            })));
        }
        let raw = panel("Raw H2 inputs")
            .child(line("Raw units with hypothetical defaults; physical meaning unknown unless noted. Changes apply at the next chunk boundary; only the EEPROM fill resets the session. * = bit the firmware is seen to read.", MUTED))
            .child(self.bit_row("i60-", "Board input $1060", 0xFF, &[(5, "branch at $A86A"), (1, "stored at $A035")], Field::In1060, cx))
            .child(self.bit_row("i80-", "Board input $1080", 0xFF, &[(4, "tested at $BB51/$BB61")], Field::In1080, cx))
            .child(self.bit_row("pa-", "PORTA PA2, PA1", 0x06, &[], Field::PortA, cx))
            .child(self.bit_row("pd-", "PORTD PD5–PD0", 0x3F, &[(5, "SPI SS level (low = mode fault)")], Field::PortD, cx))
            .child(div().pt_2().child(heading("A/D RESULTS")))
            .child(adc)
            .child(fill_row)
            .child(line("Default $FF = erased EEPROM. The real EEPROM content is not in the dump; the firmware treats $B7FF ≠ $AA as uninitialised and rewrites it (simulated only).", MUTED));
        div().flex().flex_col().gap_3().child(calib).child(raw)
    }

    // ---- CPU ---------------------------------------------------------------------------------

    fn pane_cpu(&self) -> Div {
        let Some((s, r)) = self.emu_last() else { return div().child(panel("CPU").child(para("No run yet. Press Run or Step.", MUTED))) };
        let g = &r.regs;
        let mut regs = div().flex().flex_wrap().gap_3();
        for (name, v) in [("PC", format!("${:04X}", g.pc)), ("SP", format!("${:04X}", g.sp)), ("A", format!("${:02X}", g.a)), ("B", format!("${:02X}", g.b)), ("X", format!("${:04X}", g.x)), ("Y", format!("${:04X}", g.y)), ("CC SXHINZVC", format!("{:08b}", g.cc))] {
            regs = regs.child(div().flex().flex_col().min_w(px(80.)).child(line(name, MUTED)).child(div().text_lg().font_family(MONO).child(v)));
        }
        let since = r.cycles.saturating_sub(r.cop_cycle);
        let cpu = panel("Registers")
            .child(regs)
            .child(line(format!("TCNT ${:04X} (prescaler from TMSK2) · COP serviced {} times, last {:.1} ms ago virtual · {} opcodes executed, {} with conformance cases", r.tcnt, grouped(r.cop_services), ms(since), r.opcodes.0, r.opcodes.1), MUTED));
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
                stages = stages.child(chip("■ blocked", ERR, ERR).font_weight(FontWeight::BOLD));
            }
        }
        let boot = panel("Boot landmarks").child(stages).child(line("From static disassembly; reaching one only means its first instruction executed.", MUTED));
        div().flex().flex_col().gap_3().child(cpu).child(boot).child(card().child(self.timeline(r, s))).child(self.scheduler(r)).child(self.page_grid(r))
    }

    /// PC against executed instructions from the session's samples; landmarks as vertical marks.
    fn timeline(&self, r: &emu::Run, s: &Session) -> Div {
        let first = s.trail.front().map_or(0, |t| t.0) as f64;
        let x = inst_axis(first, r.executed as f64);
        let y = Axis { lo: 0., hi: 65535., ticks: [0x0000u16, 0x4000, 0x8000, 0xC000, 0xFFFF].map(|a| (a as f64, format!("${a:04X}"))).to_vec() };
        let marks = r.milestones.iter().filter_map(|m| m.2).filter(|&i| i as f64 >= first).map(|i| (i as f64, OK)).collect();
        let mut series = vec![Series { name: "PC", color: ACCENT, runs: runs(s.trail.iter(), |t| (t.0 as f64, Some(t.2 as f64))) }];
        if let Some(b) = &r.blocker {
            series.push(Series { name: "blocked instruction", color: ERR, runs: vec![vec![(r.executed as f64, b.pc as f64)]] });
        }
        chart(
            "PC timeline",
            format!("Y: program counter · X: instructions executed · last {} samples (64 per chunk) · green marks: landmarks", s.trail.len()),
            x,
            y,
            series,
            marks,
            200.,
        )
    }

    /// Main-loop, timer-interrupt and watchdog evidence of the engine-stopped scheduler.
    fn scheduler(&self, r: &emu::Run) -> Div {
        let entries: u64 = r.interrupts.iter().map(|i| i.count).sum();
        let irq = table(
            "irq",
            &[("source", 6, false), ("vector", 6, false), ("entries", 10, true), ("last at", 12, true)],
            r.interrupts.iter().map(|i| (vec![i.name.clone(), format!("${:04X}", i.vector), grouped(i.count), format!("{:.1} ms", ms(i.last_cycle))], TEXT)).collect(),
        );
        let watch = table(
            "watch",
            &[("address", 7, false), ("executions", 11, true), ("landmark", 0, false)],
            r.watch.iter().map(|(a, name, n)| (vec![format!("${a:04X}"), grouped(*n), name.clone()], TEXT)).collect(),
        );
        let l = &r.main_loop;
        let sp = l.sp.map_or("—".into(), |(a, b)| if a == b { format!("${a:04X} (stable)") } else { format!("${a:04X}–${b:04X}") });
        // OPTION CR1:CR0 scales the COP timeout of 2^15 E cycles by 1, 4, 16 or 64 (manual 5.3.2).
        let cop_timeout = r.io.iter().find(|i| i.addr == 0x1039).map(|o| (1u64 << 15) << (2 * (o.value & 3)));
        let cop = match cop_timeout {
            Some(t) => format!("COP: {} services · longest gap {:.1} ms · timeout {:.1} ms if enabled (OPTION CR); CONFIG NOCOP unknown, not enforced", grouped(r.cop_services), ms(r.cop_gap_max), ms(t)),
            None => format!("COP: {} services", grouped(r.cop_services)),
        };
        panel("Scheduler (engine stopped)")
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(chip(format!("main loop ${:04X}: {} passes", l.pc, grouped(l.count)), if l.count > 1 { OK } else { MUTED }, BORDER))
                    .child(chip(format!("SP at loop head {sp}"), TEXT, BORDER))
                    .child(chip(format!("longest pass {:.2} ms", ms(l.gap_max)), TEXT, BORDER))
                    .child(chip(format!("{} interrupt entries, {} RTI", grouped(entries), grouped(r.rti)), if r.rti > 0 { OK } else { MUTED }, BORDER)),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_4()
                    .items_start()
                    .child(div().flex_1().min_w(px(300.)).flex().flex_col().gap_1().child(heading("INTERRUPT SOURCES (PRIORITY ORDER)")).child(if r.interrupts.is_empty() { div().child(line("no interrupt taken yet", MUTED)) } else { div().child(irq) }))
                    .child(div().flex_1().min_w(px(300.)).flex().flex_col().gap_1().child(heading("EXECUTIONS AT STATIC LANDMARKS")).child(watch)),
            )
            .child(line(cop, MUTED))
            .child(line("Not evidence of a running engine. Interrupt entry stacks PC, Y, X, A, B, CCR (9 bytes), sets I and takes 12 E cycles; conformance cases cover entry, return, repeated requests, CLI delay and HPRIO priority.", MUTED))
    }

    /// Instructions started per 256-byte page, 16 × 16 (row = high nibble of the page).
    fn page_grid(&self, r: &emu::Run) -> Div {
        const S: f32 = 16.;
        let max = r.pages.iter().map(|p| p.1).max().unwrap_or(0);
        let counts: HashMap<u8, u64> = r.pages.iter().copied().collect();
        let blocked_page = r.blocker.as_ref().map(|b| (b.pc >> 8) as u8);
        let lab = |t: String| div().w(px(S)).flex_none().flex().justify_center().text_xs().font_family(MONO).text_color(rgb(MUTED)).child(t);
        let mut columns = div().flex().gap(px(1.)).child(div().w(px(36.)));
        for lo in 0..16u8 {
            columns = columns.child(lab(format!("{lo:X}")));
        }
        let mut grid = div().flex().flex_col().gap(px(1.)).child(columns);
        for hi in 0..16u8 {
            let mut row = div().flex().gap(px(1.)).child(div().w(px(36.)).text_xs().font_family(MONO).text_color(rgb(MUTED)).child(format!("${hi:X}000")));
            for lo in 0..16u8 {
                let p = hi << 4 | lo;
                let n = counts.get(&p).copied().unwrap_or(0);
                let mut cell = div().w(px(S)).h(px(S)).bg(if n > 0 { heat(level(n, max)) } else { rgb(PANEL2).into() });
                if Some(p) == blocked_page {
                    cell = cell.border_2().border_color(rgb(ERR));
                }
                row = row.child(cell);
            }
            grid = grid.child(row);
        }
        panel("Code activity").child(grid).child(line("Instructions started per 256-byte page (row $B000, column 8 = $B800–$B8FF). Colour: log of the count; red border: blocked page.", MUTED))
    }

    // ---- memory ------------------------------------------------------------------------------

    fn pane_memory(&self) -> Div {
        let Some((_, r)) = self.emu_last() else { return div().child(panel("Memory").child(para("No run yet. Press Run or Step.", MUTED))) };
        let mut grids = div().flex().flex_wrap().gap_6().items_start().child(heat_grid("Internal RAM", 0x0000, &r.ram_internal, 16)).child(heat_grid("External RAM", 0x2000, &r.ram_external, 64));
        if let Some((_, _, ee)) = &r.eeprom {
            grids = grids.child(heat_grid("EEPROM (H2, simulated)", 0xB600, ee, 64));
        }
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.memory_map(r))
            .child(panel("RAM contents").child(grids).child(line("Each square is one byte after the last instruction; colour runs from blue (low) to red (high), dark = $00.", MUTED)))
    }

    /// The board model's address regions with fetch/read/write counts; the blocked region is red.
    fn memory_map(&self, r: &emu::Run) -> Div {
        let max = r.regions.iter().map(|g| g.fetch + g.read + g.write).max().unwrap_or(0);
        let hit = r.blocker.as_ref().and_then(|b| b.address);
        let mut row = div().flex().flex_wrap().gap_2();
        for g in &r.regions {
            let total = g.fetch + g.read + g.write;
            let blocked = hit.is_some_and(|a| (g.lo..=g.hi).contains(&a));
            let mut cell = div()
                .flex_1()
                .min_w(px(160.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(rgb(if blocked { ERR } else { BORDER }))
                .bg(if total > 0 { heat(level(total, max)) } else { rgb(PANEL2).into() })
                .child(div().text_sm().font_weight(FontWeight::BOLD).child(g.name.clone()))
                .child(div().text_xs().font_family(MONO).child(format!("${:04X}–${:04X}", g.lo, g.hi)))
                .child(div().text_xs().font_family(MONO).child(format!("F {}  R {}  W {}", grouped(g.fetch), grouped(g.read), grouped(g.write))));
            if blocked {
                cell = cell.child(div().text_xs().text_color(rgb(ERR)).font_weight(FontWeight::BOLD).child(format!("blocked at ${:04X}", hit.unwrap_or_default())));
            }
            row = row.child(cell);
        }
        let mut c = panel("Memory map").child(row).child(line("Board model regions. F = opcode/operand fetches, R = data reads, W = data writes; colour: log of the total.", MUTED));
        if r.rom_same_writes > 0 {
            c = c.child(line(format!("H1: {} EPROM-address writes of the byte already there were dropped (copy loop $8000–$9338).", grouped(r.rom_same_writes)), WARN));
        }
        for (a, v) in &r.latches {
            c = c.child(line(format!("H1 latch ${a:04X} = ${v:02X} · function unknown, no effect modelled"), WARN));
        }
        c
    }

    // ---- I/O ---------------------------------------------------------------------------------

    fn pane_io(&self, cx: &mut Context<Self>) -> Div {
        let Some((_, r)) = self.emu_last() else { return div().child(panel("I/O").child(para("No run yet. Press Run or Step.", MUTED))) };
        div().flex().flex_col().gap_3().child(self.io_table(r)).child(self.outputs(r)).child(self.events(r, cx))
    }

    /// Modelled HC11 registers the firmware touched; the blocking access is listed first in red.
    fn io_table(&self, r: &emu::Run) -> Div {
        let mut rows = vec![];
        if let Some(b) = r.blocker.as_ref().filter(|b| b.address.is_some()) {
            rows.push((vec![format!("${:04X}", b.address.unwrap_or_default()), "—".into(), "—".into(), format!("not modelled: {}", b.kind), String::new(), String::new()], ERR));
        }
        for io in &r.io {
            let bits: String = (0..8).rev().map(|bit| if io.unknown & (1 << bit) != 0 { '?' } else if io.value & (1 << bit) != 0 { '1' } else { '0' }).collect();
            let value = if io.unknown == 0 { format!("${:02X}", io.value) } else { "?".into() };
            rows.push((vec![format!("${:04X}", io.addr), io.name.clone(), value, bits, grouped(io.reads), grouped(io.writes)], TEXT));
        }
        panel("HC11 registers")
            .child(table("io", &[("addr", 6, false), ("name", 8, false), ("value", 6, false), ("bits b7…b0", 20, false), ("reads", 12, true), ("writes", 12, true)], rows))
            .child(line("Values at the current instruction. ? = unknown input bit copied into the output latch.", MUTED))
    }

    /// Raw output activity; which actuator (if any) each signal drives is unknown.
    fn outputs(&self, r: &emu::Run) -> Div {
        let o = &r.outputs;
        let width = |c: u64| if c == 0 { "—".to_string() } else { format!("{:.2} ms", ms(c)) };
        let pins = table(
            "pins",
            &[("pin", 4, false), ("level", 5, true), ("changes", 10, true), ("last high", 11, true), ("last low", 11, true)],
            (3..8).rev().map(|b| (vec![format!("PA{b}"), ((o.porta_pins >> b) & 1).to_string(), grouped(o.porta_changes[b]), width(o.porta_high[b]), width(o.porta_low[b])], TEXT)).collect(),
        );
        let mut latch_cols: Vec<(&str, usize, bool)> = vec![("latch", 6, false)];
        latch_cols.extend(["b7", "b6", "b5", "b4", "b3", "b2", "b1", "b0"].map(|b| (b, 6, true)));
        let latches = table(
            "latches",
            &latch_cols,
            [(0x1040u16, 0usize), (0x1050, 8)].iter().map(|&(a, base)| (std::iter::once(format!("${a:04X}")).chain((0..8).rev().map(|b| grouped(o.latch_changes[base + b]))).collect(), TEXT)).collect(),
        );
        let bytes = |v: &[u8]| v.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ");
        let mut serial = div()
            .flex()
            .flex_col()
            .gap_1()
            .text_sm()
            .child(div().flex().gap_3().child(div().w(px(140.)).flex_none().text_color(rgb(MUTED)).child("SPI bytes sent")).child(div().font_family(MONO).child(grouped(o.spi_bytes))))
            .child(div().id("spi-last").w_full().overflow_x_scroll().child(div().font_family(MONO).text_xs().text_color(rgb(MUTED)).whitespace_nowrap().child(format!("last: {}", bytes(&o.spi_last)))))
            .child(div().flex().gap_3().child(div().w(px(140.)).flex_none().text_color(rgb(MUTED)).child("SCI characters sent")).child(div().font_family(MONO).child(format!("{} · last: {}", grouped(o.sci_bytes), bytes(&o.sci_last)))));
        if let Some((init, ops, _)) = &r.eeprom {
            serial = serial.child(div().flex().gap_3().child(div().w(px(140.)).flex_none().text_color(rgb(MUTED)).child("Internal EEPROM")).child(div().child(format!("{} · {ops} program/erase operations (simulated, 10 ms VPP checked)", if *init { "in use" } else { "not accessed yet" }))));
        }
        panel("Raw outputs")
            .child(line("Mapping to actuators is unknown: no pin, latch bit or SPI byte has been identified as an injector or coil.", WARN))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_4()
                    .items_start()
                    .child(div().flex_1().min_w(px(300.)).flex().flex_col().gap_1().child(heading("PORTA OUTPUT-COMPARE PINS")).child(pins).child(line(format!("PORTA latch ${:02X}", o.porta_latch), MUTED)))
                    .child(div().flex_1().min_w(px(300.)).flex().flex_col().gap_1().child(heading("LATCH BIT CHANGES")).child(latches)),
            )
            .child(serial)
            .child(line("SPI/SCI received data is unknown in every profile; the firmware may only load it and discard it, otherwise the run stops.", MUTED))
    }

    /// Register-block events, latest first.
    fn events(&self, r: &emu::Run, cx: &mut Context<Self>) -> Div {
        let toggle = btn("raw", if self.emu.raw { "Show latest 16" } else { "Show all kept" }, self.emu.raw).on_click(cx.listener(|this, _, _, cx| {
            this.emu.raw = !this.emu.raw;
            cx.notify();
        }));
        let shown = if self.emu.raw { r.events.len() } else { 16 };
        let rows = r
            .events
            .iter()
            .rev()
            .take(shown)
            .map(|e| {
                let (what, color) = match e.kind {
                    'W' => ("write", TEXT),
                    'R' => ("read", TEXT),
                    'L' => ("latch", WARN),
                    'C' => ("COP", OK),
                    'I' => ("IRQ", ACCENT),
                    'T' => ("RTI →", ACCENT),
                    'O' => ("pins", WARN),
                    'S' => ("SPI →", WARN),
                    'X' => ("SCI →", WARN),
                    _ => ("EEPROM", WARN),
                };
                let port_rmw = e.addr == 0x1008 && r.events.iter().any(|other| other.inst == e.inst && other.addr == e.addr && other.kind == 'R');
                let value = if port_rmw { "input bits unknown".into() } else { format!("${:02X}", e.value) };
                (vec![grouped(e.inst), grouped(e.cycle), format!("${:04X}", e.pc), what.to_string(), format!("${:04X}", e.addr), emu_reg_name(r, e.addr), value], color)
            })
            .collect();
        panel(format!("Peripheral events · {} total, latest {} kept", grouped(r.events_total), r.events.len()))
            .child(div().flex().child(toggle))
            .child(table("events", &[("instruction", 12, true), ("E cycle", 15, true), ("PC", 6, false), ("event", 7, false), ("addr", 6, false), ("register", 8, false), ("value", 0, false)], rows))
    }

    // ---- lookup ------------------------------------------------------------------------------

    fn pane_lookup(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let Some(d) = self.emu_target() else { return div() };
        let co = self.emu.coord;
        let source = match emu::table_edits(&d.bytes, self.bytes_of(d)) {
            Ok(e) if e.is_empty() => line("Uses the original source bytes (no session edits).", MUTED),
            Ok(e) => line(format!("Uses the session working copy: {} edited byte(s), all inside 0x91AB..0x9282; everything else is the original.", e.len()), ACCENT),
            Err(e) => line(e, ERR),
        };
        let mut c = panel("Isolated map lookup · routine $B1C5..$B217 · table $91AB, stride 9")
            .child(line("Inputs are table coordinates and byte fractions (n/256) in the routine's RAM descriptor, not RPM, throttle, load or any sensor unit.", WARN))
            .child(self.knob(Knob::Coord(0), "Row (0–22)", co.row.to_string(), window, cx))
            .child(self.knob(Knob::Coord(1), "Column (0–7)", co.col.to_string(), window, cx))
            .child(self.knob(Knob::Coord(2), "Row fraction", format!("{}/256", co.row_frac), window, cx))
            .child(self.knob(Knob::Coord(3), "Column fraction", format!("{}/256", co.col_frac), window, cx))
            .child(div().flex().flex_wrap().gap_3().items_center().child(self.emu_btn("lookup", "Run lookup", !self.emu.busy, |this, cx| this.emu_run(Request::Lookup(this.emu.coord), cx), cx)).child(source));
        if self.emu.busy {
            c = c.child(line("The backend is busy with a Run chunk; Pause to run a lookup.", MUTED));
        }
        let snap = self.emu_snap();
        let cur = self.emu.lookup.as_ref().filter(|(s, _)| Some(s) == snap.as_ref()).map(|(_, r)| r);
        if cur.is_none() && self.emu.lookup.is_some() {
            c = c.child(line("The last lookup used other bytes (target or working copy changed); run it again.", MUTED));
        }
        let l = match cur {
            None => return div().child(c),
            Some(Err(e)) => return div().child(c.child(para(e.clone(), ERR))),
            Some(Ok(l)) => l,
        };
        let k = l.coord;
        let names = ["[r, c]", "[r, c+1]", "[r+1, c]", "[r+1, c+1]"];
        let cells = table("cells", &[("cell", 10, false), ("address", 8, false), ("value", 6, true), ("hex", 5, true)], names.iter().zip(l.cells).map(|(n, (a, v))| (vec![n.to_string(), format!("${a:04X}"), v.to_string(), format!("${v:02X}")], TEXT)).collect());
        let result = panel(format!("Result A = {} (${:02X}) · raw unsigned byte", l.result, l.result))
            .child(div().flex().flex_wrap().gap_6().items_start().child(four_cells(l)).child(div().flex_1().min_w(px(280.)).flex().flex_col().gap_2().child(cells).child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .text_sm()
                    .child(format!("For row {} + {}/256, column {} + {}/256", k.row, k.row_frac, k.col, k.col_frac))
                    .child(line(format!("{} instructions; stopped before RTS at $B218; result checked against the recovered arithmetic", l.instructions), MUTED))
                    .child(div().font_family(MONO).text_xs().text_color(rgb(MUTED)).child(regs_text(&l.regs))),
            )))
            .child(line(format!("Source SHA-256 {}", d.sha256), MUTED))
            .child(line(if l.edits > 0 { format!("Working SHA-256 {} · {} session edit(s) in the table", l.working_sha, l.edits) } else { format!("Working SHA-256 {} · original, no edits", l.working_sha) }, MUTED));
        div().flex().flex_col().gap_3().child(c).child(result)
    }

    // ---- notes -------------------------------------------------------------------------------

    fn pane_notes(&self, cx: &mut Context<Self>) -> Div {
        let sec = |title: &str, body: Vec<&str>| {
            let mut d = div().flex().flex_col().gap_1().max_w(px(900.)).child(div().text_base().font_weight(FontWeight::BOLD).child(title.to_string()));
            for p in body {
                d = d.child(para(p.to_string(), TEXT));
            }
            d
        };
        let run_text = format!("Run executes {} instructions per backend request and continues from that request's checkpoint (exact continuation, tested against one long run) until Pause, Reset or a blocker; there is no total cap. Execution stops before the first instruction that needs unmodelled hardware or uses an opcode without a conformance case. Session edits are not used by Run, but any edit resets the session.", grouped(CHUNK as u64));
        let mut c = panel("Model & caveats")
            .gap_4()
            .child(sec("Scope", vec![
                "The original firmware runs from reset on a patched µCsim 68HC11 board model. It is a research model, not an engine simulation.",
                "Crank speed is commanded, not produced by combustion, and throttle never changes it. Injector pulse width and ignition timing are not shown: no output pin, latch bit or SPI byte has been mapped to an injector or coil.",
            ]))
            .child(sec("Profiles", vec![
                "Strict: stops at the first access to unknown hardware.",
                "H1 (hypothesis): $1040/$1050 writes are treated as write-only latches with unknown effect, EPROM-address writes of the byte already there are dropped, and the diagnostic serial line is idle. Any other external access still stops.",
                "H2 (simulation): H1 plus named hypotheses and the supplied inputs (crank signal, A/D results, board input bytes, ports, EEPROM fill).",
            ]))
            .child(sec("Running", vec![&run_text, "H2 input changes apply at the next chunk boundary; the slider moving does not mean the firmware has seen the value. The Overview table shows requested, applied and firmware values separately."]))
            .child(sec("Crank signal (hypothesis)", vec![
                "A 60-2 wheel signal (58 teeth, gap after tooth 58) on PA7/PAI and PA0/IC3, falling edge active, as the firmware's own timer setup and tooth decoder imply. Board wiring, sensor polarity and the 2 MHz E clock that converts rpm to timer counts are unverified.",
                "Firmware speed formula (code at $429B, not assumed): $D1:$D2 = 256·58594/$BB, i.e. 15,000,064/$BB; $BB is the TCNT time between teeth 20 and 50 ($9618). Reading it as rpm assumes the 2 MHz E clock and TMSK2 prescaler 4 (2 µs per count). $BB = $FFFF means stopped or below about 229 rpm.",
            ]))
            .child(sec("Throttle input (hypothesis)", vec![
                "AN2 is the candidate from the firmware trace (range check, closed/open flags and endpoint learning on its copy $2003 → $C6); not proven on hardware. Percent maps linearly between the endpoints, which default to the calibration's initial learned values ($88FE closed, $88FD open).",
                "The firmware latches an AN2 fault ($1B) on out-of-range values (range $9284..$9285) or a large step between conversions (step limit $928A) and then substitutes a default. This is firmware behaviour and is not suppressed; Reset clears it.",
            ]))
            .child(sec("EEPROM", vec!["Default fill $FF = erased EEPROM. The real EEPROM content is not in the dump; the firmware treats $B7FF ≠ $AA as uninitialised and rewrites it (simulated only)."]));
        let mut backend = div().flex().flex_col().gap_1().max_w(px(900.)).child(div().text_base().font_weight(FontWeight::BOLD).child("Backend"));
        if let Some(d) = self.emu_target() {
            backend = backend
                .child(kv("Target", d.name.clone()).text_sm())
                .child(kv("Source SHA-256", d.sha256.clone()).text_sm())
                .child(kv("Profile", emu::profile(&d.sha256).map_or("unsupported source hash".to_string(), |p| format!("{p} · reviewed hash"))).text_sm());
        }
        backend = backend.child(kv("Simulator", emu::simulator().map_or_else(|e| e, |p| format!("µCsim {}", p.display()))).text_sm());
        if let Some((_, r)) = self.emu_last() {
            backend = backend
                .child(kv("Board model", r.board_text.clone()).text_sm())
                .child(kv("CPU core", r.core.clone()).text_sm())
                .child(kv("Last request", format!("{} instructions from instruction {}, {} executed in total", grouped(r.budget as u64), grouped(r.start), grouped(r.executed))).text_sm())
                .child(div().flex().pt_1().child(btn("raw-samples", if self.emu.raw { "Hide raw PC samples" } else { "Show raw PC samples" }, self.emu.raw).on_click(cx.listener(|this, _, _, cx| {
                    this.emu.raw = !this.emu.raw;
                    cx.notify();
                }))));
            if self.emu.raw {
                backend = backend.child(table("samples", &[("instruction", 12, true), ("E cycle", 15, true), ("PC", 6, false)], r.samples.iter().map(|(i, cyc, pc)| (vec![grouped(*i), grouped(*cyc), format!("${pc:04X}")], TEXT)).collect()));
            }
        }
        c = c.child(backend);
        div().child(c)
    }
}

/// A byte block as a value heat grid with row addresses every 8 rows.
fn heat_grid(title: &str, base: u16, bytes: &[u8], cols: usize) -> Div {
    const C: f32 = 9.;
    let mut g = div().flex().flex_col().gap(px(1.));
    for (k, row) in bytes.chunks(cols).enumerate() {
        let label = if k % 8 == 0 { format!("${:04X}", base as usize + k * cols) } else { String::new() };
        let mut r = div().flex().gap(px(1.)).h(px(C - 1.)).child(div().w(px(44.)).flex_none().text_xs().font_family(MONO).text_color(rgb(MUTED)).child(label));
        for &v in row {
            r = r.child(div().w(px(C - 1.)).h(px(C - 1.)).flex_none().bg(if v == 0 { rgb(PANEL2).into() } else { heat(v as f32 / 255.) }));
        }
        g = g.child(r);
    }
    let end = base as usize + bytes.len() - 1;
    div().flex().flex_col().gap_1().child(div().text_sm().child(format!("{title} ${base:04X}–${end:04X}"))).child(line(format!("{cols} bytes per row"), MUTED)).child(g)
}

fn emu_reg_name(r: &emu::Run, addr: u16) -> String {
    r.io.iter().find(|i| i.addr == addr).map_or_else(|| if addr == 0x103A { "COPRST".into() } else { String::new() }, |i| i.name.clone())
}

/// The four neighbouring cells as a 2 × 2 heat grid; the dot marks the interpolation point
/// between the cell centres, and the swatch shows the result on the same colour scale.
fn four_cells(l: &emu::Lookup) -> Div {
    const S: f32 = 80.;
    let cell = |v: u8| div().w(px(S)).h(px(S)).flex().items_center().justify_center().bg(heat(v as f32 / 255.)).text_base().font_family(MONO).text_color(rgb(TEXT)).child(v.to_string());
    let v = |k: usize| l.cells[k].1;
    let (fx, fy) = (l.coord.col_frac as f32 / 256., l.coord.row_frac as f32 / 256.);
    let dot = div()
        .absolute()
        .left(px(S / 2. + fx * S - 7.))
        .top(px(S / 2. + fy * S - 7.))
        .w(px(14.))
        .h(px(14.))
        .rounded_full()
        .border_2()
        .border_color(rgb(TEXT))
        .bg(heat(l.result as f32 / 255.));
    let grid = div().relative().w(px(2. * S)).h(px(2. * S)).child(div().flex().child(cell(v(0))).child(cell(v(1)))).child(div().flex().child(cell(v(2))).child(cell(v(3)))).child(dot);
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
        let mut emu = Emu { busy: true, running: true, inflight: Some(Inputs::default()), ..Emu::default() };
        let (request_generation, cancel) = (emu.generation, emu.cancel.clone());
        emu.session = Some(session(Err("old run error".into())));
        emu.lookup = Some((Snap::new("dump.bin".into(), "source-sha".into(), None), Err("old lookup error".into())));
        emu.invalidate();
        assert_ne!(request_generation, emu.generation);
        assert!(cancel.load(Ordering::Relaxed), "the in-flight backend process is told to stop");
        assert!(!emu.cancel.load(Ordering::Relaxed), "the next request gets a fresh flag");
        assert!(!emu.busy && !emu.running && emu.inflight.is_none() && emu.session.is_none() && emu.lookup.is_none());
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
        assert_eq!(h.iter().map(|p| (p.cycles, p.commanded, p.firmware, p.tps_in, p.tps_fw)).collect::<Vec<_>>(), vec![(4000, 900, None, 0x40, 0x80), (8000, 900, Some(899), 0x40, 0x80)]);
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

    #[test]
    fn slider_maps_pointer_and_keys_inside_its_range() {
        let rpm = Range { lo: 0, hi: 9000, step: 10, page: 250 };
        assert_eq!(knob_at(100., 100., 400., rpm), 0); // left edge
        assert_eq!(knob_at(500., 100., 400., rpm), 9000); // right edge
        assert_eq!(knob_at(-50., 100., 400., rpm), 0); // dragged past the left end
        assert_eq!(knob_at(900., 100., 400., rpm), 9000); // dragged past the right end
        assert_eq!(knob_at(300., 100., 400., rpm), 4500);
        assert_eq!(knob_at(301., 100., 400., rpm) % 10, 0, "snapped to the step");
        assert_eq!(knob_at(300., 100., 0., rpm), 0, "zero-width track");
        let tps = Range { lo: 0, hi: 144, step: 1, page: 14 };
        assert_eq!(knob_key("right", false, 10, tps), Some(11));
        assert_eq!(knob_key("left", true, 10, tps), Some(0), "Shift moves a page, clamped");
        assert_eq!(knob_key("pageup", false, 140, tps), Some(144));
        assert_eq!((knob_key("home", false, 70, tps), knob_key("end", false, 70, tps)), (Some(0), Some(144)));
        assert_eq!(knob_key("a", false, 70, tps), None, "other keys are not consumed");
    }

    #[test]
    fn chart_axes_use_round_ticks_and_cover_degenerate_ranges() {
        let t = time_axis(0., 9.3);
        assert_eq!(t.ticks.iter().map(|t| t.1.as_str()).collect::<Vec<_>>(), ["0", "2", "4", "6", "8"]);
        assert_eq!((t.frac(0.), t.frac(9.3), t.frac(20.)), (0., 1., 1.));
        let one = time_axis(61.1, 61.1); // a single sample still gets a usable range
        assert!(one.hi > one.lo && one.frac(61.1) == 0.);
        let y = zero_axis(1234., 1000.);
        assert_eq!((y.lo, y.hi), (0., 1500.));
        assert!(y.ticks.iter().all(|t| t.0 <= y.hi) && y.ticks.first().map(|t| t.0) == Some(0.));
        assert_eq!(zero_axis(0., 1000.).hi, 1000.);
        let i = inst_axis(0., 30_000_000.);
        assert_eq!(i.ticks.last().map(|t| t.1.as_str()), Some("30M"));
    }

    #[test]
    fn unknown_readings_break_chart_lines_instead_of_bridging() {
        let v = [Some(1.), Some(2.), None, None, Some(3.), None];
        let r = runs(v.iter().enumerate(), |&(k, y)| (k as f64, *y));
        assert_eq!(r, vec![vec![(0., 1.), (1., 2.)], vec![(4., 3.)]]);
        assert!(runs(std::iter::empty::<u8>(), |_| (0., None)).is_empty());
    }

    #[test]
    fn throttle_knob_moves_raw_from_the_closed_endpoint_either_direction() {
        assert_eq!(throttle_steps(128, (64, 208)), (64, 144));
        assert_eq!((throttle_raw(0, (64, 208)), throttle_raw(144, (64, 208))), (64, 208));
        // Reversed sensor: 0 % is the high raw value.
        assert_eq!((throttle_steps(208, (208, 64)), throttle_raw(144, (208, 64))), ((0, 144), 64));
        assert_eq!(throttle_percent(throttle_raw(72, (208, 64)), (208, 64)), Some(50));
        // A raw edit outside the endpoints pins the knob to the nearer end instead of wrapping.
        assert_eq!((throttle_steps(10, (64, 208)).0, throttle_steps(250, (64, 208)).0), (0, 144));
        assert_eq!(throttle_steps(64, (64, 64)), (0, 0), "equal endpoints: nothing to slide");
        assert_eq!(grouped(30_000_000), "30,000,000");
        assert_eq!(grouped(999), "999");
    }
}
