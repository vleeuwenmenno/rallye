//! Read-only analysis core shared by the GUI and the headless `report` mode.
//! Nothing here writes to an input file; the only writer is `write_new`, which
//! refuses to touch any path that already exists.

use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const PROVENANCE: &str = "XDF claim, not firmware proof";
pub const MAX_DUMP_BYTES: u64 = 4 * 1024 * 1024;

// ---------------------------------------------------------------- inventory

#[derive(Debug, Clone)]
pub struct Dump {
    pub path: PathBuf,
    pub name: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
}

#[derive(Debug, Default)]
pub struct Inventory {
    pub dir: PathBuf,
    pub dumps: Vec<Dump>,
    pub xdfs: Vec<PathBuf>,
    /// Files that matched a dump extension but could not be used.
    pub errors: Vec<String>,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn ext_lower(p: &Path) -> String {
    p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

pub fn load_dump(path: &Path) -> Result<Dump, String> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let meta = fs::metadata(path).map_err(|e| format!("{name}: cannot stat: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{name}: not a regular file"));
    }
    if meta.len() == 0 {
        return Err(format!("{name}: empty file (0 bytes), not a dump"));
    }
    if meta.len() > MAX_DUMP_BYTES {
        return Err(format!("{name}: {} bytes is larger than any EPROM this tool expects (max {MAX_DUMP_BYTES})", meta.len()));
    }
    let bytes = fs::read(path).map_err(|e| format!("{name}: cannot read: {e}"))?;
    Ok(Dump { path: path.to_path_buf(), name, sha256: sha256_hex(&bytes), bytes })
}

pub fn scan_dir(dir: &Path) -> Result<Inventory, String> {
    let rd = fs::read_dir(dir).map_err(|e| format!("cannot open directory {}: {e}", dir.display()))?;
    let mut paths: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    let mut inv = Inventory { dir: dir.to_path_buf(), ..Default::default() };
    for p in paths {
        match ext_lower(&p).as_str() {
            "bin" | "rom" | "ori" => match load_dump(&p) {
                Ok(d) => inv.dumps.push(d),
                Err(e) => inv.errors.push(e),
            },
            "xdf" if p.is_file() => inv.xdfs.push(p),
            _ => {}
        }
    }
    Ok(inv)
}

pub fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

impl Inventory {
    /// Validate and append one explicit dump from any folder. Returns its index; a file already
    /// loaded (same canonical path) selects the existing entry and never replaces its bytes.
    /// Second value is true when the file was newly added. Errors leave the inventory unchanged.
    pub fn import_dump(&mut self, path: &Path) -> Result<(usize, bool), String> {
        if !matches!(ext_lower(path).as_str(), "bin" | "rom" | "ori") {
            return Err(format!("{}: not a dump (expected .bin, .rom or .ori)", path.display()));
        }
        let c = canon(path);
        if let Some(i) = self.dumps.iter().position(|d| canon(&d.path) == c) {
            return Ok((i, false));
        }
        let mut d = load_dump(&c)?;
        d.path = c;
        self.dumps.push(d);
        Ok((self.dumps.len() - 1, true))
    }

    /// Same for an XDF: it must parse before it is added.
    pub fn import_xdf(&mut self, path: &Path) -> Result<(usize, bool), String> {
        if ext_lower(path) != "xdf" {
            return Err(format!("{}: not an XDF (expected .xdf)", path.display()));
        }
        let c = canon(path);
        load_xdf(&c)?;
        if let Some(i) = self.xdfs.iter().position(|x| canon(x) == c) {
            return Ok((i, false));
        }
        self.xdfs.push(c);
        Ok((self.xdfs.len() - 1, true))
    }
}

// ---------------------------------------------------------------- minimal XML DOM

#[derive(Debug, Default, Clone)]
pub struct Node {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<Node>,
}

impl Node {
    pub fn attr(&self, k: &str) -> Option<&str> {
        self.attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
    }
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }
    pub fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }
    pub fn text_of(&self, name: &str) -> String {
        self.child(name).map(|c| c.text.trim().to_string()).unwrap_or_default()
    }
}

pub fn parse_xml(src: &str) -> Result<Node, String> {
    use quick_xml::events::{BytesStart, Event};
    use quick_xml::Reader;

    fn open(e: &BytesStart) -> Result<Node, String> {
        let mut n = Node { name: e.name().as_ref().to_string(), ..Default::default() };
        for a in e.attributes() {
            let a = a.map_err(|e| format!("bad attribute: {e}"))?;
            let v = a.normalized_value(quick_xml::XmlVersion::Implicit1_0).map_err(|e| format!("bad attribute value: {e}"))?;
            n.attrs.push((a.key.as_ref().to_string(), v.into_owned()));
        }
        Ok(n)
    }

    let mut reader = Reader::from_str(src);
    let mut stack = vec![Node { name: "#document".into(), ..Default::default() }];
    loop {
        let pos = reader.buffer_position();
        let ev = reader.read_event().map_err(|e| format!("malformed XML near byte {pos}: {e}"))?;
        match ev {
            Event::Start(e) => stack.push(open(&e)?),
            Event::Empty(e) => {
                let n = open(&e)?;
                stack.last_mut().unwrap().children.push(n);
            }
            Event::End(_) => {
                let n = stack.pop().unwrap();
                match stack.last_mut() {
                    Some(parent) => parent.children.push(n),
                    None => return Err(format!("malformed XML near byte {pos}: unexpected closing tag")),
                }
            }
            Event::Text(t) => stack.last_mut().unwrap().text.push_str(&t.xml10_content()),
            Event::CData(t) => stack.last_mut().unwrap().text.push_str(&t.xml10_content()),
            Event::GeneralRef(r) => {
                let ch = match r.resolve_char_ref().map_err(|e| format!("bad char ref near byte {pos}: {e}"))? {
                    Some(c) => c,
                    None => match &*r.xml10_content() {
                        "lt" => '<',
                        "gt" => '>',
                        "amp" => '&',
                        "apos" => '\'',
                        "quot" => '"',
                        other => return Err(format!("unknown XML entity &{other}; near byte {pos}")),
                    },
                };
                stack.last_mut().unwrap().text.push(ch);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if stack.len() != 1 {
        return Err(format!("malformed XML: <{}> is never closed", stack.last().unwrap().name));
    }
    Ok(stack.pop().unwrap())
}

// ---------------------------------------------------------------- XDF model

pub fn parse_num(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let r = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16),
        None => t.parse::<u64>(),
    };
    r.map_err(|_| format!("not a number: {s:?}"))
}

#[derive(Debug, Clone, Default)]
pub struct BaseOffset {
    pub offset: u64,
    pub subtract: bool,
    pub present: bool,
    pub raw_attrs: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default)]
pub struct Axis {
    pub id: String,
    pub units: String,
    pub indexcount: Option<u64>,
    pub labels: Vec<String>,
    /// True only when every LABEL had a unique numeric `index`; labels are then placed by index
    /// (gaps stay empty). Otherwise document order is kept but must not be trusted for plotting.
    pub labels_indexed: bool,
    pub equation: String,
    pub embedded_attrs: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Table,
    Constant,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub kind: Kind,
    pub uniqueid: String,
    pub title: String,
    pub description: String,
    pub units: String,
    pub equation: String,
    /// `flags` attribute of XDFTABLE/XDFCONSTANT, kept verbatim.
    pub item_flags: String,
    /// All attributes of the data EMBEDDEDDATA element, kept verbatim (unknown ones included).
    pub embedded_attrs: Vec<(String, String)>,
    pub x: Option<Axis>,
    pub y: Option<Axis>,
}

impl Item {
    pub fn eattr(&self, k: &str) -> Option<&str> {
        self.embedded_attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct Xdf {
    pub path: PathBuf,
    pub title: String,
    pub description: String,
    pub base: BaseOffset,
    pub defaults: Vec<(String, String)>,
    pub items: Vec<Item>,
}

impl Xdf {
    fn default(&self, k: &str) -> Option<&str> {
        self.defaults.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
    }
}

fn axis(n: &Node) -> Axis {
    let raw: Vec<(Option<u64>, String)> = n
        .all("LABEL")
        .map(|l| (l.attr("index").and_then(|i| parse_num(i).ok()), l.attr("value").unwrap_or("").to_string()))
        .collect();
    let mut ix: Vec<u64> = raw.iter().filter_map(|(i, _)| *i).collect();
    ix.sort_unstable();
    ix.dedup();
    // shortcut: gap-fill cap of 4096 guards absurd indexes, raise if a real XDF needs more.
    let labels_indexed = !raw.is_empty() && ix.len() == raw.len() && ix.last().is_some_and(|&m| m < 4096);
    let labels = if labels_indexed {
        let mut v = vec![String::new(); *ix.last().unwrap() as usize + 1];
        for (i, val) in &raw {
            v[i.unwrap() as usize] = val.clone();
        }
        v
    } else {
        raw.into_iter().map(|(_, v)| v).collect()
    };
    Axis {
        id: n.attr("id").unwrap_or("").into(),
        units: n.text_of("units"),
        indexcount: n.child("indexcount").and_then(|c| parse_num(&c.text).ok()),
        labels,
        labels_indexed,
        equation: n.child("MATH").and_then(|m| m.attr("equation")).unwrap_or("").into(),
        embedded_attrs: n.child("EMBEDDEDDATA").map(|e| e.attrs.clone()).unwrap_or_default(),
    }
}

pub fn parse_xdf(path: &Path, src: &str) -> Result<Xdf, String> {
    let doc = parse_xml(src)?;
    let root = doc.child("XDFFORMAT").ok_or("not an XDF: missing <XDFFORMAT> root")?;
    let header = root.child("XDFHEADER").ok_or("malformed XDF: missing <XDFHEADER>")?;
    let mut base = BaseOffset::default();
    if let Some(b) = header.child("BASEOFFSET") {
        base.present = true;
        base.raw_attrs = b.attrs.clone();
        base.offset = parse_num(b.attr("offset").unwrap_or("0")).map_err(|e| format!("BASEOFFSET offset: {e}"))?;
        base.subtract = match b.attr("subtract").unwrap_or("0") {
            "0" => false,
            "1" => true,
            o => return Err(format!("BASEOFFSET subtract={o:?} is not 0 or 1")),
        };
    }
    let mut items = Vec::new();
    for n in &root.children {
        let kind = match n.name.as_str() {
            "XDFTABLE" => Kind::Table,
            "XDFCONSTANT" => Kind::Constant,
            _ => continue,
        };
        let (x, y, data) = if kind == Kind::Table {
            let ax = |id: &str| n.all("XDFAXIS").find(|a| a.attr("id") == Some(id));
            (ax("x").map(axis), ax("y").map(axis), ax("z"))
        } else {
            (None, None, Some(n))
        };
        let title = n.text_of("title");
        let data = data.ok_or_else(|| format!("table {title:?} has no z axis"))?;
        items.push(Item {
            kind,
            uniqueid: n.attr("uniqueid").unwrap_or("").into(),
            description: n.text_of("description"),
            units: data.text_of("units"),
            equation: data.child("MATH").and_then(|m| m.attr("equation")).unwrap_or("").into(),
            item_flags: n.attr("flags").unwrap_or("").into(),
            embedded_attrs: data.child("EMBEDDEDDATA").map(|e| e.attrs.clone()).unwrap_or_default(),
            title,
            x,
            y,
        });
    }
    Ok(Xdf {
        path: path.to_path_buf(),
        title: header.text_of("deftitle"),
        description: header.text_of("description"),
        base,
        defaults: header.child("DEFAULTS").map(|d| d.attrs.clone()).unwrap_or_default(),
        items,
    })
}

pub fn load_xdf(path: &Path) -> Result<Xdf, String> {
    let src = fs::read_to_string(path).map_err(|e| format!("{}: cannot read: {e}", path.display()))?;
    parse_xdf(path, &src).map_err(|e| format!("{}: {e}", path.display()))
}

// ---------------------------------------------------------------- layout / address hypothesis

/// Resolved byte layout. `offset` is a FILE-OFFSET HYPOTHESIS, never a verified runtime address.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub address: u64,
    pub offset: u64,
    pub rows: u64,
    pub cols: u64,
    pub elem_bits: u64,
    /// mmedtypeflags bit 0x01: what the XDF *claims*; never used to pick an interpretation.
    pub xdf_claims_signed: bool,
}

impl Layout {
    pub fn len(&self) -> u64 {
        self.rows * self.cols * self.elem_bits / 8
    }
    pub fn end(&self) -> u64 {
        self.offset + self.len()
    }
}

pub fn resolve_offset(address: u64, base: &BaseOffset) -> Result<u64, String> {
    if base.subtract {
        address.checked_sub(base.offset).ok_or_else(|| format!("address 0x{address:X} - BASEOFFSET 0x{:X} underflows", base.offset))
    } else {
        address.checked_add(base.offset).ok_or_else(|| format!("address 0x{address:X} + BASEOFFSET 0x{:X} overflows", base.offset))
    }
}

pub fn hypothesis_text(l: &Layout, base: &BaseOffset) -> String {
    let op = if base.subtract { '-' } else { '+' };
    let src = if base.present { "from XDF" } else { "absent in XDF, assumed 0" };
    format!(
        "File-offset hypothesis: file offset = XDF address 0x{:X} {op} BASEOFFSET 0x{:X} ({src}) = 0x{:X}. \
         Not a verified ECU runtime/bus mapping.",
        l.address, base.offset, l.offset
    )
}

fn count_attr(item: &Item, k: &str) -> Result<u64, String> {
    match item.eattr(k) {
        None => Ok(1),
        Some(v) => match parse_num(v)? {
            0 => Err(format!("{k}=0 is not a valid dimension")),
            n => Ok(n),
        },
    }
}

/// Layout of an item for raw 8-bit viewing. Anything this tool cannot read
/// faithfully is rejected with a reason instead of being guessed.
pub fn layout(item: &Item, xdf: &Xdf, file_len: u64) -> Result<Layout, String> {
    let elem_bits = match item.eattr("mmedelementsizebits").or(xdf.default("datasizeinbits")) {
        Some(v) => parse_num(v)?,
        None => return Err("element size not given (no mmedelementsizebits and no DEFAULTS datasizeinbits)".into()),
    };
    let flags_raw = item.eattr("mmedtypeflags");
    let flags = flags_raw.map(parse_num).transpose()?.unwrap_or(0);
    let xdf_claims_signed = match flags_raw {
        Some(_) => flags & 1 == 1,
        None => xdf.default("signed") == Some("1"),
    };
    let address = parse_num(item.eattr("mmedaddress").ok_or("no mmedaddress in EMBEDDEDDATA")?)?;
    let rows = count_attr(item, "mmedrowcount")?;
    let cols = count_attr(item, "mmedcolcount")?;
    if elem_bits != 8 {
        return Err(format!("{elem_bits}-bit elements: unsupported for cell viewing (metadata only)"));
    }
    if flags & !1 != 0 {
        return Err(format!("mmedtypeflags {} has bits other than 0x01 (layout/order unknown to this tool)", flags_raw.unwrap_or("")));
    }
    for k in ["mmedmajorstridebits", "mmedminorstridebits"] {
        let s = item.eattr(k).map(parse_num).transpose()?.unwrap_or(0);
        if s != 0 {
            return Err(format!("{k}={s}: non-contiguous stride unsupported"));
        }
    }
    let offset = resolve_offset(address, &xdf.base)?;
    let cells = rows.checked_mul(cols).ok_or("rows x cols overflows")?;
    let end = offset.checked_add(cells).ok_or("offset + size overflows")?;
    if end > file_len {
        return Err(format!(
            "out of range: needs file bytes 0x{offset:X}..0x{end:X} but file is 0x{file_len:X} ({file_len}) bytes"
        ));
    }
    Ok(Layout { address, offset, rows, cols, elem_bits, xdf_claims_signed })
}

/// Raw bytes covered by a scalar/constant, for display only (no interpretation).
pub fn raw_scalar_bytes<'a>(item: &Item, xdf: &Xdf, data: &'a [u8]) -> Result<&'a [u8], String> {
    let bits = parse_num(item.eattr("mmedelementsizebits").or(xdf.default("datasizeinbits")).unwrap_or("8"))?;
    if bits == 0 || bits % 8 != 0 {
        return Err(format!("{bits}-bit element is not whole bytes"));
    }
    let off = resolve_offset(parse_num(item.eattr("mmedaddress").ok_or("no mmedaddress")?)?, &xdf.base)?;
    let end = off.checked_add(bits / 8).ok_or("offset overflows")?;
    if end > data.len() as u64 {
        return Err(format!("out of range: 0x{off:X}..0x{end:X} beyond file length 0x{:X}", data.len()));
    }
    Ok(&data[off as usize..end as usize])
}

// ---------------------------------------------------------------- interpretation

pub fn as_signed(b: u8) -> i8 {
    b as i8
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stats {
    pub count: usize,
    pub min_u: u8,
    pub max_u: u8,
    pub min_s: i8,
    pub max_s: i8,
    pub high_bit: usize,
}

pub fn stats(cells: &[u8]) -> Option<Stats> {
    if cells.is_empty() {
        return None;
    }
    let s: Vec<i8> = cells.iter().map(|&b| as_signed(b)).collect();
    Some(Stats {
        count: cells.len(),
        min_u: *cells.iter().min()?,
        max_u: *cells.iter().max()?,
        min_s: *s.iter().min()?,
        max_s: *s.iter().max()?,
        high_bit: cells.iter().filter(|&&b| b >= 0x80).count(),
    })
}

/// Target minus baseline as i16, so 0x7F-0x80 (signed) and 255-0 (unsigned) never overflow.
pub fn cell_delta(target: u8, base: u8, signed: bool) -> i16 {
    if signed { as_signed(target) as i16 - as_signed(base) as i16 } else { target as i16 - base as i16 }
}

pub fn cells<'a>(data: &'a [u8], l: &Layout) -> &'a [u8] {
    &data[l.offset as usize..l.end() as usize]
}

/// RPM x load surface for one table. `cells` stay row-major: row = RPM (Y), column = load (X).
#[derive(Debug, Clone, PartialEq)]
pub struct PlotData {
    pub rpm: Vec<f64>,
    pub load: Vec<f64>,
    /// XDF X-axis units verbatim (for example "Load index"); never relabelled as %.
    pub load_units: String,
    pub cells: Vec<u8>,
}

/// Axis labels as numbers: must be complete (`n` labels), indexed, finite and strictly increasing.
fn numeric_axis(a: &Axis, n: u64, name: &str) -> Result<Vec<f64>, String> {
    if !a.labels_indexed {
        return Err(format!("{name} axis labels lack unique LABEL index values"));
    }
    if a.labels.len() as u64 != n {
        return Err(format!("{} {name} labels for {n} cells", a.labels.len()));
    }
    let v = a
        .labels
        .iter()
        .map(|s| s.trim().parse::<f64>().ok().filter(|v| v.is_finite()).ok_or(format!("{name} label {s:?} is not a finite number")))
        .collect::<Result<Vec<_>, _>>()?;
    if v.windows(2).any(|w| w[1] <= w[0]) {
        return Err(format!("{name} labels are not strictly increasing"));
    }
    Ok(v)
}

/// Surface only for >=2x2 tables whose XDF Y axis says RPM and both axes have complete, indexed,
/// finite, strictly increasing labels. Never falls back to row/column indexes.
pub fn plot_data(item: &Item, l: &Layout, cells: &[u8]) -> Result<PlotData, String> {
    if l.rows < 2 || l.cols < 2 {
        return Err(format!("surface needs at least 2x2 cells, table is {}x{}", l.rows, l.cols));
    }
    let y = item.y.as_ref().ok_or("no Y axis in XDF")?;
    if !y.units.trim().eq_ignore_ascii_case("rpm") {
        return Err(format!("Y axis units are {:?}, not RPM", y.units));
    }
    let rpm = numeric_axis(y, l.rows, "Y")?;
    let x = item.x.as_ref().ok_or("no X axis in XDF")?;
    let load = numeric_axis(x, l.cols, "X")?;
    if cells.len() as u64 != l.rows * l.cols {
        return Err("cell count does not match layout".into());
    }
    Ok(PlotData { rpm, load, load_units: x.units.trim().to_string(), cells: cells.to_vec() })
}

// ---------------------------------------------------------------- 16-bit scalar (read-only)

/// A single 16-bit XDFCONSTANT. Offsets are a file-offset hypothesis like `Layout`.
#[derive(Debug, Clone, PartialEq)]
pub struct ScalarLayout {
    pub address: u64,
    pub offset: u64,
    /// XDF `lsbfirst` / mmedtypeflags 0x02: bytes are little-endian.
    pub little_endian: bool,
    /// What the XDF claims (flags 0x01); display interpretation stays a viewer choice.
    pub xdf_claims_signed: bool,
}

/// Layout of a 16-bit constant. Anything else (tables, other widths, float, strides, unknown
/// flag bits, dimensions other than 1x1) is rejected with a reason, never guessed.
pub fn scalar_layout(item: &Item, xdf: &Xdf, file_len: u64) -> Result<ScalarLayout, String> {
    if item.kind != Kind::Constant {
        return Err("not an XDFCONSTANT".into());
    }
    let bits = match item.eattr("mmedelementsizebits").or(xdf.default("datasizeinbits")) {
        Some(v) => parse_num(v)?,
        None => return Err("element size not given".into()),
    };
    if bits != 16 {
        return Err(format!("{bits}-bit constant: only 16-bit scalars are decoded"));
    }
    if xdf.default("float").is_some_and(|f| f != "0") {
        return Err("DEFAULTS float is set: float layout unsupported".into());
    }
    let flags_raw = item.eattr("mmedtypeflags");
    let flags = flags_raw.map(parse_num).transpose()?;
    if flags.is_some_and(|f| f & !0x03 != 0) {
        return Err(format!("mmedtypeflags {} has bits other than 0x01 (signed) / 0x02 (LSB first)", flags_raw.unwrap_or("")));
    }
    let (little_endian, xdf_claims_signed) = match flags {
        Some(f) => (f & 2 != 0, f & 1 != 0),
        None => (xdf.default("lsbfirst") == Some("1"), xdf.default("signed") == Some("1")),
    };
    for k in ["mmedmajorstridebits", "mmedminorstridebits"] {
        let s = item.eattr(k).map(parse_num).transpose()?.unwrap_or(0);
        if s != 0 {
            return Err(format!("{k}={s}: non-contiguous stride unsupported"));
        }
    }
    if count_attr(item, "mmedrowcount")? != 1 || count_attr(item, "mmedcolcount")? != 1 {
        return Err("constant with rows/cols other than 1x1 is not a scalar".into());
    }
    let address = parse_num(item.eattr("mmedaddress").ok_or("no mmedaddress in EMBEDDEDDATA")?)?;
    let offset = resolve_offset(address, &xdf.base)?;
    let end = offset.checked_add(2).ok_or("offset + size overflows")?;
    if end > file_len {
        return Err(format!("out of range: needs file bytes 0x{offset:X}..0x{end:X} but file is 0x{file_len:X} ({file_len}) bytes"));
    }
    Ok(ScalarLayout { address, offset, little_endian, xdf_claims_signed })
}

/// Raw bytes in file order and the unsigned word they decode to. None if out of bounds.
pub fn scalar_read(l: &ScalarLayout, data: &[u8]) -> Option<([u8; 2], u16)> {
    let o = usize::try_from(l.offset).ok()?;
    let b: [u8; 2] = data.get(o..o.checked_add(2)?)?.try_into().ok()?;
    Some((b, if l.little_endian { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) }))
}

/// Target minus baseline as i32, wide enough for any pair of 16-bit words in either interpretation.
pub fn scalar_delta(target: u16, base: u16, signed: bool) -> i32 {
    if signed { target as i16 as i32 - base as i16 as i32 } else { target as i32 - base as i32 }
}

/// The ONE documented conversion, RPM = 15000000 / X (unsigned word X). Any other equation, units
/// other than RPM, an XDF signed claim or X = 0 yields an explicit reason; nothing is executed.
pub fn scalar_rpm(item: &Item, l: &ScalarLayout, word: u16) -> Result<f64, String> {
    let eq: String = item.equation.split_whitespace().collect();
    if eq != "15000000/X" || !item.units.trim().eq_ignore_ascii_case("rpm") {
        return Err(format!("equation {:?} (units {:?}) is not the documented 15000000/X RPM pattern", item.equation, item.units));
    }
    if l.xdf_claims_signed {
        return Err("XDF claims signed storage; 15000000/X is documented for unsigned".into());
    }
    if word == 0 {
        return Err("raw value 0: division by zero".into());
    }
    Ok(15_000_000.0 / word as f64)
}

// ---------------------------------------------------------------- 2D line data

/// Native 2D chart input. Cells stay row-major (row = XDF Y axis, column = XDF X axis).
#[derive(Debug, Clone, PartialEq)]
pub struct LineData {
    /// Horizontal positions, numeric axis values (or 0..n when `index_fallback` is set).
    pub x: Vec<f64>,
    /// Horizontal axis title: XDF axis units verbatim, or "sample index".
    pub x_title: String,
    /// Why numeric axis labels were not usable; the chart then uses the sample index.
    pub index_fallback: Option<String>,
    /// One name per line (the other axis label); empty for a single line.
    pub names: Vec<String>,
    /// What `names` are: XDF X-axis units verbatim.
    pub name_title: String,
    /// lines[s][i] = raw byte of line s at horizontal position i.
    pub lines: Vec<Vec<u8>>,
}

/// Curves and small maps as lines: 1xN uses the X axis horizontally with one line; N>=2 rows use
/// the Y axis horizontally with one line per column. 1x1 is a single value, not a line.
pub fn line_data(item: &Item, l: &Layout, cells: &[u8]) -> Result<LineData, String> {
    if l.rows == 1 && l.cols == 1 {
        return Err("single value: nothing to draw as a line".into());
    }
    if cells.len() as u64 != l.rows * l.cols {
        return Err("cell count does not match layout".into());
    }
    let (rows, cols) = (l.rows as usize, l.cols as usize);
    let (axis, n, name, lines, names) = if rows == 1 {
        (item.x.as_ref(), l.cols, "X", vec![cells.to_vec()], vec![])
    } else {
        let lines = (0..cols).map(|c| (0..rows).map(|r| cells[r * cols + c]).collect()).collect();
        let names = if cols > 1 { (0..cols).map(|c| item.x.as_ref().and_then(|a| a.labels.get(c).cloned()).unwrap_or_else(|| c.to_string())).collect() } else { vec![] };
        (item.y.as_ref(), l.rows, "Y", lines, names)
    };
    let (x, x_title, index_fallback) = match axis.ok_or_else(|| format!("no {name} axis in XDF")).and_then(|a| numeric_axis(a, n, name).map(|v| (v, a.units.trim().to_string()))) {
        Ok((v, u)) => (v, if u.is_empty() { format!("XDF {name} axis (no units given)") } else { u }, None),
        Err(e) => ((0..n).map(|i| i as f64).collect(), "sample index".to_string(), Some(e)),
    };
    let name_title = item.x.as_ref().map(|a| a.units.trim().to_string()).filter(|u| !u.is_empty()).unwrap_or_else(|| "XDF X axis".into());
    Ok(LineData { x, x_title, index_fallback, names, name_title, lines })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Diff {
    pub len_a: usize,
    pub len_b: usize,
    /// Differing bytes over the common prefix.
    pub changed: usize,
    /// Bytes present in only one of the two inputs.
    pub extra: usize,
}

pub fn diff(a: &[u8], b: &[u8]) -> Diff {
    let changed = a.iter().zip(b).filter(|(x, y)| x != y).count();
    Diff { len_a: a.len(), len_b: b.len(), changed, extra: a.len().abs_diff(b.len()) }
}

// ---------------------------------------------------------------- export

/// Write `content` to a path that must not exist yet (no clobber, inputs included).
pub fn write_new(path: &Path, content: &[u8]) -> Result<(), String> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("refusing to write {}: {e}", path.display()))?;
    f.write_all(content).map_err(|e| format!("write {}: {e}", path.display()))
}

fn kv_json(v: &[(String, String)]) -> serde_json::Value {
    serde_json::Value::Object(v.iter().map(|(k, v)| (k.clone(), v.clone().into())).collect())
}

fn axis_json(a: &Option<Axis>) -> serde_json::Value {
    match a {
        None => serde_json::Value::Null,
        Some(a) => serde_json::json!({
            "units": a.units, "indexcount": a.indexcount, "labels": a.labels,
            "equation_raw": a.equation, "embeddeddata_raw": kv_json(&a.embedded_attrs),
        }),
    }
}

pub fn item_json(item: &Item) -> serde_json::Value {
    serde_json::json!({
        "provenance": PROVENANCE,
        "kind": format!("{:?}", item.kind),
        "uniqueid": item.uniqueid,
        "title": item.title,
        "description_raw": item.description,
        "units": item.units,
        "equation_raw_not_executed": item.equation,
        "item_flags_raw": item.item_flags,
        "embeddeddata_raw": kv_json(&item.embedded_attrs),
        "x_axis": axis_json(&item.x),
        "y_axis": axis_json(&item.y),
    })
}

pub const WARNINGS: [&str; 5] = [
    "Read-only analysis. No calibration was written, exported to ECU, or flashed.",
    "Offsets are a file-offset hypothesis derived from XDF address and BASEOFFSET; no runtime mapping is verified.",
    "Signedness is UNRESOLVED: both unsigned and signed interpretations are shown; none is inferred from data smoothness or XDF comments.",
    "XDF descriptions, flags and axis labels are XDF claims, not firmware proof (no disassembly was performed).",
    "No engineering scaling is applied; XDF equations are shown raw and never executed.",
];

/// Full report for one table across all dumps, with diff against `baseline`.
pub fn report_json(inv: &Inventory, xdf: &Xdf, item: &Item, baseline: Option<&Dump>) -> serde_json::Value {
    let files: Vec<_> = inv
        .dumps
        .iter()
        .map(|d| {
            let lay = layout(item, xdf, d.bytes.len() as u64);
            let table = match &lay {
                Err(e) => serde_json::json!({ "error": e }),
                Ok(l) => {
                    let c = cells(&d.bytes, l);
                    let st = stats(c).unwrap();
                    let rows = |f: &dyn Fn(u8) -> serde_json::Value| -> Vec<Vec<serde_json::Value>> {
                        c.chunks(l.cols as usize).map(|r| r.iter().map(|&b| f(b)).collect()).collect()
                    };
                    let base_cmp = baseline.map(|b| match layout(item, xdf, b.bytes.len() as u64) {
                        Ok(bl) => serde_json::json!({
                            "baseline": b.name,
                            "changed_cells": diff(c, cells(&b.bytes, &bl)).changed,
                        }),
                        Err(e) => serde_json::json!({ "baseline": b.name, "error": e }),
                    });
                    serde_json::json!({
                        "hypothesis": hypothesis_text(l, &xdf.base),
                        "file_offset_hypothesis": format!("0x{:X}", l.offset),
                        "end_exclusive": format!("0x{:X}", l.end()),
                        "rows": l.rows, "cols": l.cols, "size_bytes": l.len(),
                        "in_bounds": true,
                        "signedness": "unresolved",
                        "xdf_claims_signed": l.xdf_claims_signed,
                        "min_unsigned": st.min_u, "max_unsigned": st.max_u,
                        "min_signed": st.min_s, "max_signed": st.max_s,
                        "count_ge_0x80": st.high_bit,
                        "unsigned": rows(&|b| b.into()),
                        "signed": rows(&|b| as_signed(b).into()),
                        "vs_baseline": base_cmp,
                    })
                }
            };
            let whole = baseline.map(|b| {
                let df = diff(&d.bytes, &b.bytes);
                serde_json::json!({ "baseline": b.name, "changed_bytes": df.changed, "length_difference": df.extra })
            });
            serde_json::json!({
                "name": d.name, "path": d.path, "size": d.bytes.len(), "sha256": d.sha256,
                "whole_file_vs_baseline": whole, "table": table,
            })
        })
        .collect();
    serde_json::json!({
        "tool": "rallye", "version": env!("CARGO_PKG_VERSION"),
        "warnings": WARNINGS,
        "directory": inv.dir,
        "inventory_errors": inv.errors,
        "xdf": {
            "path": xdf.path, "sha256": fs::read(&xdf.path).map(|b| sha256_hex(&b)).unwrap_or_default(),
            "title": xdf.title, "description_raw": xdf.description, "provenance": PROVENANCE,
            "baseoffset": { "present": xdf.base.present, "offset": xdf.base.offset, "subtract": xdf.base.subtract, "raw": kv_json(&xdf.base.raw_attrs) },
            "defaults_raw": kv_json(&xdf.defaults),
        },
        "table": item_json(item),
        "baseline": baseline.map(|b| &b.name),
        "files": files,
    })
}

/// One CSV row per (file, cell).
pub fn report_csv(inv: &Inventory, xdf: &Xdf, item: &Item, baseline: Option<&Dump>) -> String {
    let mut out = String::from("file,sha256,row,col,row_label,col_label,file_offset_hypothesis,hex,unsigned,signed,baseline_unsigned,changed\n");
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let base = baseline.and_then(|b| layout(item, xdf, b.bytes.len() as u64).ok().map(|l| cells(&b.bytes, &l)));
    for d in &inv.dumps {
        let Ok(l) = layout(item, xdf, d.bytes.len() as u64) else { continue };
        for (i, &b) in cells(&d.bytes, &l).iter().enumerate() {
            let (r, c) = (i as u64 / l.cols, i as u64 % l.cols);
            let lab = |a: &Option<Axis>, ix: u64| a.as_ref().and_then(|a| a.labels.get(ix as usize).cloned()).unwrap_or_default();
            let bv = base.map(|bc| bc[i]);
            out += &format!(
                "{},{},{r},{c},{},{},0x{:X},0x{b:02X},{b},{},{},{}\n",
                q(&d.name), d.sha256, q(&lab(&item.y, r)), q(&lab(&item.x, c)), l.offset + i as u64,
                as_signed(b),
                bv.map(|v| v.to_string()).unwrap_or_default(),
                bv.map(|v| (v != b).to_string()).unwrap_or_default(),
            );
        }
    }
    out
}

/// Initial table: picked by XDF title, not by a hard-coded address.
pub const DEFAULT_TABLE_TITLE: &str = "Injection, part throttle";

pub fn default_item(xdf: &Xdf) -> Option<usize> {
    xdf.items.iter().position(|i| i.title.trim() == DEFAULT_TABLE_TITLE).or(if xdf.items.is_empty() { None } else { Some(0) })
}

#[cfg(test)]
mod tests {
    use super::*;

    const XDF: &str = r#"<?xml version="1.0"?>
<XDFFORMAT version="1.80"><XDFHEADER><deftitle>T &amp; U</deftitle>
<BASEOFFSET offset="0x10" subtract="1" /><DEFAULTS datasizeinbits="8" signed="0" />
</XDFHEADER>
<XDFTABLE uniqueid="0x20" flags="0x0"><title>Injection, part throttle</title><description>claims signed</description>
<XDFAXIS id="x"><indexcount>3</indexcount><LABEL index="0" value="a"/><LABEL index="1" value="b"/><LABEL index="2" value="c"/></XDFAXIS>
<XDFAXIS id="y"><indexcount>2</indexcount></XDFAXIS>
<XDFAXIS id="z"><EMBEDDEDDATA mmedtypeflags="0x01" mmedaddress="0x20" mmedelementsizebits="8" mmedrowcount="2" mmedcolcount="3" mmedmajorstridebits="0" mmedfuture="7"/><MATH equation="X*2"/></XDFAXIS>
</XDFTABLE>
<XDFCONSTANT uniqueid="0x30"><title>Rev</title><units>RPM</units><EMBEDDEDDATA mmedelementsizebits="16" mmedaddress="0x30"/><MATH equation="15000000/X"/></XDFCONSTANT>
</XDFFORMAT>"#;

    fn xdf() -> Xdf {
        parse_xdf(Path::new("t.xdf"), XDF).unwrap()
    }

    #[test]
    fn signed_boundary() {
        assert_eq!(as_signed(0x7F), 127);
        assert_eq!(as_signed(0x80), -128);
        assert_eq!(as_signed(0xFF), -1);
        assert_eq!(as_signed(0x00), 0);
        let s = stats(&[0x00, 0x7F, 0x80, 0xFF]).unwrap();
        assert_eq!((s.min_u, s.max_u, s.min_s, s.max_s, s.high_bit), (0, 0xFF, -128, 127, 2));
    }

    #[test]
    fn xdf_parse_layout_and_baseoffset() {
        let x = xdf();
        assert_eq!(x.title, "T & U");
        assert!(x.base.present && x.base.subtract && x.base.offset == 0x10);
        let t = &x.items[default_item(&x).unwrap()];
        assert_eq!(t.equation, "X*2");
        assert_eq!(t.eattr("mmedfuture"), Some("7"), "unknown attrs preserved");
        assert_eq!(t.x.as_ref().unwrap().labels, ["a", "b", "c"]);
        let l = layout(t, &x, 0x100).unwrap();
        assert_eq!((l.address, l.offset, l.rows, l.cols, l.len()), (0x20, 0x10, 2, 3, 6));
        assert!(l.xdf_claims_signed);
        assert!(hypothesis_text(&l, &x.base).contains("0x20 - BASEOFFSET 0x10"));
        // add mode
        let mut x2 = x.clone();
        x2.base.subtract = false;
        assert_eq!(layout(t, &x2, 0x100).unwrap().offset, 0x30);
        // 16-bit constant: metadata only, raw bytes still readable
        let c = &x.items[1];
        assert!(layout(c, &x, 0x100).unwrap_err().contains("16-bit"));
        let data: Vec<u8> = (0..=255).collect();
        assert_eq!(raw_scalar_bytes(c, &x, &data).unwrap(), &[0x20, 0x21]);
    }

    #[test]
    fn surface_axes_orientation_and_invalid_axes() {
        let x = xdf();
        let mut t = x.items[0].clone(); // 2 rows x 3 cols
        let l = layout(&t, &x, 0x100).unwrap();
        // Build any axis by swapping the fixture's Y axis; `.y` then holds it (id is irrelevant here).
        let ax = |n: usize, units: &str, labels: &[(&str, &str)]| {
            let body: String = labels.iter().map(|(i, v)| format!("<LABEL index=\"{i}\" value=\"{v}\"/>")).collect();
            let src = XDF.replace(
                "<XDFAXIS id=\"y\"><indexcount>2</indexcount></XDFAXIS>",
                &format!("<XDFAXIS id=\"y\"><units>{units}</units><indexcount>{n}</indexcount>{body}</XDFAXIS>"),
            );
            parse_xdf(Path::new("t.xdf"), &src).unwrap().items[0].y.clone()
        };
        let cells = [1u8, 2, 0x7F, 0x80, 0xFF, 0x00];
        t.y = ax(2, "RPM", &[("1", "6000"), ("0", "800")]); // out of order but indexed
        t.x = ax(3, "Load index", &[("0", "11"), ("1", "22"), ("2", "100")]);
        let p = plot_data(&t, &l, &cells).unwrap();
        assert_eq!(p.rpm, [800.0, 6000.0]); // uneven spacing kept as numbers
        assert_eq!(p.load, [11.0, 22.0, 100.0]);
        assert_eq!(p.load_units, "Load index"); // verbatim, not %
        assert_eq!(p.cells, cells); // row-major: row = RPM, column = load
        let good_x = t.x.clone();
        for bad in [
            ax(2, "RPM", &[("0", "800"), ("2", "6000")]), // sparse: row 1 label empty
            ax(2, "RPM", &[("0", "800")]),                // incomplete
            ax(2, "RPM", &[("0", "800"), ("1", "800")]),  // not strictly increasing
            ax(2, "RPM", &[("0", "800"), ("1", "NaN")]),  // not finite
            ax(2, "kPa", &[("0", "800"), ("1", "900")]),  // not RPM
            ax(2, "RPM", &[("0", "800"), ("0", "900")]),  // duplicate index
            None,
        ] {
            t.y = bad;
            assert!(plot_data(&t, &l, &cells).is_err());
        }
        t.y = ax(2, "RPM", &[("0", "800"), ("1", "6000")]);
        for bad in [
            x.items[0].x.clone(),                                                     // labels "a","b","c"
            ax(3, "Load index", &[("0", "11"), ("1", "22")]),           // count != columns
            ax(3, "Load index", &[("0", "11"), ("1", "33"), ("2", "22")]), // not monotonic
            ax(3, "Load index", &[("0", "11"), ("1", "inf"), ("2", "33")]), // not finite
            None,
        ] {
            t.x = bad;
            assert!(plot_data(&t, &l, &cells).is_err());
        }
        t.x = good_x;
        assert!(plot_data(&t, &l, &cells).is_ok());
        let one_col = Layout { cols: 1, rows: 6, ..l.clone() };
        assert!(plot_data(&t, &one_col, &cells).unwrap_err().contains("2x2"));
    }

    /// Fixture with the constant's EMBEDDEDDATA attributes (and optional DEFAULTS) replaced.
    fn const_xdf(attrs: &str, equation: &str) -> Xdf {
        let src = XDF
            .replace("mmedelementsizebits=\"16\" mmedaddress=\"0x30\"", attrs)
            .replace("equation=\"15000000/X\"", &format!("equation=\"{equation}\""));
        parse_xdf(Path::new("t.xdf"), &src).unwrap()
    }

    #[test]
    fn scalar_16bit_endian_signed_and_flags() {
        let mut file = vec![0u8; 0x40];
        file[0x20..0x22].copy_from_slice(&[0x07, 0x88]);
        let x = const_xdf("mmedelementsizebits=\"16\" mmedaddress=\"0x30\"", "15000000/X");
        let it = &x.items[1];
        let l = scalar_layout(it, &x, file.len() as u64).unwrap();
        assert_eq!((l.address, l.offset, l.little_endian, l.xdf_claims_signed), (0x30, 0x20, false, false));
        assert_eq!(scalar_read(&l, &file), Some(([0x07, 0x88], 0x0788))); // big endian default
        assert_eq!(scalar_rpm(it, &l, 0x0788).unwrap().round(), 7_780.0); // 15000000/1928, matches the XDF description (unverified)
        // little-endian via flag 0x02, signed claim via 0x03
        let x = const_xdf("mmedelementsizebits=\"16\" mmedtypeflags=\"0x02\" mmedaddress=\"0x30\"", "15000000/X");
        let l = scalar_layout(&x.items[1], &x, 0x40).unwrap();
        assert!(l.little_endian && !l.xdf_claims_signed);
        assert_eq!(scalar_read(&l, &file).unwrap().1, 0x8807);
        let x = const_xdf("mmedelementsizebits=\"16\" mmedtypeflags=\"0x03\" mmedaddress=\"0x30\"", "15000000/X");
        let l = scalar_layout(&x.items[1], &x, 0x40).unwrap();
        assert!(l.little_endian && l.xdf_claims_signed);
        assert!(scalar_rpm(&x.items[1], &l, 5).unwrap_err().contains("signed"));
        // signed reinterpretation of the same word
        assert_eq!(0x8807u16 as i16, -30713);
        // unknown flag bits, float, strides, 8-bit, dimensions, address outside the file
        for bad in [
            "mmedelementsizebits=\"16\" mmedtypeflags=\"0x04\" mmedaddress=\"0x30\"",
            "mmedelementsizebits=\"16\" mmedmajorstridebits=\"8\" mmedaddress=\"0x30\"",
            "mmedelementsizebits=\"16\" mmedrowcount=\"2\" mmedaddress=\"0x30\"",
            "mmedelementsizebits=\"32\" mmedaddress=\"0x30\"",
            "mmedelementsizebits=\"16\"",
        ] {
            let x = const_xdf(bad, "15000000/X");
            assert!(scalar_layout(&x.items[1], &x, 0x100).is_err(), "{bad}");
        }
        let x = const_xdf("mmedelementsizebits=\"16\" mmedaddress=\"0x30\"", "15000000/X");
        assert!(scalar_layout(&x.items[1], &x, 0x21).unwrap_err().contains("out of range")); // 0x20..0x22 > 0x21
        assert!(scalar_layout(&x.items[1], &x, 0x22).is_ok());
        assert!(scalar_layout(&x.items[0], &x, 0x100).is_err()); // tables are never scalars
        let l = scalar_layout(&x.items[1], &x, 0x22).unwrap();
        assert_eq!(scalar_read(&l, &file[..0x21]), None); // short data: no panic
        // DEFAULTS lsbfirst applies when the item has no flags
        let src = XDF.replace("signed=\"0\" />", "signed=\"0\" lsbfirst=\"1\" />");
        let x = parse_xdf(Path::new("t.xdf"), &src).unwrap();
        assert!(scalar_layout(&x.items[1], &x, 0x40).unwrap().little_endian);
    }

    #[test]
    fn scalar_delta_and_rpm_formula() {
        assert_eq!(scalar_delta(0xFFFF, 0, false), 65_535);
        assert_eq!(scalar_delta(0, 0xFFFF, false), -65_535);
        assert_eq!(scalar_delta(0x7FFF, 0x8000, true), 65_535); // 32767 - (-32768)
        assert_eq!(scalar_delta(0x8000, 0x7FFF, true), -65_535);
        let x = const_xdf("mmedelementsizebits=\"16\" mmedaddress=\"0x30\"", "15000000/X");
        let it = &x.items[1];
        let l = scalar_layout(it, &x, 0x100).unwrap();
        assert!(scalar_rpm(it, &l, 0).unwrap_err().contains("division by zero"));
        assert_eq!(scalar_rpm(it, &l, 1).unwrap(), 15_000_000.0);
        for eq in ["X*2", "15000000/X+1", "15000001/X", "system(1)"] {
            let x = const_xdf("mmedelementsizebits=\"16\" mmedaddress=\"0x30\"", eq);
            let l = scalar_layout(&x.items[1], &x, 0x100).unwrap();
            assert!(scalar_rpm(&x.items[1], &l, 7).is_err(), "{eq}");
        }
        let src = XDF.replace("<units>RPM</units><EMBEDDEDDATA", "<units>ms</units><EMBEDDEDDATA");
        let x = parse_xdf(Path::new("t.xdf"), &src).unwrap();
        assert!(scalar_rpm(&x.items[1], &l, 7).is_err()); // right equation, units not RPM
        let x = const_xdf("mmedelementsizebits=\"16\" mmedaddress=\"0x30\"", " 15000000 / X ");
        assert!(scalar_rpm(&x.items[1], &l, 7).is_ok()); // whitespace only
    }

    fn lay(rows: u64, cols: u64) -> Layout {
        Layout { address: 0, offset: 0, rows, cols, elem_bits: 8, xdf_claims_signed: false }
    }

    fn axis_of(units: &str, labels: &[&str]) -> Option<Axis> {
        let l: String = labels.iter().enumerate().map(|(i, v)| format!("<LABEL index=\"{i}\" value=\"{v}\"/>")).collect();
        let src = XDF.replace(
            "<XDFAXIS id=\"y\"><indexcount>2</indexcount></XDFAXIS>",
            &format!("<XDFAXIS id=\"y\"><units>{units}</units>{l}</XDFAXIS>"),
        );
        parse_xdf(Path::new("t.xdf"), &src).unwrap().items[0].y.clone()
    }

    #[test]
    fn line_data_curves_and_maps() {
        let mut it = xdf().items[0].clone();
        // 1xN: X axis horizontal, one line, uneven spacing kept
        it.x = axis_of("Load index", &["6", "12", "100"]);
        let ld = line_data(&it, &lay(1, 3), &[1, 2, 3]).unwrap();
        assert_eq!((ld.x.clone(), ld.x_title.as_str(), ld.index_fallback.clone()), (vec![6., 12., 100.], "Load index", None));
        assert_eq!(ld.lines, vec![vec![1, 2, 3]]);
        assert!(ld.names.is_empty());
        // Nx1: Y axis horizontal (RPM), one line
        it.y = axis_of("RPM", &["800", "1000", "6000"]);
        let ld = line_data(&it, &lay(3, 1), &[9, 8, 7]).unwrap();
        assert_eq!((ld.x, ld.x_title.as_str()), (vec![800., 1000., 6000.], "RPM"));
        assert_eq!(ld.lines, vec![vec![9, 8, 7]]);
        // NxM row-major: rows horizontal, one line per column, named by X labels
        it.x = axis_of("Load index", &["11", "22"]);
        let ld = line_data(&it, &lay(3, 2), &[1, 2, 3, 4, 5, 6]).unwrap();
        assert_eq!(ld.lines, vec![vec![1, 3, 5], vec![2, 4, 6]]);
        assert_eq!((ld.names.clone(), ld.name_title.as_str()), (vec!["11".into(), "22".into()], "Load index"));
        assert_eq!(ld.x_title, "RPM"); // Y units, not load
        // unusable axis: explicit sample-index fallback, never relabelled
        it.y = axis_of("RPM", &["800", "800", "6000"]);
        let ld = line_data(&it, &lay(3, 1), &[9, 8, 7]).unwrap();
        assert_eq!((ld.x, ld.x_title.as_str()), (vec![0., 1., 2.], "sample index"));
        assert!(ld.index_fallback.unwrap().contains("increasing"));
        it.y = None;
        assert!(line_data(&it, &lay(3, 1), &[9, 8, 7]).unwrap().index_fallback.unwrap().contains("no Y axis"));
        // 1x1 and bad cell counts are errors
        assert!(line_data(&it, &lay(1, 1), &[1]).unwrap_err().contains("single value"));
        assert!(line_data(&it, &lay(2, 2), &[1]).is_err());
    }

    #[test]
    fn imports_dedupe_validate_and_keep_state() {
        let dir = std::env::temp_dir().join(format!("rallye-import-{}", std::process::id()));
        let (a, b) = (dir.join("a"), dir.join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("one.BIN"), [1u8, 2]).unwrap();
        fs::write(b.join("one.BIN"), [9u8, 9, 9]).unwrap(); // same name, other folder
        fs::write(b.join("notes.txt"), "x").unwrap();
        fs::write(b.join("empty.bin"), "").unwrap();
        fs::write(b.join("bad.xdf"), "<foo/>").unwrap();
        fs::write(b.join("ok.xdf"), XDF).unwrap();
        let mut inv = scan_dir(&a).unwrap();
        let original = inv.dumps[0].bytes.clone();
        // new file from another folder is appended and distinguished by path
        assert_eq!(inv.import_dump(&b.join("one.BIN")), Ok((1, true)));
        assert_eq!(inv.dumps[1].bytes, [9, 9, 9]);
        assert_ne!(inv.dumps[0].path, inv.dumps[1].path);
        // already loaded (also via a non-canonical spelling): selects existing, bytes untouched, no growth
        assert_eq!(inv.import_dump(&a.join("..").join("a").join("one.BIN")), Ok((0, false)));
        assert_eq!(inv.import_dump(&b.join("one.BIN")), Ok((1, false)));
        assert_eq!((inv.dumps.len(), inv.dumps[0].bytes.clone()), (2, original));
        // wrong extension / empty / missing: error, inventory unchanged
        for bad in [b.join("notes.txt"), b.join("empty.bin"), b.join("missing.bin")] {
            assert!(inv.import_dump(&bad).is_err(), "{}", bad.display());
        }
        assert!(inv.import_dump(&b.join("ok.xdf")).is_err());
        assert_eq!(inv.dumps.len(), 2);
        // XDF: wrong extension, parse failure, ok, duplicate
        assert!(inv.import_xdf(&b.join("one.BIN")).unwrap_err().contains(".xdf"));
        assert!(inv.import_xdf(&b.join("bad.xdf")).is_err());
        assert!(inv.xdfs.is_empty());
        assert_eq!(inv.import_xdf(&b.join("ok.xdf")), Ok((0, true)));
        assert_eq!(inv.import_xdf(&b.join("ok.xdf")), Ok((0, false)));
        fs::write(b.join("ok.xdf"), "broken XML").unwrap();
        assert!(inv.import_xdf(&b.join("ok.xdf")).is_err());
        assert_eq!(inv.xdfs.len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_unsupported_layouts() {
        let x = xdf();
        let mut t = x.items[0].clone();
        let set = |t: &mut Item, k: &str, v: &str| {
            t.embedded_attrs.retain(|(a, _)| a != k);
            t.embedded_attrs.push((k.into(), v.into()));
        };
        set(&mut t, "mmedmajorstridebits", "16");
        assert!(layout(&t, &x, 0x100).unwrap_err().contains("stride"));
        let mut t = x.items[0].clone();
        set(&mut t, "mmedtypeflags", "0x05");
        assert!(layout(&t, &x, 0x100).unwrap_err().contains("0x05"));
    }

    #[test]
    fn bounds_and_overflow() {
        let x = xdf();
        let t = &x.items[0];
        // table ends exactly at 0x16: fits in 0x16, not in 0x15
        assert!(layout(t, &x, 0x16).is_ok());
        assert!(layout(t, &x, 0x15).unwrap_err().contains("out of range"));
        let mut x2 = x.clone();
        x2.base.offset = 0x21;
        assert!(layout(t, &x2, 0x100).unwrap_err().contains("underflows"));
        x2.base = BaseOffset { offset: u64::MAX, subtract: false, present: true, raw_attrs: vec![] };
        assert!(layout(t, &x2, 0x100).unwrap_err().contains("overflows"));
        let mut t2 = t.clone();
        t2.embedded_attrs.retain(|(a, _)| a != "mmedrowcount");
        t2.embedded_attrs.push(("mmedrowcount".into(), format!("{}", u64::MAX)));
        assert!(layout(&t2, &x, 0x100).unwrap_err().contains("overflows"));
    }

    #[test]
    fn malformed_xdf() {
        assert!(parse_xdf(Path::new("x"), "<XDFFORMAT><XDFHEADER>").unwrap_err().contains("never closed"));
        assert!(parse_xdf(Path::new("x"), "<foo/>").unwrap_err().contains("XDFFORMAT"));
        assert!(parse_xdf(Path::new("x"), "<XDFFORMAT></XDFHEADER>").is_err());
    }

    #[test]
    fn cell_delta_extremes() {
        assert_eq!(cell_delta(255, 0, false), 255);
        assert_eq!(cell_delta(0, 255, false), -255);
        assert_eq!(cell_delta(0x7F, 0x80, true), 255); // 127 - (-128)
        assert_eq!(cell_delta(0x80, 0x7F, true), -255);
        assert_eq!(cell_delta(0x80, 0x7F, false), 1);
        assert_eq!(cell_delta(9, 9, true), 0);
    }

    #[test]
    fn diff_different_lengths() {
        assert_eq!(diff(&[1, 2, 3], &[1, 9]), Diff { len_a: 3, len_b: 2, changed: 1, extra: 1 });
        assert_eq!(diff(&[], &[5]), Diff { len_a: 0, len_b: 1, changed: 0, extra: 1 });
    }

    #[test]
    fn export_refuses_existing_file() {
        let dir = std::env::temp_dir().join(format!("rallye-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("input.BIN");
        fs::write(&p, b"original").unwrap();
        assert!(write_new(&p, b"clobber").is_err());
        assert_eq!(fs::read(&p).unwrap(), b"original");
        let n = dir.join("new.json");
        write_new(&n, b"{}").unwrap();
        assert!(write_new(&n, b"again").is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn scan_reports_invalid_and_empty() {
        let dir = std::env::temp_dir().join(format!("rallye-scan-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert!(scan_dir(&dir).unwrap().dumps.is_empty());
        fs::write(dir.join("empty.bin"), b"").unwrap();
        fs::write(dir.join("ok.ORI"), [1u8, 2]).unwrap();
        let inv = scan_dir(&dir).unwrap();
        assert_eq!(inv.dumps.len(), 1);
        assert_eq!(inv.dumps[0].sha256, sha256_hex(&[1, 2]));
        assert!(inv.errors[0].contains("empty"));
        assert!(scan_dir(&dir.join("missing")).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
