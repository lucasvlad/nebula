//! Version vectors (Parker et al., 1983).
//!
//! Each file carries a map from device to the number of edits that device has
//! made to it. A missing device counts as 0. Comparing two vectors pointwise
//! gives a partial order: if one vector dominates the other (greater or equal
//! everywhere, strictly greater somewhere), the dominating version has seen
//! the other and the peer can fast-forward to it. If neither dominates, the
//! two sides edited independently, which is a conflict. Wall-clock time plays
//! no part in detecting that.

use std::cmp::Ordering;
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

/// Equality is semantic: an explicit zero entry equals absence, so a peer
/// cannot make two identical histories differ by sending zeros. `merge` and
/// `increment` never insert zeros themselves.
#[derive(Debug, Clone, Default, Eq, Serialize, Deserialize)]
pub struct VersionVector(BTreeMap<DeviceId, u64>);

impl PartialEq for VersionVector {
    fn eq(&self, other: &Self) -> bool {
        self.compare(other) == Causality::Equal
    }
}

impl VersionVector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, device: &DeviceId) -> u64 {
        self.0.get(device).copied().unwrap_or(0)
    }

    /// Bumps `device`'s counter (saturating) and returns the new value.
    pub fn increment(&mut self, device: DeviceId) -> u64 {
        let c = self.0.entry(device).or_insert(0);
        *c = c.saturating_add(1);
        *c
    }

    /// Pointwise maximum.
    pub fn merge(&mut self, other: &Self) {
        for (device, &n) in &other.0 {
            if n > self.get(device) {
                self.0.insert(*device, n);
            }
        }
    }

    pub fn merged(&self, other: &Self) -> Self {
        let mut out = self.clone();
        out.merge(other);
        out
    }

    /// Walks both sorted maps once, so it is O(n + m).
    pub fn compare(&self, other: &Self) -> Causality {
        let (mut less, mut greater) = (false, false);
        let mut a = self.0.iter().peekable();
        let mut b = other.0.iter().peekable();
        loop {
            let (x, y) = match (a.peek(), b.peek()) {
                (None, None) => break,
                (Some(&(_, &x)), None) => {
                    a.next();
                    (x, 0)
                }
                (None, Some(&(_, &y))) => {
                    b.next();
                    (0, y)
                }
                (Some(&(da, &x)), Some(&(db, &y))) => match da.cmp(db) {
                    Ordering::Less => {
                        a.next();
                        (x, 0)
                    }
                    Ordering::Greater => {
                        b.next();
                        (0, y)
                    }
                    Ordering::Equal => {
                        a.next();
                        b.next();
                        (x, y)
                    }
                },
            };
            match x.cmp(&y) {
                Ordering::Less => less = true,
                Ordering::Greater => greater = true,
                Ordering::Equal => {}
            }
        }
        match (less, greater) {
            (false, false) => Causality::Equal,
            (false, true) => Causality::After,
            (true, false) => Causality::Before,
            (true, true) => Causality::Concurrent,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&DeviceId, &u64)> {
        self.0.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn dev(n: u8) -> DeviceId {
        DeviceId([n; 32])
    }

    fn from_entries(entries: &[(u8, u64)]) -> VersionVector {
        VersionVector(entries.iter().map(|&(d, n)| (dev(d), n)).collect())
    }

    #[test]
    fn empty_vectors_are_equal() {
        let (a, b) = (VersionVector::new(), VersionVector::new());
        assert_eq!(a.compare(&b), Causality::Equal);
        assert_eq!(a, b);
    }

    #[test]
    fn increment_returns_new_value_and_makes_after() {
        let old = VersionVector::new();
        let mut new = old.clone();
        assert_eq!(new.increment(dev(1)), 1);
        assert_eq!(new.increment(dev(1)), 2);
        assert_eq!(new.get(&dev(1)), 2);
        assert_eq!(new.get(&dev(2)), 0);
        assert_eq!(new.compare(&old), Causality::After);
        assert_eq!(old.compare(&new), Causality::Before);
    }

    #[test]
    fn increment_saturates() {
        let mut v = from_entries(&[(1, u64::MAX)]);
        assert_eq!(v.increment(dev(1)), u64::MAX);
    }

    #[test]
    fn independent_edits_are_concurrent() {
        let (mut a, mut b) = (VersionVector::new(), VersionVector::new());
        a.increment(dev(1));
        b.increment(dev(2));
        assert_eq!(a.compare(&b), Causality::Concurrent);
        assert_eq!(b.compare(&a), Causality::Concurrent);
    }

    #[test]
    fn merge_of_concurrent_dominates_both() {
        let (mut a, mut b) = (VersionVector::new(), VersionVector::new());
        a.increment(dev(1));
        b.increment(dev(2));
        let m = a.merged(&b);
        assert_eq!(m.compare(&a), Causality::After);
        assert_eq!(m.compare(&b), Causality::After);
    }

    #[test]
    fn explicit_zero_equals_absence() {
        let zeros = from_entries(&[(1, 0), (2, 0)]);
        let empty = VersionVector::new();
        assert_eq!(zeros.compare(&empty), Causality::Equal);
        assert_eq!(empty.compare(&zeros), Causality::Equal);
        assert_eq!(zeros, empty);
        let one = from_entries(&[(3, 1)]);
        assert_eq!(one.compare(&zeros), Causality::After);
        assert_eq!(zeros.compare(&one), Causality::Before);
    }

    #[test]
    fn merge_does_not_insert_zeros() {
        let mut a = VersionVector::new();
        a.merge(&from_entries(&[(1, 0)]));
        assert_eq!(a.iter().count(), 0);
    }

    fn vv() -> impl Strategy<Value = VersionVector> {
        proptest::collection::btree_map(0u8..4, 0u64..4, 0..=4)
            .prop_map(|m| VersionVector(m.into_iter().map(|(d, n)| (dev(d), n)).collect()))
    }

    /// Reference implementation: look every device up with `get`.
    fn naive(a: &VersionVector, b: &VersionVector) -> Causality {
        let le = (0..4).all(|d| a.get(&dev(d)) <= b.get(&dev(d)));
        let ge = (0..4).all(|d| a.get(&dev(d)) >= b.get(&dev(d)));
        match (le, ge) {
            (true, true) => Causality::Equal,
            (false, true) => Causality::After,
            (true, false) => Causality::Before,
            (false, false) => Causality::Concurrent,
        }
    }

    proptest! {
        #[test]
        fn matches_naive(a in vv(), b in vv()) {
            prop_assert_eq!(a.compare(&b), naive(&a, &b));
        }

        #[test]
        fn antisymmetric(a in vv(), b in vv()) {
            let flipped = match a.compare(&b) {
                Causality::After => Causality::Before,
                Causality::Before => Causality::After,
                other => other,
            };
            prop_assert_eq!(b.compare(&a), flipped);
        }

        #[test]
        fn reflexive(a in vv()) {
            prop_assert_eq!(a.compare(&a), Causality::Equal);
        }

        #[test]
        fn merge_commutative(a in vv(), b in vv()) {
            prop_assert_eq!(a.merged(&b), b.merged(&a));
        }

        #[test]
        fn merge_associative(a in vv(), b in vv(), c in vv()) {
            prop_assert_eq!(a.merged(&b).merged(&c), a.merged(&b.merged(&c)));
        }

        #[test]
        fn merge_idempotent(a in vv()) {
            prop_assert_eq!(a.merged(&a), a);
        }

        #[test]
        fn merge_dominates_inputs(a in vv(), b in vv()) {
            let m = a.merged(&b);
            prop_assert!(matches!(m.compare(&a), Causality::Equal | Causality::After));
            prop_assert!(matches!(m.compare(&b), Causality::Equal | Causality::After));
        }

        #[test]
        fn increment_is_after(a in vv(), d in 0u8..4) {
            let mut n = a.clone();
            n.increment(dev(d));
            prop_assert_eq!(n.compare(&a), Causality::After);
        }

        #[test]
        fn transitive(a in vv(), b in vv(), c in vv()) {
            if a.compare(&b) == Causality::After && b.compare(&c) == Causality::After {
                prop_assert_eq!(a.compare(&c), Causality::After);
            }
        }
    }
}
