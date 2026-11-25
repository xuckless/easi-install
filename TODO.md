# TODO — easi-install roadmap

Rules live in `AGENTS.md`. Completed milestones (M0–M5, v1.x, v2.0–v2.3) live in `git log` and
GitHub Releases; this file tracks only locked decisions and remaining work.

## Decisions (locked)

| Topic | Decision |
|---|---|
| Install root | Per-user `~/Applications/<App>/`, `~/.local/share/applications`, `~/.local/share/icons/hicolor`, `~/.local/bin`, `~/.local/share/easi/`. No root required. |
| AppImage | Kept as single file in install root; `.desktop` + icon read via pure-Rust SquashFS (never executed during inspection). |
| Lifecycle | Transactional manifest of every written file + atomic replace rollback + self-updated app reconciliation. |
| Missing metadata | Score candidates → clear winner: DragStage + confirm. Ambiguous: step wizard (executable / icon / name). |
| Terminal apps | Symlink into `~/.local/bin` only (no `.desktop`); offer PATH update interactively or via `--add-to-path`. |
| File association | `easi` is default double-click handler for AppImages only; `.tar.*`/`.zip`/`.7z`/`.jar`/`.sh` are registered under Right-Click → Open With by default (opt-in `--claim-archives`). Previous defaults remembered and restorable; non-app archives hand off silently and close `easi`. |
| Trust checks | Origin (`user.xdg.origin.url`) + size + SHA-256 + AppImage/detached sig verification + static script analyzer + path-escape/setuid/noexec/arch/runtime-dep guards. |
| Sandboxing | Package internals are parsed only in the re-exec'd `easi __inspect` helper (Landlock: read the package file only, no TCP/UDP; `RLIMIT_AS` 2 GiB, `RLIMIT_CPU` 60 s). Consented `.run`/`.sh` installers run via `easi __sandbox-exec`: writes only to the app dir, staging dir, easi's bin/desktop/icon roots, `~/.config`, `~/.local/share`, `/tmp` (+ `/dev/null`, `/dev/tty`, `/dev/pts`); network allowed; scrubbed env. New or changed entries in `~/.config/{autostart,systemd/user,environment.d}` or `~/.local/share/easi` fail the install (new ones removed); a failed/cancelled script is rolled back including its new top-level data entries. Landlock is best-effort; the status reflects the write-denial layer (fs rights ≤ ABI V3) and is always shown, never overstated. |
| CLI | Full subcommand parity (`install`, `inspect`, `list`, `scan`, `adopt`, `repair`, `organize`, `forget`, `set-source`, `check-updates`, `update`, `export`, `import`, `uninstall`, `learn-wmclass`, `config`, `self-update`, `self-uninstall`). Safe `-y/--yes` (opt-in `--trash` and `--add-to-path`). |
| GUI | Slint (Skia + FreeType renderer), Functional Calm design system + Drop brand, strict Redux projection of `Store` (`State` + `ManagerUi`). |
| Distribution | Self-installing binary + `curl \| sh` installer + `easi self-update` (GitHub Releases, SHA-256 + minisign verified). |
| License | Apache-2.0. |
| Layout | Cargo workspace: `easi-core`, `easi-cli`, `easi-gui`, top-level `easi` binary. |

## Core state sketch

```
Session:  Idle → Inspecting → Classified ─┬─ NotAnApp → HandingOff → Done (auto-close)
                                          ├─ Ready(plan) ── (DragStage / Install / Y) ─┐
                                          ├─ NeedsChoice(wizard step) ── answer ───────┤
                                          └─ Conflict(existing) ── replace? ───────────┤
                                                                                       ▼
          Installing(step, progress) ─┬─ Installed → OfferTrash → Done
                                      ├─ Cancelling → RollingBack → Cancelled
                                      └─ Failed → RollingBack ─┬─ ok=true  → FailedRolledBack
                                                               └─ ok=false → Failed
```
Every arrow is a row in the exhaustive `reduce` match (no `_ =>`). UI state lives in `ManagerUi`
inside `Store` and transitions via exhaustive `reduce_ui`.

## Remaining Backlog

- [ ] OS drag-and-drop onto the App Manager window (drop packages straight from Nautilus/Dolphin/Nemo)
- [ ] Optional system-wide installs (`/opt` via polkit/`pkexec`)

## Open questions
- (none)
