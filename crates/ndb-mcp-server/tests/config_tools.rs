//! First-class config tools (Gap 4).
//!
//! * `ndb.register_vector_property` replaces the `NDB_VECTOR_PROP` env knob:
//!   vector search works on a fresh DB with no env set, and the registration
//!   persists across a restart (the `.vector_props` sidecar is reloaded on
//!   open and the in-memory HNSW index rebuilt).
//! * `ndb.define_schema` names type/role/property dictionary slots so the
//!   catalog (`ndb://dictionaries`) shows real names instead of `kind:N`.

use ndb_mcp_server::McpServer;
use serde_json::Value;

fn temp_db() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("ndb-mcp-cfg-{}", uuid::Uuid::now_v7().simple()));
    p
}

fn call(server: &McpServer, line: &str) -> Value {
    serde_json::to_value(server.handle_line(line)).unwrap()
}

/// Read the JSON a resource block carries (resources/read → contents[0].text).
fn resource_json(server: &McpServer, uri: &str) -> Value {
    let v = call(
        server,
        &format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{{"uri":"{uri}"}}}}"#
        ),
    );
    let text = v["result"]["contents"][0]["text"].as_str().unwrap();
    serde_json::from_str(text).unwrap()
}

#[test]
fn vector_search_works_with_no_env_knob_and_survives_restart() {
    let dir = temp_db();
    let target;

    {
        let s = McpServer::open(&dir).unwrap();

        // Register property 20 as vector-indexed via the TOOL — no env knob.
        let reg = call(
            &s,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                "name":"ndb.register_vector_property","arguments":{"property_id":20}
            }}"#,
        );
        assert_eq!(reg["result"]["property_id"], 20);
        assert_eq!(reg["result"]["persisted"], true, "must persist: {reg}");

        // Commit a vector entity (auto-indexed since the property is registered)
        // plus a distractor pointing the other way.
        let commit = call(
            &s,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":"ndb.commit_entity","arguments":{"type_id":1,
                "properties":[{"prop_id":20,"value":{"tag":"vector","value":[1.0,0.0,0.0]}}]}
            }}"#,
        );
        target = commit["result"]["entity_id"].as_str().unwrap().to_owned();
        let _ = call(
            &s,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
                "name":"ndb.commit_entity","arguments":{"type_id":1,
                "properties":[{"prop_id":20,"value":{"tag":"vector","value":[0.0,1.0,0.0]}}]}
            }}"#,
        );

        // Search finds the target nearest — proving registration took effect
        // with no env knob.
        let r = call(
            &s,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{
                "name":"ndb.vector_search","arguments":{
                    "property_id":20,"query":[1.0,0.0,0.0],"k":1,"metric":"cosine"}
            }}"#,
        );
        let hits = r["result"]["hits"].as_array().unwrap();
        assert!(
            !hits.is_empty(),
            "fresh-DB vector search returned nothing: {r}"
        );
        assert_eq!(hits[0]["entity_id"], target, "wrong nearest: {r}");
    } // drop the server → engine dropped; commits are durable in the WAL.

    // Reopen WITHOUT re-registering and WITHOUT any env knob: the sidecar
    // reload + snapshot rebuild must make search work again.
    let s2 = McpServer::open(&dir).unwrap();
    let r = call(
        &s2,
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{
            "name":"ndb.vector_search","arguments":{
                "property_id":20,"query":[1.0,0.0,0.0],"k":1,"metric":"cosine"}
        }}"#,
    );
    let hits = r["result"]["hits"].as_array().unwrap();
    assert!(
        !hits.is_empty(),
        "after restart, vector search must work with no env knob / no re-register: {r}"
    );
    assert_eq!(
        hits[0]["entity_id"], target,
        "wrong nearest after restart: {r}"
    );

    drop(s2);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn define_schema_gives_the_catalog_real_names() {
    let dir = temp_db();
    let s = McpServer::open(&dir).unwrap();

    // Before naming: the catalog has no names for these slots.
    let before = resource_json(&s, "ndb://dictionaries");
    assert!(
        before["types"].as_array().unwrap().is_empty(),
        "fresh DB starts name-blind: {before}"
    );

    // Name three slots in one call.
    let d = call(
        &s,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"ndb.define_schema","arguments":{
                "types":[{"id":1,"name":"Person"},{"id":2,"name":"File"}],
                "roles":[{"id":1,"name":"author"}],
                "properties":[{"id":10,"name":"name"}]
            }
        }}"#,
    );
    assert_eq!(d["result"]["types"], 2, "{d}");
    assert_eq!(d["result"]["roles"], 1, "{d}");
    assert_eq!(d["result"]["properties"], 1, "{d}");

    // After: the catalog shows the real names, not `kind:N`.
    let cat = resource_json(&s, "ndb://dictionaries");
    let has = |arr: &str, id: u64, name: &str| {
        cat[arr]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["id"] == id && e["name"] == name)
    };
    assert!(has("types", 1, "Person"), "catalog missing Person: {cat}");
    assert!(has("types", 2, "File"), "catalog missing File: {cat}");
    assert!(has("roles", 1, "author"), "catalog missing author: {cat}");
    assert!(has("properties", 10, "name"), "catalog missing name: {cat}");

    drop(s);
    std::fs::remove_dir_all(&dir).ok();
}
