//! Kernel self-sandboxing for untrusted work, without `unsafe`: callers re-exec the `easi` binary
//! as a hidden helper subcommand that restricts *itself* with Landlock (best effort) and then
//! does the work or `exec`s the target. The achieved [`SandboxStatus`] is always reported so the
//! UI never claims protection the running kernel did not apply.

use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use landlock::{
    ABI, Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetError, RulesetStatus, Scope, path_beneath_rules,
};
use serde::{Deserialize, Serialize};

use crate::detect::{PackageContents, inspect_contents};
use crate::error::EasiError;
use crate::limits::LIMITS;
use crate::model::Format;
use crate::roots::InstallRoots;

/// Hidden subcommand: `__sandbox-exec --status <file> [--write <path>]... -- <cmd> [args]...`.
pub const SANDBOX_EXEC: &str = "__sandbox-exec";
/// Hidden subcommand: `__inspect <format-json> <path>`; prints an [`InspectReply`] as JSON.
pub const INSPECT: &str = "__inspect";

/// Bumped whenever [`InspectReply`] or [`PackageContents`] change shape, so a helper binary
/// swapped by self-update while the parent still runs yields a clean error instead of garbage.
pub const INSPECT_PROTOCOL: u32 = 1;

/// Newest Landlock ABI whose rights we request; best-effort mode drops what the kernel lacks.
const TARGET_ABI: ABI = ABI::V10;

/// Devices a POSIX shell needs to write even when everything else is read-only.
const SHELL_DEVICES: &[&str] = &["/dev/null", "/dev/tty"];

/// Where untrusted work runs: re-exec'd into the sandboxed `easi` helper binary (production), or
/// in this process without a sandbox (unit tests, which have no `easi` binary to re-exec).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Isolation {
    Helper(PathBuf),
    InProcess,
}

/// How much of the requested Landlock ruleset the kernel enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxStatus {
    FullyEnforced,
    PartiallyEnforced,
    NotEnforced,
}

struct StatusText {
    status: SandboxStatus,
    label: &'static str,
    script_note: &'static str,
}

const STATUS_TEXT: &[StatusText] = &[
    StatusText {
        status: SandboxStatus::FullyEnforced,
        label: "sandboxed (Landlock fully enforced)",
        script_note: "Runs sandboxed: it can only write to its install folder, ~/.config, ~/.local/share and easi's app folders.",
    },
    StatusText {
        status: SandboxStatus::PartiallyEnforced,
        label: "partially sandboxed (this kernel lacks some Landlock protections)",
        script_note: "Runs partially sandboxed: writes are limited to its install folder, ~/.config, ~/.local/share and easi's app folders, but this kernel lacks some Landlock protections.",
    },
    StatusText {
        status: SandboxStatus::NotEnforced,
        label: "not sandboxed (kernel Landlock unavailable)",
        script_note: "Runs NOT sandboxed: Landlock is unavailable here, so the script can modify any of your files.",
    },
];

impl SandboxStatus {
    fn text(self) -> &'static StatusText {
        // Fall back to the weakest claim so a missing row can never overstate protection.
        STATUS_TEXT
            .iter()
            .find(|t| t.status == self)
            .unwrap_or(&STATUS_TEXT[STATUS_TEXT.len() - 1])
    }

    pub fn label(self) -> &'static str {
        self.text().label
    }

    pub fn script_note(self) -> &'static str {
        self.text().script_note
    }
}

/// Landlock access granted beneath one path; everything not granted is denied.
#[derive(Debug, Clone, Copy)]
enum Grant {
    ReadExec,
    ReadFile,
    Full,
}

fn grant_access(grant: Grant) -> BitFlags<AccessFs> {
    match grant {
        Grant::ReadExec => AccessFs::from_read(TARGET_ABI),
        Grant::ReadFile => AccessFs::ReadFile.into(),
        Grant::Full => AccessFs::from_all(TARGET_ABI),
    }
}

/// Restrict this (single-threaded) process: all filesystem access is handled and only `rules`
/// are allowed; abstract UNIX sockets and signals are scoped to the sandbox; TCP/UDP is denied
/// when `deny_network`.
fn restrict_self(
    rules: &[(PathBuf, Grant)],
    deny_network: bool,
) -> Result<SandboxStatus, RulesetError> {
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(TARGET_ABI))?
        .scope(Scope::from_all(TARGET_ABI))?;
    if deny_network {
        ruleset = ruleset.handle_access(AccessNet::from_all(TARGET_ABI))?;
    }
    let mut created = ruleset.create()?;
    for (path, grant) in rules {
        created = created.add_rules(path_beneath_rules([path], grant_access(*grant)))?;
    }
    Ok(match created.restrict_self()?.ruleset {
        RulesetStatus::FullyEnforced => SandboxStatus::FullyEnforced,
        RulesetStatus::PartiallyEnforced => SandboxStatus::PartiallyEnforced,
        RulesetStatus::NotEnforced => SandboxStatus::NotEnforced,
    })
}

type HelperMain = fn(&[OsString]) -> ExitCode;

const HELPERS: &[(&str, HelperMain)] =
    &[(SANDBOX_EXEC, sandbox_exec_main), (INSPECT, inspect_main)];

/// Run a hidden helper subcommand if `args` (full argv) names one. Must run before any other
/// startup work so the helper restricts itself while still single-threaded.
pub fn run_helper(args: &[OsString]) -> Option<ExitCode> {
    let name = args.get(1)?.to_str()?;
    let (_, main) = HELPERS.iter().find(|(n, _)| *n == name)?;
    Some(main(&args[2..]))
}

fn helper_fail(what: &str, detail: impl std::fmt::Display) -> ExitCode {
    eprintln!("easi {what}: {detail}");
    // 126 = "found but could not run", like a shell: the target never started.
    ExitCode::from(126)
}

fn sandbox_exec_main(args: &[OsString]) -> ExitCode {
    let mut status_file: Option<PathBuf> = None;
    let mut rules: Vec<(PathBuf, Grant)> = vec![(PathBuf::from("/"), Grant::ReadExec)];
    let mut iter = args.iter();
    let command: Vec<&OsString> = loop {
        match iter.next().and_then(|a| a.to_str()) {
            Some("--status") => status_file = iter.next().map(PathBuf::from),
            Some("--write") => {
                if let Some(p) = iter.next() {
                    rules.push((PathBuf::from(p), Grant::Full));
                }
            }
            Some("--") => break iter.collect(),
            other => return helper_fail(SANDBOX_EXEC, format!("unexpected argument {other:?}")),
        }
    };
    let (Some(status_file), Some((program, program_args))) = (status_file, command.split_first())
    else {
        return helper_fail(
            SANDBOX_EXEC,
            "usage: --status <file> [--write <path>]... -- <cmd>",
        );
    };
    rules.extend(
        SHELL_DEVICES
            .iter()
            .map(|d| (PathBuf::from(d), Grant::Full)),
    );
    // Opened before restricting: the status file lives outside every writable path.
    let mut report = match File::create(&status_file) {
        Ok(f) => f,
        Err(e) => return helper_fail(SANDBOX_EXEC, e),
    };
    let status = match restrict_self(&rules, false) {
        Ok(s) => s,
        Err(e) => return helper_fail(SANDBOX_EXEC, e),
    };
    if let Err(e) = serde_json::to_writer(&mut report, &status) {
        return helper_fail(SANDBOX_EXEC, e);
    }
    drop(report);
    let err = Command::new(program).args(program_args).exec();
    helper_fail(SANDBOX_EXEC, err)
}

/// What the `__inspect` helper prints on stdout.
#[derive(Debug, Serialize, Deserialize)]
pub struct InspectReply {
    pub protocol: u32,
    pub sandbox: SandboxStatus,
    pub result: Result<PackageContents, String>,
}

/// Parsed first so a reply from a different helper version is reported, not misparsed.
#[derive(Deserialize)]
struct ProtocolOnly {
    protocol: u32,
}

fn inspect_main(args: &[OsString]) -> ExitCode {
    let [format_json, path] = args else {
        return helper_fail(INSPECT, "usage: <format-json> <path>");
    };
    let path = PathBuf::from(path);
    let format: Format = match format_json.to_str().map(serde_json::from_str) {
        Some(Ok(f)) => f,
        _ => return helper_fail(INSPECT, "invalid format argument"),
    };
    // Parsers are the attack surface: cap memory/CPU, forbid core files, read only the package.
    let caps = [
        (rlimit::Resource::AS, LIMITS.inspect_address_space_bytes),
        (rlimit::Resource::CPU, LIMITS.inspect_cpu_secs),
        (rlimit::Resource::CORE, 0),
    ];
    for (resource, cap) in caps {
        if let Err(e) = rlimit::setrlimit(resource, cap, cap) {
            return helper_fail(INSPECT, e);
        }
    }
    let sandbox = match restrict_self(&[(path.clone(), Grant::ReadFile)], true) {
        Ok(s) => s,
        Err(e) => return helper_fail(INSPECT, e),
    };
    let reply = InspectReply {
        protocol: INSPECT_PROTOCOL,
        sandbox,
        result: inspect_contents(&path, format).map_err(|e| e.to_string()),
    };
    let mut out = std::io::stdout().lock();
    let written = serde_json::to_writer(&mut out, &reply).map_err(std::io::Error::from);
    match written.and_then(|()| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => helper_fail(INSPECT, e),
    }
}

/// How a supervised child ended.
#[derive(Debug)]
pub(crate) enum WaitOutcome {
    Exited(ExitStatus),
    Cancelled,
    TimedOut,
}

/// Poll `child` until it exits, `cancel` is set, or `max` elapses; on the latter two kill its
/// whole process group (the child must be spawned with `process_group(0)`).
pub(crate) fn wait_or_kill(
    child: &mut Child,
    max: Duration,
    cancel: &AtomicBool,
) -> std::io::Result<WaitOutcome> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(WaitOutcome::Exited(status));
        }
        let outcome = if cancel.load(Ordering::Relaxed) {
            WaitOutcome::Cancelled
        } else if started.elapsed() > max {
            WaitOutcome::TimedOut
        } else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        // The group id is the child's pid (process_group(0)); `kill` avoids unsafe libc.
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", child.id())])
            .status();
        let _ = child.kill();
        let _ = child.wait();
        return Ok(outcome);
    }
}

fn inspection_failed(reason: impl Into<String>) -> EasiError {
    EasiError::InspectionFailed {
        reason: reason.into(),
    }
}

/// Parse untrusted package internals for `path` (already detected as `format`), in the
/// sandboxed helper when `isolation` names one. This is the only inspection entry callers use.
pub fn inspect_package(
    isolation: &Isolation,
    path: &Path,
    format: Format,
) -> Result<(PackageContents, SandboxStatus), EasiError> {
    let exe = match isolation {
        Isolation::InProcess => {
            return Ok((inspect_contents(path, format)?, SandboxStatus::NotEnforced));
        }
        Isolation::Helper(exe) => exe,
    };
    let format_json =
        serde_json::to_string(&format).map_err(|e| inspection_failed(e.to_string()))?;
    let mut child = Command::new(exe)
        .arg(INSPECT)
        .arg(format_json)
        .arg(path)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| inspection_failed("helper stdout unavailable"))?;
    // Drain stdout concurrently so a large reply cannot deadlock the pipe while we wait.
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        stdout
            .take(LIMITS.inspect_output_bytes + 1)
            .read_to_end(&mut buf)
            .map(|_| buf)
    });
    let outcome = wait_or_kill(
        &mut child,
        Duration::from_secs(LIMITS.inspect_wall_secs),
        &AtomicBool::new(false),
    )?;
    let output = reader
        .join()
        .map_err(|_| inspection_failed("helper output reader panicked"))??;
    let status = match outcome {
        WaitOutcome::Exited(status) => status,
        WaitOutcome::TimedOut => {
            return Err(inspection_failed(format!(
                "timed out after {}s",
                LIMITS.inspect_wall_secs
            )));
        }
        WaitOutcome::Cancelled => return Err(inspection_failed("cancelled")),
    };
    if output.len() as u64 > LIMITS.inspect_output_bytes {
        return Err(inspection_failed(format!(
            "result exceeded {} bytes",
            LIMITS.inspect_output_bytes
        )));
    }
    if let Some(sig) = status.signal() {
        return Err(inspection_failed(format!(
            "helper was killed by signal {sig} (crash, or memory/CPU limit reached)"
        )));
    }
    if !status.success() {
        return Err(inspection_failed(format!("helper exited with {status}")));
    }
    let header: ProtocolOnly = serde_json::from_slice(&output)
        .map_err(|e| inspection_failed(format!("malformed helper output: {e}")))?;
    if header.protocol != INSPECT_PROTOCOL {
        return Err(inspection_failed(format!(
            "helper speaks protocol {}, expected {INSPECT_PROTOCOL} — restart easi after updating it",
            header.protocol
        )));
    }
    let reply: InspectReply = serde_json::from_slice(&output)
        .map_err(|e| inspection_failed(format!("malformed helper output: {e}")))?;
    let contents = reply.result.map_err(inspection_failed)?;
    Ok((contents, reply.sandbox))
}

/// Resolve the running `easi` executable, e.g. to re-exec it as a helper or restart it.
/// On Linux, `/proc/self/exe` appends `" (deleted)"` once the old binary is replaced via
/// `rename()` (self-update), so strip it to reach the new binary at the same path.
pub fn resolve_current_exe(roots: &InstallRoots) -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        let s = exe.to_string_lossy();
        if let Some(stripped) = s.strip_suffix(" (deleted)") {
            let candidate = PathBuf::from(stripped);
            if candidate.exists() {
                return candidate;
            }
        }
        if exe.exists() {
            return exe;
        }
    }
    let fallback = roots.bin_dir.join("easi");
    if fallback.exists() {
        return fallback;
    }
    PathBuf::from("easi")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn every_status_has_its_own_text_row() {
        let statuses = [
            SandboxStatus::FullyEnforced,
            SandboxStatus::PartiallyEnforced,
            SandboxStatus::NotEnforced,
        ];
        for status in statuses {
            assert_eq!(status.text().status, status);
        }
        assert!(
            SandboxStatus::NotEnforced
                .script_note()
                .contains("NOT sandboxed")
        );
    }

    #[test]
    fn only_named_helpers_are_dispatched() {
        let argv = |a: &str| vec![OsString::from("easi"), OsString::from(a)];
        assert!(run_helper(&argv("install")).is_none());
        assert!(run_helper(&argv("__nope")).is_none());
        assert!(run_helper(&[OsString::from("easi")]).is_none());
    }

    #[test]
    fn helper_failures_map_to_typed_inspection_errors() {
        let tmp = TempDir::new().unwrap();
        let pkg = tmp.path().join("pkg.sh");
        std::fs::write(&pkg, "#!/bin/sh\necho hi\n").unwrap();
        // (fake helper body, expected reason fragment)
        let rows = [
            ("echo '{\"protocol\":999}'", "protocol 999"),
            ("echo 'not json'", "malformed helper output"),
            ("kill -9 $$", "killed by signal 9"),
            ("exit 3", "exited with"),
            (
                "echo '{\"protocol\":1,\"sandbox\":\"FullyEnforced\",\"result\":{\"Err\":\"boom\"}}'",
                "boom",
            ),
        ];
        for (i, (body, fragment)) in rows.iter().enumerate() {
            let helper = tmp.path().join(format!("helper{i}"));
            std::fs::write(&helper, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
            let err = inspect_package(&Isolation::Helper(helper), &pkg, Format::ShellInstaller)
                .expect_err(body);
            let msg = err.to_string();
            assert!(
                matches!(err, EasiError::InspectionFailed { .. }) && msg.contains(fragment),
                "{body}: {msg}"
            );
        }
    }

    #[test]
    fn in_process_inspection_reports_no_sandbox() {
        let tmp = TempDir::new().unwrap();
        let pkg = tmp.path().join("pkg.sh");
        std::fs::write(&pkg, "#!/bin/sh\necho hi\n").unwrap();
        let (contents, status) =
            inspect_package(&Isolation::InProcess, &pkg, Format::ShellInstaller).unwrap();
        assert_eq!(status, SandboxStatus::NotEnforced);
        assert!(contents.script_report.is_some());
    }
}
