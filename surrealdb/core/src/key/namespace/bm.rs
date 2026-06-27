//! Stores copy-on-write branch metadata, keyed by `(NamespaceId, DatabaseId)`.
//!
//! Namespace-scoped (`/*{ns}!bm{db}`) so all branches in a namespace are
//! contiguous and can be range-scanned in one pass — e.g. to find a parent's
//! live children before allowing `REMOVE DATABASE`. Kept separate from
//! `DatabaseDefinition` so ordinary databases keep a byte-identical encoding
//! (no revision bump). Gated behind `ExperimentalTarget::DatabaseBranching`.

// PoC: the constructor/range helpers are consumed once the DDL, processor overlay,
// and REMOVE guard wiring land; exercised now by the branch-meta provider test.
#![allow(dead_code)]

use anyhow::Result;
use storekey::{BorrowDecode, Encode};

use crate::catalog::{BranchMetadata, DatabaseId, NamespaceId};
use crate::key::category::{Categorise, Category};
use crate::kvs::{KVKey, impl_kv_key_storekey};

#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Encode, BorrowDecode)]
pub(crate) struct BranchMetadataKey {
	__: u8,
	_a: u8,
	pub ns: NamespaceId,
	_b: u8,
	_c: u8,
	_d: u8,
	pub db: DatabaseId,
}

impl_kv_key_storekey!(BranchMetadataKey => BranchMetadata);

pub fn new(ns: NamespaceId, db: DatabaseId) -> BranchMetadataKey {
	BranchMetadataKey::new(ns, db)
}

/// Inclusive-start byte bound covering every branch-metadata key in `ns`.
pub fn prefix(ns: NamespaceId) -> Result<Vec<u8>> {
	let mut k = super::all::new(ns).encode_key()?;
	k.extend_from_slice(b"!bm");
	Ok(k)
}

/// Exclusive-end byte bound: `!bn` is the marker immediately after `!bm`, so
/// `[prefix, suffix)` spans all `/*{ns}!bm{db}` keys regardless of db id.
pub fn suffix(ns: NamespaceId) -> Result<Vec<u8>> {
	let mut k = super::all::new(ns).encode_key()?;
	k.extend_from_slice(b"!bn");
	Ok(k)
}

impl Categorise for BranchMetadataKey {
	fn categorise(&self) -> Category {
		Category::DatabaseBranchMetadata
	}
}

impl BranchMetadataKey {
	pub fn new(ns: NamespaceId, db: DatabaseId) -> Self {
		Self {
			__: b'/',
			_a: b'*',
			ns,
			_b: b'!',
			_c: b'b',
			_d: b'm',
			db,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn key() {
		let val = BranchMetadataKey::new(NamespaceId(1), DatabaseId(2));
		let enc = BranchMetadataKey::encode_key(&val).unwrap();
		assert_eq!(enc, b"/*\x00\x00\x00\x01!bm\x00\x00\x00\x02");
	}

	#[test]
	fn prefix_suffix_span_db_ids() {
		let p = prefix(NamespaceId(1)).unwrap();
		let s = suffix(NamespaceId(1)).unwrap();
		assert_eq!(p, b"/*\x00\x00\x00\x01!bm");
		assert_eq!(s, b"/*\x00\x00\x00\x01!bn");
		// Concrete keys (incl. db id 0 and a large id) sort within [prefix, suffix).
		for id in [0u32, 2, u32::MAX] {
			let k = BranchMetadataKey::new(NamespaceId(1), DatabaseId(id)).encode_key().unwrap();
			assert!(p[..] <= k[..] && k[..] < s[..], "db id {id} out of [prefix, suffix)");
		}
	}
}
