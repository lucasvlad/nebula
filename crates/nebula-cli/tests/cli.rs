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

fn nebula_in(dir: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nebula"))
        .current_dir(dir)
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn removes_a_deleted_folder_by_relative_path() {
    let home = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("docs");
    fs::create_dir(&dir).unwrap();
    ok(home.path(), &["add", dir.to_str().unwrap(), "--as", "work"]);
    fs::remove_dir(&dir).unwrap();

    let out = nebula_in(parent.path(), home.path(), &["remove", "docs"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(ok(home.path(), &["folders"]).contains("No synced folders"));
}

#[test]
fn removes_a_deleted_folder_by_name() {
    let home = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("photos");
    fs::create_dir(&dir).unwrap();
    ok(
        home.path(),
        &["add", dir.to_str().unwrap(), "--as", "Holiday pics"],
    );
    fs::remove_dir(&dir).unwrap();

    assert!(ok(home.path(), &["ls", "Holiday pics"]).contains("Nothing recorded"));
    let out = ok(home.path(), &["remove", "Holiday pics"]);
    assert!(out.contains("Stopped syncing"), "{out}");
    assert!(ok(home.path(), &["folders"]).contains("No synced folders"));
    assert!(
        !nebula(home.path(), &["remove", "Holiday pics"])
            .status
            .success()
    );
}

#[test]
fn scan_continues_past_a_missing_folder() {
    let home = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let gone = parent.path().join("a-gone");
    let here = parent.path().join("b-here");
    fs::create_dir(&gone).unwrap();
    fs::create_dir(&here).unwrap();
    fs::write(here.join("notes.txt"), "hello").unwrap();
    ok(home.path(), &["add", gone.to_str().unwrap()]);
    ok(home.path(), &["add", here.to_str().unwrap()]);
    fs::remove_dir(&gone).unwrap();

    // Folders are scanned in path order, so the missing one comes first.
    let out = nebula(home.path(), &["scan"]);
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("notes.txt"), "{stdout}");
    assert!(stderr.contains("a-gone"), "{stderr}");
    assert!(stderr.contains("1 folder could not be scanned"), "{stderr}");
    // The other folder can still be scanned on its own, by name.
    assert!(ok(home.path(), &["scan", "b-here"]).contains("No changes"));
}
