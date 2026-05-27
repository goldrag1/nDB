//! Identifier types and sentinel constants from §8 and §11.3.
//!
//! Internal identifiers are UUID v7 (time-ordered, 128 bits). External lookup
//! keys are *not* identifiers — they live in an index outside the record
//! store and resolve to UUIDs (§8).
//!
//! Dictionary references (`type_id`, `role_id`, `prop_id`) are `u32` slots
//! into three independent namespaces. The newtypes (`TypeId`, `RoleId`,
//! `PropertyId`) prevent accidental cross-namespace use at the type level.

use uuid::Uuid;

/// `type_id` sentinel meaning "no declared type". Legal on `EntityRecord`,
/// rejected on `HyperEdgeRecord` and dictionary records (§11.3).
pub const TYPE_UNTYPED: u32 = 0;

/// `tx_id_supersede` sentinel meaning "this assertion is still active"
/// (§11.3). Saves one byte per record vs `Option<u64>`; mirrors `PostgreSQL`'s
/// `xmax = 0` trick.
pub const TX_ACTIVE: u64 = u64::MAX;

// ---------------------------------------------------------------------------
// Dictionary-id newtypes
// ---------------------------------------------------------------------------

macro_rules! u32_newtype {
    ($(#[$attr:meta])* $name:ident) => {
        $(#[$attr])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub u32);

        impl $name {
            /// Wrap a raw `u32`.
            #[inline]
            pub const fn new(v: u32) -> Self { Self(v) }

            /// Extract the raw `u32`.
            #[inline]
            pub const fn get(self) -> u32 { self.0 }
        }

        impl From<u32> for $name {
            #[inline]
            fn from(v: u32) -> Self { Self(v) }
        }

        impl From<$name> for u32 {
            #[inline]
            fn from(v: $name) -> u32 { v.0 }
        }
    };
}

u32_newtype! {
    /// Index into the type-name dictionary (`TypeNameRecord`, kind `0x04`).
    /// `TypeId(TYPE_UNTYPED)` is legal on entities only.
    TypeId
}

u32_newtype! {
    /// Index into the role-name dictionary (`RoleNameRecord`, kind `0x05`).
    /// `RoleId(0)` is reserved and illegal everywhere.
    RoleId
}

u32_newtype! {
    /// Index into the property-key dictionary (`PropertyKeyRecord`, kind `0x06`).
    /// `PropertyId(0)` is reserved and illegal everywhere.
    PropertyId
}

impl TypeId {
    /// The `TYPE_UNTYPED` sentinel as a typed value.
    pub const UNTYPED: Self = Self(TYPE_UNTYPED);

    /// True iff this id is the `TYPE_UNTYPED` sentinel.
    #[inline]
    pub const fn is_untyped(self) -> bool {
        self.0 == TYPE_UNTYPED
    }
}

// ---------------------------------------------------------------------------
// Transaction id
// ---------------------------------------------------------------------------

/// MVCC transaction identifier (monotonic per database). `TxId::ACTIVE` is
/// the `TX_ACTIVE` sentinel for `tx_id_supersede` (§11.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TxId(pub u64);

impl TxId {
    /// The `TX_ACTIVE` sentinel as a typed value.
    pub const ACTIVE: Self = Self(TX_ACTIVE);

    /// Wrap a raw `u64`.
    #[inline]
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    /// Extract the raw `u64`.
    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// True iff this `TxId` is the `TX_ACTIVE` sentinel — i.e. the assertion
    /// has not yet been superseded by any later transaction.
    #[inline]
    pub const fn is_active_sentinel(self) -> bool {
        self.0 == TX_ACTIVE
    }
}

impl From<u64> for TxId {
    #[inline]
    fn from(v: u64) -> Self {
        Self(v)
    }
}

impl From<TxId> for u64 {
    #[inline]
    fn from(v: TxId) -> u64 {
        v.0
    }
}

// ---------------------------------------------------------------------------
// UUID v7 newtypes
// ---------------------------------------------------------------------------

macro_rules! uuid_newtype {
    ($(#[$attr:meta])* $name:ident) => {
        $(#[$attr])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub Uuid);

        impl $name {
            /// Wrap an existing UUID. No version check is performed; the caller
            /// is responsible for ensuring UUID v7 was used.
            #[inline]
            pub const fn from_uuid(u: Uuid) -> Self { Self(u) }

            /// Extract the wrapped UUID.
            #[inline]
            pub const fn into_uuid(self) -> Uuid { self.0 }

            /// Generate a fresh UUID v7 from the system clock.
            #[inline]
            pub fn now_v7() -> Self { Self(Uuid::now_v7()) }

            /// Borrow the 16 raw bytes (used by the on-disk codec).
            #[inline]
            pub fn as_bytes(&self) -> &[u8; 16] { self.0.as_bytes() }

            /// Construct from the 16 raw bytes (used by the on-disk codec).
            #[inline]
            pub fn from_bytes(b: [u8; 16]) -> Self { Self(Uuid::from_bytes(b)) }
        }
    };
}

uuid_newtype! {
    /// Internal UUID v7 of an entity record. Type-distinct from `HyperedgeId`
    /// so a hyperedge id can never be silently assigned to an entity slot.
    EntityId
}

uuid_newtype! {
    /// Internal UUID v7 of a hyperedge record.
    HyperedgeId
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn uuid_v7_is_time_ordered() {
        let a = EntityId::now_v7();
        sleep(Duration::from_millis(2));
        let b = EntityId::now_v7();
        assert!(b > a, "UUID v7 must be monotonic with wall-clock time");
    }

    #[test]
    fn newtypes_carry_value_without_collision() {
        let t = TypeId::new(42);
        let r = RoleId::new(42);
        let p = PropertyId::new(42);
        assert_eq!(t.get(), 42);
        assert_eq!(r.get(), 42);
        assert_eq!(p.get(), 42);
        // The compiler-enforced distinction is what we actually want — verified
        // by the fact that this file does not even compile if you swap the
        // arguments to a function that takes RoleId vs PropertyId.
    }

    #[test]
    fn type_untyped_sentinel() {
        assert!(TypeId::UNTYPED.is_untyped());
        assert_eq!(TypeId::UNTYPED.get(), 0);
        assert!(!TypeId::new(1).is_untyped());
    }

    #[test]
    fn tx_active_sentinel() {
        assert!(TxId::ACTIVE.is_active_sentinel());
        assert_eq!(TxId::ACTIVE.get(), u64::MAX);
        assert!(!TxId::new(0).is_active_sentinel());
        assert!(!TxId::new(1234).is_active_sentinel());
    }

    #[test]
    fn uuid_roundtrip_via_bytes() {
        let original = EntityId::now_v7();
        let bytes = *original.as_bytes();
        let restored = EntityId::from_bytes(bytes);
        assert_eq!(original, restored);
    }
}
