# Rallye

Desktop inspector (Rust + GPUI 0.2.2) for Peugeot 106 IAW8P EPROM dumps and a TunerPro XDF map definition.
It only reads input files and never writes them, never flashes or talks to an ECU. The GUI lets you edit table cells in an in-memory session working copy (double-click a cell). Edits are not saved or exported; the `report` command always analyses the original files.

## Hardware identification (provenance)

- ECU: "Motorola 68HC11 / IAW8P16". Source: reported by the owner. Not verified from the dumps or from the board.
- Exact MCU suffix and software revision: unknown.
- The original firmware runs from reset on a patched software CPU with a research board model. Under the labelled simulation profile H2 (named board hypotheses and supplied raw inputs) it reaches the foreground main loop and keeps running it with timer interrupts, in bounded chunks that continue exactly from checkpoints, until Pause. A commanded crank speed (simulated 60-2 wheel) and a throttle stimulus can change while it runs. The firmware decodes the teeth and computes its own speed, shown next to the commanded one. The interpolation routine also runs in isolation. The board memory map, MCU suffix, clock and every input's physical meaning remain unverified; see [firmware emulation research](docs/firmware-emulation.md).

Reference label photograph supplied by the owner on 9 November 2026:

- Visible markings: Magneti Marelli `IAW 8P.16`, `9630699180`, `12V`, and `16308.214`.
- The marking `274 97` is visible. The owner suspects a 1997 unit; the date-code convention has not been verified.
- Investigation remains focused on the 1995 application. This photograph is reference evidence only: it has not been linked to either owned ECU or any supplied dump, and does not establish firmware compatibility, MCU suffix, or table signedness.

## Setup and run (fish)

Requirements: a Rust toolchain (tested with 1.99) and the usual GPUI Linux libraries (xkbcommon, xcb, wayland, fontconfig, freetype, vulkan). All of these were already installed on the development machine.

```fish
cd ~/Work/rallye
make test        # Rust unit tests + tools/ Python tests (simulator tests skip without UCSIM/dumps)
make check       # cargo check + clippy -D warnings
make run         # GUI on the parent directory (..)
make run DIR=/path/to/dumps
make emulator-setup  # optional: pinned µCsim for the Emulation tab, built in target/ucsim
make report      # headless text report on the parent directory
./target/release/rallye report .. --json /tmp/rallye.json --csv /tmp/rallye.csv
./target/release/rallye report .. --table "Spark advance map, table 1" --baseline "IAW8P.16 XSi 1.6 rom orginal.BIN"
```

`--json` and `--csv` only create new files. An existing path is refused, so an input dump can't be overwritten.

## What it does

- Lists every `.bin`, `.rom`, and `.ori` file in the directory with its size and SHA-256. Empty, oversized (>4 MiB), and unreadable files are listed as rejected.
- Parses the XDF tables and constants. It shows the title, the raw description, the raw `EMBEDDEDDATA` attributes (unknown ones included), the flags, the units, the raw equation (never executed), and the axis labels. All of this is labelled "XDF claim, not firmware proof".
- Starts on the table titled "Injection, part throttle". It is looked up by title in the XDF, not by a hard-coded address.
- Address mapping is shown as a **file-offset hypothesis**: XDF address ± `BASEOFFSET`. The viewer shows the bytes immediately with that assumption labelled.
- Sidebar: **Target dump**, **Baseline dump** (with `None`) and **XDF definition** are dropdown selectors (one open at a time, full file names, size and short SHA-256 below). Tables/scalars are a dense scrollable list. Reload keeps the selected dumps, XDF and table by path/uniqueid when they still exist, otherwise falls back to defaults.
- **File choosers** (native dialog, xdg-desktop-portal on Linux): a **Browse…** button beside Target, Baseline and XDF adds one file from any folder to the list and selects it; **Open folder…** in the header switches the whole folder. They work even when the current folder is empty or unreadable. Wrong extensions (dumps: `.bin/.rom/.ori`, XDF: `.xdf`), empty, oversized, unreadable and unparsable files are refused with a visible message and change nothing. A file that is already loaded (same canonical path) is just selected: its bytes and session edits are never replaced, and the baseline stays the original. Same-named files from different folders show their folder in the lists. Open folder scans first, refuses a folder with no dump and no XDF, and asks before discarding session edits (Cancel keeps the old folder and edits). Only one dialog can be open at a time. Explicitly added files are kept on Reload (a missing one is reported, then dropped); opening a different folder drops them and says so.
- **Compare baseline** toggle (default on): each cell shows the target value, `B` baseline value and `Δ` = target − baseline as signed decimal (signed mode uses i8 values; unsigned and hex use u8 values; hex mode shows values in hex, Δ stays decimal). With no baseline, or one whose table is out of range, it says comparison is unavailable instead of showing zeros. The 2D and 3D plots show the target only.
- Table cells can be shown as hex, unsigned, or signed, with a heatmap. Cells that differ from the selected baseline dump get an orange outline.
- The plot card offers **Auto / 2D / 3D / Both**. Auto stacks 2D above 3D when there is enough room, otherwise opens in 3D when a surface is possible or 2D for curves (for example 24×1 or 1×17); your choice resets when you pick another table. **2D** is a native line chart of the target working bytes in the selected Hex/Unsigned/Signed view: for a 1×N curve the X axis is horizontal with one line, for N≥2 rows the Y axis is horizontal with one line per column. Horizontal positions are proportional to the numeric XDF axis labels, titled with the XDF axis units verbatim (never relabelled as RPM). If the labels are unusable the chart says why and uses a sample index. A 1×1 table is a single value and draws no line.
- Draws an interactive **3D surface** of the target table beside the grid (below it when the window is too narrow): RPM and load on the floor, raw value as height, shaded patches with a mesh on top and a low-to-high colour scale. RPM and load positions are proportional to the XDF Y- and X-axis label values, so uneven spacing stays uneven. The load axis is named with the XDF X-axis units verbatim (for example "Load index"), never as %. The value axis has numeric ticks, and a `0` tick plus a zero-level outline when signed values cross zero. Drag with the left mouse button inside the graph to rotate (pitch is clamped), use the mouse wheel over the graph to zoom (clamped; the page does not scroll while the pointer is on the graph), and **Reset view** restores the default angle. Releasing the button or leaving the graph ends the drag.
- The surface needs at least 2×2 cells, a Y axis whose units say RPM, and complete, indexed (`LABEL index`), finite, strictly increasing labels on both axes. Otherwise it says why it is unavailable (for example single-column tables) and never falls back to row or column indexes; the value grid stays usable. Axis labels are XDF claims. The mesh only joins samples visually: ECU interpolation is not verified, and the 3D shape does not prove signedness.
- The display mode (Hex / Unsigned / Signed) also drives the surface height. **Unsigned is the default as a viewer preference only**: it is not verified firmware semantics and is not inferred from the XDF. Signed shows the same bytes as i8. Hex mode plots unsigned raw magnitude (hex is not numeric semantics). The XDF's own signed claim is shown separately.
- Shows size, bounds, unsigned and signed min/max, the number of cells ≥ 0x80, and the number of changed table cells and whole-file bytes against the baseline.

## Limitations

- Signedness is **unresolved**. Both interpretations are always shown. Nothing is inferred from data smoothness or from XDF comments. For example, the XDF says the part-throttle map is "Confirmed signed-byte storage from ECM display". That is a claim, not proof.
- No verified full runtime memory map or engineering scaling. The [firmware probe](docs/firmware-emulation.md) provides file-specific evidence of unsigned interpolation on one path; it does not yet establish complete table semantics or change the viewer's interpretation.
- Cell view supports only contiguous 8-bit, row-major tables with `mmedtypeflags` ⊆ `0x01` and zero strides. Everything else is rejected with a reason. 16-bit tables stay metadata-only.
- **16-bit scalars** (the Rev Limiter XDFCONSTANTs) are shown as a read-only single value: raw bytes in file order, the word decoded big- or little-endian from the XDF flags (`mmedtypeflags` 0x02 / DEFAULTS `lsbfirst`), target vs the original baseline and their difference, in the chosen Hex / Unsigned (u16) / Signed (i16) view. Unknown flag bits, float, strides, other dimensions and out-of-range addresses are rejected with a reason. If the equation is exactly `15000000/X` with RPM units and no signed claim, an "XDF-derived RPM (unverified)" line is shown (raw 0 shows unavailable); no other equation is ever evaluated. 16-bit values cannot be edited: the editor stays byte-based and only for 8-bit table cells.
- The XDF is described as Group N. In `IAW8P.16 XSi 1.6 rom orginal.BIN`, the bytes at the same offsets look different, so do not assume the XDF layout applies to every dump.
- The file chooser only validates by extension and content; it cannot filter the dialog by type.
- Any future tuning work must wait for evidence: verified MCU and mapping, then verified signedness and scaling.
- The Emulation tab is not a running-engine simulation. Strict mode stops after 20 instructions at the external write `$1040`; H1 stops after 68,202 instructions at the unknown board input `$1060`. Only the H2 simulation profile, built on named hypotheses and supplied raw inputs, reaches the main loop. Its crank signal is a commanded stimulus on the pins the firmware configures (wiring unverified), and its throttle channel AN2 is a traced candidate. There is no engine model and no mapped injector or ignition output.

Proprietary dumps and XDFs are git-ignored and are not part of this repository.

Build reproducibility: keep `Cargo.lock`. It pins `libc` to 0.2.189 because GPUI 0.2.2's `xattr` dependency fails with 0.2.191. All Makefile targets use `--locked`.

## Emulation tab (research, not an ECU simulation)

The **Engine inputs** card (H2) has **Start/Stop crank**, a commanded speed (30–9000 rpm, presets
250/900/3000/6000) and a throttle stimulus in percent. The percentage is written as a raw value to
an A/D channel: AN2 by default, the candidate from the firmware trace, with endpoints from the
calibration. Other channels and endpoints can be selected. Next to them the card shows the
firmware's own readings from RAM: speed `$D1` only while its period `$BB` is valid, the tooth
counter, regime and gap flags, AN2 copy/validated/processed values, learned endpoints, fault
flags, and small plots of commanded vs firmware speed and throttle input vs `$C6`. Requested
values are stimuli; measured values are what the firmware computed.

The header switches between **Maps** and **Emulation**. Emulation keeps the sidebar, binds to the selected **target** dump and works without an XDF. It is a view of the [firmware emulation research](docs/firmware-emulation.md), not a booted ECU and not an engine model.

Setup: `make emulator-setup` (needs git, a C++ compiler, flex, bison and network access once) fetches µCsim at the pinned commit into `target/ucsim`, applies `tools/ucsim-hc11-probe.patch` and builds only the HC11 simulator; nothing is installed system-wide. `make run` uses that build; set `UCSIM=/path/to/ucsim_m68hc11` to override it. Without a simulator the tab shows the exact `make -C … emulator-setup` command. `python3` must be on PATH.

- **Supported files only:** `106XSI2.ORI` (SHA-256 `e91f72ee…`) and `REMAP MILD EURO95 RUNNING.BIN` (`081bc0ed…`). Any other hash shows "unsupported" and nothing runs.
- **Firmware from reset:** **▶ Run**, **‖ Pause**, **Step 1 instruction** and **Reset**, with a **board profile** switch (Strict, H1, H2). A session runs in requests of 2,000,000 instructions; each request starts a fresh µCsim process that loads the previous request's checkpoint (complete CPU, RAM and board state), so Run continues indefinitely until Pause, Reset, an error or a blocker, with bounded work, output and memory per request. Continuing from a checkpoint is tested to give exactly the same state as one long run. The checkpoint is bound to the source hash, profile and inputs; the backend refuses it for any other. Execution stops **before** the first instruction that needs hardware the board model does not implement, or whose opcode has no conformance case; that instruction is never part of the shown state.
  - **Strict** uses only HC11 behaviour from the reference manual (register block at the reset location: INIT, OPTION, TMSK2, PORTD/DDRD, HPRIO, ADCTL, COPRST, timer flags, TCNT, PPROG, SCI control) and regions inferred from the firmware itself (internal RAM `$0000–$00FF`, external RAM `$2000–$27FF` from SP=`$27FF` and the RAM test, EPROM image `$4000–$FFFF`). On both files it executes 20 instructions, past INIT and the documented register setup, and stops before `$B834  STA A,$1040`: the board's external I/O is unknown.
  - **H1 hypothesis** (shown with a warning, never called a validated ECU): `$1040`/`$1050` writes are write-only latches with unknown, unmodelled effect; an EPROM-address write of the byte already there is dropped (the copy loop `$8000–$9338` writes back what it read, which is indistinguishable for an ignoring EPROM or a shadow RAM); the diagnostic serial line is idle. On both files it runs 68,202 instructions (235,893 E cycles, ≈118 ms at the assumed 2 MHz E clock): copy loop, RAM signature check, external RAM test, internal-EEPROM block check (reads the dump's zero bytes at `$B600–$B7FF`), four COP services and SCI setup, then stops before `$A86A  BRSET $60,X,#$20` because the board input `$1060` is unknown.
  - **H2 simulation profile** (warning-framed; engine stopped): H1 plus HC11 interrupt delivery with the IRQ/XIRQ pins inactive, `$1040`/`$1050` read back the last written value, PORTA output-compare pins, SPI master and SCI transmitter timing, the internal EEPROM at `$B600–$B7FF` (programming sequence and 10 ms checked), and **supplied raw inputs**: board input bytes `$1060` and `$1080` as bit toggles (bit 5 of `$1060` selects the `$A86A` branch), PORTA and PORTD input pins, A/D channels AN0–AN7 (raw 0–255) and the EEPROM's power-up fill byte. Defaults are explicit hypotheses; no channel is labelled as a sensor. Data received over SPI or SCI stays unknown: the firmware may only load and discard it. Inputs change at the next chunk boundary without a reset (only the EEPROM fill resets it); the raw controls are collapsed under **Show raw inputs**. On both files it reaches `$D4A2` at instruction 70,045 and keeps running: in 2,000,000 instructions (≈4.1 s virtual) 424 main-loop passes, 961 RTI and 30 timer-overflow interrupts entered and returned, foreground periodic work once per timer-overflow flag, SP at the loop head always `$27FF`.
  - Visuals: a status line (running, paused, blocked) that never moves the buttons; boot landmarks (reached, current, blocked), the blocking instruction and reason; a scheduler panel with main-loop passes, SP at the loop head, interrupt entries per source and RTI returns, executions at static landmarks and COP service gaps; raw outputs (PORTA pins with change counts, SPI and SCI bytes, EEPROM operations); RAM and EEPROM heat grids; a PC timeline of the last 1,024 samples of the session over address-region bands; a memory-map bar with fetch/read/write counts per region and the blocked region in red; a 16 × 16 code-activity grid of instructions per 256-byte page; the modelled HC11 registers with value, bits and access counts plus TCNT and COP service timing; the latest peripheral events; and collapsible raw details (all events, PC samples, board and core text). Virtual time comes from per-opcode E-cycle counts; milliseconds assume 2 MHz.
  - Initial A/B/X/Y and RAM are µCsim's zero power-on values (undefined on real hardware); CC starts at `$D0` (S, X, I set as after reset). The run uses the original source bytes; session edits are not used, but any edit resets the session.
- **Isolated lookup:** runs the original interpolation routine `$B1C5..$B217` once against a constructed RAM descriptor and stops before its `RTS` at `$B218`. A 2 × 2 heat grid shows the four cells with the interpolation point and result. Controls: row 0–22, column 0–7, row fraction and column fraction 0–255 (/256). These are table coordinates and byte fractions, **not** RPM, throttle, load or sensor units. Shown: the raw unsigned result (decimal and hex), the four cell addresses and bytes, the instruction count, registers, and source/working SHA-256. The simulator result must equal the arithmetic recovered from disassembly, or it is rejected. Session edits are used only when every edited byte of the target lies inside the table region `0x91AB..0x9282`; with any edit elsewhere the lookup is refused (never silently mixed).
- **Engine outputs:** injector pulse width, ignition timing and RPM are **not available**: there is no crank input, and no output pin or SPI byte has been mapped to an injector or coil, so no waveform or dial is drawn. Raw PORTA pins, SPI/SCI bytes and the lookup result are shown as such.
- Requests run in the background (no UI blocking) through `python3 -I` with the embedded `tools/firmware_probe.py --bridge` and a strict JSON protocol. Rust caps each captured output stream at 4 MiB, applies a 60-second bridge timeout and kills the bridge when a session is reset or replaced. Python gives each simulator invocation 20 seconds, feeds commands from a file and rejects simulator output over 4 MiB before reading it back. Every simulator process also runs 202 conformance cases derived from the HC11 manual (flags, results, memory and E cycles; 172 opcodes plus interrupt entry/return scenarios) and fails closed if any differs. No shell is used. Run and lookup results belong to the exact target and entire working copy, including edits outside the table, and a run also to its profile and inputs. Target changes and reloads invalidate pending requests; other byte, profile or input changes discard a pending result (with a notice) and reset the session. Malformed or failing backend output is shown as an error, never as a result.

## Session edits (GUI)

- Double-click a raw table cell to edit it in place. The toolbar shows its coordinates, range and any validation error. Enter applies; Esc or clicking elsewhere cancels. Signed accepts -128..127, unsigned 0..255, hex 00..FF with optional `0x`. Anything else is rejected with a visible message; values are never clamped or wrapped. Only 8-bit cells are editable.
- Each target dump has its own working copy (kept while you switch target, XDF or table). Undo/Redo apply to the current target; no-op edits create no history; history is bounded to 1000 edits. The baseline is always the original, unedited file, so editing the target against itself shows deltas.
- The sidebar SHA-256 is the source file hash, not the working copy. A dump marked ● has edits that differ from its original.
- Edits live in memory only ("session edits · not saved"). Reload and closing the window ask before discarding them; killing the process cannot be intercepted.

## Inline editing and plot layout

Double-click a table cell to edit its value in place. Enter applies to the session working copy; Escape or clicking elsewhere cancels the draft. Left/Right move the caret; Home/End jump to either end. Typing and pasting insert at the caret; Backspace/Delete remove the preceding/following character. Ctrl+A selects the whole value. Invalid values stay in the editor with an error beside the grid controls; Undo/Redo and original-file protection are unchanged.

The plot selector offers **Auto / 2D / 3D / Both**. Auto stacks 2D above 3D when both exist and the viewport has space beside the grid and at least 1100 pixels of height. Smaller windows keep a single plot; **Both** explicitly stacks the views regardless of window size. Single-axis curves still use 2D, and scalars remain value panels.
