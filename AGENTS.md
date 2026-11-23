# AGENTS.md — easi-install

Hard rules for every agent (human or AI) touching this repo. Read fully before writing code.
If a rule here conflicts with a convenience, the rule wins. If it conflicts with an explicit
user request, the request wins — then update this file.

## What we build

`easi` — make installing *downloaded* Linux apps (AppImage, `.tar.*`, `.zip`, and friends) as
easy as macOS drag-to-Applications.

- **GUI** (Rust + Slint): opens when the user double-clicks a supported file. Default flow is a
  macOS-style drag window (app icon → Applications folder). Worst case, for esoteric formats, a
  Windows-style step wizard. Never worse than that.
- **CLI**: `easi install <path>` (plus uninstall/list). Y/N confirmations allowed; free-text
  prompts only when unavoidable. Must be fully scriptable with `--yes`.
- **Wiring**: binary placement, `.desktop` entry, icons (hicolor theme), PATH symlink, MIME
  registration, desktop DB / icon cache refresh, clean uninstall.
- Install locations (per-user only in v1): `~/Applications/<App>/`,
  `~/.local/share/applications`, `~/.local/share/icons/hicolor`, `~/.local/bin`,
  manifests in `~/.local/share/easi/`. These are the **only** allowed write roots.
- Locked product decisions live in `TODO.md` → "Decisions". Don't relitigate them in code.

### Growth model
Long-term goal: support as many download-style Linux app formats as possible. v1 scope is
narrow on purpose. Adding a format or metadata source must be **adding rows** (detect table,
id-source table, install-plan table) plus tests — never a new code path that bypasses them.
If a new format can't be expressed as rows, fix the table shape first.

### Out of scope (hard)
- Distro package managers and their formats: apt/dnf/pacman/zypper, `.deb`, `.rpm`, Flatpak,
  Snap, Nix. We do not wrap, call, or replace them.
- Building from source (`./configure && make`). We may *detect* it and say so — never run it.
- Running arbitrary installer scripts from archives without explicit user consent.

## Architecture rules (non-negotiable)

### 1. Strict Redux style
- **One `State` value per process** (GUI and CLI each own one store). No globals, no
  `static mut`, no hidden caches holding app state.
- **All change goes through `fn reduce(state: State, action: Action) -> (State, Vec<Effect>)`.**
  Reducers are **pure**: no I/O, no clock, no randomness, no env reads.
- **`Action`** = a Rust `enum` of everything that can happen (user intent + I/O results).
- **`Effect`** = a Rust `enum` describing I/O to perform. A thin effect runner executes effects
  and dispatches the result back as an `Action`. The runner contains no decisions.
- **Views are functions of state.** Slint UI binds to a projection of `State`; UI callbacks only
  dispatch actions. No logic in `.slint` files beyond layout and binding.
- CLI is just another view: it renders state to the terminal and maps keypresses to actions.

### 2. Explicit finite state
- Every lifecycle is **one enum** with data attached to the variant that owns it
  (`Install::Extracting { progress }`, `Install::Failed { error }`).
- **Never two booleans for one mode.** `is_loading`/`is_done`/`has_error` → enum.
- Transitions live in **one place** (the reducer `match (state, action)`), and the match is
  **exhaustive** — no `_ =>` catch-all on state/action pairs. Every ignored pair is written
  out, so "ignore" is a decision, not an accident.
- Slow exits get their own state (`Cancelling`, `RollingBack`). Never report a state before
  it is true.
- Re-entering the current state is a no-op.

### 3. Declarative / caveman-code
- Load and follow the `caveman-code` skill for every code task.
- Behavior as data: format detection (magic bytes → `Format`), per-format install plans,
  desktop-environment quirks, install-location policy — all **tables**, read by small engines.
- Lookup beats branching. A `match` on one key that only maps to values becomes a `const` table.
- Simplest thing that works. No speculative traits, generics, plugin systems, or config knobs.
- No duplication. No single-use helpers. Comments explain *why*.

## Safety rules

- **Never** write outside the chosen install root(s) and XDG dirs. Every path we write is
  computed, canonicalized, and checked to be under an allowed root (blocks `../` and symlink
  escapes from archives).
- **Every install is a transaction** with a recorded manifest of created files. Failure ⇒
  rollback to the exact prior state. Uninstall = replay manifest in reverse.
- Never overwrite a file we did not create without explicit user confirmation.
- Synthesized app ids and their `.desktop`/icon filenames are always `easi.`-prefixed, so they
  can never shadow a distro-installed app of the same name.
- Never run code from the package (no `--appimage-extract` of untrusted files outside a temp
  dir; no executing bundled scripts) unless the user explicitly opts in for that install.
- Root/system-wide installs only via explicit choice + polkit/pkexec. Default is per-user.
- Downloaded files are untrusted: extract to a fresh temp dir, never `cd` into it to run tools.

## Code & repo conventions

- Rust stable, edition 2024. `cargo fmt` + `cargo clippy -- -D warnings` must pass.
- No `unwrap()`/`expect()` outside tests and `main` startup. Errors are typed enums
  (`thiserror`), rendered to the user via a message table.
- Dependencies: justify each new crate in the PR/commit message. Prefer std.
- Tests: table-driven, behavioral, durable. Reducers get one row per `(state, action) → next`
  including rejected pairs. Filesystem effects tested against a temp root, never `$HOME`.
- Fixtures: small synthetic packages generated by test helpers; no large binaries committed.
- Commits: small, imperative subject, one concern each. `TODO.md` is updated in the same
  commit that completes a roadmap item.

## Where things go

- `TODO.md` — roadmap and open questions. Source of truth for what's next.
- `AGENTS.md` — this file. Rules only; no roadmap.
