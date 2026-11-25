//! The boundary of every mutating easi operation: an exclusive cross-process lock (GUI, CLI and
//! the systemd timer can run at once) and an append-only journal line per completed or failed
//! operation in `$XDG_STATE_HOME/easi/journal.log`.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::error::{EasiError, format_user_error};
use crate::roots::InstallRoots;

/// How long a waiting operation retries before giving up with [`EasiError::Busy`]; longer than
/// an installer script may run so a queued operation outlasts the one ahead of it.
const LOCK_WAIT: Duration = Duration::from_secs(600);
const LOCK_RETRY: Duration = Duration::from_millis(100);
/// The journal is renamed to `journal.log.1` (one generation) once it grows past this.
const JOURNAL_ROTATE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub enum LockWait {
    /// Fail with `Busy` at once: the GUI event-loop thread, and opportunistic cache writes.
    Never,
    /// Retry for up to `LOCK_WAIT`, calling the function once when the lock is found held.
    Patiently(fn()),
}

/// Exclusive `flock` on `~/.local/share/easi/.lock`, released on drop (or process exit).
/// Locks belong to the open file, so two operations in one process exclude each other too.
#[derive(Debug)]
pub struct StateLock {
    _file: File,
}

impl StateLock {
    pub fn acquire(roots: &InstallRoots, wait: LockWait) -> Result<Self, EasiError> {
        fs::create_dir_all(&roots.easi_dir)?;
        let path = roots.lock_path();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)?;
        let started = Instant::now();
        let mut announced = false;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(TryLockError::Error(e)) => return Err(EasiError::Io(e)),
                Err(TryLockError::WouldBlock) => {}
            }
            match wait {
                LockWait::Patiently(on_wait) if started.elapsed() < LOCK_WAIT => {
                    if !announced {
                        on_wait();
                        announced = true;
                    }
                    std::thread::sleep(LOCK_RETRY);
                }
                LockWait::Patiently(_) | LockWait::Never => return Err(EasiError::Busy(path)),
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Install,
    Update,
    Uninstall,
    Adopt,
    Repair,
    Organize,
    Forget,
    SetSource,
    Import,
    LearnWmClass,
    Config,
    Setup,
    RestoreHandlers,
    SelfUpdate,
    SelfRollback,
    SelfUninstall,
}

const OP_NAMES: &[(Op, &str)] = &[
    (Op::Install, "install"),
    (Op::Update, "update"),
    (Op::Uninstall, "uninstall"),
    (Op::Adopt, "adopt"),
    (Op::Repair, "repair"),
    (Op::Organize, "organize"),
    (Op::Forget, "forget"),
    (Op::SetSource, "set-source"),
    (Op::Import, "import"),
    (Op::LearnWmClass, "learn-wmclass"),
    (Op::Config, "config"),
    (Op::Setup, "setup"),
    (Op::RestoreHandlers, "restore-handlers"),
    (Op::SelfUpdate, "self-update"),
    (Op::SelfRollback, "self-rollback"),
    (Op::SelfUninstall, "self-uninstall"),
];

/// What an install or update was made from, journaled with its outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRecord {
    pub filename: String,
    pub sha256: String,
    pub created_files: usize,
}

/// Run one mutating operation under [`StateLock`] and journal its outcome. An operation that
/// never got the lock did nothing, so it is not journaled.
pub fn run_op<T>(
    roots: &InstallRoots,
    wait: LockWait,
    op: Op,
    subject: &str,
    f: impl FnOnce() -> Result<T, EasiError>,
) -> Result<T, EasiError> {
    run_op_with_source(roots, wait, op, subject, f, |_| None)
}

/// [`run_op`] for installs and updates: `source` names what a successful one was made from.
pub fn run_op_with_source<T>(
    roots: &InstallRoots,
    wait: LockWait,
    op: Op,
    subject: &str,
    f: impl FnOnce() -> Result<T, EasiError>,
    source: impl FnOnce(&T) -> Option<SourceRecord>,
) -> Result<T, EasiError> {
    let _lock = StateLock::acquire(roots, wait)?;
    let result = f();
    let outcome = match &result {
        Ok(value) => match source(value) {
            Some(src) => format!(
                "ok source={} sha256={} files={}",
                one_token(&src.filename),
                src.sha256,
                src.created_files
            ),
            None => "ok".to_string(),
        },
        Err(err) => format!("failed error={:?}", format_user_error(err)),
    };
    let name = OP_NAMES
        .iter()
        .find(|(o, _)| *o == op)
        .map_or("unknown", |(_, n)| n);
    let line = format!(
        "{} {name} {} {outcome}\n",
        rfc3339_utc(SystemTime::now()),
        one_token(subject)
    );
    // The journal is a record, not a precondition: failing to write it never fails the op.
    let _ = append_journal(roots, &line);
    result
}

fn append_journal(roots: &InstallRoots, line: &str) -> std::io::Result<()> {
    let path = roots.journal_path();
    fs::create_dir_all(&roots.state_dir)?;
    if fs::metadata(&path).is_ok_and(|m| m.len() > JOURNAL_ROTATE_BYTES) {
        fs::rename(&path, path.with_extension("log.1"))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)?;
    // One write per line so concurrent appenders never interleave inside a line.
    file.write_all(line.as_bytes())
}

/// The last `n` journal lines, oldest first, including the rotated generation.
pub fn journal_tail(roots: &InstallRoots, n: usize) -> Result<Vec<String>, EasiError> {
    let path = roots.journal_path();
    let mut lines = Vec::new();
    for file in [path.with_extension("log.1"), path] {
        match fs::read_to_string(&file) {
            Ok(text) => lines.extend(text.lines().map(str::to_string)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(EasiError::Io(e)),
        }
    }
    Ok(lines.split_off(lines.len().saturating_sub(n)))
}

/// Keep a journal field one whitespace-free token so every line splits the same way.
fn one_token(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_whitespace() || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// RFC 3339 UTC timestamp (`2026-11-25T08:30:00Z`) via Hinnant's civil-from-days, so no date
/// crate is needed.
fn rfc3339_utc(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn every_op_has_its_own_name_row() {
        let ops = [
            Op::Install,
            Op::Update,
            Op::Uninstall,
            Op::Adopt,
            Op::Repair,
            Op::Organize,
            Op::Forget,
            Op::SetSource,
            Op::Import,
            Op::LearnWmClass,
            Op::Config,
            Op::Setup,
            Op::RestoreHandlers,
            Op::SelfUpdate,
            Op::SelfRollback,
            Op::SelfUninstall,
        ];
        assert_eq!(OP_NAMES.len(), ops.len());
        for op in ops {
            assert_eq!(
                OP_NAMES.iter().filter(|(o, _)| *o == op).count(),
                1,
                "{op:?}"
            );
        }
    }

    #[test]
    fn rfc3339_matches_known_instants() {
        // (unix seconds, RFC 3339 UTC)
        let rows = [
            (0, "1970-01-01T00:00:00Z"),
            (951_782_400, "2000-02-29T00:00:00Z"),
            (1_709_251_199, "2024-02-29T23:59:59Z"),
            (1_795_622_400, "2026-11-25T16:00:00Z"),
            (4_102_444_800, "2100-01-01T00:00:00Z"),
        ];
        for (secs, expected) in rows {
            let t = UNIX_EPOCH + Duration::from_secs(secs);
            assert_eq!(rfc3339_utc(t), expected, "{secs}");
        }
    }

    #[test]
    fn lock_excludes_a_second_holder_until_dropped() {
        let tmp = tempfile::TempDir::new().unwrap();
        let roots = InstallRoots::from_home(tmp.path());
        let held = StateLock::acquire(&roots, LockWait::Never).unwrap();
        assert!(matches!(
            StateLock::acquire(&roots, LockWait::Never),
            Err(EasiError::Busy(_))
        ));
        assert!(matches!(
            run_op(&roots, LockWait::Never, Op::Repair, "easi.x", || Ok(())),
            Err(EasiError::Busy(_))
        ));
        drop(held);
        // A sibling test forking right now shares the lock's open file until its child execs
        // (CLOEXEC), so allow a brief retry.
        let freed = (0..50).any(|_| {
            let ok = StateLock::acquire(&roots, LockWait::Never).is_ok();
            if !ok {
                std::thread::sleep(Duration::from_millis(20));
            }
            ok
        });
        assert!(freed);
        // A busy operation never ran, so it left no journal line.
        assert!(journal_tail(&roots, 10).unwrap().is_empty());
    }

    fn quiet() {}

    /// Sibling tests forking can hold a just-released lock for an instant, so queue.
    const QUEUE: LockWait = LockWait::Patiently(quiet);

    #[test]
    fn journal_records_outcomes_and_rotates_one_generation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let roots = InstallRoots::from_home(tmp.path());
        let src = SourceRecord {
            filename: "My App.AppImage".into(),
            sha256: "ab12".into(),
            created_files: 3,
        };
        let _ = run_op_with_source(
            &roots,
            QUEUE,
            Op::Install,
            "easi.my-app",
            || Ok(()),
            |_| Some(src.clone()),
        );
        let _ = run_op(&roots, QUEUE, Op::Forget, "easi.other", || {
            Err::<(), _>(EasiError::NotInstalled("easi.other".into()))
        });
        let lines = journal_tail(&roots, 10).unwrap();
        // (line, suffix after the timestamp)
        let rows = [
            (
                &lines[0],
                " install easi.my-app ok source=My_App.AppImage sha256=ab12 files=3",
            ),
            (
                &lines[1],
                " forget easi.other failed error=\"app not installed: easi.other\"",
            ),
        ];
        for (line, suffix) in rows {
            assert!(line.ends_with(suffix), "{line}");
            assert_eq!(line.find(' '), Some("1970-01-01T00:00:00Z".len()), "{line}");
        }

        fs::write(
            roots.journal_path(),
            "x".repeat(JOURNAL_ROTATE_BYTES as usize + 1),
        )
        .unwrap();
        let _ = run_op(&roots, QUEUE, Op::Repair, "easi.r", || Ok(()));
        let current = fs::read_to_string(roots.journal_path()).unwrap();
        assert_eq!(current.lines().count(), 1);
        assert!(current.contains(" repair easi.r ok"));
        let rotated = roots.journal_path().with_extension("log.1");
        assert_eq!(
            fs::metadata(rotated).unwrap().len(),
            JOURNAL_ROTATE_BYTES + 1
        );
        assert_eq!(journal_tail(&roots, 1).unwrap().len(), 1);
    }
}
