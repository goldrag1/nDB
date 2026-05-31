//! # nStack kernel — Phase 1 vertical slice
//!
//! A compile-time-verified, n-dimensional application kernel built on the nDB
//! engine. This slice proves the load-bearing primitives of the platform thesis
//! against the *real* engine, with the full correctness loop running in-process:
//!
//! * **Typed nDB binding** — domain structs round-trip to nDB entities ([`store`]).
//! * **Compile-time currency safety** — [`money::Money`] makes cross-currency
//!   arithmetic a *compile error* (Level 1).
//! * **Compile-time lifecycle safety** — [`sales::SalesOrder`] encodes its state
//!   machine in the type system; illegal transitions don't compile (Level 1).
//! * **Commit-time invariants** — `SalesOrder::confirm` enforces
//!   `total == sum(lines)` and rejects violations.
//! * **In-process test harness** — [`testkit::TestDb`] spins an ephemeral engine
//!   in a temp dir; the whole `check -> test` loop runs in milliseconds, no
//!   server, no migrations, no browser.
//!
//! The `#[entity]` proc-macro that erases the hand-written boilerplate below is
//! the next slice; here the impls are explicit so the slice compiles and proves
//! the thesis end to end.

pub use ndb_engine::EntityId;

/// Kernel error type.
pub mod error {
    /// Errors surfaced by the kernel binding and domain invariants.
    #[derive(Debug)]
    pub enum KernelError {
        /// An error bubbled up from the nDB engine (stringified to avoid leaking
        /// the engine error type across the kernel boundary in this slice).
        Engine(String),
        /// A domain invariant was violated at commit time.
        Invariant(String),
        /// A stored record could not be decoded into the requested type.
        Decode(String),
    }

    impl std::fmt::Display for KernelError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                KernelError::Engine(s) => write!(f, "engine error: {s}"),
                KernelError::Invariant(s) => write!(f, "invariant violated: {s}"),
                KernelError::Decode(s) => write!(f, "decode error: {s}"),
            }
        }
    }

    impl std::error::Error for KernelError {}
}

/// Type-safe fixed-point money. The currency lives in the type, so the compiler
/// rejects cross-currency arithmetic and currency-mismatched assignments.
pub mod money {
    use core::marker::PhantomData;
    use ndb_engine::Value;

    /// A currency. `SCALE` is the number of fractional decimal places; it maps
    /// directly onto nDB's `Value::Decimal { scale, .. }`.
    pub trait Currency: Copy + core::fmt::Debug + PartialEq + Eq {
        /// ISO-style currency code.
        const CODE: &'static str;
        /// Decimal places (e.g. VND = 0, USD = 2).
        const SCALE: u8;
    }

    /// Vietnamese đồng (no minor unit).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct VND;
    impl Currency for VND {
        const CODE: &'static str = "VND";
        const SCALE: u8 = 0;
    }

    /// US dollar (cents).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct USD;
    impl Currency for USD {
        const CODE: &'static str = "USD";
        const SCALE: u8 = 2;
    }

    /// A monetary amount in currency `C`, stored as a signed `i128` mantissa.
    ///
    /// Cross-currency arithmetic is a compile error — there is no `Add` impl
    /// between `Money<VND>` and `Money<USD>`:
    ///
    /// ```compile_fail
    /// use nstack::money::{Money, VND, USD};
    /// let _bad = Money::<VND>::new(1_000) + Money::<USD>::new(1_000);
    /// ```
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Money<C: Currency> {
        mantissa: i128,
        _c: PhantomData<C>,
    }

    impl<C: Currency> Money<C> {
        /// Construct from a raw mantissa (units of `10^-SCALE`).
        pub const fn new(mantissa: i128) -> Self {
            Self { mantissa, _c: PhantomData }
        }

        /// The raw mantissa.
        pub const fn mantissa(self) -> i128 {
            self.mantissa
        }

        /// Project into an nDB `Value::Decimal` carrying the currency's scale.
        pub fn to_value(self) -> Value {
            Value::Decimal { scale: C::SCALE, mantissa: self.mantissa }
        }

        /// Recover from an nDB `Value::Decimal` — only if the scale matches `C`.
        pub fn from_value(v: &Value) -> Option<Self> {
            match v {
                Value::Decimal { scale, mantissa } if *scale == C::SCALE => {
                    Some(Self::new(*mantissa))
                }
                _ => None,
            }
        }
    }

    impl<C: Currency> core::ops::Add for Money<C> {
        type Output = Money<C>;
        fn add(self, rhs: Self) -> Self {
            Money::new(self.mantissa + rhs.mantissa)
        }
    }

    impl<C: Currency> core::iter::Sum for Money<C> {
        fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
            iter.fold(Money::new(0), |a, b| a + b)
        }
    }
}

/// The typed binding contract: a domain struct that can be stored as an nDB
/// entity and reconstructed from one.
pub mod entity {
    use ndb_engine::{EntityId, EntityRecord, TypeId};

    /// A persistable domain entity.
    pub trait Entity: Sized {
        /// The nDB type id this entity is stored under.
        const TYPE_ID: u32;
        /// Encode `self` into an nDB entity record under `id`.
        fn to_record(&self, id: EntityId) -> EntityRecord;
        /// Decode an nDB entity record back into `Self`, if it matches.
        fn from_record(rec: &EntityRecord) -> Option<Self>;
        /// The strongly-typed nDB type id.
        fn type_id() -> TypeId {
            TypeId::new(Self::TYPE_ID)
        }
    }
}

/// Typed read/write binding over an embedded nDB [`ndb_engine::Engine`].
pub mod store {
    use crate::entity::Entity;
    use crate::error::KernelError;
    use ndb_engine::record::Record;
    use ndb_engine::{Engine, EntityId, Resolved, TxId};

    /// A kernel store wrapping one embedded engine.
    pub struct Store {
        engine: Engine,
    }

    impl Store {
        /// Wrap an engine.
        pub fn new(engine: Engine) -> Self {
            Self { engine }
        }

        /// Insert a typed entity, returning its fresh id.
        pub fn insert<E: Entity>(&mut self, e: &E) -> Result<EntityId, KernelError> {
            let id = EntityId::now_v7();
            let mut txn = self.engine.begin_write();
            txn.put_entity(e.to_record(id));
            txn.commit().map_err(|err| KernelError::Engine(err.to_string()))?;
            Ok(id)
        }

        /// Read a typed entity by id at the latest snapshot.
        pub fn get<E: Entity>(&self, id: EntityId) -> Result<Option<E>, KernelError> {
            let snap = TxId::new(self.engine.manifest().last_tx_id);
            let resolved = self
                .engine
                .snapshot_read(&id.into_uuid(), snap)
                .map_err(|err| KernelError::Engine(err.to_string()))?;
            match resolved {
                Resolved::Live(Record::Entity(rec)) => Ok(E::from_record(&rec)),
                _ => Ok(None),
            }
        }
    }
}

/// A minimal example entity used to prove the typed nDB round-trip.
pub mod customer {
    use crate::entity::Entity;
    use ndb_engine::{EntityId, EntityRecord, PropertyId, TxId, TypeId, Value};

    const TYPE_CUSTOMER: u32 = 1;
    const P_NAME: u32 = 11;
    const P_EMAIL: u32 = 10;

    /// A customer master record.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Customer {
        /// Display name.
        pub name: String,
        /// Email (also the natural lookup key in a fuller build).
        pub email: String,
    }

    impl Entity for Customer {
        const TYPE_ID: u32 = TYPE_CUSTOMER;

        fn to_record(&self, id: EntityId) -> EntityRecord {
            EntityRecord {
                entity_id: id,
                type_id: TypeId::new(TYPE_CUSTOMER),
                tx_id_assert: TxId::new(0),
                tx_id_supersede: TxId::ACTIVE,
                properties: vec![
                    (PropertyId::new(P_NAME), Value::String(self.name.clone())),
                    (PropertyId::new(P_EMAIL), Value::String(self.email.clone())),
                ],
            }
        }

        fn from_record(rec: &EntityRecord) -> Option<Self> {
            let mut name = None;
            let mut email = None;
            for (p, v) in &rec.properties {
                match (p.get(), v) {
                    (P_NAME, Value::String(s)) => name = Some(s.clone()),
                    (P_EMAIL, Value::String(s)) => email = Some(s.clone()),
                    _ => {}
                }
            }
            Some(Customer { name: name?, email: email? })
        }
    }
}

/// A sales order whose lifecycle is encoded in the type system (typestate), with
/// a commit-time balance invariant.
pub mod sales {
    use crate::error::KernelError;
    use crate::money::{Money, VND};
    use crate::EntityId;
    use core::marker::PhantomData;

    /// Lifecycle state markers.
    pub mod state {
        /// Editable draft.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct Draft;
        /// Confirmed (balance invariant has passed).
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct Confirmed;
        /// Delivered.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct Delivered;
        /// Cancelled.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct Cancelled;
    }

    /// One order line.
    #[derive(Debug, Clone)]
    pub struct SalesOrderLine {
        /// The item entity.
        pub item: EntityId,
        /// Line amount (đồng).
        pub amount: Money<VND>,
    }

    /// A sales order parameterised by its lifecycle state `S`.
    ///
    /// Illegal transitions are compile errors — `deliver()` exists only on a
    /// `Confirmed` order, so this does not compile:
    ///
    /// ```compile_fail
    /// use nstack::sales::SalesOrder;
    /// use nstack::money::Money;
    /// use nstack::EntityId;
    /// let so = SalesOrder::draft(EntityId::now_v7(), Money::new(0));
    /// let _ = so.deliver(); // no `deliver` on SalesOrder<Draft>
    /// ```
    #[derive(Debug, Clone)]
    pub struct SalesOrder<S> {
        /// Customer entity.
        pub customer: EntityId,
        /// Declared order total (đồng).
        pub total: Money<VND>,
        /// Order lines.
        pub lines: Vec<SalesOrderLine>,
        _state: PhantomData<S>,
    }

    impl SalesOrder<state::Draft> {
        /// Start a new draft order with a declared total.
        pub fn draft(customer: EntityId, total: Money<VND>) -> Self {
            Self { customer, total, lines: Vec::new(), _state: PhantomData }
        }

        /// Append a line.
        pub fn add_line(&mut self, line: SalesOrderLine) {
            self.lines.push(line);
        }

        /// Confirm the order — enforces the invariant `total == sum(lines)`.
        pub fn confirm(self) -> Result<SalesOrder<state::Confirmed>, KernelError> {
            let computed: Money<VND> = self.lines.iter().map(|l| l.amount).sum();
            if computed != self.total {
                return Err(KernelError::Invariant(format!(
                    "declared total {:?} != sum(lines) {:?}",
                    self.total, computed
                )));
            }
            Ok(self.transmute())
        }

        /// Cancel a draft.
        pub fn cancel(self) -> SalesOrder<state::Cancelled> {
            self.transmute()
        }
    }

    impl SalesOrder<state::Confirmed> {
        /// Mark a confirmed order delivered.
        pub fn deliver(self) -> SalesOrder<state::Delivered> {
            self.transmute()
        }

        /// Cancel a confirmed order.
        pub fn cancel(self) -> SalesOrder<state::Cancelled> {
            self.transmute()
        }
    }

    impl<S> SalesOrder<S> {
        fn transmute<T>(self) -> SalesOrder<T> {
            SalesOrder {
                customer: self.customer,
                total: self.total,
                lines: self.lines,
                _state: PhantomData,
            }
        }
    }
}

/// In-process test harness: an ephemeral engine in a temp dir, cleaned up on
/// drop. This is the fast `check -> test` loop — no bench, no server, no browser.
pub mod testkit {
    use crate::store::Store;
    use ndb_engine::Engine;
    use std::path::PathBuf;

    /// An ephemeral kernel store for tests.
    pub struct TestDb {
        /// The store under test.
        pub store: Store,
        dir: PathBuf,
    }

    impl TestDb {
        /// Create a fresh ephemeral engine under the system temp dir.
        pub fn new() -> Self {
            let dir = std::env::temp_dir()
                .join(format!("nstack-test-{}", uuid::Uuid::now_v7().simple()));
            let engine = Engine::create(&dir).expect("create ephemeral engine");
            Self { store: Store::new(engine), dir }
        }
    }

    impl Default for TestDb {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Drop for TestDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::customer::Customer;
    use crate::money::{Money, VND};
    use crate::sales::{SalesOrder, SalesOrderLine};
    use crate::testkit::TestDb;
    use crate::EntityId;

    #[test]
    fn customer_round_trips_through_ndb() {
        let mut db = TestDb::new();
        let c = Customer { name: "Phú Quý".into(), email: "pq@example.com".into() };
        let id = db.store.insert(&c).unwrap();
        let got: Option<Customer> = db.store.get(id).unwrap();
        assert_eq!(got, Some(c));
    }

    #[test]
    fn money_sums_and_maps_to_decimal() {
        let parts = [Money::<VND>::new(300), Money::<VND>::new(700)];
        let total: Money<VND> = parts.into_iter().sum();
        assert_eq!(total, Money::<VND>::new(1_000));
        match total.to_value() {
            ndb_engine::Value::Decimal { scale, mantissa } => {
                assert_eq!(scale, 0);
                assert_eq!(mantissa, 1_000);
            }
            other => panic!("expected Decimal, got {other:?}"),
        }
    }

    #[test]
    fn lifecycle_legal_path_works() {
        let mut so = SalesOrder::draft(EntityId::now_v7(), Money::<VND>::new(1_000));
        so.add_line(SalesOrderLine { item: EntityId::now_v7(), amount: Money::new(600) });
        so.add_line(SalesOrderLine { item: EntityId::now_v7(), amount: Money::new(400) });
        let confirmed = so.confirm().expect("balanced order confirms");
        let _delivered = confirmed.deliver();
    }

    #[test]
    fn invariant_rejects_unbalanced_order() {
        let mut so = SalesOrder::draft(EntityId::now_v7(), Money::<VND>::new(999));
        so.add_line(SalesOrderLine { item: EntityId::now_v7(), amount: Money::new(600) });
        so.add_line(SalesOrderLine { item: EntityId::now_v7(), amount: Money::new(400) });
        assert!(so.confirm().is_err());
    }
}
