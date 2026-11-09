//! Minimal GPUI dashboard over the read-only analysis core.

use crate::analysis::*;
use gpui::{
    App, Application, Bounds, Context, Div, FontWeight, Hsla, SharedString, Stateful, TitlebarOptions, Window,
    WindowBounds, WindowOptions, div, hsla, prelude::*, px, rgb, size,
};
use std::path::PathBuf;

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
enum Mode {
    Hex,
    Unsigned,
    Signed,
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
    /// User accepted viewing bytes under the file-offset hypothesis (session only).
    accepted: bool,
}

impl Rallye {
    fn new(dir: PathBuf) -> Self {
        let dir = dir.canonicalize().unwrap_or(dir);
        let mut s = Self {
            dir, inv: Err(String::new()), xdf_ix: 0, xdf: None, item: None,
            target: 0, baseline: None, mode: Mode::Unsigned, accepted: false,
        };
        s.reload();
        s
    }

    fn reload(&mut self) {
        self.inv = scan_dir(&self.dir);
        let n = self.inv.as_ref().map(|i| i.dumps.len()).unwrap_or(0);
        self.baseline = (n > 0).then_some(0);
        self.target = if n > 1 { 1 } else { 0 };
        self.load_xdf();
    }

    fn load_xdf(&mut self) {
        self.xdf = self.inv.as_ref().ok().and_then(|i| i.xdfs.get(self.xdf_ix)).map(|p| load_xdf(p));
        self.item = match &self.xdf {
            Some(Ok(x)) => default_item(x),
            _ => None,
        };
    }
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

fn tile(k: &str, v: impl Into<SharedString>, color: u32) -> Div {
    card()
        .min_w(px(130.))
        .child(div().text_xs().text_color(rgb(MUTED)).child(k.to_string()))
        .child(div().text_color(rgb(color)).font_weight(FontWeight::BOLD).child(v.into()))
}

fn heat(t: f32) -> Hsla {
    hsla(0.62 * (1.0 - t), 0.65, 0.30, 1.0)
}

impl Render for Rallye {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            .child(div().text_xs().text_color(rgb(MUTED)).child("IAW8P EPROM inspector · read-only"))
            .child(div().flex_1().text_xs().text_color(rgb(MUTED)).truncate().child(self.dir.display().to_string()))
            .child(btn("reload", "Reload", false).on_click(cx.listener(|this, _, _, cx| {
                this.reload();
                cx.notify();
            })));

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .font_family("Liberation Sans")
            .child(header)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.sidebar(cx))
                    .child(div().id("main").flex_1().min_w_0().overflow_y_scroll().p_4().child(self.main(cx))),
            )
    }
}

impl Rallye {
    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut col = div()
            .id("sidebar")
            .w(px(330.))
            .flex_none()
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .bg(rgb(PANEL))
            .border_r_1()
            .border_color(rgb(BORDER))
            .overflow_y_scroll();
        let inv = match &self.inv {
            Err(e) => return col.child(div().text_xs().text_color(rgb(ERR)).child(e.clone())),
            Ok(i) => i,
        };
        col = col.child(heading("DUMPS  (click = target, B = baseline)"));
        if inv.dumps.is_empty() {
            col = col.child(div().text_xs().text_color(rgb(WARN)).child("No .bin/.rom/.ori dumps in this directory."));
        }
        for (i, d) in inv.dumps.iter().enumerate() {
            let tgt = i == self.target;
            col = col.child(
                div()
                    .flex()
                    .gap_1()
                    .items_center()
                    .child(
                        div()
                            .id(SharedString::from(format!("dump{i}")))
                            .flex_1()
                            .min_w_0()
                            .p_2()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(rgb(if tgt { 0x1d3550 } else { PANEL2 }))
                            .border_1()
                            .border_color(rgb(if tgt { ACCENT } else { BORDER }))
                            .child(div().text_sm().truncate().child(d.name.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .font_family("JetBrainsMono Nerd Font")
                                    .child(format!("{} B · {}…{}", d.bytes.len(), &d.sha256[..10], &d.sha256[54..])),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.target = i;
                                cx.notify();
                            })),
                    )
                    .child(btn(format!("base{i}"), "B", self.baseline == Some(i)).on_click(cx.listener(move |this, _, _, cx| {
                        this.baseline = Some(i);
                        cx.notify();
                    }))),
            );
        }
        for e in &inv.errors {
            col = col.child(div().text_xs().text_color(rgb(ERR)).child(format!("rejected: {e}")));
        }
        col = col.child(heading("XDF DEFINITIONS"));
        if inv.xdfs.is_empty() {
            col = col.child(div().text_xs().text_color(rgb(WARN)).child("No .xdf in this directory."));
        }
        for (i, p) in inv.xdfs.iter().enumerate() {
            let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
            col = col.child(btn(format!("xdf{i}"), name, i == self.xdf_ix).on_click(cx.listener(move |this, _, _, cx| {
                this.xdf_ix = i;
                this.load_xdf();
                cx.notify();
            })));
        }
        match &self.xdf {
            Some(Err(e)) => col = col.child(div().text_xs().text_color(rgb(ERR)).child(e.clone())),
            Some(Ok(x)) => {
                col = col.child(heading("TABLES / SCALARS"));
                let flen = inv.dumps.get(self.target).map(|d| d.bytes.len() as u64).unwrap_or(0);
                for (i, it) in x.items.iter().enumerate() {
                    let sub = match layout(it, x, flen) {
                        Ok(l) => (format!("0x{:X} · {}×{} · 8-bit", l.address, l.rows, l.cols), MUTED),
                        Err(e) => (e, WARN),
                    };
                    let sel = self.item == Some(i);
                    col = col.child(
                        div()
                            .id(SharedString::from(format!("item{i}")))
                            .p_2()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(rgb(if sel { 0x1d3550 } else { PANEL2 }))
                            .border_1()
                            .border_color(rgb(if sel { ACCENT } else { BORDER }))
                            .child(div().text_sm().child(it.title.trim().to_string()))
                            .child(div().text_xs().text_color(rgb(sub.1)).child(sub.0))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.item = Some(i);
                                cx.notify();
                            })),
                    );
                }
            }
            None => {}
        }
        col
    }

    fn main(&self, cx: &mut Context<Self>) -> Div {
        let root = div().flex().flex_col().gap_3();
        let (Ok(inv), Some(Ok(x)), Some(ix)) = (&self.inv, &self.xdf, self.item) else {
            return root.child(card().child(div().text_color(rgb(WARN)).child("Load a directory with dumps and a valid XDF to begin.")));
        };
        let Some(dump) = inv.dumps.get(self.target) else {
            return root.child(card().child(div().text_color(rgb(WARN)).child("No dumps loaded.")));
        };
        let it = &x.items[ix];
        let raw = |v: &[(String, String)]| v.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("  ");
        let mut meta = card()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().text_lg().font_weight(FontWeight::BOLD).child(it.title.trim().to_string()))
                    .child(div().px_2().rounded_md().bg(rgb(0x3a2f12)).text_xs().text_color(rgb(WARN)).child(PROVENANCE)),
            )
            .child(kv("uniqueid / kind", format!("{} / {:?}", it.uniqueid, it.kind)))
            .child(kv("EMBEDDEDDATA (raw)", raw(&it.embedded_attrs)))
            .child(kv("item flags (raw)", it.item_flags.clone()))
            .child(kv("units", it.units.clone()))
            .child(kv("equation (not executed)", it.equation.clone()))
            .child(kv("description (raw)", it.description.clone()));
        for (n, a) in [("x axis", &it.x), ("y axis", &it.y)] {
            if let Some(a) = a {
                meta = meta.child(kv(n, format!("{} · count {} · labels [{}]", a.units, a.indexcount.map_or("?".into(), |n| n.to_string()), a.labels.join(", "))));
            }
        }
        let root = root.child(meta);

        let l = match layout(it, x, dump.bytes.len() as u64) {
            Ok(l) => l,
            Err(e) => {
                let bytes = raw_scalar_bytes(it, x, &dump.bytes)
                    .map(|b| b.iter().map(|v| format!("{v:02X}")).collect::<Vec<_>>().join(" "))
                    .unwrap_or_else(|e| e);
                return root.child(
                    card()
                        .child(div().text_color(rgb(ERR)).child(format!("Cell view unavailable: {e}")))
                        .child(kv("raw bytes at offset (no interpretation)", bytes)),
                );
            }
        };
        let root = root.child(
            card()
                .border_color(rgb(WARN))
                .child(div().text_sm().text_color(rgb(WARN)).child(hypothesis_text(&l, &x.base)))
                .when(!self.accepted, |c| {
                    c.child(div().flex().child(btn("accept", "View bytes under this file-offset hypothesis", false).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.accepted = true;
                            cx.notify();
                        }),
                    )))
                }),
        );
        if !self.accepted {
            return root;
        }

        let c = cells(&dump.bytes, &l);
        let st = stats(c).expect("layout guarantees cells");
        let base = self.baseline.and_then(|b| inv.dumps.get(b));
        let base_cells = base.and_then(|b| layout(it, x, b.bytes.len() as u64).ok().map(|bl| cells(&b.bytes, &bl)));
        let mut tiles = div()
            .flex()
            .flex_wrap()
            .gap_2()
            .child(tile("Target", dump.name.clone(), TEXT))
            .child(tile("Size", format!("{} B ({}×{})", l.len(), l.rows, l.cols), TEXT))
            .child(tile("Bounds (hypothesis)", format!("0x{:X}..0x{:X} / 0x{:X}", l.offset, l.end(), dump.bytes.len()), TEXT))
            .child(tile("Unsigned min/max", format!("{} / {}", st.min_u, st.max_u), TEXT))
            .child(tile("Signed min/max", format!("{} / {}", st.min_s, st.max_s), TEXT))
            .child(tile("Cells ≥ 0x80", st.high_bit.to_string(), TEXT))
            .child(tile(
                "Signedness",
                format!("UNRESOLVED (XDF claims {})", if l.xdf_claims_signed { "signed" } else { "unsigned" }),
                WARN,
            ));
        if let Some(b) = base {
            let whole = diff(&dump.bytes, &b.bytes);
            tiles = tiles
                .child(tile("Baseline", b.name.clone(), TEXT))
                .child(tile(
                    "Table cells changed",
                    base_cells.map(|bc| diff(c, bc).changed.to_string()).unwrap_or("baseline out of range".into()),
                    DIFF,
                ))
                .child(tile("File bytes changed", format!("{} (+{} len diff)", whole.changed, whole.extra), DIFF));
        }

        let modes = [(Mode::Hex, "Hex"), (Mode::Unsigned, "Unsigned"), (Mode::Signed, "Signed")];
        let mut bar = div().flex().gap_2().items_center().child(div().text_xs().text_color(rgb(MUTED)).child("Display"));
        for (m, label) in modes {
            bar = bar.child(btn(label, label, self.mode == m).on_click(cx.listener(move |this, _, _, cx| {
                this.mode = m;
                cx.notify();
            })));
        }
        bar = bar.child(div().text_xs().text_color(rgb(DIFF)).child("  ▢ orange outline = differs from baseline"));

        // Heat follows the selected interpretation, normalised over this table.
        let val = |b: u8| if self.mode == Mode::Signed { as_signed(b) as f32 } else { b as f32 };
        let (lo, hi) = c.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &b| (lo.min(val(b)), hi.max(val(b))));
        let label = |a: &Option<Axis>, i: u64| a.as_ref().and_then(|a| a.labels.get(i as usize).cloned()).unwrap_or_else(|| i.to_string());
        let cell_w = px(48.);
        let mut head = div().flex().child(div().w(px(64.)).text_xs().truncate().text_color(rgb(MUTED)).child(
            format!("{} \\ {}", it.y.as_ref().map(|a| a.units.as_str()).unwrap_or("row"), it.x.as_ref().map(|a| a.units.as_str()).unwrap_or("col")),
        ));
        for ci in 0..l.cols {
            head = head.child(div().w(cell_w).text_xs().text_color(rgb(MUTED)).flex().justify_center().child(label(&it.x, ci)));
        }
        let mut grid = div().flex().flex_col().gap(px(2.)).font_family("JetBrainsMono Nerd Font").child(head);
        for (ri, row) in c.chunks(l.cols as usize).enumerate() {
            let mut r = div().flex().gap(px(2.)).child(div().w(px(62.)).text_xs().text_color(rgb(MUTED)).child(label(&it.y, ri as u64)));
            for (ci, &b) in row.iter().enumerate() {
                let i = ri * l.cols as usize + ci;
                let changed = base_cells.is_some_and(|bc| bc[i] != b);
                let t = if hi > lo { (val(b) - lo) / (hi - lo) } else { 0.5 };
                let text = match self.mode {
                    Mode::Hex => format!("{b:02X}"),
                    Mode::Unsigned => b.to_string(),
                    Mode::Signed => as_signed(b).to_string(),
                };
                r = r.child(
                    div()
                        .w(cell_w - px(2.))
                        .h(px(22.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_sm()
                        .text_xs()
                        .bg(heat(t))
                        .border_2()
                        .border_color(if changed { rgb(DIFF).into() } else { heat(t) })
                        .child(text),
                );
            }
            grid = grid.child(r);
        }
        root.child(tiles).child(card().gap_2().child(bar).child(grid))
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
            |_, cx| cx.new(|_| Rallye::new(dir)),
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
