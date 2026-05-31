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

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use thiserror::Error;

use crate::encryption::Cipher;
use crate::engine::{Engine, EngineError};
use crate::error::DecodeError;
use crate::record::Record;
use crate::wal::{WalReadError, WalReader, WriteAheadLog};

/// A follower's stream position: which WAL segment + byte offset to request
/// next from the leader. Initialised from a base backup (the leader's active
/// `wal_seq`, offset = the backed-up WAL length), then advanced by
/// [`poll_once`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FollowerCursor {
    /// Leader WAL segment being streamed.
    pub wal_seq: u64,
    /// Byte offset within that segment already consumed.
    pub offset: u64,
}

/// One batch as a follower receives it, after the transport (e.g. HTTP
/// `/replicate`) has been decoded.
#[derive(Debug)]
pub struct StreamedBatch {
    /// The leader's current active `wal_seq`.
    pub current_wal_seq: u64,
    /// Whether the requested segment still exists on the leader. `false`
    /// means it was pruned past the archive window — the follower fell too
    /// far behind and must re-bootstrap from a fresh base backup.
    pub available: bool,
    /// Whether the requested segment is sealed (no longer the active one).
    /// A sealed segment, once drained, is followed by [`next_wal_seq`].
    pub segment_sealed: bool,
    /// The next WAL segment to advance to after draining a sealed one. WAL
    /// seqs are non-contiguous, so the leader supplies this. `None` when the
    /// requested segment is the newest.
    pub next_wal_seq: Option<u64>,
    /// Records to ingest (decoded; may be empty when caught up on a segment).
    pub records: Vec<Record>,
    /// Offset to request from next time within the current segment.
    pub next_offset: u64,
}

/// Result of a single [`poll_once`] step.
#[derive(Debug, PartialEq, Eq)]
pub enum PollOutcome {
    /// Ingested this many records; the cursor advanced. `0` means caught up
    /// (the loop should sleep before polling again).
    Applied(usize),
    /// The leader rotated past the follower's segment; re-bootstrap from a
    /// base backup, then resume with a fresh cursor at the new `wal_seq`.
    Rotated {
        /// The leader's current active `wal_seq`.
        current_wal_seq: u64,
    },
}

/// **Follower daemon step.** Fetch one batch via `fetch` (the caller's
/// transport — HTTP `POST /replicate`, in-process, or a test closure),
/// ingest it into `engine` preserving the leader's tx ids, and advance
/// `cursor`. The operator's daemon loop is just:
///
/// ```ignore
/// loop {
///     match poll_once(&mut engine, &mut cursor, fetch)? {
///         PollOutcome::Rotated { .. } => { /* re-bootstrap from base backup */ }
///         PollOutcome::Applied(0)     => std::thread::sleep(poll_interval),
///         PollOutcome::Applied(_)     => {} // more may be waiting — poll again
///     }
/// }
/// ```
///
/// Keeping the transport in a closure means the engine takes no network
/// dependency and the loop is deterministically testable in-process.
pub fn poll_once<F>(
    engine: &mut Engine,
    cursor: &mut FollowerCursor,
    fetch: F,
) -> Result<PollOutcome, EngineError>
where
    F: FnOnce(&FollowerCursor) -> Result<StreamedBatch, EngineError>,
{
    let batch = fetch(cursor)?;
    if !batch.available {
        return Ok(PollOutcome::Rotated {
            current_wal_seq: batch.current_wal_seq,
        });
    }
    let n = batch.records.len();
    engine.ingest_replicated(batch.records)?;
    cursor.offset = batch.next_offset;
    // A sealed segment, once fully drained (no new records), is followed by
    // the next segment — advance to it so streaming continues across the
    // leader's flush/rotation without a re-bootstrap.
    if n == 0 && batch.segment_sealed {
        if let Some(next) = batch.next_wal_seq {
            cursor.wal_seq = next;
            cursor.offset = 0;
        }
    }
    Ok(PollOutcome::Applied(n))
}

/// Error decoding a replicated record batch off the wire.
#[derive(Debug, Error)]
pub enum BatchDecodeError {
    /// The transport string wasn't valid base64.
    #[error(transparent)]
    Base64(#[from] base64::DecodeError),
    /// The decoded bytes didn't form valid records.
    #[error(transparent)]
    Record(#[from] DecodeError),
}

/// Encode a batch of records to a self-delimiting byte blob (concatenated
/// `Record::encode` outputs — each record is length-described by its own
/// envelope, so the blob is walkable with repeated `Record::decode`).
#[must_use]
pub fn encode_records(records: &[Record]) -> Vec<u8> {
    let mut out = Vec::new();
    for r in records {
        r.encode(&mut out).expect("record encode is infallible for valid records");
    }
    out
}

/// Decode a record blob produced by [`encode_records`].
pub fn decode_records(bytes: &[u8]) -> Result<Vec<Record>, DecodeError> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let (r, consumed) = Record::decode(&bytes[pos..])?;
        out.push(r);
        pos += consumed;
    }
    Ok(out)
}

/// Base64-encode a record batch for a JSON transport (the `/replicate`
/// response body). Carries every record kind verbatim — including the
/// `TxTimestamp`/`RetentionPolicy` metadata replication needs — unlike the
/// user-facing change-feed wire.
#[must_use]
pub fn encode_records_b64(records: &[Record]) -> String {
    BASE64.encode(encode_records(records))
}

/// Decode a base64 record batch produced by [`encode_records_b64`].
pub fn decode_records_b64(s: &str) -> Result<Vec<Record>, BatchDecodeError> {
    let bytes = BASE64.decode(s)?;
    Ok(decode_records(&bytes)?)
}

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
    use crate::engine::{Engine, EngineConfig};
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
    fn continuous_streaming_follower_mirrors_the_leader() {
        // The daemon's core loop, library-level: bootstrap a follower from a
        // base backup, then repeatedly stream the leader's WAL delta and
        // ingest it into the live follower engine. The replica must mirror the
        // leader — same entities, leader tx ids preserved — and survive a
        // reopen (its own WAL is valid + replayable).
        let leader_dir = temp_dir("stream-leader");
        let follower_dir = temp_dir("stream-follower");
        let mut leader = Engine::create(&leader_dir).unwrap();

        // Bootstrap: one commit, base-backup → follower.
        let id0 = EntityId::now_v7();
        put(&mut leader, id0, "init");
        leader.backup_to(&follower_dir).unwrap();
        let mut follower = Engine::open(&follower_dir).unwrap();

        // Streaming starts at the leader WAL length captured at backup time
        // (the follower already has everything before it, via the copied WAL).
        let seq = leader.active_wal_seq();
        let mut watermark = std::fs::metadata(wal_path(&leader_dir, seq)).unwrap().len();

        let mut ids = vec![id0];
        for round in 0..6 {
            let id = EntityId::now_v7();
            put(&mut leader, id, &format!("r{round}"));
            ids.push(id);

            // Follower pulls the delta and ingests it into the live engine.
            let batch = leader.wal_delta_since(watermark).unwrap();
            assert!(!batch.is_empty(), "round {round} should stream new records");
            follower.ingest_replicated(batch.records).unwrap();
            watermark = batch.next_offset;
        }

        // Replica state matches the leader at the latest tx, tx ids preserved.
        let leader_tx = leader.manifest().last_tx_id;
        assert_eq!(
            follower.manifest().last_tx_id,
            leader_tx,
            "follower tx watermark must track the leader"
        );
        let snap = TxId::new(leader_tx);
        for id in &ids {
            match follower.snapshot_read(&id.into_uuid(), snap).unwrap() {
                Resolved::Live(Record::Entity(_)) => {}
                other => panic!("replica missing {id:?}: {other:?}"),
            }
        }
        // First entity on the replica still carries the leader's tx id.
        let leader_a = match leader.snapshot_read(&id0.into_uuid(), snap).unwrap() {
            Resolved::Live(Record::Entity(e)) => e.tx_id_assert,
            o => panic!("leader read: {o:?}"),
        };
        match follower.snapshot_read(&id0.into_uuid(), snap).unwrap() {
            Resolved::Live(Record::Entity(e)) => assert_eq!(e.tx_id_assert, leader_a),
            o => panic!("replica read: {o:?}"),
        }
        follower.close().unwrap();

        // The replica's own WAL is valid: reopen replays it cleanly.
        let reopened = Engine::open(&follower_dir).unwrap();
        for id in &ids {
            assert!(matches!(
                reopened.snapshot_read(&id.into_uuid(), snap).unwrap(),
                Resolved::Live(Record::Entity(_))
            ));
        }
        reopened.close().unwrap();
        leader.close().unwrap();

        std::fs::remove_dir_all(&leader_dir).unwrap();
        std::fs::remove_dir_all(&follower_dir).unwrap();
    }

    #[test]
    fn follower_streams_continuously_across_wal_rotations() {
        // The cross-rotation cursor: with WAL archiving on, the leader flushes
        // (rotating the WAL) WHILE the follower is streaming. The follower must
        // drain the sealed segment, advance to the next one, and lose nothing —
        // no re-bootstrap. Drives the real poll_once loop with serve_replication.
        let leader_dir = temp_dir("xrot-leader");
        let follower_dir = temp_dir("xrot-follower");
        let cfg = EngineConfig {
            wal_archive_segments: 3,
            ..EngineConfig::default()
        };
        let mut leader = Engine::create_with_config(&leader_dir, cfg).unwrap();
        let mut follower = Engine::create(&follower_dir).unwrap();

        let mut cursor = FollowerCursor {
            wal_seq: leader.active_wal_seq(),
            offset: 0,
        };
        let mut ids = Vec::new();

        // Interleave commits with flushes so the WAL rotates several times
        // under the follower; the follower polls after each step.
        for round in 0..4 {
            for j in 0..3 {
                let id = EntityId::now_v7();
                put(&mut leader, id, &format!("r{round}_{j}"));
                ids.push(id);
            }
            // Flush → WAL rotates; the just-streamed segment becomes archived.
            leader.flush().unwrap();

            // Drain whatever is available (loops, advancing across the rotation).
            let mut guard = 0;
            loop {
                guard += 1;
                assert!(guard < 50, "follower should converge");
                let leader_ref = &leader;
                let before = cursor;
                let outcome = poll_once(&mut follower, &mut cursor, |c| {
                    leader_ref.serve_replication(c.wal_seq, c.offset)
                })
                .unwrap();
                match outcome {
                    PollOutcome::Rotated { .. } => panic!("archive window should prevent re-bootstrap"),
                    // Stop when a poll made no progress (no records + no segment advance).
                    PollOutcome::Applied(0) if cursor == before => break,
                    _ => {}
                }
            }
        }

        // The replica has every committed entity despite all the rotations.
        let snap = TxId::new(leader.manifest().last_tx_id);
        for id in &ids {
            assert!(
                matches!(
                    follower.snapshot_read(&id.into_uuid(), snap).unwrap(),
                    Resolved::Live(Record::Entity(_))
                ),
                "replica lost {id:?} across rotation"
            );
        }
        // The leader kept multiple sealed WAL segments (archiving on).
        assert!(leader.wal_segments().len() >= 2, "archived segments retained");

        leader.close().unwrap();
        follower.close().unwrap();
        std::fs::remove_dir_all(&leader_dir).unwrap();
        std::fs::remove_dir_all(&follower_dir).unwrap();
    }

    #[test]
    fn follower_daemon_loop_via_poll_once_catches_up() {
        // Drives the reusable poll_once daemon step in a loop, with the
        // transport closure streaming from an in-process leader — exactly the
        // operator's loop shape, deterministically tested.
        let leader_dir = temp_dir("daemon-leader");
        let follower_dir = temp_dir("daemon-follower");
        let mut leader = Engine::create(&leader_dir).unwrap();
        let mut follower = Engine::create(&follower_dir).unwrap();

        let mut cursor = FollowerCursor {
            wal_seq: leader.active_wal_seq(),
            offset: 0,
        };
        let mut ids = Vec::new();
        for i in 0..5 {
            let id = EntityId::now_v7();
            put(&mut leader, id, &format!("e{i}"));
            ids.push(id);
        }

        // Loop poll_once until caught up.
        let mut polls = 0;
        loop {
            polls += 1;
            assert!(polls < 10, "should catch up quickly");
            let leader_ref = &leader;
            let outcome =
                poll_once(&mut follower, &mut cursor, |c| {
                    leader_ref.serve_replication(c.wal_seq, c.offset)
                })
                .unwrap();
            match outcome {
                PollOutcome::Applied(0) => break,
                PollOutcome::Applied(_) => {}
                PollOutcome::Rotated { .. } => panic!("no rotation expected"),
            }
        }

        let snap = TxId::new(leader.manifest().last_tx_id);
        for id in &ids {
            assert!(
                matches!(
                    follower.snapshot_read(&id.into_uuid(), snap).unwrap(),
                    Resolved::Live(Record::Entity(_))
                ),
                "replica missing {id:?}"
            );
        }
        leader.close().unwrap();
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
