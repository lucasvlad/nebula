//! Portable relative paths inside a sync root. STUB: API only.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

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
    pub fn new(_s: &str) -> Result<Self, PathError> {
        todo!()
    }

    pub fn from_components<'a>(
        _components: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, PathError> {
        todo!()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn file_name(&self) -> &str {
        todo!()
    }

    pub fn parent(&self) -> Option<&str> {
        todo!()
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    pub fn case_key(&self) -> String {
        todo!()
    }
}

pub fn case_collisions<'a>(_paths: impl IntoIterator<Item = &'a RelPath>) -> BTreeSet<&'a RelPath> {
    todo!()
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
