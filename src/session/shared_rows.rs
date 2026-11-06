//! Structurally shared maps with individually shared business rows. Putting
//! large session/receipt values directly in HAMT/B-tree leaves inflates debug
//! stack frames and clones neighboring rows during path copies. Leaves here
//! contain only Arcs; mutation detaches exactly the requested business row.
use serde::{Deserialize, Serialize};
use std::sync::Arc;

fn borrowed<'a, V>((key, value): (&'a String, &'a Arc<V>)) -> (&'a String, &'a V) {
    (key, value.as_ref())
}
fn owned<V: Clone>((key, value): (String, Arc<V>)) -> (String, V) {
    (key, Arc::unwrap_or_clone(value))
}

macro_rules! shared_rows {
    ($name:ident,$map:ident) => {
        #[derive(Debug, PartialEq, Eq)]
        pub struct $name<V>(imbl::$map<String, Arc<V>>);
        impl<V> Clone for $name<V> {
            fn clone(&self) -> Self {
                Self(self.0.clone())
            }
        }
        impl<V> Default for $name<V> {
            fn default() -> Self {
                Self(imbl::$map::new())
            }
        }
        impl<V: Clone> $name<V> {
            pub fn new() -> Self {
                Self::default()
            }
            pub fn len(&self) -> usize {
                self.0.len()
            }
            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
            pub fn clear(&mut self) {
                self.0.clear()
            }
            pub fn contains_key(&self, key: &str) -> bool {
                self.0.contains_key(key)
            }
            pub fn get(&self, key: &str) -> Option<&V> {
                self.0.get(key).map(Arc::as_ref)
            }
            pub(crate) fn shared(&self, key: &str) -> Option<Arc<V>> {
                self.0.get(key).map(Arc::clone)
            }
            pub fn get_mut(&mut self, key: &str) -> Option<&mut V> {
                self.0.get_mut(key).map(Arc::make_mut)
            }
            pub fn insert(&mut self, key: String, value: V) -> Option<V> {
                self.0
                    .insert(key, Arc::new(value))
                    .map(Arc::unwrap_or_clone)
            }
            pub(crate) fn insert_shared(&mut self, key: String, value: Arc<V>) {
                self.0.insert(key, value);
            }
            pub fn remove(&mut self, key: &str) -> Option<V> {
                self.0.remove(key).map(Arc::unwrap_or_clone)
            }
            pub fn iter(&self) -> <&Self as IntoIterator>::IntoIter {
                self.into_iter()
            }
            pub fn keys<'a>(
                &'a self,
            ) -> std::iter::Map<
                <&'a Self as IntoIterator>::IntoIter,
                fn((&'a String, &'a V)) -> &'a String,
            > {
                self.into_iter().map(|(key, _)| key)
            }
            pub fn values<'a>(
                &'a self,
            ) -> std::iter::Map<
                <&'a Self as IntoIterator>::IntoIter,
                fn((&'a String, &'a V)) -> &'a V,
            > {
                self.into_iter().map(|(_, value)| value)
            }
            pub fn into_values(
                self,
            ) -> std::iter::Map<<Self as IntoIterator>::IntoIter, fn((String, V)) -> V> {
                self.into_iter().map(|(_, value)| value)
            }
            pub fn ptr_eq(&self, other: &Self) -> bool {
                self.0.ptr_eq(&other.0)
            }
            pub fn row_ptr_eq(&self, other: &Self, key: &str) -> bool {
                match (self.0.get(key), other.0.get(key)) {
                    (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                    _ => false,
                }
            }
        }
        impl<V: Clone> FromIterator<(String, V)> for $name<V> {
            fn from_iter<I: IntoIterator<Item = (String, V)>>(iter: I) -> Self {
                Self(iter.into_iter().map(|(k, v)| (k, Arc::new(v))).collect())
            }
        }
        impl<V: Clone> Extend<(String, V)> for $name<V> {
            fn extend<I: IntoIterator<Item = (String, V)>>(&mut self, iter: I) {
                for (k, v) in iter {
                    self.insert(k, v);
                }
            }
        }
        impl<V: Clone, const N: usize> From<[(String, V); N]> for $name<V> {
            fn from(values: [(String, V); N]) -> Self {
                values.into_iter().collect()
            }
        }
        impl<V: Clone> std::ops::Index<&str> for $name<V> {
            type Output = V;
            fn index(&self, key: &str) -> &V {
                self.get(key).expect("session row key")
            }
        }
        impl<V: Clone> std::ops::Index<&String> for $name<V> {
            type Output = V;
            fn index(&self, key: &String) -> &V {
                self.get(key).expect("session row key")
            }
        }
        impl<'a, V: Clone> IntoIterator for &'a $name<V> {
            type Item = (&'a String, &'a V);
            type IntoIter = std::iter::Map<
                <&'a imbl::$map<String, Arc<V>> as IntoIterator>::IntoIter,
                fn((&'a String, &'a Arc<V>)) -> Self::Item,
            >;
            fn into_iter(self) -> Self::IntoIter {
                self.0.iter().map(borrowed::<V>)
            }
        }
        impl<V: Clone> IntoIterator for $name<V> {
            type Item = (String, V);
            type IntoIter = std::iter::Map<
                <imbl::$map<String, Arc<V>> as IntoIterator>::IntoIter,
                fn((String, Arc<V>)) -> Self::Item,
            >;
            fn into_iter(self) -> Self::IntoIter {
                self.0.into_iter().map(owned::<V>)
            }
        }
        impl<V: Serialize + Clone> Serialize for $name<V> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_map(self.iter())
            }
        }
        impl<'de, V: Deserialize<'de> + Clone> Deserialize<'de> for $name<V> {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct Visitor<V>(std::marker::PhantomData<V>);
                impl<'de, V: Deserialize<'de> + Clone> serde::de::Visitor<'de> for Visitor<V> {
                    type Value = $name<V>;
                    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                        f.write_str("session row map")
                    }
                    fn visit_map<A: serde::de::MapAccess<'de>>(
                        self,
                        mut access: A,
                    ) -> Result<Self::Value, A::Error> {
                        let mut map = $name::default();
                        while let Some((key, value)) = access.next_entry::<String, V>()? {
                            map.insert(key, value);
                        }
                        Ok(map)
                    }
                }
                deserializer.deserialize_map(Visitor(std::marker::PhantomData))
            }
        }
    };
}
shared_rows!(SharedHashRows, HashMap);
shared_rows!(SharedOrdRows, OrdMap);

impl<V: Clone> SharedHashRows<V> {
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&String, &mut V)> {
        self.0
            .iter_mut()
            .map(|(key, value)| (key, Arc::make_mut(value)))
    }
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.iter_mut().map(|(_, value)| value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn updating_one_row_keeps_all_other_rows_shared_and_old_readers_unchanged() {
        let original: SharedHashRows<_> = (0..1000).map(|i| (i.to_string(), vec![i; 10])).collect();
        let mut current = original.clone();
        for n in 0..100 {
            current.get_mut("0").unwrap()[0] = n;
            assert!(original.row_ptr_eq(&current, "500"));
        }
        assert_eq!(original.get("0").unwrap()[0], 0);
        assert_eq!(current.get("0").unwrap()[0], 99);
        let ordered: SharedOrdRows<_> = [("b".into(), 2), ("a".into(), 1)].into();
        assert_eq!(serde_json::to_string(&ordered).unwrap(), r#"{"a":1,"b":2}"#);
        assert_eq!(
            serde_json::from_str::<SharedOrdRows<i32>>(r#"{"a":1,"b":2}"#).unwrap(),
            ordered
        );
    }
}
