# AGENTS.md — easi-install

Hard rules for every agent (human or AI) touching this repo. Read fully before writing code.
If a rule here conflicts with a convenience, the rule wins. If it conflicts with an explicit
user request, the request wins — then update this file.

## What we build

`easi` — make installing and managing *downloaded* Linux apps (AppImage, `.tar.*`, `.zip`, `.7z`,
`.jar`, bare ELF, `.run`/`.sh`/`.bin`) as easy as macOS drag-to-Applications.

- **GUI** (Rust + Slint, Skia + FreeType renderer, Functional Calm design system + Drop brand):
  - Opens into the standalone **App Manager** (Apps, Detected Apps, Android-style App Info,
    Settings) when launched directly, or into the macOS-style **DragStage** install window when
    double-clicking a supported package.
  - Enforces single-instance handoff over a per-user Unix socket (`$XDG_RUNTIME_DIR/easi.sock`)
    and auto-closes on silent non-app hand-off.
- **CLI**: `easi install|inspect|list|scan|adopt|repair|organize|forget|set-source|check-updates|update|export|import|uninstall|learn-wmclass|config|self-update|self-uninstall`
  plus `--setup` / `--restore-handlers`. Y/N confirmations allowed; `-y/--yes` is non-interactive
  and safe (keeps downloads and leaves shell rc files untouched unless `--trash` / `--add-to-path`
  are explicitly passed).
- **Wiring & Discovery**: binary placement, `.desktop` entry (absolute hicolor `Icon=` +
  `StartupWMClass=`), hicolor icons, `~/.local/bin` symlink, MIME registration with previous-handler
  backup/restore, desktop DB / icon cache refresh, clean uninstall, self-updated app reconciliation,
  and discovery/adoption/repair of existing unmanaged `$HOME` apps.
- Install locations (per-user only): `~/Applications/<App>/`, `~/.local/share/applications`,
  `~/.local/share/icons/hicolor`, `~/.local/bin`, manifests/config in `~/.local/share/easi/`,
  and optional daily `systemd --user` timer in `~/.config/systemd/user/`.
- Locked product decisions live in `TODO.md` → "Decisions". Don't relitigate them in code.

### Growth model
Adding a format, metadata source, DE quirk, or known upstream app source must be **adding rows**
to the declarative tables (`detect`, `appid`, `plan`, `inspect`, `upstream::KNOWN_APP_SOURCES`)
plus tests — never a new code path that bypasses them. If a new format can't be expressed as rows,
fix the table shape first.

### Out of scope (hard)
- Distro package managers and their formats: apt/dnf/pacman/zypper, `.deb`, `.rpm`, Flatpak,
  Snap, Nix. We do not wrap, call, or replace them.
- Building from source (`./configure && make`). We may *detect* it and say so — never run it.
- Running arbitrary installer scripts from archives without explicit user consent.
- Intercepting live HTTPS traffic from other apps.

## Architecture rules (non-negotiable)

### 1. Strict Redux style
- **One `Store` value per process** (GUI and CLI each own one store holding `State` + `ManagerUi`).
  No globals, no `static mut`, no hidden caches holding app state.
- **All change goes through pure reducers**: `fn reduce(state: State, action: Action) -> (State, Vec<Effect>)`
  and `fn reduce_ui(ui: ManagerUi, action: UiAction) -> ManagerUi`. Reducers are **pure**: no I/O,
  no clock, no randomness, no env reads.
- **`Action`** = a Rust `enum` of everything that can happen (user intent + I/O results). Use
  `Result<T, E>` rather than `(ok: bool, error: Option<String>)`.
- **`Effect`** = a Rust `enum` describing I/O to perform. A thin effect runner executes effects
  and dispatches the result back as an `Action`. The runner contains no decisions.
- **Views are functions of state.** Slint UI binds to read-only `in property` projections of
  `State` + `ManagerUi`; UI callbacks only dispatch actions. No business or navigation state
  mutations in `.slint` files beyond layout, animation, and raw text-input buffers.
- CLI is just another view: it renders state to the terminal and maps keypresses to actions.

### 2. Explicit finite state
- Every lifecycle is **one enum** with data attached to the variant that owns it
  (`Install::Extracting { progress }`, `State::Failed { error }`, `State::FailedRolledBack { error }`).
- **Never two booleans for one mode.** `is_loading`/`is_done`/`has_error` or `rolled_back: bool` → enum variants.
- Transitions live in **one place** (`reduce` / `reduce_ui`), and the match is **exhaustive** —
  no `_ =>` catch-all on state/action pairs. Every ignored pair is written out explicitly.
- Slow exits get their own state (`Cancelling`, `RollingBack`). Never report a state before it is true.
- Lock async buttons while an operation is in flight so spam-clicking cannot spawn duplicate work.

### 3. Declarative / caveman-code
- Behavior as data: format detection (magic bytes → `Format`), per-format install plans,
  DE quirks, icon/exec scoring rules, known upstream app sources — all **tables**, read by small engines.
- Lookup beats branching. A `match` on one key that only maps to values becomes a `const` table.
- Simplest thing that works. No speculative traits, generics, plugin systems, or databases for small tables.
- No duplication. No single-use helpers. Comments explain *why*.

## Safety rules

- **Never** write outside the allowed per-user write roots. Every path we write is computed,
  canonicalized, and checked via `safe_join` (blocks `../` and symlink escapes from archives).
- **Every install is a transaction** with a recorded manifest of created files and atomic replace
  backup/restore on failure or cancel. Uninstall = replay manifest in reverse.
- Never overwrite a file we did not create without explicit user confirmation.
- Synthesized app ids and their `.desktop`/icon filenames are always `easi.`-prefixed.
- Never run code from a package during inspection (pure-Rust SquashFS/archive readers only;
  no executing bundled scripts without explicit static-analysis consent).
- Downloaded files are untrusted: extract to a fresh temp dir, never `cd` into it to run tools.
- Untrusted package parsing goes only through `sandbox::inspect_package` (the `__inspect` helper)
  and consented installer scripts only through `sandbox::Confined` (`__sandbox-exec`). Never report
  a `SandboxStatus` stronger than the one the helper returned.

## Code & repo conventions

- Rust stable, edition 2024. `cargo fmt` + `cargo clippy --workspace --all-targets -- -D warnings`
  + `cargo test` must pass.
- No `unwrap()`/`expect()` outside tests and `main` startup (enforced via workspace clippy lints).
  Errors are typed enums (`thiserror`), rendered via a message table.
- Dependencies: justify each new crate in `Cargo.toml` and PR/commit message. Prefer `std`.
  Keep compression crates statically linked (`xz2`/`bzip2` `static`) so release binaries stay
  portable across Linux distros.
- Tests: table-driven, behavioral, durable. Reducers get one row per `(state, action) → next`
  including rejected pairs. Filesystem effects tested against a `TempDir`, never `$HOME`.
- Commits: small, imperative subject, one concern each. `TODO.md` stays pruned to remaining work.

## Where things go

- `TODO.md` — locked decisions and remaining roadmap only (completed items live in `git log` and GitHub Releases).
- `AGENTS.md` — this file. Rules and architecture invariants only; no roadmap.
