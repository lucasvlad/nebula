//! Deciding what happens when a peer's version of a file differs from ours.
//!
//! Both peers run [`resolve`] independently on the same pair of entries, so
//! the outcome must be symmetric: whichever side wins on one device must win
//! on the other, and conflict copy names must match. After any outcome the
//! resulting version vector is `local.version.merged(&remote.version)`.

use crate::manifest::FileEntry;
use crate::path::{PathError, RelPath};
use crate::version::Causality;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Local,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Local already has or supersedes the remote version. Nothing to apply.
    KeepLocal,
    /// The remote version supersedes ours. Apply it.
    TakeRemote,
    /// Both sides changed the file concurrently and the contents differ.
    /// The winner keeps the original path; the loser is saved as a
    /// conflict copy (see [`conflict_copy_name`]).
    Conflict { winner: Side },
}

/// Version vectors decide whether there is a conflict at all. Only when the
/// edits are concurrent do mtimes matter, and then only to pick a winner.
pub fn resolve(local: &FileEntry, remote: &FileEntry) -> Resolution {
    match local.version.compare(&remote.version) {
        Causality::Equal | Causality::After => Resolution::KeepLocal,
        Causality::Before => Resolution::TakeRemote,
        Causality::Concurrent => match (local.deleted, remote.deleted) {
            (true, true) => Resolution::KeepLocal,
            // A concurrent edit beats a delete, so no data is lost.
            (true, false) => Resolution::TakeRemote,
            (false, true) => Resolution::KeepLocal,
            (false, false) if local.content_hash == remote.content_hash => Resolution::KeepLocal,
            (false, false) => {
                let winner = if tiebreak_key(remote) > tiebreak_key(local) {
                    Side::Remote
                } else {
                    Side::Local
                };
                Resolution::Conflict { winner }
            }
        },
    }
}

/// Newest mtime wins, then the device ID, then the content hash, so that the
/// order is total and both peers always agree on the winner.
fn tiebreak_key(e: &FileEntry) -> (i64, crate::DeviceId, crate::Digest) {
    (e.mtime_ns, e.modified_by, e.content_hash)
}

/// The path the losing version is saved under:
/// `dir/name (conflict copy <device> <YYYY-MM-DD HHMMSS>).ext`.
///
/// `device_label` names the device that made the losing edit and
/// `mtime_ns` is the losing version's mtime (shown in UTC). Both peers must
/// pass the same label so they produce the same name.
pub fn conflict_copy_name(
    path: &RelPath,
    device_label: &str,
    mtime_ns: i64,
) -> Result<RelPath, PathError> {
    let name = path.file_name();
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    };
    let copy = format!(
        "{stem} (conflict copy {} {}){ext}",
        sanitize_label(device_label),
        format_utc(mtime_ns)
    );
    match path.parent() {
        Some(parent) => RelPath::new(&format!("{parent}/{copy}")),
        None => RelPath::new(&copy),
    }
}

/// Keeps the label safe to put in a filename on every platform.
fn sanitize_label(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' {
                c
            } else {
                '-'
            }
        })
        .take(32)
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned.to_string()
    }
}

/// `YYYY-MM-DD HHMMSS` in UTC. Colons are not allowed in Windows filenames.
fn format_utc(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}{:02}{:02}",
        t / 3600,
        t % 3600 / 60,
        t % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::VersionVector;
    use crate::{DeviceId, Digest};
    use proptest::prelude::*;

    const A: DeviceId = DeviceId([0xaa; 32]);
    const B: DeviceId = DeviceId([0xbb; 32]);

    fn entry(
        content: &[u8],
        mtime_ns: i64,
        by: DeviceId,
        version: &[(DeviceId, u64)],
    ) -> FileEntry {
        let mut v = VersionVector::new();
        for &(d, n) in version {
            for _ in 0..n {
                v.increment(d);
            }
        }
        FileEntry {
            path: RelPath::new("docs/report.txt").unwrap(),
            size: content.len() as u64,
            mtime_ns,
            content_hash: Digest::of(content),
            chunks: Vec::new(),
            version: v,
            modified_by: by,
            deleted: false,
        }
    }

    fn deleted(mut e: FileEntry) -> FileEntry {
        e.deleted = true;
        e.content_hash = Digest::of(&[]);
        e
    }

    #[test]
    fn dominance_fast_forwards() {
        let old = entry(b"v1", 10, A, &[(A, 1)]);
        let new = entry(b"v2", 5, B, &[(A, 1), (B, 1)]);
        // An older mtime does not matter: the vector says `new` saw `old`.
        assert_eq!(resolve(&old, &new), Resolution::TakeRemote);
        assert_eq!(resolve(&new, &old), Resolution::KeepLocal);
        assert_eq!(resolve(&old, &old), Resolution::KeepLocal);
    }

    #[test]
    fn concurrent_edits_conflict_and_newest_wins() {
        let a = entry(b"from a", 100, A, &[(A, 2)]);
        let b = entry(b"from b", 200, B, &[(A, 1), (B, 1)]);
        assert_eq!(
            resolve(&a, &b),
            Resolution::Conflict {
                winner: Side::Remote
            }
        );
        assert_eq!(
            resolve(&b, &a),
            Resolution::Conflict {
                winner: Side::Local
            }
        );
    }

    #[test]
    fn equal_mtime_breaks_tie_by_device() {
        let a = entry(b"from a", 100, A, &[(A, 1)]);
        let b = entry(b"from b", 100, B, &[(B, 1)]);
        assert_eq!(
            resolve(&a, &b),
            Resolution::Conflict {
                winner: Side::Remote
            }
        );
        assert_eq!(
            resolve(&b, &a),
            Resolution::Conflict {
                winner: Side::Local
            }
        );
    }

    #[test]
    fn concurrent_identical_content_is_not_a_conflict() {
        let a = entry(b"same", 100, A, &[(A, 1)]);
        let b = entry(b"same", 200, B, &[(B, 1)]);
        assert_eq!(resolve(&a, &b), Resolution::KeepLocal);
        assert_eq!(resolve(&b, &a), Resolution::KeepLocal);
    }

    #[test]
    fn concurrent_edit_beats_delete() {
        let edit = entry(b"edited", 100, A, &[(A, 2)]);
        let gone = deleted(entry(b"", 900, B, &[(A, 1), (B, 1)]));
        assert_eq!(resolve(&edit, &gone), Resolution::KeepLocal);
        assert_eq!(resolve(&gone, &edit), Resolution::TakeRemote);
        let gone_too = deleted(entry(b"", 50, A, &[(A, 2)]));
        assert_eq!(resolve(&gone, &gone_too), Resolution::KeepLocal);
    }

    #[test]
    fn copy_name_keeps_extension_and_directory() {
        let p = RelPath::new("docs/report.final.docx").unwrap();
        // 2026-10-13 14:15:02 UTC
        let ns = 1_791_900_902 * 1_000_000_000;
        assert_eq!(
            conflict_copy_name(&p, "Lucas's MacBook", ns)
                .unwrap()
                .as_str(),
            "docs/report.final (conflict copy Lucas-s MacBook 2026-10-13 141502).docx"
        );
    }

    #[test]
    fn copy_name_without_extension_or_dotfile() {
        let ns = 0;
        let p = RelPath::new("Makefile").unwrap();
        assert_eq!(
            conflict_copy_name(&p, "pc", ns).unwrap().as_str(),
            "Makefile (conflict copy pc 1970-01-01 000000)"
        );
        let p = RelPath::new(".bashrc").unwrap();
        assert_eq!(
            conflict_copy_name(&p, "", ns).unwrap().as_str(),
            ".bashrc (conflict copy unknown 1970-01-01 000000)"
        );
    }

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01 000000");
        assert_eq!(format_utc(951_782_400 * 1_000_000_000), "2000-02-29 000000");
        assert_eq!(format_utc(-1_000_000_000), "1969-12-31 235959");
    }

    /// What ends up at the original path from one side's point of view.
    fn outcome(local: &FileEntry, remote: &FileEntry) -> Option<Digest> {
        let kept = match resolve(local, remote) {
            Resolution::KeepLocal => local,
            Resolution::TakeRemote => remote,
            Resolution::Conflict {
                winner: Side::Local,
            } => local,
            Resolution::Conflict {
                winner: Side::Remote,
            } => remote,
        };
        (!kept.deleted).then_some(kept.content_hash)
    }

    fn arb_entry() -> impl Strategy<Value = FileEntry> {
        (
            0u8..3,
            0i64..3,
            any::<bool>(),
            any::<bool>(),
            0u64..3,
            0u64..3,
        )
            .prop_map(|(content, mtime, by_a, del, na, nb)| {
                let by = if by_a { A } else { B };
                let e = entry(&[content], mtime, by, &[(A, na), (B, nb)]);
                if del { deleted(e) } else { e }
            })
    }

    proptest! {
        /// Both peers must end up with the same content at the path.
        #[test]
        fn both_sides_agree(l in arb_entry(), r in arb_entry()) {
            // Equal vectors with different contents cannot happen honestly.
            prop_assume!(l.version.compare(&r.version) != Causality::Equal);
            prop_assert_eq!(outcome(&l, &r), outcome(&r, &l));
        }

        #[test]
        fn copy_name_is_always_a_valid_sibling(label in ".{0,40}", ns in any::<i64>()) {
            let p = RelPath::new("a/b.txt").unwrap();
            let copy = conflict_copy_name(&p, &label, ns).unwrap();
            prop_assert_eq!(copy.parent(), Some("a"));
            prop_assert!(copy.as_str().ends_with(").txt"));
        }
    }
}
