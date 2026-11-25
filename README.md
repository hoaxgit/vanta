# Vanta

Vanta is a compatibility runtime that runs Xbox Series X|S titles natively
on Windows x64. It is written in Rust. The title's own x86-64 code executes
directly on your CPU; around it, Vanta implements the console's system
APIs, translates its D3D12.X graphics calls to host Direct3D 12, and
rewrites its RDNA2 shader programs into host DXIL — in one process, with no
emulator and no interpreter on the hot path.

**This repository contains no game files or assets.** You supply your own
copy of the title. Vanta is not affiliated with, or endorsed by, any game
developer or publisher.

## Current state

The runtime is inherited from the Victory/ZTRX foundation (see
[Provenance and license](#provenance-and-license)). The measurements below
are that foundation's, made on Windows 11 with an NVIDIA GeForce RTX 3050
(8 GB); they describe the code in this repository unchanged. Vanta has not
re-measured them.

| Area | Result |
|---|---|
| Boot, first-boot screens, intro, credits | works |
| Prologue | playable from the cell to the boat chase: cell, cafeteria, robbery, police chase (wanted level, minimap), boat, binoculars |
| Stability | 1 h 30 min+ in one session without a crash |
| Frame rate | 1-3 fps in the world; the first run is slower while shaders translate, and cached after that |
| Known problems | shadowed areas render black and bright scenes overexpose (the title's ray-traced lighting does not run yet — see `docs/ray-tracing.md`); some blocky artifacts; boats sit low in the water |

## Requirements

- Windows 10/11 x64 and a CPU with AVX2 (AMD-only instructions that titles
  emit are emulated on Intel CPUs)
- A Direct3D 12 GPU with shader model 6.6 and mesh shaders; tested hardware
  is NVIDIA RTX 20 series or newer
- Rust, per `rust-toolchain.toml`
- Several GB of free Windows commit memory — raise the paging file if
  `crashes.log` reports error 1455

## Build

```powershell
cargo build --release -p vanta
```

## Launch

```powershell
powershell -ExecutionPolicy Bypass -File scripts\play.ps1 -Game "<your game folder>\<the game's exe>"
```

`scripts\play.ps1` keeps the shader cache and saved data in the folders you
choose (`-Cache`, `-Data`) and writes the run log to `%TEMP%\vanta.log`. It
presses no keys for you. Controller: XInput pads (a DualShock or DualSense
through DS4Windows works). With no pad connected, the keyboard acts as one
while the game window has focus.

On first play every pipeline's shaders are translated at first use and
cached on disk; later runs load them instead of re-translating. The cache
is keyed by the Vanta build and every translation-affecting setting, so a
rebuild or a changed switch re-translates rather than serving stale
results.

## Configuration

Copy `vanta.example.toml` to `vanta.toml` (git-ignored) and point it at
your files. Environment variables take precedence over the file:

| Variable | Meaning |
|---|---|
| `VANTA_GAME_DIR` | your own dump of the title |
| `VANTA_CACHE_DIR` | derived data (default `%LOCALAPPDATA%\vanta\cache`) |
| `VANTA_DATA_DIR` | what titles save — persistent local storage (default `%LOCALAPPDATA%\vanta\data`) |
| `VANTA_TEST_TITLES` | other Xbox Series titles used to test platform-level code (`;`-separated) |
| `VANTA_CONFIG` | selects a different config file |

Every path must be outside the repository; the tools refuse otherwise
(`docs/asset-policy.md`).

## Faster settings (optional, off by default)

`VANTA_RELAY=2`, or `VANTA_LATE_WAIT=3 VANTA_WIDE_BACK=1`, cut frame time
on the tested machine. Set them in PowerShell before the launch script.

## Diagnostics and tools

The `vanta` command front end:

| Command | Purpose |
|---|---|
| `vanta census [paths]` | which imports of a title our runtime covers; `--missing` lists the rest, most used first |
| `vanta translate [paths]` | run every shader container found through the translator; lists what cannot be handled yet |
| `vanta inspect <file>` | one PE image: headers, sections, imports, TLS, build id |
| `vanta shader-info [paths]` | find shader containers (also inside engine archives, if uncompressed) and report their layout |
| `vanta lib-exports <lib>` | the `dll!export` pairs an import library describes |
| `vanta run <exe> --execute` | prepare, map, bind and start a title (as `play.ps1` does) |

`vanta run` without `--execute` maps and binds without starting.
`--unimplemented trap` (the default) makes an unimplemented import break
into the debugger and abort when called; `zero` logs once and returns 0.
`--console series-x` or `series-s` selects the model the title sees.

For deeper diagnosis, build the diagnostics profile and use its switches:

```powershell
cargo build --profile diag -p vanta --features diagnostics
```

| Switch | Effect |
|---|---|
| `VANTA_CAPTURE=<dir>` + `VANTA_CAPTURE_FRAMES=<list>` | save those frames as PNG |
| `VANTA_KEYS=30:Enter,35:X` | press keys at those seconds |
| `VANTA_D3D12_DRED=1` | on a GPU failure, report the commands in flight |
| `VANTA_PIPELINE_WAIT_MS=24` | shorter per-frame wait for pipelines still being built |

## Documentation

- `docs/architecture.md` — how the runtime works, and what is inherited
  versus Vanta-specific
- `docs/ray-tracing.md` — the title's ray-traced lighting, translated and
  emulated shader-side, and what still blocks it
- `docs/asset-policy.md` — the no-game-data rule and its enforcement
- `CONTRIBUTING.md` — conventions for changes to this project

## Provenance and license

GPL-3.0-or-later; see `LICENSE`. Vanta is built on the Victory/ZTRX
foundation — VIctory by Marvisak, victory-fork by fxceless & ztrx by Zettrox — and the
runtime's substance is theirs; the crate layout, environment switches and
documentation carry Vanta's own naming. Vanta's own engineering work so far
is listed in `docs/architecture.md`, and starts with making the shader
cache honest about every input that affects a translation. Upstream
copyright notices and attribution are preserved throughout.
