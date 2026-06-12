//! Local identity: password hashing + an in-memory session store.
//!
//! Accounts themselves live as records in the database (see [`crate::store`]);
//! this module owns only the pure crypto — salted, iterated SHA-256 — and the
//! process-local session map (token → who, with an expiry). Sessions are
//! intentionally in-memory: a restart simply asks everyone to log in again.
//!
//! Local accounts only; an external identity provider (`OIDC`/`SAML`) is out of scope per the
//! design spec. The hash is a straightforward salted PBKDF-style loop — fine
//! for a self-hosted, single-writer app; it is not argon2.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

const ITERS: u32 = 100_000;
const SESSION_TTL_SECS: u64 = 12 * 3600;

/// A user's role, ordered by privilege.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Read-only.
    Viewer,
    /// Read + create/edit/delete records.
    Editor,
    /// Everything, plus user administration.
    Admin,
}

impl Role {
    /// Canonical lowercase name as stored on the user record.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Editor => "editor",
            Self::Admin => "admin",
        }
    }

    /// Parse a stored role string, defaulting unknown values to `Viewer`.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            "admin" => Self::Admin,
            "editor" => Self::Editor,
            _ => Self::Viewer,
        }
    }

    /// May this role create/edit/delete records?
    #[must_use]
    pub fn can_write(self) -> bool {
        matches!(self, Self::Editor | Self::Admin)
    }

    /// May this role administer users?
    #[must_use]
    pub fn is_admin(self) -> bool {
        matches!(self, Self::Admin)
    }
}

/// `N` cryptographically-random bytes from the OS CSPRNG.
fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).expect("OS RNG unavailable");
    b
}

/// Hash a password into the storable `sha256$<iters>$<salt>$<hash>` string.
#[must_use]
pub fn hash_password(password: &str) -> String {
    let salt: [u8; 16] = random_bytes();
    let digest = derive(password.as_bytes(), &salt, ITERS);
    format!("sha256${ITERS}${}${}", hex(&salt), hex(&digest))
}

/// Verify a password against a stored hash string. Constant-time on the digest.
#[must_use]
pub fn verify_password(password: &str, stored: &str) -> bool {
    let mut it = stored.split('$');
    if it.next() != Some("sha256") {
        return false;
    }
    let Some(iters) = it.next().and_then(|s| s.parse::<u32>().ok()) else {
        return false;
    };
    let (Some(salt_hex), Some(hash_hex)) = (it.next(), it.next()) else {
        return false;
    };
    let (Some(salt), Some(expected)) = (unhex(salt_hex), unhex(hash_hex)) else {
        return false;
    };
    constant_eq(&derive(password.as_bytes(), &salt, iters), &expected)
}

/// A friendly random password for the bootstrap admin (16 hex chars).
#[must_use]
pub fn random_password() -> String {
    hex(&random_bytes::<8>())
}

fn derive(password: &[u8], salt: &[u8], iters: u32) -> [u8; 32] {
    // h0 = SHA256(salt || password); h_{i+1} = SHA256(h_i || salt).
    let mut h = {
        let mut s = Sha256::new();
        s.update(salt);
        s.update(password);
        s.finalize()
    };
    for _ in 1..iters {
        let mut s = Sha256::new();
        s.update(h);
        s.update(salt);
        h = s.finalize();
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&h);
    out
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// One logged-in session.
#[derive(Clone)]
pub struct Session {
    /// The authenticated user.
    pub username: String,
    /// Their role at login time.
    pub role: Role,
    /// Unix-seconds expiry.
    pub expires: u64,
}

/// Process-local token → session map.
pub struct Sessions {
    inner: Mutex<HashMap<String, Session>>,
}

impl Default for Sessions {
    fn default() -> Self {
        Self::new()
    }
}

impl Sessions {
    /// An empty session store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Issue a fresh opaque token for a user; returns the token.
    pub fn issue(&self, username: &str, role: Role) -> String {
        let token = hex(&random_bytes::<32>());
        let s = Session {
            username: username.to_string(),
            role,
            expires: now() + SESSION_TTL_SECS,
        };
        self.inner
            .lock()
            .expect("sessions poisoned")
            .insert(token.clone(), s);
        token
    }

    /// Resolve a token to a live session, dropping it if expired.
    #[must_use]
    pub fn lookup(&self, token: &str) -> Option<Session> {
        let mut m = self.inner.lock().expect("sessions poisoned");
        match m.get(token).cloned() {
            Some(s) if s.expires > now() => Some(s),
            Some(_) => {
                m.remove(token);
                None
            }
            None => None,
        }
    }

    /// Drop a session (logout).
    pub fn revoke(&self, token: &str) {
        self.inner.lock().expect("sessions poisoned").remove(token);
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_verifies_and_rejects() {
        let h = hash_password("hunter2");
        assert!(verify_password("hunter2", &h));
        assert!(!verify_password("wrong", &h));
        assert!(!verify_password("hunter2", "garbage"));
        // Two hashes of the same password differ (random salt).
        assert_ne!(hash_password("x"), hash_password("x"));
    }

    #[test]
    fn role_parse_and_privilege() {
        assert_eq!(Role::parse("admin"), Role::Admin);
        assert_eq!(Role::parse("nonsense"), Role::Viewer);
        assert!(Role::Admin.is_admin() && Role::Admin.can_write());
        assert!(Role::Editor.can_write() && !Role::Editor.is_admin());
        assert!(!Role::Viewer.can_write());
    }

    #[test]
    fn sessions_issue_lookup_revoke() {
        let s = Sessions::new();
        let tok = s.issue("alice", Role::Editor);
        let live = s.lookup(&tok).expect("live");
        assert_eq!(live.username, "alice");
        assert_eq!(live.role, Role::Editor);
        s.revoke(&tok);
        assert!(s.lookup(&tok).is_none());
        assert!(s.lookup("nope").is_none());
    }
}
