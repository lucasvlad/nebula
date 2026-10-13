//! Version vectors. STUB: API only.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::DeviceId;

/// How one version vector relates to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Causality {
    Equal,
    /// `self` strictly dominates `other`.
    After,
    /// `other` strictly dominates `self`.
    Before,
    Concurrent,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionVector(BTreeMap<DeviceId, u64>);

impl VersionVector {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn get(&self, _device: &DeviceId) -> u64 {
        todo!()
    }
    pub fn increment(&mut self, _device: DeviceId) -> u64 {
        todo!()
    }
    pub fn merge(&mut self, _other: &Self) {
        todo!()
    }
    pub fn merged(&self, _other: &Self) -> Self {
        todo!()
    }
    pub fn compare(&self, _other: &Self) -> Causality {
        todo!()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&DeviceId, &u64)> {
        self.0.iter()
    }
}
