//! Runs the real `nebula` binary against temporary folders.
#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn nebula(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nebula"))
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .unwrap()
}

fn ok(home: &Path, args: &[&str]) -> String {
    let out = nebula(home, args);
    assert!(
        out.status.success(),
        "nebula {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn id_is_stable() {
    let home = tempfile::tempdir().unwrap();
    let a = ok(home.path(), &["id"]);
    assert_eq!(a.trim().len(), 64);
    assert_eq!(ok(home.path(), &["id"]), a);
}

#[test]
fn add_scan_edit_rescan_remove() {
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let sync = dir.path().to_str().unwrap();
    fs::write(dir.path().join("notes.txt"), "hello").unwrap();

    ok(home.path(), &["add", sync]);
    assert!(ok(home.path(), &["folders"]).contains("not scanned yet"));

    let out = ok(home.path(), &["scan"]);
    assert!(out.contains("+ added"), "{out}");
    assert!(out.contains("notes.txt"));

    fs::write(dir.path().join("notes.txt"), "hello, world").unwrap();
    let out = ok(home.path(), &["scan", sync]);
    assert!(out.contains("1 modified"), "{out}");

    fs::remove_file(dir.path().join("notes.txt")).unwrap();
    assert!(ok(home.path(), &["scan"]).contains("1 deleted"));
    assert!(ok(home.path(), &["ls", sync]).contains("deleted"));

    ok(home.path(), &["remove", sync]);
    assert!(ok(home.path(), &["folders"]).contains("No synced folders"));
    assert!(dir.path().exists());
}

#[test]
fn rejects_overlapping_and_missing_folders() {
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let inner = dir.path().join("inner");
    fs::create_dir(&inner).unwrap();
    ok(home.path(), &["add", dir.path().to_str().unwrap()]);
    assert!(
        !nebula(home.path(), &["add", inner.to_str().unwrap()])
            .status
            .success()
    );
    assert!(
        !nebula(home.path(), &["add", dir.path().to_str().unwrap()])
            .status
            .success()
    );
    let missing = dir.path().join("nope");
    assert!(
        !nebula(home.path(), &["add", missing.to_str().unwrap()])
            .status
            .success()
    );
}

#[test]
fn refuses_folder_containing_its_own_state() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join(".nebula");
    let out = nebula(&home, &["add", dir.path().to_str().unwrap()]);
    assert!(!out.status.success());
}
