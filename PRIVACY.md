# Privacy & Permissions Policy — Easi Install (`easi`)

**Plain-English summary:** `easi` is a 100% local, open-source Linux app installer and manager. It collects **zero telemetry**, **zero analytics**, **zero crash reports**, and **never runs as root**.

---

## What `easi` does (and why)

Linux doesn't have a built-in macOS-style "drag to Applications" workflow for standalone downloads (`.AppImage`, `.tar.*`, `.zip`, `.7z`, `.jar`, bare binaries). `easi` automates that plumbing safely inside your personal user folder so you can install, launch, update, repair, and cleanly uninstall downloaded apps without touching system files.

---

## Every permission `easi` uses — in plain English

| Area | What `easi` does | Why it is needed |
|---|---|---|
| **App installation folder** (`~/Applications/`) | Copies or extracts downloaded apps into your personal `Applications` folder. | Gives every downloaded app a clean, predictable home that can be removed cleanly later. |
| **App menu & icons** (`~/.local/share/applications/`, `~/.local/share/icons/hicolor/`) | Creates `.desktop` launcher entries and installs app icons. | Lets your desktop menu, dock, and Alt-Tab switcher show the app with its proper name and icon. |
| **Command-line links** (`~/.local/bin/`) | Adds symlinks for installed apps and CLI tools. | Lets you launch installed tools from your terminal. |
| **App records & settings** (`~/.local/share/easi/`) | Stores a plain-text manifest of every file `easi` created for each app, plus your preferences. | Powers clean one-click uninstall, rollback on failure, and restoring previous file handlers. |
| **Scanning existing user apps** (`$HOME`) | Reads `~/.local/share/applications/`, `~/Applications/`, and `~/.local/bin/` to list unmanaged user apps. | Lets you adopt and repair broken icons/launchers for apps you installed manually before `easi`. Never touches `/usr`, Flatpak, Snap, Nix, Steam, or Wine. |
| **Default file handler** (opt-in) | Sets `easi` as the default handler for `.AppImage` files, and adds `easi` to **Right-Click → Open With** for `.tar.*`, `.zip`, `.7z`, `.jar`, and `.sh`. | Lets you double-click AppImages or right-click archives to install them. Your normal archive manager is never hijacked by default, and previous handlers are backed up and restorable at any time. |
| **Network requests** (opt-in / user-triggered) | Connects directly to GitHub, GitLab, Codeberg, JetBrains CDN, or an app's recorded CDN/zsync URL **only** when checking for or downloading updates. | Lets you keep `easi` and your managed apps up to date. You can disable automatic checks in Settings or with `EASI_NO_AUTO_UPDATE=1`. |
| **Background update timer** (opt-in, off by default) | Creates a `systemd --user` daily timer (`~/.config/systemd/user/easi-check-updates.timer`). | Checks for app updates once a day using 0 MB RAM while idle and shows a desktop notification. Never runs a persistent background daemon. |

---

## Why `easi` is NOT malware

- **No root, ever:** `easi` never asks for `sudo`, `pkexec`, or system-wide write access. It cannot modify `/usr`, `/etc`, `/bin`, or your distro's package manager.
- **No silent execution during inspection:** `easi` inspects AppImages and archives using pure-Rust readers — it **never executes** an AppImage or bundled script just to preview it.
- **Explicit script consent:** Installer scripts (`.run`, `.sh`, `.bin`) are statically scanned for privilege escalation, network calls, system writes, and obfuscation, and require your explicit checkbox consent before running.
- **Path-escape & setuid protection:** Every extracted file path is canonicalized and verified to stay inside its target folder; setuid/setgid bits are stripped automatically.
- **Signed, verifiable releases:** Official `easi` releases are verified with **SHA-256** and **minisign** cryptographic signatures before `easi self-update` swaps the binary.
- **Zero hidden network traffic:** No telemetry, no ads, no phone-home servers, no live traffic interception.
- **Clean exit:** You can uninstall `easi` at any time (`easi self-uninstall` or **Settings → Danger Zone**), which restores your previous file handlers and leaves all your installed apps intact.
- **100% Open Source:** Every line of code is publicly auditable at `https://github.com/xuckless/easi-install`.

---

## Terms of Use & License

`easi` is free, open-source software licensed under the **Apache License, Version 2.0** (`LICENSE`).

In accordance with Section 7 & 8 of the Apache-2.0 License, `easi` is provided **"AS IS"**, **WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND**, either express or implied (including warranties of merchantability, fitness for a particular purpose, or non-infringement). You are responsible for reviewing the trust summary (SHA-256, origin, signature, and script warnings) of any third-party software you choose to install.
