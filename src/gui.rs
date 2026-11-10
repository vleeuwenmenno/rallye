//! Minimal GPUI dashboard. Dump files are only read; edits live in in-memory session working copies.

use crate::analysis::*;
use crate::edit::*;
use gpui::{
    App, Application, Bounds, ClickEvent, Context, Div, FocusHandle, FontWeight, KeyDownEvent, Hsla, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PathBuilder, PathPromptOptions, Pixels, Point, ScrollWheelEvent, SharedString, Stateful, TitlebarOptions, Window, WindowBounds,
    WindowOptions, canvas, div, hsla, point, prelude::*, px, rgb, size,
};
use std::collections::HashMap;
use std::path::PathBuf;

mod emulation;

const BG: u32 = 0x0f1115;
const PANEL: u32 = 0x161a21;
const PANEL2: u32 = 0x1f242e;
const BORDER: u32 = 0x2a303b;
const TEXT: u32 = 0xe6e9ef;
const MUTED: u32 = 0x8b93a1;
const ACCENT: u32 = 0x4fa3ff;
const WARN: u32 = 0xf2b84b;
const ERR: u32 = 0xff6b6b;
const DIFF: u32 = 0xff9f1c;

#[derive(Clone, Copy, PartialEq)]
enum Menu {
    Target,
    Baseline,
    Xdf,
}

struct Rallye {
    dir: PathBuf,
    inv: Result<Inventory, String>,
    xdf_ix: usize,
    xdf: Option<Result<Xdf, String>>,
    item: Option<usize>,
    target: usize,
    baseline: Option<usize>,
    mode: Mode,
    compare: bool,
    open: Option<Menu>,
    view: View,
    /// User's 2D/3D choice for the current table; None = automatic (both when the viewport has room).
    plot_pref: Option<PlotMode>,
    /// Explicit files added through the file chooser (canonical paths), re-added on Reload.
    imported: Vec<PathBuf>,
    /// Last chooser/import outcome, shown under the header.
    msg: Option<(String, u32)>,
    /// A native file dialog is open: no second one may start.
    picking: bool,
    /// Folder awaiting the discard-edits confirmation.
    pending_dir: Option<PathBuf>,
    /// Last pointer position while a left-button drag that started inside the 3D graph is active.
    drag: Option<Point<Pixels>>,
    /// Session working copies keyed by original dump path (created on first edit).
    work: HashMap<PathBuf, Working>,
    editor: Option<Editor>,
    confirm: Option<Confirm>,
    details: bool,
    force_close: bool,
    ed_focus: FocusHandle,
    /// Header tab: map inspector or the bounded firmware emulation view.
    emulation: bool,
    emu: emulation::Emu,
}

#[derive(Clone, Copy, PartialEq)]
enum Confirm {
    Reload,
    Close,
    Folder,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PlotMode {
    D2,
    D3,
    Both,
}

/// What a native file dialog is for.
#[derive(Clone, Copy, PartialEq)]
enum Browse {
    File(Menu),
    Folder,
}

/// Cell editor bound to the exact dump, file offset and interpretation it was opened on.
struct Editor {
    path: PathBuf,
    offset: usize,
    mode: Mode,
    cell: String,
    input: CellText,
    err: Option<String>,
}

/// Cell text in the chosen interpretation.
fn cell_text(b: u8, mode: Mode) -> String {
    match mode {
        Mode::Hex => format!("{b:02X}"),
        Mode::Unsigned => b.to_string(),
        Mode::Signed => as_signed(b).to_string(),
    }
}

/// Signed decimal delta text, or None when no valid baseline cell.
fn delta_text(t: u8, base: Option<u8>, mode: Mode) -> Option<String> {
    base.map(|b| format!("{:+}", cell_delta(t, b, mode == Mode::Signed)))
}

impl Rallye {
    fn new(dir: PathBuf, cx: &mut Context<Self>) -> Self {
        let dir = dir.canonicalize().unwrap_or(dir);
        let mut s = Self {
            dir, inv: Err(String::new()), xdf_ix: 0, xdf: None, item: None,
            target: 0, baseline: None, mode: Mode::Unsigned, compare: true, open: None,
            view: View::default(), plot_pref: None, imported: vec![], msg: None, picking: false, pending_dir: None, drag: None,
            work: HashMap::new(), editor: None, confirm: None, details: false, force_close: false, ed_focus: cx.focus_handle(),
            emulation: false, emu: emulation::Emu::default(),
        };
        s.reload();
        s
    }

    fn dirty_any(&self) -> bool {
        self.work.values().any(|w| w.dirty())
    }

    /// Reload only after an explicit discard when there are session edits.
    fn request_reload(&mut self) {
        if self.dirty_any() {
            self.confirm = Some(Confirm::Reload);
        } else {
            self.reload();
        }
    }

    fn step_history(&mut self, undo: bool) {
        self.editor = None;
        if let Some(w) = self.target_path().and_then(|p| self.work.get_mut(&p)) {
            if undo { w.undo() } else { w.redo() };
        }
    }

    fn target_path(&self) -> Option<PathBuf> {
        self.inv.as_ref().ok()?.dumps.get(self.target).map(|d| d.path.clone())
    }

    /// Working bytes when edited this session, else the original bytes.
    fn bytes_of<'a>(&'a self, d: &'a Dump) -> &'a [u8] {
        self.work.get(&d.path).map_or(&d.bytes, |w| w.bytes())
    }

    fn open_editor(&mut self, offset: usize, cell: String) {
        let (Some(path), Some(Ok(_))) = (self.target_path(), &self.xdf) else { return };
        let Some(d) = self.inv.as_ref().ok().and_then(|i| i.dumps.iter().find(|d| d.path == path)) else { return };
        let Some(&b) = self.bytes_of(d).get(offset) else { return };
        let mode = self.mode;
        self.editor = Some(Editor { path, offset, mode, cell, input: CellText::new(cell_text(b, mode)), err: None });
    }

    fn apply_edit(&mut self) {
        let Some(ed) = self.editor.as_mut() else { return };
        if ed.err.is_some() { return; }
        let v = match parse_byte(&ed.input.text, ed.mode) {
            Ok(v) => v,
            Err(e) => return ed.err = Some(e),
        };
        let Some(d) = self.inv.as_ref().ok().and_then(|i| i.dumps.iter().find(|d| d.path == ed.path)) else {
            return ed.err = Some("dump no longer loaded".into());
        };
        let w = self.work.entry(ed.path.clone()).or_insert_with(|| Working::new(d.bytes.clone()));
        match w.set(ed.offset, v) {
            Ok(_) => self.editor = None,
            Err(e) => ed.err = Some(e),
        }
    }

    fn editor_key(&mut self, e: &KeyDownEvent, cx: &mut Context<Self>) {
        let k = &e.keystroke;
        let Some(ed) = self.editor.as_mut() else { return };
        let ctrl = k.modifiers.control || k.modifiers.platform;
        let input = |ed: &mut Editor, s: &str| {
            ed.err = ed.input.insert(s).err();
        };
        match (k.key.as_str(), ctrl) {
            ("enter", _) => self.apply_edit(),
            ("escape", _) => self.editor = None,
            ("left", _) => { ed.input.left(); ed.err = None; }
            ("right", _) => { ed.input.right(); ed.err = None; }
            ("home", _) => { ed.input.home(); ed.err = None; }
            ("end", _) => { ed.input.end(); ed.err = None; }
            ("backspace", _) => { ed.input.delete(true); ed.err = None; }
            ("delete", _) => { ed.input.delete(false); ed.err = None; }
            ("a", true) => ed.input.select_all(),
            ("v", true) => {
                if let Some(t) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    input(ed, t.trim());
                }
            }
            (_, false) => {
                if let Some(c) = &k.key_char {
                    input(ed, c);
                }
            }
            _ => {}
        }
    }

    /// Rescan; keep target, baseline, XDF and table by path/uniqueid when they still exist.
    fn reload(&mut self) {
        self.emu.invalidate();
        let prev = self.inv.as_ref().ok().map(|i| {
            let path = |ix: Option<usize>| ix.and_then(|k| i.dumps.get(k)).map(|d| d.path.clone());
            (path(Some(self.target)), path(self.baseline), self.baseline.is_none(), i.xdfs.get(self.xdf_ix).cloned())
        });
        let keep_item = self.cur_uniqueid();
        // An unreadable directory still gives an (empty) inventory so the file choosers keep working.
        let mut inv = scan_dir(&self.dir).unwrap_or_else(|e| Inventory { dir: self.dir.clone(), errors: vec![e], ..Default::default() });
        // Explicit external files stay after Reload; a missing/invalid one is reported, not dropped silently.
        for p in std::mem::take(&mut self.imported) {
            let xdf = p.extension().is_some_and(|e| e.eq_ignore_ascii_case("xdf"));
            match if xdf { inv.import_xdf(&p) } else { inv.import_dump(&p) } {
                Ok((_, true)) => self.imported.push(p),
                Ok(_) => {} // now inside the folder, the scan lists it
                Err(e) => inv.errors.push(format!("external file dropped on reload: {e}")),
            }
        }
        self.inv = Ok(inv);
        (self.work, self.editor, self.plot_pref, self.msg) = (HashMap::new(), None, None, None);
        let (dumps, xdfs) = self.inv.as_ref().map(|i| (i.dumps.as_slice(), i.xdfs.as_slice())).unwrap_or((&[], &[]));
        let n = dumps.len();
        let find = |p: &Option<PathBuf>| p.as_ref().and_then(|p| dumps.iter().position(|d| &d.path == p));
        let (def_t, def_b) = (if n > 1 { 1 } else { 0 }, (n > 0).then_some(0));
        let (t, b, x) = match &prev {
            Some((pt, pb, none_b, px)) => (
                find(pt).unwrap_or(def_t),
                find(pb).or(if *none_b { None } else { def_b }),
                px.as_ref().and_then(|p| xdfs.iter().position(|q| q == p)).unwrap_or(0),
            ),
            None => (def_t, def_b, 0),
        };
        (self.target, self.baseline, self.xdf_ix, self.open) = (t, b, x, None);
        self.load_xdf(keep_item);
    }

    fn cur_uniqueid(&self) -> Option<String> {
        match (&self.xdf, self.item) {
            (Some(Ok(x)), Some(i)) => x.items.get(i).map(|it| it.uniqueid.to_string()),
            _ => None,
        }
    }

    fn load_xdf(&mut self, keep: Option<String>) {
        self.xdf = self.inv.as_ref().ok().and_then(|i| i.xdfs.get(self.xdf_ix)).map(|p| load_xdf(p));
        self.item = match &self.xdf {
            Some(Ok(x)) => keep
                .and_then(|u| x.items.iter().position(|it| it.uniqueid == u))
                .or_else(|| default_item(x)),
            _ => None,
        };
    }

    /// Native file/folder dialog. The result is applied on completion; cancel or error keeps all state.
    fn browse(&mut self, what: Browse, cx: &mut Context<Self>) {
        if self.picking {
            return;
        }
        self.picking = true;
        self.msg = None;
        let prompt = match what {
            Browse::Folder => "Open folder",
            Browse::File(Menu::Target) => "Add target dump",
            Browse::File(Menu::Baseline) => "Add baseline dump",
            Browse::File(Menu::Xdf) => "Add XDF",
        };
        let rx = cx.prompt_for_paths(PathPromptOptions { files: what != Browse::Folder, directories: what == Browse::Folder, multiple: false, prompt: Some(prompt.into()) });
        cx.spawn(async move |this, cx| {
            let r = match rx.await {
                Ok(Ok(p)) => Ok(p.and_then(|v| v.into_iter().next())),
                Ok(Err(e)) => Err(format!("file dialog failed: {e}")),
                Err(_) => Err("file dialog closed unexpectedly".to_string()),
            };
            this.update(cx, |this, cx| {
                this.picking = false;
                match (r, what) {
                    (Ok(None), _) => {} // cancelled
                    (Err(e), _) => this.msg = Some((e, ERR)),
                    (Ok(Some(p)), Browse::Folder) => this.open_folder(p, false),
                    (Ok(Some(p)), Browse::File(m)) => this.import(m, p),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Add one explicit file to the inventory and select it. Never touches working copies.
    fn import(&mut self, menu: Menu, path: PathBuf) {
        if let Err(e) = &self.inv {
            self.inv = Ok(Inventory { dir: self.dir.clone(), errors: vec![e.clone()], ..Default::default() });
        }
        let Ok(inv) = &mut self.inv else { return };
        let r = if menu == Menu::Xdf { inv.import_xdf(&path) } else { inv.import_dump(&path) };
        self.msg = Some(match r {
            Err(e) => (e, ERR),
            Ok((ix, new)) => {
                if new {
                    self.imported.push(canon(&path));
                }
                self.pick(menu, Some(ix));
                (format!("{} {}", if new { "Added" } else { "Already loaded, selected" }, path.display()), MUTED)
            }
        });
    }

    /// Switch folder only after it scans fine; session edits need the discard confirmation first.
    fn open_folder(&mut self, path: PathBuf, discard_ok: bool) {
        self.msg = None;
        if self.confirm.is_some() {
            self.msg = Some(("Folder choice ignored: another confirmation is open".into(), WARN));
            return;
        }
        let dir = canon(&path);
        match scan_dir(&dir) {
            Err(e) => self.msg = Some((e, ERR)),
            Ok(i) if i.dumps.is_empty() && i.xdfs.is_empty() => {
                self.msg = Some((format!("{}: no .bin/.rom/.ori dump or .xdf file ({} rejected); folder not opened", dir.display(), i.errors.len()), ERR));
            }
            Ok(_) if self.dirty_any() && !discard_ok => {
                (self.pending_dir, self.confirm) = (Some(dir), Some(Confirm::Folder));
            }
            Ok(inv) => {
                self.emu.invalidate();
                let dropped = self.imported.len();
                // Adopt the validated snapshot; a second scan could fail after edits were discarded.
                let n = inv.dumps.len();
                (self.dir, self.imported, self.inv) = (dir, vec![], Ok(inv));
                self.work.clear();
                self.editor = None;
                self.target = if n > 1 { 1 } else { 0 };
                self.baseline = (n > 0).then_some(0);
                self.xdf_ix = 0;
                self.open = None;
                self.plot_pref = None;
                self.load_xdf(None);
                self.msg = Some((
                    format!("Opened {}{}", self.dir.display(), if dropped > 0 { format!(" · {dropped} external file(s) from the previous folder are no longer listed") } else { String::new() }),
                    MUTED,
                ));
            }
        }
    }

    fn browse_btn(&self, cx: &mut Context<Self>, id: &'static str, label: &'static str, what: Browse) -> Stateful<Div> {
        let b = btn(id, label, false);
        if self.picking {
            return b.opacity(0.4);
        }
        b.on_click(cx.listener(move |this, _, _, cx| {
            this.browse(what, cx);
            cx.notify();
        }))
    }

    fn pick(&mut self, menu: Menu, i: Option<usize>) {
        self.editor = None;
        match (menu, i) {
            (Menu::Target, Some(i)) => {
                if self.target != i {
                    self.emu.invalidate();
                }
                self.target = i;
            }
            (Menu::Baseline, i) => self.baseline = i,
            (Menu::Xdf, Some(i)) => {
                let keep = self.cur_uniqueid();
                self.plot_pref = None;
                self.xdf_ix = i;
                self.load_xdf(keep);
            }
            _ => {}
        }
        self.open = None;
    }

    /// Labelled dropdown: wrapped selected value, compact detail, one open list at a time.
    fn selector(&self, cx: &mut Context<Self>, menu: Menu, title: &str, selected: String, detail: Option<String>, options: Vec<(Option<usize>, String, bool)>) -> Div {
        let browse = self.browse_btn(cx, match menu { Menu::Target => "browse-target", Menu::Baseline => "browse-base", Menu::Xdf => "browse-xdf" }, "Browse…", Browse::File(menu));
        let is_open = self.open == Some(menu);
        let id = match menu {
            Menu::Target => "target",
            Menu::Baseline => "base",
            Menu::Xdf => "xdf",
        };
        let head = div().flex().items_center().gap_2().child(div().flex_1().child(heading(title.to_string()))).child(browse);
        let mut d = div().flex().flex_col().gap_1().child(head).child(
            div()
                .id(SharedString::from(format!("{id}-sel")))
                .flex()
                .items_start()
                .gap_1()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(rgb(if is_open { ACCENT } else { BORDER }))
                .bg(rgb(PANEL2))
                .text_xs()
                .cursor_pointer()
                .child(div().flex_1().min_w_0().child(selected))
                .child(div().flex_none().text_color(rgb(MUTED)).child(if is_open { "▴" } else { "▾" }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open = if this.open == Some(menu) { None } else { Some(menu) };
                    cx.notify();
                })),
        );
        if let Some(t) = detail {
            d = d.child(div().text_size(px(10.)).text_color(rgb(MUTED)).font_family("JetBrainsMono Nerd Font").child(t));
        }
        if is_open {
            let mut list = div()
                .id(SharedString::from(format!("{id}-opts")))
                .max_h(px(180.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .rounded_md()
                .border_1()
                .border_color(rgb(BORDER));
            for (k, (ix, label, sel)) in options.into_iter().enumerate() {
                list = list.child(
                    div()
                        .id(SharedString::from(format!("{id}-opt{k}")))
                        .px_2()
                        .py_1()
                        .text_xs()
                        .cursor_pointer()
                        .bg(rgb(if sel { 0x1d3550 } else { PANEL }))
                        .hover(|s| s.bg(rgb(PANEL2)))
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.pick(menu, ix);
                            cx.notify();
                        })),
                );
            }
            d = d.child(list);
        }
        d
    }
}

/// File names for a list; a name that occurs more than once also shows its folder.
fn distinct_names<'a>(paths: impl Iterator<Item = &'a std::path::Path>) -> Vec<String> {
    let v: Vec<(String, String)> = paths
        .map(|p| (p.file_name().unwrap_or_default().to_string_lossy().into_owned(), p.parent().map(|d| d.display().to_string()).unwrap_or_default()))
        .collect();
    v.iter().map(|(n, d)| if v.iter().filter(|(m, _)| m == n).count() > 1 { format!("{n} — {d}") } else { n.clone() }).collect()
}

fn btn(id: impl Into<SharedString>, label: impl Into<SharedString>, active: bool) -> Stateful<Div> {
    div()
        .id(id.into())
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(rgb(if active { ACCENT } else { BORDER }))
        .bg(rgb(if active { 0x1d3550 } else { PANEL2 }))
        .text_color(rgb(TEXT))
        .text_xs()
        .cursor_pointer()
        .hover(|s| s.border_color(rgb(ACCENT)))
        .child(label.into())
}

fn card() -> Div {
    div().flex().flex_col().gap_1().p_3().rounded_lg().bg(rgb(PANEL)).border_1().border_color(rgb(BORDER))
}

fn heading(t: impl Into<SharedString>) -> Div {
    div().text_xs().text_color(rgb(MUTED)).font_weight(FontWeight::BOLD).child(t.into())
}

fn kv(k: &str, v: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .gap_2()
        .text_xs()
        .child(div().w(px(150.)).flex_none().text_color(rgb(MUTED)).child(k.to_string()))
        .child(div().flex_1().text_color(rgb(TEXT)).child(v.into()))
}

fn heat(t: f32) -> Hsla {
    hsla(0.62 * (1.0 - t), 0.65, 0.30, 1.0)
}

impl Render for Rallye {
    fn render(&mut self, w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = div()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_2()
            .bg(rgb(PANEL))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(div().text_color(rgb(TEXT)).font_weight(FontWeight::BOLD).child("Rallye"))
            .child(div().text_xs().text_color(rgb(MUTED)).child("IAW8P EPROM inspector · dump files are never written"))
            .children([("tab-maps", "Maps", false), ("tab-emu", "Emulation", true)].map(|(id, label, emu)| {
                btn(id, label, self.emulation == emu).on_click(cx.listener(move |this, _, _, cx| {
                    (this.emulation, this.editor) = (emu, None);
                    cx.notify();
                }))
            }))
            .child(div().flex_1().text_xs().text_color(rgb(MUTED)).truncate().child(self.dir.display().to_string()))
            .child(self.browse_btn(cx, "open-folder", "Open folder…", Browse::Folder))
            .child(btn("reload", "Reload", false).on_click(cx.listener(|this, _, _, cx| {
                this.request_reload();
                cx.notify();
            })));

        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .font_family("Liberation Sans")
            .child(header)
            .children(self.msg.as_ref().map(|(t, c)| div().px_4().py_1().text_xs().text_color(rgb(*c)).bg(rgb(PANEL2)).child(t.clone())))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.sidebar(cx))
                    // Emulation brings its own fixed toolbar, scrolling pane and status bar.
                    .child(if self.emulation {
                        self.emulation(w, cx).flex_1().min_w_0().into_any_element()
                    } else {
                        div().id("main").flex_1().min_w_0().overflow_y_scroll().p_4().child(self.main(w.viewport_size(), cx)).into_any_element()
                    }),
            );
        if let Some(c) = self.confirm {
            root = root.child(self.confirm_modal(c, cx));
        }
        root
    }
}

impl Rallye {
    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let col = div()
            .w(px(if self.emulation { 270. } else { 330. }))
            .flex_none()
            .flex()
            .flex_col()
            .bg(rgb(PANEL))
            .border_r_1()
            .border_color(rgb(BORDER));
        let empty = Inventory::default();
        let (inv, load_err) = match &self.inv {
            Ok(i) => (i, None),
            Err(e) => (&empty, Some(e)),
        };
        let msg = |t: String, c: u32| div().text_xs().text_color(rgb(c)).child(t);
        let dirty = |d: &Dump| self.work.get(&d.path).is_some_and(|w| w.dirty());
        let info = |d: &Dump| format!("{} B · source SHA {}…{}", d.bytes.len(), &d.sha256[..10], &d.sha256[54..]);
        let names = distinct_names(inv.dumps.iter().map(|d| d.path.as_path()));
        let name = |i: usize| if dirty(&inv.dumps[i]) { format!("{} ●", names[i]) } else { names[i].clone() };
        let tgt = inv.dumps.get(self.target);
        let base = self.baseline.and_then(|b| inv.dumps.get(b));
        let mut src = div().flex().flex_col().gap_2().p_3().border_b_1().border_color(rgb(BORDER)).flex_none();
        if let Some(e) = load_err.filter(|e| !e.is_empty()) {
            src = src.child(msg(e.clone(), ERR));
        }
        if inv.dumps.is_empty() {
            src = src.child(msg("No .bin/.rom/.ori dumps loaded. Use Browse… or Open folder….".into(), WARN));
        }
        let t_opts = (0..inv.dumps.len()).map(|i| (Some(i), name(i), i == self.target)).collect();
        let mut b_opts = vec![(None, "None (no comparison)".to_string(), self.baseline.is_none())];
        b_opts.extend((0..inv.dumps.len()).map(|i| (Some(i), name(i), self.baseline == Some(i))));
        src = src.child(self.selector(cx, Menu::Target, "TARGET DUMP · session working copy", Some(self.target).filter(|&t| t < inv.dumps.len()).map_or("—".into(), name), tgt.map(info), t_opts));
        for e in &inv.errors {
            src = src.child(msg(format!("rejected: {e}"), ERR));
        }
        // Emulation runs only the target; baseline, XDF and the table list belong to the Maps tab.
        if self.emulation {
            return col.child(src).child(div().id("emu-nav").flex_1().min_h_0().overflow_y_scroll().child(self.emu_nav(cx)));
        }
        src = src.child(self.selector(cx, Menu::Baseline, "BASELINE DUMP · original, unedited", self.baseline.filter(|&b| b < inv.dumps.len()).map_or("None".into(), name), base.map(info), b_opts));
        if inv.xdfs.is_empty() {
            src = src.child(msg("No .xdf loaded.".into(), WARN));
        }
        let xnames = distinct_names(inv.xdfs.iter().map(|p| p.as_path()));
        let opts = xnames.iter().enumerate().map(|(i, n)| (Some(i), n.clone(), i == self.xdf_ix)).collect();
        let cur = xnames.get(self.xdf_ix).cloned().unwrap_or("—".into());
        src = src.child(self.selector(cx, Menu::Xdf, "XDF DEFINITION", cur, None, opts));
        if let Some(Err(e)) = &self.xdf {
            src = src.child(msg(e.clone(), ERR));
        }
        let mut col = col.child(src);
        if let Some(Ok(x)) = &self.xdf {
            let flen = tgt.map(|d| self.bytes_of(d).len() as u64).unwrap_or(0);
            let mut list = div().id("items").flex_1().min_h_0().overflow_y_scroll().flex().flex_col();
            for (i, it) in x.items.iter().enumerate() {
                let sub = match (layout(it, x, flen), scalar_layout(it, x, flen)) {
                    (Ok(l), _) => (format!("0x{:X} · {}×{} · 8-bit", l.address, l.rows, l.cols), MUTED),
                    (Err(_), Ok(s)) => (format!("0x{:X} · single value · 16-bit · read-only", s.address), MUTED),
                    (Err(e), Err(se)) => (if it.kind == Kind::Constant { se } else { e }, WARN),
                };
                let sel = self.item == Some(i);
                list = list.child(
                    div()
                        .id(SharedString::from(format!("item{i}")))
                        .flex_none()
                        .px_3()
                        .py_1()
                        .cursor_pointer()
                        .border_l_2()
                        .border_color(rgb(if sel { ACCENT } else { PANEL }))
                        .bg(rgb(if sel { 0x1d3550 } else { PANEL }))
                        .hover(|s| s.bg(rgb(if sel { 0x1d3550 } else { PANEL2 })))
                        .child(div().text_xs().truncate().child(it.title.trim().to_string()))
                        .child(div().text_size(px(10.)).truncate().text_color(rgb(sub.1)).child(sub.0))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            (this.item, this.editor, this.plot_pref) = (Some(i), None, None);
                            cx.notify();
                        })),
                );
            }
            col = col
                .child(div().px_3().py_1().child(heading(format!("TABLES / SCALARS ({})", x.items.len()))))
                .child(list);
        }
        col
    }

    fn overlay(&self) -> Div {
        div().absolute().top_0().left_0().size_full().flex().items_center().justify_center().bg(hsla(0., 0., 0., 0.55)).occlude()
    }

    fn confirm_modal(&self, c: Confirm, cx: &mut Context<Self>) -> Div {
        let n = self.work.values().filter(|w| w.dirty()).count();
        let msg = match c {
            Confirm::Reload => format!("Reload discards session edits in {n} dump(s). They are not saved anywhere."),
            Confirm::Close => format!("Closing discards session edits in {n} dump(s). They are not saved anywhere."),
            Confirm::Folder => format!("Opening {} discards session edits in {n} dump(s). They are not saved anywhere.", self.pending_dir.as_ref().map_or(String::new(), |d| d.display().to_string())),
        };
        let discard = match c {
            Confirm::Reload => "Discard and reload",
            Confirm::Close => "Discard and close",
            Confirm::Folder => "Discard and open folder",
        };
        self.overlay().child(
            card()
                .w(px(420.))
                .gap_3()
                .child(div().text_color(rgb(WARN)).font_weight(FontWeight::BOLD).child("Unsaved session edits"))
                .child(div().text_sm().child(msg))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(btn("discard", discard, false).on_click(cx.listener(move |this, _, w, cx| {
                            this.confirm = None;
                            match c {
                                Confirm::Folder => {
                                    // Re-validates the folder; on failure the edits are kept and the error is shown.
                                    if let Some(d) = this.pending_dir.take() {
                                        this.open_folder(d, true);
                                    }
                                }
                                Confirm::Reload => {
                                    this.work.clear();
                                    this.reload();
                                }
                                Confirm::Close => {
                                    this.work.clear();
                                    this.force_close = true;
                                    w.remove_window();
                                }
                            }
                            cx.notify();
                        })))
                        .child(btn("keep", "Cancel", true).on_click(cx.listener(|this, _, _, cx| {
                            this.confirm = None;
                            this.pending_dir = None;
                            cx.notify();
                        }))),
                ),
        )
    }

    fn main(&self, viewport: gpui::Size<Pixels>, cx: &mut Context<Self>) -> Div {
        let root = div().flex().flex_col().gap_3();
        let (Ok(inv), Some(Ok(x)), Some(ix)) = (&self.inv, &self.xdf, self.item) else {
            return root.child(card().child(div().text_color(rgb(WARN)).child("Load a directory with dumps and a valid XDF to begin (Open folder… or Browse…).")));
        };
        let Some(dump) = inv.dumps.get(self.target) else {
            return root.child(card().child(div().text_color(rgb(WARN)).child("No dumps loaded.")));
        };
        let it = &x.items[ix];
        let tb = self.bytes_of(dump); // working bytes: all views, stats and comparisons use these
        let work = self.work.get(&dump.path);
        let base = self.baseline.and_then(|b| inv.dumps.get(b)); // baseline is always the original file
        let mode_name = match self.mode {
            Mode::Hex => "Hex",
            Mode::Unsigned => "Unsigned",
            Mode::Signed => "Signed",
        };
        let raw = |v: &[(String, String)]| v.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("  ");
        let mut details = card()
            .child(kv("uniqueid / kind", format!("{} / {:?}", it.uniqueid, it.kind)))
            .child(kv("EMBEDDEDDATA (raw)", raw(&it.embedded_attrs)))
            .child(kv("item flags (raw)", it.item_flags.clone()))
            .child(kv("units", it.units.clone()))
            .child(kv("equation (not executed)", it.equation.clone()))
            .child(kv("description (raw)", it.description.clone()));
        for (n, a) in [("x axis", &it.x), ("y axis", &it.y)] {
            if let Some(a) = a {
                details = details.child(kv(n, format!("{} · count {} · labels [{}]", a.units, a.indexcount.map_or("?".into(), |n| n.to_string()), a.labels.join(", "))));
            }
        }
        details = details
            .child(kv("Provenance", PROVENANCE))
            .child(kv("Target path", dump.path.display().to_string()))
            .child(kv("Target source SHA-256", format!("{} (original file, not the working copy)", dump.sha256)))
            .child(kv("XDF path", x.path.display().to_string()));
        if let Some(b) = base {
            details = details
                .child(kv("Baseline path", b.path.display().to_string()))
                .child(kv("Baseline source SHA-256", format!("{} (original)", b.sha256)));
        }

        let title = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(div().text_lg().font_weight(FontWeight::BOLD).child(it.title.trim().to_string()))
            .child(div().px_2().rounded_md().bg(rgb(0x3a2f12)).text_xs().text_color(rgb(WARN)).child(format!("{mode_name} view · signedness unverified")))
            .child(btn("details", if self.details { "Details ▾" } else { "Details ▸" }, self.details).on_click(cx.listener(|this, _, _, cx| {
                this.details = !this.details;
                cx.notify();
            })));
        let muted = |t: String| div().text_xs().text_color(rgb(MUTED)).child(t);

        let l = match layout(it, x, tb.len() as u64) {
            Ok(l) => l,
            Err(e) => {
                let sl = if it.kind == Kind::Constant { scalar_layout(it, x, tb.len() as u64) } else { Err(String::new()) };
                if let Ok(sl) = sl {
                    let root = root.child(self.scalar_card(it, x, &sl, dump, base, title, cx));
                    return if self.details { root.child(details) } else { root };
                }
                let e = match sl {
                    Err(se) if !se.is_empty() => format!("{e}; scalar view: {se}"),
                    _ => e,
                };
                let bytes = raw_scalar_bytes(it, x, tb)
                    .map(|b| b.iter().map(|v| format!("{v:02X}")).collect::<Vec<_>>().join(" "))
                    .unwrap_or_else(|e| e);
                let root = root.child(card().child(title).child(div().text_color(rgb(ERR)).child(format!("Cell view unavailable: {e}"))).child(kv("raw bytes at offset (no interpretation)", bytes)));
                return if self.details { root.child(details) } else { root };
            }
        };
        details = details
            .child(kv("Mapping hypothesis", hypothesis_text(&l, &x.base)))
            .child(kv("Bounds (hypothesis)", format!("0x{:X}..0x{:X} / 0x{:X}", l.offset, l.end(), tb.len())))
            .child(kv("Signedness", format!("UNRESOLVED (XDF claims {})", if l.xdf_claims_signed { "signed" } else { "unsigned" })));

        let c = cells(tb, &l);
        let oc = cells(&dump.bytes, &l);
        let st = stats(c).expect("layout guarantees cells");
        details = details
            .child(kv("Unsigned min / max", format!("{} / {}", st.min_u, st.max_u)))
            .child(kv("Signed min / max", format!("{} / {}", st.min_s, st.max_s)))
            .child(kv("Cells ≥ 0x80", st.high_bit.to_string()));
        let base_cells = base.and_then(|b| layout(it, x, b.bytes.len() as u64).ok().map(|bl| cells(&b.bytes, &bl)));
        let mut summary = format!("0x{:X} · {}×{} · 8-bit", l.offset, l.rows, l.cols);
        match (base, base_cells) {
            (Some(b), Some(bc)) => {
                let whole = diff(tb, &b.bytes);
                summary += &format!(" · vs baseline {} (original): {} of {} cells differ", b.name, diff(c, bc).changed, c.len());
                details = details.child(kv("File bytes changed vs baseline", format!("{} (+{} len diff)", whole.changed, whole.extra)));
            }
            (Some(_), None) => summary += " · baseline layout out of range",
            _ => summary += " · no baseline",
        }

        let cmp = self.compare && base_cells.is_some();
        let mut bar = self.mode_row(cx);
        bar = bar.child(btn("cmp", if self.compare { "Compare: on" } else { "Compare: off" }, self.compare).on_click(cx.listener(|this, _, _, cx| {
            this.compare = !this.compare;
            cx.notify();
        })));
        let hist = |id: &'static str, label: &'static str, on: bool| {
            let b = btn(id, label, false);
            if on { b } else { b.opacity(0.4) }
        };
        bar = bar
            .child(hist("undo", "Undo", work.is_some_and(|w| w.can_undo())).on_click(cx.listener(|this, _, _, cx| {
                this.step_history(true);
                cx.notify();
            })))
            .child(hist("redo", "Redo", work.is_some_and(|w| w.can_redo())).on_click(cx.listener(|this, _, _, cx| {
                this.step_history(false);
                cx.notify();
            })));
        let changed = work.map_or(0, |w| w.changed());
        bar = bar.child(if changed > 0 {
            div().text_xs().text_color(rgb(ACCENT)).child(format!("session edits · not saved · {changed} byte(s) changed"))
        } else {
            muted("no edits · double-click a cell to edit".into())
        });
        if cmp {
            bar = bar.child(muted("orange = differs from baseline · Δ target−baseline".into()));
        } else if self.compare {
            bar = bar.child(div().text_xs().text_color(rgb(WARN)).child(if base.is_none() { "Comparison unavailable: no baseline selected" } else { "Comparison unavailable: baseline layout out of range" }));
        }
        if changed > 0 {
            bar = bar.child(div().text_xs().text_color(rgb(ACCENT)).child("blue outline = edited"));
        }

        if let Some(ed) = &self.editor {
            let status = ed.err.clone().or_else(|| parse_byte(&ed.input.text, ed.mode).err());
            bar = bar.child(div().text_xs().text_color(rgb(if status.is_some() { ERR } else { ACCENT }))
                .child(format!("{} · {} · {}", ed.cell, ed.mode.range(), status.unwrap_or_else(|| "Enter applies · Esc or click elsewhere cancels".into()))));
        }

        // Heat follows the selected interpretation, normalised over this table.
        let val = |b: u8| num(b, self.mode);
        let (lo, hi) = c.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &b| (lo.min(val(b)), hi.max(val(b))));
        let label = |a: &Option<Axis>, i: u64| a.as_ref().and_then(|a| a.labels.get(i as usize).cloned()).unwrap_or_else(|| i.to_string());
        let (cell_w, cell_h) = if cmp { (px(72.), px(50.)) } else { (px(48.), px(22.)) };
        let mut head = div().flex().child(div().w(px(64.)).flex_none().text_xs().truncate().text_color(rgb(MUTED)).child(
            format!("{} \\ {}", it.y.as_ref().map(|a| a.units.as_str()).unwrap_or("row"), it.x.as_ref().map(|a| a.units.as_str()).unwrap_or("col")),
        ));
        for ci in 0..l.cols {
            head = head.child(div().w(cell_w).flex_none().text_xs().text_color(rgb(MUTED)).flex().justify_center().child(label(&it.x, ci)));
        }
        let mut grid = div().flex().flex_col().gap(px(2.)).font_family("JetBrainsMono Nerd Font").child(head);
        for (ri, row) in c.chunks(l.cols as usize).enumerate() {
            let mut r = div().flex().gap(px(2.)).child(div().w(px(62.)).flex_none().text_xs().text_color(rgb(MUTED)).child(label(&it.y, ri as u64)));
            for (ci, &b) in row.iter().enumerate() {
                let i = ri * l.cols as usize + ci;
                let bv = base_cells.map(|bc| bc[i]);
                let changed = bv.is_some_and(|v| v != b);
                let edited = oc[i] != b;
                let off = l.offset as usize + i;
                let active = self.editor.as_ref().filter(|ed| ed.path == dump.path && ed.offset == off && ed.mode == self.mode);
                let invalid = active.is_some_and(|ed| ed.err.is_some() || parse_byte(&ed.input.text, ed.mode).is_err());
                let t = if hi > lo { (val(b) - lo) / (hi - lo) } else { 0.5 };
                let mut cell = div()
                    .w(cell_w - px(2.))
                    .h(cell_h)
                    .flex_none()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .text_xs()
                    .bg(heat(t))
                    .border_2()
                    .border_color(if invalid { rgb(ERR).into() } else if active.is_some() || edited { rgb(ACCENT).into() } else if changed { rgb(DIFF).into() } else { heat(t) });
                if let Some(ed) = active {
                    cell = cell.child(div()
                        .id("inline-value")
                        .w_full().overflow_hidden()
                        .flex().items_center().justify_center()
                        .cursor(gpui::CursorStyle::IBeam)
                        .bg(rgb(if ed.input.selected_all { 0x1d3550 } else { PANEL2 }))
                        .text_color(rgb(TEXT))
                        .track_focus(&self.ed_focus)
                        .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                            this.editor_key(e, cx);
                            cx.stop_propagation();
                            cx.notify();
                        }))
                        .on_mouse_down_out(cx.listener(|this, e: &MouseDownEvent, _, cx| {
                            if e.button != MouseButton::Left { return; }
                            this.editor = None;
                            cx.notify();
                        }))
                        .child(div().flex().items_center().flex_none().h(px(18.))
                            .child(div().relative().flex_none().h(px(18.))
                                .child(ed.input.text[..ed.input.caret].to_string())
                                // Absolute caret adds no width, keeping the entire value centered.
                                .child(div().absolute().right(px(-1.)).top(px(2.))
                                    .w(px(1.)).h(px(14.)).bg(rgb(TEXT))))
                            .child(ed.input.text[ed.input.caret..].to_string())));

                } else {
                    cell = cell.child(cell_text(b, self.mode));
                }
                if let (true, Some(v), Some(d)) = (cmp, bv, delta_text(b, bv, self.mode)) {
                    cell = cell
                        .child(div().text_size(px(10.)).text_color(rgb(MUTED)).child(format!("B {}", cell_text(v, self.mode))))
                        .child(div().text_size(px(10.)).text_color(rgb(if changed { DIFF } else { MUTED })).child(format!("Δ {d}")));
                }
                let (off, desc) = (l.offset as usize + i, format!("row {} (#{ri}) · col {} (#{ci})", label(&it.y, ri as u64), label(&it.x, ci as u64)));
                r = r.child(cell.id(SharedString::from(format!("cell{i}"))).cursor_pointer().on_click(cx.listener(move |this, e: &ClickEvent, w, cx| {
                    if e.click_count() >= 2 {
                        this.open_editor(off, desc.clone());
                        w.focus(&this.ed_focus);
                        cx.notify();
                    }
                })));
            }
            grid = grid.child(r);
        }
        let (surf, line) = (plot_data(it, &l, c), line_data(it, &l, c));
        let mode = self.plot_pref.unwrap_or_else(|| auto_plot_mode(
            f32::from(viewport.width), f32::from(viewport.height), l.cols, cmp, surf.is_ok(), line.is_ok(),
        ));
        let toggle = self.plot_toggle(mode, cx);
        let plot = match (mode, surf, line) {
            (PlotMode::Both, Ok(p), Ok(ld)) => div().flex().flex_col().gap_3()
                .child(self.line_card(&ld, toggle))
                .child(self.surface_card(&p, div(), cx)),
            (PlotMode::D3, Ok(p), _) => self.surface_card(&p, toggle, cx),
            (PlotMode::D2, _, Ok(ld)) => self.line_card(&ld, toggle),
            (PlotMode::Both, Err(_), Ok(ld)) => self.line_card(&ld, toggle),
            (PlotMode::Both, Ok(p), Err(_)) => self.surface_card(&p, toggle, cx),
            (m, s, ln) => {
                let (name, e) = if m == PlotMode::D3 { ("3D surface", s.err()) } else { ("2D line chart", ln.err()) };
                card().w(px(CW + 24.)).child(div().flex().items_center().child(div().flex_1().child(heading(format!("TARGET TABLE {}", name.to_uppercase())))).child(toggle))
                    .child(div().text_xs().text_color(rgb(WARN)).child(format!("{name} unavailable: {}", e.unwrap_or_default())))
            }
        };
        let root = root.child(card().gap_1().child(title).child(muted(summary)));
        let root = if self.details { root.child(details) } else { root };
        root.child(card().gap_2().child(bar).child(div().flex().flex_wrap().gap_4().items_start().child(div().id("grid-scroll").max_w_full().overflow_x_scroll().child(grid)).child(plot)))
    }
}

/// Reserve a graph column beside the grid and enough height for both cards before auto-stacking.
fn auto_plot_mode(width: f32, height: f32, cols: u64, compare: bool, surface: bool, line: bool) -> PlotMode {
    let grid_width = 64. + cols as f32 * if compare { 72. } else { 48. };
    let needed_width = 330. + 32. + 24. + grid_width + 16. + CW + 24.;
    if surface && line && width >= needed_width && height >= 1100. {
        PlotMode::Both
    } else if surface {
        PlotMode::D3
    } else {
        PlotMode::D2
    }
}

/// Hex shows the raw word, unsigned u16 or signed i16 otherwise (viewer choice, not firmware semantics).
fn scalar_text(w: u16, mode: Mode) -> String {
    match mode {
        Mode::Hex => format!("0x{w:04X}"),
        Mode::Unsigned => w.to_string(),
        Mode::Signed => (w as i16).to_string(),
    }
}

fn rpm_text(r: &Result<f64, String>) -> String {
    match r {
        Ok(v) => format!("{v:.0} RPM"),
        Err(e) => format!("unavailable: {e}"),
    }
}

fn value_title(mode: Mode) -> &'static str {
    match mode {
        Mode::Hex => "Raw value (unsigned magnitude, hex mode)",
        Mode::Unsigned => "Raw value (unsigned u8)",
        Mode::Signed => "Raw value (signed i8)",
    }
}

impl Rallye {
    fn mode_row(&self, cx: &mut Context<Self>) -> Div {
        let mut bar = div().flex().flex_wrap().gap_2().items_center();
        for (m, label) in [(Mode::Hex, "Hex"), (Mode::Unsigned, "Unsigned"), (Mode::Signed, "Signed")] {
            bar = bar.child(btn(label, label, self.mode == m).on_click(cx.listener(move |this, _, _, cx| {
                (this.mode, this.editor) = (m, None);
                cx.notify();
            })));
        }
        bar
    }

    fn plot_toggle(&self, mode: PlotMode, cx: &mut Context<Self>) -> Div {
        let b = |id: &'static str, label: &'static str, m: PlotMode| {
            btn(id, label, mode == m).on_click(cx.listener(move |this, _, _, cx| {
                this.plot_pref = Some(m);
                cx.notify();
            }))
        };
        div().flex().flex_wrap().gap_1()
            .child(btn("plot-auto", "Auto", self.plot_pref.is_none()).on_click(cx.listener(|this, _, _, cx| {
                this.plot_pref = None;
                cx.notify();
            })))
            .child(b("plot-2d", "2D", PlotMode::D2))
            .child(b("plot-3d", "3D", PlotMode::D3))
            .child(b("plot-both", "Both", PlotMode::Both))
    }

    /// Read-only 16-bit scalar: one number, never a grid, line or surface.
    #[allow(clippy::too_many_arguments)]
    fn scalar_card(&self, it: &Item, x: &Xdf, sl: &ScalarLayout, dump: &Dump, base: Option<&Dump>, title: Div, cx: &mut Context<Self>) -> Div {
        let hexb = |r: [u8; 2]| format!("{:02X} {:02X}", r[0], r[1]);
        let endian = if sl.little_endian { "little-endian (XDF: LSB first)" } else { "big-endian (XDF: MSB first)" };
        let muted = |t: String| div().text_xs().text_color(rgb(MUTED)).child(t);
        let mut c = card()
            .gap_2()
            .child(title)
            .child(muted(format!("0x{:X} · single value · 16-bit · {endian} · read-only (16-bit editing is not supported; nothing is written)", sl.offset)))
            .child(self.mode_row(cx));
        let Some((tr, tw)) = scalar_read(sl, self.bytes_of(dump)) else {
            return c.child(div().text_color(rgb(ERR)).child("Target bytes are out of range"));
        };
        c = c.child(kv("Target (working copy)", format!("{} · bytes {} · {endian}", scalar_text(tw, self.mode), hexb(tr))));
        // Baseline is always the original, unedited file.
        let b = base.map(|b| scalar_layout(it, x, b.bytes.len() as u64).and_then(|bl| scalar_read(&bl, &b.bytes).ok_or_else(|| "out of range".to_string())));
        let bw = match &b {
            None => {
                c = c.child(muted("No baseline selected".into()));
                None
            }
            Some(Err(e)) => {
                c = c.child(div().text_xs().text_color(rgb(WARN)).child(format!("Baseline unavailable: {e}")));
                None
            }
            Some(Ok((br, bw))) => {
                c = c
                    .child(kv("Baseline (original)", format!("{} · bytes {}", scalar_text(*bw, self.mode), hexb(*br))))
                    .child(kv("Δ target − baseline", format!("{:+}", scalar_delta(tw, *bw, self.mode == Mode::Signed))));
                Some(*bw)
            }
        };
        let rt = scalar_rpm(it, sl, tw);
        c = c.child(kv("XDF-derived RPM (unverified)", format!("target {}", rpm_text(&rt))));
        if let (Ok(t), Some(bw)) = (&rt, bw) {
            let rb = scalar_rpm(it, sl, bw);
            c = c.child(kv("", format!("baseline {}", rpm_text(&rb))));
            if let Ok(b) = rb {
                c = c.child(kv("", format!("Δ {:+.0} RPM", t - b)));
            }
        }
        c.child(kv("XDF equation (not executed)", it.equation.clone()))
            .child(muted("Hex/Unsigned/Signed (i16) only change how the word is shown. The RPM line applies only the exact documented 15000000/X pattern; it is an XDF claim, not firmware proof.".into()))
    }

    /// Native 2D line chart of the target working bytes.
    fn line_card(&self, ld: &LineData, toggle: Div) -> Div {
        let ch = chart(ld, self.mode);
        let (pl, pr, pt, pb) = (46., 14., 14., 44.);
        let (pw, ph) = (CW - pl - pr, LH - pt - pb);
        let at = move |p: (f32, f32)| (pl + p.0 * pw, pt + (1. - p.1) * ph);
        let (lo, hi) = (ch.lo as f64, ch.hi as f64);
        let (x0, x1) = (ld.x[0], *ld.x.last().unwrap());
        let mut segs: Vec<([f32; 4], u32)> = Vec::new();
        let mut hseg = |y: f32, c: u32| segs.push(([pl, pt + y * ph, pl + pw, pt + y * ph], c));
        for i in 0..=4 {
            hseg(i as f32 / 4., BORDER);
        }
        if lo < 0. && hi > 0. {
            hseg(1. - ((0. - lo) / (hi - lo)) as f32, MUTED); // sign boundary
        }
        for i in 0..=4 {
            let x = pl + pw * i as f32 / 4.;
            segs.push(([x, pt, x, pt + ph], BORDER));
        }
        let n = ch.lines.len();
        let paths: Vec<(Vec<(f32, f32)>, Hsla)> = ch.lines.iter().enumerate().map(|(i, l)| (l.iter().map(|&p| at(p)).collect(), series_color(i, n))).collect();
        let cv = canvas(
            |_, _, _| (),
            move |b, _, window, _| {
                let o = b.origin;
                let pt = |q: (f32, f32)| point(o.x + px(q.0), o.y + px(q.1));
                for ([ax, ay, bx, by], c) in &segs {
                    let mut pb = PathBuilder::stroke(px(1.));
                    pb.move_to(pt((*ax, *ay)));
                    pb.line_to(pt((*bx, *by)));
                    if let Ok(path) = pb.build() {
                        window.paint_path(path, rgb(*c));
                    }
                }
                for (pts, color) in &paths {
                    let mut pb = PathBuilder::stroke(px(1.5));
                    for (k, q) in pts.iter().enumerate() {
                        if k == 0 { pb.move_to(pt(*q)) } else { pb.line_to(pt(*q)) }
                    }
                    if let Ok(path) = pb.build() {
                        window.paint_path(path, *color);
                    }
                    for q in pts {
                        let mut f = PathBuilder::fill();
                        f.move_to(pt((q.0 - 2., q.1 - 2.)));
                        f.line_to(pt((q.0 + 2., q.1 - 2.)));
                        f.line_to(pt((q.0 + 2., q.1 + 2.)));
                        f.line_to(pt((q.0 - 2., q.1 + 2.)));
                        f.close();
                        if let Ok(path) = f.build() {
                            window.paint_path(path, *color);
                        }
                    }
                }
            },
        )
        .size_full();
        let lbl = |x: f32, y: f32, w: f32, justify_end: bool, t: String| {
            let d = div().absolute().left(px(x)).top(px(y)).w(px(w)).flex().text_size(px(10.)).text_color(rgb(MUTED));
            (if justify_end { d.justify_end() } else { d.justify_center() }).child(t)
        };
        let mut area = div().relative().w(px(CW)).h(px(LH)).overflow_hidden().rounded_md().bg(rgb(BG)).child(cv);
        for i in 0..=4 {
            let f = i as f64 / 4.;
            area = area
                .child(lbl(0., pt + (1. - f as f32) * ph - 7., pl - 6., true, tick_text(lo + (hi - lo) * f, hi - lo)))
                .child(lbl(pl + pw * f as f32 - 30., pt + ph + 4., 60., false, tick_text(x0 + (x1 - x0) * f, x1 - x0)));
        }
        area = area.child(div().absolute().left(px(pl)).top(px(pt + ph + 22.)).w(px(pw)).flex().justify_center().text_size(px(10.)).text_color(rgb(TEXT)).child(ld.x_title.clone()));
        let mut legend = div().flex().flex_wrap().gap_x_3().text_xs().text_color(rgb(MUTED));
        if !ld.names.is_empty() {
            legend = legend.child(format!("one line per column, {}:", ld.name_title));
            for (i, nm) in ld.names.iter().enumerate() {
                legend = legend.child(div().flex().items_center().gap_1().child(div().w(px(10.)).h(px(3.)).bg(series_color(i, n))).child(nm.clone()));
            }
        }
        let mut c = card()
            .w(px(CW + 24.))
            .gap_2()
            .child(div().flex().items_center().gap_2().child(div().flex_1().child(heading(format!("TARGET TABLE 2D · {} × RAW VALUE", ld.x_title.to_uppercase())))).child(toggle));
        if let Some(why) = &ld.index_fallback {
            c = c.child(div().text_xs().text_color(rgb(WARN)).child(format!("Axis labels unusable ({why}): the horizontal axis is the sample index, not the XDF axis.")));
        }
        c.child(area)
            .child(legend)
            .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                "Vertical: {}. Horizontal: {}, spaced in proportion to the XDF axis values (an XDF claim) unless marked as sample index.",
                value_title(self.mode), ld.x_title
            )))
            .child(div().text_xs().text_color(rgb(WARN)).child("Lines only join samples visually; ECU interpolation is not verified. The shape does not prove signedness."))
    }
}

const LH: f32 = 300.;

fn series_color(i: usize, n: usize) -> Hsla {
    if n == 1 { rgb(ACCENT).into() } else { hsla(0.85 * i as f32 / n as f32, 0.75, 0.6, 1.) }
}

/// Normalised 2D chart points (x and y in 0..1, y up) for the selected interpretation.
struct Chart {
    lo: f32,
    hi: f32,
    lines: Vec<Vec<(f32, f32)>>,
}

fn chart(ld: &LineData, mode: Mode) -> Chart {
    let (mut lo, mut hi) = ld.lines.iter().flatten().fold((f32::MAX, f32::MIN), |(l, h), &b| (l.min(num(b, mode)), h.max(num(b, mode))));
    if hi <= lo {
        (lo, hi) = (lo - 1., hi + 1.); // constant data: flat line mid-height
    }
    let (x0, x1) = (ld.x[0], *ld.x.last().unwrap());
    let lines = ld.lines.iter().map(|l| l.iter().enumerate().map(|(i, &b)| (((ld.x[i] - x0) / (x1 - x0)) as f32, (num(b, mode) - lo) / (hi - lo))).collect()).collect();
    Chart { lo, hi, lines }
}

const CW: f32 = 480.;
const CH: f32 = 400.;
const ZS: f32 = 0.7; // half-height of the value axis in normalised world units (X/Y span -1..1)
const PITCH_MIN: f32 = 0.08;
const PITCH_MAX: f32 = 1.48;

/// Camera for the 3D surface: yaw around the vertical value axis, pitch above the floor, zoom.
#[derive(Clone, Copy, Debug, PartialEq)]
struct View {
    yaw: f32,
    pitch: f32,
    zoom: f32,
}

impl Default for View {
    fn default() -> Self {
        Self { yaw: 0.45, pitch: 0.55, zoom: 1.0 } // lowest RPM and load nearest the viewer
    }
}

impl View {
    fn rotated(self, dx: f32, dy: f32) -> Self {
        Self { yaw: (self.yaw + dx * 0.01).rem_euclid(std::f32::consts::TAU), pitch: (self.pitch + dy * 0.01).clamp(PITCH_MIN, PITCH_MAX), ..self }
    }
    fn zoomed(self, steps: f32) -> Self {
        Self { zoom: (self.zoom * 1.15f32.powf(steps)).clamp(0.5, 4.0), ..self }
    }
}

/// Orthographic projection of a normalised world point (x = load, y = RPM, z = value up) to
/// canvas pixels. Returns (screen x, screen y, depth); larger depth is farther from the viewer.
fn project(v: View, p: [f32; 3]) -> (f32, f32, f32) {
    let (s, c) = v.yaw.sin_cos();
    let (x1, y1) = (p[0] * c - p[1] * s, p[0] * s + p[1] * c);
    let (sp, cp) = v.pitch.sin_cos();
    let k = CW.min(CH) * 0.32 * v.zoom;
    (CW / 2. + x1 * k, CH / 2. - (y1 * sp + p[2] * cp) * k, y1 * cp - p[2] * sp)
}

/// Raw value in the display interpretation; hex plots unsigned magnitude.
fn num(b: u8, mode: Mode) -> f32 {
    if mode == Mode::Signed { as_signed(b) as f32 } else { b as f32 }
}

/// Table mapped to normalised world points: load and RPM proportional to their label values.
struct Surface {
    rows: usize,
    cols: usize,
    pts: Vec<[f32; 3]>,
    /// Colour position 0..1 per cell (low..high value).
    t: Vec<f32>,
    lo: f32,
    hi: f32,
}

fn unit(v: f64, a: f64, b: f64) -> f32 {
    ((v - a) / (b - a)) as f32 * 2. - 1.
}

fn surface(p: &PlotData, mode: Mode) -> Surface {
    let vals: Vec<f32> = p.cells.iter().map(|&b| num(b, mode)).collect();
    let (mut lo, mut hi) = vals.iter().fold((f32::MAX, f32::MIN), |(l, h), &v| (l.min(v), h.max(v)));
    if hi <= lo {
        (lo, hi) = (lo - 1., hi + 1.); // constant table: flat surface mid-height
    }
    let (cols, rows) = (p.load.len(), p.rpm.len());
    let (l0, l1, r0, r1) = (p.load[0], p.load[cols - 1], p.rpm[0], p.rpm[rows - 1]);
    let t: Vec<f32> = vals.iter().map(|v| (v - lo) / (hi - lo)).collect();
    let pts = (0..rows * cols).map(|i| [unit(p.load[i % cols], l0, l1), unit(p.rpm[i / cols], r0, r1), (t[i] * 2. - 1.) * ZS]).collect();
    Surface { rows, cols, pts, t, lo, hi }
}

impl Surface {
    fn z(&self, v: f32) -> f32 {
        ((v - self.lo) / (self.hi - self.lo) * 2. - 1.) * ZS
    }
}

/// One projected grid cell: corners in screen space, painted far to near.
struct Patch {
    quad: [(f32, f32); 4],
    depth: f32,
    color: Hsla,
}

/// Depth-sorted shaded patches (painter's algorithm), farthest first.
fn patches(s: &Surface, v: View) -> Vec<Patch> {
    let light = [-0.35f32, -0.45, 0.82];
    let mut out = Vec::with_capacity((s.rows - 1) * (s.cols - 1));
    for r in 0..s.rows - 1 {
        for c in 0..s.cols - 1 {
            let ix = [r * s.cols + c, r * s.cols + c + 1, (r + 1) * s.cols + c + 1, (r + 1) * s.cols + c];
            let [a, b, cc, d] = ix.map(|i| s.pts[i]);
            let (d1, d2) = ([cc[0] - a[0], cc[1] - a[1], cc[2] - a[2]], [d[0] - b[0], d[1] - b[1], d[2] - b[2]]);
            let n = [d1[1] * d2[2] - d1[2] * d2[1], d1[2] * d2[0] - d1[0] * d2[2], d1[0] * d2[1] - d1[1] * d2[0]];
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-6);
            let shade = ((n[0] * light[0] + n[1] * light[1] + n[2] * light[2]) / len).abs();
            let t = ix.iter().map(|&i| s.t[i]).sum::<f32>() / 4.;
            let pr = ix.map(|i| project(v, s.pts[i]));
            out.push(Patch {
                quad: pr.map(|p| (p.0, p.1)),
                depth: pr.iter().map(|p| p.2).sum::<f32>() / 4.,
                color: hsla(0.62 * (1. - t), 0.7, 0.2 + 0.32 * shade, 1.),
            });
        }
    }
    out.sort_by(|a, b| b.depth.total_cmp(&a.depth));
    out
}

/// Evenly spaced tick values across lo..hi (inclusive).
fn ticks(lo: f64, hi: f64, n: usize) -> Vec<f64> {
    (0..=n).map(|i| lo + (hi - lo) * i as f64 / n as f64).collect()
}

fn tick_text(v: f64, span: f64) -> String {
    if span < 5. { format!("{v:.1}") } else { format!("{v:.0}") }
}

impl Rallye {
    /// Interactive 3D RPM x load x raw-value surface of the target table.
    fn surface_card(&self, p: &PlotData, toggle: Div, cx: &mut Context<Self>) -> Div {
        let s = surface(p, self.mode);
        let v = self.view;
        let floor = -ZS;
        let pj = |q: [f32; 3]| project(v, q);
        let seg = |a: [f32; 3], b: [f32; 3]| {
            let (p, q) = (pj(a), pj(b));
            [p.0, p.1, q.0, q.1]
        };
        // Edges nearest the viewer carry the tick labels; the leftmost floor corner carries the value axis.
        let near_x = if pj([-1., 0., floor]).2 < pj([1., 0., floor]).2 { -1. } else { 1. };
        let near_y = if pj([0., -1., floor]).2 < pj([0., 1., floor]).2 { -1. } else { 1. };
        // Value axis on the far corner, clear of both tick-labelled edges.
        let corner = [-near_x, -near_y];
        let (rpm_lo, rpm_hi) = (p.rpm[0], *p.rpm.last().unwrap());
        let (ld_lo, ld_hi) = (p.load[0], *p.load.last().unwrap());
        let (lo, hi) = (s.lo as f64, s.hi as f64);
        let mut z_ticks = ticks(lo, hi, 4);
        if lo < 0. && hi > 0. {
            z_ticks.push(0.); // sign boundary always labelled
        }
        let mut lines: Vec<([f32; 4], u32)> = Vec::new(); // screen segments drawn under the surface
        for r in ticks(rpm_lo, rpm_hi, 4) {
            let y = unit(r, rpm_lo, rpm_hi);
            lines.push((seg([-1., y, floor], [1., y, floor]), BORDER));
        }
        for l in ticks(ld_lo, ld_hi, 4) {
            let x = unit(l, ld_lo, ld_hi);
            lines.push((seg([x, -1., floor], [x, 1., floor]), BORDER));
        }
        if lo < 0. && hi > 0. {
            let z0 = s.z(0.);
            for (a, b) in [([-1., -1.], [1., -1.]), ([1., -1.], [1., 1.]), ([1., 1.], [-1., 1.]), ([-1., 1.], [-1., -1.])] {
                lines.push((seg([a[0], a[1], z0], [b[0], b[1], z0]), MUTED));
            }
        }
        lines.push((seg([corner[0], corner[1], floor], [corner[0], corner[1], ZS]), MUTED));
        for t in &z_ticks {
            let z = s.z(*t as f32);
            lines.push((seg([corner[0], corner[1], z], [corner[0] * 1.08, corner[1] * 1.08, z]), MUTED));
        }
        let pats = patches(&s, v);
        let cv = canvas(
            |_, _, _| (),
            move |b, _, window, _| {
                let o = b.origin;
                let pt = |q: (f32, f32)| point(o.x + px(q.0), o.y + px(q.1));
                for ([ax, ay, bx, by], c) in &lines {
                    let mut pb = PathBuilder::stroke(px(1.));
                    pb.move_to(pt((*ax, *ay)));
                    pb.line_to(pt((*bx, *by)));
                    if let Ok(path) = pb.build() {
                        window.paint_path(path, rgb(*c));
                    }
                }
                for pa in &pats {
                    let mut f = PathBuilder::fill();
                    let mut m = PathBuilder::stroke(px(1.));
                    for (k, q) in pa.quad.iter().enumerate() {
                        if k == 0 { f.move_to(pt(*q)); m.move_to(pt(*q)) } else { f.line_to(pt(*q)); m.line_to(pt(*q)) }
                    }
                    f.close();
                    m.close();
                    if let Ok(path) = f.build() {
                        window.paint_path(path, pa.color);
                    }
                    if let Ok(path) = m.build() {
                        window.paint_path(path, hsla(0., 0., 0.04, 0.55));
                    }
                }
            },
        )
        .size_full();

        // Labels are pushed away from the floor centre, perpendicular to their own edge.
        let center = pj([0., 0., floor]);
        let label = |q: (f32, f32, f32), dir: (f32, f32, f32), push: f32, text: String, color: u32| {
            let (dx, dy) = (dir.0 - center.0, dir.1 - center.1);
            let d = (dx * dx + dy * dy).sqrt().max(1.);
            let (x, y) = (q.0 + dx / d * push, q.1 + dy / d * push);
            div().absolute().left(px(x - 60.)).top(px(y - 7.)).w(px(120.)).flex().justify_center().text_size(px(10.)).text_color(rgb(color)).child(text)
        };
        let units = if p.load_units.is_empty() { "XDF X axis (no units given)".to_string() } else { p.load_units.clone() };
        let z_title = value_title(self.mode);
        let mut area = div()
            .id("surface3d")
            .relative()
            .w(px(CW))
            .h(px(CH))
            .overflow_hidden()
            .rounded_md()
            .bg(rgb(BG))
            .cursor(if self.drag.is_some() { gpui::CursorStyle::ClosedHand } else { gpui::CursorStyle::OpenHand })
            .child(cv)
            .on_mouse_down(MouseButton::Left, cx.listener(|this, e: &MouseDownEvent, _, cx| {
                this.drag = Some(e.position);
                cx.notify();
            }))
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                let Some(last) = this.drag else { return };
                if e.pressed_button != Some(MouseButton::Left) {
                    this.drag = None; // released somewhere we did not see
                } else {
                    let d = e.position - last;
                    this.view = this.view.rotated(f32::from(d.x), f32::from(d.y));
                    this.drag = Some(e.position);
                }
                cx.notify();
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _: &MouseUpEvent, _, cx| {
                this.drag = None;
                cx.notify();
            }))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _: &MouseUpEvent, _, cx| {
                this.drag = None;
                cx.notify();
            }))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !hovered && this.drag.take().is_some() {
                    cx.notify(); // leaving the graph ends the drag
                }
            }))
            .on_scroll_wheel(cx.listener(|this, e: &ScrollWheelEvent, _, cx| {
                this.view = this.view.zoomed(f32::from(e.delta.pixel_delta(px(20.)).y) / 20.);
                cx.stop_propagation(); // wheel zooms here instead of scrolling the page
                cx.notify();
            }));
        let (rpm_dir, ld_dir) = (pj([near_x, 0., floor]), pj([0., near_y, floor]));
        for r in ticks(rpm_lo, rpm_hi, 4) {
            area = area.child(label(pj([near_x, unit(r, rpm_lo, rpm_hi), floor]), rpm_dir, 14., tick_text(r, rpm_hi - rpm_lo), MUTED));
        }
        for l in ticks(ld_lo, ld_hi, 4) {
            area = area.child(label(pj([unit(l, ld_lo, ld_hi), near_y, floor]), ld_dir, 14., tick_text(l, ld_hi - ld_lo), MUTED));
        }
        area = area
            .child(label(rpm_dir, rpm_dir, 50., "RPM".into(), TEXT))
            .child(label(ld_dir, ld_dir, 50., units.clone(), TEXT));
        for t in &z_ticks {
            let q = pj([corner[0], corner[1], s.z(*t as f32)]);
            area = area.child(
                div().absolute().left(px(q.0 + 12.)).top(px(q.1 - 7.)).px_1().rounded_sm().bg(hsla(0., 0., 0.06, 0.8)).text_size(px(10.))
                    .text_color(rgb(if *t == 0. && lo < 0. { TEXT } else { MUTED })).child(tick_text(*t, hi - lo)),
            );
        }
        let top = pj([corner[0], corner[1], ZS]);
        area = area.child(div().absolute().left(px(top.0 - 100.)).top(px(top.1 - 24.)).w(px(200.)).flex().justify_center().text_size(px(10.)).text_color(rgb(TEXT)).child(z_title));

        let mut scale = div().flex().h(px(8.)).w(px(200.)).rounded_sm().overflow_hidden();
        for i in 0..32 {
            scale = scale.child(div().flex_1().h_full().bg(hsla(0.62 * (1. - i as f32 / 31.), 0.7, 0.4, 1.)));
        }
        let legend = div()
            .flex()
            .items_center()
            .gap_2()
            .text_xs()
            .text_color(rgb(MUTED))
            .child(format!("low {}", tick_text(lo, hi - lo)))
            .child(scale)
            .child(format!("{} high", tick_text(hi, hi - lo)));
        card()
            .w(px(CW + 24.))
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().child(heading("TARGET TABLE 3D · RPM × LOAD × RAW VALUE")))
                    .child(toggle)
                    .child(btn("reset-view", "Reset view", false).on_click(cx.listener(|this, _, _, cx| {
                        this.view = View::default();
                        cx.notify();
                    }))),
            )
            .child(div().text_xs().text_color(rgb(MUTED)).child("Drag inside the graph to rotate · mouse wheel to zoom"))
            .child(area)
            .child(legend)
            .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                "RPM: XDF Y-axis labels · {units}: XDF X-axis labels · both are XDF claims, spaced in proportion to their values. Height: {z_title}."
            )))
            .child(div().text_xs().text_color(rgb(WARN)).child(
                "Mesh and shading only join samples visually; ECU interpolation is not verified. The 3D shape does not prove signedness.",
            ))
    }
}

pub fn run(dir: PathBuf) {
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1400.), px(920.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("Rallye".into()), ..Default::default() }),
                app_id: Some("rallye".into()),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| Rallye::new(dir, cx));
                let v = view.clone();
                // Block a normal close while any working copy differs from its original; the in-app dialog decides.
                window.on_window_should_close(cx, move |_, cx| {
                    v.update(cx, |this, cx| {
                        let block = !this.force_close && this.dirty_any();
                        if block {
                            this.confirm = Some(Confirm::Close);
                            cx.notify();
                        }
                        !block
                    })
                });
                view
            },
        )
        .expect("open window");
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_plot_layout_respects_space_and_available_data() {
        assert_eq!(auto_plot_mode(1800., 1200., 9, true, true, true), PlotMode::Both);
        assert_eq!(auto_plot_mode(1800., 700., 9, true, true, true), PlotMode::D3);
        assert_eq!(auto_plot_mode(1400., 1200., 9, true, true, true), PlotMode::D3);
        assert_eq!(auto_plot_mode(1800., 1200., 17, true, true, true), PlotMode::D3);
        assert_eq!(auto_plot_mode(1800., 1200., 1, true, false, true), PlotMode::D2);
        assert_eq!(auto_plot_mode(1800., 1200., 9, true, true, false), PlotMode::D3);
    }

    #[test]
    fn delta_modes_and_missing_baseline() {
        assert_eq!(delta_text(0x7F, Some(0x80), Mode::Signed).as_deref(), Some("+255"));
        assert_eq!(delta_text(0x7F, Some(0x80), Mode::Unsigned).as_deref(), Some("-1"));
        assert_eq!(delta_text(0xFF, Some(0), Mode::Hex).as_deref(), Some("+255")); // hex delta is u8-based decimal
        assert_eq!(delta_text(7, Some(7), Mode::Unsigned).as_deref(), Some("+0"));
        assert_eq!(delta_text(7, None, Mode::Unsigned), None);
    }

    #[test]
    fn cell_text_modes() {
        assert_eq!(cell_text(0xFF, Mode::Hex), "FF");
        assert_eq!(cell_text(0x80, Mode::Signed), "-128");
        assert_eq!(cell_text(0, Mode::Unsigned), "0");
    }

    fn pd(cells: Vec<u8>) -> PlotData {
        PlotData { rpm: vec![800., 1000., 6000.], load: vec![11., 22., 100.], load_units: "Load index".into(), cells }
    }

    #[test]
    fn scalar_text_modes() {
        assert_eq!(scalar_text(0x0788, Mode::Hex), "0x0788");
        assert_eq!(scalar_text(0x8000, Mode::Unsigned), "32768");
        assert_eq!(scalar_text(0x8000, Mode::Signed), "-32768");
        assert_eq!(scalar_text(0xFFFF, Mode::Signed), "-1");
        assert_eq!(rpm_text(&Ok(7780.08)), "7780 RPM");
        assert!(rpm_text(&Err("x".into())).starts_with("unavailable"));
    }

    #[test]
    fn duplicate_names_show_folder() {
        use std::path::Path;
        let v = distinct_names([Path::new("/a/x.BIN"), Path::new("/b/x.BIN"), Path::new("/a/y.BIN")].into_iter());
        assert_eq!(v, ["x.BIN — /a", "x.BIN — /b", "y.BIN"]);
    }

    fn ld(x: Vec<f64>, lines: Vec<Vec<u8>>) -> LineData {
        LineData { x, x_title: "t".into(), index_fallback: None, names: vec![], name_title: String::new(), lines }
    }

    #[test]
    fn chart_spacing_follows_axis_values_and_mode() {
        let c = chart(&ld(vec![800., 1000., 6000.], vec![vec![0, 0x7F, 0x80]]), Mode::Unsigned);
        assert_eq!((c.lo, c.hi), (0., 128.));
        assert_eq!(c.lines[0][0], (0., 0.));
        assert!((c.lines[0][1].0 - 200. / 5200.).abs() < 1e-6); // uneven spacing stays uneven
        assert_eq!(c.lines[0][2], (1., 1.));
        let c = chart(&ld(vec![0., 1., 2.], vec![vec![0x7F, 0x80, 0xFF]]), Mode::Signed);
        assert_eq!((c.lo, c.hi), (-128., 127.));
        assert_eq!(c.lines[0][0].1, 1.);
        assert_eq!(c.lines[0][1].1, 0.);
        let c = chart(&ld(vec![0., 1.], vec![vec![5, 5], vec![5, 5]]), Mode::Hex); // constant: finite and mid-height
        assert!(c.lines.iter().flatten().all(|p| p.1 == 0.5));
    }

    #[test]
    fn projection_orientation_and_depth() {
        let v = View { yaw: 0., pitch: 0.5, zoom: 1. };
        let (a, b) = (project(v, [-1., 0., 0.]), project(v, [1., 0., 0.]));
        assert!(b.0 > a.0); // +load is screen right at yaw 0
        assert!(project(v, [0., 0., 0.5]).1 < project(v, [0., 0., -0.5]).1); // higher value is up on screen
        assert!(project(v, [0., 1., 0.]).2 > project(v, [0., -1., 0.]).2); // high RPM is farther at yaw 0
        let c = project(v, [0., 0., 0.]);
        assert_eq!((c.0, c.1), (CW / 2., CH / 2.));
        let z2 = View { zoom: 2., ..v };
        assert!((project(z2, [1., 0., 0.]).0 - CW / 2. - 2. * (b.0 - CW / 2.)).abs() < 1e-3);
        // clamps
        assert_eq!(v.rotated(0., 1e6).pitch, PITCH_MAX);
        assert_eq!(v.rotated(0., -1e6).pitch, PITCH_MIN);
        assert_eq!(v.zoomed(100.).zoom, 4.);
        assert_eq!(v.zoomed(-100.).zoom, 0.5);
        assert!((0.0..std::f32::consts::TAU).contains(&v.rotated(-1e4, 0.).yaw));
    }

    #[test]
    fn surface_axes_proportional_and_signed_heights() {
        let p = pd(vec![0x00, 0x7F, 0x80, 0xFF, 1, 2, 3, 4, 5]);
        let s = surface(&p, Mode::Signed);
        assert_eq!((s.rows, s.cols, s.lo, s.hi), (3, 3, -128., 127.));
        assert_eq!(s.pts[0][..2], [-1., -1.]); // row 0 col 0 = lowest RPM, lowest load
        assert!((s.pts[1][0] - (-1. + 2. * 11. / 89.)).abs() < 1e-5); // load 22 spaced by value, not index
        assert!((s.pts[3][1] - (-1. + 2. * 200. / 5200.)).abs() < 1e-5); // RPM 1000 by value
        assert_eq!(s.pts[1][2], ZS); // 0x7F = +127 top
        assert_eq!(s.pts[2][2], -ZS); // 0x80 = -128 bottom
        assert!(s.pts[3][2] < s.pts[0][2]); // 0xFF = -1 below 0x00 = 0
        let u = surface(&p, Mode::Unsigned);
        assert_eq!((u.lo, u.hi), (0., 255.));
        assert_eq!(u.pts[3][2], ZS); // 0xFF top when unsigned
        let h = surface(&p, Mode::Hex);
        assert_eq!(h.pts, u.pts); // hex plots unsigned magnitude
    }

    #[test]
    fn constant_surface_is_flat_and_patches_sorted() {
        let s = surface(&pd(vec![7; 9]), Mode::Unsigned);
        assert!(s.pts.iter().all(|q| q[2] == 0.) && s.t.iter().all(|t| t.is_finite()));
        let ps = patches(&s, View::default());
        assert_eq!(ps.len(), 4);
        assert!(ps.windows(2).all(|w| w[0].depth >= w[1].depth)); // far first
        assert!(ps.iter().flat_map(|p| p.quad).all(|(x, y)| x.is_finite() && y.is_finite()));
    }
}
