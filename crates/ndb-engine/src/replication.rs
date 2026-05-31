//! Log-shipping replication primitives (§ leader/follower, P3).
//!
//! nDB replicates the PostgreSQL way: a **base backup** bootstraps a
//! follower ([`Engine::backup_to`](crate::Engine::backup_to)), then the
//! leader **streams its committed WAL records** and the follower appends
//! them to its own WAL. Recovery on the follower replays that WAL through
//! the exact same [`WalRecovery`](crate::wal::WalRecovery) path used after
//! a crash.
//!
//! Why this is correct by construction:
//!
//! - The engine's commit path writes records to the WAL **without
//!   re-stamping their tx ids** (see `WriteTxn::commit`), and the WAL layer
//!   ([`WriteAheadLog::append_batch`](crate::wal::WriteAheadLog::append_batch))
//!   re-encodes them verbatim. So a record shipped to a follower carries the
//!   leader's original `tx_id_assert` / `tx_id_supersede` and its companion
//!   `TxTimestamp`. The replica's MVCC view is therefore byte-for-byte
//!   identical to the leader's — no divergent tx numbering, no clock skew.
//! - The follower never invents tx ids: it only appends bytes the leader
//!   already made durable. Replay is the same well-tested code as crash
//!   recovery, so there is no second, subtly-different apply path to get
//!   wrong.
//!
//! Watermark: positions are **WAL byte offsets** (the `lsn` returned by
//! [`crate::wal::WalReader::next_record`]). A follower whose WAL is `L`
//! bytes long has consumed everything below `L`; it asks the leader for
//! records at offset `≥ L`. Because the follower's WAL is a byte-for-byte
//! copy of the leader's (from the base backup, then identical appends),
//! the offsets line up.
//!
//! Scope / limitations (honest):
//!
//! - This module is the **mechanism** (read deltas, apply deltas), not a
//!   running daemon. The network hop (a leader `/replicate?after=<lsn>`
//!   endpoint and a follower poll loop) is wired in the server layer.
//! - Catch-up is **WAL-segment-aligned**: it streams within the active WAL
//!   segment. When the leader flushes and rotates its WAL, the follower
//!   re-syncs the newly-sealed SSTable via a base-backup copy (the SSTables
//!   are immutable, so this is cheap and safe) and resumes streaming on the
//!   new segment. A continuous cross-rotation cursor is future work.
#![allow(clippy::doc_markdown)]

use std::path::Path;

use crate::encryption::Cipher;
use crate::record::Record;
use crate::wal::{WalReadError, WalReader, WriteAheadLog};

/// A batch of committed records read from a leader WAL, plus the byte
/// offset a follower should request from next time (its new watermark).
#[derive(Debug)]
pub struct ReplicationBatch {
    /// Committed records at offset `≥ after`, in WAL order. Includes the
    /// `TxTimestamp` markers so `as of "<timestamp>"` queries replicate too.
    pub records: Vec<Record>,
    /// Byte offset to pass as `after` on the next call — the durable length
    /// of the leader WAL at read time. Records torn at the tail are excluded
    /// and will be picked up once the leader makes them durable.
    pub next_offset: u64,
}

impl ReplicationBatch {
    /// Whether the follower is already caught up (no new records).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// **Leader side** — read every committed record in `wal_path` whose start
/// offset is `≥ after`. Pass `after = 0` for a full read, or the
/// `next_offset` from the previous batch to stream only the delta.
///
/// `cipher` must match the leader's at-rest cipher (`None` for plaintext).
/// A torn trailing record is treated as not-yet-durable and excluded; its
/// bytes are re-read once the leader finishes writing them.
pub fn read_wal_since(
    wal_path: &Path,
    cipher: Option<Cipher>,
    after: u64,
) -> Result<ReplicationBatch, WalReadError> {
    let mut reader = WalReader::open_with_cipher(wal_path, cipher)?;
    let mut records = Vec::new();
    while let Some((record, lsn)) = reader.next_record()? {
        if lsn >= after {
            records.push(record);
        }
    }
    Ok(ReplicationBatch {
        records,
        next_offset: reader.pos(),
    })
}

/// **Follower side** — append a replicated batch to the follower's active
/// WAL and make it durable. The records are written verbatim (tx ids
/// preserved); a subsequent [`Engine::open`](crate::Engine::open) replays
/// them into the follower's state.
///
/// Returns the follower WAL length after the append — the follower's new
/// watermark, which should equal the leader's `next_offset` when the two
/// WALs are byte-aligned.
pub fn apply_batch(
    follower_wal: &mut WriteAheadLog,
    batch: &ReplicationBatch,
) -> std::io::Result<u64> {
    if !batch.records.is_empty() {
        follower_wal.append_batch(&batch.records)?;
        follower_wal.sync()?;
    }
    Ok(follower_wal.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::id::{EntityId, PropertyId, TxId, TypeId};
    use crate::mvcc::Resolved;
    use crate::record::EntityRecord;
    use crate::value::Value;
    use crate::wal::WAL_EXTENSION;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("ndb-repl-{}-{}", tag, uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn put(engine: &mut Engine, eid: EntityId, name: &str) -> TxId {
        let mut txn = engine.begin_write();
        txn.put_entity(EntityRecord {
            entity_id: eid,
            type_id: TypeId::new(1),
            tx_id_assert: TxId::ACTIVE,
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(1), Value::String(name.into()))],
        });
        txn.commit().unwrap()
    }

    fn wal_path(dir: &Path, seq: u64) -> std::path::PathBuf {
        dir.join(format!("{seq:06}.{WAL_EXTENSION}"))
    }

    #[test]
    fn base_backup_plus_wal_stream_builds_a_consistent_replica() {
        let leader_dir = temp_dir("leader");
        let follower_dir = temp_dir("follower");
        let (a, b, c) = (EntityId::now_v7(), EntityId::now_v7(), EntityId::now_v7());
        let (ta, _tb, tc);

        let mut leader = Engine::create(&leader_dir).unwrap();
        // 1. Leader writes A.
        ta = put(&mut leader, a, "alice");

        // 2. Bootstrap the follower with a base backup (captures A in the
        //    copied WAL). The follower's WAL is now a byte-for-byte copy of
        //    the leader's, so its length is the streaming watermark.
        leader.backup_to(&follower_dir).unwrap();
        let seq = leader.manifest().active_wal_seq;
        let watermark = std::fs::metadata(wal_path(&follower_dir, seq)).unwrap().len();

        // 3. Leader keeps writing B and C (same WAL segment, no flush).
        _tb = put(&mut leader, b, "bob");
        tc = put(&mut leader, c, "carol");

        // 4. Stream the delta: read the leader WAL from the watermark…
        let batch = read_wal_since(&wal_path(&leader_dir, seq), None, watermark).unwrap();
        assert!(!batch.is_empty(), "B and C must be streamed");

        // …and apply it to the follower's WAL.
        {
            let mut fwal = WriteAheadLog::open_append(wal_path(&follower_dir, seq)).unwrap();
            let new_wm = apply_batch(&mut fwal, &batch).unwrap();
            fwal.close().unwrap();
            assert_eq!(new_wm, batch.next_offset, "follower WAL must align with leader");
        }
        leader.close().unwrap();

        // 5. Open the follower — all three entities are visible at tc, with
        //    the leader's own tx ids preserved.
        let follower = Engine::open(&follower_dir).unwrap();
        for (eid, who) in [(a, "alice"), (b, "bob"), (c, "carol")] {
            match follower.snapshot_read(&eid.into_uuid(), tc).unwrap() {
                Resolved::Live(Record::Entity(e)) => assert_eq!(e.entity_id, eid, "{who}"),
                other => panic!("replica read of {who}: {other:?}"),
            }
        }
        // A's record on the replica still carries the leader's tx id.
        match follower.snapshot_read(&a.into_uuid(), ta).unwrap() {
            Resolved::Live(Record::Entity(e)) => {
                assert_eq!(e.tx_id_assert, ta, "leader tx id preserved on replica");
            }
            other => panic!("replica tx-id check: {other:?}"),
        }
        follower.close().unwrap();

        std::fs::remove_dir_all(&leader_dir).unwrap();
        std::fs::remove_dir_all(&follower_dir).unwrap();
    }

    #[test]
    fn read_wal_since_watermark_is_resumable() {
        let dir = temp_dir("resume");
        let mut engine = Engine::create(&dir).unwrap();
        let seq = engine.manifest().active_wal_seq;
        let e1 = EntityId::now_v7();
        put(&mut engine, e1, "one");

        // Full read from 0 captures e1; the returned next_offset, used as
        // the next `after`, yields an empty batch (nothing new).
        let first = read_wal_since(&wal_path(&dir, seq), None, 0).unwrap();
        assert!(!first.is_empty());
        let again = read_wal_since(&wal_path(&dir, seq), None, first.next_offset).unwrap();
        assert!(again.is_empty(), "no new records past the watermark");

        // A further write is picked up from the same watermark.
        put(&mut engine, EntityId::now_v7(), "two");
        let delta = read_wal_since(&wal_path(&dir, seq), None, first.next_offset).unwrap();
        assert!(!delta.is_empty(), "second write must appear in the delta");

        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
