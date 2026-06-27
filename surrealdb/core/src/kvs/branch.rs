//! Copy-on-write database branching — the merge cursor.
//!
//! A branch is a first-class database (its own [`DatabaseId`]) whose keyspace is
//! logically layered on top of a *parent* database read at a pinned version. The
//! branch range holds only the rows that have diverged: overwrites, inserts, and
//! tombstones (deletes of rows that still exist in the parent). Everything else
//! falls through to `parent@base_version`.
//!
//! [`MergeCursor`] performs that overlay as a streaming 2-way merge of two already
//! sorted sources — the branch cursor (current) and the parent cursor (pinned) —
//! yielding rows in the *branch* keyspace with **branch-wins** semantics and
//! **tombstones hiding** the underlying parent row.
//!
//! This module is the hard part of the feature proven in isolation; wiring it into
//! the scan path in `dbs/processor.rs` and persisting tombstones is layered on top.
//! Gated behind `ExperimentalTarget::DatabaseBranching`.

// PoC: the merge cursor is exercised by its unit tests but not yet wired into the
// scan path in `dbs/processor.rs`; remove once `collect_range` consumes it.
#![allow(dead_code)]

use std::iter::Peekable;

use super::{Key, Val};

/// Number of leading bytes of a record key that address the namespace + database:
/// `/` `*` ns(4) `*` db(4) — i.e. everything up to (but not including) the `*`
/// separator that precedes the table name. Two records in the same namespace that
/// refer to the same logical row differ *only* within this prefix (the db field),
/// so comparing the bytes after it (`key[NSDB_PREFIX_LEN..]`) compares logical rows.
pub(crate) const NSDB_PREFIX_LEN: usize = 11;

/// The "logical key" of a record key: the portion that is identical between a branch
/// row and the parent row it shadows. Used as the merge ordering key.
#[inline]
fn logical(key: &[u8]) -> &[u8] {
	// A well-formed record key always carries the ns+db prefix. Be defensive: a key
	// shorter than the prefix sorts by its whole self (can't collide with real rows).
	key.get(NSDB_PREFIX_LEN..).unwrap_or(key)
}

/// Rewrite a parent-keyspace key into the branch keyspace by splicing the branch's
/// ns+db prefix in front of the parent row's logical portion. The result is a key
/// downstream decodes as a branch record.
#[inline]
fn rekey_into_branch(branch_prefix: &[u8], parent_key: &[u8]) -> Key {
	debug_assert_eq!(branch_prefix.len(), NSDB_PREFIX_LEN);
	let mut k =
		Vec::with_capacity(NSDB_PREFIX_LEN + parent_key.len().saturating_sub(NSDB_PREFIX_LEN));
	k.extend_from_slice(branch_prefix);
	k.extend_from_slice(logical(parent_key));
	k
}

/// A branch-side entry: a live value, or a tombstone marking a row deleted in the
/// branch that still exists in the parent. Tombstones suppress the parent row and
/// are themselves never yielded.
pub(crate) type BranchEntry = (Key, Option<Val>);

/// Streaming copy-on-write overlay of a branch range over a parent range.
///
/// Both inputs MUST be sorted ascending by their *logical* key (which, for a single
/// table range, is the same as sorting by the raw key — the ns+db prefix is constant
/// within each source). The cursor yields `(Key, Val)` in the branch keyspace.
pub(crate) struct MergeCursor<B, P>
where
	B: Iterator<Item = BranchEntry>,
	P: Iterator<Item = (Key, Val)>,
{
	branch: Peekable<B>,
	parent: Peekable<P>,
	/// The branch's ns+db prefix, spliced onto parent rows so they surface as branch keys.
	branch_prefix: Key,
}

/// What to do on a single step, decided from peeked heads before mutating either side
/// (keeps the borrow checker happy: peeks are dropped before the `next()` calls).
enum Step {
	TakeBranch,
	TakeParent,
	TakeBoth,
	Done,
}

impl<B, P> MergeCursor<B, P>
where
	B: Iterator<Item = BranchEntry>,
	P: Iterator<Item = (Key, Val)>,
{
	pub(crate) fn new(branch: B, parent: P, branch_prefix: Key) -> Self {
		debug_assert_eq!(
			branch_prefix.len(),
			NSDB_PREFIX_LEN,
			"branch_prefix must be the {NSDB_PREFIX_LEN}-byte ns+db addressing prefix"
		);
		Self {
			branch: branch.peekable(),
			parent: parent.peekable(),
			branch_prefix,
		}
	}

	fn decide(&mut self) -> Step {
		match (self.branch.peek(), self.parent.peek()) {
			(None, None) => Step::Done,
			(Some(_), None) => Step::TakeBranch,
			(None, Some(_)) => Step::TakeParent,
			(Some((bk, _)), Some((pk, _))) => match logical(bk).cmp(logical(pk)) {
				std::cmp::Ordering::Less => Step::TakeBranch,
				std::cmp::Ordering::Greater => Step::TakeParent,
				// Same logical row present in both ranges — branch wins, consume both.
				std::cmp::Ordering::Equal => Step::TakeBoth,
			},
		}
	}
}

impl<B, P> Iterator for MergeCursor<B, P>
where
	B: Iterator<Item = BranchEntry>,
	P: Iterator<Item = (Key, Val)>,
{
	type Item = (Key, Val);

	fn next(&mut self) -> Option<Self::Item> {
		loop {
			match self.decide() {
				Step::Done => return None,
				Step::TakeBranch => {
					let (bk, bv) = self.branch.next().expect("peeked branch head");
					match bv {
						Some(v) => return Some((bk, v)),
						// Tombstone with no parent row to hide — nothing to emit, keep going.
						None => continue,
					}
				}
				Step::TakeParent => {
					let (pk, pv) = self.parent.next().expect("peeked parent head");
					return Some((rekey_into_branch(&self.branch_prefix, &pk), pv));
				}
				Step::TakeBoth => {
					let (bk, bv) = self.branch.next().expect("peeked branch head");
					let _shadowed = self.parent.next().expect("peeked parent head");
					match bv {
						// Branch overwrite wins over the parent row.
						Some(v) => return Some((bk, v)),
						// Tombstone hides the parent row: emit neither, advance.
						None => continue,
					}
				}
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const NS: [u8; 4] = [0, 0, 0, 1];

	/// Build a record key `/`*`ns`*`db`*tb\0<id>` for the given database id.
	/// The first [`NSDB_PREFIX_LEN`] bytes are the ns+db addressing prefix.
	fn rkey(db: u32, id: &[u8]) -> Key {
		let mut k = Vec::new();
		k.push(b'/');
		k.push(b'*');
		k.extend_from_slice(&NS);
		k.push(b'*');
		k.extend_from_slice(&db.to_be_bytes());
		// logical portion begins here (index 11)
		k.extend_from_slice(b"*tb\0");
		k.extend_from_slice(id);
		k
	}

	const PARENT_DB: u32 = 1;
	const BRANCH_DB: u32 = 2;

	fn branch_prefix() -> Key {
		rkey(BRANCH_DB, b"")[..NSDB_PREFIX_LEN].to_vec()
	}

	/// Run the merge and return (key, value) pairs with values as UTF-8 for readability.
	fn run(branch: Vec<BranchEntry>, parent: Vec<(Key, Val)>) -> Vec<(Key, String)> {
		MergeCursor::new(branch.into_iter(), parent.into_iter(), branch_prefix())
			.map(|(k, v)| (k, String::from_utf8(v).unwrap()))
			.collect()
	}

	fn bval(db: u32, id: &[u8], v: &str) -> BranchEntry {
		(rkey(db, id), Some(v.as_bytes().to_vec()))
	}
	fn tomb(db: u32, id: &[u8]) -> BranchEntry {
		(rkey(db, id), None)
	}
	fn pval(db: u32, id: &[u8], v: &str) -> (Key, Val) {
		(rkey(db, id), v.as_bytes().to_vec())
	}

	#[test]
	fn parent_only_falls_through() {
		let out = run(vec![], vec![pval(PARENT_DB, b"a", "pa"), pval(PARENT_DB, b"b", "pb")]);
		// Both parent rows surface, rewritten into the branch keyspace.
		assert_eq!(
			out,
			vec![(rkey(BRANCH_DB, b"a"), "pa".into()), (rkey(BRANCH_DB, b"b"), "pb".into())]
		);
	}

	#[test]
	fn branch_overwrite_wins() {
		let out = run(
			vec![bval(BRANCH_DB, b"b", "branch-b")],
			vec![pval(PARENT_DB, b"a", "pa"), pval(PARENT_DB, b"b", "pb")],
		);
		assert_eq!(
			out,
			vec![
				(rkey(BRANCH_DB, b"a"), "pa".into()), // fell through from parent
				(rkey(BRANCH_DB, b"b"), "branch-b".into()), // branch shadowed parent's "pb"
			]
		);
	}

	#[test]
	fn branch_insert_interleaves_in_order() {
		// Branch adds "a0" (before "a") and "c" (after "b"); parent has "a","b".
		let out = run(
			vec![bval(BRANCH_DB, b"a0", "new-a0"), bval(BRANCH_DB, b"c", "new-c")],
			vec![pval(PARENT_DB, b"a", "pa"), pval(PARENT_DB, b"b", "pb")],
		);
		assert_eq!(
			out,
			vec![
				(rkey(BRANCH_DB, b"a"), "pa".into()),
				(rkey(BRANCH_DB, b"a0"), "new-a0".into()),
				(rkey(BRANCH_DB, b"b"), "pb".into()),
				(rkey(BRANCH_DB, b"c"), "new-c".into()),
			]
		);
	}

	#[test]
	fn tombstone_hides_parent_row() {
		let out = run(
			vec![tomb(BRANCH_DB, b"b")],
			vec![
				pval(PARENT_DB, b"a", "pa"),
				pval(PARENT_DB, b"b", "pb"),
				pval(PARENT_DB, b"c", "pc"),
			],
		);
		// "b" is deleted in the branch — only a and c remain.
		assert_eq!(
			out,
			vec![(rkey(BRANCH_DB, b"a"), "pa".into()), (rkey(BRANCH_DB, b"c"), "pc".into())]
		);
	}

	#[test]
	fn tombstone_without_parent_is_noop() {
		// Deleting a row that never existed in the parent yields nothing for that key.
		let out = run(vec![tomb(BRANCH_DB, b"ghost")], vec![pval(PARENT_DB, b"a", "pa")]);
		assert_eq!(out, vec![(rkey(BRANCH_DB, b"a"), "pa".into())]);
	}

	#[test]
	fn both_empty() {
		assert!(run(vec![], vec![]).is_empty());
	}

	#[test]
	fn branch_only_when_parent_empty() {
		let out = run(vec![bval(BRANCH_DB, b"x", "bx"), tomb(BRANCH_DB, b"y")], vec![]);
		// Live branch row surfaces; tombstone over nothing is dropped.
		assert_eq!(out, vec![(rkey(BRANCH_DB, b"x"), "bx".into())]);
	}

	#[test]
	fn mixed_overwrite_insert_delete_fallthrough() {
		// Parent: a,b,c,d.  Branch: overwrite b, delete c, insert b5 (between b and c).
		let out = run(
			vec![bval(BRANCH_DB, b"b", "B!"), bval(BRANCH_DB, b"b5", "B5"), tomb(BRANCH_DB, b"c")],
			vec![
				pval(PARENT_DB, b"a", "pa"),
				pval(PARENT_DB, b"b", "pb"),
				pval(PARENT_DB, b"c", "pc"),
				pval(PARENT_DB, b"d", "pd"),
			],
		);
		assert_eq!(
			out,
			vec![
				(rkey(BRANCH_DB, b"a"), "pa".into()),  // fall-through
				(rkey(BRANCH_DB, b"b"), "B!".into()),  // overwrite wins
				(rkey(BRANCH_DB, b"b5"), "B5".into()), // branch insert, correctly ordered
				// c deleted by tombstone — absent
				(rkey(BRANCH_DB, b"d"), "pd".into()), // fall-through
			]
		);
	}

	#[test]
	fn emitted_keys_are_in_branch_keyspace() {
		// Every emitted key must carry the branch db id, never the parent's.
		let out = run(vec![bval(BRANCH_DB, b"b", "bb")], vec![pval(PARENT_DB, b"a", "pa")]);
		for (k, _) in &out {
			assert_eq!(&k[..NSDB_PREFIX_LEN], branch_prefix().as_slice());
			assert_ne!(&k[7..11], &PARENT_DB.to_be_bytes(), "parent db id leaked into output");
		}
	}
}
