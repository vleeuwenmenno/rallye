#!/usr/bin/env python3
"""Package the built Linux x64 app and pinned simulator, without any ECU files."""

import hashlib
import os
import platform
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import urllib.request
import zipfile

import firmware_probe


ROOT = Path(__file__).resolve().parent.parent
SIM = ROOT / "target/ucsim"
DIST = ROOT / "target/dist"
NAME = "rallye-linux-x86_64"


def appimage_asset(name, url, digest):
    """Cache immutable upstream packaging tools, checking their published SHA-256."""
    cache = ROOT / "target/appimage-tools"
    cache.mkdir(parents=True, exist_ok=True)
    path = cache / name
    if not path.exists() or hashlib.sha256(path.read_bytes()).hexdigest() != digest:
        with urllib.request.urlopen(url, timeout=60) as response:
            data = response.read()
        if hashlib.sha256(data).hexdigest() != digest:
            raise ValueError(f"Checksum mismatch for {name}")
        path.write_bytes(data)
    path.chmod(0o755)
    return path


def build_appimage(bundle, stage):
    tool = appimage_asset("appimagetool", "https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage",
                          "ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0")
    runtime = appimage_asset("runtime-x86_64", "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64",
                             "2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d")
    appdir = stage / "Rallye.AppDir"
    shutil.copytree(bundle, appdir)
    shutil.copy2(appdir / "rallye", appdir / "AppRun")
    (appdir / "rallye.desktop").write_text(
        "[Desktop Entry]\nType=Application\nName=Rallye\nExec=rallye\nIcon=rallye\n"
        "Comment=Offline ECU firmware workbench\nCategories=Development;\nTerminal=false\n")
    (appdir / "rallye.svg").write_text('''<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256" viewBox="0 0 256 256">
<rect width="256" height="256" rx="48" fill="#171c24"/>
<path d="M52 180a88 88 0 1 1 152 0" fill="none" stroke="#58b4ff" stroke-width="18" stroke-linecap="round"/>
<path d="M128 144l52-64" stroke="#ffb52e" stroke-width="14" stroke-linecap="round"/>
<circle cx="128" cy="144" r="14" fill="#ffb52e"/>
</svg>''')
    output = DIST / f"{NAME}.AppImage"
    env = {**os.environ, "ARCH": "x86_64"}
    subprocess.run([str(tool), "--appimage-extract-and-run", "--runtime-file", str(runtime),
                    str(appdir), str(output)], env=env, check=True)
    output.chmod(0o755)
    # Extraction mode also works on CI runners without FUSE.
    subprocess.run([str(output), "--appimage-extract-and-run", "--help"], check=True, stdout=subprocess.DEVNULL)
    return output


def main():
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise SystemExit("Release packaging currently supports Linux x86_64 only")
    DIST.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="rallye-package-") as temp:
        stage = Path(temp)
        bundle = stage / NAME
        (bundle / "bin").mkdir(parents=True)
        (bundle / "libexec").mkdir()
        (bundle / "licenses").mkdir()
        shutil.copy2(ROOT / "target/release/rallye", bundle / "bin/rallye")
        shutil.copy2(SIM / "src/sims/m68hc11.src/ucsim_m68hc11", bundle / "libexec/ucsim_m68hc11")
        shutil.copy2(SIM / "COPYING", bundle / "licenses/ucsim-COPYING")
        shutil.copy2(ROOT / "docs/releases.md", bundle / "README.md")
        launcher = bundle / "rallye"
        launcher.write_text('''#!/bin/sh
set -eu
bundle=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
export UCSIM="$bundle/libexec/ucsim_m68hc11"
exec "$bundle/bin/rallye" "$@"
''')
        launcher.chmod(0o755)
        # Exercise the actual packaged programs without loading any private firmware.
        subprocess.run([str(launcher), "--help"], check=True, stdout=subprocess.DEVNULL)
        firmware_probe.run_simulator(str(bundle / "libexec/ucsim_m68hc11"), bytes(65536), [])
        revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
        sim_revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=SIM, text=True).strip()
        (bundle / "BUILD.txt").write_text(f"Rallye commit: {revision}\nµCsim base: {sim_revision}\n")
        # Ship the complete upstream source with exactly the patch used by emulator-setup.
        source = stage / "ucsim-source"
        source.mkdir()
        archive = stage / "upstream.tar"
        subprocess.run(["git", "archive", "--format=tar", "-o", str(archive), "HEAD"], cwd=SIM, check=True)
        with tarfile.open(archive) as tar:
            tar.extractall(source, filter="data")
        subprocess.run(["git", "apply", str(ROOT / "tools/ucsim-hc11-probe.patch")], cwd=source, check=True)
        shutil.copy2(ROOT / "tools/ucsim-hc11-probe.patch", source / "rallye.patch")
        (source / "RALLYE-BUILD.txt").write_text(
            f"Upstream: https://github.com/danieldrotos/ucsim.git\nCommit: {sim_revision}\n"
            "rallye.patch is already applied. Build on Linux with:\n"
            "./configure --enable-m68hc11-sim\nmake preps\nmake libs\n"
            "make -C src/sims/m68hc11.src\nSee COPYING for the license.\n")
        for folder, filename in [(bundle, f"{NAME}.tar.gz"), (source, "ucsim-source.tar.gz")]:
            with tarfile.open(DIST / filename, "w:gz") as tar:
                tar.add(folder, arcname=folder.name)
        zipped = DIST / f"{NAME}.zip"
        with zipfile.ZipFile(zipped, "w", compression=zipfile.ZIP_DEFLATED) as archive:
            for path in sorted(bundle.rglob("*")):
                archive.write(path, path.relative_to(stage))
        appimage = build_appimage(bundle, stage)
        files = [DIST / f"{NAME}.tar.gz", zipped, appimage, DIST / "ucsim-source.tar.gz"]
        (DIST / "SHA256SUMS").write_text("".join(
            f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n" for path in files))
    print(f"Release assets: {DIST}")


if __name__ == "__main__":
    main()
