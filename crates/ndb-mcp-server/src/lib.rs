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
//! - **Tools, not resources or prompts.** The full MCP spec also
//!   models resources (read-only context blobs) and prompts (chat
//!   templates). v1 ships only `tools/*` since that's the minimum
//!   surface to make nDB usable from an agent.
//!
//! Tools exposed (call via `tools/call` with `{"name": "<tool>",
//! "arguments": {...}}`):
//!
//! - `ndb.health` — `{}` → `{"status": "ok", ...}`
//! - `ndb.read` — `{"uuid": "..."}` → `ReadResponse`
//! - `ndb.commit_entity` — `{"type_id", "properties":[{prop_id, value}]}`
//!   → `{"tx_id", "entity_id"}`
//! - `ndb.iter` — `{}` → array of records (capped at 1000 in v1; full
//!   streaming MCP shape is a v2 conversation)
//! - `ndb.lookup_by_key` — `{"property_id", "value"}` → entity uuid or null
//! - `ndb.vector_search` — `{"property_id", "query":[f32], "k", "metric"}`
//!   → list of `{entity_id, distance}` sorted ascending
//! - `ndb.property_lookup` — `{"type_id", "property_id", "value"}` →
//!   list of entity uuids (exact match)
//! - `ndb.property_range` — `{"type_id", "property_id", "low"?, "high"?}`
//!   → list of entity uuids (range)
//!
//! All wire payloads use the same JSON shape as ndb-engine::wire.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ndb_engine::{
    Distance, Engine, EngineError, EntityId, EntityRecord, JsonRecord, JsonValue, PropertyId,
    Resolved, TxId, TypeId, Value,
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
        | "ndb.lookup_by_key"
        | "ndb.vector_search"
        | "ndb.property_lookup"
        | "ndb.property_range" => Some(Capability::Read),
        "ndb.iter" => Some(Capability::Iter),
        "ndb.commit_entity" => Some(Capability::Commit),
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
    engine: Arc<Mutex<Engine>>,
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
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, McpError> {
        let path = path.as_ref();
        let engine = if path.exists() && path.join("CURRENT").exists() {
            Engine::open(path)?
        } else {
            Engine::create(path)?
        };
        Ok(Self {
            engine: Arc::new(Mutex::new(engine)),
            principal: None,
            audit: None,
        })
    }

    /// Wrap an existing engine — useful for tests.
    #[must_use]
    pub fn from_engine(engine: Engine) -> Self {
        Self {
            engine: Arc::new(Mutex::new(engine)),
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
            let eng = self.engine.lock().expect("engine mutex poisoned");
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

    fn record_audit(
        &self,
        principal: &str,
        tool: &str,
        status: u16,
        failure: Option<&str>,
    ) {
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
    pub fn engine(&self) -> Arc<Mutex<Engine>> {
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
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "ndb-mcp-server", "version": env!("CARGO_PKG_VERSION")},
                }),
            ),
            "tools/list" => JsonRpcResponse::ok(id, serde_json::json!({"tools": tool_list()})),
            "tools/call" => match self.handle_tool_call(&req.params) {
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
            "ndb.iter" => self.tool_iter(args),
            "ndb.lookup_by_key" => self.tool_lookup_by_key(args),
            "ndb.vector_search" => self.tool_vector_search(args),
            "ndb.property_lookup" => self.tool_property_lookup(args),
            "ndb.property_range" => self.tool_property_range(args),
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
        let mut engine = self.engine.lock().expect("engine mutex poisoned");
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
        let mut engine = self.engine.lock().expect("engine mutex poisoned");
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

    fn tool_iter(&self, args: &serde_json::Value) -> Result<serde_json::Value, McpError> {
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(1000_usize, |n| n.try_into().unwrap_or(1000));
        let mut engine = self.engine.lock().expect("engine mutex poisoned");
        let snap = TxId::new(engine.manifest().last_tx_id);
        let mut records = engine.snapshot_iter(snap)?;
        if records.len() > limit {
            records.truncate(limit);
        }
        let payload: Vec<JsonRecord> = records.iter().map(JsonRecord::from).collect();
        Ok(serde_json::json!({"records": payload}))
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
        let engine = self.engine.lock().expect("engine mutex poisoned");
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
        let engine = self.engine.lock().expect("engine mutex poisoned");
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
        let engine = self.engine.lock().expect("engine mutex poisoned");
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
        let engine = self.engine.lock().expect("engine mutex poisoned");
        let hits = engine.property_range(type_id, property_id, low.as_ref(), high.as_ref());
        Ok(serde_json::json!({
            "entity_ids": hits
                .into_iter()
                .map(|id| id.into_uuid().to_string())
                .collect::<Vec<_>>(),
        }))
    }
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

fn tool_list() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({"name": "ndb.health", "description": "liveness probe"}),
        serde_json::json!({"name": "ndb.read", "description": "look up a UUID at the latest snapshot"}),
        serde_json::json!({"name": "ndb.commit_entity", "description": "commit a new entity with a type and properties"}),
        serde_json::json!({"name": "ndb.iter", "description": "list visible records (capped at 'limit' or 1000)"}),
        serde_json::json!({"name": "ndb.lookup_by_key", "description": "find an entity by external lookup-key value"}),
        serde_json::json!({"name": "ndb.vector_search", "description": "k-NN search over a vector-indexed property"}),
        serde_json::json!({"name": "ndb.property_lookup", "description": "exact match on (type, property, value)"}),
        serde_json::json!({"name": "ndb.property_range", "description": "range query on (type, property) with low/high bounds"}),
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
            let mut e = e.lock().unwrap();
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
}
