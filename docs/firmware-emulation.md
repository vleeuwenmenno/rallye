# Firmware emulation investigation — 9 November 2026

## Result

**Update, 10 November 2026 (later):** under the labelled simulation profile H2 (named
board hypotheses, supplied raw inputs, engine stopped), both supported files reach the
foreground loop `$D4A2` and keep running it with RTI and timer-overflow interrupts entered
and returned, in chained checkpoint requests with no total cap (see
[Continuous engine-stopped run](#continuous-engine-stopped-run-simulation-profile-h2-10-november-2026)).
Strict mode still stops after 20 instructions at the external write `$1040`; H1 after
68,202 at a read of board input `$1060`. This is **an engine-stopped scheduler, not a
running-engine simulation**.

The original investigation (below) ran five startup instructions and separately invoked
the lookup routine with constructed RAM inputs. Live throttle/RPM controls should still
follow a verified board model rather than invent sensor-to-map conversions.

No source dump, XDF, physical ECU, EEPROM, or immobilizer state was modified.
No private firmware was uploaded. The GUI later gained an **Emulation** tab that
shows the board-model run and the lookup interactively (see the README); it adds
no CPU model of its own.

## How the EPROM is used

In expanded mode, an HC11 can fetch instructions and data directly over its
external address/data bus. Board address decoding selects the external memory;
the processor does not need to copy the entire EPROM into RAM first. The normal
reset vector is a two-byte address at CPU addresses `0xFFFE–0xFFFF`. Internal
memory and mode configuration determine which device supplies an access.
These are CPU-family facts, not proof of this ECU's board wiring.
([Motorola/NXP reference manual](https://www.nxp.com/docs/en/reference-manual/M68HC11RM.pdf),
sections 2.6, 3.6 and 5.3.)

An XDF is a PC-side interpretation of bytes. The examined code instead uses
numeric addresses, RAM variables and lookup routines; no XDF is involved in
that execution.

## Evidence from the supplied files

The following detailed addresses apply to `106XSI2.ORI` and
`REMAP MILD EURO95 RUNNING.BIN` only:

- XSI2 SHA-256: `e91f72ee8e80068b523fcff4c2bb5ecf0ae3f8df6cfe3ec31808133e215d5663`.
- Mild SHA-256: `081bc0ede46963f101bab5e6a0a69e9a7babb11bb1122928a815c8f1032da725`.
- Both are 65,536 bytes. Bytes at offsets `0xFFFE–0xFFFF` are `B8 00`.
- Decoding at file offset `0xB800` produces coherent startup code. Five original
  instructions execute to `0xB80E`; the stack pointer becomes `0x27FF`.
- The caller at `0x647E–0x64A4` and interpolator at `0xB1C5–0xB218` are identical
  in these two files. Interpolator SHA-256:
  `6b25c337bcc9308cc838ea93f0c51a4fca2db67b06be6613989d99ecb3a12cdc`.

**Evidence level:** raw bytes verified locally; instruction decoding checked with
installed Capstone `cstool m6811`. µCsim disassembly was used for display only;
additional incorrect entries remain outside the patched instruction. Executing a constructed
flat memory image supports the mapping hypothesis, but does not prove the full
board map, reset configuration, compatibility with a particular ECU, or all
paths' runtime reachability.

`106XSI3.ORI` also has reset candidate `0xB800`; its `LDD #$91AB` is at file
offset `0x63F6`, so its code addresses differ. The file named
`IAW8P.16 XSi 1.6 rom orginal.BIN` has a different layout at the examined
locations. Neither file is accepted by the probe. Filenames do not establish
firmware compatibility or a production year.

### The actual table reference

The bytes below were decoded, not merely found by searching for `91 AB`:

```text
647E  B6 20 13     LDAA $2023
6481  18 CE 21 A2  LDY  #$21A2       ; descriptor in RAM
6485  F6 92 BE     LDAB $92BE        ; contains 9
6488  18 E7 06     STAB 6,Y
648B  CE 92 B5     LDX  #$92B5       ; column breakpoints
648E  BD B2 72     JSR  $B272        ; index/fraction helper
6491  18 ED 00     STD  0,Y
6494  FC 20 3C     LDD  $203C        ; precomputed row index/fraction
6497  18 ED 02     STD  2,Y
649A  CC 91 AB     LDD  #$91AB       ; table base
649D  18 ED 04     STD  4,Y
64A0  BD B1 C5     JSR  $B1C5        ; interpolation
64A3  16           TAB
64A4  39           RTS
```

The descriptor contains column index/fraction at offsets 0/1, row index/fraction
at 2/3, a big-endian base pointer at 4/5, and column stride at 6. The interpolator
forms `base + row_index * stride + column_index`, reads neighboring bytes in two
rows, and interpolates first across columns and then between rows. Fractions
are byte fractions with denominator 256; each interpolation rounds the magnitude
of the difference before adding or subtracting it.

Stride 9 is confirmed for this caller. The row-index producer at `0xD64D–0xD661`
loads the word-axis base `0x92C2` and count 24 from `0x92F2`, passes RAM `$BB`
to helper `$B2A8`, and stores its index/fraction result at `$203C`.
The 24 descending big-endian words at `0x92C2–0x92F1` range from 27273 to 2000.
Applying `round(15000000 / word)` reproduces all 24 XDF RPM labels, including
3501, 4202 and 7003. The code reference and raw words are verified in both
supported dumps; the numerical match alone does not prove the timer clock,
sensor conversion or boundary behavior. Those still need tracing before RPM
can serve as an emulator input.
The probe uses base rows 0–22 and columns 0–7; neighbors reach row 23 and column 8.
It does not extrapolate or use the final row/column as the base of a pair.

The column breakpoints referenced by the firmware are the raw bytes
`00 03 0B 16 25 36 59 84 C9` at `0x92B5–0x92BD`. These are not the XDF's displayed
`11, 22, …, 100` labels. The physical meaning and scaling of `$2023` remain
unknown; do not call this axis throttle percent.

### Signedness: new evidence, limited scope

The table-specific interpolator subtracts neighboring bytes and uses `BCS`
(unsigned borrow) to choose the direction of each interpolation. It uses
`MUL; ADCA #0` for rounded magnitude scaling. Motorola documents this rounding
sequence in the MUL instruction description (printed page 560).

One decoded downstream caller additionally does:

```text
5FA1  BD 64 7E  JSR  $647E
5FA4  D7 CD     STAB $CD
5FA6  4F        CLRA          ; zero-extend B into D
5FA7  05        ASLD
5FA8  05        ASLD
5FA9  DD CB     STD  $CB      ; unsigned result multiplied by four
```

**Verified by local disassembly:** these instructions implement unsigned-byte
interpolation and zero-extension on this path. This is stronger evidence than
the XDF comment claiming signed storage. It does not establish pulse-width
units, final scaling, a later offset encoding, all consumers, or other firmware
revisions. No XDF settings or GUI semantics were changed by this investigation.

## Emulator evaluation and repeatable probe

[µCsim](https://github.com/danieldrotos/ucsim) was built locally at commit
`59a14ff99efe7e92474faf4c4ddf57b5e4ff0631` (reported version 0.9.12).
It is GPL-2.0-or-later. It remains an external research executable, not a Rust
dependency and not bundled: the GUI only starts it as a separate process.

Two defects were found in the examined revision:

1. `cl_m6800::lda` cleared C and retained stale Z. On the HC11, loads preserve C
   and recompute Z. This changed a tested interpolation result. The reference
   manual's LDA condition-code table (printed page 551) confirms the correction.
2. Its disassembly table listed `18 CE hh ll` (`LDY #imm16`) as three bytes
   instead of four, misaligning following instructions.

[The small local patch](../tools/ucsim-hc11-probe.patch) fixes these two issues.
The probe began with a carry/Z conformance check and fails if that check or
any lookup result fails; the board-model work below replaced that single check with 92
manual-derived cases and found six more defects. This is **not a general certification of µCsim**.
Other load/store helpers, reset flags, interrupt behavior and cycle accounting
need audit before wider execution. The probe explicitly initializes CC and
does not interpret simulator time as ECU time.

The HC11 class inherits a generic memory/peripheral setup, including unrelated
CIA/PIA devices around `0x8000`; it is unsuitable as an IAW8P board model without
further work. The isolated routine uses none of these peripheral addresses.
([Source at the tested revision](https://github.com/danieldrotos/ucsim/blob/59a14ff99efe7e92474faf4c4ddf57b5e4ff0631/src/core/motorola.src/m6800.cc).)

The alternative [MAME HC11 core](https://github.com/mamedev/mame/blob/master/src/devices/cpu/mc68hc11/mc68hc11.cpp)
has explicit A1 support and analog-input callbacks, but also timer and opcode
TODOs. It is a candidate to audit, not an already validated ECU emulator.

### Reproduce on this workspace

`make emulator-setup` fetches only the pinned commit into `target/ucsim`
(git-ignored), applies `tools/ucsim-hc11-probe.patch` once, runs
`./configure --enable-m68hc11-sim`, `make preps`, `make -j4 libs` and
`make -C src/sims/m68hc11.src -j4`. No system installation is required, and
rerunning it is a no-op. The Makefile then uses that build by default; an
`UCSIM` environment or command-line value overrides it. In fish:

```fish
cd ~/Work/rallye
make emulator-setup
make firmware-probe
make firmware-probe PROBE_DUMP='../REMAP MILD EURO95 RUNNING.BIN'
```

`tools/firmware_probe.py` accepts only the two identified source hashes, creates
an Intel HEX copy in a temporary directory, and runs bounded instruction counts
in a subprocess. Its JSON output now also contains a one-line summary of the strict
and H1 startup replays. It performs 13 lookups using original table bytes and six
synthetic boundary cases in emulator memory, restores synthetic cells, checks
the source hash again, and prints JSON. It stops before the routine's final RTS;
it does not pretend that a caller or sensor subsystem supplied the inputs.

**Checks performed:** both accepted dumps passed the LDA conformance check,
five-instruction startup stop, and all 19 lookup cases. A case with different
row and column fractions checks that the fraction inputs are not swapped.
The synthetic cases include crossing `0x7F/0x80`, plus ascending and descending
full-byte ranges on both axes.
The asserted results use arithmetic recovered from disassembly, not the XDF
formula. Original files remain unchanged.

## Continuous execution: the main loop is located

The following is **static disassembly evidence**, checked against both supported
source hashes. These instructions have not been reached by the startup replay.

```text
B943  0E           CLI              ; enable maskable interrupts
B944  7E D4 A2     JMP  $D4A2       ; enter foreground loop
D4A2  FC 10 0E     LDD  $100E       ; read timer counter
...
D4DC  86 55        LDAA #$55
D4DE  B7 10 3A     STAA $103A       ; watchdog service, on this path
D4E1  43           COMA            ; A becomes $AA
D4E2  B7 10 3A     STAA $103A
D4E5  12 B5 01 03  BRSET $B5,#1,$D4EC
D4E9  7E D5 C8     JMP  $D5C8       ; skip periodic work if flag absent
D4EC  15 B5 01     BCLR $B5,#1
...
D888  7E D4 A2     JMP  $D4A2       ; next foreground iteration
```

At offsets `$FFDE–$FFDF`, both dumps contain `74 3C`. The standard HC11 timer
overflow vector occupies these addresses. Its handler at `$743C` writes `$80`
to `$1025` (TFLG2), increments the word at `$210C`, and has a path through
`$7477: BSET $B5,#1`. This connects the timer interrupt to the foreground flag
check above. It does not establish the clock frequency or prove every handler
path reaches that flag update.

Register names, vector assignments and watchdog service semantics are from the
[Motorola/NXP reference manual](https://www.nxp.com/docs/en/reference-manual/M68HC11RM.pdf),
sections 5.4.3, 5.6 and 10.3. This is an interrupt-driven firmware structure:
repeatedly executing the foreground instructions without advancing peripherals
would miss scheduled work. A repeated PC alone is not proof of a booted ECU;
the fatal-stop path at `$B967` also branches to itself.

### The concrete next boot barriers

Startup writes INIT `$103D=$01`, then OPTION `$1039=$BA`, TMSK2 `$1024=$01`,
PORTD `$1008=$FF`, DDRD `$1009=$1A`, HPRIO `$103C=$21`, and ADCTL `$1030=$14`.
For the documented A-family map, INIT `$01` preserves the reset RAM/register
locations. That does not make the following timer, ADC and port writes ordinary
RAM writes. Their side effects and protected-write timing need a CPU/peripheral
model. See manual sections 3.5 and 12.

At `$B834/$B839`, writes to `$1040/$1050` are outside that register block.
At `$B846–$B85A`, the firmware reads and writes the same addresses from `$8000`
through `$9338`. This suggests board-specific memory behaviour, but does not
prove RAM shadowing or its control signal. The later RAM test at `$5BCA` starts
at `$2000`, with its upper bound derived from `$27FF`. These are concrete bus
requirements, not reasons to make the whole EPROM writable.

The µCsim core at the pinned revision also inherits generic CIA/PIA devices at
`$8000/$8008` and `$8010/$8020` from `cl_m6800::mk_hw_elements`. The patch now removes
them for the HC11 (see the board-model section below).

The first continuous-execution milestone should be **key on, engine stopped**:
reach `$D4A2` through unmodified startup, advance virtual time, observe timer
interrupts and watchdog servicing, and repeatedly return to the foreground loop
without unexplained accesses. Only then add a verified crank signal and sensor
channels. Injector/ignition traces also need output-driver mapping; no current
result establishes engine operation or a safe calibration.

## What a live ECU simulation still needs

1. **CPU and bus profile:** exact MCU suffix, clock, RAM, internal EEPROM/config,
   external RAM and address decoding. Startup sets SP to `0x27FF`, accesses
   `$1040/$1050`, and later copies `$8000…$9338` onto the same addresses. These
   require a board explanation; treating the entire dump as writable RAM hides
   the problem. A1's default register block alone does not explain every access.
2. **Peripheral model:** correct ADC timing, timers/input capture/output compare,
   interrupt priorities, watchdog and external I/O. Unsupported access must stop
   with its address and instruction rather than silently return a plausible zero.
3. **Sensor harness:** crank pulse train, throttle signal, manifold pressure,
   coolant/air temperatures and supply inputs, using verified channels and
   conversions. A mirrored Peugeot 106 NFW/TU5J2 8P wiring reference lists separate
   throttle, MAP, engine-speed and temperature sensors. Its page is dated 2000
   and does not establish the owner's 1995 pinout.
   ([Wiring reference](https://jdmfsm.info/Auto/--General--/Autoelectric%20schemas/Autoelectric%20RUS/auto/peugeot/106/nfw/nfw.htm).)
4. **Output trace:** log firmware map reads, injector/ignition output transitions,
   and eventual pulse durations against virtual time. Connect those verified
   signals to the existing GPUI grid and plots, with Run/Pause/Step and live
   sensor controls. Keep synthetic sensor inputs distinct from measured data.
5. **Engine model, separately:** throttle-to-airflow/manifold dynamics and engine
   speed feedback require a plant model and physical parameters. Firmware plus
   arbitrary sensor sliders alone cannot establish torque, AFR or safe tuning.

The board-model replay below now covers the register setup, `$1040/$1050` (as a labelled
hypothesis) and the copy loop. A fully booted ECU is not yet proven; no live engine
controls have been added to the app.

## Board-model replay (10 November 2026)

### What runs

`tools/ucsim-hc11-probe.patch` (applied by `make emulator-setup` to µCsim `59a14ff`)
now contains CPU fixes and a research board model, `cl_iaw8p`, compiled into the HC11
simulator. The generic 6850/6821 devices that `cl_m6800::mk_hw_elements` placed at
`$8000–$8023` are no longer created for the HC11. Every CPU access goes through the board
model; console and loader accesses do not. An access the model does not implement
records the address, value, instruction address and reason, and nothing executes after
it. The bridge then replays to the instruction before, so the shown state contains only
modelled behaviour.

| Region | Treatment | Evidence |
|---|---|---|
| `$0000–$00FF` | internal RAM | HC11 reset location (INIT `$01`) |
| `$0100–$0FFF` | stop | not mapped for the A-family layout; CPU suffix unknown |
| `$1000–$103F` | modelled registers, others stop | reference manual |
| `$1040–$1FFF` | stop (H1: writes to `$1040`/`$1050` recorded) | board decode unknown |
| `$2000–$27FF` | external RAM | SP=`$27FF`; RAM test `$2000–$27F9` |
| `$2800–$3FFF` | stop | dump bytes are all zero here |
| `$4000–$FFFF` | read-only EPROM image | code and vectors; file offset = CPU address |

Modelled registers and their documented effects: INIT (`$01` once within 64 E cycles;
anything else stops), OPTION (IRQE/DLY/CR bits frozen after 64 cycles; CSEL stops),
TMSK2 (prescaler accepts only the first write within 64 cycles), TFLG1/TFLG2 (write 1 to clear), TMSK1, PACTL
(pulse-accumulator enable stops), TCNT (free-running at E/prescaler, TOF on overflow,
RTIF every 2^13 × RTR cycles from reset), PORTD output latch and DDRD (pin reads stop;
a BSET/BCLR read-modify-write is allowed only if the next instruction overwrites N/Z,
and copied unknown input-pin latch bits cannot become outputs until a known write
replaces them; the GUI shows those bits as `?`), HPRIO (PSEL only;
mode bits stop), ADCTL (needs ADPU; CCF sets autonomously after 4 × 32 cycles, even
without a register read; result reads `$1031–$1034`
stop because sensor values are not invented), COPRST (`$55`/`$AA` service events; the
timeout is not enforced because CONFIG/NOCOP is unknown), PPROG (`$00` only), and SCI
BAUD/SCCR1/SCCR2 (stored). Clearing the I bit while any timer or SCI interrupt enable is
set stops, because interrupt delivery is not modelled (see defects below).

Cycle counts come from µCsim's per-opcode tables, checked against the manual for every
opcode on the replayed path. Milliseconds assume a 2 MHz E clock (8 MHz crystal). The
firmware's `BAUD=$33` gives 1202.9 baud at 2 MHz, which is consistent with that
assumption but does not prove it.

### µCsim defects found and fixed in the patch

Each is covered by a conformance case; all cases run in every simulator process and
any difference rejects the result.

1. `STAA/STAB`, `LDX/LDY/LDS`, `STX/STY/STS` cleared C and kept a stale Z (same mask bug
   as LDA). The manual: N/Z from the value, V cleared, C unchanged.
2. HC11 `CPX` used the 6800 routine, which never sets C.
3. `ABY` computed `X+B` instead of `Y+B`. This corrupted the checksum routine at
   `$BD6F` and produced a bogus EPROM write at `$569C` before it was fixed.
4. `CPY ,X` (`1A AC`) and `CPX ,Y` (`CD AC`) were decoded as opcode `AD`, so the RAM test
   at `$5BEC` stopped as an invalid instruction; disassembly was fixed too.
5. `BSET` did not clear V.
6. `MUL` took 3 instead of 10 E cycles.

HC11 read cycles of `CLR` are not modelled. Interrupt stacking and further defects were
fixed later (see the H2 section, items 7–12).

### Opcode audit

`tools/firmware_probe.py` held 92 cases (82 opcodes) at this stage, now 202 (172 opcodes
plus interrupt scenarios), with registers, flags, PC, memory and E cycles hand-derived
from the manual. The replay stops before the first execution of any opcode without a
case. Both supported files execute 81 distinct opcodes on the H1 path; all are covered.

### Results (both supported files are identical here)

Strict: 20 instructions, 71 E cycles. INIT `$01`, OPTION `$BA`, TMSK2 `$01`, PORTD `$FF`,
DDRD `$1A`, HPRIO `$21` (PSEL only; reset value `$26` assumes normal expanded mode, as
the reset vector is in the external EPROM), ADCTL `$14` all take effect. Blocked before
`$B834  B7 10 40  STAA $1040`.

H1 (instruction numbers of first execution): external latches at 21, first COP service
at 24, copy loop at 28 (4,921 write-backs dropped), RAM check `$5BA6` at 34,484,
internal-EEPROM block check `$BC31` at 66,250 (reads `$B600–$B7FF`; H1 sees the dump's zeros there), SCI setup `$A882` at 68,145 (`SCCR2=$24`: receiver and
its interrupt enabled, still masked), routine `$A833` at 68,189. Blocked before
`$A86A  1E 60 20 08  BRSET $60,X,#$20` with X=`$1000`: a read of board input `$1060`
bit 5. 68,202 instructions, 235,893 E cycles.

### Next blockers, in order (resolved for H2, see below; strict and H1 still stop here)

1. **`$1060` bit 5** (H1, `$A86A`): an unknown board input. Without board evidence the
   honest next step is an explicitly user-selected bit value, shown as such, to explore
   both branches; no physical investigation is planned.
2. **`$1050` read-modify-write** at `$B904` (`BSET $50,X,#$08`): needs the latch's read
   behaviour.
3. **A/D results** after `JSR $4017` (`$401A`/`$4037` wait for CCF, then `$1031–$1034` are
   stored to `$2003–$200A`): sensor channels and conversions are unknown; the replay stops
   rather than invent values.
4. **Interrupt delivery**: `$B932` enables TOI and RTII, then `CLI` at `$B943`. This needs
   HC11 stacking (with Y), vector fetch, I-bit and HPRIO priority handling, plus
   conformance cases for RTI.
5. Only then can the foreground loop `$D4A2` and timer-driven work be observed with the
   engine stopped.

### GUI bridge (`--bridge`)

The Emulation tab runs the same script with `python3 -I -c <embedded script> --bridge`
and one strict JSON request on stdin. The request carries the unedited source bytes,
their SHA-256 and, for a lookup, session edits restricted to `0x91AB..0x9282`.
The bridge rechecks the profile hash, the lookup routine hash and all bounds.

- **Run:** `{"action": "run", "profile": "strict"|"h1"|"h2", "budget": 0..2000000,
  "inputs": null | {H2 inputs}, "checkpoint": null | {"state", "digest"}}`. Without a
  checkpoint the session starts at reset; with one, a fresh µCsim loads it. The request
  runs up to `budget` instructions in at most 64 steps, sampling PC, instruction count and
  E cycles after each, and returns a new checkpoint unless blocked. If a blocker occurs,
  the request is repeated from the same start and stops one instruction earlier. µCsim's
  clock delta must equal the board model's cycle delta, and the instruction counts must
  agree, otherwise the result is discarded. Commands are fed from a file, which µCsim does
  not echo. Protocol version 3.
- **Lookup:** one call of `$B1C5` with a constructed descriptor at `$2100`, stopped
  before `RTS` at `$B218`. The simulator's `A` must equal the recovered arithmetic,
  otherwise the result is rejected.
- Expression markers delimit output blocks, and the parser fails closed on any
  unexpected output. The 202 conformance cases run last in every simulator process,
  each after its own reset, so their state cannot leak into the trace.

## Continuous engine-stopped run: simulation profile H2 (10 November 2026)

### Result

Under the labelled simulation profile **H2**, both supported files run from reset into the
foreground loop `$D4A2` (first reached at instruction 70,045; 70,044 with `$1060` bit 5 set)
and keep running it with timer interrupts. Numbers below are identical for both files.

| Run | Instructions | Virtual time | Main-loop passes | RTI / TOF interrupts | RTI returns | SP at `$D4A2` |
|---|---|---|---|---|---|---|
| one request, `$1060`=`$00` | 2,000,000 | 4.06 s | 424 | 961 / 30 | 990 | `$27FF` only |
| one request, `$1060`=`$20` | 2,000,000 | 4.06 s | 423 | 961 / 30 | 990 | `$27FF` only |
| ten chained requests (XSI2) | 20,000,000 | 40.8 s | 4,245 | 9,917 / 310 | 10,227 | `$27FF` only |

Virtual time assumes a 2 MHz E clock; the ten-request run took 6.9 s of wall time. One
interrupt can be in progress when a request ends, so returns are entries or entries − 1.
Executions at static landmarks confirm the scheduler structure described above: the TOF
handler `$743C` runs once per TOF entry, sets `$B5` bit 0 at `$7477`, and the foreground
consumes it at `$D4EC` exactly as often (30 of 30, 310 of 310). The RTI handler `$78C5`
runs once per RTI entry. COP is serviced on every loop pass (428 services in 2,000,000
instructions); the longest gap, 127,972 E cycles (64 ms, during startup), is shorter than
the 262 ms timeout that OPTION CR=`10` would give if CONFIG enables the watchdog (unknown,
not enforced).

**This is an engine-stopped scheduler, not a running engine.** No crank or input-capture
edge occurs, no output has been mapped to an injector or coil, and every input below is a
supplied constant.

### How a session continues (design)

Each Run request starts a new µCsim process, loads the previous request's checkpoint, runs
at most 2,000,000 instructions and saves a new checkpoint. The board model writes the
checkpoint (`set hardware iaw8p save|load`): CPU registers, internal and external RAM,
every board field (registers, timers, flags, latches, SPI/SCI/EEPROM state, statistics,
event ring) and the absolute instruction count. Loading rejects a malformed or incomplete
file. The bridge compresses it and binds it with a SHA-256 over source hash, profile,
inputs and state; any other source, profile or input set is refused. Tests show that
chained requests give byte-identical checkpoints and identical reports to one long run
(H2 split at 123,457 of 400,000 and 333,333 of 900,000; H1 split at 20,011 of 50,000).
A blocked request is repeated from the same checkpoint up to the instruction before the
blocker, and returns no checkpoint. This was chosen over a persistent interactive µCsim
process: µCsim's console echo interleaves with its output, while one bounded process per
request keeps the existing timeouts, output limits, cancellation (kill the child) and
fail-closed parsing.

### Hypotheses and inputs of H2 (all named, none verified on hardware)

| Item | H2 treatment | Evidence type |
|---|---|---|
| `$1040`, `$1050` | as H1, and reads return the last written value | hypothesis (`$B904` read-modify-write) |
| `$1060`, `$1080` | supplied input bytes, default `$00`; bits used: `$1060` bit 5 (`$A86A`), bit 1 (`$A035`); `$1080` bit 4 (`$BB51/$BB61`) | firmware evidence of the reads; values hypothetical |
| PORTA inputs PA0–PA2, PA7 | supplied, default 0; no edges | hypothesis (engine stopped) |
| PORTD inputs PD0–PD5 | supplied, default `$3F` (RxD idle high, SS high) | hypothesis |
| A/D AN0–AN7 | supplied raw results, default `$80` | hypothesis; no channel meaning |
| IRQ, XIRQ pins | inactive | hypothesis |
| Input capture, pulse accumulator | no edges (PACTL PAEN allowed) | hypothesis (engine stopped) |
| SPI, SCI received data | unknown: a load must be discarded by the next instruction, else stop | fail-closed |
| SCI receive line | idle since reset: RDRF, OR, IDLE stay 0 | hypothesis; manual: IDLE needs prior activity |
| Internal EEPROM `$B600–$B7FF` | present (CONFIG EEON=1), power-up content = supplied fill byte, default `$FF` | firmware evidence: dump bytes there are all zero and `$DD62`/`$B425` program/erase there; content unknown |

With the default erased EEPROM, the firmware finds `$B7FF ≠ $AA`, schedules a bulk
erase (`$DD69`, command `$0C` at `$2474`) and reprograms bytes from its RTI handler
(`$B425`): 187 program/erase operations in the first 2,000,000 instructions, 288 in
20,000,000. This happens only in the simulated EEPROM.

### Hardware modelled for H2 (reference manual sections)

- **Interrupts** (5.5, Figures 5-3/5-4/5-5): between instructions, the highest-priority
  pending enabled source (HPRIO PSEL promoted first, then IRQ, RTI, IC1–3, OC1–5, TOF,
  PAOV, PAI, SPI, SCI) is served when I=0: PC, Y, X, A, B, CCR stacked (9 bytes), I set,
  vector fetched, 12 E cycles (cycles 3–14 of SWI). When CLI changes I from 1 to 0, the
  following instruction runs first; another CLI with I already clear does not renew that
  delay (section 5.7.2). RTI pulls the same 9 bytes; X can be cleared but not set. Strict
  and H1 still stop when I is cleared, because the IRQ pin level is unknown there.
- **Timer**: TOC1–TOC5 compare against TCNT and set OCxF; TCTL1/OC1M/OC1D pin actions on
  PA3–PA7 with OC1 overriding (7.5.1); CFORC forces actions; PORTA latch drives pins no
  output compare owns (7.3). TCTL2 and PACNT are stored.
- **SPI master** (8): transfer of 8 bits at E/2…E/32, SPIF, WCOL, mode fault from SS,
  flag-clearing sequences.
- **SCI transmitter** (9): idle preamble on TE 0→1, double-buffered TDR/shifter, TDRE/TC
  at the BAUD-derived bit time (BAUD `$33`: 1,664 E cycles per bit).
- **EEPROM** (4.5.4): EELAT/EEPGM interlock, latched address/data, byte/row/bulk erase,
  programming only clears bits, VPP for at least 10 ms (20,000 E cycles at 2 MHz), no
  reads while EELAT=1; ODD/EVEN test modes stop.

### Further µCsim defects found and fixed

Found by the new conformance cases (each now covered):

7. CCR bits S and X were forced to 1 by a 6800 "unused bits" operator; on the HC11 they are
   real bits (RTI cannot set X, for example).
8. Interrupt entry and RTI used 6800 stacking without Y; the HC11 order is PCL, PCH, IYL,
   IYH, IXL, IXH, A, B, CCR.
9. `ADCA`/`ADCB` computed H without the carry-in.
10. `LDD` and `STD` kept a stale V (the manual clears it).
11. `IDIV`/`FDIV` took 40 instead of 41 E cycles; `LDY ext` took 5 instead of 6.
12. Reset left CCR at `$C0`; the HC11 resets with S, X and I set (`$D0`).

### Conformance

202 cases, hand-derived from the manual, run last in every simulator process: 172 opcodes,
both branch outcomes for every conditional branch on the path, and nine interrupt
scenarios (entry stacks Y and sets I; handler clears TOF and RTI resumes; masked by I; CLI
delay; taken after CLI + 1; a second CLI does not renew the delay; the same source re-entering after RTI while its flag stays
set; fixed RTI-over-TOF priority; HPRIO promoting TOF). The interrupt scenarios set board
flags through a test-only `set hardware iaw8p reg` command. Both files execute exactly these
172 opcodes on the H2 path, so no blocker remains there. Peripheral behaviour has its own
tests (`tools/test_firmware_probe.py`): supplied inputs and read-back, A/D channel
selection, SPI timing and unknown received data, the EEPROM sequence, output-compare pin
actions and the SCI transmitter.

### Limitations and new assumptions

- The E clock (2 MHz), CPU suffix, CONFIG (EEON, NOCOP) and the whole board decode
  remain unverified. EEPROM presence is inferred from the firmware, not measured.
- No engine: no crank/cam edges, so the fuel and ignition scheduling paths that need them
  have not run. The SPI bytes (frames built at `$A0CF` from a pointer table at `$8010`, and
  `$55` from `$A1AB`) and the three SCI characters (`$0F`) go to unknown devices.
- One cycle of write-inhibit after a TOCx high-byte write, SPI begin/end synchronisation
  delays, SCI receive, wake-up and break are not modelled; an access that would need
  them stops. Opcodes outside the 172 still stop.
- Strict and H1 results are unchanged (20 and 68,202 instructions). H1 reads the dump's
  zero bytes at `$B600–$B7FF` as EPROM; its landmark at `$BC31` is the internal-EEPROM block
  check, not an EPROM checksum as first labelled.
