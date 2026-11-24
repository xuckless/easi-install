<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/readme-header-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/readme-header-light.svg">
    <img alt="Easi Install" src="assets/readme-header-light.svg" width="100%">
  </picture>
</p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/flow-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/flow-light.svg">
    <img alt="How easi works" src="assets/flow-light.svg" width="100%">
  </picture>
</p>

Install *downloaded* Linux apps as easily as macOS drag-to-Applications — with clean uninstall, pre-install permissions, and zero root required.

## Quick install

```sh
curl -fsSL https://raw.githubusercontent.com/xuckless/easi-install/main/scripts/install.sh | sh
```

Installs `easi` into `~/.local/bin/easi` (SHA-256 verified) and runs `easi --setup` to register it for supported package types while remembering your previous archive manager for non-app archives.

## Usage

### GUI
- **Double-click** any downloaded package in your file manager (or right-click → **Open With → easi**), review the permissions/trust summary, and drag the app into **Applications** (or click **Install**).
- **Open `easi` directly** for the standalone **App Manager**: browse packages via the native OS file picker, check for upstream app updates, toggle automatic `easi` update checks, and uninstall apps in one click.
- **Non-app archives** (documents, source trees, Windows binaries, distro packages) are handed off silently to your previous archive/file manager.

### CLI
```sh
easi install <path>                # interactive install (Y/N prompts)
easi install --dry-run <path>      # preview planned writes without touching disk
easi inspect <path>                # read-only trust & permissions summary
easi install -y <path>             # non-interactive / scriptable
easi list                          # list installed apps (reconciles self-updated apps)
easi export [apps.toml]            # export managed apps & update sources
easi import <apps.toml>            # reinstall exported apps on a new Linux machine
easi scan                          # discover existing unmanaged user apps in $HOME
easi adopt <app|path> [--all]      # adopt existing app(s) and repair icons/launchers
easi repair <app>                  # repair missing icon, .desktop, StartupWMClass, or CLI link
easi set-source <app> [url]        # auto-detect or link update source (GitHub, GitLab, Codeberg, Electron CDN, JetBrains, direct URL)
easi forget <app>                  # stop managing an app without deleting its files
easi uninstall <app>               # clean uninstall (replays manifest in reverse)
easi check-updates                 # check upstream releases (AppImage zsync / GitHub Releases)
easi self-update                   # update easi from GitHub Releases (SHA-256 verified)
easi config --auto-update false    # disable brew-style auto update checks (or EASI_NO_AUTO_UPDATE=1)
easi --setup                       # register MIME types and remember previous handlers
```

## Supported formats
- **AppImage** (Type 1 & Type 2 — read via pure-Rust SquashFS, never executed during inspection)
- **Archives**: `.tar`, `.tar.gz`, `.tar.xz`, `.tar.zst`, `.tar.bz2`, `.tar.lz4`, `.zip`, `.7z`
- **Single-file apps**: executable `.jar` (wrapped with `java -jar`), bare Linux ELF binaries
- **Self-extracting scripts**: `.run`, `.sh`, `.bin` (statically scanned for privileges, network, system writes, and obfuscation; requires explicit user consent)

*(Distro package formats like `.deb`, `.rpm`, Flatpak, Snap, and Arch `.pkg.tar.zst` are intentionally left to your distro's package manager.)*

## Desktop environments
Per-user only (works on immutable roots like SteamOS): **GNOME**, **KDE Plasma** (SteamOS / CachyOS), **Cinnamon / MATE / XFCE** (Linux Mint), **COSMIC**, **Hyprland**, and **Sway**.

## Safety & install locations
`easi` never requires root and only writes under:
- `~/Applications/<App>/`
- `~/.local/share/applications/`
- `~/.local/share/icons/hicolor/`
- `~/.local/bin/`
- `~/.local/share/easi/installed/<app-id>.toml`

Every install is transactional with rollback on failure/cancel, blocks archive path/symlink escapes, strips setuid/setgid bits, reconciles apps that self-update in place, and sends desktop notifications on install, uninstall, and updates.

## Building from source

```sh
cargo build --release
```

Requires Rust stable (edition 2024) and on Linux: `libfontconfig1-dev` and `libxkbcommon-dev`.

## License

Apache-2.0 — see [LICENSE](LICENSE).
