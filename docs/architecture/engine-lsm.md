# nDB Engine — LSM Storage Core

The `ndb-engine` crate is a self-contained, `unsafe`-free, append-only
log-structured merge (LSM) store. This diagram traces a write from
`commit()` through the WAL into the memtable, its flush to immutable
mmap'd SSTables, size-tiered compaction catalogued by the manifest, and
the snapshot read path resolved through MVCC.

Source: `crates/ndb-engine/src/` — `wal.rs`, `memtable.rs`, `sstable.rs`,
`block_index.rs`, `db.rs` (manifest), `mvcc.rs`, `codec.rs`,
`encryption.rs`.

```mermaid
flowchart TB
    %% ── Write entry ────────────────────────────────────────────
    subgraph API["Write API · engine.rs"]
        direction TB
        BW["begin_write → WriteTxn&lt;'a&gt;<br/>buffer put_entity · put_hyperedge · delete"]
        CM["commit → assign monotonic TxId"]
        BW --> CM
    end

    %% ── Durability ─────────────────────────────────────────────
    subgraph DUR["1 · Durability · wal.rs"]
        direction TB
        WAL["WriteAheadLog.append<br/>+ fsync"]
        WALF[("*.wal on disk")]
        WAL --> WALF
        REC["WalRecovery · WalReader<br/>replay on restart"]
        WALF -.->|crash recovery| REC
    end

    %% ── In-memory ──────────────────────────────────────────────
    subgraph MEM["2 · In-memory · memtable.rs"]
        direction TB
        MT["Memtable<br/>ordered map: Id+TxId → Record<br/>keeps MVCC versions"]
    end

    %% ── On-disk immutable ──────────────────────────────────────
    subgraph DISK["3 · On-disk immutable · sstable.rs"]
        direction TB
        SW["SSTableWriter<br/>sorted blocks + SSTableFooter"]
        BIX["block_index.rs<br/>block offsets"]
        ENC["encryption.rs<br/>optional AES-256-GCM at rest"]
        SST[("SSTable files")]
        SR["SSTableReader<br/>mmap via memmap2<br/>CRC32 header/index only — lazy pages"]
        SW --> BIX --> SST
        SW --> ENC --> SST
        SST --> SR
    end

    %% ── Catalog ────────────────────────────────────────────────
    subgraph CAT["4 · Catalog · db.rs"]
        direction TB
        MAN["Manifest of ManifestEntry + seq<br/>atomic swap on compaction"]
        MANF[("MANIFEST file")]
        MAN --> MANF
    end

    %% ── Read path ──────────────────────────────────────────────
    subgraph READ["Read path · mvcc.rs"]
        direction TB
        RD["read · iter · lookup<br/>at snapshot TxId"]
        MV["resolve · visible_at<br/>newest version with effective_tx ≤ snapshot<br/>tombstones supersede"]
        OUT["Resolved&lt;Record&gt; → caller"]
        RD --> MV --> OUT
    end

    %% ── Flows ──────────────────────────────────────────────────
    CM --> WAL
    WAL --> MT
    MT -->|flush when full| SW
    SST -->|compact · size-tiered| SW
    SST --> MAN
    MANF -.->|live SSTable set| SR

    MT -.->|newest versions| MV
    SR -.->|merge oldest→newest| MV

    %% ── Cross-cut ──────────────────────────────────────────────
    CODEC["codec.rs — one binary record format<br/>shared by WAL · memtable · SSTable"]
    CODEC -.-> WAL
    CODEC -.-> MT
    CODEC -.-> SW

    classDef store fill:#e8f0fe,stroke:#3367d6,color:#0b2a6b;
    classDef file fill:#fff3e0,stroke:#e8870c,color:#5a3000;
    classDef xcut fill:#f3e8fd,stroke:#7b1fa2,color:#3a0a55;
    class BW,CM,WAL,REC,MT,SW,BIX,ENC,SR,MAN,RD,MV,OUT store;
    class WALF,SST,MANF file;
    class CODEC,ENC xcut;
```

## Reading the diagram

- **Write order is durability-first.** `commit()` assigns a monotonic
  `TxId`, the WAL appends + `fsync`s *before* the mutation enters the
  memtable, so a crash replays from `*.wal` (`WalRecovery`).
- **The memtable is the only mutable structure.** It holds MVCC versions
  keyed by `(Id, TxId)`; once full it flushes to an immutable SSTable.
- **SSTables are append-only and mmap'd.** `SSTableReader` CRC-checks only
  the footer/block index, leaving entry pages to lazy fault-in
  (`memmap2`) — this is what keeps RSS bounded on 10 GB+ datasets.
  Encryption at rest (`AES-256-GCM`) is an optional layer on the same file.
- **Compaction is size-tiered** and re-enters the same `SSTableWriter`;
  the `Manifest` catalogues the live set and is swapped atomically.
- **Reads are lock-free snapshots.** `mvcc::resolve()` merges the memtable
  and the SSTable set oldest→newest, keeping the newest version with
  `effective_tx ≤ snapshot` and honouring tombstones — enabling
  time-travel to any past `TxId`.
- **`codec.rs` is the single binary record format** shared across WAL,
  memtable, and SSTable, so encoding is identical end to end.
