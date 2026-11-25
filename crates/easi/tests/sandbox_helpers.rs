//! End-to-end checks of the hidden sandbox helpers through the real `easi` binary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

use easi_core::sandbox::{INSPECT, INSPECT_PROTOCOL, InspectReply, SANDBOX_EXEC, SandboxStatus};
use tempfile::TempDir;

const EASI: &str = env!("CARGO_BIN_EXE_easi");

fn enforced(status: SandboxStatus, what: &str) -> bool {
    if status == SandboxStatus::NotEnforced {
        eprintln!("skipping {what} denial assertions: kernel reports Landlock not enforced");
        return false;
    }
    true
}

#[test]
fn inspect_helper_returns_versioned_contents() {
    let tmp = TempDir::new().unwrap();
    let pkg = tmp.path().join("setup.sh");
    std::fs::write(&pkg, "#!/bin/sh\nmkdir -p \"$HOME/.config/x\"\n").unwrap();
    let out = Command::new(EASI)
        .args([INSPECT, "\"ShellInstaller\""])
        .arg(&pkg)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let reply: InspectReply = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(reply.protocol, INSPECT_PROTOCOL);
    assert!(reply.result.unwrap().script_report.is_some());
    eprintln!("inspect helper sandbox: {}", reply.sandbox.label());
}

#[test]
fn sandbox_exec_confines_writes_to_granted_paths() {
    let tmp = TempDir::new().unwrap();
    let granted = tmp.path().join("granted");
    let denied = tmp.path().join("denied");
    std::fs::create_dir_all(&granted).unwrap();
    std::fs::create_dir_all(&denied).unwrap();
    let status_file = tmp.path().join("status.json");
    let script = format!(
        "echo x > '{}/f'; echo x 2>/dev/null > '{}/f'; true",
        granted.display(),
        denied.display()
    );
    let out = Command::new(EASI)
        .arg(SANDBOX_EXEC)
        .arg("--status")
        .arg(&status_file)
        .arg("--write")
        .arg(&granted)
        .args(["--", "sh", "-c", &script])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let status: SandboxStatus =
        serde_json::from_slice(&std::fs::read(&status_file).unwrap()).unwrap();
    assert!(granted.join("f").exists());
    if enforced(status, "sandbox-exec") {
        assert!(!denied.join("f").exists());
    }
}
