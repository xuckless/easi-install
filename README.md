# easi

Make installing *downloaded* Linux apps (AppImage, `.tar.{gz,xz,zst,bz2}`, `.zip`) as easy as macOS drag-to-Applications.

## Quick install

```sh
curl -fsSL https://raw.githubusercontent.com/xuckl/easi-install/main/scripts/install.sh | sh
```

The installer downloads the latest verified release binary into `~/.local/bin/easi` and runs `easi --setup` to register `easi` as the default handler for supported package MIME types (remembering your previous archive manager so non-app archives are handed off silently).

## Usage

### GUI
- Double-click a downloaded `.AppImage`, `.tar.*`, or `.zip` in your file manager (or run `easi <path>`).
- Drag the app icon onto **Applications** inside the window — or press **Install**.
- If a package is ambiguous (multiple executables), a short step wizard lets you pick the main program, icon, and name.
- Non-app archives (documents, source trees, Windows binaries) are silently handed off to your previous archive manager.

### CLI
```sh
easi install <path>        # interactive install (Y/N prompts)
easi install -y <path>     # non-interactive / scriptable
easi list                  # list apps installed by easi
easi uninstall <app>       # clean uninstall (replays manifest in reverse)
easi --setup               # register MIME types and remember previous handlers
easi self-update           # update easi from GitHub Releases (SHA-256 verified)
```

## Supported formats (v1)
- AppImage (Type 1 and Type 2 — read via pure-Rust SquashFS, never executed during inspection)
- `.tar.gz`, `.tar.xz`, `.tar.zst`, `.tar.bz2`
- `.zip`

## Install locations (per-user)
`easi` never requires root in v1 and only writes under:
- `~/Applications/<App>/`
- `~/.local/share/applications/`
- `~/.local/share/icons/hicolor/`
- `~/.local/bin/`
- `~/.local/share/easi/installed/<app-id>.toml`

Every install is transactional: failures or cancellations roll back cleanly, path escapes (`../` and symlink escapes) are blocked, setuid/setgid bits are stripped, and uninstall replays the recorded manifest in reverse.

## Building from source

```sh
cargo build --release
```

Requires Rust stable (edition 2024) and on Linux: `libfontconfig1-dev` and `libxkbcommon-dev`.

## License

Apache-2.0 — see [LICENSE](LICENSE).
