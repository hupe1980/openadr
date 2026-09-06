//! Storage: the trait the VTN talks to, and the backends behind it.
//!
//! Every object is a document with a handful of indexed attributes — id, owner, targets, programme,
//! name, and the window it is active over. That is exactly the shape the API queries, so the trait
//! below is deliberately narrow and all three backends store the same thing.
//!
//! Every list method follows the same three steps, in this order: **filter, order, paginate**.
//! Both halves are correctness properties, not performance ones: filtering after a page is cut
//! silently drops records, and paginating without an order returns non-repeatable pages.
//!
//! What keeps the backends interchangeable is `suite` — the behaviours every one of them runs.
//! A rule it does not name is a rule they may differ on.

use async_trait::async_trait;
use std::sync::Arc;

use crate::core::{Access, Grant};
use crate::model::{
    ClientId, ClientName, Event, EventRequest, ObjectId, ObjectType, Program, ProgramName,
    ProgramRequest, Report, ReportRequest, Resource, ResourceName, Subscription,
    SubscriptionRequest, Timestamp, Ven, VenName,
};
use crate::vtn::notify::Fanout;

mod memory;
mod outbox;
#[cfg(feature = "postgres")]
mod postgres;
#[cfg(any(feature = "sqlite", feature = "postgres"))]
mod sql;
#[cfg(feature = "sqlite")]
mod sqlite;
#[cfg(test)]
mod suite;

pub use memory::MemoryStorage;
pub use outbox::{
    BreakerPolicy, DeadLetter, OutboxId, OutboxStats, Queued, RetryPolicy, SubscriberHealth,
};
#[cfg(feature = "postgres")]
#[cfg_attr(docsrs, doc(cfg(feature = "postgres")))]
pub use postgres::PostgresStorage;
#[cfg(feature = "sqlite")]
#[cfg_attr(docsrs, doc(cfg(feature = "sqlite")))]
pub use sqlite::SqliteStorage;

/// Why a storage operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    /// No object with that id.
    #[error("{object_type} {id} not found")]
    NotFound {
        /// The kind of object.
        object_type: ObjectType,
        /// The id that was looked up.
        id: ObjectId,
    },
    /// A uniqueness constraint would be violated.
    #[error("{object_type} with {field} {value:?} already exists")]
    Conflict {
        /// The kind of object.
        object_type: ObjectType,
        /// The unique field.
        field: &'static str,
        /// The duplicate value.
        value: String,
    },
    /// A referenced object does not exist.
    #[error("{field} {value:?} does not reference an existing object")]
    DanglingReference {
        /// The referencing field.
        field: &'static str,
        /// The value that pointed nowhere.
        value: String,
    },
    /// The backend is unavailable.
    #[error("storage backend unavailable: {0}")]
    Unavailable(String),
}

/// Pagination, shared by every collection endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    /// How many records to skip.
    pub skip: usize,
    /// How many to return. The schema caps this at 50.
    pub limit: usize,
}

impl Default for Page {
    fn default() -> Self {
        Self {
            skip: 0,
            limit: Self::MAX_LIMIT,
        }
    }
}

impl Page {
    /// The schema's `maximum` for `limit`.
    ///
    /// The wire model's [`MAX_PAGE_LIMIT`](crate::model::MAX_PAGE_LIMIT), not a second copy of it:
    /// the client pages with the same number and the VEN runtime compares against it.
    pub const MAX_LIMIT: usize = crate::model::MAX_PAGE_LIMIT;

    /// Apply to an already-ordered slice.
    ///
    /// Offset pagination over an unordered set returns arbitrary, non-repeatable pages; callers
    /// must sort first. [`order_by_creation`] is the ordering every collection uses.
    pub fn apply<T: Clone>(&self, items: &[T]) -> Vec<T> {
        items
            .iter()
            .skip(self.skip)
            .take(self.limit)
            .cloned()
            .collect()
    }
}

/// The identifier prefix for a kind of object.
///
/// Prefixed so a failing test says `evt-00000003` rather than `3`.
pub fn prefix(kind: ObjectType) -> &'static str {
    match kind {
        ObjectType::Program => "prg",
        ObjectType::Event => "evt",
        ObjectType::Report => "rpt",
        ObjectType::Subscription => "sub",
        ObjectType::Ven => "ven",
        ObjectType::Resource => "res",
    }
}

/// Build the identifier for the `n`th object of a kind.
///
/// Here rather than beside a backend, because all three call it and a fixture written against one
/// has to read the same against the others. It did not: the in-memory backend had its own copy of
/// the prefix table and one counter shared across *every* type, so the same three writes produced
/// `prg-00000000, evt-00000001, evt-00000002` there and `prg-00000001, evt-00000001, evt-00000002`
/// on SQL. Harmless — an identifier is opaque — and still a second implementation of a rule, which
/// is the shape every other divergence in this crate has had.
pub fn sequenced_id(kind: ObjectType, n: i64) -> ObjectId {
    ObjectId::new(format!("{}-{n:08}", prefix(kind))).expect("generated ids are valid")
}

/// The order every collection is returned in: oldest first, ties broken by id.
///
/// The specification does not prescribe an order, but offset pagination without one is unstable —
/// two identical requests may return different pages. Creation order also means an append-only
/// collection never reshuffles the pages a client has already walked.
pub fn order_by_creation<T>(items: &mut [T], key: impl Fn(&T) -> (Timestamp, &ObjectId)) {
    items.sort_by(|a, b| {
        let (a_time, a_id) = key(a);
        let (b_time, b_id) = key(b);
        a_time.cmp(&b_time).then_with(|| a_id.cmp(b_id))
    });
}

/// Filters for `GET /programs`.
///
/// Every query carries the [`Access`] of the caller, because **filtering has to happen before
/// pagination**. Cutting a page and then dropping records from it produces short pages and silently
/// lost data: a VEN entitled to one of two requested targets would see a handful of its events, and
/// a client that stops on a short page never learns the rest exist.
#[derive(Debug, Clone)]
pub struct ProgramQuery {
    /// Exact-match a programme name.
    ///
    /// Not declared by `openadr3.yaml`. Paginating hundreds of tariffs to find one by name is the
    /// difference between one request and twenty, which is why this VTN accepts it.
    pub program_name: Option<ProgramName>,
    /// Who is asking, and for which targets.
    pub access: Access,
    /// Pagination, applied last.
    pub page: Page,
}

/// Filters for `GET /events`.
#[derive(Debug, Clone)]
pub struct EventQuery {
    /// Restrict to one programme.
    pub program_id: Option<ObjectId>,
    /// Drop events whose intervals have all elapsed by this instant.
    pub active_at: Option<Timestamp>,
    /// Who is asking, and for which targets.
    pub access: Access,
    /// Pagination, applied last.
    pub page: Page,
}

/// Filters for `GET /reports`.
#[derive(Debug, Clone)]
pub struct ReportQuery {
    /// Restrict to reports for events in one programme.
    pub program_id: Option<ObjectId>,
    /// Restrict to one event.
    pub event_id: Option<ObjectId>,
    /// Restrict to one reporting client.
    pub client_name: Option<ClientName>,
    /// Who is asking. Reports are owned, not targeted.
    pub access: Access,
    /// Pagination, applied last.
    pub page: Page,
}

/// What kind of client owns a stored object.
///
/// A `clientID` alone cannot say. The specification never labels a token "BL" or "VEN" — the
/// distinction falls out of the *scopes* the credential carried `[API securitySchemes]` — and the
/// notification fan-out runs long after the request that created the subscription, with no
/// credential anywhere in reach. So it is recorded once, beside the identity it qualifies.
///
/// Deriving it instead is the trap. "A client with no `ven` object is business logic" reads
/// plausibly and hands every targeted event to any VEN that creates its subscription before it
/// registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerKind {
    /// Business logic: reads every object, whatever its targets.
    BusinessLogic,
    /// A VEN: reads what its grant reaches, with target hiding applied.
    Ven,
}

impl OwnerKind {
    /// The wire spelling used in the storage backends' `owner_kind` column.
    pub const fn as_str(self) -> &'static str {
        match self {
            OwnerKind::BusinessLogic => "BL",
            OwnerKind::Ven => "VEN",
        }
    }

    /// Read one back, treating anything unrecognised as a VEN.
    ///
    /// Fail closed: an unreadable value must not promote a subscriber to seeing everything.
    pub fn from_str_or_ven(raw: &str) -> Self {
        if raw == OwnerKind::BusinessLogic.as_str() {
            OwnerKind::BusinessLogic
        } else {
            OwnerKind::Ven
        }
    }

    /// Whether this owner reads without restriction.
    pub const fn is_business_logic(self) -> bool {
        matches!(self, OwnerKind::BusinessLogic)
    }
}

/// A subscription and what kind of client owns it, for the notification fan-out.
///
/// The fan-out needs one thing `GET /subscriptions` does not: whether the subscriber reads as
/// business logic. `Subscription` is a wire type and the specification gives it nowhere to say so,
/// which is why this pairs the two rather than widening it.
#[derive(Debug, Clone, PartialEq)]
pub struct Subscriber {
    /// The subscription itself.
    pub subscription: Subscription,
    /// What its creator was.
    pub owner: OwnerKind,
}

/// Filters for `GET /subscriptions`.
#[derive(Debug, Clone)]
pub struct SubscriptionQuery {
    /// Restrict to one programme.
    pub program_id: Option<ObjectId>,
    /// Restrict to one client name.
    pub client_name: Option<ClientName>,
    /// Restrict to subscriptions watching these object types.
    pub objects: Vec<ObjectType>,
    /// Who is asking. Subscriptions are owned, not targeted.
    pub access: Access,
    /// Pagination, applied last.
    pub page: Page,
}

/// Filters for `GET /vens`.
#[derive(Debug, Clone)]
pub struct VenQuery {
    /// Exact-match a VEN name.
    pub ven_name: Option<VenName>,
    /// Who is asking, and for which targets.
    ///
    /// Visibility here is decided by ownership, never by targeting — the Definitions say target
    /// hiding is deliberately not performed on `ven` objects. `?targets=` is therefore an ordinary
    /// additive filter, read through [`Access::requested_filter`].
    pub access: Access,
    /// Pagination, applied last.
    pub page: Page,
}

/// Filters for `GET /resources`.
#[derive(Debug, Clone)]
pub struct ResourceQuery {
    /// Restrict to resources of one VEN.
    pub ven_id: Option<ObjectId>,
    /// Exact-match a resource name.
    pub resource_name: Option<ResourceName>,
    /// Who is asking, and for which targets.
    ///
    /// A resource is owned through its VEN, and — as for [`VenQuery`] — `?targets=` is a filter
    /// rather than part of the privacy rule.
    pub access: Access,
    /// Pagination, applied last.
    pub page: Page,
}

/// What a VTN needs from a database.
///
/// Implementations are responsible for uniqueness constraints, referential integrity and stamping
/// `modificationDateTime`; they are *not* responsible for authorization, which happens a layer up so
/// that every backend enforces exactly the same rules.
///
/// # Why every mutation takes a [`Fanout`]
///
/// Because the notification has to be queued in the *same* transaction as the change. The caller
/// captures the snapshot — it is the layer that knows about subscriptions and grants — and the
/// backend calls [`Fanout::deliveries`], which is pure and synchronous, from inside its transaction.
/// So a change and the record that it must be announced commit together or not at all, and a crash
/// cannot land between them. Pass [`Fanout::none()`] when notifications are not wanted.
#[async_trait]
pub trait Storage: Send + Sync + 'static {
    /// Whether the backend is reachable.
    async fn healthy(&self) -> bool;

    // -- programs ----------------------------------------------------------
    /// Create a programme.
    async fn create_program(
        &self,
        request: ProgramRequest,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Program, StorageError>;
    /// Fetch a programme.
    async fn get_program(&self, id: &ObjectId) -> Result<Program, StorageError>;
    /// List programmes.
    async fn list_programs(&self, query: &ProgramQuery) -> Result<Vec<Program>, StorageError>;
    /// Replace a programme.
    async fn update_program(
        &self,
        id: &ObjectId,
        request: ProgramRequest,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Program, StorageError>;
    /// Delete a programme.
    async fn delete_program(&self, id: &ObjectId, fanout: &Fanout)
    -> Result<Program, StorageError>;

    // -- events ------------------------------------------------------------
    /// Create an event.
    async fn create_event(
        &self,
        request: EventRequest,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Event, StorageError>;
    /// Fetch an event.
    async fn get_event(&self, id: &ObjectId) -> Result<Event, StorageError>;
    /// List events.
    async fn list_events(&self, query: &EventQuery) -> Result<Vec<Event>, StorageError>;
    /// Replace an event.
    async fn update_event(
        &self,
        id: &ObjectId,
        request: EventRequest,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Event, StorageError>;
    /// Delete an event.
    async fn delete_event(&self, id: &ObjectId, fanout: &Fanout) -> Result<Event, StorageError>;

    // -- reports -----------------------------------------------------------
    /// Create a report.
    async fn create_report(
        &self,
        request: ReportRequest,
        owner: Option<ClientId>,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Report, StorageError>;
    /// Fetch a report.
    async fn get_report(&self, id: &ObjectId) -> Result<Report, StorageError>;
    /// List reports.
    async fn list_reports(&self, query: &ReportQuery) -> Result<Vec<Report>, StorageError>;
    /// Replace a report.
    async fn update_report(
        &self,
        id: &ObjectId,
        request: ReportRequest,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Report, StorageError>;
    /// Delete a report.
    async fn delete_report(&self, id: &ObjectId, fanout: &Fanout) -> Result<Report, StorageError>;

    // -- subscriptions -----------------------------------------------------
    /// Create a subscription.
    ///
    /// `owner_kind` is stored alongside the identity because the fan-out needs it and cannot
    /// re-derive it: see [`OwnerKind`].
    async fn create_subscription(
        &self,
        request: SubscriptionRequest,
        owner: ClientId,
        owner_kind: OwnerKind,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Subscription, StorageError>;
    /// Fetch a subscription.
    async fn get_subscription(&self, id: &ObjectId) -> Result<Subscription, StorageError>;
    /// List subscriptions.
    async fn list_subscriptions(
        &self,
        query: &SubscriptionQuery,
    ) -> Result<Vec<Subscription>, StorageError>;

    /// Every subscription that could be told about a write to one of these object types.
    ///
    /// The fan-out's own query, rather than `list_subscriptions` with an unrestricted [`Access`]
    /// and `usize::MAX` for a limit. Two things follow from having it: the page-size hack goes
    /// away, and each subscription arrives with its [`OwnerKind`], which is what stops a
    /// business-logic subscriber being evaluated as a VEN with an empty grant — and therefore
    /// never told about any targeted object at all.
    async fn subscribers(
        &self,
        object_types: &[ObjectType],
    ) -> Result<Vec<Subscriber>, StorageError>;
    /// Replace a subscription.
    async fn update_subscription(
        &self,
        id: &ObjectId,
        request: SubscriptionRequest,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Subscription, StorageError>;
    /// Delete a subscription.
    async fn delete_subscription(
        &self,
        id: &ObjectId,
        fanout: &Fanout,
    ) -> Result<Subscription, StorageError>;

    // -- vens --------------------------------------------------------------
    /// Create a VEN.
    async fn create_ven(&self, ven: Ven, fanout: &Fanout) -> Result<Ven, StorageError>;
    /// Fetch a VEN.
    async fn get_ven(&self, id: &ObjectId) -> Result<Ven, StorageError>;
    /// Fetch the VEN belonging to a client.
    async fn get_ven_by_client(&self, client_id: &ClientId) -> Result<Option<Ven>, StorageError>;
    /// List VENs.
    async fn list_vens(&self, query: &VenQuery) -> Result<Vec<Ven>, StorageError>;
    /// Replace a VEN.
    async fn update_ven(
        &self,
        id: &ObjectId,
        ven: Ven,
        fanout: &Fanout,
    ) -> Result<Ven, StorageError>;
    /// Delete a VEN and its resources.
    async fn delete_ven(&self, id: &ObjectId, fanout: &Fanout) -> Result<Ven, StorageError>;

    // -- resources ---------------------------------------------------------
    /// Create a resource.
    async fn create_resource(
        &self,
        resource: Resource,
        fanout: &Fanout,
    ) -> Result<Resource, StorageError>;
    /// Fetch a resource.
    async fn get_resource(&self, id: &ObjectId) -> Result<Resource, StorageError>;
    /// List resources.
    async fn list_resources(&self, query: &ResourceQuery) -> Result<Vec<Resource>, StorageError>;
    /// Replace a resource.
    async fn update_resource(
        &self,
        id: &ObjectId,
        resource: Resource,
        fanout: &Fanout,
    ) -> Result<Resource, StorageError>;
    /// Delete a resource.
    async fn delete_resource(
        &self,
        id: &ObjectId,
        fanout: &Fanout,
    ) -> Result<Resource, StorageError>;

    // -- privacy support ---------------------------------------------------
    /// The targets granted to a client, being the union of its VEN's and its resources' targets.
    async fn grant_for(&self, client_id: &ClientId) -> Result<Grant, StorageError>;

    /// Every VEN's grant, for routing push notifications.
    async fn all_grants(&self) -> Result<Vec<(ObjectId, ClientId, Grant)>, StorageError>;

    // -- outbox ------------------------------------------------------------
    /// Queue deliveries.
    ///
    /// Called on the write path, so it must be cheap: one insert per entitled subscriber and no
    /// network. Everything expensive happens in the dispatcher afterwards.
    async fn enqueue(
        &self,
        deliveries: Vec<crate::vtn::notify::Delivery>,
        now: Timestamp,
    ) -> Result<(), StorageError>;

    /// Take up to `limit` entries that are due, leasing them so a second dispatcher does not take
    /// the same ones.
    ///
    /// The lease is what makes more than one dispatcher safe. An entry whose lease expires without
    /// being completed becomes due again, so a dispatcher that dies mid-delivery loses nothing but
    /// time — at the cost of an occasional repeat, which is why delivery is at-least-once.
    async fn claim_due(
        &self,
        now: Timestamp,
        limit: usize,
        lease: core::time::Duration,
        owner: &str,
    ) -> Result<Vec<Queued>, StorageError>;

    /// Record a successful delivery, removing the entry.
    async fn complete(&self, id: OutboxId) -> Result<(), StorageError>;

    /// Record a failure and schedule another attempt, or abandon the entry.
    ///
    /// An entry is abandoned when its attempts are exhausted *or* when the failure was permanent —
    /// a `400` from the receiver is not going to become a `200`. Returns `true` if it was.
    async fn record_failure(
        &self,
        id: OutboxId,
        failure: &crate::vtn::notify::DeliveryFailure,
        now: Timestamp,
        policy: &RetryPolicy,
    ) -> Result<bool, StorageError>;

    /// Wait until there may be outbox work, or until `timeout` elapses.
    ///
    /// The default sleeps, which is polling: a backend with no way to be told about a write can
    /// only look again. Postgres overrides it with `LISTEN`/`NOTIFY`, so a dispatcher wakes when a
    /// write commits rather than when the timer says so.
    ///
    /// Waking early is always allowed; the dispatcher simply finds nothing. Never waking is not,
    /// which is why the timeout is a bound and not a hint.
    async fn await_outbox(&self, timeout: core::time::Duration) {
        tokio::time::sleep(timeout).await;
    }

    /// What the queue looks like right now.
    async fn outbox_stats(&self, now: Timestamp) -> Result<OutboxStats, StorageError>;

    /// The entries that were given up on, newest first.
    ///
    /// The counterpart of `dead` in [`OutboxStats`]: the count says something is wrong, this says
    /// what. Serves `GET /admin/outbox`.
    async fn dead_letters(&self, limit: usize) -> Result<Vec<DeadLetter>, StorageError>;

    /// Make every abandoned entry due again, returning how many were revived.
    ///
    /// What an operator does after fixing the endpoint that was refusing them. The attempt counter
    /// is reset, because the attempts that were spent were spent against a broken receiver and
    /// counting them against a working one would abandon the entry again immediately.
    async fn revive_dead(&self, now: Timestamp) -> Result<u64, StorageError>;

    // -- the subscriber circuit breaker ------------------------------------
    /// Record what happened to a notification bound for a subscription.
    ///
    /// `abandoned` is the only outcome that counts against a subscriber: a retry is still in
    /// flight, and a delivery that eventually succeeded is a subscriber that works. A success
    /// clears the row entirely, so "healthy" is the absence of state rather than a value.
    ///
    /// Returns `true` when this outcome *opened* the breaker, so the dispatcher can say so once
    /// rather than on every subsequent abandonment.
    async fn note_delivery(
        &self,
        subscription: &ObjectId,
        abandoned: bool,
        now: Timestamp,
        policy: &BreakerPolicy,
        error: Option<&str>,
    ) -> Result<bool, StorageError>;

    /// Every subscription with a delivery failure against it, worst first.
    ///
    /// Read on the write path — [`Fanout`] skips a cut-off subscriber — so it returns only the
    /// unhealthy, which in a working VTN is no rows at all. It also serves
    /// `GET /admin/subscribers`, where the ones that are merely failing matter as much as the ones
    /// already cut off.
    async fn subscriber_health(&self) -> Result<Vec<SubscriberHealth>, StorageError>;

    /// Forget every recorded failure, closing every breaker. Returns how many were cleared.
    ///
    /// The other half of `POST /admin/outbox/retry`: reviving the dead letters without closing the
    /// breaker that stopped new ones being queued would revive a backlog and then immediately stop
    /// adding to it, which is half a recovery.
    async fn clear_subscriber_health(&self) -> Result<u64, StorageError>;

    // -- retention ----------------------------------------------------------

    /// Delete up to `limit` reports created strictly before `before`. Returns how many went.
    ///
    /// **Oldest first**, so a bounded sweep reaches the far end rather than removing an arbitrary
    /// `limit` for ever, and **bounded**, so a first pass over a year of data is many small
    /// transactions rather than one long lock.
    ///
    /// It queues no notification: expiry is the VTN's own housekeeping, and OpenADR has no way to
    /// say "a report you filed has been forgotten". See [`crate::vtn::retention`] `[D-113]`.
    async fn purge_reports(&self, before: Timestamp, limit: usize) -> Result<u64, StorageError>;

    /// How many reports exist, and how old the oldest is. For `GET /health`.
    async fn report_stats(&self, now: Timestamp) -> Result<ReportStats, StorageError>;
}

/// What the report table looks like right now.
///
/// The two numbers an operator needs to decide whether retention is configured correctly: how much
/// is there, and how far back it goes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportStats {
    /// Reports stored.
    pub count: u64,
    /// Age in seconds of the oldest one.
    pub oldest_seconds: Option<i64>,
}

/// A shared handle to a storage backend.
pub type SharedStorage = Arc<dyn Storage>;
