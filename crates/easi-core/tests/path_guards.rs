//! Property tests for the path-escape guards: `safe_join`, the on-disk symlink resolver run by
//! `extract_archive_safe`, and the write-root check. Case counts stay modest so `cargo test`
//! stays fast; the hand-written hostile corpus lives next to the extractors.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Component, Path, PathBuf};

use easi_core::Format;
use easi_core::install::extract_archive_safe;
use easi_core::roots::{InstallRoots, safe_join};
use proptest::prelude::*;
use proptest::sample::select;
use tempfile::TempDir;

/// Where `path` really lands: the kernel's resolution of its longest existing prefix, with the
/// missing rest appended as plain names. Callers only pass rests without `..`, so lexical
/// appending cannot climb.
fn real_location(path: &Path) -> PathBuf {
    let comps: Vec<Component> = path.components().collect();
    (0..=comps.len())
        .rev()
        .find_map(|i| {
            let prefix: PathBuf = comps[..i].iter().collect();
            fs::canonicalize(prefix)
                .ok()
                .map(|canon| comps[i..].iter().fold(canon, |acc, c| acc.join(c)))
        })
        .unwrap_or_else(|| path.to_path_buf())
}

/// Every symlink under `dir`, without following any of them.
fn symlinks_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read_dir") {
        let path = entry.expect("entry").path();
        let file_type = fs::symlink_metadata(&path).expect("meta").file_type();
        if file_type.is_symlink() {
            out.push(path);
        } else if file_type.is_dir() {
            symlinks_under(&path, out);
        }
    }
}

/// Link spots in the `safe_join` tree; each may hold a link to one of `LINK_TARGETS`.
const LINK_SPOTS: &[&str] = &["l0", "a/l1", "a/b/l2", "c/l3"];
const LINK_TARGETS: &[&str] = &[
    "/",
    "/etc",
    ".",
    "..",
    "../..",
    "../../..",
    "../outside",
    "../../outside",
    "a",
    "a/b",
    "b",
    "missing",
    "l0",
    "../l0",
];
const REL_PARTS: &[&str] = &["a", "b", "c", "x", "l0", "l1", "l2", "l3", "..", ".", ""];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4096))]

    #[test]
    fn safe_join_never_lands_outside_root(
        links in proptest::collection::vec(proptest::option::of(select(LINK_TARGETS)), LINK_SPOTS.len()),
        parts in proptest::collection::vec(select(REL_PARTS), 0..6),
        absolute in any::<bool>(),
    ) {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("a/b")).expect("mkdir");
        fs::create_dir_all(root.join("c")).expect("mkdir");
        fs::create_dir_all(tmp.path().join("outside")).expect("mkdir");
        for (spot, target) in LINK_SPOTS.iter().zip(&links) {
            if let Some(target) = target {
                symlink(target, root.join(spot)).expect("symlink");
            }
        }
        let canon_root = fs::canonicalize(&root).expect("canon");
        let rel = format!("{}{}", if absolute { "/" } else { "" }, parts.join("/"));
        let rel = Path::new(&rel);
        if let Ok(joined) = safe_join(&root, rel) {
            prop_assert!(
                rel.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir)),
                "accepted {rel:?}"
            );
            let real = real_location(&joined);
            prop_assert!(real.starts_with(&canon_root), "{rel:?} -> {real:?}");
        }
    }

    #[test]
    fn extracted_symlinks_stay_inside_root(
        // Two names keep paths colliding often enough to build chains (`a/a -> ../a`, then
        // `a/a/b -> ../../a` really lands in `a` and climbs out). Fewer `..`s than the link's
        // parent depth, so most links pass the per-entry lexical check and the on-disk
        // resolver is what gets exercised.
        entries in proptest::collection::vec(
            (1usize..5).prop_flat_map(|depth| (
                proptest::collection::vec(select(&["a", "b"][..]), depth),
                // None = directory; Some((ups, names)) = link to `../` x ups + names.
                proptest::option::of((0..depth, proptest::collection::vec(select(&["a", "b"][..]), 0..3))),
            )),
            2..10,
        ),
    ) {
        let tmp = TempDir::new().expect("tempdir");
        let tar_path = tmp.path().join("gen.tar");
        let mut builder = tar::Builder::new(fs::File::create(&tar_path).expect("create"));
        for (path, link) in &entries {
            let path = path.join("/");
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_mode(0o755);
            match link {
                Some((ups, names)) => {
                    // Leading `..`s only: the oracle's lexical tail is then plain names.
                    let mut target: Vec<&str> = vec![".."; *ups];
                    target.extend(names.iter().copied());
                    let target = if target.is_empty() { ".".to_string() } else { target.join("/") };
                    header.set_entry_type(tar::EntryType::Symlink);
                    builder.append_link(&mut header, &path, &target).expect("link");
                }
                None => {
                    header.set_entry_type(tar::EntryType::Directory);
                    builder.append_data(&mut header, &path, std::io::empty()).expect("dir");
                }
            }
        }
        builder.finish().expect("finish");
        let root = tmp.path().join("root");
        fs::create_dir_all(&root).expect("mkdir");
        let root = fs::canonicalize(&root).expect("canon");
        if extract_archive_safe(&tar_path, Format::Tar, &root).is_ok() {
            let mut links = Vec::new();
            symlinks_under(&root, &mut links);
            for link in links {
                let parent = fs::canonicalize(link.parent().expect("parent")).expect("canon parent");
                let real = real_location(&parent.join(fs::read_link(&link).expect("read_link")));
                prop_assert!(real.starts_with(&root), "{link:?} -> {real:?} ({entries:?})");
            }
        }
    }

    #[test]
    fn write_root_guard_rejects_parent_and_relative_paths(
        base in 0usize..6,
        before in proptest::collection::vec(select(&["App", "bin", ".", "x"][..]), 0..4),
        after in proptest::collection::vec(select(&["App", "..", ".", ".bashrc"][..]), 0..4),
        relative in proptest::collection::vec(select(&["App", "bin", ".", ".."][..]), 1..5),
    ) {
        let roots = InstallRoots::from_home(Path::new("/home/u"));
        let bases = [
            &roots.applications_dir,
            &roots.desktop_dir,
            &roots.icons_dir,
            &roots.bin_dir,
            &roots.easi_dir,
            &roots.state_dir,
        ];
        // An allowed root with a `..` somewhere below it.
        let mut climbing = bases[base].clone();
        climbing.extend(before.iter().chain(&[".."]).chain(&after));
        prop_assert!(!roots.is_within_allowed_write_roots(&climbing), "{climbing:?}");
        let relative = PathBuf::from(relative.join("/"));
        prop_assert!(!roots.is_within_allowed_write_roots(&relative), "{relative:?}");
    }
}
