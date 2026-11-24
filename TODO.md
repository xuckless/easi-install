# TODO — easi-install roadmap

Rules live in `AGENTS.md`. This file is the plan. Tick items in the commit that finishes them.

## Decisions (locked from design Q&A, 2026-11-23)

| Topic | Decision |
|---|---|
| Install root | Per-user `~/Applications/<App>/`. `.desktop` → `~/.local/share/applications`, icons → `~/.local/share/icons/hicolor`, CLI links → `~/.local/bin`. No root in v1. |
| v1 formats | AppImage (type 1 + 2), `tar.{gz,xz,zst,bz2}`, `.zip`. Esoteric formats (`.run`, 7z, bare binaries…) added post-v1 as encountered, via wizard. |
| AppImage | Kept as single file, moved into install root. Only icon + `.desktop` extracted for wiring. |
| Lifecycle | Manifest of every written file. `uninstall`, `list`. No upstream update checks. |
| Re-drop installed app | "X 1.2 installed — replace with 1.3?" → transactional swap, manifest replaced. |
| Missing metadata | Score candidates (main ELF, icon, name) → clear winner: drag flow + confirm. Ambiguous: wizard pages (pick program / pick icon / name). |
| Terminal apps | No GUI detected → symlink into `~/.local/bin` only, no `.desktop`. Offer to add `~/.local/bin` to PATH if missing. |
| File association | easi is **default handler** for all supported MIME types. Previous default remembered at setup. |
| Non-app archive | Silently hand off to the remembered previous handler (file-roller, Ark…). |
| Source file after install | Success screen offers "Move download to Trash" (default on). CLI: Y/N. |
| Trust checks (v1) | Origin (`user.xdg.origin.url`) + size + sha256 summary; verify AppImage embedded sig and detached `.sig/.asc` when present; refuse path escapes, strip setuid, warn on odd perms. Nothing beyond. |
| CLI | Subcommands: `easi install <path>`, `easi uninstall <app>`, `easi list`, `easi self-update`. `-y/--yes` for non-interactive. Y/N prompts; free text only if unavoidable. |
| GUI | Slint, **minimal default style** first — user designs visuals after seeing layout. Drag icon → Applications folder *inside easi's window* (DMG-style) + Install button for keyboard. Worst case: step wizard. |
| DEs (v1) | GNOME first (Wayland + X11), then KDE Plasma. Others later. |
| Distribution of easi | Self-installing binary + `curl \| sh` installer + `easi self-update` (brew-like). Source: GitHub Releases, sha256 + minisign verified. |
| License | Apache-2.0 (`LICENSE`, `license = "Apache-2.0"` in workspace). |
| App id | First hit in a source table: AppStream `metainfo/*.xml` `<id>` → embedded `.desktop` basename → slug of archive top dir / AppImage filename / main ELF name, with version/arch/ext tokens stripped by a token table. Synthesized ids get namespace `easi.<slug>`; written files are `easi.<slug>.desktop`. Stored in the manifest with the original filename so the replace flow matches later versions. |
| Layout | Cargo workspace: `easi-core` (state, reducer, detect, plans, effect runner), `easi-cli`, `easi-gui`. Single `easi` binary chooses GUI vs CLI by argv/TTY. |

## Core state sketch (to be refined in M0)

```
Session:  Idle → Inspecting → Classified ─┬─ NotAnApp → HandingOff → Done
                                          ├─ Ready(plan) ── (drag / Install / Y) ─┐
                                          ├─ NeedsChoice(wizard step) ── answer ──┤
                                          └─ Conflict(existing) ── replace? ──────┤
                                                                                  ▼
          Installing(step, progress) ─┬─ Installed → OfferTrash → Done
                                      ├─ Cancelling → RollingBack → Cancelled
                                      └─ Failed → RollingBack → Failed(rolled_back)
```
Every arrow is a row in the reducer match. Every unlisted `(state, action)` pair is written out explicitly.

## M0 — Skeleton
- [x] Cargo workspace: `easi-core`, `easi-cli`, `easi-gui`, top-level `easi` bin
- [x] Store shape stubbed: empty `State` / `Action` / `Effect` enums + exhaustive `reduce`
- [x] CLI subcommand skeleton (clap): install / uninstall / list / self-update, `-y`
- [x] GUI skeleton: default-style Slint window (`crates/easi-gui/ui/app.slint`)
- [x] CI (GitHub Actions): fmt, clippy `-D warnings`, test
- [x] `.gitignore`
- [x] License: Apache-2.0
- [x] Fill `State`/`Action`/`Effect` per the state sketch above
- [x] Effect runner loop (dispatch → reduce → run effects → dispatch results)
- [x] Reducer transition test table (one row per `(state, action) → next`)

## M1 — Detect & inspect (pure + read-only effects)
- [x] Magic-byte format table: AppImage t1/t2, gzip, xz, zstd, bzip2, zip
- [x] Archive listing without full extraction (tar/zip streaming)
- [x] AppImage: read embedded `.desktop` + icon from squashfs (no executing it)
- [x] Candidate scoring table: main executable, icon, app name, version
- [x] App id resolver: source-priority table + version/arch token-strip table (table-driven tests with real-world filenames)
- [x] `StartupWMClass` guess from ELF name; post-v1: learn the real one on first launch
- [x] Classification: `App(Gui) | App(Terminal) | NotAnApp | Ambiguous`
- [x] Trust summary: origin xattr, size, sha256, signature check, perms/escape scan

## M2 — Install engine + CLI
- [x] Install plan as data: `Format × AppKind → [Step]`
- [x] Safe extractor: temp dir, canonicalized paths under root, strip setuid
- [x] Transaction: manifest of created files, rollback on failure/cancel
- [x] Wiring: `.desktop` (rewritten `Exec`/`Icon`), hicolor icons, `~/.local/bin` symlink
- [x] Refresh: `update-desktop-database`, `gtk-update-icon-cache` / `kbuildsycoca` (via DE table)
- [x] Conflict detection by app id → replace flow
- [x] Manifest store (`~/.local/share/easi/installed/<id>.toml`)
- [x] `easi install | uninstall | list`, `-y`, Y/N prompts
- [x] PATH check + offer to fix shell rc
- [x] Offer move-to-trash (XDG trash spec)

## M3 — GUI (minimal Slint)
- [x] Drop window: app icon, name/version, Applications folder, in-window drag + Install button
- [x] Trust summary panel
- [x] Progress / success (trash checkbox) / failure screens
- [x] Wizard fallback: pick program, pick icon, confirm name
- [x] Conflict dialog (replace?)
- [x] GUI vs CLI selection in `easi` main
- [ ] → hand to user for visual design pass

## M4 — Desktop integration
- [x] `easi --setup`: register MIME types, set easi as default, remember previous handlers
- [x] Hand-off of non-app archives to remembered handler
- [x] DE quirk table: GNOME (Wayland + X11) verified
- [x] DE quirk table: KDE Plasma (Wayland + X11) verified

## M5 — Ship easi
- [x] GitHub Releases pipeline: binaries + sha256 + minisign sig
- [x] `curl | sh` install script (installs easi via its own flow, runs `--setup`)
- [x] `easi self-update`
- [x] README / user docs

## Post-v1 backlog
- [ ] Esoteric formats as encountered (`.run`/`.sh` self-extractors, 7z, bare binaries, …)
- [ ] More DEs: XFCE, Cinnamon, MATE, tiling WMs (rofi/wofi/fuzzel launchers)
- [ ] Optional system-wide installs (`/opt` via pkexec)
- [ ] More app id / metadata sources as formats grow (snap-style `meta/`, AppImage update info, ELF `.note` / embedded version strings, icon-from-ELF resources)
- [ ] Upstream update checks (AppImage zsync, GitHub releases)

## Wishlist
- [ ] `easi inspect <path>` — read-only inspection and trust summary without installing
- [ ] `--dry-run` flag for `easi install` — print planned writes without touching disk
- [ ] File picker button in empty GUI window (in addition to drag-and-drop and `easi <path>`)
- [ ] Desktop notification on install completion

## Open questions
- (none)
