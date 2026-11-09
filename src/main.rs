mod analysis;
mod gui;

use analysis::*;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
rallye — read-only IAW8P EPROM / XDF inspector

USAGE:
  rallye [DIR]                       open the GUI on DIR (default: ..)
  rallye report [DIR] [options]      headless text report on stdout
OPTIONS (report):
  --xdf PATH        XDF to use (default: first *.xdf in DIR)
  --table TEXT      XDF table title or uniqueid (default: \"Injection, part throttle\")
  --baseline NAME   dump file name used as diff baseline (default: first dump)
  --json PATH       also write the JSON report to a NEW file (never overwrites)
  --csv PATH        also write per-cell CSV to a NEW file (never overwrites)
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.first().map(String::as_str) == Some("report") {
        return match report(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }
    if args.len() > 1 || args.first().is_some_and(|a| a.starts_with('-')) {
        eprint!("{USAGE}");
        return ExitCode::FAILURE;
    }
    gui::run(PathBuf::from(args.first().map(String::as_str).unwrap_or("..")));
    ExitCode::SUCCESS
}

/// Pick the XDF: explicit path, else first in the inventory.
pub fn pick_xdf(inv: &Inventory, explicit: Option<&str>) -> Result<Xdf, String> {
    match explicit {
        Some(p) => load_xdf(&PathBuf::from(p)),
        None => load_xdf(inv.xdfs.first().ok_or_else(|| format!("no *.xdf file in {}", inv.dir.display()))?),
    }
}

fn report(args: &[String]) -> Result<(), String> {
    let mut dir = None;
    let (mut xdf_p, mut table, mut base, mut json, mut csv) = (None, None, None, None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let slot = match a.as_str() {
            "--xdf" => &mut xdf_p,
            "--table" => &mut table,
            "--baseline" => &mut base,
            "--json" => &mut json,
            "--csv" => &mut csv,
            s if s.starts_with('-') => return Err(format!("unknown option {s}\n{USAGE}")),
            s if dir.is_none() => {
                dir = Some(s.to_string());
                continue;
            }
            s => return Err(format!("unexpected argument {s}")),
        };
        *slot = Some(it.next().ok_or_else(|| format!("{a} needs a value"))?.clone());
    }
    let inv = scan_dir(&PathBuf::from(dir.as_deref().unwrap_or("..")))?;
    if inv.dumps.is_empty() {
        return Err(format!("no usable .bin/.rom/.ori dumps in {} ({} rejected)", inv.dir.display(), inv.errors.len()));
    }
    let xdf = pick_xdf(&inv, xdf_p.as_deref())?;
    let ix = match &table {
        Some(t) => xdf.items.iter().position(|i| i.title.trim() == t.trim() || i.uniqueid.eq_ignore_ascii_case(t)),
        None => default_item(&xdf),
    }
    .ok_or_else(|| format!("table {:?} not found in XDF", table.as_deref().unwrap_or(DEFAULT_TABLE_TITLE)))?;
    let item = &xdf.items[ix];
    let baseline = match &base {
        Some(n) => Some(inv.dumps.iter().find(|d| &d.name == n).ok_or_else(|| format!("baseline {n:?} is not a loaded dump"))?),
        None => inv.dumps.first(),
    };

    println!("RALLYE REPORT (read-only)");
    for w in WARNINGS {
        println!("  ! {w}");
    }
    println!("\nDirectory: {}", inv.dir.display());
    for d in &inv.dumps {
        println!("  {:<40} {:>7} B  sha256 {}", d.name, d.bytes.len(), d.sha256);
    }
    for e in &inv.errors {
        println!("  REJECTED: {e}");
    }
    println!("\nXDF: {} — {:?}", xdf.path.display(), xdf.title);
    println!("  BASEOFFSET: present={} offset=0x{:X} subtract={} raw={:?}", xdf.base.present, xdf.base.offset, xdf.base.subtract, xdf.base.raw_attrs);
    println!("  Items ({}):", xdf.items.len());
    let len0 = inv.dumps[0].bytes.len() as u64;
    for i in &xdf.items {
        let st = match layout(i, &xdf, len0) {
            Ok(l) => format!("8-bit {}x{} @0x{:X}", l.rows, l.cols, l.offset),
            Err(e) => format!("not viewable: {e}"),
        };
        println!("    [{}] {:<45} {st}", i.uniqueid, i.title.trim());
    }
    println!("\nTable: {:?} (uniqueid {}) — {PROVENANCE}", item.title.trim(), item.uniqueid);
    println!("  description (raw): {}", item.description);
    println!("  equation (raw, not executed): {:?}   units: {:?}   item flags: {:?}", item.equation, item.units, item.item_flags);
    println!("  EMBEDDEDDATA (raw): {:?}", item.embedded_attrs);
    for (n, a) in [("x", &item.x), ("y", &item.y)] {
        if let Some(a) = a {
            println!("  {n} axis: units {:?}, indexcount {:?}, labels {:?}", a.units, a.indexcount, a.labels);
        }
    }
    println!("  Signedness: UNRESOLVED (XDF typeflag claim: {})", if layout(item, &xdf, len0).is_ok_and(|l| l.xdf_claims_signed) { "signed" } else { "unsigned/none" });
    if let Some(b) = baseline {
        println!("  Baseline: {}", b.name);
    }
    for d in &inv.dumps {
        print!("\n  {}: ", d.name);
        match layout(item, &xdf, d.bytes.len() as u64) {
            Err(e) => println!("UNSUPPORTED/ERROR: {e}"),
            Ok(l) => {
                let c = cells(&d.bytes, &l);
                let s = stats(c).unwrap();
                println!("{}", hypothesis_text(&l, &xdf.base));
                println!("    size {} B ({}x{}), bounds 0x{:X}..0x{:X} of 0x{:X}", l.len(), l.rows, l.cols, l.offset, l.end(), d.bytes.len());
                println!("    unsigned min/max {}/{}   signed min/max {}/{}   cells >=0x80: {}", s.min_u, s.max_u, s.min_s, s.max_s, s.high_bit);
                if let Some(b) = baseline {
                    let whole = diff(&d.bytes, &b.bytes);
                    let tbl = layout(item, &xdf, b.bytes.len() as u64).map(|bl| diff(c, cells(&b.bytes, &bl)).changed.to_string());
                    println!(
                        "    vs baseline: table cells changed {}   whole file bytes changed {} (+{} length difference)",
                        tbl.unwrap_or_else(|e| format!("n/a ({e})")), whole.changed, whole.extra
                    );
                }
            }
        }
    }
    println!("\n  (full unsigned + signed cell grids: --json / --csv)");
    if let Some(p) = json {
        let v = report_json(&inv, &xdf, item, baseline);
        write_new(&PathBuf::from(&p), serde_json::to_string_pretty(&v).unwrap().as_bytes())?;
        println!("wrote {p}");
    }
    if let Some(p) = csv {
        write_new(&PathBuf::from(&p), report_csv(&inv, &xdf, item, baseline).as_bytes())?;
        println!("wrote {p}");
    }
    Ok(())
}
