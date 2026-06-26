use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::ops::{Deref, DerefMut};

use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::sql::{SqlFormat, ToSql};
use crate::{SurrealValue, Value};

/// Represents an object with key-value pairs in SurrealDB
///
/// Keys are stored canonically sorted in a `BTreeMap<String, Value>`. The
/// optional second field is a **presentation order** side-car (a permutation of
/// the sorted entries) used only for output — it is excluded from equality,
/// hashing, and the serialized data, so two objects that differ only in display
/// order remain equal and serialise to the same shape. `None` = sorted output.
#[derive(Clone, Debug, Default)]
pub struct Object(pub(crate) BTreeMap<String, Value>, pub(crate) Option<Box<[u32]>>);

impl Object {
	/// Create a new empty object
	pub fn new() -> Self {
		Object(BTreeMap::new(), None)
	}

	/// Insert a key-value pair into the object
	pub fn insert(&mut self, key: impl Into<String>, value: impl SurrealValue) -> Option<Value> {
		self.0.insert(key.into(), value.into_value())
	}

	/// Convert into the inner BTreeMap<String, Value>
	pub fn into_inner(self) -> BTreeMap<String, Value> {
		self.0
	}

	/// Set the presentation order (a permutation of indices into the sorted
	/// entries). Affects only output formatting, never comparison or hashing.
	pub fn set_display_order(&mut self, order: Option<Box<[u32]>>) {
		self.1 = order;
	}

	/// Consume the object, returning entries in presentation order when set,
	/// otherwise sorted.
	pub fn into_iter_display(self) -> Vec<(String, Value)> {
		let entries: Vec<(String, Value)> = self.0.into_iter().collect();
		match self.1 {
			Some(order) => {
				let mut slots: Vec<Option<(String, Value)>> =
					entries.into_iter().map(Some).collect();
				order
					.iter()
					.filter_map(|&i| slots.get_mut(i as usize).and_then(Option::take))
					.collect()
			}
			None => entries,
		}
	}

	/// Iterate entries in presentation order when set, otherwise sorted.
	pub fn iter_display(&self) -> Vec<(&String, &Value)> {
		let entries: Vec<(&String, &Value)> = self.0.iter().collect();
		match &self.1 {
			Some(order) => {
				order.iter().filter_map(|&i| entries.get(i as usize).copied()).collect()
			}
			None => entries,
		}
	}
}

// Comparison / hashing / serialized data delegate to the sorted map, ignoring
// the presentation-order side-car.
impl PartialEq for Object {
	fn eq(&self, other: &Self) -> bool {
		self.0 == other.0
	}
}
impl Eq for Object {}
impl PartialOrd for Object {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		Some(self.cmp(other))
	}
}
impl Ord for Object {
	fn cmp(&self, other: &Self) -> Ordering {
		self.0.cmp(&other.0)
	}
}
impl Hash for Object {
	fn hash<H: Hasher>(&self, state: &mut H) {
		self.0.hash(state);
	}
}

impl Serialize for Object {
	/// Serialise as a map. When a display order is set, entries are emitted in
	/// that order, so JSON/CBOR clients receive keys in written order.
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		let entries = self.iter_display();
		let mut map = serializer.serialize_map(Some(entries.len()))?;
		for (k, v) in entries {
			map.serialize_entry(k, v)?;
		}
		map.end()
	}
}

impl<'de> Deserialize<'de> for Object {
	fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		Ok(Object(BTreeMap::deserialize(deserializer)?, None))
	}
}

#[cfg(feature = "arbitrary")]
impl<'a> arbitrary::Arbitrary<'a> for Object {
	fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
		Ok(Object(BTreeMap::arbitrary(u)?, None))
	}
}

impl Deref for Object {
	type Target = BTreeMap<String, Value>;

	fn deref(&self) -> &Self::Target {
		&self.0
	}
}

impl DerefMut for Object {
	fn deref_mut(&mut self) -> &mut Self::Target {
		&mut self.0
	}
}

impl ToSql for Object {
	fn fmt_sql(&self, f: &mut String, fmt: SqlFormat) {
		use crate::sql::fmt_sql_key_value;

		if self.is_empty() {
			return f.push_str("{  }");
		}

		if fmt.is_pretty() {
			f.push('{');
		} else {
			f.push_str("{ ");
		}

		if !self.is_empty() {
			let inner_fmt = fmt.increment();
			fmt_sql_key_value(self.iter_display().into_iter(), f, inner_fmt);
		}

		if fmt.is_pretty() {
			f.push('}');
		} else {
			f.push_str(" }");
		}
	}
}

impl<T: SurrealValue> From<BTreeMap<&str, T>> for Object {
	fn from(v: BTreeMap<&str, T>) -> Self {
		Self(v.into_iter().map(|(key, val)| (key.to_string(), val.into_value())).collect(), None)
	}
}

impl<T: SurrealValue> From<BTreeMap<String, T>> for Object {
	fn from(v: BTreeMap<String, T>) -> Self {
		Self(v.into_iter().map(|(key, val)| (key, val.into_value())).collect(), None)
	}
}

impl<T: SurrealValue> FromIterator<(String, T)> for Object {
	fn from_iter<I: IntoIterator<Item = (String, T)>>(iter: I) -> Self {
		Self(BTreeMap::from_iter(iter.into_iter().map(|(k, v)| (k, v.into_value()))), None)
	}
}

impl<T: SurrealValue> From<HashMap<&str, T>> for Object {
	fn from(v: HashMap<&str, T>) -> Self {
		Self(v.into_iter().map(|(key, val)| (key.to_string(), val.into_value())).collect(), None)
	}
}

impl<T: SurrealValue> From<HashMap<String, T>> for Object {
	fn from(v: HashMap<String, T>) -> Self {
		Self(v.into_iter().map(|(key, val)| (key, val.into_value())).collect(), None)
	}
}

impl IntoIterator for Object {
	type Item = (String, Value);
	type IntoIter = std::collections::btree_map::IntoIter<String, Value>;
	fn into_iter(self) -> Self::IntoIter {
		self.0.into_iter()
	}
}

impl<'a> IntoIterator for &'a Object {
	type Item = (&'a String, &'a Value);
	type IntoIter = std::collections::btree_map::Iter<'a, String, Value>;
	fn into_iter(self) -> Self::IntoIter {
		self.0.iter()
	}
}

impl<'a> IntoIterator for &'a mut Object {
	type Item = (&'a String, &'a mut Value);
	type IntoIter = std::collections::btree_map::IterMut<'a, String, Value>;
	fn into_iter(self) -> Self::IntoIter {
		self.0.iter_mut()
	}
}
