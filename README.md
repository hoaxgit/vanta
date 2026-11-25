<div align="center">

# Vanta

**Xbox Series X|S titles, running natively on your Windows hardware**

[![Build](https://github.com/hoaxgit/vanta/actions/workflows/ci.yml/badge.svg)](https://github.com/hoaxgit/vanta/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-GPL--3.0--or--later-blue)](LICENSE)

</div>

Vanta is a Windows runtime for Xbox Series X|S software. Written in Rust, it
takes a title's own x86-64 executable, maps it, and lets your CPU execute it
directly — no emulation, no interpretation on the hot path. What Vanta
replaces is the console around the code: it implements the system APIs the
title calls, turns its D3D12.X graphics commands into host Direct3D 12, and
recompiles its RDNA2 shader machine code into DXIL your GPU accepts. All of
it in one process, on your hardware, at your clock speed.

**No game files live in this repository.** Vanta ships none, downloads none,
and asks for none: you point it at your own dump of a title. The project is
not affiliated with, or endorsed by, any game developer or publisher.

## Status

Vanta's code grew out of the Victory/ZTRX foundation (see
[Credits and license](#credits-and-license)). The results below are
Vanta's own, read from its logs on Windows 11 with a Ryzen 7 5700X and a
GeForce RTX 5060:

| Milestone | Result |
|---|---|
| Executable loading | verified — a plain PE32+ x86-64 dump maps and binds all 587 imports with none missing |
| Boot and menus | verified — the title boots and reaches its first-run account screen |
| Intro cutscene | partial — playback starts but the picture stays black (below); it can be skipped |
| Gameplay | verified — with the cutscene skipped, in-world play runs (the prison scene), with imperfect graphics |
| Controller | verified over XInput (wired or 2.4 GHz pad); a Bluetooth pad is invisible to XInput |
| Frame rate | 30 fps in the menu; roughly 1 fps in the world; the first run is slower while shaders translate, then they are served from cache |
| Stability | long sessions without a crash, including in-game play past the cutscene |

The same logs name the gaps: the title requests about 1.24 million
shader-visible views against a host heap capped near one million (beyond
it, views read null slots); a set of console pipeline-creation vtable slots
is still unimplemented; host pipelines drop the depth-bounds test; the
title's ray-traced lighting does not run yet (`docs/ray-tracing.md`); and
cutscenes stay black — the video data arrives (the title's Bink2Scarlett
decoder binds with nothing missing) but the decoded frames never reach the
screen. Each is tracked in `docs/architecture.md`.

## System requirements

- Windows 10 or 11, x64, and a CPU with AVX2 (instructions only AMD CPUs
  execute are emulated on Intel)
- A Direct3D 12 GPU with shader model 6.6 and mesh shaders — NVIDIA RTX 20
  series and newer is what has been tested
- Rust, as pinned in `rust-toolchain.toml`
- A few gigabytes of spare Windows commit memory; if `crashes.log` reports
  error 1455, enlarge the paging file

## Building

One command builds the release binary to `target\release\vanta.exe`:

```powershell
cargo build --release -p vanta
```

## Running a title

```powershell
powershell -ExecutionPolicy Bypass -File scripts\play.ps1 -Game "<your game folder>\<the game's exe>"
```

The script keeps the shader cache and saved data wherever you point it
(`-Cache`, `-Data`) and writes the run log to `%TEMP%\vanta.log`. It never
presses keys for you. Pads reach the game through XInput — a DualShock or
DualSense behind DS4Windows works too — and while no pad is connected and
the game window has focus, the keyboard stands in for one.

The first run of a title translates every pipeline's shaders the moment
they are first used and stores them on disk; later runs load them instead.
The cache key covers the Vanta build and every setting that can change a
translation, so a rebuilt binary or a flipped switch re-translates rather
than serving a stale result.

## Paths and settings

Copy `vanta.example.toml` to `vanta.toml` (git-ignored) and point it at
your files. Environment variables win over the file:

| Variable | Meaning |
|---|---|
| `VANTA_GAME_DIR` | where your own dump of the title lives |
| `VANTA_CACHE_DIR` | derived data such as translated shaders (default `%LOCALAPPDATA%\vanta\cache`) |
| `VANTA_DATA_DIR` | where titles keep saves — persistent local storage (default `%LOCALAPPDATA%\vanta\data`) |
| `VANTA_TEST_TITLES` | further Xbox Series titles for exercising platform-level code (`;`-separated) |
| `VANTA_CONFIG` | use a different config file |

Every path must sit outside this repository; the tooling refuses anything
else (`docs/asset-policy.md`).

## Tuning performance (optional, off by default)

`VANTA_RELAY=2`, alone or with `VANTA_LATE_WAIT=3 VANTA_WIDE_BACK=1`, cut
frame time on the tested machine. Set them in PowerShell before launching.

The world's ~1 fps is a CPU limit, not a GPU one — the GPU sits at ~2-32%
utilization while the CPU snapshots and compares title buffers. On the RTX
5060, moving the 1-16 MB buffers onto the GPU cut the per-frame
"preparing" time about threefold:

```powershell
$env:VANTA_CARD_BUFFERS_MIN_MB = "1"   # default 16: only >=16 MB buffers use the card
$env:VANTA_CARD_BUFFERS_MB    = "2048" # default 512: raise if the card budget fills
$env:VANTA_JOIN_COPIES        = "1"
$env:VANTA_CARD_REHASH_RUNS   = "1"
```

These come from one machine's measurements; they relocate buffer
bookkeeping, they do not remove the CPU bottleneck.

## Tools and diagnostics

Everything is driven through one command:

| Command | Purpose |
|---|---|
| `vanta census [paths]` | how much of a title's import surface this runtime covers; `--missing` lists the rest, most-referenced first |
| `vanta translate [paths]` | push every shader container found through the translator; names what it cannot handle yet |
| `vanta inspect <file>` | one PE image — headers, sections, imports, TLS, build identity |
| `vanta shader-info [paths]` | locate shader containers (including inside engine archives, when uncompressed) and describe their layout |
| `vanta lib-exports <lib>` | the `dll!export` pairs an import library describes |
| `vanta run <exe> --execute` | prepare, map, bind and start a title — what `play.ps1` runs |

`vanta run` without `--execute` maps and binds, then stops.
`--unimplemented trap` (the default) makes a call to an unimplemented
import break into the debugger and abort; `zero` logs once and returns 0.
`--console series-x` or `series-s` picks the console model the title sees.

Diagnostics reach stderr structured: an uptime timestamp, a severity and
the source line on every entry. `VANTA_LOG` (or `RUST_LOG`) chooses the
level — `$env:VANTA_LOG = "vanta_gpu=trace"` — and `info` is the default.

For deeper work, build the diagnostics profile and use its switches:

```powershell
cargo build --profile diag -p vanta --features diagnostics
```

| Switch | Effect |
|---|---|
| `VANTA_CAPTURE=<dir>` + `VANTA_CAPTURE_FRAMES=<list>` | dump those frames as PNG |
| `VANTA_KEYS=30:Enter,35:X` | synthesize key presses at chosen seconds |
| `VANTA_D3D12_DRED=1` | after a GPU failure, report the commands in flight |
| `VANTA_PIPELINE_WAIT_MS=24` | shorten the per-frame wait for pipelines still building |

## Further reading

- `docs/architecture.md` — how the runtime works, and which parts are
  inherited versus Vanta's own
- `docs/ray-tracing.md` — the title's ray-traced lighting: translated,
  emulated shader-side, and what still blocks it
- `docs/asset-policy.md` — the no-game-data rule and how it is enforced
- `CONTRIBUTING.md` — how changes to this project are made

## Credits and license

Vanta is GPL-3.0-or-later; the full text is in `LICENSE`. The runtime's
substance descends from the Victory/ZTRX foundation — VIctory by Marvisak,
victory-fork by fxceless, ztrx by Zettrox — and this project would not
exist without them. What Vanta adds on top — the naming, the crate layout,
the environment switches, the documentation, and its own engineering work,
recorded in `docs/architecture.md` and starting with a shader cache made
honest about every input that affects a translation — is deliberately
marked as its own. Upstream copyright notices and attribution are
preserved throughout.
