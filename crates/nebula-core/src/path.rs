//! Portable relative paths inside a sync root.
//!
//! Peer input is hostile, so [`RelPath::new`] is strict: the result is NFC,
//! `/`-separated and legal on macOS, Linux and Windows.

use std::collections::{BTreeSet, HashMap};
use std::fmt;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

/// Longest path we accept, in bytes.
pub const MAX_PATH_BYTES: usize = 4096;
/// Longest single component we accept, in bytes.
pub const MAX_COMPONENT_BYTES: usize = 255;

/// A validated, NFC-normalized, `/`-separated path relative to a sync root
/// that is legal on macOS, Linux and Windows.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RelPath(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("path is empty")]
    Empty,
    #[error("path is absolute")]
    Absolute,
    #[error("path component {0:?} is not allowed")]
    BadComponent(String),
    #[error("character {0:?} is not allowed on every platform")]
    BadChar(char),
    #[error("{0:?} is a reserved name on Windows")]
    Reserved(String),
    #[error("path is too long")]
    TooLong,
}

impl RelPath {
    pub fn new(s: &str) -> Result<Self, PathError> {
        if s.is_empty() {
            return Err(PathError::Empty);
        }
        let bytes = s.as_bytes();
        let drive_prefix = bytes[0].is_ascii_alphabetic() && bytes.get(1) == Some(&b':');
        if s.starts_with(['/', '\\']) || drive_prefix {
            return Err(PathError::Absolute);
        }
        let normalized: String = s.nfc().collect();
        if normalized.len() > MAX_PATH_BYTES {
            return Err(PathError::TooLong);
        }
        for component in normalized.split('/') {
            validate_component(component)?;
        }
        Ok(Self(normalized))
    }

    /// Joins the components with `/` and validates the result. A component
    /// containing `/` is split and each piece validated.
    pub fn from_components<'a>(
        components: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, PathError> {
        let joined = components.into_iter().collect::<Vec<_>>().join("/");
        Self::new(&joined)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// Everything before the last `/`, or `None` for a top-level path.
    pub fn parent(&self) -> Option<&str> {
        self.0.rsplit_once('/').map(|(parent, _)| parent)
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// Equal for names that would collide on a case-insensitive filesystem.
    pub fn case_key(&self) -> String {
        self.0.to_lowercase()
    }
}

fn validate_component(c: &str) -> Result<(), PathError> {
    if c.is_empty() || c == "." || c == ".." {
        return Err(PathError::BadComponent(c.to_owned()));
    }
    if c.len() > MAX_COMPONENT_BYTES {
        return Err(PathError::TooLong);
    }
    let forbidden = |ch: char| ch < ' ' || ch == '\u{7f}' || "<>:\"\\|?*".contains(ch);
    if let Some(bad) = c.chars().find(|&ch| forbidden(ch)) {
        return Err(PathError::BadChar(bad));
    }
    if c.ends_with(['.', ' ']) {
        return Err(PathError::BadComponent(c.to_owned()));
    }
    if is_reserved(c) {
        return Err(PathError::Reserved(c.to_owned()));
    }
    Ok(())
}

/// Windows device names, ignoring case, any extension and trailing spaces.
fn is_reserved(component: &str) -> bool {
    let stem = component
        .split('.')
        .next()
        .unwrap_or(component)
        .trim_end_matches(' ');
    let upper = stem.to_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let Some(suffix) = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
    else {
        return false;
    };
    matches!(
        suffix,
        "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "\u{b9}" | "\u{b2}" | "\u{b3}"
    )
}

/// Every input path that would collide with a different input path on a
/// case-insensitive filesystem, either as a whole or through a shared
/// directory prefix spelled differently.
pub fn case_collisions<'a>(paths: impl IntoIterator<Item = &'a RelPath>) -> BTreeSet<&'a RelPath> {
    let paths: Vec<&RelPath> = paths.into_iter().collect();
    let mut spellings: HashMap<String, BTreeSet<&str>> = HashMap::new();
    for path in &paths {
        for prefix in prefixes(&path.0) {
            spellings
                .entry(prefix.to_lowercase())
                .or_default()
                .insert(prefix);
        }
    }
    paths
        .into_iter()
        .filter(|path| {
            prefixes(&path.0).any(|prefix| {
                spellings
                    .get(&prefix.to_lowercase())
                    .is_some_and(|set| set.len() > 1)
            })
        })
        .collect()
}

/// Each directory prefix of `path`, then the full path.
fn prefixes(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/')
        .map(|(i, _)| &path[..i])
        .chain(std::iter::once(path))
}

impl TryFrom<String> for RelPath {
    type Error = PathError;
    fn try_from(s: String) -> Result<Self, PathError> {
        Self::new(&s)
    }
}

impl From<RelPath> for String {
    fn from(p: RelPath) -> String {
        p.0
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn p(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    #[test]
    fn accepts_valid_paths() {
        for s in [
            "a",
            "a.txt",
            "dir/sub/file.tar.gz",
            "\u{e9}t\u{e9}/\u{65e5}\u{672c}\u{8a9e}.txt",
            ".hidden",
            "a b/c d",
            "console.txt",
            "CONX",
            "com10",
            "lpt",
            "...a",
        ] {
            let path = p(s);
            assert_eq!(path.as_str(), s);
            assert_eq!(path.to_string(), s);
        }
    }

    #[test]
    fn nfd_is_normalized_to_nfc() {
        let nfd = p("e\u{301}.txt");
        assert_eq!(nfd.as_str(), "\u{e9}.txt");
        assert_eq!(nfd, p("\u{e9}.txt"));
    }

    #[test]
    fn rejects_empty_and_absolute() {
        assert_eq!(RelPath::new(""), Err(PathError::Empty));
        for s in ["/x", "\\x", "C:foo", "c:\\foo", "z:"] {
            assert_eq!(RelPath::new(s), Err(PathError::Absolute), "{s}");
        }
    }

    #[test]
    fn rejects_bad_components() {
        for s in [
            "a/../b", "./a", "a/.", "..", "a//b", "a/", "foo.", "foo ", "a/foo./b",
        ] {
            assert!(
                matches!(RelPath::new(s), Err(PathError::BadComponent(_))),
                "{s}"
            );
        }
        assert_eq!(
            RelPath::new("a//b"),
            Err(PathError::BadComponent(String::new()))
        );
    }

    #[test]
    fn rejects_bad_chars() {
        for (s, c) in [
            ("a\\b", '\\'),
            ("x/a:b", ':'),
            ("x?", '?'),
            ("a/b*c", '*'),
            ("<a", '<'),
            ("a>", '>'),
            ("a\"b", '"'),
            ("a|b", '|'),
            ("a\nb", '\n'),
            ("a\0b", '\0'),
            ("a\u{7f}", '\u{7f}'),
        ] {
            assert_eq!(RelPath::new(s), Err(PathError::BadChar(c)), "{s:?}");
        }
    }

    #[test]
    fn rejects_reserved_names() {
        for s in [
            "con",
            "CON",
            "CON.txt",
            "Aux.tar.gz",
            "lpt1",
            "dir/NUL",
            "prn.x",
            "com0",
            "COM9.txt",
            "com\u{b9}",
            "LPT\u{b2}.txt",
            "COM\u{b3}",
            "con .txt",
        ] {
            assert!(
                matches!(RelPath::new(s), Err(PathError::Reserved(_))),
                "{s}"
            );
        }
        assert_eq!(
            RelPath::new("a/CON.txt"),
            Err(PathError::Reserved("CON.txt".to_owned()))
        );
    }

    #[test]
    fn length_limits() {
        let ok = "a".repeat(MAX_COMPONENT_BYTES);
        assert!(RelPath::new(&ok).is_ok());
        let long = "a".repeat(MAX_COMPONENT_BYTES + 1);
        assert_eq!(RelPath::new(&long), Err(PathError::TooLong));

        let seg = "a".repeat(100);
        let many = vec![seg.as_str(); 41].join("/");
        assert!(many.len() > MAX_PATH_BYTES);
        assert_eq!(RelPath::new(&many), Err(PathError::TooLong));
        let fits = vec![seg.as_str(); 40].join("/");
        assert!(RelPath::new(&fits).is_ok());
    }

    #[test]
    fn component_length_counts_bytes() {
        assert_eq!(RelPath::new(&"\u{e9}".repeat(128)), Err(PathError::TooLong));
        assert!(RelPath::new(&"\u{e9}".repeat(127)).is_ok());
    }

    #[test]
    fn from_components_validates() {
        assert_eq!(
            p("a/b/c"),
            RelPath::from_components(["a", "b", "c"]).unwrap()
        );
        assert_eq!(
            RelPath::from_components(Vec::<&str>::new()),
            Err(PathError::Empty)
        );
        assert!(RelPath::from_components(["a", ".."]).is_err());
        assert!(RelPath::from_components(["a", ""]).is_err());
        assert_eq!(p("a/b"), RelPath::from_components(["a/b"]).unwrap());
    }

    #[test]
    fn file_name_and_parent() {
        let nested = p("a/b/c.txt");
        assert_eq!(nested.file_name(), "c.txt");
        assert_eq!(nested.parent(), Some("a/b"));
        assert_eq!(nested.components().collect::<Vec<_>>(), ["a", "b", "c.txt"]);
        let top = p("c.txt");
        assert_eq!(top.file_name(), "c.txt");
        assert_eq!(top.parent(), None);
    }

    #[test]
    fn case_key_lowercases() {
        assert_eq!(p("Docs/A.TXT").case_key(), "docs/a.txt");
    }

    fn collisions(paths: &[&str]) -> Vec<String> {
        let parsed: Vec<RelPath> = paths.iter().map(|s| p(s)).collect();
        case_collisions(&parsed)
            .into_iter()
            .map(|r| r.as_str().to_owned())
            .collect()
    }

    #[test]
    fn collisions_on_file_name() {
        assert_eq!(collisions(&["a.txt", "A.txt"]), ["A.txt", "a.txt"]);
    }

    #[test]
    fn collisions_on_directory() {
        assert_eq!(
            collisions(&["Docs/a.txt", "docs/b.txt"]),
            ["Docs/a.txt", "docs/b.txt"]
        );
    }

    #[test]
    fn no_collisions() {
        assert!(collisions(&["docs/a.txt", "docs/b.txt"]).is_empty());
        assert!(collisions(&["a.txt", "a.txt"]).is_empty());
        assert!(collisions(&[]).is_empty());
    }

    #[test]
    fn collisions_only_report_involved_paths() {
        assert_eq!(
            collisions(&["x/y", "Docs/a", "docs/a", "docs/a/deep", "other"]),
            ["Docs/a", "docs/a", "docs/a/deep"]
        );
    }

    #[test]
    fn invalid_string_fails_deserialization_conversion() {
        // serde goes through `TryFrom<String>`.
        assert_eq!(
            RelPath::try_from("../x".to_owned()),
            Err(PathError::BadComponent("..".to_owned()))
        );
        assert_eq!(RelPath::try_from(String::new()), Err(PathError::Empty));
        let ok = RelPath::try_from("a/b".to_owned()).unwrap();
        assert_eq!(String::from(ok), "a/b");
    }

    proptest! {
        #[test]
        fn accepted_paths_are_idempotent_and_safe(
            s in "[a-zA-Z0-9./\\\\: ?\u{301}\u{e9}]{0,24}"
        ) {
            if let Ok(path) = RelPath::new(&s) {
                prop_assert_eq!(RelPath::new(path.as_str()), Ok(path.clone()));
                prop_assert!(!path.as_str().starts_with('/'));
                prop_assert!(!path.as_str().contains('\\'));
                prop_assert!(path.components().all(|c| c != ".." && c != "." && !c.is_empty()));
            }
        }

        #[test]
        fn arbitrary_strings_never_break_invariants(s in any::<String>()) {
            if let Ok(path) = RelPath::new(&s) {
                prop_assert_eq!(RelPath::new(path.as_str()), Ok(path.clone()));
                prop_assert!(!path.as_str().starts_with('/'));
                prop_assert!(!path.as_str().contains('\\'));
                prop_assert!(path.components().all(|c| c != ".."));
            }
        }

        #[test]
        fn collisions_match_naive_check(
            names in proptest::collection::vec("[a-bA-B]{1,2}(/[a-bA-B]{1,2}){0,2}", 0..6)
        ) {
            let parsed: Vec<RelPath> = names.iter().filter_map(|n| RelPath::new(n).ok()).collect();
            let found = case_collisions(&parsed);
            let prefix_of = |path: &RelPath, n: usize| path.components().take(n).collect::<Vec<_>>().join("/");
            for a in &parsed {
                let expected = parsed.iter().any(|b| {
                    (1..=a.components().count().min(b.components().count())).any(|n| {
                        let (pa, pb) = (prefix_of(a, n), prefix_of(b, n));
                        pa != pb && pa.to_lowercase() == pb.to_lowercase()
                    })
                });
                prop_assert_eq!(found.contains(a), expected);
            }
        }
    }
}
