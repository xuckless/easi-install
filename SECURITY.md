# Security Policy — Easi Install (`easi`)

## Supported Versions

| Version | Supported |
|---|---|
| `2.x` (latest) | Yes |
| `< 2.0` | No (please run `easi self-update`) |

## Security Model & Invariants

`easi` treats every downloaded package as untrusted input:

1. **Per-user sandbox roots only**: `easi` never elevates to root and only writes inside `$HOME` (`~/Applications`, `~/.local/share/applications`, `~/.local/share/icons/hicolor`, `~/.local/bin`, `~/.local/share/easi`, and opt-in `~/.config/systemd/user`).
2. **Path traversal & symlink escape prevention**: Every extracted path is canonicalized and verified via `safe_join` before writing. Archives containing `../` or symlink escapes are rejected.
3. **Privilege & permission hygiene**: `setuid` and `setgid` bits are stripped from all extracted files.
4. **No code execution during inspection**: AppImages (SquashFS), `.tar.*`, `.zip`, `.7z`, and `.jar` packages are inspected using pure-Rust parsers without executing anything inside the package.
5. **Static script analysis & explicit consent**: `.run`/`.sh`/`.bin` installers are statically scanned for privilege escalation (`sudo`/`pkexec`), network access, writes outside `$HOME`, and binary/obfuscated payloads; execution is blocked until the user explicitly consents.
6. **Transactional install & rollback**: Every install records a manifest of created files and backs up existing versions during replacement so failures or cancellations roll back cleanly.
7. **Cryptographically signed self-updates**: Official releases publish `SHA256SUMS` signed with `minisign`, with a trusted comment naming the release tag (`tag:vX.Y.Z`). `easi self-update` always verifies the signature in-process against the public key compiled into the binary (no external tool, no environment override), refuses the update if the signature is missing, invalid, or names a different tag, and only then checks the tarball's SHA-256 before atomically replacing the binary. The `install.sh` bootstrap verifies the signature when `minisign` is installed; without it, it verifies only the SHA-256 checksum and warns on stderr that the publisher signature was not checked.

## Reporting a Vulnerability

If you discover a security vulnerability (for example, a path-escape bypass, archive parser crash, signature verification flaw, or desktop-entry injection):

- **Please do NOT open a public GitHub issue.**
- Report it privately via **GitHub Security Advisories** on `https://github.com/xuckless/easi-install/security/advisories/new`.
- Include steps to reproduce and, if possible, a minimal test archive or proof of concept.

We will acknowledge reports promptly and coordinate a fix and release.
