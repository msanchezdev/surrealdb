use std::fmt::{Display, Formatter};

use revision::{
	DeserializeRevisioned, Revisioned, SerializeRevisioned, SkipRevisioned, revisioned,
};
use serde::{Deserialize, Serialize};
use storekey::{BorrowDecode, Encode};
use surrealdb_strand::Strand;
use surrealdb_types::{SqlFormat, ToSql};

use crate::catalog::NamespaceId;
use crate::expr::ChangeFeed;
use crate::expr::statements::info::InfoStructure;
use crate::kvs::impl_kv_value_revisioned;
use crate::sql::statements::define::DefineDatabaseStatement;
use crate::sql::{Expr, Idiom, Literal};
use crate::val::Value;

#[derive(
	Debug,
	Clone,
	Copy,
	PartialEq,
	Eq,
	PartialOrd,
	Ord,
	Hash,
	Serialize,
	Deserialize,
	Encode,
	BorrowDecode,
)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct DatabaseId(pub u32);

impl_kv_value_revisioned!(DatabaseId);

impl Revisioned for DatabaseId {
	fn revision() -> u16 {
		1
	}
}

impl SerializeRevisioned for DatabaseId {
	#[inline]
	fn serialize_revisioned<W: std::io::Write>(
		&self,
		writer: &mut W,
	) -> Result<(), revision::Error> {
		SerializeRevisioned::serialize_revisioned(&self.0, writer)
	}
}

impl DeserializeRevisioned for DatabaseId {
	#[inline]
	fn deserialize_revisioned<R: std::io::Read>(reader: &mut R) -> Result<Self, revision::Error> {
		DeserializeRevisioned::deserialize_revisioned(reader).map(DatabaseId)
	}
}

impl SkipRevisioned for DatabaseId {
	#[inline]
	fn skip_revisioned<R: std::io::Read>(reader: &mut R) -> Result<(), revision::Error> {
		<u32 as SkipRevisioned>::skip_revisioned(reader)
	}
}

impl revision::WalkRevisioned for DatabaseId {
	type Walker<'r, R: revision::BorrowedReader + 'r> = revision::LeafWalker<'r, DatabaseId, R>;

	#[inline]
	fn walk_revisioned<'r, R: revision::BorrowedReader>(
		reader: &'r mut R,
	) -> Result<Self::Walker<'r, R>, revision::Error> {
		Ok(revision::LeafWalker::new(reader))
	}
}

impl Display for DatabaseId {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

impl From<u32> for DatabaseId {
	fn from(value: u32) -> Self {
		Self(value)
	}
}

#[revisioned(revision = 1)]
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct DatabaseDefinition {
	pub(crate) namespace_id: NamespaceId,
	pub(crate) database_id: DatabaseId,
	pub(crate) name: Strand,
	pub(crate) comment: Option<String>,
	pub(crate) changefeed: Option<ChangeFeed>,
	pub(crate) strict: bool,
}
impl_kv_value_revisioned!(DatabaseDefinition);

impl DatabaseDefinition {
	fn to_sql_definition(&self) -> DefineDatabaseStatement {
		DefineDatabaseStatement {
			name: Expr::Idiom(Idiom::field(self.name.clone())),
			comment: self
				.comment
				.clone()
				.map(|v| Expr::Literal(Literal::String(v.into())))
				.unwrap_or(Expr::Literal(Literal::None)),
			changefeed: self.changefeed.map(|v| v.into()),
			..Default::default()
		}
	}
}

impl ToSql for DatabaseDefinition {
	fn fmt_sql(&self, f: &mut String, fmt: SqlFormat) {
		self.to_sql_definition().fmt_sql(f, fmt)
	}
}

impl InfoStructure for DatabaseDefinition {
	fn structure(self) -> Value {
		Value::from(map! {
			"name" => self.name.into(),
			"comment", if let Some(v) = self.comment => v.into(),
			"id" => self.database_id.0.into(),
		})
	}
}

/// Copy-on-write branch metadata for a database.
///
/// Stored in a separate additive catalog key (`key::namespace::bm`) keyed by the
/// branch's `(NamespaceId, DatabaseId)` — deliberately NOT a field on
/// [`DatabaseDefinition`], so ordinary databases keep a byte-identical on-disk
/// encoding (no revision bump, no `catalog::compat` fixture churn). A row exists
/// here only for branches; its absence means "ordinary database".
///
/// Gated behind `ExperimentalTarget::DatabaseBranching`.
#[revisioned(revision = 1)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) struct BranchMetadata {
	/// The parent database this branch was created from (same namespace).
	pub(crate) parent: DatabaseId,
	/// The parent versionstamp this branch is pinned to — its fall-through base for
	/// reads and the fast-forward-merge anchor. Always set for a branch (resolved at
	/// `DEFINE DATABASE … FROM` time, even when branched at "now").
	pub(crate) base_version: u64,
}
impl_kv_value_revisioned!(BranchMetadata);
