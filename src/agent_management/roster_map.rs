//! Persistent ordered map for the roster collections (058 S4).
//!
//! A `BTreeMap`-compatible facade over `imbl::OrdMap`: cloning a snapshot is
//! O(1) per collection, an edit copies only its O(log n) path, and
//! [`RosterMap::diff`] skips subtrees the two versions still share. A
//! one-row `with_state` therefore neither copies nor compares unrelated
//! history. Serialises exactly like a `BTreeMap` (key order, map shape).

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub type Iter<'a, K, V> = imbl::ordmap::Iter<'a, K, V, imbl::shared_ptr::DefaultSharedPtr>;
/// Cloneable (unlike imbl's own `Keys`/`Values`), as `BTreeMap`'s are.
pub type Keys<'a, K, V> = std::iter::Map<Iter<'a, K, V>, for<'b> fn((&'b K, &'b V)) -> &'b K>;
pub type Values<'a, K, V> = std::iter::Map<Iter<'a, K, V>, for<'b> fn((&'b K, &'b V)) -> &'b V>;

#[derive(Clone)]
pub struct RosterMap<K, V>(imbl::OrdMap<K, V>);

/// One differing entry between two versions of a collection.
pub enum Delta<'a, K, V> {
    Put(&'a K, &'a V),
    Delete(&'a K),
}

impl<K: Ord + Clone, V: Clone> RosterMap<K, V> {
    pub fn new() -> Self {
        Self(imbl::OrdMap::new())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.get(key)
    }

    pub fn get_key_value<Q>(&self, key: &Q) -> Option<(&K, &V)>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.get_key_value(key)
    }

    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.contains_key(key)
    }

    /// Copies only the path to `key` (and `V` if it is shared).
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.get_mut(key)
    }

    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.0.insert(key, value)
    }

    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.remove(key)
    }

    pub fn iter(&self) -> Iter<'_, K, V> {
        self.0.iter()
    }

    pub fn keys(&self) -> Keys<'_, K, V> {
        self.0.iter().map(|(k, _)| k)
    }

    pub fn values(&self) -> Values<'_, K, V> {
        self.0.iter().map(|(_, v)| v)
    }

    pub fn range<R, Q>(&self, range: R) -> imbl::ordmap::RangedIter<'_, K, V, imbl::shared_ptr::DefaultSharedPtr>
    where
        R: std::ops::RangeBounds<Q>,
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.range(range)
    }

    /// `BTreeMap::entry`; `or_default` / `or_insert_with` / `and_modify`
    /// copy only the path to `key`.
    pub fn entry(&mut self, key: K) -> imbl::ordmap::Entry<'_, K, V, imbl::shared_ptr::DefaultSharedPtr> {
        self.0.entry(key)
    }

    pub fn first_key_value(&self) -> Option<(&K, &V)> {
        self.0.get_min().map(|(k, v)| (k, v))
    }

    pub fn last_key_value(&self) -> Option<(&K, &V)> {
        self.0.get_max().map(|(k, v)| (k, v))
    }

    /// Visits every entry; only removed keys change the persistent map.
    pub fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        let gone: Vec<K> = self.0.iter().filter(|(k, v)| !keep(k, v)).map(|(k, _)| k.clone()).collect();
        for key in gone {
            self.0.remove(&key);
        }
    }

    /// Mutate every value (whole-collection work by definition; unchanged
    /// values stay shared only if `f` leaves them equal and untouched).
    pub fn for_each_value_mut(&mut self, mut f: impl FnMut(&K, &mut V)) {
        let keys: Vec<K> = self.0.keys().cloned().collect();
        for key in keys {
            if let Some(value) = self.0.get_mut(&key) {
                f(&key, value);
            }
        }
    }

    pub fn pop_first(&mut self) -> Option<(K, V)> {
        let key = self.0.get_min()?.0.clone();
        self.0.remove_with_key(&key)
    }

    pub fn clear(&mut self) {
        self.0 = imbl::OrdMap::new();
    }

    /// Whether both versions are the same persistent tree (no change at all).
    pub fn ptr_eq(&self, other: &Self) -> bool {
        self.0.ptr_eq(&other.0)
    }

    /// Entries that differ from `base`, in key order. Shared subtrees are
    /// skipped, so the work follows the edits, not the collection size.
    pub fn diff<'a>(&'a self, base: &'a Self) -> Vec<Delta<'a, K, V>>
    where
        V: PartialEq,
    {
        if self.ptr_eq(base) {
            return Vec::new();
        }
        base.0
            .diff(&self.0)
            .map(|item| match item {
                imbl::ordmap::DiffItem::Add(k, v) => Delta::Put(k, v),
                imbl::ordmap::DiffItem::Update { new: (k, v), .. } => Delta::Put(k, v),
                imbl::ordmap::DiffItem::Remove(k, _) => Delta::Delete(k),
            })
            .collect()
    }

    pub fn into_values(self) -> impl Iterator<Item = V> {
        self.0.into_iter().map(|(_, v)| v)
    }

    pub fn into_keys(self) -> impl Iterator<Item = K> {
        self.0.into_iter().map(|(k, _)| k)
    }

    pub fn to_btree(&self) -> BTreeMap<K, V> {
        self.0.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}

impl<K: Ord + Clone, V: Clone> Default for RosterMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Ord + Clone, V: Clone + PartialEq> PartialEq for RosterMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.ptr_eq(other) || self.0 == other.0
    }
}

impl<K: Ord + Clone, V: Clone + Eq> Eq for RosterMap<K, V> {}

impl<K: Ord + Clone + fmt::Debug, V: Clone + fmt::Debug> fmt::Debug for RosterMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.0.iter()).finish()
    }
}

impl<K: Ord + Clone, V: Clone> From<BTreeMap<K, V>> for RosterMap<K, V> {
    fn from(map: BTreeMap<K, V>) -> Self {
        Self(map.into_iter().collect())
    }
}

impl<K: Ord + Clone, V: Clone, const N: usize> From<[(K, V); N]> for RosterMap<K, V> {
    fn from(entries: [(K, V); N]) -> Self {
        Self(entries.into_iter().collect())
    }
}

impl<K: Ord + Clone, V: Clone> FromIterator<(K, V)> for RosterMap<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl<K: Ord + Clone, V: Clone> Extend<(K, V)> for RosterMap<K, V> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        for (k, v) in iter {
            self.0.insert(k, v);
        }
    }
}

impl<'a, K: Ord + Clone, V: Clone> IntoIterator for &'a RosterMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<K: Ord + Clone, V: Clone> IntoIterator for RosterMap<K, V> {
    type Item = (K, V);
    type IntoIter = imbl::ordmap::ConsumingIter<K, V, imbl::shared_ptr::DefaultSharedPtr>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<K, V, Q> std::ops::Index<&Q> for RosterMap<K, V>
where
    K: Ord + Clone + Borrow<Q>,
    V: Clone,
    Q: Ord + ?Sized,
{
    type Output = V;
    fn index(&self, key: &Q) -> &V {
        self.0.get(key).expect("key not in roster map")
    }
}

impl<K: Ord + Clone + Serialize, V: Clone + Serialize> Serialize for RosterMap<K, V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.0.iter())
    }
}

impl<'de, K, V> Deserialize<'de> for RosterMap<K, V>
where
    K: Ord + Clone + Deserialize<'de>,
    V: Clone + Deserialize<'de>,
{
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        BTreeMap::<K, V>::deserialize(deserializer).map(Self::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialises_like_a_btreemap_and_diffs_only_edits() {
        let base: RosterMap<String, u32> = (0..5000).map(|n| (format!("k{n:05}"), n)).collect();
        let tree: BTreeMap<String, u32> = base.to_btree();
        assert_eq!(serde_json::to_string(&base).unwrap(), serde_json::to_string(&tree).unwrap());
        let back: RosterMap<String, u32> = serde_json::from_str(&serde_json::to_string(&tree).unwrap()).unwrap();
        assert_eq!(back, base);
        let mut next = base.clone();
        assert!(next.diff(&base).is_empty());
        *next.get_mut("k00010").unwrap() += 1;
        next.remove("k04000");
        next.insert("new".into(), 1);
        let changes = next.diff(&base);
        assert_eq!(changes.len(), 3);
        assert!(base.get("k04000").is_some(), "the base version is untouched");
    }
}
