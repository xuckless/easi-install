# Security Policy — Easi Install (`easi`)

## Supported Versions

| Version | Supported |
|---|---|
| `2.x` (latest) | Yes |
| `< 2.0` | No (please run `easi self-update`) |

## Security Model & Invariants

`easi` treats every downloaded package as untrusted input:

1. **Per-user write roots only**: `easi` never elevates to root and only writes inside `$HOME` (`~/Applications`, `~/.local/share/applications`, `~/.local/share/icons/hicolor`, `~/.local/bin`, `~/.local/share/easi`, the operation journal in `$XDG_STATE_HOME/easi` (default `~/.local/state/easi`), and opt-in `~/.config/systemd/user`). The write-root check refuses relative paths and any `..` component.
2. **Path traversal & symlink escape prevention**: Every extracted path goes through `safe_join`, which rejects `..`, absolute names and any existing (or dangling) symlink on the way that resolves outside the extraction root. Symlink entries must stay inside the root lexically, and after extraction every link in the tree is re-resolved against the real files (chains, loops and links into the root itself are rejected). Hard links, device and FIFO entries are never created. Archives are capped at 100,000 entries and 16 GiB extracted. `safe_join`, the on-disk resolver and the write-root check are property-tested, and a hostile-archive corpus (zip-slip, absolute names, hard links, devices, duplicate file/symlink entries, deep nesting, long and non-UTF-8 names, entry and size bombs) runs in CI.
3. **Privilege & permission hygiene**: `setuid` and `setgid` bits are stripped from all extracted files, and files easi creates honour the user's umask.
4. **No code execution during inspection**: AppImages (SquashFS), `.tar.*`, `.zip`, `.7z`, and `.jar` packages are inspected using pure-Rust parsers without executing anything inside the package. Parsing runs in a re-executed `easi __inspect` helper that restricts itself with Landlock (read the package only, no TCP/UDP) and resource limits (2 GiB address space, 60 s CPU, 120 s wall clock).
5. **Static script analysis & explicit consent**: `.run`/`.sh`/`.bin` installers are statically scanned for privilege escalation (`sudo`/`pkexec`), network access, writes outside `$HOME`, and binary/obfuscated payloads; execution is blocked until the user explicitly consents. A consented installer runs in `easi __sandbox-exec` (Landlock write allowlist, scrubbed environment, 300 s limit); new or changed autostart/systemd/environment.d entries fail the install. Landlock is best-effort and the achieved status is always shown, never overstated.
6. **Transactional install & rollback**: Every install records a manifest of created files and backs up existing versions during replacement so failures or cancellations roll back cleanly.
7. **Crash-safe state**: Manifests, config, launchers and units are replaced atomically (temp file + fsync + rename). Mutating operations hold an exclusive lock on `~/.local/share/easi/.lock` and append to an operation journal. Unreadable manifests, handler records and config are kept aside, never silently deleted.
8. **Desktop-entry injection**: `Exec=` program paths are always quoted and escaped per the Desktop Entry spec; organizing an app re-renders every moved `Exec=` argument with the same quoting.
9. **App update trust**: Updates verify any digest the source publishes (GitHub asset SHA-256, electron-builder SHA-512, zsync SHA-1) and fail hard on mismatch. Download hosts must be on the source's allowlist or equal the host pinned on first update, and a publisher signature recorded at install must still verify with the same key; changing either requires an explicit re-trust.
10. **Cryptographically signed self-updates**: Official releases publish `SHA256SUMS` signed with `minisign`, with a trusted comment naming the release tag (`tag:vX.Y.Z`). `easi self-update` always verifies the signature in-process against the public key compiled into the binary (no external tool, no environment override), refuses the update if the signature is missing, invalid, or names a different tag, and only then checks the tarball's SHA-256. The new binary is staged under a random name beside the old one and must report the expected version via `--version` (10 s limit) before it atomically replaces the binary; the replaced binary is kept as `<bin>.prev` for `easi self-update --rollback`. The `install.sh` bootstrap verifies the signature and the same `tag:` binding when `minisign` is installed; without it, it verifies only the SHA-256 checksum and warns on stderr that the publisher signature was not checked.
11. **Build supply chain**: CI and release workflows pin every third-party action to a full commit SHA, build and test with `--locked`, and run `cargo-deny` (RustSec advisories, licenses, sources) on every push plus a weekly advisory scan; ignored advisories are listed with their reason in `deny.toml`. Releases run the full test suite before packaging, publish GitHub build-provenance attestations for each tarball, and never run concurrently.

## Verifying a Release

The release public key is `RWQnPmfJNaxDO2ecscxiezHEPZmmtizudyau8OENs5gw21DZWlmc2IRQ` (also in `scripts/install.sh`).

```sh
TAG=v2.5.10 ARCH=x86_64
base=https://github.com/xuckless/easi-install/releases/download/$TAG
curl -fsSLO "$base/easi-$TAG-linux-$ARCH.tar.gz" -O "$base/SHA256SUMS" -O "$base/SHA256SUMS.minisig"

# 1. Publisher signature, bound to this exact tag (an older signed release must not pass).
minisign -V -P RWQnPmfJNaxDO2ecscxiezHEPZmmtizudyau8OENs5gw21DZWlmc2IRQ -m SHA256SUMS \
  | sed -n 's/^Trusted comment: //p' | tr ' ' '\n' | grep -Fqx "tag:$TAG" && echo "signature ok"

# 2. Tarball integrity.
sha256sum --check --ignore-missing SHA256SUMS

# 3. Build provenance: built by this repository's release workflow.
gh attestation verify "easi-$TAG-linux-$ARCH.tar.gz" --repo xuckless/easi-install
```

Releases before v2.5.8 have no `tag:` token in the trusted comment; attestations exist from the first release built with provenance enabled.

## Reporting a Vulnerability

If you discover a security vulnerability (for example, a path-escape bypass, archive parser crash, signature verification flaw, or desktop-entry injection):

- **Please do NOT open a public GitHub issue.**
- Report it privately via **GitHub Security Advisories** on `https://github.com/xuckless/easi-install/security/advisories/new`.
- Include steps to reproduce and, if possible, a minimal test archive or proof of concept.

We will acknowledge reports promptly and coordinate a fix and release.
