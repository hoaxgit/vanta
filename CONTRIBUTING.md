# Contributing to Vanta

Vanta is an Xbox Series X|S to Windows x64 compatibility runtime, built on
the Victory/ZTRX foundation (VIctory by Marvisak; victory-fork by
fxceless). This document is the project's own conventions; the runtime's
design is described in `docs/architecture.md`.

## Terminology and naming

- The project is **Vanta** in prose; crates are `vanta-*`, environment
  switches `VANTA_*`, the config file `vanta.toml` (template:
  `vanta.example.toml`).
- New user-facing interfaces take Vanta's own names. Existing upstream
  API surface (import names, console semantics, config keys) is
  compatibility surface: keep it stable unless there is a reason a title
  can already see.

## Provenance and licence

- GPL-3.0-or-later (`LICENSE`). Preserve copyright notices and upstream
  attribution wherever they appear.
- The runtime's substance is inherited; never present inherited code or
  measurements as new Vanta work. When you extend an inherited component,
  describe the change, not the file.
- `crates/vanta-formats/src/bcpack-tables.bin` is the one non-own data
  exception (see `docs/asset-policy.md`); the repository's licence does not
  cover it and no similar exception is added casually.

## No game data, ever

Nothing from any title is committed — files, strings, fixtures, dumps or
derivatives. Tests take real-file paths from environment variables and
skip when unset. `cargo xtask policy` (the pre-commit hook; install with
`cargo xtask install-hooks`) enforces the allowlist; exceptions need an
entry with a reason in `xtask/src/policy.rs`. All derived data lives
outside the repository — the tools refuse to write inside it.

## Code conventions

- `rustfmt` with the repository's `rustfmt.toml`; run
  `cargo fmt --all --check` before committing.
- Comments carry the tradeoffs: where an implementation deliberately does
  less than the console, mark it `// shortcut:` and state what is missing
  and why. That marker is the project's de-scoping TODO list — do not
  leave bare `TODO`/`FIXME` comments.
- Unimplemented imports trap by default (log, debugger break, abort).
  Making one return zero instead is a per-import decision, because silent
  zeros are how compatibility layers mislead themselves.

## Cache-key discipline

Anything that can change a converter's output must be part of its cache
key — inputs read mid-conversion, environment switches, host-device
facts. Vanta's first own fix was exactly such a gap (shader translations
cached under a key that ignored four inputs; see
`vanta_shader::translation_settings` and `docs/architecture.md`). A new
switch that affects translation output goes into `SETTINGS_SWITCHES`; a
new converter input goes into the fingerprint. The regression tests in
`crates/vanta-shader/src/emit/mod.rs` and
`crates/vanta-gpu/src/shader_cache.rs` pin this.

## Testing

- Unit tests live with the code they test, exercised as a title would
  call it (through the vtables, with the console's ABI shapes).
- GPU-dependent tests take the host device and **silently skip** without
  one; a few also cover WARP. Never make them fail on a machine without
  a GPU.
- Process-ending behavior (trap policy, thread teardown) is tested by
  spawning child test processes.
- Differential shader tests compare against the console SDK's compiler
  when `VANTA_CONSOLE_DXC`/`VANTA_PC_DXC` are set, and skip otherwise.
- Assert on properties, never on snapshots of title data.

Run the crates you touched:

```powershell
cargo test -p vanta-<crate>
```

## Documentation honesty

- Never document functionality that is not in the code. Status claims
  carry their origin: inherited measurements stay attributed to the
  foundation, Vanta's own results to Vanta.
- A change that alters visible behavior updates `README.md` and
  `docs/architecture.md` in the same commit.
