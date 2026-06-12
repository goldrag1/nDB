//! Capability hyperedges — persistent ReBAC store for the engine (§17.1, v2.0 §2.9).
#![allow(clippy::doc_markdown)] // "ReBAC" is a standard auth acronym, used liberally in module docs.
//!
//! v1 server kept its principals + capability map in `.principals.json`:
//! a JSON file loaded at startup, in-memory thereafter. That works for
//! a single process but it isn't part of the database — backups don't
//! capture it, replicas don't see it, the schema has no way to express
//! "this token's grants expire at T".
//!
//! v2.0 moves the capability set into the engine itself, modelled as
//! hyperedges of the reserved type [`TYPE_CAPABILITY`]. Each row says
//! "principal X can perform action Y against target Z, granted at GA,
//! expiring at EA" — `subject` is a role pointing at the principal
//! entity; the rest are properties.
//!
//! ## Reserved IDs
//!
//! All sit in the `0xFFFF_FFEx` reserved block — same convention used by
//! validation's `0xFFFF_FFFx`. Client-allocated ids stay well below
//! `0xFFFF_FF00` so collisions are structurally impossible.
//!
//! | Const | Value | Meaning |
//! |-------|-------|---------|
//! | [`TYPE_PRINCIPAL`] | `0xFFFF_FFE0` | Type of the principal entity (1 per token) |
//! | [`PROP_PRINCIPAL_NAME`] | `0xFFFF_FFE1` | Display name (string) |
//! | [`PROP_PRINCIPAL_TOKEN`] | `0xFFFF_FFE2` | Bearer token (string) — opaque |
//! | [`TYPE_CAPABILITY`] | `0xFFFF_FFE3` | Capability hyperedge type |
//! | [`ROLE_SUBJECT`] | `0xFFFF_FFE4` | "Who" — points to the principal entity |
//! | [`PROP_ACTION`] | `0xFFFF_FFE5` | "What" — string (`"read"`, `"commit"`, …) |
//! | [`PROP_TARGET`] | `0xFFFF_FFE6` | "On what" — string (`"*"` or a path) |
//! | [`PROP_GRANTED_AT`] | `0xFFFF_FFE7` | Unix microseconds (timestamp) |
//! | [`PROP_EXPIRES_AT`] | `0xFFFF_FFE8` | Unix microseconds (0 = never) |
//!
//! ## Matching semantics
//!
//! - **Action** matches when the property equals the queried action, OR
//!   when the property is the wildcard `"*"`.
//! - **Target** matches when the property equals the queried target, OR
//!   when the property is the wildcard `"*"`.
//! - **Expiry** matches when `expires_at == 0` (never expires) OR
//!   `expires_at > now_us`.
//!
//! Wildcards on both axes let the operator grant "admin" via a single
//! capability with `action = "*"` and `target = "*"`. There's no implicit
//! transitive grant — every check looks for a single matching hyperedge.

use crate::engine::Engine;
use crate::id::{EntityId, PropertyId, RoleId, TxId, TypeId};
use crate::mvcc::Resolved;
use crate::record::Record;
use crate::value::Value;

/// Reserved type id for the principal entity. One entity per bearer
/// token; carries `PROP_PRINCIPAL_NAME` + `PROP_PRINCIPAL_TOKEN`.
pub const TYPE_PRINCIPAL: TypeId = TypeId::new(0xFFFF_FFE0);

/// Display name stamped on every principal entity.
pub const PROP_PRINCIPAL_NAME: PropertyId = PropertyId::new(0xFFFF_FFE1);

/// Opaque bearer token; constant-time-compared at auth time.
pub const PROP_PRINCIPAL_TOKEN: PropertyId = PropertyId::new(0xFFFF_FFE2);

/// Reserved type id for capability hyperedges.
pub const TYPE_CAPABILITY: TypeId = TypeId::new(0xFFFF_FFE3);

/// Role: the principal entity granted by this capability.
pub const ROLE_SUBJECT: RoleId = RoleId::new(0xFFFF_FFE4);

/// Action this capability allows. String value; `"*"` is wildcard.
pub const PROP_ACTION: PropertyId = PropertyId::new(0xFFFF_FFE5);

/// Target this capability authorises. String value; `"*"` is wildcard.
pub const PROP_TARGET: PropertyId = PropertyId::new(0xFFFF_FFE6);

/// Unix microseconds when the capability was granted. Informational —
/// not used for matching, but useful in audit / debugging.
pub const PROP_GRANTED_AT: PropertyId = PropertyId::new(0xFFFF_FFE7);

/// Unix microseconds when the capability expires. `0` means never.
pub const PROP_EXPIRES_AT: PropertyId = PropertyId::new(0xFFFF_FFE8);

/// The wildcard string used by [`PROP_ACTION`] and [`PROP_TARGET`].
pub const WILDCARD: &str = "*";

// ---------------------------------------------------------------------------
// Engine-side capability check
// ---------------------------------------------------------------------------

impl Engine {
    /// Does `subject` hold a live capability authorising `action` against
    /// `target` at time `now_us`?
    ///
    /// Walks the adjacency index for hyperedges incident on `subject`,
    /// filters to type [`TYPE_CAPABILITY`], and matches each by action
    /// (wildcard-aware), target (wildcard-aware), and expiry. Returns
    /// `true` on the first match.
    ///
    /// `now_us` is unix microseconds. Pass `0` to disable expiry checks
    /// (treat every non-expired capability as live).
    ///
    /// Reads happen at the latest committed snapshot — capability
    /// changes via `/commit` become effective on the next `has_capability`
    /// call.
    pub fn has_capability(
        &self,
        subject: EntityId,
        action: &str,
        target: &str,
        now_us: i64,
    ) -> Result<bool, crate::engine::EngineError> {
        let snapshot = TxId::new(self.manifest().last_tx_id);
        let edges = self.hyperedges_for_entity(subject);
        for hid in edges {
            let resolved = self.snapshot_read(&hid.into_uuid(), snapshot)?;
            let Resolved::Live(Record::HyperEdge(h)) = resolved else {
                continue;
            };
            if h.type_id != TYPE_CAPABILITY {
                continue;
            }
            // Sanity: subject must appear in ROLE_SUBJECT on this edge.
            let is_subject = h
                .roles
                .iter()
                .any(|(rid, eid)| *rid == ROLE_SUBJECT && *eid == subject);
            if !is_subject {
                continue;
            }
            if match_capability(&h.properties, action, target, now_us) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Resolve a bearer token to its principal entity, if any. Compares
    /// every principal entity's `PROP_PRINCIPAL_TOKEN` against `token`
    /// in constant time per candidate.
    ///
    /// Returns `(entity_id, name)` on hit. `None` if no match.
    pub fn principal_by_token(
        &mut self,
        token: &str,
    ) -> Result<Option<(EntityId, String)>, crate::engine::EngineError> {
        let snapshot = TxId::new(self.manifest().last_tx_id);
        let mut found: Option<(EntityId, String)> = None;
        let stream = self.snapshot_iter_streaming(snapshot);
        for rec in stream {
            let rec = rec?;
            let Record::Entity(e) = rec else { continue };
            if e.type_id != TYPE_PRINCIPAL {
                continue;
            }
            let mut tok: Option<String> = None;
            let mut name: Option<String> = None;
            for (pid, val) in &e.properties {
                if *pid == PROP_PRINCIPAL_TOKEN
                    && let Value::String(s) = val
                {
                    tok = Some(s.clone());
                }
                if *pid == PROP_PRINCIPAL_NAME
                    && let Value::String(s) = val
                {
                    name = Some(s.clone());
                }
            }
            if let (Some(t), Some(n)) = (tok, name)
                && constant_time_eq(t.as_bytes(), token.as_bytes())
            {
                // Don't break early — keep scanning so a partial-prefix
                // collision doesn't short-circuit to the wrong row.
                found = Some((e.entity_id, n));
            }
        }
        Ok(found)
    }

    /// Whether the engine has ANY capability hyperedge or principal
    /// entity. Used by the server's bootstrap-import flow to decide
    /// "should I migrate principals.json now?" without reading every
    /// record.
    pub fn has_any_capability_or_principal(&self) -> Result<bool, crate::engine::EngineError> {
        if self.hyperedge_type_count(TYPE_CAPABILITY) > 0 {
            return Ok(true);
        }
        // No type cluster for entities yet; do a snapshot scan but break
        // on the first principal hit. Cheap on small DBs; for a large
        // DB with no principals at all this scan is the worst case, and
        // the operator can pre-seed the engine to skip the bootstrap.
        let snapshot = TxId::new(self.manifest().last_tx_id);
        let stream = self.snapshot_iter_streaming(snapshot);
        for rec in stream {
            if let Record::Entity(e) = rec?
                && e.type_id == TYPE_PRINCIPAL
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn match_capability(
    properties: &[(PropertyId, Value)],
    action: &str,
    target: &str,
    now_us: i64,
) -> bool {
    let mut got_action: Option<&str> = None;
    let mut got_target: Option<&str> = None;
    let mut expires_at: i64 = 0;
    for (pid, val) in properties {
        if *pid == PROP_ACTION
            && let Value::String(s) = val
        {
            got_action = Some(s.as_str());
        }
        if *pid == PROP_TARGET
            && let Value::String(s) = val
        {
            got_target = Some(s.as_str());
        }
        if *pid == PROP_EXPIRES_AT
            && let Value::Timestamp(ts) | Value::I64(ts) = val
        {
            expires_at = *ts;
        }
    }
    let action_ok = got_action.is_some_and(|s| s == WILDCARD || s == action);
    let target_ok = got_target.is_some_and(|s| s == WILDCARD || s == target);
    let expiry_ok = expires_at == 0 || now_us == 0 || expires_at > now_us;
    action_ok && target_ok && expiry_ok
}

/// Constant-time byte comparison. Resists timing side-channels when
/// comparing secrets (bearer tokens).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Engine;
    use crate::id::{HyperedgeId, TxId};
    use crate::record::{EntityRecord, HyperEdgeRecord};

    fn temp_engine(name: &str) -> Engine {
        let dir = std::env::temp_dir().join(format!("ndb-cap-{name}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        Engine::create(&dir).unwrap()
    }

    fn seed_principal(engine: &mut Engine, name: &str, token: &str) -> EntityId {
        let eid = EntityId::now_v7();
        let mut txn = engine.begin_write();
        txn.put_entity(EntityRecord {
            entity_id: eid,
            type_id: TYPE_PRINCIPAL,
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![
                (PROP_PRINCIPAL_NAME, Value::String(name.into())),
                (PROP_PRINCIPAL_TOKEN, Value::String(token.into())),
            ],
        });
        txn.commit().unwrap();
        eid
    }

    fn seed_capability(
        engine: &mut Engine,
        subject: EntityId,
        action: &str,
        target: &str,
        expires_at: i64,
    ) {
        let mut txn = engine.begin_write();
        txn.put_hyperedge(HyperEdgeRecord {
            hyperedge_id: HyperedgeId::now_v7(),
            type_id: TYPE_CAPABILITY,
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            roles: vec![(ROLE_SUBJECT, subject)],
            hyperedge_roles: Vec::new(),
            properties: vec![
                (PROP_ACTION, Value::String(action.into())),
                (PROP_TARGET, Value::String(target.into())),
                (PROP_GRANTED_AT, Value::Timestamp(1_000_000)),
                (PROP_EXPIRES_AT, Value::Timestamp(expires_at)),
            ],
        });
        txn.commit().unwrap();
    }

    #[test]
    fn has_capability_returns_true_on_exact_match() {
        let mut engine = temp_engine("exact");
        let alice = seed_principal(&mut engine, "alice", "token-a");
        seed_capability(&mut engine, alice, "read", "/iter", 0);
        assert!(
            engine
                .has_capability(alice, "read", "/iter", 1_000)
                .unwrap()
        );
    }

    #[test]
    fn has_capability_action_wildcard() {
        let mut engine = temp_engine("wild_a");
        let alice = seed_principal(&mut engine, "alice", "tok");
        seed_capability(&mut engine, alice, WILDCARD, "/iter", 0);
        assert!(engine.has_capability(alice, "read", "/iter", 0).unwrap());
        assert!(engine.has_capability(alice, "commit", "/iter", 0).unwrap());
    }

    #[test]
    fn has_capability_target_wildcard() {
        let mut engine = temp_engine("wild_t");
        let alice = seed_principal(&mut engine, "alice", "tok");
        seed_capability(&mut engine, alice, "read", WILDCARD, 0);
        assert!(engine.has_capability(alice, "read", "/iter", 0).unwrap());
        assert!(
            engine
                .has_capability(alice, "read", "/read/foo", 0)
                .unwrap()
        );
    }

    #[test]
    fn has_capability_admin_double_wildcard() {
        let mut engine = temp_engine("admin");
        let bob = seed_principal(&mut engine, "bob", "tok");
        seed_capability(&mut engine, bob, WILDCARD, WILDCARD, 0);
        assert!(engine.has_capability(bob, "read", "/iter", 0).unwrap());
        assert!(engine.has_capability(bob, "commit", "/commit", 0).unwrap());
    }

    #[test]
    fn has_capability_returns_false_when_no_grant() {
        let mut engine = temp_engine("no_grant");
        let stranger = seed_principal(&mut engine, "stranger", "tok");
        assert!(!engine.has_capability(stranger, "read", "/iter", 0).unwrap());
    }

    #[test]
    fn has_capability_action_mismatch_returns_false() {
        let mut engine = temp_engine("action_no");
        let alice = seed_principal(&mut engine, "alice", "tok");
        seed_capability(&mut engine, alice, "read", "/iter", 0);
        assert!(!engine.has_capability(alice, "commit", "/iter", 0).unwrap());
    }

    #[test]
    fn has_capability_expired_returns_false() {
        let mut engine = temp_engine("expired");
        let alice = seed_principal(&mut engine, "alice", "tok");
        seed_capability(&mut engine, alice, "read", "/iter", 500_000);
        // now > expires → reject
        assert!(
            !engine
                .has_capability(alice, "read", "/iter", 1_000_000)
                .unwrap()
        );
        // now < expires → allow
        assert!(
            engine
                .has_capability(alice, "read", "/iter", 100_000)
                .unwrap()
        );
        // expires=0 → always allow (when seeded so)
        seed_capability(&mut engine, alice, "read", "/iter", 0);
        assert!(
            engine
                .has_capability(alice, "read", "/iter", 9_999_999)
                .unwrap()
        );
    }

    #[test]
    fn principal_by_token_resolves_after_commit() {
        let mut engine = temp_engine("tok_resolve");
        let alice = seed_principal(&mut engine, "Alice", "tok-alice");
        let bob = seed_principal(&mut engine, "Bob", "tok-bob");
        assert_eq!(
            engine.principal_by_token("tok-alice").unwrap(),
            Some((alice, "Alice".into()))
        );
        assert_eq!(
            engine.principal_by_token("tok-bob").unwrap(),
            Some((bob, "Bob".into()))
        );
        assert_eq!(engine.principal_by_token("tok-nobody").unwrap(), None);
    }

    #[test]
    fn has_any_capability_or_principal_starts_empty_then_true() {
        let mut engine = temp_engine("any");
        assert!(!engine.has_any_capability_or_principal().unwrap());
        let _ = seed_principal(&mut engine, "p", "tok");
        assert!(engine.has_any_capability_or_principal().unwrap());
    }

    #[test]
    fn capability_persists_across_engine_restart() {
        let dir = std::env::temp_dir().join(format!("ndb-cap-restart-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let alice;
        {
            let mut engine = Engine::create(&dir).unwrap();
            alice = seed_principal(&mut engine, "alice", "tok");
            seed_capability(&mut engine, alice, "read", "/iter", 0);
            engine.flush().unwrap();
            engine.close().unwrap();
        }
        let mut engine = Engine::open(&dir).unwrap();
        assert!(engine.has_capability(alice, "read", "/iter", 0).unwrap());
        assert_eq!(
            engine.principal_by_token("tok").unwrap().map(|(_, n)| n),
            Some("alice".into())
        );
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
