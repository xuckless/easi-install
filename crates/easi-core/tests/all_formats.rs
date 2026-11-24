//! End-to-end integration tests across all 7 v1 formats:
//! AppImage Type 2 (real SquashFS), AppImage Type 1, .tar.gz, .tar.xz, .tar.zst, .tar.bz2, .zip.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs::{self, File};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use backhand::{FilesystemWriter, NodeHeader};
use bzip2::write::BzEncoder;
use easi_core::detect::detect_file_format;
use easi_core::install::uninstall_app;
use easi_core::manifest::list_manifests;
use easi_core::roots::InstallRoots;
use easi_core::runner::EffectRunner;
use easi_core::{Action, AppKind, DoneOutcome, Format, State, Store};
use flate2::write::GzEncoder;
use tempfile::TempDir;
use xz2::write::XzEncoder;

const FAKE_ELF: &[u8] = b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00";
const FAKE_PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake-png-bytes";

fn write_tar_entries<W: Write>(writer: W, files: &[(&str, &[u8], u32)]) -> W {
    let mut builder = tar::Builder::new(writer);
    for (rel, data, mode) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(*mode);
        header.set_cksum();
        builder
            .append_data(&mut header, *rel, *data)
            .expect("append tar entry");
    }
    builder.into_inner().expect("finish tar")
}

fn make_appimage_type2(dir: &Path, filename: &str) -> PathBuf {
    let mut fs_writer = FilesystemWriter::default();
    let dir_hdr = NodeHeader {
        permissions: 0o755,
        uid: 0,
        gid: 0,
        mtime: 0,
    };
    let file_hdr = NodeHeader {
        permissions: 0o644,
        uid: 0,
        gid: 0,
        mtime: 0,
    };
    fs_writer
        .push_file(
            Cursor::new(b"[Desktop Entry]\nType=Application\nName=Squash App\nExec=squashapp\nIcon=squashapp\n".as_slice()),
            "squashapp.desktop",
            file_hdr,
        )
        .expect("push desktop");
    fs_writer
        .push_dir("icons", dir_hdr)
        .expect("push icons dir");
    fs_writer
        .push_file(Cursor::new(FAKE_PNG), "icons/squashapp.png", file_hdr)
        .expect("push icon");

    let mut squashfs_bytes = Cursor::new(Vec::new());
    fs_writer
        .write(&mut squashfs_bytes)
        .expect("write squashfs");

    // Prefix with a 64-byte ELF header carrying AppImage Type 2 magic `AI\x02` at offset 8.
    let mut header = [0u8; 64];
    header[0..4].copy_from_slice(b"\x7fELF");
    header[4] = 2; // ELF64
    header[5] = 1; // little-endian
    header[8..11].copy_from_slice(b"AI\x02");

    let path = dir.join(filename);
    let mut out = File::create(&path).expect("create appimage2");
    out.write_all(&header).expect("write elf header");
    out.write_all(&squashfs_bytes.into_inner())
        .expect("write squashfs payload");
    path
}

fn make_appimage_type1(dir: &Path, filename: &str) -> PathBuf {
    let mut header = [0u8; 64];
    header[0..4].copy_from_slice(b"\x7fELF");
    header[4] = 2;
    header[5] = 1;
    header[8..11].copy_from_slice(b"AI\x01");
    let path = dir.join(filename);
    fs::write(&path, header).expect("write appimage1");
    path
}

fn make_zip(dir: &Path, filename: &str, files: &[(&str, &[u8], u32)]) -> PathBuf {
    let path = dir.join(filename);
    let file = File::create(&path).expect("create zip");
    let mut zip = zip::ZipWriter::new(file);
    for (rel, data, mode) in files {
        let opts = zip::write::SimpleFileOptions::default().unix_permissions(*mode);
        zip.start_file(*rel, opts).expect("start zip file");
        zip.write_all(data).expect("write zip data");
    }
    zip.finish().expect("finish zip");
    path
}

fn run_full_store_install(roots: &InstallRoots, pkg: &Path) -> (String, PathBuf) {
    let runner = EffectRunner::new(roots.clone());
    let mut store = Store::new();

    store.dispatch_and_run(
        Action::OpenPath(pkg.to_path_buf()),
        |e| runner.run(e),
        |_| {},
    );
    assert!(
        matches!(store.state(), State::Ready { .. }),
        "expected Ready for {}, got {:?}",
        pkg.display(),
        store.state()
    );

    store.dispatch_and_run(Action::ConfirmInstall, |e| runner.run(e), |_| {});
    assert!(
        matches!(store.state(), State::OfferTrash { .. }),
        "expected OfferTrash for {}, got {:?}",
        pkg.display(),
        store.state()
    );

    store.dispatch_and_run(Action::TrashSource, |e| runner.run(e), |_| {});
    match store.state() {
        State::Done {
            outcome: DoneOutcome::Installed { summary, trashed },
        } => {
            assert!(*trashed, "expected source moved to trash");
            assert!(!pkg.exists(), "source should no longer exist after trash");
            (summary.app_id.clone(), summary.install_dir.clone())
        }
        other => panic!("expected Done(Installed), got {other:?}"),
    }
}

#[test]
fn detects_installs_and_uninstalls_all_seven_v1_formats() {
    let tmp = TempDir::new().expect("tempdir");
    let roots = InstallRoots::from_home(tmp.path());
    let dl = tmp.path().join("Downloads");
    fs::create_dir_all(&dl).expect("mkdir downloads");

    let gui_files: &[(&str, &[u8], u32)] = &[
        ("Demo/bin/demo", FAKE_ELF, 0o755),
        (
            "Demo/demo.desktop",
            b"[Desktop Entry]\nName=Demo App\nExec=demo\nIcon=demo\n",
            0o644,
        ),
        ("Demo/icons/256x256/demo.png", FAKE_PNG, 0o644),
    ];
    let cli_files: &[(&str, &[u8], u32)] = &[("tool-1.0/mytool", FAKE_ELF, 0o755)];

    // 1. AppImage Type 2 (real SquashFS with embedded .desktop + icon)
    let ai2 = make_appimage_type2(&dl, "SquashApp-1.2.0-x86_64.AppImage");
    // 2. AppImage Type 1
    let ai1 = make_appimage_type1(&dl, "LegacyApp-0.9.AppImage");
    // 3. .tar.gz (GUI)
    let tgz = dl.join("demo-gz-1.0.tar.gz");
    write_tar_entries(
        GzEncoder::new(
            File::create(&tgz).expect("tgz"),
            flate2::Compression::fast(),
        ),
        gui_files,
    )
    .finish()
    .expect("finish tgz");
    // 4. .tar.xz (GUI)
    let txz = dl.join("demo-xz-1.0.tar.xz");
    write_tar_entries(
        XzEncoder::new(File::create(&txz).expect("txz"), 1),
        gui_files,
    )
    .finish()
    .expect("finish txz");
    // 5. .tar.zst (Terminal CLI)
    let tzst = dl.join("mytool-1.0.0-linux-x86_64.tar.zst");
    write_tar_entries(
        zstd::stream::write::Encoder::new(File::create(&tzst).expect("tzst"), 1).expect("zst enc"),
        cli_files,
    )
    .finish()
    .expect("finish tzst");
    // 6. .tar.bz2 (GUI)
    let tbz2 = dl.join("demo-bz2-1.0.tar.bz2");
    write_tar_entries(
        BzEncoder::new(
            File::create(&tbz2).expect("tbz2"),
            bzip2::Compression::fast(),
        ),
        gui_files,
    )
    .finish()
    .expect("finish tbz2");
    // 7. .zip (GUI)
    let zip_pkg = make_zip(&dl, "demo-zip-1.0.zip", gui_files);

    let cases = [
        ("appimage2", ai2, Format::AppImageType2, AppKind::Gui, true),
        ("appimage1", ai1, Format::AppImageType1, AppKind::Gui, false),
        ("tar.gz", tgz, Format::TarGz, AppKind::Gui, true),
        ("tar.xz", txz, Format::TarXz, AppKind::Gui, true),
        ("tar.zst", tzst, Format::TarZst, AppKind::Terminal, false),
        ("tar.bz2", tbz2, Format::TarBz2, AppKind::Gui, true),
        ("zip", zip_pkg, Format::Zip, AppKind::Gui, true),
    ];

    for (label, path, expected_fmt, expected_kind, expect_icon) in cases {
        assert_eq!(
            detect_file_format(&path).expect("detect format"),
            expected_fmt,
            "format detection failed for {label}"
        );

        let (app_id, install_dir) = run_full_store_install(&roots, &path);
        assert!(install_dir.exists(), "install dir missing for {label}");

        let desktop = roots.desktop_dir.join(format!("{app_id}.desktop"));
        if expected_kind == AppKind::Gui {
            assert!(desktop.exists(), ".desktop missing for {label}");
        } else {
            assert!(
                !desktop.exists(),
                "terminal app must not write .desktop ({label})"
            );
        }

        if expect_icon {
            let icon = roots.icons_dir.join(format!("256x256/apps/{app_id}.png"));
            assert!(icon.exists(), "hicolor icon missing for {label}");
        }

        assert_eq!(list_manifests(&roots).expect("list").len(), 1);
        uninstall_app(&roots, &app_id).expect("uninstall");
        assert!(
            list_manifests(&roots)
                .expect("list after uninstall")
                .is_empty(),
            "manifest left behind for {label}"
        );
        assert!(!desktop.exists(), ".desktop left behind for {label}");
    }
}

#[test]
fn detects_and_installs_tar_bare_elf_and_shell_installer_with_script_report() {
    let tmp = TempDir::new().expect("tempdir");
    let roots = InstallRoots::from_home(tmp.path());
    let dl = tmp.path().join("Downloads");
    fs::create_dir_all(&dl).expect("mkdir downloads");

    // 1. Plain uncompressed .tar
    let plain_tar = dl.join("plaintool-1.0.tar");
    write_tar_entries(
        File::create(&plain_tar).expect("tar"),
        &[("plaintool/mybin", FAKE_ELF, 0o755)],
    );
    assert_eq!(detect_file_format(&plain_tar).expect("fmt"), Format::Tar);
    let (tar_id, _) = run_full_store_install(&roots, &plain_tar);
    uninstall_app(&roots, &tar_id).expect("uninstall tar");

    // 2. Bare single-file ELF binary
    let bare_elf = dl.join("fastcli-2.0-linux-amd64");
    fs::write(&bare_elf, FAKE_ELF).expect("write elf");
    assert_eq!(detect_file_format(&bare_elf).expect("fmt"), Format::BareElf);
    let (elf_id, elf_dir) = run_full_store_install(&roots, &bare_elf);
    assert!(elf_dir.join("fastcli-2.0-linux-amd64").exists());
    uninstall_app(&roots, &elf_id).expect("uninstall elf");

    // 3. Shell installer (.sh) — static script analyzer detects actions + records created files
    let sh_pkg = dl.join("myinstaller-1.0.sh");
    fs::write(
        &sh_pkg,
        "#!/bin/sh\n# curl https://example.com\nmkdir -p \"$EASI_INSTALL_DIR\"\necho hi > \"$EASI_INSTALL_DIR/installed.txt\"\n",
    )
    .expect("write sh");
    assert_eq!(
        detect_file_format(&sh_pkg).expect("fmt"),
        Format::ShellInstaller
    );
    let runner = EffectRunner::new(roots.clone());
    let mut store = Store::new();
    store.dispatch_and_run(Action::OpenPath(sh_pkg.clone()), |e| runner.run(e), |_| {});
    match store.state() {
        State::Ready { plan, trust } => {
            assert!(!plan.planned_writes.is_empty());
            let report = trust
                .script_report
                .as_ref()
                .expect("script report must be present");
            assert!(
                report.actions.iter().any(|a| a.contains("network")),
                "expected network action in {:?}",
                report.actions
            );
        }
        other => panic!("expected Ready for shell installer, got {other:?}"),
    }
    store.dispatch_and_run(Action::ConfirmInstall, |e| runner.run(e), |_| {});
    assert!(matches!(store.state(), State::OfferTrash { .. }));
    let created_file = roots.applications_dir.join("Myinstaller/installed.txt");
    assert!(created_file.exists(), "script output must exist");
    uninstall_app(&roots, "easi.myinstaller").expect("uninstall sh app");
    assert!(
        !created_file.exists(),
        "uninstall must remove files created by script"
    );
}

#[test]
fn detects_jar_tar_lz4_metadata_and_blocks_distro_pkginfo() {
    let tmp = TempDir::new().expect("tempdir");
    let roots = InstallRoots::from_home(tmp.path());
    let dl = tmp.path().join("Downloads");
    fs::create_dir_all(&dl).expect("mkdir downloads");

    // 1. Executable .jar with Main-Class -> installed with java -jar wrapper
    let jar_pkg = make_zip(
        &dl,
        "MyLauncher-3.2.0.jar",
        &[(
            "META-INF/MANIFEST.MF",
            b"Manifest-Version: 1.0\nMain-Class: com.example.Main\n",
            0o644,
        )],
    );
    assert_eq!(detect_file_format(&jar_pkg).expect("fmt"), Format::Jar);
    let (jar_id, jar_dir) = run_full_store_install(&roots, &jar_pkg);
    assert!(jar_dir.join("MyLauncher-3.2.0.jar").exists());
    uninstall_app(&roots, &jar_id).expect("uninstall jar");

    // 2. .tar.lz4 with package.json metadata (productName + version)
    let tlz4 = dl.join("electronapp.tar.lz4");
    write_tar_entries(
        lz4_flex::frame::FrameEncoder::new(File::create(&tlz4).expect("lz4")),
        &[
            ("app/electronapp", FAKE_ELF, 0o755),
            ("app/chrome-sandbox", FAKE_ELF, 0o755),
            (
                "app/package.json",
                br#"{"productName":"Super Editor","version":"4.5.6"}"#,
                0o644,
            ),
        ],
    )
    .finish()
    .expect("finish lz4");
    assert_eq!(detect_file_format(&tlz4).expect("fmt"), Format::TarLz4);
    let runner = EffectRunner::new(roots.clone());
    let mut store = Store::new();
    store.dispatch_and_run(Action::OpenPath(tlz4), |e| runner.run(e), |_| {});
    match store.state() {
        State::Ready { plan, .. } => {
            assert_eq!(plan.name, "Super Editor");
            assert_eq!(plan.version.as_deref(), Some("4.5.6"));
            assert_eq!(plan.kind, AppKind::Gui);
        }
        other => panic!("expected Ready for tar.lz4, got {other:?}"),
    }

    // 3. Arch pacman .pkg.tar.zst (.PKGINFO) -> classified as NotAnApp
    let arch_pkg = dl.join("foo-1.0-1-x86_64.pkg.tar.zst");
    write_tar_entries(
        zstd::stream::write::Encoder::new(File::create(&arch_pkg).expect("zst"), 1).expect("enc"),
        &[
            (".PKGINFO", b"pkgname = foo\npkgver = 1.0-1\n", 0o644),
            ("usr/bin/foo", FAKE_ELF, 0o755),
        ],
    )
    .finish()
    .expect("finish arch pkg");
    let res = easi_core::inspect::classify_package(&arch_pkg, |_| None).expect("classify");
    assert!(matches!(res, easi_core::Classified::NotAnApp { .. }));
}
