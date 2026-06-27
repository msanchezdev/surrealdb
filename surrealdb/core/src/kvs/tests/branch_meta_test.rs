//! Tests for the copy-on-write branch-metadata catalog key + provider methods.
//!
//! Exercises the separate additive `key::namespace::bm` keyspace:
//! put/get/del round-trip plus the per-namespace children scan that backs the
//! `REMOVE DATABASE` guard.

use crate::catalog::{BranchMetadata, DatabaseId, NamespaceId};
use crate::dbs::Capabilities;
use crate::kvs::Datastore;
use crate::kvs::LockType::Optimistic;
use crate::kvs::TransactionType::Write;

#[tokio::test]
async fn branch_meta_roundtrip_and_children_scan() {
	let ds = Datastore::builder()
		.with_capabilities(Capabilities::all())
		.build_with_path("memory")
		.await
		.unwrap();
	let tx = ds.transaction(Write, Optimistic).await.unwrap();

	let ns = NamespaceId(1);
	let parent = DatabaseId(1);
	let other_parent = DatabaseId(9);

	let child_a = DatabaseId(2);
	let child_b = DatabaseId(3);
	let unrelated = DatabaseId(4);

	// Two branches of `parent`, one branch of a different parent.
	tx.put_branch_meta(
		ns,
		child_a,
		&BranchMetadata {
			parent,
			base_version: 100,
		},
	)
	.await
	.unwrap();
	tx.put_branch_meta(
		ns,
		child_b,
		&BranchMetadata {
			parent,
			base_version: 200,
		},
	)
	.await
	.unwrap();
	tx.put_branch_meta(
		ns,
		unrelated,
		&BranchMetadata {
			parent: other_parent,
			base_version: 5,
		},
	)
	.await
	.unwrap();

	// Exact fetch returns the pinned metadata.
	let meta = tx.get_branch_meta(ns, child_a).await.unwrap().expect("branch meta should exist");
	assert_eq!(
		meta,
		BranchMetadata {
			parent,
			base_version: 100,
		}
	);

	// An ordinary (non-branch) database has no metadata.
	assert!(tx.get_branch_meta(ns, DatabaseId(42)).await.unwrap().is_none());

	// Children scan returns exactly the two branches of `parent` (order-independent).
	let mut children = tx.branch_children(ns, parent).await.unwrap();
	children.sort_by_key(|d| d.0);
	assert_eq!(children, vec![child_a, child_b]);

	// The unrelated branch is found only under its own parent.
	assert_eq!(tx.branch_children(ns, other_parent).await.unwrap(), vec![unrelated]);

	// Deletion removes it from both exact-get and the children scan.
	tx.del_branch_meta(ns, child_a).await.unwrap();
	assert!(tx.get_branch_meta(ns, child_a).await.unwrap().is_none());
	assert_eq!(tx.branch_children(ns, parent).await.unwrap(), vec![child_b]);

	tx.cancel().await.unwrap();
}
