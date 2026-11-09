# Rallye

Read-only desktop inspector (Rust + GPUI 0.2.2) for Peugeot 106 IAW8P EPROM dumps and a TunerPro XDF map definition.
It never writes to input files, never flashes or talks to an ECU, and never edits calibration data.

## Hardware identification (provenance)

- ECU: "Motorola 68HC11 / IAW8P16". Source: reported by the owner. Not verified from the dumps or from the board.
- Exact MCU suffix and software revision: unknown.
- No disassembly has been done, so none of the code paths or memory maps are verified.

## Setup and run (fish)

Requirements: a Rust toolchain (tested with 1.99) and the usual GPUI Linux libraries (xkbcommon, xcb, wayland, fontconfig, freetype, vulkan). All of these were already installed on the development machine.

```fish
cd ~/Work/rallye
make test        # core unit tests
make check       # cargo check + clippy -D warnings
make run         # GUI on the parent directory (..)
make run DIR=/path/to/dumps
make report      # headless text report on the parent directory
./target/release/rallye report .. --json /tmp/rallye.json --csv /tmp/rallye.csv
./target/release/rallye report .. --table "Spark advance map, table 1" --baseline "IAW8P.16 XSi 1.6 rom orginal.BIN"
```

`--json` and `--csv` only create new files. An existing path is refused, so an input dump can't be overwritten.

## What it does

- Lists every `.bin`, `.rom`, and `.ori` file in the directory with its size and SHA-256. Empty, oversized (>4 MiB), and unreadable files are listed as rejected.
- Parses the XDF tables and constants. It shows the title, the raw description, the raw `EMBEDDEDDATA` attributes (unknown ones included), the flags, the units, the raw equation (never executed), and the axis labels. All of this is labelled "XDF claim, not firmware proof".
- Starts on the table titled "Injection, part throttle". It is looked up by title in the XDF, not by a hard-coded address.
- Address mapping is shown as a **file-offset hypothesis**: XDF address ± `BASEOFFSET`. You have to click to view bytes under that hypothesis.
- Table cells can be shown as hex, unsigned, or signed, with a heatmap. Cells that differ from the selected baseline dump get an orange outline.
- Shows size, bounds, unsigned and signed min/max, the number of cells ≥ 0x80, and the number of changed table cells and whole-file bytes against the baseline.

## Limitations

- Signedness is **unresolved**. Both interpretations are always shown. Nothing is inferred from data smoothness or from XDF comments. For example, the XDF says the part-throttle map is "Confirmed signed-byte storage from ECM display". That is a claim, not proof.
- No disassembly, no verified runtime memory map, and no engineering scaling.
- Cell view supports only contiguous 8-bit, row-major tables with `mmedtypeflags` ⊆ `0x01` and zero strides. Everything else is rejected with a reason. The 16-bit rev-limiter scalars show metadata and raw bytes only.
- The XDF is described as Group N. In `IAW8P.16 XSi 1.6 rom orginal.BIN`, the bytes at the same offsets look different, so do not assume the XDF layout applies to every dump.
- No folder picker. Pass a directory on the command line, then use Reload.
- Any future tuning work must wait for evidence: verified MCU and mapping, then verified signedness and scaling.

Proprietary dumps and XDFs are git-ignored and are not part of this repository.
