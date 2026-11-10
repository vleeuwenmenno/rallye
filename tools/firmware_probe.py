#!/usr/bin/env python3
"""Run bounded IAW8P firmware experiments with an external, patched µCsim.

This is not an engine simulator: under H2 a commanded crank-wheel signal and raw sensor values
are stimuli; the firmware computes its own readings from them. The patched simulator carries a research
board model (tools/ucsim-hc11-probe.patch) whose memory map and registers are
documented hypotheses for two identified files only; any CPU access it does not
model stops execution. No firmware is patched on disk. See docs/firmware-emulation.md.

`--bridge` reads one JSON request on stdin and prints one JSON response. The GUI
uses it to replay startup from reset under a bounded instruction budget ("run")
and for one lookup through the original routine ("lookup").
"""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import zlib


SUPPORTED = {
    "e91f72ee8e80068b523fcff4c2bb5ecf0ae3f8df6cfe3ec31808133e215d5663": "106XSI2.ORI (XSI2)",
    "081bc0ede46963f101bab5e6a0a69e9a7babb11bb1122928a815c8f1032da725": "REMAP MILD EURO95 RUNNING.BIN (mild remap)",
}
LOOKUP_SHA256 = "6b25c337bcc9308cc838ea93f0c51a4fca2db67b06be6613989d99ecb3a12cdc"
PROTOCOL = 4
# 24 rows x stride 9 read by the 0x647E caller; the only bytes a session edit may change.
TABLE_FIRST, TABLE_LAST = 0x91AB, 0x9282
MAX_RUN = 2_000_000  # instructions per run request; a session continues from its checkpoint
SAMPLES = 64        # PC/cycle samples per request
E_CLOCK_HZ = 2_000_000  # assumption: 8 MHz crystal / 4; labels virtual time and the 10 ms EEPROM check
CORE = "µCsim 59a14ff HC11 core + tools/ucsim-hc11-probe.patch (CPU fixes, HC11 interrupts, IAW8P board model)"
BOARD = {
    "strict": (0, "Strict: only HC11 behaviour from the reference manual plus regions inferred from the "
                  "firmware itself. Any access to external I/O ($1040-$1FFF) or a write to EPROM addresses stops."),
    "h1": (1, "Hypothesis H1 (unverified board assumptions): writes to $1040 and $1050 are write-only latches with "
              "unknown, unmodelled effect; an EPROM-address write of the byte already there is dropped (equivalent "
              "whether the board ignores it or shadows it into RAM); any other external access still stops; the SCI "
              "receive line is idle (no diagnostic tester connected)."),
    "h2": (2, "Simulation profile H2 (H1 plus named hypotheses and supplied raw inputs): $1040/$1050 read back the "
              "last written value; board input bytes $1060 and $1080, PORTA PA1/PA2 and PORTD input pins and A/D "
              "channels AN0-AN7 are supplied raw values (changeable at a chunk boundary); a simulated 60-2 crank wheel "
              "at the commanded speed drives PA7 (pulse accumulator) and PA0 (input capture 3) together, active edge "
              "falling, as the firmware's own setup implies (wiring unverified); IC1/IC2 pins constant; IRQ and XIRQ "
              "pins inactive; SPI and SCI received data unknown (only discarded loads may read it); SCI receive line "
              "idle; internal EEPROM at $B600-$B7FF (CONFIG EEON=1 inferred) starts with the supplied fill byte. "
              "No engine model: speed is commanded, never computed from throttle or load."),
}
INPUT_LIMITS = {"in1060": 255, "in1080": 255, "porta_in": 255, "portd_in": 63, "eeprom_fill": 255, "crank_rpm": 9000}
# H2 defaults: explicit simulated values, not measurements. PORTD inputs high = idle SCI RxD (PD0) and SS (PD5).
DEFAULT_INPUTS = {"in1060": 0x00, "in1080": 0x00, "porta_in": 0x00, "portd_in": 0x3F, "eeprom_fill": 0xFF, "crank_rpm": 0,
                  "adc": [0x80] * 8}
LOOP_PC = 0xD4A2  # foreground loop head (static disassembly, both supported files)
REGIONS = [("internal RAM", 0x0000, 0x00FF), ("unmapped", 0x0100, 0x0FFF), ("HC11 registers", 0x1000, 0x103F),
           ("external I/O (unknown)", 0x1040, 0x1FFF), ("external RAM (inferred)", 0x2000, 0x27FF),
           ("unmapped (dump bytes zero)", 0x2800, 0x3FFF), ("EPROM image", 0x4000, 0xFFFF)]
REGISTER_NAMES = {0x00: "PORTA", 0x08: "PORTD", 0x09: "DDRD", 0x0B: "CFORC", 0x0C: "OC1M", 0x0D: "OC1D",
                  0x0E: "TCNT hi", 0x0F: "TCNT lo", 0x16: "TOC1 hi", 0x17: "TOC1 lo", 0x18: "TOC2 hi", 0x19: "TOC2 lo",
                  0x1A: "TOC3 hi", 0x1B: "TOC3 lo", 0x1C: "TOC4 hi", 0x1D: "TOC4 lo", 0x1E: "TOC5 hi", 0x1F: "TOC5 lo",
                  0x20: "TCTL1", 0x21: "TCTL2", 0x22: "TMSK1", 0x23: "TFLG1", 0x24: "TMSK2", 0x25: "TFLG2",
                  0x26: "PACTL", 0x27: "PACNT", 0x28: "SPCR", 0x29: "SPSR", 0x2A: "SPDR", 0x2B: "BAUD", 0x2C: "SCCR1",
                  0x2D: "SCCR2", 0x2E: "SCSR", 0x2F: "SCDR", 0x30: "ADCTL", 0x31: "ADR1", 0x32: "ADR2", 0x33: "ADR3",
                  0x34: "ADR4", 0x39: "OPTION", 0x3A: "COPRST", 0x3B: "PPROG", 0x3C: "HPRIO", 0x3D: "INIT"}
# Static disassembly landmarks on the startup path (both supported files), in boot order.
MILESTONES = [(0xB800, "Reset vector"), (0xB80E, "INIT write"), (0xB811, "OPTION/timer/port/ADC setup"),
              (0xB834, "External latches $1040/$1050"), (0xB83C, "First COP service"),
              (0xB846, "Copy loop $8000-$9338"), (0x5BA6, "RAM check and RAM test"),
              (0xBC31, "EEPROM block check $B600-$B7FF"), (0xA882, "SCI setup"), (0xA833, "Routine $A833 (reads $1060)"),
              (0xB8BE, "A/D start"), (0xB943, "Interrupts enabled (CLI)"), (0xD4A2, "Foreground main loop")]
# Addresses whose execution counts show the steady state (static disassembly, both supported files).
WATCH = [(0xD4A2, "foreground loop head"), (0xD4EC, "periodic work: $B5 bit 0 found set"),
         (0xD5C8, "loop continuation $D5C8 (reached with and without the flag)"), (0x743C, "TOF handler entry"),
         (0x7477, "TOF handler sets $B5 bit 0"), (0x78C5, "RTI handler entry")]
SOURCES = ["IRQ", "RTI", "IC1", "IC2", "IC3", "OC1", "OC2", "OC3", "OC4", "OC5", "TOF", "PAOV", "PAI", "SPI", "SCI"]
VECTORS = [0xFFF2, 0xFFF0, 0xFFEE, 0xFFEC, 0xFFEA, 0xFFE8, 0xFFE6, 0xFFE4, 0xFFE2, 0xFFE0, 0xFFDE, 0xFFDC, 0xFFDA,
           0xFFD8, 0xFFD6]
# Firmware RAM bytes sampled 64 times per request (static disassembly, both supported files; see docs).
PROBES = [(0xBB, "period hi"), (0xBC, "period lo"), (0xD1, "rpm hi"), (0xD2, "rpm lo"), (0xD8, "tooth"),
          (0xD9, "next tooth"), (0xAE, "sync flags"), (0x2003, "AN2 copy"), (0xC6, "AN2 validated"),
          (0x200F, "AN2 processed"), (0x0E, "AN2 closed endpoint"), (0x0D, "AN2 open endpoint"), (0xAA, "flags $AA"),
          (0x200A, "AN3 copy"), (0x2093, "sync errors"), (0x2096, "resyncs")]
STATE_LIMIT = 4 << 20       # decompressed checkpoint
CHECKPOINT_LIMIT = 1 << 20  # encoded checkpoint in a request
SIM_TIMEOUT = 20
OUTPUT_LIMIT = 4 << 20
REQUEST_LIMIT = 2 << 20


def ihex(data):
    lines = []
    for address in range(0, len(data), 16):
        chunk = data[address:address + 16]
        record = bytes((len(chunk), address >> 8, address & 255, 0)) + chunk
        lines.append(":" + record.hex().upper() + f"{(-sum(record)) & 255:02X}")
    return "\n".join(lines + [":00000001FF", ""])


def lerp_byte(first, second, fraction):
    # SUBA/BCS, magnitude MUL, ADCA #0 rounds from product bit 7.
    distance = (abs(second - first) * fraction + 128) >> 8
    return first + (distance if second >= first else -distance)


def expected_lookup(cells, column_fraction, row_fraction):
    upper = lerp_byte(cells[0], cells[1], column_fraction)
    lower = lerp_byte(cells[2], cells[3], column_fraction)
    return lerp_byte(upper, lower, row_fraction)


def check_source(data):
    digest = hashlib.sha256(data).hexdigest()
    if len(data) != 65536 or digest not in SUPPORTED:
        raise ValueError("Unsupported dump: this experiment accepts only the identified XSI2 and mild-remap hashes")
    if hashlib.sha256(data[0xb1c5:0xb219]).hexdigest() != LOOKUP_SHA256:
        raise ValueError("Lookup routine does not match the reviewed bytes")
    return digest


# Instruction conformance: expected results hand-derived from the HC11 reference manual
# (instruction glossary, condition codes and cycle counts). Code runs from external RAM at
# $2400; defaults below apply unless a case overrides them. Every opcode the startup replay
# executes must be covered here, otherwise the replay stops before that instruction.
CODE = 0x2400
WINDOWS = (0x0050, 0x2500, 0x2510, 0x25E0, 0x25F0)  # 16-byte memory windows checked after every case
PRE = {"a": 0, "b": 0, "x": 0x2500, "y": 0x2500, "sp": 0x25F0, "cc": 0xD0}
# Interrupt scenarios: board register state set directly (test scaffolding), vectors pointed at RAM handlers.
IRQ_PRE = dict(a=0x12, b=0x34, x=0x5678, y=0x9ABC, cc=0xC1, board={0x24: 0x80, 0x25: 0x80},
               vectors={0xFFDE: 0x2420, 0xFFF0: 0x2430}, isr={0x2420: "8680b710253b", 0x2430: "3b"})
IRQ_STACK = [0xC1, 0x34, 0x12, 0x56, 0x78, 0x9A, 0xBC, 0x24, 0x00]  # CCR B A XH XL YH YL PCH PCL
SETUP = {"mem", "steps", "board", "vectors", "isr"}  # case keys that are not CPU registers
CASES = [
    ("INX", "08", dict(x=0x00FF, cc=0xD5), dict(x=0x0100, cc=0xD1), 3),
    ("CBA", "11", dict(a=0x10, b=0x20), dict(cc=0xD9), 2),
    ("BSET dir", "145081", dict(cc=0xD3, mem={0x50: [0x01]}), dict(cc=0xD9, mem={0x50: [0x81]}), 6),
    ("BCLR dir", "155001", dict(cc=0xD2, mem={0x50: [0x01]}), dict(cc=0xD4, mem={0x50: [0x00]}), 6),
    ("TAB", "16", dict(a=0x80, cc=0xD7), dict(b=0x80, cc=0xD9), 2),
    ("TBA", "17", dict(a=0x55, cc=0xDA), dict(a=0, cc=0xD4), 2),
    ("BSET ,X", "1c0540", dict(cc=0xD4), dict(cc=0xD0, mem={0x2505: [0x40]}), 7),
    ("BRSET ,X taken", "1e052110", dict(mem={0x2505: [0x23]}), dict(pc=0x2414), 7),
    ("BRSET ,X not taken", "1e052110", dict(mem={0x2505: [0x03]}), dict(), 7),
    ("BRA", "2010", dict(), dict(pc=0x2412), 3),
    ("BHI taken", "2210", dict(), dict(pc=0x2412), 3),
    ("BHI not taken", "2210", dict(cc=0xD1), dict(), 3),
    ("BLS taken", "2310", dict(cc=0xD4), dict(pc=0x2412), 3),
    ("BLS not taken", "2310", dict(), dict(), 3),
    ("BNE taken", "2610", dict(), dict(pc=0x2412), 3),
    ("BNE not taken", "2610", dict(cc=0xD4), dict(), 3),
    ("BEQ taken", "2710", dict(cc=0xD4), dict(pc=0x2412), 3),
    ("BEQ not taken", "2710", dict(), dict(), 3),
    ("PULX", "38", dict(mem={0x25F1: [0x12, 0x34]}), dict(x=0x1234, sp=0x25F2), 5),
    ("RTS", "39", dict(mem={0x25F1: [0x24, 0x10]}), dict(sp=0x25F2, pc=0x2410), 5),
    ("ABX", "3a", dict(x=0x24FF, b=0x02, cc=0xD5), dict(x=0x2501), 3),
    ("PSHX", "3c", dict(x=0x1234), dict(sp=0x25EE, mem={0x25EF: [0x12, 0x34]}), 4),
    ("MUL carry", "3d", dict(a=0x80, b=0x81), dict(a=0x40, b=0x80, cc=0xD1), 10),
    ("MUL no carry", "3d", dict(a=0x10, b=0x07, cc=0xD1), dict(a=0x00, b=0x70, cc=0xD0), 10),
    ("COMA", "43", dict(a=0x0F, cc=0xD2), dict(a=0xF0, cc=0xD9), 2),
    ("ASLA carry", "48", dict(a=0xC0), dict(a=0x80, cc=0xD9), 2),
    ("ASLA overflow", "48", dict(a=0x40), dict(a=0x80, cc=0xDA), 2),
    ("INCA", "4c", dict(a=0x7F, cc=0xD1), dict(a=0x80, cc=0xDB), 2),
    ("TSTA", "4d", dict(cc=0xD3), dict(cc=0xD4), 2),
    ("CLRA", "4f", dict(a=0x55, cc=0xDB), dict(a=0, cc=0xD4), 2),
    ("COMB", "53", dict(b=0xFF), dict(b=0, cc=0xD5), 2),
    ("ASLB", "58", dict(b=0x81), dict(b=0x02, cc=0xD3), 2),
    ("DECB", "5a", dict(b=0x80, cc=0xD1), dict(b=0x7F, cc=0xD3), 2),
    ("TSTB", "5d", dict(b=0x80, cc=0xD7), dict(cc=0xD8), 2),
    ("CLRB", "5f", dict(b=0x80, cc=0xD9), dict(b=0, cc=0xD4), 2),
    ("DEC ext", "7a2510", dict(cc=0xD1, mem={0x2510: [1]}), dict(cc=0xD5, mem={0x2510: [0]}), 6),
    ("INC ext", "7c2510", dict(cc=0xD9, mem={0x2510: [0xFF]}), dict(cc=0xD5, mem={0x2510: [0]}), 6),
    ("TST ext", "7d2510", dict(cc=0xD7, mem={0x2510: [0x80]}), dict(cc=0xD8), 6),
    ("CLR ext", "7f2510", dict(cc=0xDB, mem={0x2510: [0x55]}), dict(cc=0xD4, mem={0x2510: [0]}), 6),
    ("CMPA imm", "8180", dict(a=0x7F), dict(cc=0xDB), 2),
    ("SUBD imm borrow", "830001", dict(), dict(a=0xFF, b=0xFF, cc=0xD9), 4),
    ("SUBD imm overflow", "830001", dict(a=0x80), dict(a=0x7F, b=0xFF, cc=0xD2), 4),
    ("ANDA imm", "840f", dict(a=0xF0, cc=0xD3), dict(a=0, cc=0xD5), 2),
    ("LDAA imm", "8680", dict(cc=0xD5), dict(a=0x80, cc=0xD9), 2),
    ("CPX imm borrow", "8c0001", dict(x=0), dict(cc=0xD9), 4),
    ("CPX imm overflow", "8c0001", dict(x=0x8000), dict(cc=0xD2), 4),
    ("LDS imm", "8e0000", dict(cc=0xD9), dict(sp=0, cc=0xD5), 3),
    ("XGDX", "8f", dict(a=0x12, b=0x34, x=0xABCD, cc=0xD5), dict(a=0xAB, b=0xCD, x=0x1234), 3),
    ("LDAA dir", "9650", dict(a=0x11, cc=0xD9), dict(a=0, cc=0xD5), 3),
    ("STAA dir", "9750", dict(a=0x80, cc=0xD5), dict(cc=0xD9, mem={0x50: [0x80]}), 3),
    ("CMPA ,X", "a105", dict(a=0x10, mem={0x2505: [0x10]}), dict(cc=0xD4), 4),
    ("LDAA ,X", "a605", dict(cc=0xDF, mem={0x2505: [0x7F]}), dict(a=0x7F, cc=0xD1), 4),
    ("STAA ,X", "a705", dict(cc=0xD9, mem={0x2505: [0x55]}), dict(cc=0xD5, mem={0x2505: [0]}), 4),
    ("BITA ext", "b52510", dict(a=0x0F, cc=0xD3, mem={0x2510: [0xF0]}), dict(cc=0xD5), 4),
    ("LDAA ext", "b62510", dict(cc=0xD5, mem={0x2510: [0x80]}), dict(a=0x80, cc=0xD9), 4),
    ("STAA ext", "b72510", dict(cc=0xD9, mem={0x2510: [0x55]}), dict(cc=0xD5, mem={0x2510: [0]}), 4),
    ("EORA ext", "b82510", dict(a=0xFF, cc=0xD3, mem={0x2510: [0x0F]}), dict(a=0xF0, cc=0xD9), 4),
    ("ORAA ext", "ba2510", dict(cc=0xDA), dict(cc=0xD4), 4),
    ("CPX ext", "bc2510", dict(x=0x1234, cc=0xD1, mem={0x2510: [0x12, 0x34]}), dict(cc=0xD4), 6),
    ("JSR ext", "bd2410", dict(), dict(sp=0x25EE, pc=0x2410, mem={0x25EF: [0x24, 0x03]}), 6),
    ("CMPB imm", "c101", dict(), dict(cc=0xD9), 2),
    ("ADDD imm carry", "c30001", dict(a=0xFF, b=0xFF), dict(a=0, b=0, cc=0xD5), 4),
    ("ADDD imm overflow", "c30001", dict(a=0x7F, b=0xFF), dict(a=0x80, b=0, cc=0xDA), 4),
    ("LDAB imm", "c600", dict(b=5, cc=0xD9), dict(b=0, cc=0xD5), 2),
    ("LDD imm", "cc8000", dict(b=7, cc=0xD5), dict(a=0x80, b=0, cc=0xD9), 3),
    ("LDX imm", "ce0000", dict(cc=0xD9), dict(x=0, cc=0xD5), 3),
    ("STAB dir", "d750", dict(b=0x80, cc=0xD5), dict(cc=0xD9, mem={0x50: [0x80]}), 3),
    ("STD dir", "dd50", dict(cc=0xD9, mem={0x50: [1, 1]}), dict(cc=0xD5, mem={0x50: [0, 0]}), 4),
    ("ADDD ,X", "e305", dict(b=0xFF, mem={0x2505: [0, 1]}), dict(a=1, b=0), 6),
    ("LDAB ,X", "e605", dict(cc=0xD5, mem={0x2505: [0x80]}), dict(b=0x80, cc=0xD9), 4),
    ("STAB ,X", "e705", dict(b=0x7F, cc=0xDF), dict(cc=0xD1, mem={0x2505: [0x7F]}), 4),
    ("LDD ,X", "ec05", dict(cc=0xD5, mem={0x2505: [0x80, 0x01]}), dict(a=0x80, b=1, cc=0xD9), 5),
    ("CMPB ext", "f12510", dict(b=0x80, mem={0x2510: [1]}), dict(cc=0xD2), 4),
    ("ADDD ext", "f32510", dict(a=0x12, b=0x34, mem={0x2510: [0, 1]}), dict(b=0x35), 6),
    ("LDAB ext", "f62510", dict(b=9, cc=0xD9), dict(b=0, cc=0xD5), 4),
    ("STAB ext", "f72510", dict(b=0x80, cc=0xD5), dict(cc=0xD9, mem={0x2510: [0x80]}), 4),
    ("LDD ext", "fc2510", dict(a=0x11, b=0x22, cc=0xD9), dict(a=0, b=0, cc=0xD5), 5),
    ("STD ext", "fd2510", dict(a=0x80, cc=0xD5), dict(cc=0xD9, mem={0x2510: [0x80, 0]}), 5),
    ("LDX ext", "fe2510", dict(cc=0xD5, mem={0x2510: [0x80, 0x00]}), dict(x=0x8000, cc=0xD9), 5),
    ("STX ext", "ff2510", dict(x=0, cc=0xD9, mem={0x2510: [1, 1]}), dict(cc=0xD5, mem={0x2510: [0, 0]}), 5),
    ("INY", "1808", dict(y=0xFFFF), dict(y=0, cc=0xD4), 4),
    ("ABY", "183a", dict(x=0x1000, b=5), dict(y=0x2505), 4),
    ("XGDY", "188f", dict(a=0x12, b=0x34, y=0xABCD), dict(a=0xAB, b=0xCD, y=0x1234), 4),
    ("CMPA ,Y", "18a105", dict(mem={0x2505: [1]}), dict(cc=0xD9), 5),
    ("STAA ,Y", "18a705", dict(a=0x80, cc=0xD5), dict(cc=0xD9, mem={0x2505: [0x80]}), 5),
    ("LDY imm", "18ce0000", dict(cc=0xD9), dict(y=0, cc=0xD5), 4),
    ("LDAB ,Y", "18e605", dict(cc=0xD5, mem={0x2505: [0x80]}), dict(b=0x80, cc=0xD9), 5),
    ("STD ,Y", "18ed05", dict(cc=0xD9, mem={0x2505: [1, 1]}), dict(cc=0xD5, mem={0x2505: [0, 0]}), 6),
    ("STY ,Y", "18ef05", dict(cc=0xD5), dict(cc=0xD1, mem={0x2505: [0x25, 0x00]}), 6),
    ("CPD imm", "1a830001", dict(), dict(cc=0xD9), 5),
    ("CPD ext", "1ab32510", dict(a=0x12, b=0x34, cc=0xD1, mem={0x2510: [0x12, 0x34]}), dict(cc=0xD4), 7),
    ("CPX ,Y", "cdac05", dict(x=0x2000, mem={0x2505: [0x20, 0x01]}), dict(cc=0xD9), 7),
    # H2 path (simulation profile): every further opcode the firmware executes before and in the main loop.
    ("BRCLR ,X taken", "1f052110", dict(mem={0x2505: [0x02]}), dict(pc=0x2414), 7),
    ("BRCLR ,X not taken", "1f052110", dict(mem={0x2505: [0x03]}), dict(), 7),
    ("BCLR ,X", "1d0581", dict(cc=0xDA, mem={0x2505: [0x81]}), dict(cc=0xD4, mem={0x2505: [0x00]}), 7),
    ("BRCLR dir taken", "13502110", dict(mem={0x50: [0x02]}), dict(pc=0x2414), 6),
    ("BRSET dir taken", "12502110", dict(mem={0x50: [0x23]}), dict(pc=0x2414), 6),
    ("BPL taken", "2a10", dict(), dict(pc=0x2412), 3),
    ("BPL not taken", "2a10", dict(cc=0xD8), dict(), 3),
    ("BMI taken", "2b10", dict(cc=0xD8), dict(pc=0x2412), 3),
    ("BMI not taken", "2b10", dict(), dict(), 3),
    ("BCC taken", "2410", dict(), dict(pc=0x2412), 3),
    ("BCC not taken", "2410", dict(cc=0xD1), dict(), 3),
    ("BCS taken", "2510", dict(cc=0xD1), dict(pc=0x2412), 3),
    ("BCS not taken", "2510", dict(), dict(), 3),
    ("BGE taken", "2c10", dict(cc=0xDA), dict(pc=0x2412), 3),
    ("BGE not taken", "2c10", dict(cc=0xD8), dict(), 3),
    ("BLT taken", "2d10", dict(cc=0xD8), dict(pc=0x2412), 3),
    ("BLT not taken", "2d10", dict(cc=0xDA), dict(), 3),
    ("BGT taken", "2e10", dict(), dict(pc=0x2412), 3),
    ("BGT not taken", "2e10", dict(cc=0xD4), dict(), 3),
    ("BLE taken", "2f10", dict(cc=0xD4), dict(pc=0x2412), 3),
    ("BLE not taken", "2f10", dict(), dict(), 3),
    ("BSR", "8d10", dict(), dict(pc=0x2412, sp=0x25EE, mem={0x25EF: [0x24, 0x02]}), 6),
    ("JMP ext", "7e2410", dict(), dict(pc=0x2410), 3),
    ("JMP ,X", "6e10", dict(), dict(pc=0x2510), 3),
    ("NOP", "01", dict(cc=0xDF), dict(), 2),
    ("IDIV", "02", dict(b=7, x=2, cc=0xDB), dict(b=1, x=3, cc=0xD8), 41),
    ("LSRD", "04", dict(a=0x01, b=0x03), dict(a=0, b=0x81, cc=0xD3), 3),
    ("ASLD", "05", dict(a=0x80, b=0x81), dict(a=0x01, b=0x02, cc=0xD3), 3),
    ("TPA", "07", dict(cc=0xD5), dict(a=0xD5), 2),
    ("DEX", "09", dict(x=1), dict(x=0, cc=0xD4), 3),
    ("CLI", "0e", dict(), dict(cc=0xC0), 2),
    ("SEI", "0f", dict(cc=0xC0), dict(cc=0xD0), 2),
    ("SBA", "10", dict(a=0x10, b=0x20), dict(a=0xF0, cc=0xD9), 2),
    ("ABA half/overflow", "1b", dict(a=0x7F, b=0x01), dict(a=0x80, cc=0xFA), 2),
    ("ABA carry", "1b", dict(a=0xFF, b=0x01), dict(a=0, cc=0xF5), 2),
    ("PULA", "32", dict(cc=0xDF, mem={0x25F1: [0x77]}), dict(a=0x77, sp=0x25F1), 4),
    ("PULB", "33", dict(mem={0x25F1: [0x88]}), dict(b=0x88, sp=0x25F1), 4),
    ("PSHA", "36", dict(a=0x99), dict(sp=0x25EF, mem={0x25F0: [0x99]}), 3),
    ("PSHB", "37", dict(b=0x5A), dict(sp=0x25EF, mem={0x25F0: [0x5A]}), 3),
    ("RTI", "3b", dict(mem={0x25F1: [0xC1, 0x34, 0x12, 0x56, 0x78, 0x9A, 0xBC, 0x24, 0x10]}),
     dict(a=0x12, b=0x34, x=0x5678, y=0x9ABC, cc=0xC1, sp=0x25F9, pc=0x2410), 12),
    ("RTI cannot set X", "3b", dict(cc=0x90, mem={0x25F1: [0xD0, 0, 0, 0x25, 0, 0x25, 0, 0x24, 0x10]}),
     dict(cc=0x90, sp=0x25F9, pc=0x2410), 12),
    ("NEGA", "40", dict(a=0x80), dict(a=0x80, cc=0xDB), 2),
    ("ROLA", "49", dict(a=0x80, cc=0xD1), dict(a=0x01, cc=0xD3), 2),
    ("DECA", "4a", dict(a=0x80, cc=0xD9), dict(a=0x7F, cc=0xD3), 2),
    ("NEGB zero", "50", dict(cc=0xD3), dict(cc=0xD4), 2),
    ("LSRB", "54", dict(b=0x01), dict(b=0, cc=0xD7), 2),
    ("INCB", "5c", dict(b=0x7F), dict(b=0x80, cc=0xDA), 2),
    ("DEC ,X", "6a05", dict(cc=0xD1), dict(cc=0xD9, mem={0x2505: [0xFF]}), 6),
    ("INC ,X", "6c05", dict(mem={0x2505: [0x7F]}), dict(cc=0xDA, mem={0x2505: [0x80]}), 6),
    ("TST ,X", "6d05", dict(cc=0xDB), dict(cc=0xD4), 6),
    ("SUBA imm", "8001", dict(), dict(a=0xFF, cc=0xD9), 2),
    ("SBCA imm", "8200", dict(cc=0xD1), dict(a=0xFF, cc=0xD9), 2),
    ("BITA imm", "8580", dict(a=0x80, cc=0xD1), dict(cc=0xD9), 2),
    ("ADCA imm", "8901", dict(a=0x0E, cc=0xD1), dict(a=0x10, cc=0xF0), 2),
    ("ORAA imm", "8a80", dict(a=0x01), dict(a=0x81, cc=0xD8), 2),
    ("ADDA imm", "8b01", dict(a=0x0F), dict(a=0x10, cc=0xF0), 2),
    ("CMPA dir", "9150", dict(a=0x01, mem={0x50: [0x02]}), dict(cc=0xD9), 3),
    ("ADDA dir", "9b50", dict(a=0x01, cc=0xD1, mem={0x50: [0x02]}), dict(a=0x03, cc=0xD0), 3),
    ("CPX dir", "9c50", dict(x=0x1234, mem={0x50: [0x12, 0x35]}), dict(cc=0xD9), 5),
    ("SUBA ,X", "a005", dict(a=0x80, mem={0x2505: [0x01]}), dict(a=0x7F, cc=0xD2), 4),
    ("SUBD ,X", "a305", dict(a=0x80, mem={0x2505: [0x00, 0x01]}), dict(a=0x7F, b=0xFF, cc=0xD2), 6),
    ("ORAA ,X", "aa05", dict(), dict(cc=0xD4), 4),
    ("ADDA ,X", "ab05", dict(a=0x7F, mem={0x2505: [0x01]}), dict(a=0x80, cc=0xFA), 4),
    ("SUBA ext", "b02510", dict(a=0x7F, mem={0x2510: [0xFF]}), dict(a=0x80, cc=0xDB), 4),
    ("CMPA ext", "b12510", dict(a=0x10, mem={0x2510: [0x20]}), dict(cc=0xD9), 4),
    ("SUBD ext", "b32510", dict(mem={0x2510: [0x00, 0x01]}), dict(a=0xFF, b=0xFF, cc=0xD9), 6),
    ("ANDA ext", "b42510", dict(a=0xFF, cc=0xD2, mem={0x2510: [0x80]}), dict(a=0x80, cc=0xD8), 4),
    ("ADDA ext", "bb2510", dict(a=0xFF, mem={0x2510: [0x01]}), dict(a=0, cc=0xF5), 4),
    ("STS ext", "bf2510", dict(cc=0xDE), dict(cc=0xD0, mem={0x2510: [0x25, 0xF0]}), 5),
    ("SUBB imm", "c080", dict(), dict(b=0x80, cc=0xDB), 2),
    ("ANDB imm", "c40f", dict(b=0xF0), dict(b=0, cc=0xD4), 2),
    ("SUBB dir", "d050", dict(), dict(cc=0xD4), 3),
    ("CMPB dir", "d150", dict(b=0x80, mem={0x50: [0x01]}), dict(cc=0xD2), 3),
    ("ADDD dir", "d350", dict(a=0xFF, b=0xFF, mem={0x50: [0x00, 0x01]}), dict(a=0, b=0, cc=0xD5), 5),
    ("ANDB dir", "d450", dict(b=0xF0, cc=0xD3, mem={0x50: [0x0F]}), dict(b=0, cc=0xD5), 3),
    ("LDAB dir", "d650", dict(mem={0x50: [0x80]}), dict(b=0x80, cc=0xD8), 3),
    ("LDD dir", "dc50", dict(a=5, b=5, cc=0xDE, mem={0x50: [0x00, 0x01]}), dict(a=0, b=1, cc=0xD0), 4),
    ("LDX dir", "de50", dict(cc=0xD9), dict(x=0, cc=0xD5), 4),
    ("STX dir", "df50", dict(x=0x8000), dict(cc=0xD8, mem={0x50: [0x80, 0x00]}), 4),
    ("SUBB ,X", "e005", dict(mem={0x2505: [0x01]}), dict(b=0xFF, cc=0xD9), 4),
    ("CMPB ,X", "e105", dict(b=1, mem={0x2505: [0x01]}), dict(cc=0xD4), 4),
    ("STD ,X", "ed05", dict(a=0x12, b=0x34, cc=0xD2), dict(cc=0xD0, mem={0x2505: [0x12, 0x34]}), 5),
    ("LDX ,X", "ee05", dict(mem={0x2505: [0x12, 0x34]}), dict(x=0x1234), 5),
    ("SUBB ext", "f02510", dict(b=5, cc=0xDF, mem={0x2510: [0x03]}), dict(b=2, cc=0xD0), 4),
    ("PULY", "1838", dict(mem={0x25F1: [0xAB, 0xCD]}), dict(y=0xABCD, sp=0x25F2), 6),
    ("PSHY", "183c", dict(y=0x1234), dict(sp=0x25EE, mem={0x25EF: [0x12, 0x34]}), 5),
    ("SUBA ,Y", "18a005", dict(a=0x10, mem={0x2505: [0x10]}), dict(a=0, cc=0xD4), 5),
    ("SUBD ,Y", "18a305", dict(b=1, mem={0x2505: [0x00, 0x01]}), dict(b=0, cc=0xD4), 7),
    ("LDAA ,Y", "18a605", dict(a=7, cc=0xD9), dict(a=0, cc=0xD5), 5),
    ("ORAA ,Y", "18aa05", dict(a=0x0F, cc=0xD3, mem={0x2505: [0xF0]}), dict(a=0xFF, cc=0xD9), 5),
    ("ADDA ,Y", "18ab05", dict(a=0x08, mem={0x2505: [0x08]}), dict(a=0x10, cc=0xF0), 5),
    ("CMPB ,Y", "18e105", dict(mem={0x2505: [0x80]}), dict(cc=0xDB), 5),
    ("ADDD ,Y", "18e305", dict(a=0x7F, b=0xFF, mem={0x2505: [0x00, 0x01]}), dict(a=0x80, b=0, cc=0xDA), 7),
    ("ANDB ,Y", "18e405", dict(b=0x81, mem={0x2505: [0x80]}), dict(b=0x80, cc=0xD8), 5),
    ("STAB ,Y", "18e705", dict(cc=0xD9, mem={0x2505: [0x55]}), dict(cc=0xD5, mem={0x2505: [0]}), 5),
    ("ADDB ,Y", "18eb05", dict(b=0x80, mem={0x2505: [0x80]}), dict(b=0, cc=0xD7), 5),
    ("LDD ,Y", "18ec05", dict(a=1, b=2, cc=0xD9), dict(a=0, b=0, cc=0xD5), 6),
    ("LDY ext", "18fe2510", dict(mem={0x2510: [0x80, 0x00]}), dict(y=0x8000, cc=0xD8), 6),
    ("STY ext", "18ff2510", dict(y=0, cc=0xD9, mem={0x2510: [1, 1]}), dict(cc=0xD5, mem={0x2510: [0, 0]}), 6),
    ("LDX ,Y", "cdee05", dict(mem={0x2505: [0x80, 0x00]}), dict(x=0x8000, cc=0xD8), 6),
    ("STX ,Y", "cdef05", dict(x=0x8001), dict(cc=0xD8, mem={0x2505: [0x80, 0x01]}), 6),
    # Crank path (simulated 60-2 wheel, H2): every further opcode the firmware executes with crank edges.
    ("IDIV remainder >= quotient", "02", dict(b=20, x=7, cc=0xDF), dict(b=6, x=2, cc=0xD8), 41),
    # Division by zero: quotient $FFFF, C=1; D (the remainder) is indeterminate, so the model keeps it
    # and marks it unknown: any later read of it stops the run.
    ("IDIV by zero", "02", dict(b=5, x=0, cc=0xDE), dict(x=0xFFFF, cc=0xD9), 41),
    ("FDIV", "03", dict(a=0x20, x=0x8000, cc=0xDF), dict(a=0, b=0, x=0x4000, cc=0xD8), 41),
    ("FDIV remainder >= quotient", "03", dict(b=1, x=0xFFFF, cc=0xDF), dict(b=1, x=1, cc=0xD8), 41),
    ("TSX", "30", dict(cc=0xDF), dict(x=0x25F1), 3),
    ("LSRA", "44", dict(a=0x01), dict(a=0, cc=0xD7), 2),
    ("RORA", "46", dict(a=0x01, cc=0xD1), dict(a=0x80, cc=0xD9), 2),
    ("RORB", "56", dict(b=0x02, cc=0xDE), dict(b=0x01, cc=0xD0), 2),
    ("COM ext", "732510", dict(mem={0x2510: [0x0F]}), dict(cc=0xD9, mem={0x2510: [0xF0]}), 6),
    ("LSR ext", "742510", dict(mem={0x2510: [0x81]}), dict(cc=0xD3, mem={0x2510: [0x40]}), 6),
    ("ROR ext", "762510", dict(cc=0xD1), dict(cc=0xDA, mem={0x2510: [0x80]}), 6),
    ("ASL ext", "782510", dict(mem={0x2510: [0xC0]}), dict(cc=0xD9, mem={0x2510: [0x80]}), 6),
    ("ROL ext", "792510", dict(cc=0xD1, mem={0x2510: [0x40]}), dict(cc=0xDA, mem={0x2510: [0x81]}), 6),
    ("EORA imm", "88ff", dict(a=0x0F, cc=0xD3), dict(a=0xF0, cc=0xD9), 2),
    ("SUBA dir", "9050", dict(a=0x10, mem={0x50: [0x11]}), dict(a=0xFF, cc=0xD9), 3),
    ("SUBD dir", "9350", dict(a=0x80, mem={0x50: [0x00, 0x01]}), dict(a=0x7F, b=0xFF, cc=0xD2), 5),
    ("SBCA ext", "b22510", dict(cc=0xD1), dict(a=0xFF, cc=0xD9), 4),
    ("ADCB imm", "c901", dict(b=0x0F, cc=0xD1), dict(b=0x11, cc=0xF0), 2),
    ("ADDB imm", "cb01", dict(b=0x7F), dict(b=0x80, cc=0xFA), 2),
    ("ADDB ,X", "eb05", dict(b=0xFF, mem={0x2505: [0x01]}), dict(b=0, cc=0xF5), 4),
    ("ADDB ext", "fb2510", dict(b=0x01, cc=0xDF, mem={0x2510: [0x02]}), dict(b=0x03, cc=0xD0), 4),
    ("DEY", "1809", dict(y=1), dict(y=0, cc=0xD4), 4),
    ("TSY", "1830", dict(), dict(y=0x25F1), 4),
    ("TYS", "1835", dict(y=0x2580), dict(sp=0x257F), 4),
    ("CPY imm borrow", "188c2501", dict(), dict(cc=0xD9), 5),
    ("CPY ext overflow", "18bc2510", dict(y=0x7FFF, mem={0x2510: [0x80, 0x00]}), dict(cc=0xDB), 7),
    ("LDY dir", "18de50", dict(cc=0xD7, mem={0x50: [0x80, 0x01]}), dict(y=0x8001, cc=0xD9), 5),
    ("CPD dir", "1a9350", dict(a=0x12, b=0x34, mem={0x50: [0x12, 0x34]}), dict(cc=0xD4), 6),
    ("CPD ,X", "1aa305", dict(mem={0x2505: [0x00, 0x01]}), dict(cc=0xD9), 7),
    ("BVC taken", "2810", dict(), dict(pc=0x2412), 3),
    ("BVC not taken", "2810", dict(cc=0xD2), dict(), 3),
    ("BVS taken", "2910", dict(cc=0xD2), dict(pc=0x2412), 3),
    ("BVS not taken", "2910", dict(), dict(), 3),
    # Interrupt response (manual 5.5, Figures 5-3/5-4, CLI and RTI descriptions), simulation profile.
    # Timer overflow pending and enabled; vector $FFDE -> $2420: LDAA #$80; STAA $1025 (clear TOF); RTI.
    ("IRQ entry stacks Y, sets I", "010101", dict(IRQ_PRE, steps=1),
     dict(pc=0x2422, sp=0x25E7, a=0x80, cc=0xD9, mem={0x25E8: IRQ_STACK}), 14),
    ("IRQ handler clears TOF, RTI resumes", "010101", dict(IRQ_PRE, steps=4),
     dict(pc=0x2401, mem={0x25E8: IRQ_STACK}), 32),
    ("IRQ masked by I", "010101", dict(IRQ_PRE, cc=0xD1, steps=1), dict(pc=0x2401, cc=0xD1), 2),
    ("IRQ after CLI waits one instruction", "0e0101", dict(IRQ_PRE, cc=0xD1, steps=2), dict(pc=0x2402, cc=0xC1), 4),
    ("IRQ taken after CLI + 1", "0e0101", dict(IRQ_PRE, cc=0xD1, steps=3),
     dict(pc=0x2422, sp=0x25E7, a=0x80, cc=0xD9, mem={0x25E8: [0xC1, 0x34, 0x12, 0x56, 0x78, 0x9A, 0xBC, 0x24, 0x02]}), 18),
    ("Second CLI does not renew IRQ delay", "0e0e01", dict(IRQ_PRE, cc=0xD1, steps=3),
     dict(pc=0x2422, sp=0x25E7, a=0x80, cc=0xD9, mem={0x25E8: [0xC1, 0x34, 0x12, 0x56, 0x78, 0x9A, 0xBC, 0x24, 0x02]}), 18),
    # RTIF never cleared by its handler ($2430: RTI): the same source re-enters right after each RTI.
    ("IRQ repeats while flag stays set", "010101", dict(IRQ_PRE, board={0x24: 0x40, 0x25: 0x40}, steps=2),
     dict(pc=0x2400, mem={0x25E8: IRQ_STACK}), 48),
    # RTIF and TOF both pending: fixed order serves RTI first; HPRIO PSEL=0 promotes TOF.
    ("IRQ fixed priority RTI over TOF", "010101", dict(IRQ_PRE, board={0x24: 0xC0, 0x25: 0xC0}, isr={0x2430: "8640"}, steps=1),
     dict(pc=0x2432, sp=0x25E7, a=0x40, cc=0xD1, mem={0x25E8: IRQ_STACK}), 14),
    ("IRQ HPRIO promotes TOF", "010101", dict(IRQ_PRE, board={0x24: 0xC0, 0x25: 0xC0, 0x3C: 0x20}, isr={0x2430: "8640"}, steps=1),
     dict(pc=0x2422, sp=0x25E7, a=0x80, cc=0xD9, mem={0x25E8: IRQ_STACK}), 14),
]


def opcode_key(code):
    """Page-qualified opcode as recorded by the board model: 0x100/0x200/0x300 for prefixes 18/1A/CD."""
    page = {0x18: 0x100, 0x1A: 0x200, 0xCD: 0x300}.get(code[0])
    return page + code[1] if page else code[0]


AUDITED = frozenset(opcode_key(bytes.fromhex(code)) for _, code, _, _, _ in CASES)
MARK = 7177  # expression markers 7177kkk delimit output blocks
REG_COMMAND = {"a": "A", "b": "B", "x": "IX", "y": "IY", "sp": "SP", "cc": "CC"}


def window_bytes(mem):
    """The five checked windows as one bytearray per window base, overlaid with mem."""
    out = {base: bytearray(16) for base in WINDOWS}
    for address, values in mem.items():
        for k, value in enumerate(values):
            base = (address + k) & ~0xF
            if base not in out:
                raise ValueError(f"conformance address {address + k:#06x} outside the checked windows")
            out[base][(address + k) & 0xF] = value
    return out


def conformance_commands():
    commands = ["iaw8p_profile=2"]  # the cases run in their own resets after the trace; I=0 needs the simulation profile
    for k, (_, code, pre, _, _) in enumerate(CASES):
        regs = {**PRE, **{r: v for r, v in pre.items() if r not in SETUP}}
        commands.append("reset")
        for base, values in window_bytes(pre.get("mem", {})).items():
            commands.append(f"set memory rom {base:#x} " + " ".join(str(v) for v in values))
        commands.append(f"set memory rom {CODE:#x} " + " ".join(["1"] * 32))  # NOPs: no stale code after the case
        commands.append(f"set memory rom {CODE:#x} " + " ".join(str(b) for b in bytes.fromhex(code)))
        for vector, target in pre.get("vectors", {}).items():
            commands.append(f"set memory rom {vector:#x} {target >> 8} {target & 255}")
        for address, handler in pre.get("isr", {}).items():
            commands.append(f"set memory rom {address:#x} " + " ".join(str(b) for b in bytes.fromhex(handler)))
        commands += [f"set hardware iaw8p reg {r:#x} {v:#x}" for r, v in pre.get("board", {}).items()]
        commands += [f"{REG_COMMAND[r]}={v:#x}" for r, v in regs.items()]
        commands += [f"pc {CODE:#x}", f"step {pre.get('steps', 1)}", f"expr {MARK}*1000+{500 + k}", "info registers",
                     "expr iaw8p_cycles+1000000"]
        commands += [f"dump /h rom {base:#x} {base + 15:#x} 16" for base in WINDOWS]
    return commands


def check_conformance(output):
    """Every case must match its manual-derived registers, flags, PC, memory windows and E cycles."""
    blocks = dict(re.findall(rf"^{MARK}([5-9]\d\d)\n(.*?)(?=^{MARK}\d{{3}}$|\Z)", output, re.M | re.S))
    for k, (name, code, pre, post, cycles) in enumerate(CASES):
        block = blocks.get(str(500 + k), "")
        regs = REGISTERS.search(block)
        cyc = re.search(r"^1(\d{6})$", block, re.M)
        if not regs or not cyc:
            raise ValueError(f"conformance case {name}: missing simulator output")
        got = {"a": int(regs[1], 16), "b": int(regs[2], 16), "cc": int(regs[3], 2), "x": int(regs[4], 16),
               "y": int(regs[5], 16), "sp": int(regs[6], 16), "pc": int(regs[7], 16), "cycles": int(cyc[1])}
        want = {**PRE, **{r: v for r, v in pre.items() if r not in SETUP}, "pc": CODE + len(code) // 2,
                **{r: v for r, v in post.items() if r != "mem"}, "cycles": cycles}
        memory = window_bytes({**pre.get("mem", {}), **post.get("mem", {})})
        for base, values in memory.items():
            line = re.search(rf"^0x{base:04x} ((?:[0-9a-f]{{2}} ){{15}}[0-9a-f]{{2}})", block, re.M)
            if not line or bytes.fromhex(line[1]) != values:
                raise ValueError(f"conformance case {name}: memory at {base:#06x} differs from the manual")
        if got != want:
            diff = {r: (hex(got[r]), hex(want[r])) for r in want if got[r] != want[r]}
            raise ValueError(f"conformance case {name} ({code}) differs from the HC11 manual (got, expected): {diff}")
    return len(CASES)


def run_simulator(simulator, data, commands, state=None):
    """Run µCsim on an Intel HEX copy of data; return (stdout, saved checkpoint text or None).

    Commands go through a file (µCsim does not echo it, so output lines stay intact); "{dir}" in a
    command names the private temporary directory, where "in.state" holds the checkpoint to load and
    "out.state" receives the one saved. The conformance cases run last, after their own resets, so
    they cannot leak into the trace.
    """
    executable = shutil.which(str(simulator))
    if not executable:
        raise ValueError(f"µCsim executable not found: {simulator}")
    with tempfile.TemporaryDirectory(prefix="rallye-firmware-probe-") as directory:
        folder = Path(directory)
        hex_path, command_path = folder / "input.hex", folder / "commands.txt"
        hex_path.write_text(ihex(data), encoding="ascii")
        if state is not None:
            (folder / "in.state").write_text(state, encoding="ascii")
        commands = [c.replace("{dir}", directory) for c in commands]
        command_path.write_text("\n".join(commands + conformance_commands() + ["quit"]) + "\n", encoding="ascii")
        with tempfile.TemporaryFile() as out:
            # Output goes to a file and is size-checked before it is read back.
            subprocess.run([executable, "-q", "-R", "1", "-c", str(command_path), str(hex_path)],
                           stdin=subprocess.DEVNULL, stdout=out, stderr=subprocess.STDOUT,
                           timeout=SIM_TIMEOUT, check=True)
            if out.tell() > OUTPUT_LIMIT:
                raise ValueError("Simulator output exceeded the size limit")
            out.seek(0)
            output = out.read().decode("utf-8", "replace")
        saved = folder / "out.state"
        if saved.exists() and saved.stat().st_size > STATE_LIMIT:
            raise ValueError("Simulator checkpoint exceeded the size limit")
        saved = saved.read_text(encoding="ascii") if saved.exists() else None
    if re.search(r"Parser error|syntax error|Unknown command|Undefined subcommand|Invalid instruction", output, re.I):
        raise ValueError("Simulator rejected a command or instruction:\n" + output[-2000:])
    if "IAW8P" not in output and "iaw8p" not in output and "Undefined variable" in output:
        raise ValueError("Simulator lacks the IAW8P board model; run make emulator-setup")
    check_conformance(output)
    return output, saved


def probe(simulator, dump):
    data = dump.read_bytes()
    digest = check_source(data)
    commands = ["reset", "CC=0xd0", "step 5", "break 0xb218"]
    cases = []

    def case(name, row, column, column_fraction, row_fraction, synthetic=None):
        address = 0x91ab + row * 9 + column
        locations = [address, address + 1, address + 9, address + 10]
        cells = [data[location] for location in locations]
        commands.extend(["IY=0x2100", "SP=0x27ff", "CC=0xd0",
                         f"set memory rom 0x2100 {column} {column_fraction} {row} {row_fraction} 0x91 0xab 9"])
        if synthetic is not None:
            cells = synthetic
            for location, value in zip(locations, cells):
                commands.append(f"set memory rom {location} {value}")
        commands.extend(["pc 0xb1c5", "step 100"])
        cases.append({"name": name, "row": row, "column": column,
                      "column_fraction_256": column_fraction, "row_fraction_256": row_fraction,
                      "synthetic": synthetic is not None,
                      "expected_raw": expected_lookup(cells, column_fraction, row_fraction)})
        if synthetic is not None:
            for location in locations:
                commands.append(f"set memory rom {location} {data[location]}")

    # Each lookup reads four neighbors, so the final row/column cannot be a base.
    for row, column in [(0, 0), (7, 3), (15, 5), (22, 7)]:
        for fraction in [0, 128, 255]:
            case(f"original-r{row}-c{column}-f{fraction}", row, column, fraction, fraction)
    case("original-independent-fractions", 7, 3, 64, 192)
    case("synthetic-7f-80-midpoint", 0, 0, 128, 128, [127, 128, 127, 128])
    case("synthetic-00-ff-midpoint", 0, 0, 128, 128, [0, 255, 0, 255])
    case("synthetic-ff-00-midpoint", 0, 0, 128, 128, [255, 0, 255, 0])
    case("synthetic-row-7f-80-midpoint", 0, 0, 128, 128, [127, 127, 128, 128])
    case("synthetic-row-00-ff-midpoint", 0, 0, 128, 128, [0, 0, 255, 255])
    case("synthetic-row-ff-00-midpoint", 0, 0, 128, 128, [255, 255, 0, 0])

    output, _ = run_simulator(simulator, data, commands)
    stops = re.findall(r"Stop at 0x([0-9a-f]+):[^\n]*\nA= \$([0-9a-f]{2})", output, re.I)
    if len(stops) != 1 + len(cases) + len(CASES) or int(stops[0][0], 16) != 0xb80e:
        raise ValueError("Unexpected startup/lookup stops:\n" + output)
    for case_result, (pc, value) in zip(cases, stops[1:]):
        actual = int(value, 16)
        if int(pc, 16) != 0xb218 or actual != case_result["expected_raw"]:
            raise ValueError(f"Lookup mismatch: {case_result}, PC={pc}, A={actual}")
        case_result["actual_raw"] = actual
    if hashlib.sha256(dump.read_bytes()).hexdigest() != digest:
        raise ValueError("Source changed during the experiment")
    return {
        "source": str(dump.resolve()), "sha256": digest,
        "mapping": "hypothesis: file offset equals CPU address",
        "cpu_check": f"{len(CASES)} manual-derived instruction conformance cases: passed",
        "startup": {name: summary(run(simulator, data, name, MAX_RUN, DEFAULT_INPUTS if name == "h2" else None))
                    for name in BOARD},
        "lookup": "original instructions 0xB1C5..0xB217; stop before RTS at 0xB218",
        "scope": "lookup: isolated arithmetic only; startup: engine-stopped scheduler under the named profiles, not an engine simulation",
        "cases": cases,
    }


# --- parsing µCsim state blocks -------------------------------------------------

REGISTERS = re.compile(
    r"A= \$([0-9a-f]{2})[^\n]*B= \$([0-9a-f]{2})[^\n]*\nCC= ([01]{8})\n[^\n]*\n"
    r"IX= 0x([0-9a-f]{4})[^\n]*\nIY= 0x([0-9a-f]{4})[^\n]*\nSP= 0x([0-9a-f]{4})[^\n]*\n"
    r"0x([0-9a-f]{4,6}) +(?:[A-Z] +)?((?:[0-9a-f]{2} )+) *([A-Z]+|--)[ \t]*([^;\n]*?)[ \t]*(?:;[^\n]*)?$", re.M)
COUNTERS = re.compile(r"Inst= (\d+) Fetch= \d+ Read= \d+ Write= (\d+)$", re.M)
# µCsim echoes stdin in arbitrary chunks that can prefix any output line, so patterns are not
# anchored at line start and markers are expression results that never occur in the input text.
MARKER = 7177000
RAM_LINE = re.compile(r"0x00([0-9a-f])0 ((?:[0-9a-f]{2} ){16})", re.M)


def parse_state(block):
    """Registers, next instruction, counters and internal RAM from one marker block."""
    regs = REGISTERS.search(block)
    counters = COUNTERS.search(block)
    ram_lines = RAM_LINE.findall(block)
    if not regs or not counters or [int(row, 16) for row, _ in ram_lines] != list(range(16)):
        raise ValueError("Malformed simulator state output:\n" + block[-2000:])
    a, b, cc, x, y, sp, pc = (int(regs[1], 16), int(regs[2], 16), int(regs[3], 2), int(regs[4], 16),
                              int(regs[5], 16), int(regs[6], 16), int(regs[7], 16))
    return {
        "registers": {"pc": pc, "sp": sp, "a": a, "b": b, "x": x, "y": y, "cc": cc},
        "next": {"pc": pc, "bytes": regs[8].strip().upper(), "mnemonic": regs[9], "operand": regs[10].strip()},
        "instructions": int(counters[1]), "writes": int(counters[2]),
        "ram": bytes.fromhex("".join(line for _, line in ram_lines)),
    }


IAW8P_HEAD = re.compile(r"IAW8P profile=(\d) cycles=(\d+) inst=(\d+) tcnt=(\d+) cop_services=(\d+) "
                        r"cop_cycle=(\d+) rom_same_writes=(\d+) events=(\d+)$", re.M)
IAW8P_STOP = re.compile(r"^IAW8P-STOP kind=(\S+) addr=0x([0-9a-f]+) val=(-?\d+) pc=0x([0-9a-f]+) inst=(\d+) why=(.*)$", re.M)
SAMPLE = re.compile(r"^IAW8P-SAMPLE (\d+) (\d+) (\d+) (\d+) (\d+) (\d+) (\d+) (\d+) (\d+) (\d+) (\d+) \|((?: \d+)*)$", re.M)
CLKS = re.compile(r"since last reset= [0-9.e+-]+ sec \((\d+) clks\)")


def chunks(budget):
    """Split an instruction budget into at most SAMPLES nearly equal positive steps."""
    n = min(SAMPLES, budget)
    return [budget // n + (k < budget % n) for k in range(n)] if n else []


def parse_checkpoint(text):
    """The board model's checkpoint file: CPU registers, RAM, every board field and the event ring."""
    lines = text.split("\n")
    if lines[0] != "IAW8P-STATE 2" or "end" not in lines:
        raise ValueError("Malformed simulator checkpoint")
    state = {"fields": {}, "mem": {}, "ev": []}
    try:
        for line in lines[1:lines.index("end")]:
            tag, *p = line.split()
            if tag == "inst":
                state["inst"] = int(p[0])
            elif tag == "cpu":
                state["cpu"] = dict(zip(("a", "b", "x", "y", "sp", "pc", "cc"), map(int, p)))
            elif tag == "mem":
                state["mem"][int(p[0])] = bytes.fromhex(p[1])
            elif tag == "f":
                values = [int(v) for v in p[2:]]
                if len(values) != int(p[1]):
                    raise ValueError(p[0])
                state["fields"][p[0]] = values
            elif tag == "s":
                pairs = [int(v) for v in p[3:]]
                if len(pairs) != 2 * int(p[2]):
                    raise ValueError(p[0])
                state["fields"][p[0]] = dict(zip(pairs[0::2], pairs[1::2]))
            elif tag == "ev":
                state["ev"].append([int(v) for v in p])
            else:
                raise ValueError(tag)
    except (ValueError, IndexError) as error:
        raise ValueError(f"Malformed simulator checkpoint ({error})") from None
    if "inst" not in state or "cpu" not in state or len(state["ev"]) != 256 or set(state["mem"]) != {0, 0x2000}:
        raise ValueError("Incomplete simulator checkpoint")
    return state


def input_commands(profile, inputs):
    if profile != "h2":
        return []
    named = [("in1060", inputs["in1060"]), ("in1080", inputs["in1080"]), ("porta_in", inputs["porta_in"]),
             ("portd_in", inputs["portd_in"]), ("eeprom_fill", inputs["eeprom_fill"]), ("crank_rpm", inputs["crank_rpm"])]
    named += [(f"adc{k}", v) for k, v in enumerate(inputs["adc"])]
    return [f"iaw8p_{name}={value}" for name, value in named]


def board_trace(simulator, data, profile, budget, inputs, state):
    """One simulator process: from reset or from a checkpoint, `budget` more instructions, then save.

    Returns samples, the stop report and the parsed checkpoint after the last instruction.
    """
    commands = ["reset", "CC=0xd0", f"iaw8p_profile={BOARD[profile][0]}", f"iaw8p_loop_pc={LOOP_PC:#x}",
                *input_commands(profile, inputs)]
    if state is not None:
        commands.append('set hardware iaw8p load "{dir}/in.state"')
    sample = 'set hardware iaw8p sample "' + ",".join(f"{a:x}" for a, _ in PROBES) + '"'
    commands += [f"expr {MARK}*1000", "state", sample]
    for k, size in enumerate(chunks(budget), 1):
        commands += [f"step {size}", f"expr {MARK}*1000+{k}", sample]
    commands += [f"expr {MARK}*1000+498", "state", 'set hardware iaw8p save "{dir}/out.state"',
                 f"expr {MARK}*1000+499", "info registers", "info hardware iaw8p"]
    output, saved = run_simulator(simulator, data, commands, state)
    if state is not None and "IAW8P-LOADED" not in output:
        raise ValueError("Simulator rejected the checkpoint")
    if saved is None or "IAW8P-SAVED" not in output:
        raise ValueError("Simulator did not save a checkpoint; run make emulator-setup")
    parts = re.split(rf"^{MARK}(\d{{3}})$", output, flags=re.M)
    blocks = dict(zip(parts[1::2], parts[2::2]))
    samples = []
    for k in range(len(chunks(budget)) + 1):
        m = SAMPLE.search(blocks.get(f"{k:03d}", ""))
        if not m:
            raise ValueError(f"Malformed simulator sample {k}:\n" + blocks.get(f"{k:03d}", "")[-2000:])
        inst, cycles, pc, sp, a, b, x, y, cc, edges, pos = map(int, m.groups()[:11])
        probe = [int(v) for v in m[12].split()]
        if len(probe) != len(PROBES):
            raise ValueError(f"Malformed simulator sample {k}: probe bytes missing")
        samples.append({"inst": inst, "cycles": cycles, "pc": pc, "crank_edges": edges, "crank_pos": pos, "probe": probe,
                        "registers": {"pc": pc, "sp": sp, "a": a, "b": b, "x": x, "y": y, "cc": cc}})
    clks = [CLKS.search(blocks.get("000", "")), CLKS.search(blocks.get("498", ""))]
    report = blocks.get("499", "")
    head, regs = IAW8P_HEAD.search(report), REGISTERS.search(report)
    if not head or "IAW8P-END" not in report or int(head[1]) != BOARD[profile][0] or not all(clks) or not regs:
        raise ValueError("Simulator lacks the IAW8P board model or ignored the profile; run make emulator-setup")
    stop = IAW8P_STOP.search(report)
    return {
        "samples": samples, "clk_delta": int(clks[1][1]) - int(clks[0][1]), "text": saved,
        "state": parse_checkpoint(saved),
        "next": {"bytes": regs[8].strip().upper(), "text": f"{regs[9]} {regs[10].strip()}".strip()},
        "stop": stop and {"kind": stop[1], "address": int(stop[2], 16), "value": int(stop[3]), "pc": int(stop[4], 16),
                          "inst": int(stop[5]), "why": stop[6].strip()},
    }


def opcode_name(key):
    return ({0x100: "18 ", 0x200: "1A ", 0x300: "CD "}.get(key & 0x300, "") + f"{key & 0xFF:02X}")


def blocker_of(trace, limit):
    """The first instruction that must not run: an unsupported access, or an opcode without a conformance case."""
    found = None
    if trace["stop"]:
        s = trace["stop"]
        found = {"inst": s["inst"], "kind": s["kind"], "address": s["address"] if s["kind"] != "interrupt" else None,
                 "value": s["value"] if s["value"] >= 0 else None, "reason": s["why"]}
    unaudited = sorted((inst, key) for key, inst in trace["opcodes"].items() if key not in AUDITED and inst <= limit)
    if unaudited and (found is None or unaudited[0][0] <= found["inst"]):
        inst, key = unaudited[0]
        found = {"inst": inst, "kind": "unaudited_opcode", "address": None, "value": None,
                 "reason": f"opcode {opcode_name(key)} has no instruction conformance case yet; not trusted"}
    return found


def seal(sha, profile, inputs, text):
    """Checkpoint bound to its session identity (source hash, profile, inputs); not a secret."""
    blob = base64.b64encode(zlib.compress(text.encode("ascii"), 6)).decode("ascii")
    identity = json.dumps([sha, profile, inputs, blob], sort_keys=True)
    return {"state": blob, "digest": hashlib.sha256(identity.encode()).hexdigest()}


def unseal(sha, profile, inputs, checkpoint):
    if (not isinstance(checkpoint, dict) or set(checkpoint) != {"state", "digest"}
            or not all(isinstance(v, str) for v in checkpoint.values()) or len(checkpoint["state"]) > CHECKPOINT_LIMIT):
        raise ValueError("checkpoint must be {state, digest} strings within the size limit")
    identity = json.dumps([sha, profile, inputs, checkpoint["state"]], sort_keys=True)
    if hashlib.sha256(identity.encode()).hexdigest() != checkpoint["digest"]:
        raise ValueError("checkpoint belongs to another source, profile or input set; reset the session")
    try:
        raw = zlib.decompressobj().decompress(base64.b64decode(checkpoint["state"], validate=True), STATE_LIMIT + 1)
    except (ValueError, zlib.error):
        raise ValueError("checkpoint is not valid compressed data") from None
    if len(raw) > STATE_LIMIT:
        raise ValueError("checkpoint too large")
    return raw.decode("ascii")


def crank_state(f, inputs):
    """The simulated crank signal (stimulus side, not a firmware reading)."""
    pos = f["crank_pos"][0]
    return {"rpm": inputs["crank_rpm"], "edges": f["crank_edges"][0], "rises": f["crank_rises"][0], "position": pos,
            "tooth": pos // 2 + 1 if pos < 116 else None, "level": f["crank_level"][0], "residual": f["crank_acc"][0],
            "pacnt": f["reg"][0x27], "tic3": f["tic"][2] if f["tic_valid"][0] & 4 else None}


def describe(trace, data, profile, inputs, start, budget):
    """Response fields for the GUI, all from the checkpoint after the last executed instruction."""
    st, f = trace["state"], trace["state"]["fields"]
    reg, final = f["reg"], trace["samples"][-1]
    first, execs = f["first_pc"], f["exec_pc"]
    toc = f["toc"]
    sci_flags = (0 if f["sci_tdr_full"][0] else 0x80) | (
        0x40 if not (f["sci_tdr_full"][0] or f["sci_shifting"][0] or f["sci_preamble"][0]) else 0)

    def value(r):
        special = {0x00: f["pa_pins"][0], 0x0E: f["tcnt"][0] >> 8, 0x0F: f["tcnt"][0] & 0xFF, 0x2E: sci_flags,
                   0x2F: f["sci_tdr"][0]}
        special.update({0x16 + 2 * k: toc[k] >> 8 for k in range(5)})
        special.update({0x17 + 2 * k: toc[k] & 0xFF for k in range(5)})
        special.update({0x31 + k: f["adr"][k] for k in range(4)})
        return special.get(r, reg[r])

    nev = f["nev"][0]
    ring = {e[0]: e[1:] for e in st["ev"]}
    events = [[k, *ring[k % 256][:3], chr(ring[k % 256][5]), ring[k % 256][3], ring[k % 256][4]]
              for k in range(max(0, nev - 256), nev)]
    events = [[k, inst, cycle, pc, kind, addr, val] for k, inst, cycle, pc, kind, addr, val in events]
    last = lambda ring_bytes, n: [ring_bytes[k % 32] for k in range(max(0, n - 32), n)]
    loop_n = f["loop_n"][0]
    return {
        "profile": profile, "board": BOARD[profile][1], "core": CORE, "start": start, "budget": budget,
        "cycles": final["cycles"], "e_clock_hz": E_CLOCK_HZ, "registers": final["registers"],
        "samples": [[s["inst"], s["cycles"], s["pc"]] for s in trace["samples"]],
        "milestones": [[address, name, first.get(address)] for address, name in MILESTONES],
        "watch": [[address, name, execs.get(address, 0)] for address, name in WATCH],
        "regions": [[name, lo, hi, *f["counts"][3 * k:3 * k + 3]] for k, (name, lo, hi) in enumerate(REGIONS)],
        "io": [[0x1000 + r, REGISTER_NAMES.get(r, ""), value(r), f["reg_rd"][r], f["reg_wr"][r],
                f["portd_unknown"][0] if r == 0x08 else 0]
               for r in range(64) if f["reg_rd"][r] or f["reg_wr"][r]],
        "latches": [[a, f[n][0]] for a, n, h in ((0x1040, "latch1040", "has_latch1040"), (0x1050, "latch1050", "has_latch1050"))
                    if f[h][0]],
        "events": events, "events_total": nev,
        "pages": [[p, c] for p, c in enumerate(f["pages"]) if c],
        "cop_services": f["cop_services"][0], "cop_cycle": f["cop_cycle"][0], "cop_gap_max": f["cop_gap_max"][0],
        "tcnt": f["tcnt"][0], "rom_same_writes": f["rom_same_writes"][0],
        "opcodes": {"executed": len(trace["opcodes"]), "audited": len(AUDITED)},
        "interrupts": [[k, SOURCES[k], VECTORS[k], n, f["irq_cycle"][k]] for k, n in enumerate(f["irq_n"]) if n],
        "rti": f["rti_n"][0],
        "loop": {"pc": LOOP_PC, "count": loop_n, "sp_min": f["loop_sp_min"][0] if loop_n else None,
                 "sp_max": f["loop_sp_max"][0] if loop_n else None, "gap_max": f["loop_gap_max"][0]},
        "outputs": {"porta_latch": f["pa_latch"][0], "porta_pins": f["pa_pins"][0], "porta_changes": f["pa_changes"],
                    "porta_high_cycles": f["pa_high_w"], "porta_low_cycles": f["pa_low_w"],
                    "latch_changes": f["latch_changes"],
                    "spi_bytes": f["spi_n"][0], "spi_last": last(f["spi_last"], f["spi_n"][0]),
                    "sci_bytes": f["sci_n"][0], "sci_last": last(f["sci_last"], f["sci_n"][0])},
        "eeprom": {"init": bool(f["ee_init"][0]), "ops": f["ee_ops"][0], "bytes": bytes(f["ee"]).hex()}
        if profile == "h2" else None,
        "ram": {"internal": st["mem"][0].hex(), "external": st["mem"][0x2000].hex()},
        # Per sample: instruction, E cycle, crank falling edges so far, then the PROBES bytes.
        "trace": [[s["inst"], s["cycles"], s["crank_edges"], *s["probe"]] for s in trace["samples"]],
        "probes": [[a, name] for a, name in PROBES],
        "crank": crank_state(f, inputs) if profile == "h2" else None,
        "inputs": inputs,
    }


# Inputs fixed for a whole session: the EEPROM fill only matters at its first access after reset.
SESSION_INPUTS = {"eeprom_fill"}


def changed_inputs(profile, before, after):
    """Names of H2 inputs that differ between a checkpoint's inputs and the next request's."""
    if profile != "h2":
        if before is not None or after is not None:
            raise ValueError("inputs are only accepted by the h2 simulation profile")
        return []
    changed = sorted(k for k in after if before[k] != after[k])
    fixed = [k for k in changed if k in SESSION_INPUTS]
    if fixed:
        raise ValueError(f"{', '.join(fixed)} cannot change during a session; reset the session")
    return changed


def run(simulator, data, profile, budget, inputs=None, checkpoint=None, sha=None, checkpoint_inputs=None):
    """Continue a session for at most `budget` instructions under one board profile.

    Without a checkpoint the session starts at reset. When an instruction would need unmodelled
    hardware (or an unaudited opcode), the request is repeated from the same start and stops just
    before it, so every reported state was produced only by modelled behaviour. Chunked continuation
    through checkpoints executes exactly like one long run (see the tests).
    """
    sha = sha or hashlib.sha256(data).hexdigest()
    # A checkpoint is verified against the inputs it was sealed with; changed inputs take effect at
    # its instruction boundary (the next A/D conversion, crank edge or port read) and the new
    # checkpoint is sealed with the new inputs.
    before = inputs if checkpoint_inputs is None else checkpoint_inputs
    changed = changed_inputs(profile, before, inputs) if checkpoint is not None else []
    state = unseal(sha, profile, before, checkpoint) if checkpoint is not None else None
    start = parse_checkpoint(state)["inst"] if state is not None else 0

    def trace_for(n):
        t = board_trace(simulator, data, profile, n, inputs, state)
        t["opcodes"] = t["state"]["fields"]["first_op"]
        return t

    trace = trace_for(budget)
    blocker = blocker_of(trace, start + budget)
    if blocker:
        trace = trace_for(blocker["inst"] - 1 - start)
        if trace["stop"] or blocker_of(trace, blocker["inst"] - 1):
            raise ValueError("Replay before the blocked instruction was not clean; result discarded")
    final = trace["samples"][-1]
    executed = blocker["inst"] - 1 if blocker else start + budget
    begin = trace["samples"][0]
    if (final["inst"] != executed or trace["state"]["inst"] != executed or begin["inst"] != start
            or final["cycles"] - begin["cycles"] != trace["clk_delta"]):
        raise ValueError("Simulator counters disagree with the board model; result discarded")
    if blocker:
        blocker.update(pc=final["pc"], bytes=trace["next"]["bytes"], text=trace["next"]["text"])
        del blocker["inst"]
    return {**describe(trace, data, profile, inputs, start, budget), "executed": executed, "blocker": blocker,
            "inputs_changed": changed,
            "checkpoint": None if blocker else seal(sha, profile, inputs, trace["text"])}


def summary(result):
    b = result["blocker"]
    where = f"blocked before ${b['pc']:04X} ({b['kind']}: {b['reason']})" if b else "budget reached"
    stage = [name for _, name, inst in result["milestones"] if inst is not None][-1:] or ["none"]
    loop = result["loop"]["count"]
    irq = sum(n for *_, n, _ in result["interrupts"])
    return (f"{result['executed']} instructions, {result['cycles']} E cycles, last landmark: {stage[0]}; "
            f"{loop} main-loop passes, {irq} interrupts, {result['rti']} RTI; {where}")


def lookup(simulator, data, row, column, row_fraction, column_fraction, edits):
    """One call of the original routine 0xB1C5 with a constructed descriptor; stops before RTS."""
    image = bytearray(data)
    for address, value in edits:
        image[address] = value
    if image[:TABLE_FIRST] != data[:TABLE_FIRST] or image[TABLE_LAST + 1:] != data[TABLE_LAST + 1:]:
        raise ValueError("Working copy differs outside the table region")
    base = 0x91ab + row * 9 + column
    locations = [base, base + 1, base + 9, base + 10]
    commands = ["reset", "IY=0x2100", "SP=0x27ff", "CC=0xd0",
                f"set memory rom 0x2100 {column} {column_fraction} {row} {row_fraction} 0x91 0xab 9",
                "break 0xb218", "pc 0xb1c5", "step 100", f"expr {MARKER // 1000}*1000", "info registers", "state",
                "dump /h rom 0 0xff 16"]
    output, _ = run_simulator(simulator, bytes(image), commands)
    end = re.split(rf"{MARKER}$", output, flags=re.M)
    if len(end) != 2:
        raise ValueError("Missing simulator end marker:\n" + output[-2000:])
    state = parse_state(re.split(rf"^{MARK}\d{{3}}$", end[1], flags=re.M)[0])  # conformance output follows
    registers = state["registers"]
    if registers["pc"] != 0xb218:
        raise ValueError(f"Routine stopped at ${registers['pc']:04X}, not before RTS at $B218")
    cells = [image[location] for location in locations]
    expected = expected_lookup(cells, column_fraction, row_fraction)
    if registers["a"] != expected:
        raise ValueError(f"Simulator result {registers['a']} differs from the recovered arithmetic {expected}")
    return {"working_sha256": hashlib.sha256(image).hexdigest(), "edits": len(edits),
            "row": row, "column": column, "row_fraction": row_fraction, "column_fraction": column_fraction,
            "result": registers["a"], "cells": [[location, image[location]] for location in locations],
            "instructions": state["instructions"], "registers": registers}


# --- JSON bridge ------------------------------------------------------------------

COMMON = {"version", "action", "simulator", "sha256", "image"}
FIELDS = {"run": COMMON | {"profile", "budget", "inputs", "checkpoint", "checkpoint_inputs"},
          "lookup": COMMON | {"row", "column", "row_fraction", "column_fraction", "edits"}}


def integer(request, name, low, high):
    value = request[name]
    if type(value) is not int or not low <= value <= high:
        raise ValueError(f"{name} must be an integer {low}..{high}")
    return value


def validate_inputs(profile, inputs):
    """H2 takes every simulated input explicitly; strict and H1 take none."""
    if profile != "h2":
        if inputs is not None:
            raise ValueError("inputs are only accepted by the h2 simulation profile")
        return None
    if not isinstance(inputs, dict) or set(inputs) != set(INPUT_LIMITS) | {"adc"}:
        raise ValueError(f"h2 inputs must be exactly {sorted(set(INPUT_LIMITS) | {'adc'})}")
    for name, high in INPUT_LIMITS.items():
        integer(inputs, name, 0, high)
    adc = inputs["adc"]
    if not isinstance(adc, list) or len(adc) != 8 or any(type(v) is not int or not 0 <= v <= 255 for v in adc):
        raise ValueError("adc must be 8 raw A/D results 0..255 (channels AN0..AN7)")
    if inputs["porta_in"] & 0x81:
        raise ValueError("porta_in bits 0 and 7 must be 0: in h2, PA0 and PA7 carry the simulated crank signal")
    return inputs


def handle(request):
    """Validate one request strictly, run it, and return the response payload."""
    if not isinstance(request, dict) or not isinstance(request.get("action"), str) or request["action"] not in FIELDS:
        raise ValueError("action must be 'run' or 'lookup'")
    if set(request) != FIELDS[request["action"]]:
        raise ValueError(f"request fields must be exactly {sorted(FIELDS[request['action']])}")
    if type(request["version"]) is not int or request["version"] != PROTOCOL:
        raise ValueError(f"protocol version must be {PROTOCOL}")
    if not isinstance(request["simulator"], str) or not request["simulator"]:
        raise ValueError("simulator must be a path")
    if request["action"] == "run":
        if not isinstance(request["profile"], str) or request["profile"] not in BOARD:
            raise ValueError(f"profile must be one of {sorted(BOARD)}")
        params = (request["profile"], integer(request, "budget", 0, MAX_RUN),
                  validate_inputs(request["profile"], request["inputs"]), request["checkpoint"])
        if (request["checkpoint"] is None) != (request["checkpoint_inputs"] is None) and request["profile"] == "h2":
            raise ValueError("checkpoint_inputs must accompany a checkpoint (the inputs it was sealed with), and only then")
        before = request["checkpoint_inputs"]
        before = None if before is None else validate_inputs(request["profile"], before)
    else:
        edits = request["edits"]
        if not isinstance(edits, list) or len(edits) > TABLE_LAST - TABLE_FIRST + 1:
            raise ValueError("edits must be a list of at most 216 [address, value] pairs")
        for edit in edits:
            if (not isinstance(edit, list) or len(edit) != 2 or any(type(v) is not int for v in edit)
                    or not TABLE_FIRST <= edit[0] <= TABLE_LAST or not 0 <= edit[1] <= 255):
                raise ValueError(f"edit {edit!r} is outside the table region 0x91AB..0x9282 or not a byte")
        if len({edit[0] for edit in edits}) != len(edits):
            raise ValueError("edits contain duplicate addresses")
        params = (integer(request, "row", 0, 22), integer(request, "column", 0, 7),
                  integer(request, "row_fraction", 0, 255), integer(request, "column_fraction", 0, 255), edits)
    image, sha = request["image"], request["sha256"]
    if not isinstance(image, str) or len(image) != 2 * 65536 or not re.fullmatch(r"[0-9a-f]+", image):
        raise ValueError("image must be 65536 bytes as lowercase hex")
    data = bytes.fromhex(image)
    if not isinstance(sha, str) or hashlib.sha256(data).hexdigest() != sha:
        raise ValueError("sha256 does not match the image")
    check_source(data)
    if request["action"] == "run":
        result = run(request["simulator"], data, *params, sha=sha, checkpoint_inputs=before)
    else:
        result = lookup(request["simulator"], data, *params)
    return {"version": PROTOCOL, "ok": True, "action": request["action"], "sha256": sha,
            "profile_name": SUPPORTED[sha], **result}


def bridge():
    # Cancellation (SIGTERM from the GUI) unwinds normally: the running µCsim is killed by
    # subprocess.run and the temporary directory is removed.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    raw = sys.stdin.read(REQUEST_LIMIT + 1)
    try:
        if len(raw) > REQUEST_LIMIT:
            raise ValueError("request too large")
        response = handle(json.loads(raw))
    except subprocess.TimeoutExpired:
        response = {"version": PROTOCOL, "ok": False, "error": f"simulator timed out after {SIM_TIMEOUT} s"}
    except subprocess.CalledProcessError as error:
        response = {"version": PROTOCOL, "ok": False,
                    "error": f"simulator exited with {error.returncode}: {(error.stderr or '')[-1000:]}"}
    except (OSError, ValueError) as error:  # json.JSONDecodeError is a ValueError
        response = {"version": PROTOCOL, "ok": False, "error": str(error)}
    print(json.dumps(response))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--simulator", type=Path)
    parser.add_argument("--dump", type=Path)
    parser.add_argument("--bridge", action="store_true", help="answer one JSON request from stdin")
    args = parser.parse_args()
    if args.bridge:
        return bridge()
    if not args.simulator or not args.dump:
        parser.error("--simulator and --dump are required")
    try:
        print(json.dumps(probe(args.simulator, args.dump), indent=2))
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(1, f"error: {error}\n")


if __name__ == "__main__":
    main()
