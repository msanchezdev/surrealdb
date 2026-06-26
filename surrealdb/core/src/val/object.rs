use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::ops::{Deref, DerefMut};

use anyhow::Result;
use http::{HeaderMap, HeaderName, HeaderValue};
use revision::{
	BorrowedReader, DeserializeRevisioned, Revisioned, SerializeRevisioned, SkipRevisioned,
	WalkRevisioned, revisioned,
};
use storekey::{BorrowDecode, BorrowReader, DecodeError, Encode, EncodeError, Writer};
use surrealdb_collections::{Entry as VecMapEntry, VecMap, VecMapIntoIter};
use surrealdb_types::{SqlFormat, ToSql, write_sql};

use crate::err::Error;
use crate::expr::literal::ObjectEntry;
use crate::fmt::EscapeObjectKey;
use crate::val::{IndexFormat, RecordId, Strand, Value};

/// Canonical, always-sorted object map.
///
/// This is the byte-identical successor to the original `Object` newtype: it
/// keeps the exact on-disk, revision, and storekey encoding the engine has
/// always used (sorted keys, `#[revision(indexed_map)]`). The presentation
/// order side-car lives on [`Object`], **not** here, so storage, comparison,
/// index keys, and complex Record IDs are entirely unaffected.
///
/// - **Rev 1** — `u16 revision || VecMap<Strand, Value>` (length-prefixed sorted entries).
/// - **Rev 2** — optimised envelope, inner `VecMap` written via the indexed-map prologue.
#[revisioned(revision(1), revision(2, optimised))]
#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Hash, Encode, BorrowDecode)]
#[storekey(format = "()")]
#[storekey(format = "IndexFormat")]
pub(crate) struct ObjectMap(#[revision(indexed_map)] pub(crate) VecMap<Strand, Value>);

impl Deref for ObjectMap {
	type Target = VecMap<Strand, Value>;
	fn deref(&self) -> &Self::Target {
		&self.0
	}
}

impl DerefMut for ObjectMap {
	fn deref_mut(&mut self) -> &mut Self::Target {
		&mut self.0
	}
}

impl IntoIterator for ObjectMap {
	type Item = (Strand, Value);
	type IntoIter = VecMapIntoIter<Strand, Value>;
	fn into_iter(self) -> Self::IntoIter {
		self.0.into_iter()
	}
}

/// An object value: a canonical sorted map plus an **optional** presentation
/// order used only for output.
///
/// `order`, when present, is a permutation of indices into the sorted entries
/// (`order[i]` is the sorted position of the `i`-th field in display order). It
/// is deliberately excluded from `PartialEq`/`Eq`/`Ord`/`Hash` and from every
/// encoding path, so two objects that differ only in display order remain
/// equal, hash identically, and serialise to identical bytes. This guarantees
/// the feature can never affect comparison, dedup, index keys, Record IDs, or
/// storage (see issue #4053). `order: None` is byte-for-byte today's behaviour.
#[derive(Clone, Debug, Default)]
pub(crate) struct Object(pub(crate) ObjectMap, pub(crate) Option<Box<[u32]>>);

// --- Comparison / hashing: delegate to the sorted map, ignore `order`. ---

impl PartialEq for Object {
	fn eq(&self, other: &Self) -> bool {
		self.0 == other.0
	}
}
impl Eq for Object {}
impl PartialOrd for Object {
	fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
		Some(self.cmp(other))
	}
}
impl Ord for Object {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		self.0.cmp(&other.0)
	}
}
impl Hash for Object {
	fn hash<H: Hasher>(&self, state: &mut H) {
		self.0.hash(state);
	}
}

// --- Encoding: delegate to the sorted map; `order` is never written. ---

impl<F> Encode<F> for Object
where
	ObjectMap: Encode<F>,
{
	fn encode<W: std::io::Write>(&self, w: &mut Writer<W>) -> Result<(), EncodeError> {
		self.0.encode(w)
	}
}

impl<'de, F> BorrowDecode<'de, F> for Object
where
	ObjectMap: BorrowDecode<'de, F>,
{
	fn borrow_decode(r: &mut BorrowReader<'de>) -> Result<Self, DecodeError> {
		Ok(Object(ObjectMap::borrow_decode(r)?, None))
	}
}

impl SerializeRevisioned for Object {
	fn serialize_revisioned<W: std::io::Write>(&self, w: &mut W) -> Result<(), revision::Error> {
		self.0.serialize_revisioned(w)
	}
}

impl DeserializeRevisioned for Object {
	fn deserialize_revisioned<R: std::io::Read>(r: &mut R) -> Result<Self, revision::Error> {
		Ok(Object(ObjectMap::deserialize_revisioned(r)?, None))
	}
}

impl Revisioned for Object {
	fn revision() -> u16 {
		ObjectMap::revision()
	}
}

// Skipping and zero-copy walking delegate to the inner map, which carries the
// real wire shape. The presentation-order side-car is never on the wire.
impl SkipRevisioned for Object {
	fn skip_revisioned<R: std::io::Read>(r: &mut R) -> Result<(), revision::Error> {
		ObjectMap::skip_revisioned(r)
	}
}

impl WalkRevisioned for Object {
	type Walker<'r, R: BorrowedReader + 'r> = <ObjectMap as WalkRevisioned>::Walker<'r, R>;

	fn walk_revisioned<'r, R: BorrowedReader>(
		reader: &'r mut R,
	) -> Result<Self::Walker<'r, R>, revision::Error> {
		ObjectMap::walk_revisioned(reader)
	}
}

// --- Constructors: build a sorted map with no presentation order. ---

impl From<ObjectMap> for Object {
	fn from(v: ObjectMap) -> Self {
		Self(v, None)
	}
}

impl From<BTreeMap<&str, Value>> for Object {
	fn from(v: BTreeMap<&str, Value>) -> Self {
		let mut entries = Vec::with_capacity(v.len());
		entries.extend(v.into_iter().map(|(k, val)| (Strand::from(k), val)));
		Self(ObjectMap(VecMap::from_sorted_vec_unchecked(entries)), None)
	}
}

impl From<BTreeMap<String, Value>> for Object {
	fn from(v: BTreeMap<String, Value>) -> Self {
		let mut entries = Vec::with_capacity(v.len());
		entries.extend(v.into_iter().map(|(k, val)| (Strand::from(k), val)));
		Self(ObjectMap(VecMap::from_sorted_vec_unchecked(entries)), None)
	}
}

impl From<BTreeMap<Strand, Value>> for Object {
	fn from(v: BTreeMap<Strand, Value>) -> Self {
		Self(ObjectMap(VecMap::from(v)), None)
	}
}

impl From<VecMap<Strand, Value>> for Object {
	fn from(v: VecMap<Strand, Value>) -> Self {
		Self(ObjectMap(v), None)
	}
}

impl From<VecMap<String, Value>> for Object {
	fn from(v: VecMap<String, Value>) -> Self {
		let mut entries = Vec::with_capacity(v.len());
		entries.extend(v.into_iter().map(|(k, val)| (Strand::from(k), val)));
		Self(ObjectMap(VecMap::from_sorted_vec_unchecked(entries)), None)
	}
}

impl From<VecMap<&str, Value>> for Object {
	fn from(v: VecMap<&str, Value>) -> Self {
		let mut entries = Vec::with_capacity(v.len());
		entries.extend(v.into_iter().map(|(k, val)| (Strand::from(k), val)));
		Self(ObjectMap(VecMap::from_sorted_vec_unchecked(entries)), None)
	}
}

impl FromIterator<(String, Value)> for Object {
	fn from_iter<T: IntoIterator<Item = (String, Value)>>(iter: T) -> Self {
		Self(ObjectMap(VecMap::from_iter(iter.into_iter().map(|(k, v)| (Strand::from(k), v)))), None)
	}
}

impl FromIterator<(Strand, Value)> for Object {
	fn from_iter<T: IntoIterator<Item = (Strand, Value)>>(iter: T) -> Self {
		Self(ObjectMap(VecMap::from_iter(iter)), None)
	}
}

impl<'a> FromIterator<(&'a str, Value)> for Object {
	fn from_iter<T: IntoIterator<Item = (&'a str, Value)>>(iter: T) -> Self {
		Self(ObjectMap(VecMap::from_iter(iter.into_iter().map(|(k, v)| (Strand::from(k), v)))), None)
	}
}

impl From<BTreeMap<String, String>> for Object {
	fn from(v: BTreeMap<String, String>) -> Self {
		let mut entries = Vec::with_capacity(v.len());
		entries.extend(v.into_iter().map(|(k, v)| (Strand::from(k), Value::from(v))));
		Self(ObjectMap(VecMap::from_sorted_vec_unchecked(entries)), None)
	}
}

impl From<Vec<(String, Value)>> for Object {
	fn from(v: Vec<(String, Value)>) -> Self {
		Self(ObjectMap(VecMap::from_iter(v.into_iter().map(|(k, val)| (Strand::from(k), val)))), None)
	}
}

impl From<HashMap<&str, Value>> for Object {
	fn from(v: HashMap<&str, Value>) -> Self {
		Self(ObjectMap(VecMap::from_iter(v.into_iter().map(|(key, val)| (Strand::from(key), val)))), None)
	}
}

impl From<HashMap<String, Value>> for Object {
	fn from(v: HashMap<String, Value>) -> Self {
		Self(ObjectMap(VecMap::from_iter(v.into_iter().map(|(k, val)| (Strand::from(k), val)))), None)
	}
}

impl From<Option<Self>> for Object {
	fn from(v: Option<Self>) -> Self {
		v.unwrap_or_default()
	}
}

impl TryFrom<Object> for crate::types::PublicObject {
	type Error = anyhow::Error;

	fn try_from(s: Object) -> Result<Self, Self::Error> {
		// Carry the presentation order across the boundary. Both sides sort the
		// same string keys identically, so the index permutation is preserved.
		let order = s.1.clone();
		let mut obj = s
			.0
			.0
			.into_iter()
			.map(|(k, v)| crate::types::PublicValue::try_from(v).map(|v| (k.into_string(), v)))
			.collect::<Result<crate::types::PublicObject, Self::Error>>()?;
		obj.set_display_order(order);
		Ok(obj)
	}
}

impl From<crate::types::PublicObject> for Object {
	fn from(s: crate::types::PublicObject) -> Self {
		let mut entries = Vec::with_capacity(s.len());
		entries.extend(s.into_iter().map(|(k, v)| (Strand::from(k), Value::from(v))));
		Self(ObjectMap(VecMap::from_sorted_vec_unchecked(entries)), None)
	}
}

impl Deref for Object {
	type Target = VecMap<Strand, Value>;
	fn deref(&self) -> &Self::Target {
		&self.0.0
	}
}

impl DerefMut for Object {
	fn deref_mut(&mut self) -> &mut Self::Target {
		&mut self.0.0
	}
}

impl IntoIterator for Object {
	type Item = (Strand, Value);
	type IntoIter = VecMapIntoIter<Strand, Value>;
	fn into_iter(self) -> Self::IntoIter {
		self.0.0.into_iter()
	}
}

impl TryInto<BTreeMap<String, String>> for Object {
	type Error = Error;
	fn try_into(self) -> Result<BTreeMap<String, String>, Self::Error> {
		self.into_iter().map(|(k, v)| Ok((k.into_string(), v.coerce_to()?))).collect()
	}
}

impl TryInto<HeaderMap> for Object {
	type Error = Error;
	fn try_into(self) -> Result<HeaderMap, Self::Error> {
		let mut headermap = HeaderMap::new();
		for (k, v) in self {
			let k: HeaderName = k.as_str().parse()?;
			let v: HeaderValue = v.coerce_to::<String>()?.parse()?;
			headermap.insert(k, v);
		}

		Ok(headermap)
	}
}

impl Object {
	/// Insert a key-value pair into the object.
	///
	/// The key is accepted as anything convertible to [`Strand`]
	/// (including `String`, `&str`, and `Strand`), which keeps call
	/// sites ergonomic.
	#[inline]
	pub fn insert(&mut self, key: impl Into<Strand>, value: Value) -> Option<Value> {
		self.0.insert(key.into(), value)
	}

	/// Look up a value by key.
	///
	/// Takes `&str`, so `&String` callers work transparently via deref
	/// coercion — avoiding the `Borrow<String>` trait bound that would
	/// otherwise be required on `Strand`.
	#[inline]
	pub fn get(&self, key: &str) -> Option<&Value> {
		self.0.get(key)
	}

	/// Look up a value by key for mutation.
	#[inline]
	pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
		self.0.get_mut(key)
	}

	/// Check whether the object contains a given key.
	#[inline]
	pub fn contains_key(&self, key: &str) -> bool {
		self.0.contains_key(key)
	}

	/// Remove and return the value for `key`.
	#[inline]
	pub fn remove(&mut self, key: &str) -> Option<Value> {
		self.0.remove(key)
	}

	/// Return the map entry for `key`, so callers can use the
	/// `Entry` API without manual interning.
	#[inline]
	pub fn entry(&mut self, key: impl Into<Strand>) -> VecMapEntry<'_, Strand, Value> {
		self.0.entry(key.into())
	}

	/// Fetch the record id if there is one
	pub fn rid(&self) -> Option<RecordId> {
		match self.get("id") {
			Some(Value::RecordId(v)) => Some(v.clone()),
			_ => None,
		}
	}

	pub fn into_literal(self) -> Vec<ObjectEntry> {
		self.0
			.0
			.into_iter()
			.map(|(k, v)| ObjectEntry {
				key: k,
				value: v.into_literal(),
			})
			.collect()
	}

	/// Build the presentation order from a sequence of keys in the order they
	/// were written by the query, recording where each landed in the sorted
	/// map. Keys not present are skipped.
	pub(crate) fn set_written_order<'a, I>(&mut self, written_keys: I)
	where
		I: IntoIterator<Item = &'a str>,
	{
		let sorted: Vec<&Strand> = self.0.0.keys().collect();
		let mut order: Vec<u32> = Vec::new();
		for key in written_keys {
			if let Some(idx) = sorted.iter().position(|k| k.as_str() == key) {
				order.push(idx as u32);
			}
		}
		self.1 = if order.is_empty() {
			None
		} else {
			Some(order.into_boxed_slice())
		};
	}

	/// Iterate entries in presentation order when a side-car order is set,
	/// otherwise in canonical sorted order.
	pub(crate) fn iter_display(&self) -> Vec<(&Strand, &Value)> {
		let entries: Vec<(&Strand, &Value)> = self.0.0.iter().collect();
		match &self.1 {
			Some(order) => {
				order.iter().filter_map(|&i| entries.get(i as usize).copied()).collect()
			}
			None => entries,
		}
	}
}

impl std::ops::Add for Object {
	type Output = Self;

	fn add(self, rhs: Self) -> Self::Output {
		Self(ObjectMap(VecMap::merge_sorted_prefer_rhs(self.0.0, rhs.0.0)), None)
	}
}

impl ToSql for Object {
	fn fmt_sql(&self, f: &mut String, sql_fmt: SqlFormat) {
		if self.is_empty() {
			return f.push_str("{  }");
		}

		if sql_fmt.is_pretty() {
			f.push('{');
		} else {
			f.push_str("{ ");
		}

		if !self.is_empty() {
			let inner_fmt = sql_fmt.increment();
			if sql_fmt.is_pretty() {
				f.push('\n');
				inner_fmt.write_indent(f);
			}
			for (i, (key, value)) in self.iter_display().into_iter().enumerate() {
				if i > 0 {
					inner_fmt.write_separator(f);
				}
				write_sql!(f, sql_fmt, "{}: ", EscapeObjectKey(key.as_str()));
				value.fmt_sql(f, inner_fmt);
			}
			if sql_fmt.is_pretty() {
				f.push('\n');
				sql_fmt.write_indent(f);
			}
		}

		if sql_fmt.is_pretty() {
			f.push('}');
		} else {
			f.push_str(" }");
		}
	}
}

#[cfg(test)]
mod tests {
	use storekey::{decode_borrow, encode_vec};

	use super::*;

	fn obj_written(pairs: &[(&str, i64)], written: &[&str]) -> Object {
		let mut o = Object::default();
		for (k, v) in pairs {
			o.insert(*k, Value::from(*v));
		}
		o.set_written_order(written.iter().copied());
		o
	}

	#[test]
	fn order_excluded_from_eq_and_hash() {
		// Same fields, different written order → must stay equal + hash-equal.
		let a = obj_written(&[("a", 1), ("b", 2), ("c", 3)], &["c", "a", "b"]);
		let b = obj_written(&[("a", 1), ("b", 2), ("c", 3)], &["a", "b", "c"]);
		assert_eq!(a, b, "objects differing only in display order must be equal");

		let mut ha = std::collections::hash_map::DefaultHasher::new();
		let mut hb = std::collections::hash_map::DefaultHasher::new();
		a.hash(&mut ha);
		b.hash(&mut hb);
		assert_eq!(ha.finish(), hb.finish(), "hashes must match regardless of order");
		assert_eq!(a.cmp(&b), std::cmp::Ordering::Equal, "Ord must ignore order");
	}

	#[test]
	fn display_order_is_preserved() {
		let a = obj_written(&[("a", 1), ("b", 2), ("c", 3)], &["c", "a", "b"]);
		let keys: Vec<&str> = a.iter_display().into_iter().map(|(k, _)| k.as_str()).collect();
		assert_eq!(keys, vec!["c", "a", "b"], "iter_display must follow written order");

		// Logical iteration stays sorted.
		let sorted: Vec<&str> = a.keys().map(|k| k.as_str()).collect();
		assert_eq!(sorted, vec!["a", "b", "c"], "logical iteration stays sorted");
	}

	#[test]
	fn storekey_encoding_is_byte_identical() {
		// An object with a display-order side-car must encode to exactly the
		// same bytes as the same object without one.
		let ordered = obj_written(&[("a", 1), ("b", 2), ("c", 3)], &["c", "a", "b"]);
		let mut plain = Object::default();
		for (k, v) in [("a", 1), ("b", 2), ("c", 3)] {
			plain.insert(k, Value::from(v));
		}
		assert!(ordered.1.is_some() && plain.1.is_none());

		let enc_ordered = encode_vec(&ordered).expect("encode ordered");
		let enc_plain = encode_vec(&plain).expect("encode plain");
		assert_eq!(enc_ordered, enc_plain, "side-car must not change encoded bytes");

		// And it round-trips back to the canonical (sorted, order-less) value.
		let decoded: Object = decode_borrow(&enc_ordered).expect("decode");
		assert_eq!(decoded, ordered);
		assert!(decoded.1.is_none(), "decoded value carries no display order");
	}
}
