# Linux x64 releases

Publishing a GitHub Release triggers `.github/workflows/release-linux.yml`. It builds
the release tag on Ubuntu 24.04 x86_64 with Rust 1.99.0, builds the pinned and patched
µCsim, runs checks, and attaches these assets to that release:

- `rallye-linux-x86_64.AppImage`: single-file app and simulator.
- `rallye-linux-x86_64.zip` and `.tar.gz`: app, simulator, launcher and simulator license.
- `ucsim-source.tar.gz`: complete simulator source with the project's patch applied.
- `SHA256SUMS`: SHA-256 checksums for all four assets.

The workflow also supports **Run workflow** for testing a selected branch/tag. Manual
runs keep artifacts in Actions; they do not create or modify a release. Re-running a
release build replaces its assets of the same name.

## Run a downloaded build

For AppImage (fish-compatible):

```fish
chmod +x rallye-linux-x86_64.AppImage
./rallye-linux-x86_64.AppImage /path/to/your/dumps
```

If FUSE mounting is unavailable, use
`./rallye-linux-x86_64.AppImage --appimage-extract-and-run /path/to/your/dumps`.
For ZIP, extract it with `unzip rallye-linux-x86_64.zip` and use the same launcher
as the tarball below. All formats contain the same app and simulator.

Extract the app archive and run its top-level launcher (fish-compatible commands):

```fish
tar -xzf rallye-linux-x86_64.tar.gz
./rallye-linux-x86_64/rallye /path/to/your/dumps
```

The launcher sets `UCSIM` to the bundled simulator. Python 3, an X11 or Wayland desktop,
working Vulkan drivers, and the shared system libraries are still required. This is a
Linux x64 glibc build targeting Ubuntu 24.04 or compatible newer systems. The AppImage
packages the app and simulator; it does not bundle Python, system libraries or GPU
drivers. On Ubuntu 24.04, runtime packages include:

```fish
sudo apt-get install python3 libxcb1 libxcb-xkb1 libxkbcommon0 libxkbcommon-x11-0 libwayland-client0 libvulkan1 libfontconfig1 libfreetype6 libncurses6 libncursesw6 libstdc++6
```

ECU dumps and XDFs are not included or uploaded. Firmware-dependent tests skip on
GitHub because those private files are absent; the standalone CPU conformance suite
still runs against the built simulator with a synthetic image. Full firmware tests
remain available locally through `make test` with the owner's dumps.

## Build locally

`make release-package` builds the app and simulator and writes assets to `target/dist`.
It downloads checksum-pinned appimagetool 1.9.1 and the 20251108 x86_64 runtime from
the official AppImage releases into `target/appimage-tools`. These tools are not
installed system-wide. Packaging needs `squashfs-tools` and `desktop-file-utils`.
`make emulator-conformance` exercises the simulator without needing a firmware dump.
The bundled `BUILD.txt` records the app commit and simulator base commit.
