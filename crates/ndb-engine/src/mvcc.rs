//! Multi-Version Concurrency Control — visibility logic (§10).
#![allow(clippy::doc_markdown)] // "SSTable", "MVCC" used liberally as domain terms.
//!
//! v1 model (decisions locked here):
//!
//! - **Append-only storage; supersession derived at read time.** Every
//!   `EntityRecord` / `HyperEdgeRecord` is written with `tx_id_supersede =
//!   TX_ACTIVE` and never mutated on disk. The fact that version *V₁* was
//!   replaced by *V₂* is derived by comparing `tx_id_assert` values across
//!   versions of the same key — the engine never seeks back to update an
//!   older record. This trades a small per-read cost (scan all versions of
//!   a key) for purity (LSM-immutable files, simpler write path) and is
//!   exactly Datomic's approach. Compaction (future commit) prunes
//!   versions that no live snapshot needs.
//!
//! - **Tombstones carry their effective tx in `tx_id_supersede`.** A
//!   tombstone written at `tx = T` has `tx_id_supersede = T` and represents
//!   "this entity/hyperedge is deleted starting at T". The
//!   [`Visibility::winner_at`] resolver treats the tombstone as the latest
//!   "event" for a key when the snapshot is ≥ T.
//!
//! - **Snapshot semantics.** A read transaction with snapshot `S` sees
//!   exactly the records whose `tx_id_assert ≤ S` (and, for tombstones,
//!   whose `tx_id_supersede ≤ S`). Among those, the resolver picks the
//!   highest-tx record — that's the "winner" the caller sees. A tombstone
//!   winner is returned as `Resolved::Deleted`; an entity/hyperedge winner
//!   as `Resolved::Live(record)`.
//!
//! Read-your-own-writes within a transaction works trivially because the
//! visibility test is `tx ≤ snapshot`: the current transaction sets
//! `snapshot = its_own_tx_id` and therefore sees its own writes.

use crate::id::{TX_ACTIVE, TxId};
use crate::record::Record;

/// Outcome of resolving multiple versions of one key against a snapshot.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolved<R> {
    /// No version of this key is visible at the requested snapshot. Either
    /// the key was never asserted before `snapshot`, or only tombstones for
    /// future txs exist.
    Missing,
    /// A tombstone is the latest visible event — the key is deleted as of
    /// this snapshot.
    Deleted {
        /// Tx that committed the delete.
        deleted_at: TxId,
    },
    /// An entity or hyperedge record is the latest visible version.
    Live(R),
}

impl<R> Resolved<R> {
    /// `Some(R)` iff the resolved state is `Live`; `Missing` and `Deleted`
    /// both return `None`. Convenient for "I just want the record or nothing"
    /// callers.
    pub fn into_live(self) -> Option<R> {
        match self {
            Self::Live(r) => Some(r),
            _ => None,
        }
    }

    /// Map the inner record. Useful for borrowing helpers that want
    /// `Resolved<&Record>` from a `Resolved<Record>`.
    pub fn map<T>(self, f: impl FnOnce(R) -> T) -> Resolved<T> {
        match self {
            Self::Missing => Resolved::Missing,
            Self::Deleted { deleted_at } => Resolved::Deleted { deleted_at },
            Self::Live(r) => Resolved::Live(f(r)),
        }
    }
}

/// The effective tx of a record from the visibility resolver's perspective.
///
/// - Entity / HyperEdge: `tx_id_assert`
/// - Tombstone: `tx_id_supersede` (the delete tx)
/// - Dictionary records: `tx_id_assert` is not stored on disk; they are
///   schemaless metadata visible to all snapshots. The resolver treats them
///   as effective-tx `0` so any snapshot >= 0 sees them.
#[must_use]
pub fn effective_tx(record: &Record) -> TxId {
    match record {
        Record::Entity(e) => e.tx_id_assert,
        Record::HyperEdge(h) => h.tx_id_assert,
        Record::Tombstone(t) => t.tx_id_supersede,
        Record::TypeName(_) | Record::RoleName(_) | Record::PropertyKey(_) => TxId::new(0),
    }
}

/// Whether a record is visible at the requested snapshot.
#[must_use]
pub fn visible_at(record: &Record, snapshot: TxId) -> bool {
    let tx = effective_tx(record).get();
    match record {
        Record::Tombstone(t) => snapshot.get() >= t.tx_id_supersede.get(),
        Record::Entity(_) | Record::HyperEdge(_) => snapshot.get() >= tx,
        // Dictionary records are timeless.
        Record::TypeName(_) | Record::RoleName(_) | Record::PropertyKey(_) => true,
    }
}

/// Pick the visible "winner" from an iterator over candidate records.
///
/// This is the general-purpose form. The caller is responsible for collecting
/// all records relevant to one logical entity / hyperedge — including any
/// tombstones for the same identifier even if they sort to a different
/// `SSTableKey` bucket.
pub fn resolve_iter<'a, I: IntoIterator<Item = &'a Record>>(
    versions: I,
    snapshot: TxId,
) -> Resolved<&'a Record> {
    let mut best: Option<&Record> = None;
    let mut best_tx = 0u64;
    let mut best_is_tombstone = false;
    for rec in versions {
        if !visible_at(rec, snapshot) {
            continue;
        }
        let tx = effective_tx(rec).get();
        let is_tomb = matches!(rec, Record::Tombstone(_));
        // Tie-breaking: if two versions share the same effective tx (should
        // be rare in practice, but possible during a multi-record batch),
        // a tombstone wins so deletions are sticky.
        let winner = match best {
            None => true,
            Some(_) if tx > best_tx => true,
            Some(_) if tx == best_tx && is_tomb && !best_is_tombstone => true,
            _ => false,
        };
        if winner {
            best = Some(rec);
            best_tx = tx;
            best_is_tombstone = is_tomb;
        }
    }
    match best {
        None => Resolved::Missing,
        Some(Record::Tombstone(t)) => Resolved::Deleted {
            deleted_at: t.tx_id_supersede,
        },
        Some(r) => Resolved::Live(r),
    }
}

/// Slice-friendly form of [`resolve_iter`].
pub fn resolve(versions: &[Record], snapshot: TxId) -> Resolved<&Record> {
    resolve_iter(versions.iter(), snapshot)
}

/// Owned-result form: takes ownership of the version vec and returns one
/// owned `Record` (cloning the winner if it isn't movable out of the slice).
pub fn resolve_owned(versions: Vec<Record>, snapshot: TxId) -> Resolved<Record> {
    let winner_tx;
    let winner_is_tomb;
    match resolve(&versions, snapshot) {
        Resolved::Missing => return Resolved::Missing,
        Resolved::Deleted { deleted_at } => return Resolved::Deleted { deleted_at },
        Resolved::Live(r) => {
            winner_tx = effective_tx(r).get();
            winner_is_tomb = matches!(r, Record::Tombstone(_));
        }
    }
    // Move the matching record out of `versions`. Match by (effective_tx,
    // is_tombstone) — same predicate the resolver used. Tie-breaks on the
    // first match (the resolver's stable-traversal also took the first
    // qualifying record, so the indexes line up).
    for (i, candidate) in versions.iter().enumerate() {
        if effective_tx(candidate).get() == winner_tx
            && matches!(candidate, Record::Tombstone(_)) == winner_is_tomb
            && visible_at(candidate, snapshot)
        {
            return Resolved::Live(versions.into_iter().nth(i).unwrap());
        }
    }
    unreachable!("resolved winner must exist in input vector")
}

/// Convenience: assert that a record's `tx_id_supersede` field has the v1
/// expected value `TX_ACTIVE` for live entity/hyperedge records. Tombstones
/// are exempt. Use in writer paths that need to enforce v1 discipline.
#[must_use]
pub fn assert_v1_supersede_invariant(record: &Record) -> bool {
    match record {
        Record::Entity(e) => e.tx_id_supersede.get() == TX_ACTIVE,
        Record::HyperEdge(h) => h.tx_id_supersede.get() == TX_ACTIVE,
        Record::Tombstone(_)
        | Record::TypeName(_)
        | Record::RoleName(_)
        | Record::PropertyKey(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{EntityId, HyperedgeId, PropertyId, RoleId, TypeId};
    use crate::record::{EntityRecord, HyperEdgeRecord, TombstoneRecord};
    use crate::value::Value;

    fn entity_v(id: EntityId, tx: u64) -> Record {
        Record::Entity(EntityRecord {
            entity_id: id,
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(tx),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(1), Value::I64(i64::try_from(tx).unwrap()))],
        })
    }
    fn tombstone(target: uuid::Uuid, tx: u64) -> Record {
        Record::Tombstone(TombstoneRecord {
            target_id: target,
            tx_id_supersede: TxId::new(tx),
        })
    }

    #[test]
    fn missing_when_snapshot_predates_all_versions() {
        let id = EntityId::now_v7();
        let versions = vec![entity_v(id, 10), entity_v(id, 20)];
        assert_eq!(resolve(&versions, TxId::new(5)), Resolved::Missing);
    }

    #[test]
    fn latest_visible_wins() {
        let id = EntityId::now_v7();
        let v10 = entity_v(id, 10);
        let v20 = entity_v(id, 20);
        let v30 = entity_v(id, 30);
        let versions = vec![v10, v20.clone(), v30];
        let resolved = resolve(&versions, TxId::new(25));
        match resolved {
            Resolved::Live(r) => assert_eq!(r, &v20),
            other => panic!("expected Live(v20), got {other:?}"),
        }
    }

    #[test]
    fn tombstone_deletes_when_snapshot_reaches_it() {
        let id = EntityId::now_v7();
        let v10 = entity_v(id, 10);
        let tomb = tombstone(id.into_uuid(), 20);
        let versions = vec![v10.clone(), tomb];
        assert_eq!(
            resolve(&versions, TxId::new(15)),
            Resolved::Live(&v10),
            "tombstone not yet active at S=15"
        );
        assert!(matches!(
            resolve(&versions, TxId::new(20)),
            Resolved::Deleted { .. }
        ));
        assert!(matches!(
            resolve(&versions, TxId::new(100)),
            Resolved::Deleted { .. }
        ));
    }

    #[test]
    fn reassert_after_tombstone_revives_entity() {
        let id = EntityId::now_v7();
        let v10 = entity_v(id, 10);
        let tomb = tombstone(id.into_uuid(), 20);
        let v30 = entity_v(id, 30);
        let versions = vec![v10, tomb, v30.clone()];
        match resolve(&versions, TxId::new(30)) {
            Resolved::Live(r) => assert_eq!(r, &v30),
            other => panic!("expected Live(v30), got {other:?}"),
        }
        // At snapshot=25, still deleted.
        assert!(matches!(
            resolve(&versions, TxId::new(25)),
            Resolved::Deleted { .. }
        ));
    }

    #[test]
    fn read_your_own_writes_within_a_transaction() {
        // Snapshot = writer's own tx_id; the writer's own record IS visible.
        let id = EntityId::now_v7();
        let own = entity_v(id, 42);
        match resolve(std::slice::from_ref(&own), TxId::new(42)) {
            Resolved::Live(r) => assert_eq!(r, &own),
            other => panic!("expected Live, got {other:?}"),
        }
    }

    #[test]
    fn dictionary_records_visible_to_all_snapshots() {
        let rec = Record::HyperEdge(HyperEdgeRecord {
            hyperedge_id: HyperedgeId::now_v7(),
            type_id: TypeId::new(7),
            tx_id_assert: TxId::new(50),
            tx_id_supersede: TxId::ACTIVE,
            roles: vec![(RoleId::new(1), EntityId::now_v7())],
            properties: vec![],
        });
        // A hyperedge with tx_assert=50 is invisible at snapshot 49, visible
        // at snapshot 50.
        assert!(!visible_at(&rec, TxId::new(49)));
        assert!(visible_at(&rec, TxId::new(50)));
        assert!(visible_at(&rec, TxId::new(51)));
        // Dictionary record: timeless.
        let dict = Record::TypeName(crate::record::TypeNameRecord {
            id: TypeId::new(1),
            name: "X".into(),
        });
        assert!(visible_at(&dict, TxId::new(0)));
    }

    #[test]
    fn empty_versions_resolve_to_missing() {
        assert_eq!(resolve(&[], TxId::new(1)), Resolved::Missing);
    }

    #[test]
    fn tombstone_wins_tie_break_when_same_tx() {
        let id = EntityId::now_v7();
        // Pathological: both records have effective_tx == 10. Tombstone wins.
        let live = entity_v(id, 10);
        let tomb = tombstone(id.into_uuid(), 10);
        let versions = vec![live, tomb];
        assert!(matches!(
            resolve(&versions, TxId::new(10)),
            Resolved::Deleted { .. }
        ));
    }

    #[test]
    fn assert_v1_supersede_invariant_holds_for_fresh_writes() {
        let live = entity_v(EntityId::now_v7(), 1);
        assert!(assert_v1_supersede_invariant(&live));
        // Hand-craft a record violating the invariant.
        let bad = Record::Entity(EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(1),
            tx_id_supersede: TxId::new(2), // not TX_ACTIVE
            properties: vec![],
        });
        assert!(!assert_v1_supersede_invariant(&bad));
    }
}
