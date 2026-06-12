//! nDB Model Context Protocol server — bridge to AI agents.
#![warn(missing_docs)]
#![allow(
    clippy::doc_markdown,           // "MCP", "Engine", "SSTable" used liberally
    clippy::cast_possible_truncation, // f64 → f32 in vector query is intentional
    clippy::redundant_closure,      // map(|jv| jv.try_into()) needs the closure
    clippy::redundant_closure_for_method_calls,
    clippy::let_and_return,         // test helper readability
)]
//!
//! v1 MCP shape decisions:
//!
//! - **JSON-RPC 2.0 over stdio.** Standard MCP transport. Each line is
//!   a complete request or response (no line-counting / chunked
//!   framing in v1).
//! - **Embedded engine.** The server takes a `Path` and opens the
//!   database directly via `ndb_engine::Engine` — no HTTP hop. Agents
//!   that want network transport can fall back to ndb-server + the
//!   wire protocol.
//! - **Tools, resources, and prompts.** Tools are the action surface;
//!   `resources/*` expose read-only context blobs (`ndb://dictionaries`,
//!   `ndb://schema`, `ndb://stats`) so an agent can discover the schema
//!   without trial-and-error tool calls, and `prompts/*` ship query
//!   templates (`explore_entity`, `semantic_search`). Every tool carries a
//!   JSON-Schema `inputSchema`, and `ndb.iter` paginates with a cursor.
//!
//! Tools exposed (call via `tools/call` with `{"name": "<tool>",
//! "arguments": {...}}`):
//!
//! - `ndb.health` — `{}` → `{"status": "ok", ...}`
//! - `ndb.read` — `{"uuid": "..."}` → `ReadResponse`
//! - `ndb.commit_entity` — `{"type_id", "properties":[{prop_id, value}]}`
//!   → `{"tx_id", "entity_id"}`
//! - `ndb.commit_hyperedge` — `{"type_id", "roles":[{role_id, entity_id}],
//!   "hyperedge_roles"?:[{role_id, hyperedge_id}], "properties"?}` →
//!   `{"tx_id", "hyperedge_id"}`. Models an n-ary relationship as one record.
//! - `ndb.neighbors` — `{"uuid", "limit"?}` → `{"hyperedges":[...]}`. One-hop
//!   traversal: live hyperedges incident to an entity, with their role-fillers.
//! - `ndb.read_as_of` — `{"uuid", "as_of_tx"? | "as_of_timestamp_us"?}` →
//!   time-travel read at a past snapshot.
//! - `ndb.iter` — `{}` → array of records (capped at 1000 in v1; full
//!   streaming MCP shape is a v2 conversation)
//! - `ndb.lookup_by_key` — `{"property_id", "value"}` → entity uuid or null
//! - `ndb.vector_search` — `{"property_id", "query":[f32], "k", "metric"}`
//!   → list of `{entity_id, distance}` sorted ascending
//! - `ndb.property_lookup` — `{"type_id", "property_id", "value"}` →
//!   list of entity uuids (exact match)
//! - `ndb.property_range` — `{"type_id", "property_id", "low"?, "high"?}`
//!   → list of entity uuids (range)
//! - `ndb.arrow_export` — `{"batch_rows"?}` → base64 Arrow IPC stream of all
//!   records (GPU/analytics on-ramp; see `docs/gpu-dgx-spark.md`)
//! - `ndb.arrow_vectors` — `{"type_id", "property_id"}` → base64 Arrow IPC of a
//!   dense `FixedSizeList<Float32,dim>` embedding matrix (cuVS)
//! - `ndb.arrow_edge_index` — `{}` → base64 Arrow IPC hyperedge incidence list
//!   (cuGraph/PyG)
//!
//! All wire payloads use the same JSON shape as ndb-engine::wire.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ndb_engine::{
    Distance, Engine, EngineError, EntityId, EntityRecord, HyperEdgeRecord, HyperedgeId,
    JsonRecord, JsonValue, PropertyId, Record, Resolved, RoleId, TxId, TypeId, Value,
};
use ndb_server::{AuditEntry, AuditLog, Capability, Principal};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Errors raised by the MCP server.
#[derive(Debug, Error)]
pub enum McpError {
    /// I/O failure during stdio loop.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Engine-layer failure.
    #[error(transparent)]
    Engine(#[from] EngineError),
    /// JSON-RPC envelope was malformed.
    #[error("bad JSON-RPC: {0}")]
    BadRpc(&'static str),
    /// Tool name unknown.
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    /// Tool arguments were missing or wrong-shaped.
    #[error("bad arguments: {0}")]
    BadArgs(String),
    /// Underlying value/UUID conversion error.
    #[error("conversion: {0}")]
    Convert(String),
    /// Configured principal lacks the capability required for this tool.
    #[error("forbidden: principal '{principal}' lacks capability '{capability}'")]
    Forbidden {
        /// Principal name.
        principal: String,
        /// Required capability name.
        capability: &'static str,
    },
}

fn now_micros() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros())
}

/// Map an MCP tool name to the [`Capability`] required to call it.
fn tool_capability(tool: &str) -> Option<Capability> {
    match tool {
        "ndb.health" => Some(Capability::Health),
        "ndb.read"
        | "ndb.read_as_of"
        | "ndb.neighbors"
        | "ndb.lookup_by_key"
        | "ndb.vector_search"
        | "ndb.property_lookup"
        | "ndb.property_range"
        | "ndb.arrow_export"
        | "ndb.arrow_vectors"
        | "ndb.arrow_edge_index" => Some(Capability::Read),
        "ndb.iter" => Some(Capability::Iter),
        "ndb.commit_entity" | "ndb.commit_hyperedge" => Some(Capability::Commit),
        _ => None,
    }
}

fn capability_str(c: Capability) -> &'static str {
    match c {
        Capability::Health => "health",
        Capability::Read => "read",
        Capability::Iter => "iter",
        Capability::Commit => "commit",
        Capability::Flush => "flush",
        Capability::Compact => "compact",
        Capability::Admin => "admin",
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC types (subset of what MCP needs)
// ---------------------------------------------------------------------------

/// Incoming JSON-RPC request frame.
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    /// JSON-RPC version; always `"2.0"`.
    #[serde(default)]
    pub jsonrpc: String,
    /// Request id (echoed in the response).
    pub id: Option<serde_json::Value>,
    /// Method name (e.g. `tools/list`, `tools/call`, `initialize`).
    pub method: String,
    /// Method parameters. Optional per JSON-RPC 2.0.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// Outgoing JSON-RPC response frame.
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

/// JSON-RPC error object.
#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    /// Numeric error code.
    pub code: i64,
    /// Short human-readable message.
    pub message: String,
}

impl JsonRpcResponse {
    fn ok(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    fn err(id: serde_json::Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// MCP server handle. Owns a shared engine.
pub struct McpServer {
    engine: Arc<RwLock<Engine>>,
    /// Optional principal. When set, every tool call is checked against
    /// the principal's capabilities. When unset, every tool is allowed
    /// (the legacy stdio behaviour — appropriate for a single-tenant
    /// local launch).
    principal: Option<Principal>,
    /// Optional audit log. Each tool call emits a row.
    audit: Option<Arc<Mutex<AuditLog>>>,
}

impl McpServer {
    /// Open the database directory (creating it if missing) and prepare
    /// the server for a stdio loop.
    ///
    /// At-rest encryption is sourced from `NDB_ENC_KEY` — if set, the
    /// engine encrypts new files (on create) or refuses to open unless
    /// the marker fingerprint matches.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, McpError> {
        let path = path.as_ref();
        let engine = if path.exists() && path.join("CURRENT").exists() {
            Engine::open_from_env(path)?
        } else {
            Engine::create_from_env(path)?
        };
        Ok(Self {
            engine: Arc::new(RwLock::new(engine)),
            principal: None,
            audit: None,
        })
    }

    /// Wrap an existing engine — useful for tests.
    #[must_use]
    pub fn from_engine(engine: Engine) -> Self {
        Self {
            engine: Arc::new(RwLock::new(engine)),
            principal: None,
            audit: None,
        }
    }

    /// Install a principal. Every subsequent tool call is gated by the
    /// principal's capability set (the same enum used by ndb-server).
    /// Without a principal, the legacy unrestricted behaviour applies.
    #[must_use]
    pub fn with_principal(mut self, p: Principal) -> Self {
        self.principal = Some(p);
        self
    }

    /// Enable audit logging into `<db>/.audit.jsonl`. The MCP server
    /// shares the audit-file format with ndb-server so a single SIEM
    /// pipeline ingests both surfaces.
    pub fn with_audit_log(mut self) -> Result<Self, McpError> {
        let dir = {
            let eng = self.engine.write().expect("engine lock poisoned");
            eng.path().to_path_buf()
        };
        let log = AuditLog::open(&dir)?;
        self.audit = Some(Arc::new(Mutex::new(log)));
        Ok(self)
    }

    /// Audit-log path, if enabled.
    #[must_use]
    pub fn audit_log_path(&self) -> Option<std::path::PathBuf> {
        self.audit
            .as_ref()
            .map(|a| a.lock().expect("audit mutex poisoned").path().to_path_buf())
    }

    fn record_audit(&self, principal: &str, tool: &str, status: u16, failure: Option<&str>) {
        if let Some(log) = &self.audit {
            let entry = AuditEntry {
                ts_us: now_micros(),
                principal,
                method: "mcp.tools/call",
                path: tool,
                status,
                tx_id: None,
                failure,
            };
            if let Err(e) = log.lock().expect("audit mutex poisoned").append(&entry) {
                eprintln!("audit log write failed: {e}");
            }
        }
    }

    /// Borrow the underlying engine handle.
    #[must_use]
    pub fn engine(&self) -> Arc<RwLock<Engine>> {
        Arc::clone(&self.engine)
    }

    /// Run the stdio JSON-RPC loop until EOF. Each input line is parsed
    /// as a JSON-RPC request; the response is written to `out` followed
    /// by a newline.
    pub fn run_stdio<R: Read, W: Write>(&self, input: R, mut out: W) -> Result<(), McpError> {
        let reader = BufReader::new(input);
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let resp = self.handle_line(&line);
            let s = serde_json::to_string(&resp).map_err(|e| {
                McpError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            })?;
            out.write_all(s.as_bytes())?;
            out.write_all(b"\n")?;
            out.flush()?;
        }
        Ok(())
    }

    /// Handle a single line. Public so tests can drive it directly.
    pub fn handle_line(&self, line: &str) -> JsonRpcResponse {
        let req: JsonRpcRequest = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                return JsonRpcResponse::err(
                    serde_json::Value::Null,
                    -32700,
                    format!("parse error: {e}"),
                );
            }
        };
        let id = req.id.clone().unwrap_or(serde_json::Value::Null);
        match req.method.as_str() {
            "initialize" => JsonRpcResponse::ok(
                id,
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}, "resources": {}, "prompts": {}},
                    "serverInfo": {"name": "ndb-mcp-server", "version": env!("CARGO_PKG_VERSION")},
                }),
            ),
            "tools/list" => JsonRpcResponse::ok(id, serde_json::json!({"tools": tool_list()})),
            "tools/call" => match self.handle_tool_call(&req.params) {
                Ok(result) => JsonRpcResponse::ok(id, result),
                Err(e) => JsonRpcResponse::err(id, -32000, e.to_string()),
            },
            "resources/list" => {
                JsonRpcResponse::ok(id, serde_json::json!({"resources": resource_list()}))
            }
            "resources/read" => match self.handle_resource_read(&req.params) {
                Ok(result) => JsonRpcResponse::ok(id, result),
                Err(e) => JsonRpcResponse::err(id, -32000, e.to_string()),
            },
            "prompts/list" => {
                JsonRpcResponse::ok(id, serde_json::json!({"prompts": prompt_list()}))
            }
            "prompts/get" => match handle_prompt_get(&req.params) {
                Ok(result) => JsonRpcResponse::ok(id, result),
                Err(e) => JsonRpcResponse::err(id, -32000, e.to_string()),
            },
            other => JsonRpcResponse::err(id, -32601, format!("method not found: {other}")),
        }
    }

    fn handle_tool_call(&self, params: &serde_json::Value) -> Result<serde_json::Value, McpError> {
        let name = params
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| McpError::BadArgs("tools/call missing 'name'".into()))?;
        let empty = serde_json::Value::Object(serde_json::Map::new());
        let args = params.get("arguments").unwrap_or(&empty);

        // ReBAC gating: when a principal is installed, every recognised
        // tool must have a capability the principal holds. Unknown tools
        // are rejected the same way they were before — capability check
        // is purely additive.
        let principal_name: String = self
            .principal
            .as_ref()
            .map_or_else(|| "anonymous".into(), |p| p.name.clone());
        if let Some(p) = &self.principal
            && let Some(needed) = tool_capability(name)
            && !p.allows(needed)
        {
            let err = McpError::Forbidden {
                principal: p.name.clone(),
                capability: capability_str(needed),
            };
            self.record_audit(&principal_name, name, 403, Some(&err.to_string()));
            return Err(err);
        }

        let result = match name {
            "ndb.health" => Ok(serde_json::json!({"status": "ok"})),
            "ndb.read" => self.tool_read(args),
            "ndb.commit_entity" => self.tool_commit_entity(args),
            "ndb.commit_hyperedge" => self.tool_commit_hyperedge(args),
            "ndb.neighbors" => self.tool_neighbors(args),
            "ndb.read_as_of" => self.tool_read_as_of(args),
            "ndb.iter" => self.tool_iter(args),
            "ndb.lookup_by_key" => self.tool_lookup_by_key(args),
            "ndb.vector_search" => self.tool_vector_search(args),
            "ndb.property_lookup" => self.tool_property_lookup(args),
            "ndb.property_range" => self.tool_property_range(args),
            "ndb.arrow_export" => self.tool_arrow_export(args),
            "ndb.arrow_vectors" => self.tool_arrow_vectors(args),
            "ndb.arrow_edge_index" => self.tool_arrow_edge_index(args),
            other => Err(McpError::UnknownTool(other.into())),
        };
        let (status, failure_owned) = match &result {
            Ok(_) => (200, None),
            Err(e) => (500, Some(e.to_string())),
        };
        self.record_audit(&principal_name, name, status, failure_owned.as_deref());
        result
    }

    fn tool_read(&self, args: &serde_json::Value) -> Result<serde_json::Value, McpError> {
        let uuid_str = args
            .get("uuid")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| McpError::BadArgs("read: missing 'uuid'".into()))?;
        let uuid = Uuid::parse_str(uuid_str).map_err(|e| McpError::Convert(e.to_string()))?;
        let engine = self.engine.write().expect("engine lock poisoned");
        let snap = TxId::new(engine.manifest().last_tx_id);
        let resolved = engine.snapshot_read(&uuid, snap)?;
        Ok(match resolved {
            Resolved::Missing => serde_json::json!({"outcome": "missing"}),
            Resolved::Deleted { deleted_at } => serde_json::json!({
                "outcome": "deleted",
                "deleted_at": deleted_at.get(),
            }),
            Resolved::Live(r) => {
                let jr: JsonRecord = (&r).into();
                serde_json::json!({"outcome": "live", "record": jr})
            }
        })
    }

    fn tool_commit_entity(&self, args: &serde_json::Value) -> Result<serde_json::Value, McpError> {
        let type_id = args
            .get("type_id")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| McpError::BadArgs("commit_entity: missing 'type_id'".into()))?;
        let type_id = u32::try_from(type_id)
            .map_err(|_| McpError::BadArgs("commit_entity: type_id must fit in u32".into()))?;
        let props_json = args
            .get("properties")
            .ok_or_else(|| McpError::BadArgs("commit_entity: missing 'properties'".into()))?;
        let props_arr = props_json.as_array().ok_or_else(|| {
            McpError::BadArgs("commit_entity: 'properties' must be an array".into())
        })?;
        let mut properties: Vec<(PropertyId, Value)> = Vec::with_capacity(props_arr.len());
        for p in props_arr {
            let prop_id = p
                .get("prop_id")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| McpError::BadArgs("properties[].prop_id required".into()))?;
            let prop_id = u32::try_from(prop_id)
                .map_err(|_| McpError::BadArgs("prop_id must fit in u32".into()))?;
            let jv: JsonValue = serde_json::from_value(
                p.get("value")
                    .cloned()
                    .ok_or_else(|| McpError::BadArgs("properties[].value required".into()))?,
            )
            .map_err(|e| McpError::Convert(e.to_string()))?;
            let v: Value = jv
                .try_into()
                .map_err(|e: ndb_engine::WireError| McpError::Convert(e.to_string()))?;
            properties.push((PropertyId::new(prop_id), v));
        }
        let mut engine = self.engine.write().expect("engine lock poisoned");
        let mut txn = engine.begin_write();
        let entity_id = EntityId::now_v7();
        txn.put_entity(EntityRecord {
            entity_id,
            type_id: TypeId::new(type_id),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties,
        });
        let tx = txn.commit()?;
        Ok(serde_json::json!({
            "tx_id": tx.get(),
            "entity_id": entity_id.into_uuid().to_string(),
        }))
    }

    /// List visible records with cursor pagination (A3). `snapshot_iter`
    /// returns records in a stable merge-sorted order, so an integer offset is
    /// a valid, cheap cursor: page N covers `[cursor, cursor+limit)`. The
    /// snapshot is pinned by echoing `snapshot_tx` back to the caller, who
    /// passes it on subsequent pages so the view doesn't shift under concurrent
    /// writes. `next_cursor` is `null` once the last page is returned.
    fn tool_iter(&self, args: &serde_json::Value) -> Result<serde_json::Value, McpError> {
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(1000_usize, |n| n.try_into().unwrap_or(1000));
        let cursor = args
            .get("cursor")
            .and_then(serde_json::Value::as_u64)
            .map_or(0_usize, |n| n.try_into().unwrap_or(0));
        let engine = self.engine.write().expect("engine lock poisoned");
        // Pin the snapshot the caller is paging over: explicit `snapshot_tx`
        // for pages 2..N, else the current head for page 1.
        let snap = args
            .get("snapshot_tx")
            .and_then(serde_json::Value::as_u64)
            .map_or_else(|| TxId::new(engine.manifest().last_tx_id), TxId::new);
        let records = engine.snapshot_iter(snap)?;
        // Filter internal v2.0 metadata records, then window by [cursor, +limit).
        let filtered: Vec<&ndb_engine::Record> = records
            .iter()
            .filter(|r| {
                !matches!(
                    r,
                    ndb_engine::Record::TxTimestamp(_) | ndb_engine::Record::RetentionPolicy(_)
                )
            })
            .collect();
        let total = filtered.len();
        let end = cursor.saturating_add(limit).min(total);
        let window = filtered.get(cursor..end).unwrap_or(&[]);
        let payload: Vec<JsonRecord> = window.iter().map(|r| JsonRecord::from(*r)).collect();
        let next_cursor = if end < total {
            serde_json::json!(end)
        } else {
            serde_json::Value::Null
        };
        Ok(serde_json::json!({
            "records": payload,
            "next_cursor": next_cursor,
            "snapshot_tx": snap.get(),
        }))
    }

    fn tool_lookup_by_key(&self, args: &serde_json::Value) -> Result<serde_json::Value, McpError> {
        let property_id = args
            .get("property_id")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| McpError::BadArgs("lookup_by_key: missing 'property_id'".into()))?;
        let property_id = u32::try_from(property_id)
            .map_err(|_| McpError::BadArgs("property_id must fit in u32".into()))?;
        let jv: JsonValue = serde_json::from_value(
            args.get("value")
                .cloned()
                .ok_or_else(|| McpError::BadArgs("lookup_by_key: missing 'value'".into()))?,
        )
        .map_err(|e| McpError::Convert(e.to_string()))?;
        let v: Value = jv
            .try_into()
            .map_err(|e: ndb_engine::WireError| McpError::Convert(e.to_string()))?;
        let engine = self.engine.write().expect("engine lock poisoned");
        let hit = engine.lookup_by_external_key(PropertyId::new(property_id), &v);
        Ok(match hit {
            Some(id) => serde_json::json!({"entity_id": id.into_uuid().to_string()}),
            None => serde_json::json!({"entity_id": null}),
        })
    }

    fn tool_vector_search(&self, args: &serde_json::Value) -> Result<serde_json::Value, McpError> {
        let property_id = args
            .get("property_id")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| McpError::BadArgs("vector_search: missing 'property_id'".into()))?;
        let property_id = u32::try_from(property_id)
            .map_err(|_| McpError::BadArgs("property_id must fit in u32".into()))?;
        let query: Vec<f32> = args
            .get("query")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| McpError::BadArgs("vector_search: 'query' array required".into()))?
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0) as f32)
            .collect();
        let k = args
            .get("k")
            .and_then(serde_json::Value::as_u64)
            .map_or(10_usize, |n| n.try_into().unwrap_or(10));
        let metric = match args
            .get("metric")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("l2_squared")
        {
            "cosine" => Distance::Cosine,
            _ => Distance::L2Squared,
        };
        let engine = self.engine.write().expect("engine lock poisoned");
        let hits = engine.vector_search(PropertyId::new(property_id), &query, k, metric);
        let payload: Vec<_> = hits
            .into_iter()
            .map(|(id, d)| {
                serde_json::json!({
                    "entity_id": id.into_uuid().to_string(),
                    "distance": d,
                })
            })
            .collect();
        Ok(serde_json::json!({"hits": payload}))
    }

    fn tool_property_lookup(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let (type_id, property_id, value) = parse_type_prop_value(args)?;
        let engine = self.engine.write().expect("engine lock poisoned");
        let hits = engine.property_lookup(type_id, property_id, &value);
        Ok(serde_json::json!({
            "entity_ids": hits
                .into_iter()
                .map(|id| id.into_uuid().to_string())
                .collect::<Vec<_>>(),
        }))
    }

    fn tool_property_range(&self, args: &serde_json::Value) -> Result<serde_json::Value, McpError> {
        let type_id = args
            .get("type_id")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| McpError::BadArgs("property_range: missing 'type_id'".into()))?;
        let type_id = TypeId::new(
            u32::try_from(type_id).map_err(|_| McpError::BadArgs("type_id u32".into()))?,
        );
        let property_id = args
            .get("property_id")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| McpError::BadArgs("property_range: missing 'property_id'".into()))?;
        let property_id = PropertyId::new(
            u32::try_from(property_id).map_err(|_| McpError::BadArgs("prop_id u32".into()))?,
        );
        let low = args
            .get("low")
            .map(|v| serde_json::from_value::<JsonValue>(v.clone()))
            .transpose()
            .map_err(|e| McpError::Convert(e.to_string()))?
            .map(|jv| jv.try_into())
            .transpose()
            .map_err(|e: ndb_engine::WireError| McpError::Convert(e.to_string()))?;
        let high = args
            .get("high")
            .map(|v| serde_json::from_value::<JsonValue>(v.clone()))
            .transpose()
            .map_err(|e| McpError::Convert(e.to_string()))?
            .map(|jv| jv.try_into())
            .transpose()
            .map_err(|e: ndb_engine::WireError| McpError::Convert(e.to_string()))?;
        let engine = self.engine.write().expect("engine lock poisoned");
        let hits = engine.property_range(type_id, property_id, low.as_ref(), high.as_ref());
        Ok(serde_json::json!({
            "entity_ids": hits
                .into_iter()
                .map(|id| id.into_uuid().to_string())
                .collect::<Vec<_>>(),
        }))
    }

    /// `ndb.arrow_export` — every visible record at the latest snapshot as an
    /// Arrow IPC stream (fixed-size batches, one schema), base64-encoded for
    /// the JSON-RPC envelope. The on-ramp to Polars/pandas/DuckDB/cuDF and the
    /// GPU pipeline (see `docs/gpu-dgx-spark.md`).
    fn tool_arrow_export(&self, args: &serde_json::Value) -> Result<serde_json::Value, McpError> {
        let batch_rows = args
            .get("batch_rows")
            .and_then(serde_json::Value::as_u64)
            .map_or(65_536_usize, |n| n.try_into().unwrap_or(65_536))
            .max(1);
        let records = self.snapshot_records()?;
        let ipc = ndb_arrow::records_to_ipc_stream_chunked(&records, batch_rows)
            .map_err(|e| McpError::Convert(e.to_string()))?;
        Ok(arrow_payload(&ipc))
    }

    /// `ndb.arrow_vectors` — a dense `primary_id + embedding:
    /// FixedSizeList<Float32, dim>` Arrow batch for the given vector property,
    /// base64-encoded. The layout cuVS/RAPIDS expect for GPU ANN re-rank.
    fn tool_arrow_vectors(&self, args: &serde_json::Value) -> Result<serde_json::Value, McpError> {
        let type_id = args
            .get("type_id")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| McpError::BadArgs("arrow_vectors: missing 'type_id'".into()))?;
        let property_id = args
            .get("property_id")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| McpError::BadArgs("arrow_vectors: missing 'property_id'".into()))?;
        let records = self.snapshot_records()?;
        let batch = ndb_arrow::vector_column_batch(
            &records,
            TypeId::new(type_id),
            PropertyId::new(property_id),
        )
        .map_err(|e| McpError::Convert(e.to_string()))?;
        let ipc =
            ndb_arrow::batch_to_ipc_stream(&batch).map_err(|e| McpError::Convert(e.to_string()))?;
        Ok(arrow_payload(&ipc))
    }

    /// `ndb.arrow_edge_index` — every hyperedge flattened into a bipartite
    /// `(hyperedge_id, role_id, participant_id, participant_kind)` incidence
    /// list for cuGraph/DGL/PyG, base64-encoded Arrow IPC.
    fn tool_arrow_edge_index(
        &self,
        _args: &serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let records = self.snapshot_records()?;
        let batch = ndb_arrow::hyperedge_edge_index(&records)
            .map_err(|e| McpError::Convert(e.to_string()))?;
        let ipc =
            ndb_arrow::batch_to_ipc_stream(&batch).map_err(|e| McpError::Convert(e.to_string()))?;
        Ok(arrow_payload(&ipc))
    }

    /// Read every record at the latest snapshot — shared by the Arrow exporters.
    fn snapshot_records(&self) -> Result<Vec<Record>, McpError> {
        let engine = self.engine.write().expect("engine lock poisoned");
        let snap = TxId::new(engine.manifest().last_tx_id);
        Ok(engine.snapshot_iter(snap)?)
    }

    /// Commit an n-ary relationship as a single hyperedge record. This is the
    /// agent-facing surface for nDB's distinctive model: a relationship of any
    /// arity is one record, not a junction table. `roles` connect entities;
    /// optional `hyperedge_roles` connect other hyperedges (e.g. a pathway
    /// whose fillers are reaction hyperedges). Total arity must be ≥ 1.
    fn tool_commit_hyperedge(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let type_id = args
            .get("type_id")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| McpError::BadArgs("commit_hyperedge: missing 'type_id'".into()))?;
        let type_id = u32::try_from(type_id)
            .map_err(|_| McpError::BadArgs("commit_hyperedge: type_id must fit in u32".into()))?;

        let roles = parse_role_list::<EntityId>(args.get("roles"), "roles")?;
        let hyperedge_roles =
            parse_role_list::<HyperedgeId>(args.get("hyperedge_roles"), "hyperedge_roles")?;
        if roles.is_empty() && hyperedge_roles.is_empty() {
            return Err(McpError::BadArgs(
                "commit_hyperedge: total arity must be ≥ 1 (provide 'roles' and/or 'hyperedge_roles')".into(),
            ));
        }
        let properties = parse_property_list(args.get("properties"))?;

        let mut engine = self.engine.write().expect("engine lock poisoned");
        let mut txn = engine.begin_write();
        let hyperedge_id = HyperedgeId::now_v7();
        txn.put_hyperedge(HyperEdgeRecord {
            hyperedge_id,
            type_id: TypeId::new(type_id),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            roles,
            hyperedge_roles,
            properties,
        });
        let tx = txn.commit()?;
        Ok(serde_json::json!({
            "tx_id": tx.get(),
            "hyperedge_id": hyperedge_id.into_uuid().to_string(),
        }))
    }

    /// One-hop traversal: given an entity UUID, return the live hyperedges
    /// incident to it, each with its role-fillers, so an agent can walk the
    /// graph without re-joining a junction table. Bounded by `limit`
    /// (default 100) to stay cheap on power-law hubs.
    fn tool_neighbors(&self, args: &serde_json::Value) -> Result<serde_json::Value, McpError> {
        let uuid_str = args
            .get("uuid")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| McpError::BadArgs("neighbors: missing 'uuid'".into()))?;
        let uuid = Uuid::parse_str(uuid_str).map_err(|e| McpError::Convert(e.to_string()))?;
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(100_usize, |n| n.try_into().unwrap_or(100));
        let entity = EntityId::from_uuid(uuid);

        let engine = self.engine.write().expect("engine lock poisoned");
        let snap = TxId::new(engine.manifest().last_tx_id);
        let incident = engine.hyperedges_for_entity_capped(entity, limit);
        let mut edges = Vec::with_capacity(incident.len());
        for hid in incident {
            if let Resolved::Live(Record::HyperEdge(hr)) =
                engine.snapshot_read(&hid.into_uuid(), snap)?
            {
                let jr: JsonRecord = (&Record::HyperEdge(hr)).into();
                edges.push(jr);
            }
        }
        Ok(serde_json::json!({
            "entity_id": uuid.to_string(),
            "hyperedges": edges,
        }))
    }

    /// Time-travel read. Resolve a snapshot from either an explicit
    /// `as_of_tx` or a wall-clock `as_of_timestamp_us` (microseconds since the
    /// Unix epoch, mapped via the last commit at-or-before that instant), then
    /// read `uuid` as it existed at that snapshot.
    fn tool_read_as_of(&self, args: &serde_json::Value) -> Result<serde_json::Value, McpError> {
        let uuid_str = args
            .get("uuid")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| McpError::BadArgs("read_as_of: missing 'uuid'".into()))?;
        let uuid = Uuid::parse_str(uuid_str).map_err(|e| McpError::Convert(e.to_string()))?;

        let engine = self.engine.write().expect("engine lock poisoned");
        let snap = if let Some(tx) = args.get("as_of_tx").and_then(serde_json::Value::as_u64) {
            TxId::new(tx)
        } else if let Some(ts) = args
            .get("as_of_timestamp_us")
            .and_then(serde_json::Value::as_i64)
        {
            match engine.tx_at_or_before(ts) {
                Some(tx) => tx,
                // No commit at-or-before that instant: the entity could not
                // have existed yet.
                None => {
                    return Ok(serde_json::json!({"outcome": "missing", "as_of_tx": 0}));
                }
            }
        } else {
            return Err(McpError::BadArgs(
                "read_as_of: provide 'as_of_tx' or 'as_of_timestamp_us'".into(),
            ));
        };

        let resolved = engine.snapshot_read(&uuid, snap)?;
        Ok(match resolved {
            Resolved::Missing => serde_json::json!({"outcome": "missing", "as_of_tx": snap.get()}),
            Resolved::Deleted { deleted_at } => serde_json::json!({
                "outcome": "deleted",
                "deleted_at": deleted_at.get(),
                "as_of_tx": snap.get(),
            }),
            Resolved::Live(r) => {
                let jr: JsonRecord = (&r).into();
                serde_json::json!({"outcome": "live", "record": jr, "as_of_tx": snap.get()})
            }
        })
    }

    /// MCP `resources/read` (A4). Read-only context blobs an agent can pull to
    /// discover the schema without trial-and-error tool calls. Gated by the
    /// `read` capability when a principal is installed.
    fn handle_resource_read(
        &self,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        if let Some(p) = &self.principal
            && !p.allows(Capability::Read)
        {
            return Err(McpError::Forbidden {
                principal: p.name.clone(),
                capability: "read",
            });
        }
        let uri = params
            .get("uri")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| McpError::BadArgs("resources/read missing 'uri'".into()))?;

        let engine = self.engine.write().expect("engine lock poisoned");
        let snap = TxId::new(engine.manifest().last_tx_id);
        let records = engine.snapshot_iter(snap)?;

        // Pull the dictionaries (type/role/property names) out of the stream.
        let mut types: Vec<serde_json::Value> = Vec::new();
        let mut roles: Vec<serde_json::Value> = Vec::new();
        let mut properties: Vec<serde_json::Value> = Vec::new();
        let (mut entities, mut hyperedges) = (0_u64, 0_u64);
        for r in &records {
            match r {
                Record::TypeName(t) => {
                    types.push(serde_json::json!({"id": t.id.get(), "name": t.name}));
                }
                Record::RoleName(t) => {
                    roles.push(serde_json::json!({"id": t.id.get(), "name": t.name}));
                }
                Record::PropertyKey(t) => {
                    properties.push(serde_json::json!({"id": t.id.get(), "name": t.name}));
                }
                Record::Entity(_) => entities += 1,
                Record::HyperEdge(_) => hyperedges += 1,
                _ => {}
            }
        }

        let text = match uri {
            "ndb://dictionaries" => serde_json::json!({
                "types": types, "roles": roles, "properties": properties,
            }),
            "ndb://schema" => serde_json::json!({
                "description": "nDB is schemaless per-entity; these dictionaries name the type/role/property slots in use.",
                "types": types, "roles": roles, "properties": properties,
            }),
            "ndb://stats" => serde_json::json!({
                "snapshot_tx": snap.get(),
                "entities": entities,
                "hyperedges": hyperedges,
                "type_count": types.len(),
                "role_count": roles.len(),
                "property_count": properties.len(),
            }),
            other => return Err(McpError::BadArgs(format!("unknown resource uri: {other}"))),
        };
        Ok(serde_json::json!({
            "contents": [{
                "uri": uri,
                "mimeType": "application/json",
                "text": serde_json::to_string(&text).unwrap_or_default(),
            }]
        }))
    }
}

/// Wrap raw Arrow IPC bytes in the JSON envelope MCP tools return: base64 for
/// the binary, plus the decoded byte length so a client can size its buffer.
fn arrow_payload(ipc: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "format": "arrow_ipc_stream",
        "encoding": "base64",
        "byte_len": ipc.len(),
        "data": BASE64.encode(ipc),
    })
}

/// Static `resources/list` set (A4).
fn resource_list() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "uri": "ndb://dictionaries",
            "name": "Dictionaries",
            "description": "type / role / property-key id↔name maps",
            "mimeType": "application/json",
        }),
        serde_json::json!({
            "uri": "ndb://schema",
            "name": "Schema overview",
            "description": "dictionaries plus a note on nDB's per-entity schemaless model",
            "mimeType": "application/json",
        }),
        serde_json::json!({
            "uri": "ndb://stats",
            "name": "Database stats",
            "description": "entity / hyperedge counts and dictionary sizes at the latest snapshot",
            "mimeType": "application/json",
        }),
    ]
}

/// Static `prompts/list` set (A4): query templates an agent can fill in.
fn prompt_list() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "name": "explore_entity",
            "description": "Read an entity and walk its one-hop hyperedge neighbourhood",
            "arguments": [{"name": "uuid", "description": "entity UUID", "required": true}],
        }),
        serde_json::json!({
            "name": "semantic_search",
            "description": "Find entities nearest to a query vector on a vector-indexed property",
            "arguments": [
                {"name": "property_id", "description": "vector property id", "required": true},
                {"name": "k", "description": "how many neighbours", "required": false},
            ],
        }),
    ]
}

/// MCP `prompts/get` (A4): expand a named template into chat messages.
fn handle_prompt_get(params: &serde_json::Value) -> Result<serde_json::Value, McpError> {
    let name = params
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| McpError::BadArgs("prompts/get missing 'name'".into()))?;
    let args = params.get("arguments").cloned().unwrap_or_default();
    let text = match name {
        "explore_entity" => {
            let uuid = args
                .get("uuid")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("<uuid>");
            format!(
                "Call ndb.read with {{\"uuid\":\"{uuid}\"}} to fetch the entity, then \
                 ndb.neighbors with the same uuid to list the hyperedges it participates \
                 in. Summarise the entity and how it connects to its neighbours."
            )
        }
        "semantic_search" => {
            let property_id = args
                .get("property_id")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let k = args
                .get("k")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(10);
            format!(
                "Embed the user's query into a vector, then call ndb.vector_search with \
                 {{\"property_id\":{property_id},\"query\":[...],\"k\":{k},\"metric\":\"cosine\"}}. \
                 Read the top hits with ndb.read and explain why each is relevant."
            )
        }
        other => return Err(McpError::BadArgs(format!("unknown prompt: {other}"))),
    };
    Ok(serde_json::json!({
        "messages": [{
            "role": "user",
            "content": {"type": "text", "text": text},
        }]
    }))
}

/// Parse a `[{role_id, <id_field>}]` JSON array into role-filler pairs. `T` is
/// the filler id type (`EntityId` for `roles`, `HyperedgeId` for
/// `hyperedge_roles`); both wrap a UUID and the field name is `<field>` minus
/// its trailing `s` is not assumed — the id key is the singular id field.
fn parse_role_list<T: FromUuid>(
    value: Option<&serde_json::Value>,
    field: &str,
) -> Result<Vec<(RoleId, T)>, McpError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let arr = value
        .as_array()
        .ok_or_else(|| McpError::BadArgs(format!("'{field}' must be an array")))?;
    let id_key = T::ID_KEY;
    let mut out = Vec::with_capacity(arr.len());
    for r in arr {
        let role_id = r
            .get("role_id")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| McpError::BadArgs(format!("{field}[].role_id required")))?;
        let role_id = u32::try_from(role_id)
            .map_err(|_| McpError::BadArgs(format!("{field}[].role_id must fit in u32")))?;
        if role_id == 0 {
            return Err(McpError::BadArgs(format!(
                "{field}[].role_id must be non-zero"
            )));
        }
        let id_str = r
            .get(id_key)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| McpError::BadArgs(format!("{field}[].{id_key} required")))?;
        let id = Uuid::parse_str(id_str).map_err(|e| McpError::Convert(e.to_string()))?;
        out.push((RoleId::new(role_id), T::from_uuid(id)));
    }
    Ok(out)
}

/// Bridge trait so `parse_role_list` can build either an `EntityId` or a
/// `HyperedgeId` from a UUID, and knows which JSON key carries the id.
trait FromUuid {
    /// JSON key the filler id is read from (`"entity_id"` / `"hyperedge_id"`).
    const ID_KEY: &'static str;
    /// Wrap a parsed UUID in the concrete id newtype.
    fn from_uuid(u: Uuid) -> Self;
}

impl FromUuid for EntityId {
    const ID_KEY: &'static str = "entity_id";
    fn from_uuid(u: Uuid) -> Self {
        EntityId::from_uuid(u)
    }
}

impl FromUuid for HyperedgeId {
    const ID_KEY: &'static str = "hyperedge_id";
    fn from_uuid(u: Uuid) -> Self {
        HyperedgeId::from_uuid(u)
    }
}

/// Parse a `[{prop_id, value}]` JSON array into property pairs. Accepts a
/// missing/absent list as empty. Shared by hyperedge commit; entity commit
/// keeps its own inline copy.
fn parse_property_list(
    value: Option<&serde_json::Value>,
) -> Result<Vec<(PropertyId, Value)>, McpError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let arr = value
        .as_array()
        .ok_or_else(|| McpError::BadArgs("'properties' must be an array".into()))?;
    let mut out = Vec::with_capacity(arr.len());
    for p in arr {
        let prop_id = p
            .get("prop_id")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| McpError::BadArgs("properties[].prop_id required".into()))?;
        let prop_id = u32::try_from(prop_id)
            .map_err(|_| McpError::BadArgs("prop_id must fit in u32".into()))?;
        let jv: JsonValue = serde_json::from_value(
            p.get("value")
                .cloned()
                .ok_or_else(|| McpError::BadArgs("properties[].value required".into()))?,
        )
        .map_err(|e| McpError::Convert(e.to_string()))?;
        let v: Value = jv
            .try_into()
            .map_err(|e: ndb_engine::WireError| McpError::Convert(e.to_string()))?;
        out.push((PropertyId::new(prop_id), v));
    }
    Ok(out)
}

fn parse_type_prop_value(
    args: &serde_json::Value,
) -> Result<(TypeId, PropertyId, Value), McpError> {
    let type_id = args
        .get("type_id")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| McpError::BadArgs("missing 'type_id'".into()))?;
    let type_id =
        TypeId::new(u32::try_from(type_id).map_err(|_| McpError::BadArgs("type_id u32".into()))?);
    let property_id = args
        .get("property_id")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| McpError::BadArgs("missing 'property_id'".into()))?;
    let property_id = PropertyId::new(
        u32::try_from(property_id).map_err(|_| McpError::BadArgs("prop_id u32".into()))?,
    );
    let jv: JsonValue = serde_json::from_value(
        args.get("value")
            .cloned()
            .ok_or_else(|| McpError::BadArgs("missing 'value'".into()))?,
    )
    .map_err(|e| McpError::Convert(e.to_string()))?;
    let value: Value = jv
        .try_into()
        .map_err(|e: ndb_engine::WireError| McpError::Convert(e.to_string()))?;
    Ok((type_id, property_id, value))
}

/// A `{tag, value}` wire-value, referenced by every tool that takes or filters
/// on a property value. Defined once and `$ref`'d so the schemas stay DRY.
fn wire_value_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": "Tagged nDB value, e.g. {\"tag\":\"string\",\"value\":\"x\"}, {\"tag\":\"i64\",\"value\":7}, {\"tag\":\"vector\",\"value\":[0.1,0.2]}",
        "properties": {
            "tag": {"type": "string", "enum": [
                "null", "bool", "i64", "f64", "string", "bytes",
                "timestamp", "uuid", "decimal", "vector"
            ]},
            "value": {}
        },
        "required": ["tag"]
    })
}

/// Build one `tools/list` entry with a JSON-Schema `inputSchema` (A2). Agents
/// — especially coding agents — call tools far more reliably when each
/// argument is typed and the required set is explicit.
#[allow(clippy::needless_pass_by_value)] // `properties` is moved into the json! tree
fn tool(
    name: &str,
    description: &str,
    properties: serde_json::Value,
    required: &[&str],
) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
        }
    })
}

#[allow(clippy::too_many_lines)] // one declarative entry per tool — flat by design
fn tool_list() -> Vec<serde_json::Value> {
    let uuid_prop = serde_json::json!({"type": "string", "description": "canonical UUID v7"});
    let type_id_prop = serde_json::json!({"type": "integer", "minimum": 0, "description": "TypeName dictionary id"});
    let prop_id_prop = serde_json::json!({"type": "integer", "minimum": 1, "description": "PropertyKey dictionary id"});
    let role_array = serde_json::json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": {
                "role_id": {"type": "integer", "minimum": 1},
                "entity_id": {"type": "string", "description": "entity UUID"}
            },
            "required": ["role_id", "entity_id"]
        }
    });
    let property_array = serde_json::json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": {
                "prop_id": prop_id_prop,
                "value": wire_value_schema()
            },
            "required": ["prop_id", "value"]
        }
    });
    vec![
        tool("ndb.health", "liveness probe", serde_json::json!({}), &[]),
        tool(
            "ndb.read",
            "look up a UUID at the latest snapshot",
            serde_json::json!({"uuid": uuid_prop}),
            &["uuid"],
        ),
        tool(
            "ndb.commit_entity",
            "commit a new entity with a type and properties",
            serde_json::json!({"type_id": type_id_prop, "properties": property_array}),
            &["type_id", "properties"],
        ),
        tool(
            "ndb.commit_hyperedge",
            "commit an n-ary relationship (hyperedge) connecting entities and/or other hyperedges by role",
            serde_json::json!({
                "type_id": type_id_prop,
                "roles": role_array,
                "hyperedge_roles": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "role_id": {"type": "integer", "minimum": 1},
                            "hyperedge_id": {"type": "string"}
                        },
                        "required": ["role_id", "hyperedge_id"]
                    }
                },
                "properties": property_array
            }),
            &["type_id"],
        ),
        tool(
            "ndb.neighbors",
            "one-hop traversal: live hyperedges incident to an entity UUID, with their role-fillers",
            serde_json::json!({
                "uuid": uuid_prop,
                "limit": {"type": "integer", "minimum": 1, "default": 100}
            }),
            &["uuid"],
        ),
        tool(
            "ndb.read_as_of",
            "time-travel read of a UUID at a past snapshot",
            serde_json::json!({
                "uuid": uuid_prop,
                "as_of_tx": {"type": "integer", "minimum": 0, "description": "snapshot transaction id"},
                "as_of_timestamp_us": {"type": "integer", "description": "wall-clock microseconds since Unix epoch"}
            }),
            &["uuid"],
        ),
        tool(
            "ndb.iter",
            "list visible records with cursor pagination; returns {records, next_cursor, snapshot_tx}",
            serde_json::json!({
                "limit": {"type": "integer", "minimum": 1, "default": 1000},
                "cursor": {"type": "integer", "minimum": 0, "description": "offset from a prior next_cursor; omit for the first page"},
                "snapshot_tx": {"type": "integer", "minimum": 0, "description": "pin a snapshot across pages; echo back the value returned in the first page"}
            }),
            &[],
        ),
        tool(
            "ndb.lookup_by_key",
            "find an entity by external lookup-key value",
            serde_json::json!({"property_id": prop_id_prop, "value": wire_value_schema()}),
            &["property_id", "value"],
        ),
        tool(
            "ndb.vector_search",
            "k-NN search over a vector-indexed property",
            serde_json::json!({
                "property_id": prop_id_prop,
                "query": {"type": "array", "items": {"type": "number"}, "description": "query vector (f32)"},
                "k": {"type": "integer", "minimum": 1, "default": 10},
                "metric": {"type": "string", "enum": ["l2_squared", "cosine"], "default": "l2_squared"}
            }),
            &["property_id", "query"],
        ),
        tool(
            "ndb.property_lookup",
            "exact match on (type, property, value)",
            serde_json::json!({"type_id": type_id_prop, "property_id": prop_id_prop, "value": wire_value_schema()}),
            &["type_id", "property_id", "value"],
        ),
        tool(
            "ndb.property_range",
            "range query on (type, property) with low/high bounds",
            serde_json::json!({
                "type_id": type_id_prop,
                "property_id": prop_id_prop,
                "low": wire_value_schema(),
                "high": wire_value_schema()
            }),
            &["type_id", "property_id"],
        ),
        tool(
            "ndb.arrow_export",
            "export all records as a base64 Arrow IPC stream (fixed-size batches, one schema) for Polars/pandas/DuckDB/cuDF",
            serde_json::json!({
                "batch_rows": {"type": "integer", "minimum": 1, "default": 65536, "description": "rows per Arrow batch"}
            }),
            &[],
        ),
        tool(
            "ndb.arrow_vectors",
            "export a dense embedding matrix (primary_id + FixedSizeList<Float32,dim>) as base64 Arrow IPC for GPU ANN (cuVS)",
            serde_json::json!({"type_id": type_id_prop, "property_id": prop_id_prop}),
            &["type_id", "property_id"],
        ),
        tool(
            "ndb.arrow_edge_index",
            "export the hyperedge incidence list (hyperedge_id, role_id, participant_id, participant_kind) as base64 Arrow IPC for GNNs (cuGraph/PyG)",
            serde_json::json!({}),
            &[],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndb_engine::value::TAG_STRING;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "ndb-mcp-{}-{}",
            name,
            uuid::Uuid::now_v7().simple()
        ));
        p
    }

    fn call(server: &McpServer, line: &str) -> serde_json::Value {
        let resp = server.handle_line(line);
        let s = serde_json::to_value(&resp).unwrap();
        s
    }

    #[test]
    fn initialize_returns_capabilities() {
        let dir = temp_dir("init");
        let server = McpServer::open(&dir).unwrap();
        let resp = call(&server, r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#);
        assert_eq!(resp["result"]["serverInfo"]["name"], "ndb-mcp-server");
        assert!(resp["result"]["capabilities"]["tools"].is_object());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tools_list_returns_all_tools() {
        let dir = temp_dir("tools_list");
        let server = McpServer::open(&dir).unwrap();
        let resp = call(&server, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"ndb.health"));
        assert!(names.contains(&"ndb.read"));
        assert!(names.contains(&"ndb.commit_entity"));
        assert!(names.contains(&"ndb.vector_search"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn health_tool_returns_ok() {
        let dir = temp_dir("health");
        let server = McpServer::open(&dir).unwrap();
        let resp = call(
            &server,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ndb.health"}}"#,
        );
        assert_eq!(resp["result"]["status"], "ok");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn commit_then_read_round_trip() {
        let dir = temp_dir("commit_read");
        let server = McpServer::open(&dir).unwrap();
        // Configure validation: type 1 requires prop 10 to be a string.
        {
            let e = server.engine();
            let mut e = e.write().unwrap();
            e.require_property(TypeId::new(1), PropertyId::new(10));
            e.expect_value_tag(TypeId::new(1), PropertyId::new(10), TAG_STRING);
        }
        let resp = call(
            &server,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{
                "name":"ndb.commit_entity",
                "arguments":{
                    "type_id":1,
                    "properties":[{"prop_id":10,"value":{"tag":"string","value":"alice@example.com"}}]
                }
            }}"#,
        );
        let tx_id = resp["result"]["tx_id"].as_u64().unwrap();
        assert!(tx_id > 0);
        let uuid = resp["result"]["entity_id"].as_str().unwrap().to_owned();

        let read = call(
            &server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{{
                    "name":"ndb.read","arguments":{{"uuid":"{uuid}"}}
                }}}}"#
            ),
        );
        assert_eq!(read["result"]["outcome"], "live");
        assert_eq!(read["result"]["record"]["entity_id"], uuid);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unknown_method_returns_jsonrpc_error() {
        let dir = temp_dir("bad_method");
        let server = McpServer::open(&dir).unwrap();
        let resp = call(
            &server,
            r#"{"jsonrpc":"2.0","id":6,"method":"definitely/not/a/method"}"#,
        );
        assert!(resp["error"]["code"].as_i64().unwrap() == -32601);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unknown_tool_returns_error() {
        let dir = temp_dir("bad_tool");
        let server = McpServer::open(&dir).unwrap();
        let resp = call(
            &server,
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"nope.no"}}"#,
        );
        assert!(resp["error"].is_object());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn malformed_json_returns_parse_error() {
        let dir = temp_dir("parse_err");
        let server = McpServer::open(&dir).unwrap();
        let resp = call(&server, "not valid json {{{");
        assert_eq!(resp["error"]["code"].as_i64().unwrap(), -32700);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn principal_without_capability_is_forbidden() {
        use std::collections::BTreeSet;
        let dir = temp_dir("mcp_rebac");
        let server = McpServer::open(&dir).unwrap().with_principal(Principal {
            name: "read-only-bot".into(),
            capabilities: BTreeSet::from([Capability::Read, Capability::Iter]),
            entity_id: None,
        });
        let resp = call(
            &server,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                "name":"ndb.commit_entity",
                "arguments":{"type_id":1,"properties":[]}
            }}"#,
        );
        assert!(resp["error"].is_object(), "got: {resp:?}");
        let msg = resp["error"]["message"].as_str().unwrap();
        assert!(msg.contains("read-only-bot"));
        assert!(msg.contains("commit"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn principal_with_admin_can_call_everything() {
        use std::collections::BTreeSet;
        let dir = temp_dir("mcp_admin");
        let server = McpServer::open(&dir).unwrap().with_principal(Principal {
            name: "root".into(),
            capabilities: BTreeSet::from([Capability::Admin]),
            entity_id: None,
        });
        let resp = call(
            &server,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                "name":"ndb.commit_entity",
                "arguments":{
                    "type_id":1,
                    "properties":[{"prop_id":10,"value":{"tag":"string","value":"x"}}]
                }
            }}"#,
        );
        assert!(resp["result"]["tx_id"].as_u64().unwrap() > 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn audit_log_records_tool_calls() {
        let dir = temp_dir("mcp_audit");
        let server = McpServer::open(&dir).unwrap().with_audit_log().unwrap();
        let _ = call(
            &server,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"ndb.health"}}"#,
        );
        let _ = call(
            &server,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"nope.no"}}"#,
        );
        let path = server.audit_log_path().unwrap();
        let s = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 2, "got: {s:?}");
        let row1: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(row1["method"], "mcp.tools/call");
        assert_eq!(row1["path"], "ndb.health");
        assert_eq!(row1["status"], 200);
        let row2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(row2["path"], "nope.no");
        assert_eq!(row2["status"], 500);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn iter_returns_records() {
        let dir = temp_dir("iter");
        let server = McpServer::open(&dir).unwrap();
        for i in 0..3 {
            call(
                &server,
                &format!(
                    r#"{{"jsonrpc":"2.0","id":{i},"method":"tools/call","params":{{
                        "name":"ndb.commit_entity",
                        "arguments":{{
                            "type_id":1,
                            "properties":[{{"prop_id":1,"value":{{"tag":"i64","value":{i}}}}}]
                        }}
                    }}}}"#
                ),
            );
        }
        let resp = call(
            &server,
            r#"{"jsonrpc":"2.0","id":100,"method":"tools/call","params":{"name":"ndb.iter"}}"#,
        );
        let recs = resp["result"]["records"].as_array().unwrap();
        assert_eq!(recs.len(), 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Commit a bare entity of `type_id` with no properties; return its UUID.
    fn commit_bare_entity(server: &McpServer, type_id: u32) -> String {
        let resp = call(
            server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{
                    "name":"ndb.commit_entity",
                    "arguments":{{"type_id":{type_id},"properties":[]}}
                }}}}"#
            ),
        );
        resp["result"]["entity_id"].as_str().unwrap().to_owned()
    }

    #[test]
    fn commit_hyperedge_and_neighbors_round_trip() {
        let dir = temp_dir("hyperedge");
        let server = McpServer::open(&dir).unwrap();
        let a = commit_bare_entity(&server, 5);
        let b = commit_bare_entity(&server, 5);

        // type 7, two entity role-fillers (roles 1 and 2).
        let resp = call(
            &server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{{
                    "name":"ndb.commit_hyperedge",
                    "arguments":{{
                        "type_id":7,
                        "roles":[
                            {{"role_id":1,"entity_id":"{a}"}},
                            {{"role_id":2,"entity_id":"{b}"}}
                        ]
                    }}
                }}}}"#
            ),
        );
        let hid = resp["result"]["hyperedge_id"].as_str().unwrap().to_owned();
        assert!(resp["result"]["tx_id"].as_u64().unwrap() > 0);

        // neighbors(a) must surface the hyperedge with both role-fillers.
        let nb = call(
            &server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{{
                    "name":"ndb.neighbors","arguments":{{"uuid":"{a}"}}
                }}}}"#
            ),
        );
        let edges = nb["result"]["hyperedges"].as_array().unwrap();
        assert_eq!(edges.len(), 1, "entity a participates in exactly one edge");
        assert_eq!(edges[0]["kind"], "hyper_edge");
        assert_eq!(edges[0]["hyperedge_id"], hid);
        let roles = edges[0]["roles"].as_array().unwrap();
        assert_eq!(roles.len(), 2, "edge is binary");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn commit_hyperedge_rejects_zero_arity() {
        let dir = temp_dir("hyperedge_zero");
        let server = McpServer::open(&dir).unwrap();
        let resp = call(
            &server,
            r#"{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{
                "name":"ndb.commit_hyperedge","arguments":{"type_id":7,"roles":[]}
            }}"#,
        );
        // A hyperedge with no role-fillers is rejected before touching the engine.
        assert!(resp["error"].is_object(), "zero-arity must error");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_as_of_sees_history() {
        let dir = temp_dir("as_of");
        let server = McpServer::open(&dir).unwrap();
        let resp = call(
            &server,
            r#"{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{
                "name":"ndb.commit_entity",
                "arguments":{"type_id":5,"properties":[]}
            }}"#,
        );
        let tx = resp["result"]["tx_id"].as_u64().unwrap();
        let uuid = resp["result"]["entity_id"].as_str().unwrap().to_owned();

        // At the creating tx: live.
        let at = call(
            &server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":14,"method":"tools/call","params":{{
                    "name":"ndb.read_as_of","arguments":{{"uuid":"{uuid}","as_of_tx":{tx}}}
                }}}}"#
            ),
        );
        assert_eq!(at["result"]["outcome"], "live");
        assert_eq!(at["result"]["as_of_tx"], tx);

        // One tx earlier: the entity did not exist yet.
        let before = call(
            &server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":15,"method":"tools/call","params":{{
                    "name":"ndb.read_as_of","arguments":{{"uuid":"{uuid}","as_of_tx":{}}}
                }}}}"#,
                tx - 1
            ),
        );
        assert_eq!(before["result"]["outcome"], "missing");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tools_list_entries_carry_input_schema() {
        let dir = temp_dir("schemas");
        let server = McpServer::open(&dir).unwrap();
        let resp = call(&server, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
        let tools = resp["result"]["tools"].as_array().unwrap();
        for t in tools {
            let schema = &t["inputSchema"];
            assert_eq!(
                schema["type"], "object",
                "{} lacks object schema",
                t["name"]
            );
            assert!(
                schema["properties"].is_object(),
                "{} lacks properties",
                t["name"]
            );
            assert!(
                schema["required"].is_array(),
                "{} lacks required",
                t["name"]
            );
        }
        // commit_hyperedge requires only type_id (roles/hyperedge_roles optional).
        let che = tools
            .iter()
            .find(|t| t["name"] == "ndb.commit_hyperedge")
            .unwrap();
        let required: Vec<&str> = che["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["type_id"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn iter_paginates_with_cursor() {
        let dir = temp_dir("paginate");
        let server = McpServer::open(&dir).unwrap();
        for _ in 0..5 {
            commit_bare_entity(&server, 5);
        }
        // Page 1: limit 2 → 2 records + a next_cursor + pinned snapshot_tx.
        let p1 = call(
            &server,
            r#"{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{
                "name":"ndb.iter","arguments":{"limit":2}
            }}"#,
        );
        assert_eq!(p1["result"]["records"].as_array().unwrap().len(), 2);
        let next = p1["result"]["next_cursor"].as_u64().unwrap();
        assert_eq!(next, 2);
        let snap = p1["result"]["snapshot_tx"].as_u64().unwrap();

        // Walk the rest, pinning the same snapshot, until next_cursor is null.
        let mut seen = 2_usize;
        let mut cursor = next;
        loop {
            let p = call(
                &server,
                &format!(
                    r#"{{"jsonrpc":"2.0","id":21,"method":"tools/call","params":{{
                        "name":"ndb.iter",
                        "arguments":{{"limit":2,"cursor":{cursor},"snapshot_tx":{snap}}}
                    }}}}"#
                ),
            );
            seen += p["result"]["records"].as_array().unwrap().len();
            if let Some(n) = p["result"]["next_cursor"].as_u64() {
                cursor = n;
            } else {
                break;
            }
        }
        assert_eq!(seen, 5, "pagination visits every record exactly once");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resources_list_and_read_dictionaries() {
        let dir = temp_dir("resources");
        let server = McpServer::open(&dir).unwrap();
        let list = call(
            &server,
            r#"{"jsonrpc":"2.0","id":30,"method":"resources/list"}"#,
        );
        let uris: Vec<&str> = list["result"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["uri"].as_str().unwrap())
            .collect();
        assert!(uris.contains(&"ndb://dictionaries"));
        assert!(uris.contains(&"ndb://stats"));

        let read = call(
            &server,
            r#"{"jsonrpc":"2.0","id":31,"method":"resources/read","params":{"uri":"ndb://stats"}}"#,
        );
        let body = read["result"]["contents"][0]["text"].as_str().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        assert!(parsed["entities"].is_u64());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prompts_list_and_get() {
        let dir = temp_dir("prompts");
        let server = McpServer::open(&dir).unwrap();
        let list = call(
            &server,
            r#"{"jsonrpc":"2.0","id":40,"method":"prompts/list"}"#,
        );
        let names: Vec<&str> = list["result"]["prompts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"explore_entity"));

        let got = call(
            &server,
            r#"{"jsonrpc":"2.0","id":41,"method":"prompts/get","params":{
                "name":"explore_entity","arguments":{"uuid":"abc"}
            }}"#,
        );
        let text = got["result"]["messages"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("ndb.read"));
        assert!(text.contains("ndb.neighbors"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn arrow_export_tools_return_base64_ipc() {
        let dir = temp_dir("arrow");
        let server = McpServer::open(&dir).unwrap();
        // Commit an entity (type 5) with a 3-d vector on property 20.
        call(
            &server,
            r#"{"jsonrpc":"2.0","id":50,"method":"tools/call","params":{
                "name":"ndb.commit_entity",
                "arguments":{"type_id":5,"properties":[
                    {"prop_id":20,"value":{"tag":"vector","value":[1.0,2.0,3.0]}}
                ]}
            }}"#,
        );

        // The Arrow IPC stream begins with the 0xFFFFFFFF continuation marker;
        // decode the base64 and check it.
        let is_arrow = |b64: &str| {
            let bytes = BASE64.decode(b64).expect("valid base64");
            bytes.len() >= 4 && bytes[..4] == [0xFF, 0xFF, 0xFF, 0xFF]
        };

        let exp = call(
            &server,
            r#"{"jsonrpc":"2.0","id":51,"method":"tools/call","params":{"name":"ndb.arrow_export"}}"#,
        );
        assert_eq!(exp["result"]["encoding"], "base64");
        assert!(is_arrow(exp["result"]["data"].as_str().unwrap()));

        let vecs = call(
            &server,
            r#"{"jsonrpc":"2.0","id":52,"method":"tools/call","params":{
                "name":"ndb.arrow_vectors","arguments":{"type_id":5,"property_id":20}
            }}"#,
        );
        assert!(is_arrow(vecs["result"]["data"].as_str().unwrap()));

        let edges = call(
            &server,
            r#"{"jsonrpc":"2.0","id":53,"method":"tools/call","params":{"name":"ndb.arrow_edge_index"}}"#,
        );
        assert!(is_arrow(edges["result"]["data"].as_str().unwrap()));

        // Missing required params → JSON-RPC error.
        let bad = call(
            &server,
            r#"{"jsonrpc":"2.0","id":54,"method":"tools/call","params":{"name":"ndb.arrow_vectors"}}"#,
        );
        assert!(bad["error"].is_object());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
