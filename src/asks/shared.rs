//! Copy-on-write collections for [`super::store::AskState`] (r273 stab).
//!
//! The hot ask store is read and written as one value. Before r273 every
//! owned snapshot and every write draft deep-copied the whole state: on the
//! host-a store (791 hot asks, 5.7k actions, 8.5k artifacts) one copy was
//! ~44 MB / 35 ms on Linux and 250 ms-16 s on Windows, and up to 256
//! management connection threads could each hold one (11+ GB).
//!
//! `Shared<T>` keeps a collection behind an `Arc`. Cloning a state bumps
//! reference counts; the first mutable access (`DerefMut`) clones only that
//! collection, and only while another state still shares it. Reads are
//! unchanged (`Deref`), so call sites keep the `BTreeMap` API. Equality and
//! row diffs short-circuit on pointer identity: an untouched collection of a
//! write draft is the very same allocation as the committed state's.
//! Serialization is transparent (the persisted format is unchanged).
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::sync::Arc;

pub struct Shared<T>(Arc<T>);

impl<T> Shared<T> {
    pub fn new(value: T) -> Self {
        Self(Arc::new(value))
    }

    /// Both values are the same allocation (so certainly equal).
    pub fn ptr_eq(a: &Self, b: &Self) -> bool {
        Arc::ptr_eq(&a.0, &b.0)
    }
}

impl<T: Clone> Shared<T> {
    /// The owned collection; copies it only while it is still shared.
    pub fn into_inner(self) -> T {
        Arc::unwrap_or_clone(self.0)
    }
}

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T: Default> Default for Shared<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> std::ops::Deref for Shared<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: Clone> std::ops::DerefMut for Shared<T> {
    fn deref_mut(&mut self) -> &mut T {
        Arc::make_mut(&mut self.0)
    }
}

impl<T> From<T> for Shared<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T: PartialEq> PartialEq for Shared<T> {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || *self.0 == *other.0
    }
}

impl<T: Eq> Eq for Shared<T> {}

impl<T: PartialEq> PartialEq<T> for Shared<T> {
    fn eq(&self, other: &T) -> bool {
        *self.0 == *other
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Shared<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<T: Serialize> Serialize for Shared<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Shared<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self::new)
    }
}

impl<'a, T> IntoIterator for &'a Shared<T>
where
    &'a T: IntoIterator,
{
    type Item = <&'a T as IntoIterator>::Item;
    type IntoIter = <&'a T as IntoIterator>::IntoIter;
    fn into_iter(self) -> Self::IntoIter {
        (&*self.0).into_iter()
    }
}

impl<'a, T: Clone> IntoIterator for &'a mut Shared<T>
where
    &'a mut T: IntoIterator,
{
    type Item = <&'a mut T as IntoIterator>::Item;
    type IntoIter = <&'a mut T as IntoIterator>::IntoIter;
    fn into_iter(self) -> Self::IntoIter {
        Arc::make_mut(&mut self.0).into_iter()
    }
}

impl<T: Clone + IntoIterator> IntoIterator for Shared<T> {
    type Item = T::Item;
    type IntoIter = T::IntoIter;
    fn into_iter(self) -> Self::IntoIter {
        self.into_inner().into_iter()
    }
}

impl<A, T: FromIterator<A>> FromIterator<A> for Shared<T> {
    fn from_iter<I: IntoIterator<Item = A>>(iter: I) -> Self {
        Self::new(T::from_iter(iter))
    }
}

impl<A, T: Clone + Extend<A>> Extend<A> for Shared<T> {
    fn extend<I: IntoIterator<Item = A>>(&mut self, iter: I) {
        Arc::make_mut(&mut self.0).extend(iter)
    }
}

#[cfg(test)]
mod tests {
    use super::Shared;
    use std::collections::BTreeMap;

    #[test]
    fn clone_shares_until_first_mutation() {
        let mut a: Shared<BTreeMap<String, String>> = Shared::default();
        a.insert("k".into(), "v".into());
        let mut b = a.clone();
        assert!(Shared::ptr_eq(&a, &b));
        assert_eq!(a, b);
        // A read never detaches.
        assert_eq!(b.get("k").map(String::as_str), Some("v"));
        assert!(Shared::ptr_eq(&a, &b));
        b.insert("k2".into(), "v2".into());
        assert!(!Shared::ptr_eq(&a, &b));
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 2);
        assert_ne!(a, b);
    }

    #[test]
    fn serialization_is_transparent() {
        let mut a: Shared<BTreeMap<String, u32>> = Shared::default();
        a.insert("x".into(), 1);
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(json, r#"{"x":1}"#);
        let back: Shared<BTreeMap<String, u32>> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
        let plain: BTreeMap<String, u32> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, plain);
    }
}
