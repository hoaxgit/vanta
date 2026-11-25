# Vanta architecture

Vanta runs an Xbox Series X|S title as a native Windows x64 process. The
title's code is never interpreted: the executable is mapped into the
process with the real Windows loader and its imports are bound to Vanta's
implementations. At the boundaries where the title talks to the console —
system APIs, graphics, shaders — Vanta translates: console API calls run
as Rust code, D3D12.X command recording is re-recorded on host
`ID3D12GraphicsCommandList`s, and console RDNA2 shader programs are
compiled once into host DXIL and cached on disk.

```
 user's files (outside repo)             Vanta
 ───────────────────────────             ─────────────────────────────────────────
 game assets ──────────────────────────► vanta-pipeline + vanta-game
                                          (vanta-formats, vanta-shader)
                                                     │
                                                     ▼
                                          cache_dir: converted data
                                                     │
 game executable ─► vanta-loader ────► title code (native x86-64)
                     prepare, map, bind      │ imports
                                             ▼
                                          vanta-hle ── host Windows (shared APIs)
                                             │ D3D12.X
                                             ▼
                                          vanta-gpu ─► host D3D12
                                             │ shaders at pipeline creation
                                             ▼
                                          vanta-shader ◄─► cache_dir
```

## Crates

| Crate | Role |
|-------|------|
| `vanta-core` | config, asset-policy guard, logging |
| `vanta-pe` | read-only PE32+ and import-library analysis |
| `vanta-loader` | prepare, map, bind and start the title image |
| `vanta-hle` | console API implementations, host pass-through policy |
| `vanta-gpu` | D3D12.X objects, command lists and GPU memory → host D3D12 |
| `vanta-shader` | console shader containers → host shaders |
| `vanta-formats` | the GPU's texture layouts (sizes, tiling, detiling), audio codecs |
| `vanta-pipeline` | converter trait, content-addressed cache |
| `vanta-game` | the title-specific formats, build identities, hooks |
| `apps/vanta` | command-line front end |
| `xtask` | asset-policy check, git hooks |
| `tests/fixtures/hle-probe` | a stand-in "title" (our own code) for loader tests |

Everything except `vanta-game` is platform-level and can be tested with
any Xbox Series title.

## CPU execution

`vanta-loader` runs the title's own x86-64 code, unchanged, on the host
CPU. The path from exe to running code:

1. **Prepare** (`prepare.rs`): the executable is copied and patched into a
   DLL image — no entry point, the import directory pointed at its
   terminator, bound imports stripped, TLS callbacks detached (and the
   relocation that could resurrect them at a new base neutralised).
2. **Map** (`windows.rs`): the image is mapped by the real Windows loader,
   so debuggers and crash dumps see a genuine module, and every import
   table slot is bound — including delay-loads, eagerly — through the
   resolver chain below.
3. **Start** (`threads.rs`): TLS notices reach mapped images, the title's
   entry runs, and `ExitProcess` is replicated in console order (end other
   threads, detach in reverse init order, a 5 s watchdog).

Unresolved imports get generated stubs (`thunks.rs`): the default `trap`
policy logs the `dll!function` name, breaks into a debugger and aborts
when called; `zero` (`vanta run --unimplemented zero`) logs once and
returns 0.

Static TLS is the one place title code is rewritten: an exe expects to
own TLS slot 0, but a mapped DLL gets another slot, so
`vanta-loader::tls` finds the `mov r64, gs:[0x58]` loads in the title's
code and redirects them through a thunk that zeroes `_tls_index` for the
title image. The scan is by byte pattern; the repository keeps an x86-64
decoder that a future title could require for a verified instruction
decode instead.

AMD-only instructions titles emit (`clzero`, `monitorx`/`mwaitx`, `clwb`
loops, SSE4a `extrq`/`insertq`) are emulated in an exception handler so
Intel CPUs run the same code (`vanta-loader::crash`).

## Import resolution and the system environment

For every `dll!function` the title imports, in order:

1. our implementation in `vanta-hle::modules` (always wins);
2. the host's own function, if `HostPolicy` allows that DLL (host DLLs are
   loaded from System32 only — a title cannot plant one; there is an
   integration test);
3. an export of a DLL the title ships beside its executable, mapped like
   the executable (only DLLs neither of the above serves);
4. a generated stub that names the import when called (`--unimplemented
   trap` breaks into the debugger and aborts; `zero` logs once and
   returns 0).

`LoadLibrary*` of a console-only DLL we implement (e.g.
`xgameruntime.dll`) returns a pseudo handle; `GetProcAddress` on it finds
our functions. `QueryApiImpl` hands out our runtime objects by provider
and interface id (`vanta-hle::modules::xgameruntime` lists the areas). The
console Vanta impersonates is an offline retail machine: one signed-in
local user with full privileges, the title's licence valid, store and
networking calls failing with documented HRESULTs, and every Winsock and
WinHTTP exit guarded to loopback only.

Async follows the XAsync/XTaskQueue model: ports of every dispatch mode
(manual, thread-pool, serialized, immediate) over a shared worker pool,
with block status and result sizes living in the block's system area.

## Memory

The console has unified memory: title GPU addresses are CPU addresses.
The host splits them, so `vanta-gpu::shadow` keeps persistent host copies
("shadows") of the title buffers shaders touch, plus 4 MiB windows for
memory outside any buffer. Before each submission, ranges that draws read
are refreshed page by page using a 64 KiB write-watch journal
(`watch.rs`: kernel dirty bits, or guard pages plus a vectored exception
handler for physical-view memory). Ranges shaders may write are
snapshotted before the run, compared in 64 KiB pieces after the card
finishes, and only differing bytes are merged back into title memory.
Large buffers can instead live in card memory ("cards") where a
compute-shader hashing pass finds the dirty pieces on the GPU.

`vanta-hle::modules::xmem` serves console allocations from host memory:
`VirtualAlloc2` with the console's address-range and alignment
constraints plus `MEM_WRITE_WATCH` for the dirty-page tracking above, and
64 KiB physical pages as pagefile-backed sections mapped into placeholder
reservations, remap-locked against concurrent GPU-side readers.

## Graphics

Titles have D3D12.X inlined, so `vanta-gpu` hands out COM objects with
the console's exact vtable layouts (`device.rs`, `list.rs`, `queue.rs`)
and keeps each list's inline-packet fields at values that make every
inlined fast path fall through to our table. Every list call is
translated immediately into recording on a host
`ID3D12GraphicsCommandList`; at each draw or dispatch `draw.rs`'s
`State::bind` reconciles the host list with the title's current heaps,
root signature, pipeline state and user data.

Descriptors are plain title memory, so `descriptor.rs` writes tagged
records into the slots themselves and mirrors shader-visible heaps into
host descriptor heaps; CBV tables are re-packed into a dynamic ring of
host slots (host CBVs need 256-byte alignment, console ones 16). Host
pipeline-state objects are built lazily at a pipeline's first draw —
titles create ~60,000 PSOs and draw with a few dozen.

Console-only semantics are emulated where titles depend on them:
depth-and-UAV textures get colour "twins" and a plane-drawing pass;
rect/fan/quad topologies are expanded; a list that waits on a memory
value is cut into parts the queue releases as the value lands
(`list::cut`, `queue::release_held`); query results are fabricated; the
frame clock is software.

## Shaders and their cache

Translation runs when the title creates a pipeline, from the bytes it
passes, wherever it loaded them from. `vanta_shader::ir::build` decodes
the GFX10 program into SSA, one IR node per machine instruction;
`vanta_shader::structure` then removes EXEC: lane masks become per-lane
booleans, wave-wide mask tests become per-lane branches, and vector
instructions keep a guard only where a lane can reach them with its EXEC
bit clear. `vanta_shader::resource` traces each descriptor and scalar
address to the root parameter it comes from (table, heap, root
descriptor or constant) — a descriptor a waterfall loop reads from the
first lane (`v_readfirstlane_b32`) is traced to the per-lane load or
choice that gives each lane its own, and accessed with a non-uniform heap
index in one pass. `emit/lower.rs` then lowers the IR one machine
instruction at a time into hand-rolled LLVM bitcode, wrapped into a
signed PC container the host driver accepts.

Programs written for 64-lane waves on a 32-lane host GPU go through a
retry cascade — alternative forms, widened wave-wide branches, exact and
then approximate local-wave emulation — before giving up; `vanta
translate` runs every shader it finds (containers, SGD5 files) through
these and lists what cannot be handled yet.

A lowering that cannot express an instruction yields a `Missing` value
that only fails the program when something with an effect needs it (an
export, a branch, a store, an atomic); a phi join whose incoming value is
missing is silently filled with zero unless `VANTA_STRICT_JOINS` is set
(40 programs stop by name with it — vehicle paint pixel shaders, foliage
mesh shaders). This is the main known bucket of wrong rendering, and it
is invisible in a normal run.

Translated shaders are cached on disk under `cache_dir`, keyed by BLAKE3
of the Vanta build and of every input that can change a translation:
the seven translation switches (`VANTA_WAVE_WIDE`,
`VANTA_WAVE_DEEP_SKIPS`, `VANTA_WAVE_OWN_BITS`, `VANTA_F16_NEAREST`,
`VANTA_WAVE_LOCAL`, `VANTA_STRICT_JOINS`, `VANTA_MESH_OUTPUT_STRICT`),
the host device's wave size, the view-heap limit, the loop limit, group
sync and shader optimisation settings. Before Vanta's fix, the last four
inputs were missing from the key, so changing a switch could serve a
stale translation that the switch was meant to prevent (see Provenance).
Failures are not stored: a shader that does not translate re-runs the
full retry cascade on every launch, on the pipeline workers, while the
title waits.

## Audio and input

Input is `gameinput.dll` over an 8 ms poll of XInput slots and the
keyboard, producing immutable reference-counted reading objects; device
callbacks run on the poll thread. Reading callbacks are not delivered
(the probed title polls) — a known de-scope for titles that need
push-model input.

Audio covers the XMA codec (`vanta-formats`), and the title's fmod path
is replay-tested at startup. Rate-conversion APU kinds return
`E_NOTIMPL`, and DSP convolution outputs zeros — no reverb — both known
de-scopes. HDR output is always reported off.

## Filesystem and storage

Title storage is a drive on the console, and titles build paths on it.
`vanta-hle::modules::files` binds every path-taking title import
(kernel32 and the C/C++ runtime) and rewrites paths on a virtual drive
to the host folder behind it: `G:` is the title folder, `T:` a temp
folder, and persistent local storage is another such drive over a folder
under `VANTA_DATA_DIR` (game saves are files there). Narrow (ANSI)
paths are decoded and forwarded to the wide implementations, so host
folders outside the ANSI code page work. Other paths reach the host
unchanged. `ReadFile`/`fread` notify the GPU layer so kernel writes into
watched buffers are observed.

DirectStorage is a single scheduler over all title queues: weighted
round-robin priorities, a staging budget, worker threads doing positional
reads plus zlib inflate (miniz_oxide) and BCPack decode into title
addresses or physical-page windows, in-order completion.

## Diagnostics

- `vanta census` classifies a title's imports without running anything
  (it does not map title DLLs; `vanta run` without `--execute` does).
- `vanta translate` reports shader-level coverage, program by program.
- `vanta run --execute` installs a crash reporter that names faults as
  `module+rva`, unwinds the faulting thread's stack, classifies the
  memory at the faulting address, decodes MSVC C++ throws, and appends
  Rust panics to the same log (`vanta-loader::crash`).
- The diagnostics build adds frame capture (`VANTA_CAPTURE`,
  `VANTA_CAPTURE_FRAMES`), scripted input (`VANTA_KEYS`), and D3D12 DRED
  reporting on GPU failure (`VANTA_D3D12_DRED`).
- In the code, a `// shortcut:` comment marks a deliberate, documented
  de-scope — the tradeoff is stated where the code would otherwise look
  wrong. These are the project's TODO markers.

Testing follows three tiers: ABI-shape tests that exercise the emulated
objects exactly as a title does; algorithm tests of the shadow, merge and
flight machinery against plain-`Vec` models; and GPU-dependent tests that
create the real host device and silently skip without one (WARP included
for a few paths). Shader translation has differential tests against the
console SDK's compiler when its paths are provided, and validator tests
against a real `dxil.dll`. Tests that need title files take their paths
from environment variables and skip when unset.

## Provenance: inherited, modified, Vanta-specific

The runtime's substance is inherited; Vanta does not claim otherwise.

| Layer | Status |
|-------|--------|
| Loader, HLE, GPU, shader translator, formats, pipeline cache, app, `xtask` (all crates above) | inherited from the Victory/ZTRX foundation (VIctory by Marvisak; victory-fork by fxceless) |
| Crate naming (`vanta-*`), `VANTA_*` environment switches, `vanta.toml` configuration, README and this documentation | modified in the migration to Vanta |
| Shader-cache key completeness (every translation-affecting input hashed; `translation_settings()` in `vanta-shader`, `fingerprint()` in `vanta-gpu`, with regression tests), project identity, conventions, history restart | Vanta-specific, 2026-11 |

## Selected known de-scoping

From the `shortcut:` markers, the ones most likely to matter to a new
title first: `XMemGetWorkingSetStatistics` reports whole-process commit as
the title's memory use; several wide-character path functions
(`SetCurrentDirectoryW`, `CopyFileW`, …) pass virtual-drive paths
un-rewritten; GPU writes through dynamically-indexed UAVs are not copied
back to title memory; barriers are tracked per resource, not per
subresource; a blocking fence wait (`SetEventOnCompletion` with a null
event) returns `E_NOTIMPL`; phi joins with missing inputs draw as zero
(the 40 programs above). The `census` and `translate` tools measure both
sides of this list against any title you point them at.
