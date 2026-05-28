//! End-to-end demonstration of the nDB v1 engine.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]
//!
//! What this example shows:
//!   1. Create a fresh database directory.
//!   2. Set up domain identifiers (TypeIds, RoleIds, PropertyIds).
//!   3. Register a lookup-key (email) and a validation constraint
//!      (required property + value-tag).
//!   4. Insert entities (Alice, Bob) and a hyperedge (Approval).
//!   5. Read by UUID with snapshot isolation.
//!   6. Look up by external key (email).
//!   7. Traverse adjacency (Alice → approvals).
//!   8. Group by hyperedge type.
//!   9. Tombstone an entity.
//!  10. Flush + compact.
//!  11. Close the engine.
//!  12. Re-open and verify state persisted.
//!
//! Run with:
//!   cargo run -p ndb-engine --example basic

use ndb_engine::record::Record;
use ndb_engine::{
    Engine, EntityId, EntityRecord, HyperEdgeRecord, HyperedgeId, PropertyId, Resolved, RoleId,
    TxId, TypeId, Value, value::TAG_STRING,
};

fn header(label: &str) {
    println!("\n{:━<70}", "");
    println!("┃ {label}");
    println!("{:━<70}", "");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Create a fresh database directory under /tmp.
    let dir = std::env::temp_dir().join(format!("ndb-example-{}", uuid::Uuid::now_v7().simple()));
    println!("nDB example database: {}", dir.display());

    // 2. Domain identifiers — these would live in metadata hyperedges
    //    in a real app; here we hard-code them for the example.
    let customer_type = TypeId::new(1);
    let approval_type = TypeId::new(100);
    let email_prop = PropertyId::new(10);
    let name_prop = PropertyId::new(11);
    let approver_role = RoleId::new(1);
    let request_role = RoleId::new(2);

    header("Create + configure engine");
    let mut engine = Engine::create(&dir)?;
    // 3. Validation + lookup-key constraints.
    engine.require_property(customer_type, email_prop);
    engine.expect_value_tag(customer_type, email_prop, TAG_STRING);
    engine.register_lookup_key(email_prop);
    println!("constraints registered: email required + TAG_STRING; email is a lookup key");

    header("Insert Alice + Bob (entities)");
    let alice = EntityId::now_v7();
    let bob = EntityId::now_v7();
    {
        let mut txn = engine.begin_write();
        txn.put_entity(EntityRecord {
            entity_id: alice,
            type_id: customer_type,
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![
                (name_prop, Value::String("Alice".into())),
                (email_prop, Value::String("alice@example.com".into())),
            ],
        });
        txn.put_entity(EntityRecord {
            entity_id: bob,
            type_id: customer_type,
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![
                (name_prop, Value::String("Bob".into())),
                (email_prop, Value::String("bob@example.com".into())),
            ],
        });
        let tx = txn.commit()?;
        println!("committed entities at tx={}", tx.get());
    }

    header("Demonstrate validation rejecting a bad commit");
    {
        let mut txn = engine.begin_write();
        // No email — required property missing.
        txn.put_entity(EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: customer_type,
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(name_prop, Value::String("Carol".into()))],
        });
        match txn.commit() {
            Ok(_) => panic!("validation should have rejected the commit"),
            Err(e) => println!("rejected as expected: {e}"),
        }
    }

    header("Approval hyperedge: Alice approves request 42");
    let request_42 = EntityId::now_v7();
    let approval = HyperedgeId::now_v7();
    {
        let mut txn = engine.begin_write();
        txn.put_entity(EntityRecord {
            entity_id: request_42,
            type_id: customer_type, // reusing for simplicity
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(email_prop, Value::String("request42@x.com".into()))],
        });
        txn.put_hyperedge(HyperEdgeRecord {
            hyperedge_id: approval,
            type_id: approval_type,
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            roles: vec![(approver_role, alice), (request_role, request_42)],
            hyperedge_roles: Vec::new(),
            properties: vec![],
        });
        let tx = txn.commit()?;
        println!("committed approval at tx={}", tx.get());
    }

    let snap = TxId::new(engine.manifest().last_tx_id);
    println!("\ncurrent snapshot tx={}", snap.get());

    header("Read Alice by UUID");
    match engine.snapshot_read(&alice.into_uuid(), snap)? {
        Resolved::Live(Record::Entity(e)) => {
            println!("Alice's record: {} properties", e.properties.len());
            for (p, v) in &e.properties {
                println!("  prop={} value={v:?}", p.get());
            }
        }
        other => println!("unexpected: {other:?}"),
    }

    header("Look up Alice by email (lookup-key index)");
    let by_email =
        engine.lookup_by_external_key(email_prop, &Value::String("alice@example.com".into()));
    println!(
        "email → entity_id: {by_email:?} (matches alice = {})",
        by_email == Some(alice)
    );

    header("Adjacency: which hyperedges involve Alice?");
    let alice_neighbors = engine.hyperedges_for_entity(alice);
    println!("Alice's hyperedges: {} found", alice_neighbors.len());
    for h in &alice_neighbors {
        println!("  {}", h.into_uuid());
    }

    header("Group by hyperedge type");
    let approvals = engine.hyperedges_by_type(approval_type);
    println!("All approvals: {}", approvals.len());

    header("Delete request 42");
    {
        let mut txn = engine.begin_write();
        txn.delete(request_42.into_uuid());
        txn.commit()?;
    }
    let snap = TxId::new(engine.manifest().last_tx_id);
    match engine.snapshot_read(&request_42.into_uuid(), snap)? {
        Resolved::Deleted { deleted_at } => {
            println!("request_42 deleted at tx={}", deleted_at.get());
        }
        other => println!("unexpected: {other:?}"),
    }

    header("Flush memtable to SSTable, then compact");
    engine.flush()?;
    let (mt_records, mt_bytes) = engine.memtable_stats();
    println!(
        "after flush: memtable={mt_records} records ({mt_bytes} bytes), {} SSTables",
        engine.sstable_count()
    );

    // Force a second flush to give compaction something to merge.
    {
        let mut txn = engine.begin_write();
        txn.put_entity(EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: customer_type,
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(email_prop, Value::String("dave@example.com".into()))],
        });
        txn.commit()?;
    }
    engine.flush()?;
    println!("after second flush: {} SSTables", engine.sstable_count());
    let stats = engine.compact()?;
    println!(
        "compaction: records_in={}, records_out={}, sstables_in={}, new_seq={:?}",
        stats.records_in, stats.records_out, stats.sstables_in, stats.new_sstable_seq
    );
    println!("after compact: {} SSTables", engine.sstable_count());

    header("Close, reopen, verify state persisted");
    engine.close()?;
    let mut engine = Engine::open(&dir)?;
    // After restart, lookup-key registrations + validation constraints are
    // wiped (in-memory only in v1). Re-register if you need them, but the
    // primary store carries all the data.
    engine.register_lookup_key(email_prop);
    engine.rebuild_indexes()?;
    let snap = TxId::new(engine.manifest().last_tx_id);
    match engine.snapshot_read(&alice.into_uuid(), snap)? {
        Resolved::Live(Record::Entity(_)) => println!("✓ Alice still alive after restart"),
        other => println!("Alice lookup after restart: {other:?}"),
    }
    let alice_email_again =
        engine.lookup_by_external_key(email_prop, &Value::String("alice@example.com".into()));
    println!(
        "✓ email lookup after restart: {} (== alice: {})",
        alice_email_again.is_some(),
        alice_email_again == Some(alice)
    );
    let approvals_after = engine.hyperedges_by_type(approval_type);
    println!("✓ approvals after restart: {}", approvals_after.len());

    engine.close()?;
    // Optional cleanup; comment out to inspect on-disk layout.
    std::fs::remove_dir_all(&dir)?;
    println!("\nexample done.");
    Ok(())
}
