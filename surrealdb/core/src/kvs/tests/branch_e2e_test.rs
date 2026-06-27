//! End-to-end test of copy-on-write database branching on the versioned SurrealKV
//! backend, driven through SurrealQL exactly as a user would:
//!
//!   * `DEFINE DATABASE experiment FROM main` creates a branch,
//!   * the branch transparently sees the parent's rows (fall-through),
//!   * branch-local writes diverge, and
//!   * the parent stays isolated from the branch's writes.
//!
//! Requires a versioned datastore (the overlay reads `parent@base_version`), so the
//! SurrealKV path is opened with `?versioned=true`, and the `DatabaseBranching`
//! experimental capability is enabled.

use surrealdb_types::Value;

use crate::dbs::capabilities::Targets;
use crate::dbs::{Capabilities, QueryResult, Session};
use crate::kvs::Datastore;

/// Number of rows in a `SELECT` query result.
fn rows(qr: QueryResult) -> usize {
	match qr.result.expect("query should succeed") {
		Value::Array(a) => a.len(),
		Value::None => 0,
		other => panic!("expected an array result, got {other:?}"),
	}
}

#[tokio::test]
async fn database_branching_end_to_end_surrealkv() {
	// A versioned SurrealKV datastore (versioning is required for `parent@base_version`).
	let dir = std::env::temp_dir().join(format!("sdb-branch-e2e-{}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	let path = format!("surrealkv://{}?versioned=true", dir.display());

	let ds = Datastore::builder()
		.with_capabilities(Capabilities::all().with_experimental(Targets::All))
		.build_with_path(&path)
		.await
		.unwrap();

	// Seed the parent database and COMMIT it first, so the branch's base_version
	// (pinned at DEFINE time) sits after the seed data's commit.
	// PoC: the copy-on-write read overlay is wired into the compute executor, so the
	// session selects it (the new streaming planner is not yet branch-aware — follow-up).
	let main = Session::owner()
		.with_ns("demo")
		.with_db("main")
		.new_planner_strategy(crate::dbs::NewPlannerStrategy::ComputeOnly);
	let res = ds
		.execute(
			"DEFINE NAMESPACE demo;
			 DEFINE DATABASE main;
			 CREATE product:laptop SET name = 'Laptop', price = 1000;
			 CREATE product:phone  SET name = 'Phone',  price = 500;
			 CREATE product:tablet SET name = 'Tablet', price = 300;",
			&main,
			None,
		)
		.await
		.unwrap();
	for qr in res {
		assert!(qr.result.is_ok(), "seed statement failed: {:?}", qr.result);
	}

	// Sanity: the parent's own SELECT works (isolates table-resolution from branching).
	let mut res = ds.execute("SELECT * FROM product;", &main, None).await.unwrap();
	assert_eq!(rows(res.remove(0)), 3, "parent SELECT should return 3 rows");

	// Now branch `main` (separate transaction → base_version is after the seed commit).
	let res = ds.execute("DEFINE DATABASE experiment FROM main;", &main, None).await.unwrap();
	for qr in res {
		assert!(qr.result.is_ok(), "branch statement failed: {:?}", qr.result);
	}

	// The branch transparently inherits the parent's three rows (copy-on-write).
	let branch = Session::owner()
		.with_ns("demo")
		.with_db("experiment")
		.new_planner_strategy(crate::dbs::NewPlannerStrategy::ComputeOnly);
	let mut res = ds.execute("SELECT * FROM product;", &branch, None).await.unwrap();
	assert_eq!(rows(res.remove(0)), 3, "branch should inherit the parent's 3 rows");

	// Diverge the branch with a branch-only record; the branch now shows four.
	let mut res = ds
		.execute(
			"CREATE product:keyboard SET name = 'Keyboard', price = 80;
			 SELECT * FROM product;",
			&branch,
			None,
		)
		.await
		.unwrap();
	let select = res.remove(1);
	assert_eq!(rows(select), 4, "branch should show its 3 inherited + 1 local row");

	// The parent is untouched by the branch's writes (keyspace isolation).
	let mut res = ds.execute("SELECT * FROM product;", &main, None).await.unwrap();
	assert_eq!(rows(res.remove(0)), 3, "parent must stay isolated from branch writes");

	let _ = std::fs::remove_dir_all(&dir);
}
