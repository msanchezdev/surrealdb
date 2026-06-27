use anyhow::{Result, bail};
use reblessive::tree::Stk;

use super::DefineKind;
use crate::catalog::providers::{DatabaseProvider, NamespaceProvider};
use crate::catalog::{BranchMetadata, DatabaseDefinition};
use crate::ctx::FrozenContext;
use crate::dbs::capabilities::ExperimentalTarget;
use crate::dbs::Options;
use crate::doc::CursorDoc;
use crate::err::Error;
use crate::expr::changefeed::ChangeFeed;
use crate::expr::parameterize::expr_to_ident;
use crate::expr::statements::info::InfoStructure;
use crate::expr::{Base, Expr, FlowResultExt, Literal};
use crate::iam::{Action, ResourceKind};
use crate::val::{Datetime, Value};

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct DefineDatabaseStatement {
	pub kind: DefineKind,
	pub id: Option<u32>,
	pub name: Expr,
	pub strict: bool,
	pub comment: Expr,
	pub changefeed: Option<ChangeFeed>,
	/// Copy-on-write branch source: `FROM <source_db>`. `None` = ordinary database.
	/// Gated behind `ExperimentalTarget::DatabaseBranching`.
	pub from: Option<Expr>,
	/// Optional `VERSION <datetime>` after `FROM` — the point in the source's
	/// history to branch from. `None` = branch at the source's current state
	/// (still pinned to a concrete versionstamp at compute time).
	pub from_version: Option<Expr>,
}

impl Default for DefineDatabaseStatement {
	fn default() -> Self {
		Self {
			kind: DefineKind::Default,
			id: None,
			name: Expr::Literal(Literal::None),
			comment: Expr::Literal(Literal::None),
			changefeed: None,
			strict: false,
			from: None,
			from_version: None,
		}
	}
}

impl DefineDatabaseStatement {
	/// Process this type returning a computed simple Value
	#[instrument(level = "trace", name = "DefineDatabaseStatement::compute", skip_all)]
	pub(crate) async fn compute(
		&self,
		stk: &mut Stk,
		ctx: &FrozenContext,
		opt: &Options,
		doc: Option<&CursorDoc>,
	) -> Result<Value> {
		// Allowed to run?
		ctx.is_allowed(opt, Action::Edit, ResourceKind::Database, Base::Ns)?;

		// Get the NS
		let ns = opt.ns()?;

		// Fetch the transaction
		let txn = ctx.tx();
		let nsv = txn.get_or_add_ns(Some(ctx), ns).await?;

		// Process the name
		let name = expr_to_ident(stk, ctx, opt, doc, &self.name, "database name").await?;

		// Branch precondition checks (`DEFINE DATABASE … FROM …`).
		if self.from.is_some() {
			// Gated behind the experimental capability.
			if !ctx.get_capabilities().allows_experimental(&ExperimentalTarget::DatabaseBranching) {
				bail!(Error::Thrown(
					"Database branching is experimental; enable it with \
					 --allow-experimental database_branching"
						.to_string(),
				));
			}
			// Q1: OVERWRITE would silently discard a branch's data — reject the combination.
			if matches!(self.kind, DefineKind::Overwrite) {
				bail!(Error::Thrown(
					"DEFINE DATABASE ... OVERWRITE cannot be combined with FROM (branching)"
						.to_string(),
				));
			}
		}

		// Check if the definition exists
		let database_id = if let Some(db) = txn.get_db_by_name(ns, &name, None).await? {
			match self.kind {
				DefineKind::Default => {
					if !opt.import {
						bail!(Error::DbAlreadyExists {
							name: name.clone(),
						});
					}
				}
				DefineKind::Overwrite => {}
				DefineKind::IfNotExists => {
					return Ok(Value::None);
				}
			}

			db.database_id
		} else {
			ctx.try_get_sequences()?.next_database_id(Some(ctx), nsv.namespace_id).await?
		};

		let comment = stk
			.run(|stk| self.comment.compute(stk, ctx, opt, doc))
			.await
			.catch_return()?
			.cast_to()?;

		// Set the database definition, keyed by namespace name and database name.
		let db_def = DatabaseDefinition {
			namespace_id: nsv.namespace_id,
			database_id,
			name: name.clone().into(),
			comment,
			changefeed: self.changefeed,
			strict: self.strict,
		};
		txn.put_db(nsv.name.as_str(), db_def).await?;

		// Copy-on-write branch metadata: resolve the parent + pin the base version.
		if let Some(from_expr) = &self.from {
			// Resolve the source database (must already exist in this namespace).
			let src_name =
				expr_to_ident(stk, ctx, opt, doc, from_expr, "source database name").await?;
			let Some(src_db) = txn.get_db_by_name(ns, &src_name, None).await? else {
				bail!(Error::DbNotFound {
					name: src_name,
				});
			};
			// v1 is single-level: the source must not itself be a branch.
			if txn.get_branch_meta(nsv.namespace_id, src_db.database_id).await?.is_some() {
				bail!(Error::Thrown(format!(
					"Cannot branch from `{src_name}`: branching from a branch is not supported"
				)));
			}
			// Resolve and pin the base version — always a concrete versionstamp so the
			// branch is isolated from later parent writes.
			let base_version = match &self.from_version {
				// `VERSION <datetime>` selects a point in the source's history (same
				// spelling/mechanism as `SELECT … VERSION`).
				Some(vexpr) => {
					let ts_impl = txn.timestamp_impl();
					let dt = stk
						.run(|stk| vexpr.compute(stk, ctx, opt, doc))
						.await
						.catch_return()?
						.cast_to::<Datetime>()?;
					dt.to_version_stamp(ts_impl.as_ref())?
				}
				// Branch-at-now: pin to the current monotonic versionstamp. This is
				// strictly greater than every already-committed write, so the branch
				// sees the source's full current state. (A wall-clock `now` datetime
				// maps to HLC counter 0 and could miss same-millisecond writes.)
				None => txn.timestamp().await?.as_versionstamp() as u64,
			};
			txn.put_branch_meta(nsv.namespace_id, database_id, &BranchMetadata {
				parent: src_db.database_id,
				base_version,
			})
			.await?;
		}

		// Clear the cache
		if let Some(cache) = ctx.get_cache() {
			cache.clear();
		}

		// Clear the cache
		txn.clear_cache();
		// Ok all good
		Ok(Value::None)
	}
}
impl InfoStructure for DefineDatabaseStatement {
	fn structure(self) -> Value {
		Value::from(map! {
			"name" => self.name.structure(),
			"comment" => self.comment.structure(),
		})
	}
}
