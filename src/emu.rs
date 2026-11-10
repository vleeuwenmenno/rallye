//! Bounded bridge to the external µCsim research backend (`tools/firmware_probe.py --bridge`).
//! Not an engine emulator: it runs the original firmware on a patched µCsim with an explicit,
//! partly hypothetical board model that stops at the first unsupported access. A session runs in
//! bounded chunks; each chunk continues from the previous chunk's checkpoint in a fresh simulator
//! process. Also runs one isolated lookup. See docs/firmware-emulation.md.

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const SCRIPT: &str = include_str!("../tools/firmware_probe.py");
pub const SETUP: &str = concat!("make -C ", env!("CARGO_MANIFEST_DIR"), " emulator-setup");
const TIMEOUT: Duration = Duration::from_secs(60);
const OUTPUT_LIMIT: u64 = 4 << 20;
/// Instruction budget of one request (MAX_RUN in firmware_probe.py); a session has no total cap.
pub const MAX_RUN: u32 = 2_000_000;
/// Assumed E clock (8 MHz crystal / 4); only labels virtual time.
pub const E_CLOCK_HZ: u64 = 2_000_000;
const PROTOCOL: u64 = 3;
/// The only bytes a session edit may change for a lookup (24 rows × stride 9 at 0x91AB).
pub const TABLE: RangeInclusive<usize> = 0x91AB..=0x9282;
/// Reviewed firmware profiles; the bridge re-checks the same list (SUPPORTED in firmware_probe.py).
const PROFILES: [(&str, &str); 2] = [
    ("e91f72ee8e80068b523fcff4c2bb5ecf0ae3f8df6cfe3ec31808133e215d5663", "106XSI2.ORI (XSI2)"),
    ("081bc0ede46963f101bab5e6a0a69e9a7babb11bb1122928a815c8f1032da725", "REMAP MILD EURO95 RUNNING.BIN (mild remap)"),
];

pub fn profile(sha: &str) -> Option<&'static str> {
    PROFILES.iter().find(|(s, _)| *s == sha).map(|(_, name)| *name)
}

/// Table coordinates and byte fractions (n/256) of the lookup descriptor; not engine units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Coord {
    pub row: u8,
    pub col: u8,
    pub row_frac: u8,
    pub col_frac: u8,
}

impl Coord {
    pub const ROW_MAX: u8 = 22;
    pub const COL_MAX: u8 = 7;
    /// File offsets of the four cells the routine interpolates.
    pub fn cells(self) -> [usize; 4] {
        let base = TABLE.start() + self.row as usize * 9 + self.col as usize;
        [base, base + 1, base + 9, base + 10]
    }
}

/// Board profile. Strict uses only manual-documented HC11 behaviour and regions inferred from the
/// firmware; H1 adds named, unverified board assumptions; H2 adds more and takes supplied inputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Board {
    Strict,
    H1,
    H2,
}

impl Board {
    pub fn key(self) -> &'static str {
        match self {
            Board::Strict => "strict",
            Board::H1 => "h1",
            Board::H2 => "h2",
        }
    }
}

/// H2 simulated inputs in raw units. Their physical meaning is unknown; the defaults are explicit
/// hypotheses (DEFAULT_INPUTS in firmware_probe.py), not measurements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inputs {
    pub in1060: u8,
    pub in1080: u8,
    /// PORTA input pins PA0-PA2 and PA7 (output bits are ignored).
    pub porta_in: u8,
    /// PORTD input pins PD0-PD5 (0..=63).
    pub portd_in: u8,
    /// Internal EEPROM content at power-up (every byte).
    pub eeprom_fill: u8,
    /// Raw A/D results of channels AN0..AN7.
    pub adc: [u8; 8],
}

impl Default for Inputs {
    fn default() -> Self {
        Self { in1060: 0x00, in1080: 0x00, porta_in: 0x00, portd_in: 0x3F, eeprom_fill: 0xFF, adc: [0x80; 8] }
    }
}

impl Inputs {
    fn json(&self) -> Value {
        json!({"in1060": self.in1060, "in1080": self.in1080, "porta_in": self.porta_in, "portd_in": self.portd_in,
            "eeprom_fill": self.eeprom_fill, "adc": self.adc})
    }
}

/// Opaque continuation state of a session, bound by the backend to source hash, profile and inputs.
#[derive(Clone, Debug, PartialEq)]
pub struct Checkpoint {
    pub state: String,
    pub digest: String,
    /// Instructions executed when it was taken.
    pub inst: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunReq {
    pub board: Board,
    pub budget: u32,
    /// Sent only for H2.
    pub inputs: Inputs,
    /// None: start from reset.
    pub checkpoint: Option<Checkpoint>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Run(RunReq),
    Lookup(Coord),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Regs {
    pub pc: u16,
    pub sp: u16,
    pub a: u8,
    pub b: u8,
    pub x: u16,
    pub y: u16,
    pub cc: u8,
}

/// The first instruction that was not executed, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct Blocker {
    pub pc: u16,
    pub bytes: String,
    pub text: String,
    pub kind: String,
    pub address: Option<u16>,
    pub value: Option<u16>,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Region {
    pub name: String,
    pub lo: u16,
    pub hi: u16,
    pub fetch: u64,
    pub read: u64,
    pub write: u64,
}

/// One modelled HC11 register the firmware touched.
#[derive(Clone, Debug, PartialEq)]
pub struct Io {
    pub addr: u16,
    pub name: String,
    pub value: u8,
    pub reads: u64,
    pub writes: u64,
    pub unknown: u8,
}

/// R read, W write, L external latch write, C COP service, I interrupt entry (addr: vector),
/// T RTI (addr: return PC), O PORTA output pins changed, S SPI byte sent, X SCI character sent,
/// E EEPROM byte programmed or erased.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub seq: u64,
    pub inst: u64,
    pub cycle: u64,
    pub pc: u16,
    pub kind: char,
    pub addr: u16,
    pub value: u8,
}

/// Interrupt source served at least once: (Figure 5-4 name, vector, entries, E cycle of the last).
#[derive(Clone, Debug, PartialEq)]
pub struct Irq {
    pub name: String,
    pub vector: u16,
    pub count: u64,
    pub last_cycle: u64,
}

/// Visits of the foreground loop head and the stack pointer seen there.
#[derive(Clone, Debug, PartialEq)]
pub struct Loop {
    pub pc: u16,
    pub count: u64,
    pub sp: Option<(u16, u16)>,
    pub gap_max: u64,
}

/// Raw outputs of the firmware; which actuator (if any) each drives is unknown.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Outputs {
    pub porta_latch: u8,
    pub porta_pins: u8,
    pub porta_changes: [u64; 8],
    pub spi_bytes: u64,
    pub spi_last: Vec<u8>,
    pub sci_bytes: u64,
    pub sci_last: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub board: Board,
    pub board_text: String,
    pub core: String,
    pub budget: u32,
    /// Instructions executed before this request (the checkpoint it continued from).
    pub start: u64,
    pub executed: u64,
    pub cycles: u64,
    pub regs: Regs,
    /// (instructions, E cycles, PC) after each replay chunk, starting at reset.
    pub samples: Vec<(u64, u64, u16)>,
    pub blocker: Option<Blocker>,
    /// (address, landmark, first instruction number that executed it)
    pub milestones: Vec<(u16, String, Option<u64>)>,
    pub regions: Vec<Region>,
    pub io: Vec<Io>,
    pub latches: Vec<(u16, u8)>,
    pub events: Vec<Event>,
    pub events_total: u64,
    /// Instructions started per 256-byte page.
    pub pages: Vec<(u8, u64)>,
    pub cop_services: u64,
    pub cop_cycle: u64,
    pub tcnt: u16,
    pub rom_same_writes: u64,
    pub opcodes: (u64, u64),
    pub cop_gap_max: u64,
    /// (address, landmark, executions)
    pub watch: Vec<(u16, String, u64)>,
    pub interrupts: Vec<Irq>,
    pub rti: u64,
    pub main_loop: Loop,
    pub outputs: Outputs,
    /// H2 only: (initialised, program/erase operations, 512 bytes).
    pub eeprom: Option<(bool, u64, Vec<u8>)>,
    pub ram_internal: Vec<u8>,
    pub ram_external: Vec<u8>,
    /// None after a blocker: the session cannot continue.
    pub checkpoint: Option<Checkpoint>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Lookup {
    pub coord: Coord,
    pub working_sha: String,
    pub edits: usize,
    pub result: u8,
    pub cells: [(u16, u8); 4],
    pub instructions: u64,
    pub regs: Regs,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Run(Box<Run>),
    Lookup(Lookup),
}

/// Simulator from `UCSIM` (path or name on PATH), else the project-local `make emulator-setup` build.
pub fn simulator() -> Result<PathBuf, String> {
    let (p, from) = match std::env::var_os("UCSIM").filter(|v| !v.is_empty()) {
        Some(v) => (PathBuf::from(v), "UCSIM"),
        None => {
            let exe = std::env::current_exe().map_err(|e| format!("cannot locate rallye executable: {e}"))?;
            (exe.parent().unwrap_or(Path::new(".")).join("../ucsim/src/sims/m68hc11.src/ucsim_m68hc11"), "default")
        }
    };
    let found = if p.components().count() == 1 {
        std::env::var_os("PATH").and_then(|ps| std::env::split_paths(&ps).map(|d| d.join(&p)).find(|c| c.is_file()))
    } else {
        p.is_file().then(|| p.canonicalize().unwrap_or(p.clone()))
    };
    found.ok_or_else(|| format!("µCsim backend not found ({from}: {}). Build it with: {SETUP}", p.display()))
}

/// Session edits as (offset, value); refused when any edited byte lies outside the table region.
pub fn table_edits(original: &[u8], work: &[u8]) -> Result<Vec<(usize, u8)>, String> {
    if original.len() != work.len() {
        return Err("working copy length differs from the source".into());
    }
    let edits: Vec<_> = original.iter().zip(work).enumerate().filter(|(_, (o, w))| o != w).map(|(i, (_, &w))| (i, w)).collect();
    let outside = edits.iter().filter(|(i, _)| !TABLE.contains(i)).count();
    if outside > 0 {
        return Err(format!("working copy has {outside} edited byte(s) outside 0x91AB..0x9282; lookup refused (Undo them or use Reload)"));
    }
    Ok(edits)
}

/// Validated JSON request for the bridge. `original` must be the unedited source bytes.
pub fn request_json(req: &Request, sim: &Path, original: &[u8], sha: &str, edits: &[(usize, u8)]) -> Result<String, String> {
    if profile(sha).is_none() || original.len() != 65536 {
        return Err("unsupported firmware: only the reviewed XSI2 and mild-remap source hashes can be emulated".into());
    }
    let mut v = json!({"version": PROTOCOL, "simulator": sim.to_string_lossy(), "sha256": sha, "image": hex(original)});
    match req {
        Request::Run(r) if r.budget <= MAX_RUN && r.inputs.portd_in <= 63 => {
            v["action"] = "run".into();
            v["profile"] = r.board.key().into();
            v["budget"] = r.budget.into();
            v["inputs"] = if r.board == Board::H2 { r.inputs.json() } else { Value::Null };
            v["checkpoint"] = r.checkpoint.as_ref().map_or(Value::Null, |c| json!({"state": c.state, "digest": c.digest}));
        }
        &Request::Lookup(c) if c.row <= Coord::ROW_MAX && c.col <= Coord::COL_MAX && edits.iter().all(|(i, _)| TABLE.contains(i)) => {
            v["action"] = "lookup".into();
            for (k, n) in [("row", c.row), ("column", c.col), ("row_fraction", c.row_frac), ("column_fraction", c.col_frac)] {
                v[k] = n.into();
            }
            v["edits"] = edits.iter().map(|&(i, b)| json!([i, b])).collect();
        }
        _ => return Err(format!("request out of range: {req:?}")),
    }
    Ok(v.to_string())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|v| format!("{v:02x}")).collect()
}

/// Run one request through the bridge (blocking; call from a background thread). Setting `cancel`
/// kills the backend process tree's root and returns an error.
pub fn run(req: &Request, sim: &Path, original: &[u8], sha: &str, edits: &[(usize, u8)], cancel: &AtomicBool) -> Result<Outcome, String> {
    let input = request_json(req, sim, original, sha, edits)?;
    let out = bridge(input, cancel)?;
    let v: Value = serde_json::from_str(&out).map_err(|e| format!("malformed backend response: {e}"))?;
    parse(req, sha, edits.len(), &v)
}

/// python3 -I with the embedded script: no shell, bounded output, hard timeout, cancellable.
/// Cancellation terminates the process group containing Python and its µCsim child.
fn bridge(input: String, cancel: &AtomicBool) -> Result<String, String> {
    let mut cmd = Command::new("python3");
    cmd.args(["-I", "-c", SCRIPT, "--bridge"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0); // python and its µCsim share one group
    let mut child = cmd.spawn().map_err(|e| format!("cannot start python3: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    fn drain(r: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = r.take(OUTPUT_LIMIT + 1).read_to_end(&mut v);
            v
        })
    }
    let (out, err) = (drain(child.stdout.take().expect("piped stdout")), drain(child.stderr.take().expect("piped stderr")));
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < deadline && !cancel.load(Ordering::Relaxed) => std::thread::sleep(Duration::from_millis(20)),
            r => {
                stop(&mut child);
                return Err(match r {
                    Err(e) => format!("backend wait failed: {e}"),
                    _ if cancel.load(Ordering::Relaxed) => "cancelled".into(),
                    _ => format!("backend timed out after {} s", TIMEOUT.as_secs()),
                });
            }
        }
    };
    let _ = writer.join();
    let (out, err) = (out.join().unwrap_or_default(), err.join().unwrap_or_default());
    if out.len() as u64 > OUTPUT_LIMIT {
        return Err("backend output exceeded 4 MiB".into());
    }
    if !status.success() {
        let e = String::from_utf8_lossy(&err);
        let tail: String = e.chars().rev().take(600).collect::<Vec<_>>().into_iter().rev().collect();
        return Err(format!("backend failed ({status}): {tail}"));
    }
    String::from_utf8(out).map_err(|_| "backend output is not UTF-8".into())
}

/// End the bridge and the µCsim it started. SIGTERM to the process group lets Python remove its
/// temporary directory and µCsim exit; SIGKILL follows if Python is still alive after 2 s.
fn stop(child: &mut Child) {
    #[cfg(unix)]
    {
        let group = format!("-{}", child.id());
        let _ = Command::new("kill").args(["-TERM", "--", &group]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        let end = Instant::now() + Duration::from_secs(2);
        while Instant::now() < end {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn bad(k: &str) -> String {
    format!("malformed backend response: field '{k}'")
}

fn field<'a>(v: &'a Value, k: &str) -> Result<&'a Value, String> {
    v.get(k).ok_or_else(|| bad(k))
}

fn num(v: &Value, k: &str, max: u64) -> Result<u64, String> {
    field(v, k)?.as_u64().filter(|&n| n <= max).ok_or_else(|| bad(k))
}

fn text(v: &Value, k: &str) -> Result<String, String> {
    field(v, k)?.as_str().map(str::to_string).ok_or_else(|| bad(k))
}

fn opt(v: &Value, k: &str) -> Result<Option<u16>, String> {
    match field(v, k)? {
        Value::Null => Ok(None),
        _ => num(v, k, 0xFFFF).map(|n| Some(n as u16)),
    }
}

fn regs(v: &Value) -> Result<Regs, String> {
    let w = |k| num(v, k, 0xFFFF).map(|n| n as u16);
    let b = |k| num(v, k, 0xFF).map(|n| n as u8);
    Ok(Regs { pc: w("pc")?, sp: w("sp")?, a: b("a")?, b: b("b")?, x: w("x")?, y: w("y")?, cc: b("cc")? })
}

fn list<'a>(v: &'a Value, k: &str) -> Result<&'a Vec<Value>, String> {
    field(v, k)?.as_array().ok_or_else(|| bad(k))
}

/// Strict response check: every echoed field must match the request it answers.
pub fn parse(req: &Request, sha: &str, edits: usize, v: &Value) -> Result<Outcome, String> {
    if v.get("version") != Some(&json!(PROTOCOL)) {
        return Err(bad("version"));
    }
    match v.get("ok").and_then(Value::as_bool) {
        Some(true) => {}
        Some(false) => return Err(format!("backend: {}", text(v, "error")?)),
        None => return Err(bad("ok")),
    }
    if text(v, "sha256")? != sha {
        return Err(bad("sha256"));
    }
    let action = text(v, "action")?;
    match req {
        Request::Run(r) => {
            let start = r.checkpoint.as_ref().map_or(0, |c| c.inst);
            let inputs = if r.board == Board::H2 { r.inputs.json() } else { Value::Null };
            if action != "run"
                || text(v, "profile")? != r.board.key()
                || num(v, "budget", MAX_RUN as u64)? != r.budget as u64
                || num(v, "start", u64::MAX)? != start
                || field(v, "inputs")? != &inputs
            {
                return Err(bad("run echo"));
            }
            parse_run(r.board, r.budget, start, v).map(|r| Outcome::Run(Box::new(r)))
        }
        &Request::Lookup(c) => {
            let echo = [("row", c.row), ("column", c.col), ("row_fraction", c.row_frac), ("column_fraction", c.col_frac)];
            if action != "lookup" || echo.iter().any(|&(k, n)| num(v, k, 255).ok() != Some(n as u64)) || num(v, "edits", 216)? != edits as u64 {
                return Err(bad("lookup echo"));
            }
            let cells = list(v, "cells")?;
            let expect = c.cells();
            if cells.len() != 4 {
                return Err(bad("cells"));
            }
            let mut out = [(0, 0); 4];
            for (k, cell) in cells.iter().enumerate() {
                match cell.as_array().map(Vec::as_slice) {
                    Some([a, b]) if a.as_u64() == Some(expect[k] as u64) && b.as_u64().is_some_and(|b| b <= 0xFF) => {
                        out[k] = (expect[k] as u16, b.as_u64().unwrap_or_default() as u8)
                    }
                    _ => return Err(bad("cells")),
                }
            }
            let (result, r) = (num(v, "result", 0xFF)? as u8, regs(field(v, "registers")?)?);
            if r.pc != 0xB218 || r.a != result {
                return Err(bad("registers"));
            }
            let working_sha = text(v, "working_sha256")?;
            if working_sha.len() != 64 || (edits == 0) != (working_sha == sha) {
                return Err(bad("working_sha256"));
            }
            Ok(Outcome::Lookup(Lookup { coord: c, working_sha, edits, result, cells: out, instructions: num(v, "instructions", 10_000)?, regs: r }))
        }
    }
}

/// Fixed-length JSON array of numbers.
fn nums<const N: usize>(v: &Value, k: &str) -> Result<[u64; N], String> {
    let a = v.as_array().filter(|a| a.len() == N).ok_or_else(|| bad(k))?;
    let mut out = [0; N];
    for (o, x) in out.iter_mut().zip(a) {
        *o = x.as_u64().ok_or_else(|| bad(k))?;
    }
    Ok(out)
}

fn word(n: u64, k: &str) -> Result<u16, String> {
    u16::try_from(n).map_err(|_| bad(k))
}

fn byte(n: u64, k: &str) -> Result<u8, String> {
    u8::try_from(n).map_err(|_| bad(k))
}

fn bytes_of(v: &Value, k: &str, len: usize) -> Result<Vec<u8>, String> {
    let s = text(v, k)?;
    if s.len() != 2 * len || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(bad(k));
    }
    (0..len).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|_| bad(k))).collect()
}

fn byte_list(v: &Value, k: &str) -> Result<Vec<u8>, String> {
    list(v, k)?.iter().map(|b| b.as_u64().and_then(|b| u8::try_from(b).ok()).ok_or_else(|| bad(k))).collect()
}

/// A run must be internally consistent: counters, samples and landmarks within what was executed.
fn parse_run(board: Board, budget: u32, start: u64, v: &Value) -> Result<Run, String> {
    let executed = num(v, "executed", start + budget as u64)?;
    if executed < start {
        return Err(bad("executed"));
    }
    let cycles = num(v, "cycles", u64::MAX)?;
    if num(v, "e_clock_hz", u64::MAX)? != E_CLOCK_HZ {
        return Err(bad("e_clock_hz"));
    }
    let blocker = match field(v, "blocker")? {
        Value::Null => None,
        b => Some(Blocker { pc: num(b, "pc", 0xFFFF)? as u16, bytes: text(b, "bytes")?, text: text(b, "text")?, kind: text(b, "kind")?, address: opt(b, "address")?, value: opt(b, "value")?, reason: text(b, "reason")? }),
    };
    if blocker.is_none() && executed != start + budget as u64 {
        return Err(bad("executed"));
    }
    let regs = regs(field(v, "registers")?)?;
    let samples = list(v, "samples")?
        .iter()
        .map(|s| nums::<3>(s, "samples").and_then(|[i, c, pc]| Ok((i, c, word(pc, "samples")?))))
        .collect::<Result<Vec<_>, _>>()?;
    let ordered = samples.windows(2).all(|w| w[0].0 <= w[1].0 && w[0].1 <= w[1].1);
    let from_reset = start > 0 || samples.first().map(|s| s.1) == Some(0);
    if !ordered || samples.first().map(|s| s.0) != Some(start) || !from_reset || samples.last() != Some(&(executed, cycles, regs.pc)) {
        return Err(bad("samples"));
    }
    if blocker.as_ref().is_some_and(|b| b.pc != regs.pc) {
        return Err(bad("blocker"));
    }
    let milestones = list(v, "milestones")?
        .iter()
        .map(|m| match m.as_array().map(Vec::as_slice) {
            Some([a, n, i]) => {
                let inst = if i.is_null() { None } else { Some(i.as_u64().filter(|&i| (1..=executed).contains(&i)).ok_or_else(|| bad("milestones"))?) };
                Ok((word(a.as_u64().ok_or_else(|| bad("milestones"))?, "milestones")?, n.as_str().ok_or_else(|| bad("milestones"))?.to_string(), inst))
            }
            _ => Err(bad("milestones")),
        })
        .collect::<Result<Vec<_>, String>>()?;
    let regions = list(v, "regions")?
        .iter()
        .map(|r| match r.as_array().map(Vec::as_slice) {
            Some([n, rest @ ..]) if rest.len() == 5 => {
                let [lo, hi, fetch, read, write] = nums::<5>(&Value::Array(rest.to_vec()), "regions")?;
                Ok(Region { name: n.as_str().ok_or_else(|| bad("regions"))?.to_string(), lo: word(lo, "regions")?, hi: word(hi, "regions")?, fetch, read, write })
            }
            _ => Err(bad("regions")),
        })
        .collect::<Result<Vec<_>, String>>()?;
    let contiguous = regions.first().map(|r| r.lo) == Some(0) && regions.last().map(|r| r.hi) == Some(0xFFFF) && regions.windows(2).all(|w| w[0].hi as u32 + 1 == w[1].lo as u32);
    if !contiguous {
        return Err(bad("regions"));
    }
    let io = list(v, "io")?
        .iter()
        .map(|r| match r.as_array().map(Vec::as_slice) {
            Some([a, n, val, rd, wr, unknown]) => {
                let [a, val, rd, wr, unknown] = nums::<5>(&json!([a, val, rd, wr, unknown]), "io")?;
                let addr = word(a, "io").ok().filter(|a| (0x1000..=0x103F).contains(a)).ok_or_else(|| bad("io"))?;
                Ok(Io { addr, name: n.as_str().ok_or_else(|| bad("io"))?.to_string(), value: byte(val, "io")?, reads: rd, writes: wr, unknown: byte(unknown, "io")? })
            }
            _ => Err(bad("io")),
        })
        .collect::<Result<Vec<_>, String>>()?;
    let latches = list(v, "latches")?.iter().map(|l| nums::<2>(l, "latches").and_then(|[a, b]| Ok((word(a, "latches")?, byte(b, "latches")?)))).collect::<Result<Vec<_>, _>>()?;
    if board == Board::Strict && !latches.is_empty() {
        return Err(bad("latches")); // strict never accepts an external write
    }
    let events = list(v, "events")?
        .iter()
        .map(|e| match e.as_array().map(Vec::as_slice) {
            Some([seq, inst, cycle, pc, kind, addr, value]) => {
                let [seq, inst, cycle, pc, addr, value] = nums::<6>(&json!([seq, inst, cycle, pc, addr, value]), "events")?;
                let kind = kind.as_str().and_then(|k| k.chars().next().filter(|c| k.len() == 1 && "RWLCITOSXE".contains(*c))).ok_or_else(|| bad("events"))?;
                if inst > executed || cycle > cycles {
                    return Err(bad("events"));
                }
                Ok(Event { seq, inst, cycle, pc: word(pc, "events")?, kind, addr: word(addr, "events")?, value: byte(value, "events")? })
            }
            _ => Err(bad("events")),
        })
        .collect::<Result<Vec<_>, String>>()?;
    let pages = list(v, "pages")?.iter().map(|p| nums::<2>(p, "pages").and_then(|[p, c]| Ok((byte(p, "pages")?, c)))).collect::<Result<Vec<_>, _>>()?;
    if pages.iter().map(|p| p.1).sum::<u64>() != executed {
        return Err(bad("pages"));
    }
    let ops = field(v, "opcodes")?;
    let watch = list(v, "watch")?
        .iter()
        .map(|w| match w.as_array().map(Vec::as_slice) {
            Some([a, n, c]) => Ok((word(a.as_u64().ok_or_else(|| bad("watch"))?, "watch")?, n.as_str().ok_or_else(|| bad("watch"))?.to_string(), c.as_u64().filter(|&c| c <= executed).ok_or_else(|| bad("watch"))?)),
            _ => Err(bad("watch")),
        })
        .collect::<Result<Vec<_>, String>>()?;
    let interrupts = list(v, "interrupts")?
        .iter()
        .map(|i| match i.as_array().map(Vec::as_slice) {
            Some([_, n, vec, c, last]) => Ok(Irq {
                name: n.as_str().ok_or_else(|| bad("interrupts"))?.to_string(),
                vector: word(vec.as_u64().ok_or_else(|| bad("interrupts"))?, "interrupts")?,
                count: c.as_u64().ok_or_else(|| bad("interrupts"))?,
                last_cycle: last.as_u64().filter(|&l| l <= cycles).ok_or_else(|| bad("interrupts"))?,
            }),
            _ => Err(bad("interrupts")),
        })
        .collect::<Result<Vec<_>, String>>()?;
    let rti = num(v, "rti", u64::MAX)?;
    let entries: u64 = interrupts.iter().map(|i| i.count).sum();
    if rti > entries {
        return Err(bad("rti")); // every return needs an entry
    }
    let l = field(v, "loop")?;
    let count = num(l, "count", executed)?;
    let sp = match (field(l, "sp_min")?, field(l, "sp_max")?) {
        (Value::Null, Value::Null) if count == 0 => None,
        _ if count > 0 => Some((num(l, "sp_min", 0xFFFF)? as u16, num(l, "sp_max", 0xFFFF)? as u16)),
        _ => return Err(bad("loop")),
    };
    let main_loop = Loop { pc: num(l, "pc", 0xFFFF)? as u16, count, sp, gap_max: num(l, "gap_max", cycles)? };
    let o = field(v, "outputs")?;
    let changes = nums::<8>(field(o, "porta_changes")?, "outputs")?;
    let outputs = Outputs {
        porta_latch: num(o, "porta_latch", 0xFF)? as u8,
        porta_pins: num(o, "porta_pins", 0xFF)? as u8,
        porta_changes: changes,
        spi_bytes: num(o, "spi_bytes", u64::MAX)?,
        spi_last: byte_list(o, "spi_last")?,
        sci_bytes: num(o, "sci_bytes", u64::MAX)?,
        sci_last: byte_list(o, "sci_last")?,
    };
    if outputs.spi_last.len() > 32 || outputs.sci_last.len() > 32 {
        return Err(bad("outputs"));
    }
    let eeprom = match field(v, "eeprom")? {
        Value::Null if board != Board::H2 => None,
        e if board == Board::H2 => Some((field(e, "init")?.as_bool().ok_or_else(|| bad("eeprom"))?, num(e, "ops", u64::MAX)?, bytes_of(e, "bytes", 512)?)),
        _ => return Err(bad("eeprom")),
    };
    let ram = field(v, "ram")?;
    let checkpoint = match field(v, "checkpoint")? {
        Value::Null => None,
        _ if blocker.is_some() => return Err(bad("checkpoint")),
        c => Some(Checkpoint { state: text(c, "state")?, digest: text(c, "digest")?, inst: executed }),
    };
    if blocker.is_none() && checkpoint.is_none() {
        return Err(bad("checkpoint"));
    }
    Ok(Run {
        board,
        board_text: text(v, "board")?,
        core: text(v, "core")?,
        budget,
        executed,
        cycles,
        regs,
        samples,
        blocker,
        milestones,
        regions,
        io,
        latches,
        events,
        events_total: num(v, "events_total", u64::MAX)?,
        pages,
        cop_services: num(v, "cop_services", u64::MAX)?,
        cop_cycle: num(v, "cop_cycle", cycles)?,
        tcnt: num(v, "tcnt", 0xFFFF)? as u16,
        rom_same_writes: num(v, "rom_same_writes", u64::MAX)?,
        opcodes: (num(ops, "executed", 1024)?, num(ops, "audited", 1024)?),
        cop_gap_max: num(v, "cop_gap_max", cycles)?,
        start,
        watch,
        interrupts,
        rti,
        main_loop,
        outputs,
        eeprom,
        ram_internal: bytes_of(ram, "internal", 256)?,
        ram_external: bytes_of(ram, "external", 2048)?,
        checkpoint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const XSI2: &str = "e91f72ee8e80068b523fcff4c2bb5ecf0ae3f8df6cfe3ec31808133e215d5663";
    const C: Coord = Coord { row: 7, col: 3, row_frac: 192, col_frac: 64 };
    static NO: AtomicBool = AtomicBool::new(false);

    fn regs_json(pc: u16, a: u8) -> Value {
        json!({"pc": pc, "sp": 0x27FF, "a": a, "b": 0, "x": 0, "y": 0, "cc": 0xD0})
    }

    fn run_req(board: Board, budget: u32, checkpoint: Option<Checkpoint>) -> Request {
        Request::Run(RunReq { board, budget, inputs: Inputs::default(), checkpoint })
    }

    fn common(v: &mut Value) {
        for (k, x) in [
            ("watch", json!([[0xD4A2, "loop", 0]])),
            ("interrupts", json!([])),
            ("rti", json!(0)),
            ("loop", json!({"pc": 0xD4A2, "count": 0, "sp_min": null, "sp_max": null, "gap_max": 0})),
            ("outputs", json!({"porta_latch": 0, "porta_pins": 0, "porta_changes": [0, 0, 0, 0, 0, 0, 0, 0], "spi_bytes": 0, "spi_last": [], "sci_bytes": 0, "sci_last": []})),
            ("eeprom", Value::Null),
            ("ram", json!({"internal": "00".repeat(256), "external": "00".repeat(2048)})),
            ("cop_gap_max", json!(0)),
            ("inputs", Value::Null),
        ] {
            v[k] = x;
        }
    }

    fn run_json() -> Value {
        let mut v = json!({"version": 3, "ok": true, "action": "run", "sha256": XSI2, "profile": "strict", "board": "b", "core": "c",
            "budget": 100, "start": 0, "executed": 20, "cycles": 71, "e_clock_hz": 2_000_000, "registers": regs_json(0xB834, 6),
            "samples": [[0, 0, 0xB800], [10, 40, 0xB81B], [20, 71, 0xB834]],
            "blocker": {"pc": 0xB834, "bytes": "B7 10 40", "text": "STA A,$1040", "kind": "write", "address": 0x1040, "value": 6, "reason": "r"},
            "milestones": [[0xB800, "Reset vector", 1], [0xB834, "External latches", null]],
            "regions": [["internal RAM", 0, 0xFF, 0, 2, 4], ["rest", 0x100, 0xFFFF, 55, 0, 7]],
            "io": [[0x103D, "INIT", 1, 0, 1, 0]], "latches": [], "events": [[0, 6, 23, 0xB80E, "W", 0x103D, 1]], "events_total": 1,
            "pages": [[0xB8, 20]], "cop_services": 0, "cop_cycle": 0, "tcnt": 17, "rom_same_writes": 0,
            "opcodes": {"executed": 7, "audited": 82}, "checkpoint": null});
        common(&mut v);
        v
    }

    /// A continued H2 chunk: 1000 instructions from a checkpoint at 1000, with interrupts and the loop.
    fn h2_json() -> Value {
        let mut v = run_json();
        common(&mut v);
        for (k, x) in [
            ("profile", json!("h2")),
            ("start", json!(1000)),
            ("budget", json!(1000)),
            ("executed", json!(2000)),
            ("cycles", json!(8000)),
            ("samples", json!([[1000, 4000, 0xD4A2], [2000, 8000, 0xD4A5]])),
            ("registers", regs_json(0xD4A5, 0)),
            ("blocker", Value::Null),
            ("pages", json!([[0xD4, 2000]])),
            ("interrupts", json!([[1, "RTI", 0xFFF0, 3, 7000], [10, "TOF", 0xFFDE, 1, 6000]])),
            ("rti", json!(4)),
            ("loop", json!({"pc": 0xD4A2, "count": 5, "sp_min": 0x27FF, "sp_max": 0x27FF, "gap_max": 900})),
            ("eeprom", json!({"init": true, "ops": 2, "bytes": "ff".repeat(512)})),
            ("inputs", Inputs::default().json()),
            ("events", json!([[0, 1500, 6000, 0x743C, "I", 0xFFDE, 10], [1, 1600, 6100, 0x7480, "T", 0xD4A5, 0]])),
            ("checkpoint", json!({"state": "eJw=", "digest": "d"})),
        ] {
            v[k] = x;
        }
        v
    }

    fn cp(inst: u64) -> Checkpoint {
        Checkpoint { state: "s".into(), digest: "d".into(), inst }
    }

    const RUN: fn() -> Request = || run_req(Board::Strict, 100, None);

    #[test]
    fn strict_run_response() {
        let Ok(Outcome::Run(r)) = parse(&RUN(), XSI2, 0, &run_json()) else { panic!() };
        assert_eq!((r.executed, r.cycles, r.blocker.unwrap().address, r.milestones[1].2, r.checkpoint), (20, 71, Some(0x1040), None, None));
        // stale or mismatched answers are rejected
        assert!(parse(&run_req(Board::Strict, 99, None), XSI2, 0, &run_json()).is_err());
        assert!(parse(&run_req(Board::H1, 100, None), XSI2, 0, &run_json()).is_err());
        assert!(parse(&RUN(), &"0".repeat(64), 0, &run_json()).is_err());
        assert!(parse(&Request::Lookup(C), XSI2, 0, &run_json()).is_err());
        let bad_values = [
            ("executed", json!(101)),
            ("blocker", Value::Null), // without a blocker the whole budget must have run
            ("version", json!(2)),
            ("samples", json!([[0, 0, 0xB800], [20, 70, 0xB834]])), // last sample must match the final state
            ("samples", json!([[5, 0, 0xB800], [20, 71, 0xB834]])), // must start where the request started
            ("milestones", json!([[0xB800, "Reset", 21]])), // after the last executed instruction
            ("regions", json!([["a", 0, 0xFF, 0, 0, 0]])), // must cover the 64 KiB address space
            ("io", json!([[0x1040, "x", 0, 0, 1, 0]])), // outside the HC11 register block
            ("io", json!([[0x1008, "PORTD", 0, 1, 1, 256]])), // unknown-bit mask must fit a byte
            ("latches", json!([[0x1040, 6]])), // strict never accepts an external write
            ("events", json!([[0, 6, 23, 0xB80E, "Q", 0x103D, 1]])),
            ("pages", json!([[0xB8, 19]])), // must account for every executed instruction
            ("e_clock_hz", json!(8_000_000)),
            ("checkpoint", json!({"state": "s", "digest": "d"})), // nothing continues past a blocker
            ("inputs", Inputs::default().json()), // strict takes no inputs
            ("eeprom", json!({"init": false, "ops": 0, "bytes": ""})), // only H2 models the EEPROM
        ];
        for (k, bad) in bad_values {
            let mut v = run_json();
            v[k] = bad;
            assert!(parse(&RUN(), XSI2, 0, &v).is_err(), "{k}");
        }
        let mut v = run_json();
        v["blocker"]["pc"] = json!(0xB837); // blocked instruction must be the next PC
        assert!(parse(&RUN(), XSI2, 0, &v).is_err());
        v = json!({"version": 3, "ok": false, "error": "Unsupported dump"});
        assert_eq!(parse(&RUN(), XSI2, 0, &v), Err("backend: Unsupported dump".into()));
    }

    #[test]
    fn continued_h2_response() {
        let req = run_req(Board::H2, 1000, Some(cp(1000)));
        let Ok(Outcome::Run(r)) = parse(&req, XSI2, 0, &h2_json()) else { panic!("{:?}", parse(&req, XSI2, 0, &h2_json())) };
        assert_eq!((r.start, r.executed, r.rti, r.main_loop.sp, r.interrupts[1].name.as_str()), (1000, 2000, 4, Some((0x27FF, 0x27FF)), "TOF"));
        assert_eq!(r.checkpoint.as_ref().map(|c| c.inst), Some(2000));
        assert_eq!(r.eeprom.as_ref().map(|e| e.2.len()), Some(512));
        // the response must continue exactly the checkpoint and inputs that were sent
        assert!(parse(&run_req(Board::H2, 1000, Some(cp(999))), XSI2, 0, &h2_json()).is_err());
        assert!(parse(&run_req(Board::H2, 1000, None), XSI2, 0, &h2_json()).is_err());
        let other = Request::Run(RunReq { board: Board::H2, budget: 1000, inputs: Inputs { in1060: 0x20, ..Inputs::default() }, checkpoint: Some(cp(1000)) });
        assert!(parse(&other, XSI2, 0, &h2_json()).is_err());
        for (k, bad) in [
            ("rti", json!(5)),                     // more returns than entries
            ("checkpoint", Value::Null),           // a clean chunk must be continuable
            ("eeprom", Value::Null),               // H2 reports its EEPROM
            ("samples", json!([[0, 0, 0xB800], [2000, 8000, 0xD4A5]])), // must start at the checkpoint
            ("loop", json!({"pc": 0xD4A2, "count": 5, "sp_min": null, "sp_max": null, "gap_max": 0})),
            ("events", json!([[0, 1500, 6000, 0x743C, "I", 0xFFDE, 256]])),
        ] {
            let mut v = h2_json();
            v[k] = bad;
            assert!(parse(&req, XSI2, 0, &v).is_err(), "{k}");
        }
    }

    fn lookup_json() -> Value {
        json!({"version": 3, "ok": true, "action": "lookup", "sha256": XSI2, "working_sha256": XSI2, "edits": 0,
            "row": 7, "column": 3, "row_fraction": 192, "column_fraction": 64, "result": 147, "instructions": 38,
            "cells": [[0x91AB + 66, 148], [0x91AB + 67, 187], [0x91AB + 75, 132], [0x91AB + 76, 175]], "registers": regs_json(0xB218, 147)})
    }

    #[test]
    fn strict_lookup_response() {
        let Ok(Outcome::Lookup(l)) = parse(&Request::Lookup(C), XSI2, 0, &lookup_json()) else { panic!() };
        assert_eq!((l.result, l.cells[3]), (147, (0x91AB + 76, 175)));
        let swapped = Coord { row_frac: 64, col_frac: 192, ..C };
        assert!(parse(&Request::Lookup(swapped), XSI2, 0, &lookup_json()).is_err()); // fractions must echo exactly
        assert!(parse(&Request::Lookup(C), XSI2, 1, &lookup_json()).is_err()); // edit count and working SHA must agree
        for (k, bad) in [("result", json!(146)), ("cells", json!([[0, 1]])), ("registers", regs_json(0xB217, 147)), ("working_sha256", json!("x"))] {
            let mut v = lookup_json();
            v[k] = bad;
            assert!(parse(&Request::Lookup(C), XSI2, 0, &v).is_err(), "{k}");
        }
    }

    #[test]
    fn requests_validate_bounds_profile_inputs_and_edits() {
        let img = vec![0u8; 65536];
        let sim = Path::new("/x/ucsim");
        assert!(request_json(&RUN(), sim, &img, &"0".repeat(64), &[]).unwrap_err().starts_with("unsupported"));
        assert!(request_json(&run_req(Board::H1, MAX_RUN + 1, None), sim, &img, XSI2, &[]).is_err());
        let v: Value = serde_json::from_str(&request_json(&run_req(Board::H1, 7, None), sim, &img, XSI2, &[]).unwrap()).unwrap();
        assert_eq!((v["action"].clone(), v["profile"].clone(), v["budget"].clone(), v["version"].clone()), (json!("run"), json!("h1"), json!(7), json!(3)));
        assert_eq!((v["inputs"].clone(), v["checkpoint"].clone()), (Value::Null, Value::Null)); // H1 takes no inputs
        let v: Value = serde_json::from_str(&request_json(&run_req(Board::H2, 7, Some(cp(5))), sim, &img, XSI2, &[]).unwrap()).unwrap();
        assert_eq!((v["inputs"]["portd_in"].clone(), v["inputs"]["adc"][7].clone()), (json!(0x3F), json!(0x80)));
        assert_eq!(v["checkpoint"], json!({"state": "s", "digest": "d"}));
        let bad = Request::Run(RunReq { board: Board::H2, budget: 7, inputs: Inputs { portd_in: 64, ..Inputs::default() }, checkpoint: None });
        assert!(request_json(&bad, sim, &img, XSI2, &[]).is_err());
        assert!(request_json(&Request::Lookup(Coord { row: 23, ..C }), sim, &img, XSI2, &[]).is_err());
        assert!(request_json(&Request::Lookup(Coord { col: 8, ..C }), sim, &img, XSI2, &[]).is_err());
        assert!(request_json(&Request::Lookup(C), sim, &img, XSI2, &[(0x9283, 1)]).is_err());
        let v: Value = serde_json::from_str(&request_json(&Request::Lookup(C), sim, &img, XSI2, &[(0x9282, 1)]).unwrap()).unwrap();
        assert_eq!((v["row_fraction"].clone(), v["column_fraction"].clone(), v["edits"].clone()), (json!(192), json!(64), json!([[0x9282, 1]])));
        assert_eq!(v["image"].as_str().unwrap().len(), 131072);
        assert_eq!(profile(XSI2), Some("106XSI2.ORI (XSI2)"));
    }

    #[test]
    fn edits_limited_to_table_region() {
        let o = vec![0u8; 65536];
        let mut w = o.clone();
        w[0x91AB] = 1;
        w[0x9282] = 2;
        assert_eq!(table_edits(&o, &w), Ok(vec![(0x91AB, 1), (0x9282, 2)]));
        w[0x9283] = 3;
        assert!(table_edits(&o, &w).unwrap_err().contains("1 edited byte(s) outside"));
        assert_eq!(Coord { row: 22, col: 7, row_frac: 0, col_frac: 0 }.cells()[3], 0x9282);
    }

    fn backend() -> Option<(PathBuf, Vec<u8>)> {
        let sim = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/ucsim/src/sims/m68hc11.src/ucsim_m68hc11");
        let img = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../106XSI2.ORI")).ok()?;
        sim.is_file().then_some((sim, img))
    }

    fn go(sim: &Path, img: &[u8], req: Request) -> Run {
        match run(&req, sim, img, XSI2, &[], &NO) {
            Ok(Outcome::Run(r)) => *r,
            r => panic!("{r:?}"),
        }
    }

    /// End to end through python3 and the patched µCsim; skipped without `make emulator-setup` or the dump.
    #[test]
    fn real_backend_run_and_lookup() {
        let Some((sim, img)) = backend() else { return eprintln!("skipped: no simulator or dump") };
        // Strict: past INIT and the documented register setup, blocked before the unknown external write.
        let s = go(&sim, &img, run_req(Board::Strict, MAX_RUN, None));
        let b = s.blocker.clone().expect("blocker");
        assert_eq!((s.executed, s.regs.pc, b.address, b.value, b.kind.as_str(), s.checkpoint.is_none()), (20, 0xB834, Some(0x1040), Some(6), "write", true));
        let init = s.io.iter().find(|r| r.addr == 0x103D).expect("INIT written");
        assert_eq!((init.value, init.writes), (1, 1));
        // H1: copy loop, RAM test and EEPROM block check pass; blocked before reading board input $1060.
        let h = go(&sim, &img, run_req(Board::H1, MAX_RUN, None));
        let b = h.blocker.clone().expect("blocker");
        assert_eq!((h.executed, h.regs.pc, b.address, b.kind.as_str()), (68_202, 0xA86A, Some(0x1060), "read"));
        assert!(h.cop_services >= 4 && h.rom_same_writes == 4921, "{h:?}");
        assert_eq!(h.latches, vec![(0x1040, 6), (0x1050, 3)]);
        assert_eq!(go(&sim, &img, run_req(Board::H1, MAX_RUN, None)), h); // deterministic
        let reset = go(&sim, &img, run_req(Board::H1, 0, None));
        assert_eq!((reset.executed, reset.cycles, reset.regs.pc, reset.samples.len()), (0, 0, 0xB800, 1));
        let mut work = img.clone();
        let cell = C.cells()[0];
        work[cell] ^= 0xFF;
        let edits = table_edits(&img, &work).unwrap();
        let Ok(Outcome::Lookup(l)) = run(&Request::Lookup(C), &sim, &img, XSI2, &edits, &NO) else { panic!() };
        assert_eq!((l.cells[0], l.edits, l.regs.pc), ((cell as u16, work[cell]), 1, 0xB218));
        assert_ne!(l.working_sha, XSI2);
    }

    /// H2 session: chunks continue exactly; the foreground loop and timer interrupts run repeatedly.
    #[test]
    fn real_backend_h2_session() {
        let Some((sim, img)) = backend() else { return eprintln!("skipped: no simulator or dump") };
        let one = go(&sim, &img, run_req(Board::H2, 900_000, None));
        let a = go(&sim, &img, run_req(Board::H2, 333_333, None));
        let b = go(&sim, &img, run_req(Board::H2, 566_667, a.checkpoint.clone()));
        assert_eq!(one.checkpoint.as_ref().map(|c| &c.state), b.checkpoint.as_ref().map(|c| &c.state)); // same machine state
        assert_eq!((b.start, b.executed, b.cycles, b.main_loop.clone(), b.interrupts.clone()), (333_333, one.executed, one.cycles, one.main_loop.clone(), one.interrupts.clone()));
        assert!(one.blocker.is_none() && one.main_loop.count > 150, "{:?}", one.main_loop);
        assert_eq!(one.main_loop.sp, Some((0x27FF, 0x27FF)));
        let entries: u64 = one.interrupts.iter().map(|i| i.count).sum();
        assert!(one.interrupts.iter().any(|i| i.name == "TOF" && i.count >= 10) && entries - one.rti <= 1, "{:?}", one.interrupts);
        // A checkpoint of one input set is refused for another.
        let other = Request::Run(RunReq { board: Board::H2, budget: 10, inputs: Inputs { in1060: 0x20, ..Inputs::default() }, checkpoint: a.checkpoint.clone() });
        assert!(run(&other, &sim, &img, XSI2, &[], &NO).unwrap_err().contains("another source, profile or input set"));
        // Cancellation stops the backend at once.
        let cancel = AtomicBool::new(true);
        let t = Instant::now();
        assert_eq!(run(&run_req(Board::H2, MAX_RUN, None), &sim, &img, XSI2, &[], &cancel), Err("cancelled".into()));
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn missing_simulator_names_setup_command() {
        // SAFETY: tests touching UCSIM run in this one test only.
        unsafe { std::env::set_var("UCSIM", "/nonexistent/ucsim_m68hc11") };
        let e = simulator().unwrap_err();
        unsafe { std::env::remove_var("UCSIM") };
        assert!(e.contains("make -C ") && e.ends_with("emulator-setup"), "{e}");
    }
}
