# Inputs default to the parent directory; override with: make run DIR=/path
DIR ?= ..
PROBE_DUMP ?= $(DIR)/106XSI2.ORI
# Pinned µCsim research backend, built inside target/ by `make emulator-setup` (no system install).
UCSIM_REV := 59a14ff99efe7e92474faf4c4ddf57b5e4ff0631
UCSIM_SRC := $(CURDIR)/target/ucsim
# An UCSIM from the environment or command line overrides the project-local build.
UCSIM ?= $(UCSIM_SRC)/src/sims/m68hc11.src/ucsim_m68hc11
# The simulator's own makefiles must not inherit this project's variables (DIR, UCSIM, ...).
SUBMAKE := env -u MAKEFLAGS -u MFLAGS -u MAKEOVERRIDES make

.PHONY: build check test run report firmware-probe emulator-setup emulator-conformance release-package

build:
	cargo build --locked --release

emulator-conformance:
	PYTHONPATH=tools UCSIM="$(UCSIM)" python3 -B -c 'import os, firmware_probe as f; f.run_simulator(os.environ["UCSIM"], bytes(65536), []); print(f"Passed {len(f.CASES)} simulator conformance cases")'

release-package: build emulator-setup
	python3 -B tools/package_release.py

check:
	cargo check --locked
	cargo clippy --locked -- -D warnings

# Simulator tests in tools/ are skipped when UCSIM or the identified dumps are missing.
test:
	DUMP_DIR="$(DIR)" cargo test --locked
	UCSIM="$(UCSIM)" DUMP_DIR="$(DIR)" python3 -B -m unittest discover -s tools -t tools

run: build
	UCSIM="$(UCSIM)" ./target/release/rallye "$(DIR)"

report: build
	./target/release/rallye report "$(DIR)"

# Optional research tool; the GUI's Emulation tab uses the same script and simulator.
firmware-probe:
	python3 tools/firmware_probe.py --simulator "$(UCSIM)" --dump "$(PROBE_DUMP)"

# Idempotent: fetches only the pinned commit, applies the reviewed patch once, builds only the HC11 simulator.
# A tree carrying an older patch version gets its tracked sources restored to the pinned commit first.
emulator-setup:
	@test "$$(git -C "$(UCSIM_SRC)" rev-parse HEAD 2>/dev/null)" = "$(UCSIM_REV)" || { \
	  mkdir -p "$(UCSIM_SRC)" && git -C "$(UCSIM_SRC)" init -q && \
	  git -C "$(UCSIM_SRC)" fetch -q --depth 1 https://github.com/danieldrotos/ucsim.git $(UCSIM_REV) && \
	  git -C "$(UCSIM_SRC)" checkout -q --force --detach FETCH_HEAD; }
	git -C "$(UCSIM_SRC)" apply --reverse --check "$(CURDIR)/tools/ucsim-hc11-probe.patch" 2>/dev/null || { \
	  git -C "$(UCSIM_SRC)" checkout -q $(UCSIM_REV) -- src && \
	  git -C "$(UCSIM_SRC)" apply "$(CURDIR)/tools/ucsim-hc11-probe.patch"; }
	cd "$(UCSIM_SRC)" && { test -f Makefile || ./configure --enable-m68hc11-sim; } && \
	  $(SUBMAKE) preps && $(SUBMAKE) -j4 libs && $(SUBMAKE) -C src/sims/m68hc11.src -j4
	@echo "µCsim ready: $(UCSIM_SRC)/src/sims/m68hc11.src/ucsim_m68hc11"
