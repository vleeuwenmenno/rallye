"""Tests for the µCsim bridge. Simulator tests run only when UCSIM and the identified dumps exist."""

import hashlib
import json
import os
import re
from pathlib import Path
import shutil
import subprocess
import sys
import unittest

import firmware_probe as fp

SCRIPT = Path(fp.__file__).read_text(encoding="utf-8")
UCSIM = os.environ.get("UCSIM", "")
DUMP_DIR = Path(os.environ.get("DUMP_DIR", ".."))
DUMPS = [DUMP_DIR / "106XSI2.ORI", DUMP_DIR / "REMAP MILD EURO95 RUNNING.BIN"]
ZEROS = bytes(65536)


def request(action="run", image=ZEROS, **fields):
    base = {"version": 4, "action": action, "simulator": "ucsim_m68hc11",
            "sha256": hashlib.sha256(image).hexdigest(), "image": image.hex()}
    if action == "run":
        base.update(profile="strict", budget=100, inputs=None, checkpoint=None, checkpoint_inputs=None)
    else:
        base.update(row=7, column=3, row_fraction=192, column_fraction=64, edits=[])
    base.update(fields)
    return base


def bridge(payload):
    """Run the bridge exactly as the GUI does: isolated interpreter, script text, JSON on stdin."""
    result = subprocess.run([sys.executable, "-I", "-c", SCRIPT, "--bridge"], input=payload,
                            capture_output=True, text=True, timeout=60, check=True)
    return json.loads(result.stdout)


class Validation(unittest.TestCase):
    def rejects(self, req, message):
        with self.assertRaisesRegex(ValueError, message):
            fp.handle(req)

    def test_fields_must_match_exactly(self):
        self.rejects(request(extra=1), "fields must be exactly")
        req = request()
        del req["budget"]
        self.rejects(req, "fields must be exactly")
        self.rejects(request(action="boot"), "action must be")
        self.rejects(request(action=["run"]), "action must be")
        self.rejects(request(version=1), "protocol version")
        self.rejects(request(version=True), "protocol version")

    def test_bounds_and_types(self):
        for budget in (-1, fp.MAX_RUN + 1, True, 2.0, "2"):
            self.rejects(request(budget=budget), "budget must be")
        for profile in ("H1", "loose", 1, None):
            self.rejects(request(profile=profile), "profile must be")
        for name, bad in (("row", 23), ("column", 8), ("row_fraction", 256),
                          ("column_fraction", -1), ("row", False)):
            self.rejects(request("lookup", **{name: bad}), f"{name} must be")

    def test_inputs_only_for_h2_and_complete(self):
        self.rejects(request(inputs=fp.DEFAULT_INPUTS), "only accepted by the h2")
        self.rejects(request(profile="h2"), "h2 inputs must be exactly")
        self.rejects(request(profile="h2", inputs={**fp.DEFAULT_INPUTS, "extra": 1}), "h2 inputs must be exactly")
        self.rejects(request(profile="h2", inputs={**fp.DEFAULT_INPUTS, "portd_in": 64}), "portd_in must be")
        self.rejects(request(profile="h2", inputs={**fp.DEFAULT_INPUTS, "adc": [0] * 7}), "adc must be")
        self.rejects(request(profile="h2", inputs={**fp.DEFAULT_INPUTS, "adc": [256] + [0] * 7}), "adc must be")
        self.rejects(request(profile="h2", inputs={**fp.DEFAULT_INPUTS, "crank_rpm": 9001}), "crank_rpm must be")
        self.rejects(request(profile="h2", inputs={**fp.DEFAULT_INPUTS, "porta_in": 0x01}), "PA0 and PA7 carry")
        # A checkpoint must name the inputs it was sealed with, and only a checkpoint may.
        self.rejects(request(profile="h2", inputs=fp.DEFAULT_INPUTS, checkpoint_inputs=fp.DEFAULT_INPUTS), "checkpoint_inputs")
        self.rejects(request(profile="h2", inputs=fp.DEFAULT_INPUTS, checkpoint={"state": "", "digest": ""}), "checkpoint_inputs")

    def test_live_input_changes(self):
        """Every H2 input except the EEPROM power-up fill may change at a chunk boundary."""
        changed = {**fp.DEFAULT_INPUTS, "crank_rpm": 900, "adc": [0x80, 0x80, 0x40] + [0x80] * 5}
        self.assertEqual(fp.changed_inputs("h2", fp.DEFAULT_INPUTS, changed), ["adc", "crank_rpm"])
        with self.assertRaisesRegex(ValueError, "eeprom_fill cannot change"):
            fp.changed_inputs("h2", fp.DEFAULT_INPUTS, {**fp.DEFAULT_INPUTS, "eeprom_fill": 0})
        self.assertEqual(fp.changed_inputs("h1", None, None), [])

    def test_checkpoint_is_bound_to_session_identity(self):
        sealed = fp.seal("a" * 64, "h2", fp.DEFAULT_INPUTS, "IAW8P-STATE 2\nend\n")
        self.assertEqual(fp.unseal("a" * 64, "h2", fp.DEFAULT_INPUTS, sealed), "IAW8P-STATE 2\nend\n")
        for sha, profile, inputs in (("b" * 64, "h2", fp.DEFAULT_INPUTS), ("a" * 64, "h1", None),
                                     ("a" * 64, "h2", {**fp.DEFAULT_INPUTS, "in1060": 0x20})):
            with self.assertRaisesRegex(ValueError, "another source, profile or input set"):
                fp.unseal(sha, profile, inputs, sealed)
        with self.assertRaisesRegex(ValueError, "checkpoint must be"):
            fp.unseal("a" * 64, "h2", fp.DEFAULT_INPUTS, {"state": sealed["state"]})
        with self.assertRaisesRegex(ValueError, "Malformed simulator checkpoint"):
            fp.parse_checkpoint("IAW8P-STATE 2\nf reg 2 1\nend")
        with self.assertRaisesRegex(ValueError, "Malformed simulator checkpoint"):
            fp.parse_checkpoint("IAW8P-STATE 1\nend")  # older layout without crank and capture state

    def test_edits_only_inside_the_table(self):
        for edit in ([0x91AA, 1], [0x9283, 1], [0x91AB, 256], [0x91AB], [0x91AB, True]):
            self.rejects(request("lookup", edits=[edit]), "outside the table region")
        self.rejects(request("lookup", edits=[[0x91AB, 1], [0x91AB, 2]]), "duplicate")

    def test_image_hash_and_profile(self):
        self.rejects(request(image=bytes(100)), "65536 bytes")
        self.rejects(request(sha256="0" * 64), "does not match")
        self.rejects(request(image=ZEROS), "Unsupported dump")

    def test_bridge_reports_errors_as_json(self):
        self.assertEqual(bridge("{")["ok"], False)
        self.assertIn("Unsupported dump", bridge(json.dumps(request()))["error"])


class Arithmetic(unittest.TestCase):
    def test_independent_fractions_are_not_swapped(self):
        cells = [10, 200, 50, 90]
        self.assertEqual(fp.expected_lookup(cells, 64, 192), 60)
        self.assertEqual(fp.expected_lookup(cells, 192, 64), 135)
        self.assertEqual(fp.expected_lookup([0, 255, 0, 255], 128, 128), 128)


class Conformance(unittest.TestCase):
    def test_cases_are_well_formed(self):
        names = [c[0] for c in fp.CASES]
        self.assertEqual(len(names), len(set(names)))
        for name, code, pre, post, cycles in fp.CASES:
            self.assertTrue(1 <= len(bytes.fromhex(code)) <= 4 and 2 <= cycles <= 48, name)  # IDIV 41, IRQ cases
            self.assertLessEqual(set(pre) | set(post), {"a", "b", "x", "y", "sp", "cc", "pc"} | fp.SETUP, name)
            fp.window_bytes({**pre.get("mem", {}), **post.get("mem", {})})  # stays inside the checked windows

    def test_opcode_keys_follow_prefix_pages(self):
        self.assertEqual([fp.opcode_key(bytes.fromhex(c)) for c in ("86", "18a7", "1a83", "cdac")], [0x86, 0x1A7, 0x283, 0x3AC])
        self.assertEqual(fp.opcode_name(0x3AC), "CD AC")
        self.assertIn(0x13A, fp.AUDITED)  # ABY: an upstream bug used X instead of Y

    def test_unaudited_opcode_becomes_the_blocker(self):
        trace = {"stop": {"kind": "read", "address": 0x1060, "value": -1, "pc": 0xA86A, "inst": 50, "why": "w"},
                 "opcodes": {0x86: 1, 0x3E: 30}}
        self.assertEqual(fp.blocker_of(trace, 100)["kind"], "unaudited_opcode")  # WAI first ran at 30 < 50
        trace["opcodes"] = {0x86: 1}
        blocker = fp.blocker_of(trace, 100)
        self.assertEqual((blocker["kind"], blocker["address"], blocker["value"], blocker["inst"]), ("read", 0x1060, None, 50))
        self.assertIsNone(fp.blocker_of({"stop": None, "opcodes": {0x86: 1}}, 100))

    def test_chunks_cover_the_budget(self):
        for budget in (0, 1, 63, 64, 65, 200_000):
            parts = fp.chunks(budget)
            self.assertEqual((sum(parts), len(parts) <= fp.SAMPLES, all(p > 0 for p in parts)), (budget, True, True))


STATE = """sA= $01   1   +1 .  B= $00   0   +0 .  D= $0100   256  +256
CC= 11010000
      HINZVC
IX= 0x0000 00 00 00 00 00 00 00 00 ........
IY= 0x0000 00 00 00 00 00 00 00 00 ........
SP= 0x27ff 00 00 00 00 00 00 00 00 ........
0xb80e    b7 10 3d          STA    A,$103d            ; [$103d]=$00
CPU state= OK PC= 0x00b80e frequency= 8000000 HZ
Inst= 5 Fetch= 13 Read= 1 Write= 3
""" + "".join(("re" if row == 0 else "") + f"0x00{row:x}0" + " 01" * 16 + " ................\n" for row in range(16))


class Parse(unittest.TestCase):
    def test_echo_prefixed_state(self):
        state = fp.parse_state(STATE)
        self.assertEqual(state["registers"], {"pc": 0xB80E, "sp": 0x27FF, "a": 1, "b": 0, "x": 0, "y": 0, "cc": 0xD0})
        self.assertEqual(state["next"], {"pc": 0xB80E, "bytes": "B7 10 3D", "mnemonic": "STA", "operand": "A,$103d"})
        self.assertEqual((state["instructions"], state["writes"], state["ram"]), (5, 3, bytes([1]) * 256))

    def test_incomplete_state_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "Malformed"):
            fp.parse_state(STATE.split("0x00f0")[0])


@unittest.skipUnless(UCSIM and shutil.which(UCSIM) and all(d.is_file() for d in DUMPS),
                     "needs UCSIM (make emulator-setup) and both identified dumps in DUMP_DIR")
class Simulator(unittest.TestCase):
    def test_strict_run_models_startup_registers_and_stops_at_external_io(self):
        for dump in DUMPS:
            result = fp.run(UCSIM, dump.read_bytes(), "strict", fp.MAX_RUN)
            self.assertEqual((result["executed"], result["registers"]["pc"]), (20, 0xB834))
            blocker = result["blocker"]
            self.assertEqual((blocker["kind"], blocker["address"], blocker["value"], blocker["bytes"]), ("write", 0x1040, 6, "B7 10 40"))
            io = {a: (v, r, w) for a, _, v, r, w, unknown in result["io"]}
            self.assertTrue(all(row[5] == 0 for row in result["io"]))
            # INIT, OPTION, TMSK2, PORTD, DDRD, HPRIO (PSEL only), ADCTL: each written once by the original code
            self.assertEqual(io, {0x103D: (0x01, 0, 1), 0x1039: (0xBA, 0, 1), 0x1024: (0x01, 0, 1), 0x1008: (0xFF, 0, 1),
                                  0x1009: (0x1A, 0, 1), 0x103C: (0x21, 0, 1), 0x1030: (0x14, 0, 1)})
            self.assertEqual(result["latches"], [])
            self.assertEqual([m[2] for m in result["milestones"][:4]], [1, 6, 7, None])

    def test_h1_runs_copy_loop_and_ram_test_then_stops_at_board_input(self):
        for dump in DUMPS:
            result = fp.run(UCSIM, dump.read_bytes(), "h1", fp.MAX_RUN)
            blocker = result["blocker"]
            self.assertEqual((result["registers"]["pc"], blocker["kind"], blocker["address"]), (0xA86A, "read", 0x1060))
            self.assertEqual((result["latches"], result["rom_same_writes"]), ([[0x1040, 6], [0x1050, 3]], 4921))
            self.assertGreaterEqual(result["cop_services"], 4)
            reached = {name: inst for _, name, inst in result["milestones"]}
            self.assertIsNotNone(reached["RAM check and RAM test"])
            self.assertIsNone(reached["Interrupts enabled (CLI)"])
            self.assertEqual(result["opcodes"]["executed"], 81)
            io = {row[0]: row for row in result["io"]}
            self.assertEqual(io[0x1030][2], 0x94)  # CCF completes without a register read.
            self.assertNotEqual(io[0x1008][5], 0)  # RMW retains unknown input bits.

    def test_replay_is_deterministic_and_prefix_consistent(self):
        data = DUMPS[0].read_bytes()
        full = fp.run(UCSIM, data, "h1", 40_000)
        self.assertEqual(full, fp.run(UCSIM, data, "h1", 40_000))
        part = fp.run(UCSIM, data, "h1", 1_000)
        self.assertIsNone(part["blocker"])
        self.assertEqual(part["executed"], 1_000)
        # the same instruction count reached through a different chunking gives the same machine state
        self.assertEqual(fp.run(UCSIM, data, "h1", 37)["registers"], fp.run(UCSIM, data, "h1", 37)["registers"])
        first = {name: inst for _, name, inst in full["milestones"]}
        self.assertEqual(first["Copy loop $8000-$9338"], {name: inst for _, name, inst in part["milestones"]}["Copy loop $8000-$9338"])

    def test_unsupported_access_stops_before_the_instruction(self):
        data = DUMPS[0].read_bytes()
        result = fp.run(UCSIM, data, "strict", 20)  # exactly up to the blocked write: not yet a blocker
        self.assertIsNone(result["blocker"])
        self.assertEqual(fp.run(UCSIM, data, "strict", 21)["blocker"]["pc"], 0xB834)

    def snippet(self, code, steps, profile="strict", inputs=None):
        """Run hand-written code from external RAM on the board model; return (stop, registers, RAM $50-$5F)."""
        commands = ["reset", "CC=0xd0", f"iaw8p_profile={fp.BOARD[profile][0]}",
                    *fp.input_commands(profile, inputs or fp.DEFAULT_INPUTS),
                    "set memory rom 0x2400 " + " ".join(str(b) for b in bytes.fromhex(code)),
                    "pc 0x2400", f"step {steps}", "expr 7177499", "info hardware iaw8p", "dump /h rom 0x50 0x5f 16"]
        out, _ = fp.run_simulator(UCSIM, DUMPS[0].read_bytes(), commands)
        report = out.split("7177499", 1)[1]
        stop = fp.IAW8P_STOP.search(report)
        regs = {int(a, 16): int(v) for a, v in re.findall(r"^IAW8P-REG 0x([0-9a-f]+) (\d+) ", report, re.M)}
        ram = bytes.fromhex(re.search(r"^0x0050 ((?:[0-9a-f]{2} ){15}[0-9a-f]{2})", report, re.M)[1])
        return (stop[1], int(stop[2], 16)) if stop else None, regs, ram

    def test_time_protected_and_once_only_registers(self):
        # INIT $01 once inside the first 64 E cycles; a second write stops.
        self.assertEqual(self.snippet("8601b7103d", 2)[0], None)
        self.assertEqual(self.snippet("8601b7103db7103d", 3)[0], ("register_write", 0x103D))
        # OPTION: IRQE/DLY/CR1:0 ($33) keep their early value after 64 cycles; ADPU/CME may change.
        _, regs, _ = self.snippet("86bab71039" + "3d" * 7 + "4fb71039", 11)
        self.assertEqual(regs[0x1039], 0x32)
        # TMSK2 prescaler bits are frozen after 64 cycles; enables are not.
        _, regs, _ = self.snippet("3d" * 7 + "86c3b71024", 9)
        self.assertEqual(regs[0x1024], 0xC0)
        # Only the first early write can set the prescaler; later writes still change enables.
        self.assertEqual(self.snippet("8601b7102486f2b71024", 4)[1][0x1024], 0xF1)
        self.assertEqual(self.snippet("8602b71024", 2)[1][0x1024], 0x02)
        # HPRIO: PSEL may change, mode bits may not.
        self.assertEqual(self.snippet("8621b7103c", 2)[1][0x103C], 0x21)
        self.assertEqual(self.snippet("86e6b7103c", 2)[0], ("register_write", 0x103C))

    def test_portd_unknown_inputs_cannot_become_outputs(self):
        # BSET copies unknown input pins into the latch. LDA resolves flags, not latch bits.
        self.assertEqual(self.snippet("ce10001c080186ffb71009", 4)[0], ("register_write", 0x1009))
        # A full known store replaces those bits before changing the direction.
        self.assertIsNone(self.snippet("ce10001c08018600b71008863fb71009", 6)[0])
        # A bit explicitly forced by BSET is known and may become an output.
        self.assertIsNone(self.snippet("ce10001c08028602b71009", 4)[0])

    def test_adc_completion_is_visible_without_reading_adctl(self):
        self.assertEqual(self.snippet("8680b710398614b71030", 4)[1][0x1030], 0x14)
        self.assertEqual(self.snippet("8680b710398614b71030" + "3d" * 13, 17)[1][0x1030], 0x94)

    def test_timer_flags_counter_and_adc_timing(self):
        # TCNT runs at E/1 after reset: two reads 9 E cycles apart (LDD ext 5 + STD dir 4).
        _, _, ram = self.snippet("fc100edd54fc100edd56", 4)
        self.assertEqual(int.from_bytes(ram[6:8], "big") - int.from_bytes(ram[4:6], "big"), 9)
        # ~66,000 cycles: TOF (bit 7) after 65,536 and RTIF (bit 6) every 8,192; writing 1 clears only that flag.
        _, _, ram = self.snippet("ce2af80926fdb610259752" + "8680b71025b610259753", 2 * 11000 + 7)
        self.assertEqual((ram[2], ram[3]), (0xC0, 0x40))
        # ADC: CCF is clear right after ADCTL is written and set after four 32-cycle conversions.
        _, _, ram = self.snippet("8680b710398614b71030b610309750" + "3d" * 13 + "b610309751", 21)
        self.assertEqual((ram[0], ram[1]), (0x14, 0x94))
        self.assertEqual(self.snippet("8614b71030", 2)[0], ("register_write", 0x1030))  # ADPU=0: not powered

    def test_unsupported_accesses_stop(self):
        self.assertEqual(self.snippet("b61031", 1)[0], ("adc_result", 0x1031))  # sensor values are not invented
        self.assertEqual(self.snippet("b61008", 1)[0], ("port_read", 0x1008))
        self.assertEqual(self.snippet("b61060", 1)[0], ("read", 0x1060))
        self.assertEqual(self.snippet("b60200", 1)[0], ("read", 0x0200))
        self.assertEqual(self.snippet("b61002", 1)[0], ("register_read", 0x1002))  # PIOC: not modelled
        self.assertEqual(self.snippet("b6100e", 1)[0], None)
        # EPROM-address writes: strict always stops; H1 drops only a write of the byte already there.
        same = DUMPS[0].read_bytes()[0x8000]
        code = f"86{same:02x}b78000"
        self.assertEqual(self.snippet(code, 2)[0], ("write", 0x8000))
        self.assertEqual(self.snippet(code, 2, "h1")[0], None)
        self.assertEqual(self.snippet(f"86{same ^ 1:02x}b78000", 2, "h1")[0], ("write", 0x8000))
        self.assertEqual(self.snippet("8606b71040", 2, "h1")[0], None)
        self.assertEqual(self.snippet("8606b71041", 2, "h1")[0], ("write", 0x1041))
        # Strict and H1 do not know the IRQ pin level: clearing I stops; H2 assumes the pin inactive.
        self.assertEqual(self.snippet("8680b710240e", 3)[0], ("interrupt", 0))
        self.assertEqual(self.snippet("8680b710240e", 3, "h1")[0], ("interrupt", 0))
        self.assertIsNone(self.snippet("0e01", 2, "h2")[0])
        # Nothing executes after the first stop.
        stop, _, ram = self.snippet("b6106086019750", 3)
        self.assertEqual((stop, ram[0]), (("read", 0x1060), 0))

    def test_h2_supplied_inputs_and_board_readback(self):
        h2 = dict(profile="h2", inputs={**fp.DEFAULT_INPUTS, "in1060": 0xA5, "in1080": 0x3C, "porta_in": 0x06})
        _, _, ram = self.snippet("b610609750b610809751b6100097528606b710404fb61040975386f0b71000b610009754", 15, **h2)
        # $1060, $1080 supplied; PORTA reads PA1/PA2 as supplied, PA0/PA7 from the crank signal (wheel at rest in
        # the gap: high), plus output pins (low after reset); latch read-back.
        self.assertEqual(list(ram[:5]), [0xA5, 0x3C, 0x87, 0x06, 0x87 | 0x70])
        self.assertEqual(self.snippet("b61060", 1, "h2", {**fp.DEFAULT_INPUTS})[0], None)
        self.assertEqual(self.snippet("b61050", 1, "h2")[0], ("read", 0x1050))  # never written: no read-back value

    def test_h2_adc_results_come_from_supplied_channels(self):
        adc = dict(profile="h2", inputs={**fp.DEFAULT_INPUTS, "adc": [10, 11, 12, 13, 14, 15, 16, 17]})
        wait = "3d" * 13  # 130 cycles: four 32-cycle conversions
        # MULT=1, CD:CC=01: AN4..AN7; MULT=0, channel 2: four conversions of AN2
        _, _, ram = self.snippet("8680b710398614b71030" + wait + "fc1031dd50fc1033dd52", 21, **adc)
        self.assertEqual(list(ram[:4]), [14, 15, 16, 17])
        _, _, ram = self.snippet("8680b710398602b71030" + wait + "fc1031dd50fc1033dd52", 21, **adc)
        self.assertEqual(list(ram[:4]), [12, 12, 12, 12])
        self.assertEqual(self.snippet("8680b710398614b71030b61031", 5, **adc)[0], ("adc_result", 0x1031))  # not complete

    def test_h2_spi_master_timing_and_unknown_received_data(self):
        # SPCR=$5C (master, E/2): 8 bits take 16 E cycles; SPSR read then SPDR access clears SPIF.
        code = "865cb71028" + "8655b7102a" + "b610299750" + "3d3d" + "b610299751" + "b6102a86009752" + "b610299753"
        _, regs, ram = self.snippet(code, 15, "h2")
        self.assertEqual((ram[0], ram[1], ram[3]), (0x00, 0x80, 0x00))
        # Received data may only be loaded and then discarded; using it stops.
        self.assertEqual(self.snippet("865cb71028b6102a4c", 4, "h2")[0], ("flags", 0x2408))

    def test_h2_eeprom_programming_sequence(self):
        h2 = dict(profile="h2", inputs={**fp.DEFAULT_INPUTS, "eeprom_fill": 0xFF})
        delay = "18ce0d05" + "1809" + "26fc"  # 3333 x (DEY 4 + BNE 3) > 20,000 E cycles (10 ms at 2 MHz)
        program = "8602b7103b" + "860fb7b610" + "8603b7103b" + delay + "4fb7103b" + "b6b6109750"
        _, _, ram = self.snippet(program, 6 + 1 + 3333 * 2 + 2 + 2, **h2)
        self.assertEqual(ram[0], 0x0F)  # programming only clears bits of the erased $FF
        # EELAT and EEPGM set by one write: neither is set, nothing is programmed.
        _, regs, _ = self.snippet("8603b7103b", 2, **h2)
        self.assertEqual(regs[0x103B], 0x00)
        # VPP on for less than 10 ms: result undefined, stops.
        self.assertEqual(self.snippet("8602b7103b860fb7b6108603b7103b4fb7103b", 8, **h2)[0], ("register_write", 0x103B))
        self.assertEqual(self.snippet("8602b7103bb6b610", 3, **h2)[0], ("read", 0xB610))  # EELAT=1: not readable

    def test_h2_output_compare_pin_action(self):
        # TCTL1 OM3:OL3=11 (set PA5 on OC3); CFORC FOC3 forces it; PORTA reads the pin back.
        _, _, ram = self.snippet("8630b710208620b7100bb610009750", 7, "h2")
        self.assertEqual(ram[0] & 0x20, 0x20)
        # PORTA latch writes do not reach a pin an output compare owns, and do once it is released.
        _, _, ram = self.snippet("8620b710204fb71000b6100097508600b71020b610009751", 9, "h2", {**fp.DEFAULT_INPUTS})
        self.assertEqual((ram[0] & 0x20, ram[1] & 0x20), (0x00, 0x00))

    def test_h2_sci_transmitter_flags(self):
        # BAUD=$30 (E/16/13), TE on: preamble, then a character; TDRE/TC from the double-buffered transmitter.
        code = "8630b7102b8608b7102d" + "b6102e9750" + "8655b7102f" + "b6102e9751"
        _, _, ram = self.snippet(code, 8, "h2")
        self.assertEqual((ram[0], ram[1]), (0x80, 0x00))  # preamble queued: TC=0; then TDR full: TDRE=0, TC=0

    def test_lookups_match_recovered_arithmetic(self):
        data = DUMPS[0].read_bytes()
        result = fp.lookup(UCSIM, data, 7, 3, 192, 64, [])
        cells = [data[a] for a in (0x91AB + 66, 0x91AB + 67, 0x91AB + 75, 0x91AB + 76)]
        self.assertEqual(result["result"], fp.expected_lookup(cells, 64, 192))
        self.assertEqual(result["working_sha256"], hashlib.sha256(data).hexdigest())
        # Boundary base row 22 / column 7 reads the table's last byte 0x9282; edit it in the working copy.
        original = fp.lookup(UCSIM, data, 22, 7, 255, 255, [])
        edited = fp.lookup(UCSIM, data, 22, 7, 255, 255, [[0x9282, data[0x9282] ^ 0xFF]])
        self.assertEqual(edited["cells"][3], [0x9282, data[0x9282] ^ 0xFF])
        self.assertNotEqual(edited["working_sha256"], original["working_sha256"])
        self.assertEqual(edited["result"], fp.expected_lookup([c for _, c in edited["cells"]], 255, 255))
        self.assertEqual(edited["registers"]["pc"], 0xB218)

    def test_bridge_end_to_end(self):
        data = DUMPS[1].read_bytes()
        response = bridge(json.dumps(request("lookup", image=data, simulator=UCSIM, row=0, column=0,
                                             row_fraction=0, column_fraction=0)))
        self.assertTrue(response["ok"], response)
        self.assertEqual((response["result"], response["profile_name"]), (data[0x91AB], "REMAP MILD EURO95 RUNNING.BIN (mild remap)"))
        response = bridge(json.dumps(request(image=data, simulator=UCSIM, budget=3)))
        self.assertEqual((response["executed"], response["blocker"], len(response["samples"])), (3, None, 4))
        # A session continues from the returned checkpoint; another input set may not reuse it.
        first = bridge(json.dumps(request(image=data, simulator=UCSIM, profile="h2", budget=1000, inputs=fp.DEFAULT_INPUTS)))
        nxt = bridge(json.dumps(request(image=data, simulator=UCSIM, profile="h2", budget=1000, inputs=fp.DEFAULT_INPUTS,
                                        checkpoint=first["checkpoint"], checkpoint_inputs=fp.DEFAULT_INPUTS)))
        self.assertEqual((nxt["start"], nxt["executed"], nxt["inputs_changed"]), (1000, 2000, []))
        other = {**fp.DEFAULT_INPUTS, "in1060": 0x20, "crank_rpm": 900}
        # Claiming the checkpoint was sealed with other inputs is refused ...
        stale = bridge(json.dumps(request(image=data, simulator=UCSIM, profile="h2", budget=1000, inputs=other,
                                          checkpoint=first["checkpoint"], checkpoint_inputs=other)))
        self.assertIn("another source, profile or input set", stale["error"])
        # ... while naming its real inputs applies the new ones from that instruction on.
        live = bridge(json.dumps(request(image=data, simulator=UCSIM, profile="h2", budget=1000, inputs=other,
                                         checkpoint=first["checkpoint"], checkpoint_inputs=fp.DEFAULT_INPUTS)))
        self.assertEqual((live["start"], live["executed"], live["inputs_changed"], live["inputs"]),
                         (1000, 2000, ["crank_rpm", "in1060"], other))
        again = bridge(json.dumps(request(image=data, simulator=UCSIM, profile="h2", budget=10, inputs=other,
                                          checkpoint=live["checkpoint"], checkpoint_inputs=fp.DEFAULT_INPUTS)))
        self.assertIn("another source, profile or input set", again["error"])  # the new checkpoint carries the new inputs

    def test_h2_reaches_repeated_foreground_work_and_timer_interrupts(self):
        """Engine-stopped scheduler: main loop, timer ISRs entered and returned, SP stable; both $1060 bit-5 states."""
        for dump in DUMPS:
            for bit5 in (0, 0x20):
                inputs = {**fp.DEFAULT_INPUTS, "in1060": bit5}
                r = fp.run(UCSIM, dump.read_bytes(), "h2", fp.MAX_RUN, inputs)
                self.assertIsNone(r["blocker"], r["blocker"])
                watch = {address: n for address, _, n in r["watch"]}
                irq = {name: n for _, name, _, n, _ in r["interrupts"]}
                self.assertGreater(r["loop"]["count"], 300)
                self.assertEqual((r["loop"]["sp_min"], r["loop"]["sp_max"]), (0x27FF, 0x27FF))
                self.assertGreater(irq["TOF"], 20)
                self.assertGreater(irq["RTI"], 600)
                self.assertEqual(watch[0x743C], irq["TOF"])  # every TOF entry runs the handler ...
                self.assertEqual(watch[0x78C5], irq["RTI"])
                self.assertIn(sum(irq.values()) - r["rti"], (0, 1))  # ... and returns (one may be in progress)
                self.assertEqual(watch[0x7477], irq["TOF"])  # TOF handler sets $B5 bit 0 ...
                self.assertIn(watch[0xD4EC], (irq["TOF"], irq["TOF"] - 1))  # ... and the foreground consumes it
                # $A833 branch on $1060 bit 5 sets or clears RAM $B1 bit 2.
                self.assertEqual(bytes.fromhex(r["ram"]["internal"])[0xB1] & 4, 4 if bit5 else 0)

    def test_chunked_session_equals_one_shot(self):
        data = DUMPS[0].read_bytes()
        for profile, inputs, total, split in (("h2", fp.DEFAULT_INPUTS, 400_000, 123_457), ("h1", None, 50_000, 20_011)):
            one = fp.run(UCSIM, data, profile, total, inputs)
            a = fp.run(UCSIM, data, profile, split, inputs)
            b = fp.run(UCSIM, data, profile, total - split, inputs, a["checkpoint"])
            unpack = lambda r: fp.unseal(hashlib.sha256(data).hexdigest(), profile, inputs, r["checkpoint"])
            self.assertEqual(unpack(one), unpack(b))
            keep = lambda r: {k: v for k, v in r.items() if k not in ("start", "budget", "samples", "trace", "checkpoint")}
            self.assertEqual(keep(one), keep(b))
            self.assertEqual((b["start"], b["samples"][0][0], b["samples"][-1]), (split, split, one["samples"][-1]))
        # H1 still stops before $A86A when continued from a checkpoint; no checkpoint past a blocker.
        a = fp.run(UCSIM, data, "h1", 60_000)
        b = fp.run(UCSIM, data, "h1", fp.MAX_RUN, None, a["checkpoint"])
        self.assertEqual((b["executed"], b["registers"]["pc"], b["checkpoint"]), (68_202, 0xA86A, None))

    def test_bounded_long_run_through_checkpoints(self):
        data = DUMPS[1].read_bytes()
        r, loops = None, []
        for _ in range(5):
            r = fp.run(UCSIM, data, "h2", fp.MAX_RUN, fp.DEFAULT_INPUTS, r and r["checkpoint"])
            self.assertIsNone(r["blocker"])
            loops.append(r["loop"]["count"])
            self.assertLess(len(r["checkpoint"]["state"]), fp.CHECKPOINT_LIMIT)
        self.assertEqual(r["executed"], 5 * fp.MAX_RUN)
        self.assertTrue(all(b - a > 300 for a, b in zip(loops, loops[1:])), loops)  # steady progress per chunk
        self.assertEqual((r["loop"]["sp_min"], r["loop"]["sp_max"]), (0x27FF, 0x27FF))


def firmware_rpm(period):
    """$429B: IDIV 58594/$BB, FDIV remainder/$BB; $D1:$D2 = low quotient byte : high fraction byte."""
    q, r = divmod(58594, period)
    return ((q & 0xFF) << 8) | (((r << 16) // period) >> 8)


@unittest.skipUnless(UCSIM and shutil.which(UCSIM) and all(d.is_file() for d in DUMPS),
                     "needs UCSIM (make emulator-setup) and both identified dumps in DUMP_DIR")
class Crank(unittest.TestCase):
    """Simulated 60-2 crank signal on PA7/PA0 and the firmware's own response to it (H2)."""

    snippet = Simulator.snippet

    def test_pulse_accumulator_and_input_capture(self):
        # 6000 rpm: one half-tooth per 2e6 / 12000 = 166.7 E cycles. From rest in the gap, falling edges come
        # at E cycles ceil(166.7 k) for k = 1, 3: 167 and 500; the rising edge between at 334. TCNT runs at E/1 after reset.
        crank = dict(profile="h2", inputs={**fp.DEFAULT_INPUTS, "crank_rpm": 6000})
        setup = "8640b71026" "8602b71021" "86feb71027"  # PACTL PAEN falling; TCTL2 IC3 falling; PACNT=$FE
        read = "b610259750" "b610279751" "b610239752" "fc1014dd53"
        stop, _, ram = self.snippet(setup + "3d" * 55 + read, 6 + 55 + 8, **crank)
        self.assertIsNone(stop)
        # Two counted edges: $FE -> $FF -> $00 sets PAOVF and PAIF; IC3 captured TCNT at the second falling edge.
        self.assertEqual((ram[0] & 0x30, ram[1], ram[2] & 0x01, int.from_bytes(ram[3:5], "big")), (0x30, 0x00, 0x01, 500))
        # PEDGE=1 and EDG3A: only the rising edge at E cycle 334 counts and is captured.
        setup = "8650b71026" "8601b71021" "86feb71027"
        _, _, ram = self.snippet(setup + "3d" * 55 + read, 6 + 55 + 8, **crank)
        self.assertEqual((ram[0] & 0x30, ram[1], ram[2] & 0x01, int.from_bytes(ram[3:5], "big")), (0x10, 0xFF, 0x01, 334))
        # Stopped wheel: no edge, and TIC3 has no defined content yet.
        stop, _, ram = self.snippet(setup + "3d" * 55 + "b610279751", 6 + 55 + 2, "h2")
        self.assertEqual((stop, ram[1]), (None, 0xFE))
        self.assertEqual(self.snippet("fc1014", 1, "h2")[0], ("register_read", 0x1014))
        # Gated time accumulation is not modelled while a crank signal exists.
        self.assertEqual(self.snippet("8660b71026", 2, **crank)[0], ("register_write", 0x1026))

    def test_unknown_division_remainder_may_not_be_used(self):
        self.assertIsNone(self.snippet("ce000002" "8f" "ce1234" "dd50", 5, "h2")[0])  # IDIV /0, XGDX, LDX #
        self.assertEqual(self.snippet("ce000002" "4c", 3, "h2")[0], ("flags", 0x2404))  # INCA reads the remainder
        self.assertEqual(self.snippet("ce000002" "8f" "08", 4, "h2")[0], ("flags", 0x2405))  # INX after XGDX

    def test_multiple_unknown_inputs_do_not_erase_existing_taint(self):
        for serial in ("102a", "102f"):
            # Undefined remainder moved into X must survive a separate unknown A load.
            self.assertEqual(self.snippet("ce0000028fb6" + serial + "4fff0050", 6, "h2")[0],
                             ("register_read", int(serial, 16)))
            # An unknown B load must not hide the still-unknown A value.
            self.assertEqual(self.snippet("b6" + serial + "f6" + serial + "5f9750", 4, "h2")[0],
                             ("register_read", int(serial, 16)))

    def test_interrupt_preserves_unknown_flags_after_stacked_accumulator_overwrite(self):
        commands = ["reset", "CC=0xd0", "SP=0x25f0", "iaw8p_profile=2",
                    *fp.input_commands("h2", fp.DEFAULT_INPUTS),
                    "set memory rom 0x2400 0x0e 0xb6 0x10 0x2a 0x26 0x00 0x01",
                    "set memory rom 0x2420 0x86 0x80 0xb7 0x10 0x25 0x86 0x01 0xb7 0x25 0xea 0x3b",
                    "set memory rom 0xffde 0x24 0x20",
                    "set hardware iaw8p reg 0x24 0x80", "set hardware iaw8p reg 0x25 0x80",
                    "pc 0x2400", "step 8", "expr 7177499", "info hardware iaw8p"]
        out, _ = fp.run_simulator(UCSIM, DUMPS[0].read_bytes(), commands)
        stop = fp.IAW8P_STOP.search(out.split("7177499", 1)[1])
        self.assertIsNotNone(stop)
        self.assertEqual((stop[1], int(stop[2], 16)), ("flags", 0x2404))

    def session(self, data, plan, inputs=None):
        """Run (inputs-update, chunks) steps as one session, changing inputs at chunk boundaries."""
        before, cp, out = inputs or {**fp.DEFAULT_INPUTS}, None, []
        for update, n in plan:
            for _ in range(n):
                now = {**before, **update}
                r = fp.run(UCSIM, data, "h2", fp.MAX_RUN, now, cp, checkpoint_inputs=before if cp else None)
                self.assertIsNone(r["blocker"], r["blocker"])
                out.append(r)
                before, cp = now, r["checkpoint"]
        return out

    @staticmethod
    def ram(r, address):
        internal, external = bytes.fromhex(r["ram"]["internal"]), bytes.fromhex(r["ram"]["external"])
        return internal[address] if address < 0x100 else external[address - 0x2000]

    def word(self, r, address):
        return self.ram(r, address) << 8 | self.ram(r, address + 1)

    def test_firmware_rpm_follows_commanded_speed_on_both_dumps(self):
        for dump in DUMPS:
            data = dump.read_bytes()
            runs = self.session(data, [({}, 1), ({"crank_rpm": 250}, 2), ({"crank_rpm": 900}, 2),
                                       ({"crank_rpm": 3000}, 2), ({"crank_rpm": 6500}, 2), ({"crank_rpm": 0}, 2)])
            self.assertEqual([r["start"] for r in runs], [k * fp.MAX_RUN for k in range(len(runs))])  # never reset
            for r in runs[1:-2]:
                rpm, period = r["inputs"]["crank_rpm"], self.word(r, 0xBB)
                # The firmware times 30 teeth (180 degrees) at 2 us per TCNT count: 15e6 / rpm counts ...
                self.assertLessEqual(abs(period - 15_000_000 / rpm), 2, (dump.name, rpm, period))
                # ... and divides that period itself; $D1 is its result, not an echo of the input.
                self.assertEqual(self.word(r, 0xD1), firmware_rpm(period))
                self.assertLessEqual(abs(self.word(r, 0xD1) - rpm), max(2, rpm // 1000))
                self.assertTrue(self.ram(r, 0xAD) & 0x10)  # gap verified at tooth 1 ($9381)
                crank = r["crank"]
                self.assertGreater(crank["edges"], 0)
                self.assertIn("PAOV", {name for _, name, _, _, _ in r["interrupts"]})
            # Crank stopped: the firmware times out on its own ($BB = $FFFF, $D1 = 0, stall regime $AE bit 0).
            stopped = runs[-1]
            self.assertEqual((self.word(stopped, 0xBB), self.word(stopped, 0xD1), self.ram(stopped, 0xAE) & 1), (0xFFFF, 0, 1))
            self.assertEqual(self.ram(stopped, 0xAD) & 0x10, 0)
            self.assertEqual(hashlib.sha256(dump.read_bytes()).hexdigest(), fp.check_source(data))

    def test_throttle_candidate_reaches_firmware_values(self):
        data = DUMPS[0].read_bytes()
        an2 = lambda v: {"adc": [0x80, 0x80, v, 0x80, 0x80, 0x80, 0x80, 0x80]}
        runs = self.session(data, [({"crank_rpm": 900}, 2), (an2(0x40), 1), (an2(0xD0), 1), (an2(0x90), 1)])
        values = [(self.ram(r, 0x2003), self.ram(r, 0xC6), self.ram(r, 0x200F)) for r in runs[1:]]
        # Raw AN2 is copied to $2003 (A/D sequence $4034/$4108) and validated into $C6 and $200F ($5FAF) ...
        self.assertEqual(values, [(0x80,) * 3, (0x40,) * 3, (0xD0,) * 3, (0x90,) * 3])
        # ... $AA bit 6 is set near the learned closed endpoint $0E, bit 7 near the open one $0D ($7A1E).
        self.assertEqual([self.ram(r, 0xAA) & 0x40 for r in runs[1:3]], [0, 0x40])
        self.assertEqual([self.ram(r, 0xAA) & 0x80 for r in runs[1:]], [0, 0, 0x80, 0])
        self.assertEqual((self.ram(runs[-1], 0x0E), self.ram(runs[-1], 0x0D)), (data[0x88FE], data[0x88FD]))
        self.assertEqual(runs[2]["inputs_changed"], ["adc"])

    def test_crank_session_chunks_equal_one_run(self):
        data = DUMPS[1].read_bytes()
        inputs = {**fp.DEFAULT_INPUTS, "crank_rpm": 1234}
        warm = fp.run(UCSIM, data, "h2", 300_000, inputs)
        one = fp.run(UCSIM, data, "h2", 600_000, inputs, warm["checkpoint"])
        a = fp.run(UCSIM, data, "h2", 277_777, inputs, warm["checkpoint"])
        b = fp.run(UCSIM, data, "h2", 600_000 - 277_777, inputs, a["checkpoint"])
        self.assertNotEqual(a["crank"]["residual"], 0)  # split inside a half tooth
        unpack = lambda r: fp.unseal(hashlib.sha256(data).hexdigest(), "h2", inputs, r["checkpoint"])
        self.assertEqual(unpack(one), unpack(b))
        self.assertEqual((one["crank"], one["trace"][-1]), (b["crank"], b["trace"][-1]))

    def test_engine_stopped_baseline_is_unchanged(self):
        r = fp.run(UCSIM, DUMPS[0].read_bytes(), "h2", fp.MAX_RUN, fp.DEFAULT_INPUTS)
        irq = {name: n for _, name, _, n, _ in r["interrupts"]}
        self.assertEqual((r["loop"]["count"], irq), (424, {"RTI": 961, "TOF": 30}))
        self.assertEqual((r["crank"]["edges"], r["crank"]["tooth"], r["crank"]["level"]), (0, None, 1))

if __name__ == "__main__":
    unittest.main()
