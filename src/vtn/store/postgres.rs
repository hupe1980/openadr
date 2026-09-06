//! The Postgres backend.
//!
//! The multi-tenant, utility-scale VTN. SQLite covers the site controller and the single-tenant
//! pilot; this covers the deployment with tens of thousands of VENs, several server instances and
//! a database that is somebody else's job to run.
//!
//! ## Why this is not "the SQLite queries with different placeholders"
//!
//! Two Postgres primitives change the design rather than the syntax:
//!
//! * **`SELECT … FOR UPDATE SKIP LOCKED`** is the right way to claim outbox entries. SQLite has no
//!   equivalent, so there the lease columns *are* the mechanism and two dispatchers contend on one
//!   write lock. Here the lease is only a recovery net for a dispatcher that dies mid-delivery, and
//!   claiming scales with the number of dispatchers.
//! * **`LISTEN`/`NOTIFY`** wakes a dispatcher when a write commits, so notification latency is the
//!   commit rather than the poll interval. That matters when the payload is a curtailment
//!   instruction. See [`Storage::await_outbox`].
//!
//! Target filtering also stops being a placeholder list: `= ANY($1)` binds the whole set as one
//! array parameter, so a VEN granted two hundred targets is one bind rather than two hundred.
//!
//! ## Shape
//!
//! Identical to SQLite's, deliberately: one table per object holding the request body as JSON plus
//! the columns the API queries, cascades and uniqueness declared in the schema, one `object_target`
//! table so the privacy predicate is one join whatever it filters. The conformance suite is
//! what holds the two to the same behaviour.

use async_trait::async_trait;
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use std::sync::Arc;

use crate::core::{Grant, GrantIndex, OwnerFilter, TargetFilter, active_window};
use crate::model::notification::AnyObject;
use crate::model::{
    ClientId, Event, EventRequest, ObjectId, ObjectType, Operation, Program, ProgramRequest,
    Report, ReportRequest, Resource, Subscription, SubscriptionRequest, Target, Timestamp, Ven,
};
use crate::vtn::notify::{Delivery, Fanout};

use super::outbox::support;
use super::sql::{
    cooldown_end, decode, encode, id, not_found, route_columns, route_from_columns, stamp, time,
};
use super::{
    BreakerPolicy, DeadLetter, EventQuery, OutboxId, OutboxStats, OwnerKind, ProgramQuery, Queued,
    ReportQuery, ReportStats, ResourceQuery, RetryPolicy, SharedStorage, Storage, StorageError,
    Subscriber, SubscriberHealth, SubscriptionQuery, VenQuery, sequenced_id,
};

/// The channel a write announces itself on, and a dispatcher listens to.
const OUTBOX_CHANNEL: &str = "openadr_outbox";

/// The schema.
///
/// Applied on connect. The project is pre-release and the schema is authoritative rather than
/// historical: there are no migrations to run.
///
/// Timestamps are `TEXT COLLATE "C"`, not `TIMESTAMPTZ`. `TIMESTAMPTZ` would mean converting
/// through `chrono` or `time` on every read and write, for a value this crate already has as a
/// `jiff::Timestamp`; the text form is written fixed-width by [`stamp`] so it orders correctly, and
/// the `C` collation makes that byte-wise regardless of what the database's default collation does
/// with punctuation.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS program (
    id            TEXT PRIMARY KEY,
    created_at    TEXT COLLATE "C" NOT NULL,
    modified_at   TEXT COLLATE "C" NOT NULL,
    program_name  TEXT NOT NULL UNIQUE,
    doc           TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS event (
    id           TEXT PRIMARY KEY,
    created_at   TEXT COLLATE "C" NOT NULL,
    modified_at  TEXT COLLATE "C" NOT NULL,
    program_id   TEXT NOT NULL REFERENCES program(id) ON DELETE CASCADE,
    -- The window the event is live over, resolved once at write time, so `?active=` is a
    -- comparison of two columns rather than an interval expansion per event per request.
    -- NULL active_from means "nothing to elapse"; NULL active_to means "never ends".
    active_from  TEXT COLLATE "C",
    active_to    TEXT COLLATE "C",
    doc          TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS event_program ON event(program_id);
CREATE INDEX IF NOT EXISTS event_active ON event(active_to);

CREATE TABLE IF NOT EXISTS report (
    id           TEXT PRIMARY KEY,
    created_at   TEXT COLLATE "C" NOT NULL,
    modified_at  TEXT COLLATE "C" NOT NULL,
    event_id     TEXT NOT NULL REFERENCES event(id) ON DELETE CASCADE,
    client_id    TEXT,
    client_name  TEXT NOT NULL,
    doc          TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS report_event ON report(event_id);
CREATE INDEX IF NOT EXISTS report_client ON report(client_id);
-- Retention sweeps oldest-first over the whole table, which without this is a full scan on every
-- pass of a task that runs for ever.
CREATE INDEX IF NOT EXISTS report_created ON report(created_at);

CREATE TABLE IF NOT EXISTS subscription (
    id           TEXT PRIMARY KEY,
    created_at   TEXT COLLATE "C" NOT NULL,
    modified_at  TEXT COLLATE "C" NOT NULL,
    client_id    TEXT NOT NULL,
    -- 'BL' or 'VEN'. The fan-out runs long after the request that created the row and has no
    -- credential to ask, and a business-logic subscriber evaluated as a VEN with an empty grant
    -- is told about no targeted object at all.
    owner_kind   TEXT NOT NULL DEFAULT 'VEN',
    client_name  TEXT NOT NULL,
    program_id   TEXT REFERENCES program(id) ON DELETE CASCADE,
    doc          TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS subscription_client ON subscription(client_id);

-- Which object types a subscription watches, so ?objects= is an indexed join rather than a
-- JSON scan.
CREATE TABLE IF NOT EXISTS subscription_object (
    subscription_id TEXT NOT NULL REFERENCES subscription(id) ON DELETE CASCADE,
    object_type     TEXT NOT NULL,
    PRIMARY KEY (subscription_id, object_type)
);
-- The fan-out asks "which subscriptions watch this object type", and the primary key above leads
-- with `subscription_id` — so answering it meant walking every subscription in the VTN and probing.
-- On the highest-rate write in the system, `POST /reports`, that is O(all subscriptions) for an
-- answer that is almost always empty: a thousand event subscriptions cost every report write a
-- thousand probes. Leading with `object_type` makes it a range scan over the matching rows only
-- `[D-116]`.
CREATE INDEX IF NOT EXISTS subscription_object_type
    ON subscription_object(object_type, subscription_id);


CREATE TABLE IF NOT EXISTS ven (
    id           TEXT PRIMARY KEY,
    created_at   TEXT COLLATE "C" NOT NULL,
    modified_at  TEXT COLLATE "C" NOT NULL,
    client_id    TEXT NOT NULL UNIQUE,
    ven_name     TEXT NOT NULL UNIQUE,
    doc          TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS resource (
    id             TEXT PRIMARY KEY,
    created_at     TEXT COLLATE "C" NOT NULL,
    modified_at    TEXT COLLATE "C" NOT NULL,
    ven_id         TEXT NOT NULL REFERENCES ven(id) ON DELETE CASCADE,
    resource_name  TEXT NOT NULL,
    doc            TEXT NOT NULL,
    UNIQUE (ven_id, resource_name)
);

-- Targets for every kind of object that carries them, in one table so the privacy predicate is one
-- join whatever it is filtering.
CREATE TABLE IF NOT EXISTS object_target (
    object_id    TEXT NOT NULL,
    object_type  TEXT NOT NULL,
    target       TEXT NOT NULL,
    PRIMARY KEY (object_id, object_type, target)
);
CREATE INDEX IF NOT EXISTS object_target_lookup ON object_target(target, object_type);

-- Monotonic identifiers, so ordering by (created_at, id) is stable and pages never reshuffle.
CREATE TABLE IF NOT EXISTS id_sequence (
    name  TEXT PRIMARY KEY,
    next  BIGINT NOT NULL
);

-- The transactional outbox. One row per (change, recipient), written in the same transaction as the
-- change itself, so a notification cannot be lost to a crash between the two.
-- The subscriber circuit breaker. Only the unhealthy have a row, so the table's size is the size
-- of the problem and a working VTN reads nothing here.
CREATE TABLE IF NOT EXISTS subscriber_health (
    subscription_id      TEXT PRIMARY KEY REFERENCES subscription(id) ON DELETE CASCADE,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    -- When the breaker last opened, and when one probe will be let through. NULL means closed.
    cut_off_since        TEXT COLLATE "C",
    retry_at             TEXT COLLATE "C",
    last_error           TEXT
);

CREATE TABLE IF NOT EXISTS outbox (
    id               BIGSERIAL PRIMARY KEY,
    enqueued_at      TEXT COLLATE "C" NOT NULL,
    subscription_id  TEXT,
    -- What the notification is about. Not needed to deliver it, but an operator looking at a stuck
    -- or abandoned row needs to know which event or report it concerns.
    object_type      TEXT NOT NULL,
    object_id        TEXT NOT NULL,
    callback_url     TEXT,
    bearer_token     TEXT,
    topic            TEXT,
    notification     TEXT NOT NULL,
    attempts         INTEGER NOT NULL DEFAULT 0,
    -- NULL means abandoned: attempts are exhausted and it will not be tried again.
    next_attempt_at  TEXT COLLATE "C",
    -- A lease held by a dispatcher. With SKIP LOCKED this is not how entries are claimed; it is how
    -- an entry held by a dispatcher that died is eventually reclaimed.
    lease_until      TEXT COLLATE "C",
    lease_owner      TEXT,
    last_error       TEXT
);
CREATE INDEX IF NOT EXISTS outbox_due ON outbox(next_attempt_at, lease_until);
"#;

/// A durable store backed by Postgres.
pub struct PostgresStorage {
    pool: PgPool,
    url: String,
    /// The `LISTEN` connection, opened on first use.
    ///
    /// Separate from the pool because a listening connection is not returnable to it: it is blocked
    /// in `recv`. Behind a mutex because `recv` needs `&mut`, and behind an `Option` because a VTN
    /// that never runs a dispatcher should not hold a connection open for nothing.
    listener: tokio::sync::Mutex<Option<sqlx::postgres::PgListener>>,
}

impl std::fmt::Debug for PostgresStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresStorage").finish_non_exhaustive()
    }
}

impl PostgresStorage {
    /// Connect and apply the schema.
    pub async fn open(url: &str) -> Result<Self, StorageError> {
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .connect(url)
            .await
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;

        let store = Self {
            pool,
            url: url.to_string(),
            listener: tokio::sync::Mutex::new(None),
        };
        store.apply_schema().await?;
        Ok(store)
    }

    /// Connect and wrap in an `Arc`, ready for [`crate::vtn::VtnBuilder::storage`].
    pub async fn shared(url: &str) -> Result<SharedStorage, StorageError> {
        Ok(Arc::new(Self::open(url).await?))
    }

    /// Apply the schema, under an advisory lock so that two instances may start at once.
    ///
    /// `CREATE TABLE IF NOT EXISTS` is not safe to run concurrently on PostgreSQL: the existence
    /// check and the creation are not one step, so two sessions racing on a table both decide to
    /// create it and the loser fails with an error naming an internal catalogue index. This is the
    /// shared-VTN backend, so two instances starting at once is a rolling deploy rather than an edge
    /// case (D-125).
    ///
    /// The lock is transaction-scoped, so it is released on commit or on the connection dying and a
    /// process killed mid-schema blocks nobody.
    async fn apply_schema(&self) -> Result<(), StorageError> {
        /// `openadr` schema lock. Arbitrary, and only ever compared with itself.
        const SCHEMA_LOCK: i64 = 0x0AD3_5CE3_0000_0001_u64 as i64;

        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(SCHEMA_LOCK)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::raw_sql(SCHEMA).execute(&mut *tx).await.map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(())
    }

    /// Empty every table. For tests, which want a clean database per behaviour.
    #[doc(hidden)]
    pub async fn truncate_all(&self) -> Result<(), StorageError> {
        sqlx::raw_sql(
            "TRUNCATE program, event, report, subscription, subscription_object, subscriber_health, \
             ven, resource, object_target, id_sequence, outbox RESTART IDENTITY CASCADE",
        )
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    /// Mint an identifier that sorts in creation order.
    async fn next_id(
        tx: &mut sqlx::PgConnection,
        kind: ObjectType,
    ) -> Result<ObjectId, StorageError> {
        let name = kind.collection();
        // One statement: the upsert returns the value it wrote, so there is no read-back to race.
        let n: i64 = sqlx::query(
            "INSERT INTO id_sequence (name, next) VALUES ($1, 1) \
             ON CONFLICT (name) DO UPDATE SET next = id_sequence.next + 1 \
             RETURNING next",
        )
        .bind(name)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?
        .get(0);

        Ok(sequenced_id(kind, n))
    }

    /// Queue the notifications a change produces, inside the transaction that made it.
    ///
    /// The `NOTIFY` is transactional too: Postgres holds it until commit, so a dispatcher is never
    /// woken for a change that then rolled back.
    async fn queue(
        tx: &mut sqlx::PgConnection,
        fanout: &Fanout,
        object: AnyObject,
        operation: Operation,
    ) -> Result<(), StorageError> {
        let deliveries = fanout.deliveries(&object, operation);
        if deliveries.is_empty() {
            return Ok(());
        }
        insert_deliveries(tx, &deliveries, fanout.now()).await?;
        announce(tx).await
    }

    /// The objects a cascade is about to take with it.
    ///
    /// A cascade deletes objects a subscriber asked to hear about, and the database will not
    /// announce them. Reading them first costs two queries on a `DELETE` — and only when something
    /// is actually subscribed — and it is the difference between a VEN learning that its dispatch
    /// was cancelled and holding an event that no longer exists.
    async fn cascaded_from_program(
        tx: &mut sqlx::PgConnection,
        program_id: &ObjectId,
    ) -> Result<Vec<AnyObject>, StorageError> {
        let mut out = Vec::new();
        let events = sqlx::query(
            "SELECT id, created_at, modified_at, doc FROM event WHERE program_id = $1 \
             ORDER BY created_at, id",
        )
        .bind(program_id.as_str())
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
        for row in &events {
            out.push(AnyObject::Event(Event {
                id: id(row.get(0))?,
                created_date_time: time(row.get(1))?,
                modification_date_time: time(row.get(2))?,
                object_type: ObjectType::Event,
                content: decode(row.get(3))?,
            }));
        }
        let reports = sqlx::query(
            "SELECT r.id, r.created_at, r.modified_at, r.client_id, r.doc FROM report r \
             JOIN event e ON e.id = r.event_id WHERE e.program_id = $1 ORDER BY r.created_at, r.id",
        )
        .bind(program_id.as_str())
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
        for row in &reports {
            let client: Option<String> = row.get(3);
            out.push(AnyObject::Report(Report {
                id: id(row.get(0))?,
                created_date_time: time(row.get(1))?,
                modification_date_time: time(row.get(2))?,
                object_type: ObjectType::Report,
                client_id: client
                    .map(|c| ClientId::new(c).map_err(|e| StorageError::Unavailable(e.to_string())))
                    .transpose()?,
                content: decode(row.get(4))?,
            }));
        }
        let subscriptions = sqlx::query(
            "SELECT id, created_at, modified_at, client_id, doc FROM subscription \
             WHERE program_id = $1 ORDER BY created_at, id",
        )
        .bind(program_id.as_str())
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
        for row in &subscriptions {
            out.push(AnyObject::Subscription(Subscription {
                id: id(row.get(0))?,
                created_date_time: time(row.get(1))?,
                modification_date_time: time(row.get(2))?,
                object_type: ObjectType::Subscription,
                client_id: ClientId::new(row.get::<String, _>(3))
                    .map_err(|e| StorageError::Unavailable(e.to_string()))?,
                content: decode(row.get(4))?,
            }));
        }
        Ok(out)
    }

    /// The reports an event's deletion is about to take with it.
    ///
    /// `report.eventID` is a cascading foreign key, so the rows go whether or not anybody says so.
    /// A VEN that filed a compliance report is subscribed to its deletion, and OpenADR has no way
    /// to tell it afterwards that one happened.
    async fn cascaded_from_event(
        tx: &mut sqlx::PgConnection,
        event_id: &ObjectId,
    ) -> Result<Vec<AnyObject>, StorageError> {
        let rows = sqlx::query(
            "SELECT id, created_at, modified_at, client_id, doc FROM report WHERE event_id = $1 \
             ORDER BY created_at, id",
        )
        .bind(event_id.as_str())
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
        rows.iter()
            .map(|row| {
                let client: Option<String> = row.get(3);
                Ok(AnyObject::Report(Report {
                    id: id(row.get(0))?,
                    created_date_time: time(row.get(1))?,
                    modification_date_time: time(row.get(2))?,
                    object_type: ObjectType::Report,
                    client_id: client
                        .map(|c| {
                            ClientId::new(c).map_err(|e| StorageError::Unavailable(e.to_string()))
                        })
                        .transpose()?,
                    content: decode(row.get(4))?,
                }))
            })
            .collect()
    }

    /// The resources a VEN's deletion is about to take with it.
    async fn cascaded_from_ven(
        tx: &mut sqlx::PgConnection,
        ven_id: &ObjectId,
    ) -> Result<Vec<AnyObject>, StorageError> {
        let rows = sqlx::query(
            "SELECT id, created_at, modified_at, doc FROM resource WHERE ven_id = $1 \
             ORDER BY created_at, id",
        )
        .bind(ven_id.as_str())
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
        rows.iter()
            .map(|row| {
                let mut resource: Resource = decode(row.get(3))?;
                resource.id = id(row.get(0))?;
                resource.created_date_time = time(row.get(1))?;
                resource.modification_date_time = time(row.get(2))?;
                Ok(AnyObject::Resource(resource))
            })
            .collect()
    }

    /// Replace the target rows for an object.
    async fn write_targets(
        tx: &mut sqlx::PgConnection,
        object_id: &ObjectId,
        object_type: ObjectType,
        targets: &[Target],
    ) -> Result<(), StorageError> {
        sqlx::query("DELETE FROM object_target WHERE object_id = $1 AND object_type = $2")
            .bind(object_id.as_str())
            .bind(object_type.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        for target in targets {
            sqlx::query(
                "INSERT INTO object_target (object_id, object_type, target) VALUES ($1, $2, $3) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(object_id.as_str())
            .bind(object_type.as_str())
            .bind(target.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        }
        Ok(())
    }
}

/// Tell any listening dispatcher that the queue has grown.
async fn announce(tx: &mut sqlx::PgConnection) -> Result<(), StorageError> {
    sqlx::query("SELECT pg_notify($1, '')")
        .bind(OUTBOX_CHANNEL)
        .execute(tx)
        .await
        .map_err(db)?;
    Ok(())
}

/// Map a database failure onto a storage error, recognising the constraint violations that the API
/// reports as `409` or `400` rather than `500`.
fn db(e: sqlx::Error) -> StorageError {
    if let sqlx::Error::Database(err) = &e {
        // SQLSTATE, not message text: Postgres localises messages, and a VTN whose conflict
        // detection depends on the server's `lc_messages` is a VTN that returns 500s abroad.
        match err.code().as_deref() {
            Some("23505") => {
                let (object_type, field) = classify_unique(err.constraint().unwrap_or_default());
                return StorageError::Conflict {
                    object_type,
                    field,
                    value: err.message().to_string(),
                };
            }
            Some("23503") => {
                return StorageError::DanglingReference {
                    field: "reference",
                    value: err.message().to_string(),
                };
            }
            _ => {}
        }
    }
    StorageError::Unavailable(e.to_string())
}

/// Map a database error, supplying the value a uniqueness conflict is *about*.
///
/// The database names the column; only the caller knows the value the client sent. Without this the
/// `problem` body quotes the server's own message back — `VEN with clientID "unique constraint
/// failed: ven.client_id" already exists` — which tells the client nothing it can act on and
/// discloses the schema, on a path that is by definition reachable by anyone who can write.
fn conflict_value(e: sqlx::Error, value: impl Fn(&'static str) -> String) -> StorageError {
    match db(e) {
        StorageError::Conflict {
            object_type, field, ..
        } => StorageError::Conflict {
            object_type,
            field,
            value: value(field),
        },
        other => other,
    }
}

/// Recover which constraint a uniqueness violation refers to.
///
/// Postgres names the *constraint*, and the names are the ones the schema declares implicitly:
/// `<table>_<column>_key` for a column `UNIQUE`, `<table>_<cols>_key` for a table one.
fn classify_unique(constraint: &str) -> (ObjectType, &'static str) {
    match constraint {
        "program_program_name_key" => (ObjectType::Program, "programName"),
        "ven_client_id_key" => (ObjectType::Ven, "clientID"),
        "ven_ven_name_key" => (ObjectType::Ven, "venName"),
        "resource_ven_id_resource_name_key" => (ObjectType::Resource, "resourceName"),
        _ => (ObjectType::Program, "unknown"),
    }
}

/// A `WHERE` fragment and the parameter number the caller should use next.
struct Sql {
    clause: String,
    next: usize,
}

/// Render [`TargetFilter`] as a predicate.
///
/// One `= ANY($n)` rather than SQLite's placeholder list, so a VEN granted two hundred targets
/// costs one bind and one plan rather than two hundred of each.
fn target_sql(filter: &TargetFilter, object_type: ObjectType, alias: &str, next: usize) -> Sql {
    let untargeted = format!(
        "NOT EXISTS (SELECT 1 FROM object_target t \
         WHERE t.object_id = {alias}.id AND t.object_type = '{}')",
        object_type.as_str()
    );
    match filter {
        TargetFilter::Any => Sql {
            clause: "TRUE".into(),
            next,
        },
        TargetFilter::Untargeted => Sql {
            clause: untargeted,
            next,
        },
        TargetFilter::UntargetedOrAnyOf(allowed) if allowed.is_empty() => Sql {
            clause: untargeted,
            next,
        },
        TargetFilter::AnyOf(allowed) if allowed.is_empty() => Sql {
            clause: "FALSE".into(),
            next,
        },
        TargetFilter::UntargetedOrAnyOf(_) => Sql {
            clause: format!("({untargeted} OR {})", carries(alias, object_type, next)),
            next: next + 1,
        },
        TargetFilter::AnyOf(_) => Sql {
            clause: carries(alias, object_type, next),
            next: next + 1,
        },
    }
}

/// `EXISTS` over the target table, bound with `= ANY($n)` so a VEN granted two hundred targets is
/// one bind and one query plan rather than two hundred of each.
fn carries(alias: &str, object_type: ObjectType, next: usize) -> String {
    format!(
        "EXISTS (SELECT 1 FROM object_target t \
         WHERE t.object_id = {alias}.id AND t.object_type = '{}' \
         AND t.target = ANY(${next}))",
        object_type.as_str()
    )
}

/// The targets a [`TargetFilter`] binds, if any.
fn target_binds(filter: &TargetFilter) -> Option<Vec<String>> {
    match filter {
        TargetFilter::UntargetedOrAnyOf(allowed) | TargetFilter::AnyOf(allowed)
            if !allowed.is_empty() =>
        {
            Some(allowed.iter().map(|t| t.to_string()).collect())
        }
        _ => None,
    }
}

/// Render [`OwnerFilter`] as a predicate.
fn owner_sql(filter: &OwnerFilter, column: &str, next: usize) -> Sql {
    match filter {
        OwnerFilter::Any => Sql {
            clause: "TRUE".into(),
            next,
        },
        OwnerFilter::Only(_) => Sql {
            clause: format!("{column} = ${next}"),
            next: next + 1,
        },
        OwnerFilter::Nothing => Sql {
            clause: "FALSE".into(),
            next,
        },
    }
}

fn owner_bind(filter: &OwnerFilter) -> Option<ClientId> {
    match filter {
        OwnerFilter::Only(client) => Some(client.clone()),
        _ => None,
    }
}

/// Record which object types a subscription watches.
async fn write_subscription_objects(
    tx: &mut sqlx::PgConnection,
    subscription_id: &ObjectId,
    request: &SubscriptionRequest,
) -> Result<(), StorageError> {
    sqlx::query("DELETE FROM subscription_object WHERE subscription_id = $1")
        .bind(subscription_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    for operation in &request.object_operations {
        for object_type in &operation.objects {
            sqlx::query(
                "INSERT INTO subscription_object (subscription_id, object_type) VALUES ($1, $2) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(subscription_id.as_str())
            .bind(object_type.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        }
    }
    Ok(())
}

/// Queue every delivery a write produced, in **one** statement.
///
/// One `INSERT` per delivery costs a round trip per recipient inside the transaction, which is most
/// of what the fan-out was measured to cost (`tests/load.rs`, D-116). `UNNEST` makes it one
/// statement; the rows, the transaction and the order are identical, since `outbox.id` is a sequence
/// a multi-row insert assigns in argument order.
async fn insert_deliveries(
    tx: &mut sqlx::PgConnection,
    deliveries: &[Delivery],
    now: Timestamp,
) -> Result<(), StorageError> {
    let mut subscription_ids = Vec::with_capacity(deliveries.len());
    let mut object_types = Vec::with_capacity(deliveries.len());
    let mut object_ids = Vec::with_capacity(deliveries.len());
    let mut callback_urls = Vec::with_capacity(deliveries.len());
    let mut bearer_tokens = Vec::with_capacity(deliveries.len());
    let mut topics = Vec::with_capacity(deliveries.len());
    let mut notifications = Vec::with_capacity(deliveries.len());

    for delivery in deliveries {
        let (object_type, object_id) = support::subject(delivery);
        let (callback_url, bearer_token, topic) = route_columns(&delivery.route);
        subscription_ids.push(delivery.subscription_id.as_ref().map(|s| s.to_string()));
        object_types.push(object_type.to_string());
        object_ids.push(object_id.to_string());
        callback_urls.push(callback_url.map(str::to_string));
        bearer_tokens.push(bearer_token.map(str::to_string));
        topics.push(topic.map(str::to_string));
        notifications.push(encode(&delivery.notification)?);
    }

    sqlx::query(
        "INSERT INTO outbox (enqueued_at, subscription_id, object_type, object_id, \
         callback_url, bearer_token, topic, notification, next_attempt_at) \
         SELECT $1, t.subscription_id, t.object_type, t.object_id, t.callback_url, \
                t.bearer_token, t.topic, t.notification, $1 \
         FROM UNNEST($2::text[], $3::text[], $4::text[], $5::text[], $6::text[], $7::text[], \
                     $8::text[]) \
              AS t(subscription_id, object_type, object_id, callback_url, bearer_token, topic, \
                   notification)",
    )
    .bind(stamp(now))
    .bind(&subscription_ids)
    .bind(&object_types)
    .bind(&object_ids)
    .bind(&callback_urls)
    .bind(&bearer_tokens)
    .bind(&topics)
    .bind(&notifications)
    .execute(tx)
    .await
    .map_err(db)?;
    Ok(())
}

#[async_trait]
impl Storage for PostgresStorage {
    async fn healthy(&self) -> bool {
        sqlx::query("SELECT 1").fetch_one(&self.pool).await.is_ok()
    }

    // -- programs ----------------------------------------------------------

    async fn create_program(
        &self,
        request: ProgramRequest,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Program, StorageError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let object_id = Self::next_id(&mut tx, ObjectType::Program).await?;
        sqlx::query(
            "INSERT INTO program (id, created_at, modified_at, program_name, doc) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(object_id.as_str())
        .bind(stamp(now))
        .bind(stamp(now))
        .bind(request.program_name.as_str())
        .bind(encode(&request)?)
        .execute(&mut *tx)
        .await
        .map_err(|e| conflict_value(e, |_| request.program_name.to_string()))?;
        Self::write_targets(&mut tx, &object_id, ObjectType::Program, &request.targets).await?;

        let program = Program {
            id: object_id,
            created_date_time: now,
            modification_date_time: now,
            object_type: ObjectType::Program,
            content: request,
        };
        Self::queue(
            &mut tx,
            fanout,
            AnyObject::Program(program.clone()),
            Operation::Create,
        )
        .await?;
        tx.commit().await.map_err(db)?;
        Ok(program)
    }

    async fn get_program(&self, object_id: &ObjectId) -> Result<Program, StorageError> {
        let row = sqlx::query("SELECT id, created_at, modified_at, doc FROM program WHERE id = $1")
            .bind(object_id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?
            .ok_or_else(|| not_found(ObjectType::Program, object_id))?;
        Ok(Program {
            id: id(row.get(0))?,
            created_date_time: time(row.get(1))?,
            modification_date_time: time(row.get(2))?,
            object_type: ObjectType::Program,
            content: decode(row.get(3))?,
        })
    }

    async fn list_programs(&self, query: &ProgramQuery) -> Result<Vec<Program>, StorageError> {
        let filter = query.access.target_filter();
        let targets = target_sql(&filter, ObjectType::Program, "o", 1);
        let mut n = targets.next;
        let mut sql = format!(
            "SELECT o.id, o.created_at, o.modified_at, o.doc FROM program o WHERE {}",
            targets.clause
        );
        if query.program_name.is_some() {
            sql.push_str(&format!(" AND o.program_name = ${n}"));
            n += 1;
        }
        sql.push_str(&format!(
            " ORDER BY o.created_at, o.id LIMIT ${n} OFFSET ${}",
            n + 1
        ));

        let mut q = sqlx::query(&sql);
        if let Some(binds) = target_binds(&filter) {
            q = q.bind(binds);
        }
        if let Some(name) = &query.program_name {
            q = q.bind(name.to_string());
        }
        q = q.bind(query.page.limit as i64).bind(query.page.skip as i64);

        let rows = q.fetch_all(&self.pool).await.map_err(db)?;
        rows.iter()
            .map(|row| {
                Ok(Program {
                    id: id(row.get(0))?,
                    created_date_time: time(row.get(1))?,
                    modification_date_time: time(row.get(2))?,
                    object_type: ObjectType::Program,
                    content: decode(row.get(3))?,
                })
            })
            .collect()
    }

    async fn update_program(
        &self,
        object_id: &ObjectId,
        request: ProgramRequest,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Program, StorageError> {
        let existing = self.get_program(object_id).await?;
        // A `PUT` that changes nothing must not bump the modification time: a client that polls
        // on it would see a change that did not happen.
        let modified = if existing.content == request {
            existing.modification_date_time
        } else {
            now
        };

        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query(
            "UPDATE program SET modified_at = $2, program_name = $3, doc = $4 WHERE id = $1",
        )
        .bind(object_id.as_str())
        .bind(stamp(modified))
        .bind(request.program_name.as_str())
        .bind(encode(&request)?)
        .execute(&mut *tx)
        .await
        .map_err(|e| conflict_value(e, |_| request.program_name.to_string()))?;
        Self::write_targets(&mut tx, object_id, ObjectType::Program, &request.targets).await?;

        let program = Program {
            id: object_id.clone(),
            created_date_time: existing.created_date_time,
            modification_date_time: modified,
            object_type: ObjectType::Program,
            content: request,
        };
        Self::queue(
            &mut tx,
            fanout,
            AnyObject::Program(program.clone()),
            Operation::Update,
        )
        .await?;
        tx.commit().await.map_err(db)?;
        Ok(program)
    }

    async fn delete_program(
        &self,
        object_id: &ObjectId,
        fanout: &Fanout,
    ) -> Result<Program, StorageError> {
        let program = self.get_program(object_id).await?;
        let mut tx = self.pool.begin().await.map_err(db)?;
        let cascaded = if fanout.is_empty() {
            Vec::new()
        } else {
            Self::cascaded_from_program(&mut tx, object_id).await?
        };
        // Events, reports and programme-scoped subscriptions go by cascade. `object_target` is
        // polymorphic and so has no foreign key to be cascaded *through*: every kind of row a
        // cascade removes has to be cleared here by hand. Reports carry no targets; events and
        // subscriptions both do, and the subscriptions were missed (D-123).
        sqlx::query(
            "DELETE FROM object_target WHERE object_id IN \
             (SELECT id FROM event WHERE program_id = $1 \
              UNION ALL SELECT id FROM subscription WHERE program_id = $1)",
        )
        .bind(object_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query("DELETE FROM object_target WHERE object_id = $1")
            .bind(object_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query("DELETE FROM program WHERE id = $1")
            .bind(object_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        for object in cascaded {
            Self::queue(&mut tx, fanout, object, Operation::Delete).await?;
        }
        Self::queue(
            &mut tx,
            fanout,
            AnyObject::Program(program.clone()),
            Operation::Delete,
        )
        .await?;
        tx.commit().await.map_err(db)?;
        Ok(program)
    }

    // -- events ------------------------------------------------------------

    async fn create_event(
        &self,
        request: EventRequest,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Event, StorageError> {
        let window = active_window(&request, now).ok().flatten();
        let mut tx = self.pool.begin().await.map_err(db)?;
        let object_id = Self::next_id(&mut tx, ObjectType::Event).await?;
        sqlx::query(
            "INSERT INTO event (id, created_at, modified_at, program_id, active_from, active_to, doc) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(object_id.as_str())
        .bind(stamp(now))
        .bind(stamp(now))
        .bind(request.program_id.as_str())
        .bind(window.map(|(from, _)| stamp(from)))
        .bind(window.and_then(|(_, to)| to).map(stamp))
        .bind(encode(&request)?)
        .execute(&mut *tx)
        .await
        .map_err(|e| match db(e) {
            // The only foreign key on an event is its programme.
            StorageError::DanglingReference { .. } => StorageError::DanglingReference {
                field: "programID",
                value: request.program_id.to_string(),
            },
            other => other,
        })?;
        Self::write_targets(&mut tx, &object_id, ObjectType::Event, &request.targets).await?;

        let event = Event {
            id: object_id,
            created_date_time: now,
            modification_date_time: now,
            object_type: ObjectType::Event,
            content: request,
        };
        Self::queue(
            &mut tx,
            fanout,
            AnyObject::Event(event.clone()),
            Operation::Create,
        )
        .await?;
        tx.commit().await.map_err(db)?;
        Ok(event)
    }

    async fn get_event(&self, object_id: &ObjectId) -> Result<Event, StorageError> {
        let row = sqlx::query("SELECT id, created_at, modified_at, doc FROM event WHERE id = $1")
            .bind(object_id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?
            .ok_or_else(|| not_found(ObjectType::Event, object_id))?;
        Ok(Event {
            id: id(row.get(0))?,
            created_date_time: time(row.get(1))?,
            modification_date_time: time(row.get(2))?,
            object_type: ObjectType::Event,
            content: decode(row.get(3))?,
        })
    }

    async fn list_events(&self, query: &EventQuery) -> Result<Vec<Event>, StorageError> {
        let filter = query.access.target_filter();
        let targets = target_sql(&filter, ObjectType::Event, "o", 1);
        let mut n = targets.next;
        let mut sql = format!(
            "SELECT o.id, o.created_at, o.modified_at, o.doc FROM event o WHERE {}",
            targets.clause
        );
        if query.program_id.is_some() {
            sql.push_str(&format!(" AND o.program_id = ${n}"));
            n += 1;
        }
        if query.active_at.is_some() {
            // `active_from IS NULL` is a report-only event: nothing to elapse, so always live.
            sql.push_str(&format!(
                " AND (o.active_from IS NULL OR o.active_to IS NULL OR o.active_to > ${n})"
            ));
            n += 1;
        }
        sql.push_str(&format!(
            " ORDER BY o.created_at, o.id LIMIT ${n} OFFSET ${}",
            n + 1
        ));

        let mut q = sqlx::query(&sql);
        if let Some(binds) = target_binds(&filter) {
            q = q.bind(binds);
        }
        if let Some(program_id) = &query.program_id {
            q = q.bind(program_id.to_string());
        }
        if let Some(at) = query.active_at {
            q = q.bind(stamp(at));
        }
        q = q.bind(query.page.limit as i64).bind(query.page.skip as i64);

        let rows = q.fetch_all(&self.pool).await.map_err(db)?;
        rows.iter()
            .map(|row| {
                Ok(Event {
                    id: id(row.get(0))?,
                    created_date_time: time(row.get(1))?,
                    modification_date_time: time(row.get(2))?,
                    object_type: ObjectType::Event,
                    content: decode(row.get(3))?,
                })
            })
            .collect()
    }

    async fn update_event(
        &self,
        object_id: &ObjectId,
        request: EventRequest,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Event, StorageError> {
        let existing = self.get_event(object_id).await?;
        let modified = if existing.content == request {
            existing.modification_date_time
        } else {
            now
        };
        // The window is derived from the intervals, so it is recomputed with them.
        let window = active_window(&request, now).ok().flatten();

        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query(
            "UPDATE event SET modified_at = $2, program_id = $3, active_from = $4, \
             active_to = $5, doc = $6 WHERE id = $1",
        )
        .bind(object_id.as_str())
        .bind(stamp(modified))
        .bind(request.program_id.as_str())
        .bind(window.map(|(from, _)| stamp(from)))
        .bind(window.and_then(|(_, to)| to).map(stamp))
        .bind(encode(&request)?)
        .execute(&mut *tx)
        .await
        .map_err(|e| match db(e) {
            StorageError::DanglingReference { .. } => StorageError::DanglingReference {
                field: "programID",
                value: request.program_id.to_string(),
            },
            other => other,
        })?;
        Self::write_targets(&mut tx, object_id, ObjectType::Event, &request.targets).await?;

        let event = Event {
            id: object_id.clone(),
            created_date_time: existing.created_date_time,
            modification_date_time: modified,
            object_type: ObjectType::Event,
            content: request,
        };
        Self::queue(
            &mut tx,
            fanout,
            AnyObject::Event(event.clone()),
            Operation::Update,
        )
        .await?;
        tx.commit().await.map_err(db)?;
        Ok(event)
    }

    async fn delete_event(
        &self,
        object_id: &ObjectId,
        fanout: &Fanout,
    ) -> Result<Event, StorageError> {
        let event = self.get_event(object_id).await?;
        let mut tx = self.pool.begin().await.map_err(db)?;
        let cascaded = if fanout.is_empty() {
            Vec::new()
        } else {
            Self::cascaded_from_event(&mut tx, object_id).await?
        };
        sqlx::query("DELETE FROM object_target WHERE object_id = $1")
            .bind(object_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query("DELETE FROM event WHERE id = $1")
            .bind(object_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        for object in cascaded {
            Self::queue(&mut tx, fanout, object, Operation::Delete).await?;
        }
        Self::queue(
            &mut tx,
            fanout,
            AnyObject::Event(event.clone()),
            Operation::Delete,
        )
        .await?;
        tx.commit().await.map_err(db)?;
        Ok(event)
    }

    // -- reports -----------------------------------------------------------

    async fn create_report(
        &self,
        request: ReportRequest,
        owner: Option<ClientId>,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Report, StorageError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let object_id = Self::next_id(&mut tx, ObjectType::Report).await?;
        sqlx::query(
            "INSERT INTO report (id, created_at, modified_at, event_id, client_id, \
             client_name, doc) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(object_id.as_str())
        .bind(stamp(now))
        .bind(stamp(now))
        .bind(request.event_id.as_str())
        .bind(owner.as_ref().map(|c| c.as_str()))
        .bind(request.client_name.as_str())
        .bind(encode(&request)?)
        .execute(&mut *tx)
        .await
        .map_err(|e| match db(e) {
            StorageError::DanglingReference { .. } => StorageError::DanglingReference {
                field: "eventID",
                value: request.event_id.to_string(),
            },
            other => other,
        })?;

        let report = Report {
            id: object_id,
            created_date_time: now,
            modification_date_time: now,
            object_type: ObjectType::Report,
            client_id: owner,
            content: request,
        };
        Self::queue(
            &mut tx,
            fanout,
            AnyObject::Report(report.clone()),
            Operation::Create,
        )
        .await?;
        tx.commit().await.map_err(db)?;
        Ok(report)
    }

    async fn get_report(&self, object_id: &ObjectId) -> Result<Report, StorageError> {
        let row = sqlx::query(
            "SELECT id, created_at, modified_at, client_id, doc FROM report WHERE id = $1",
        )
        .bind(object_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?
        .ok_or_else(|| not_found(ObjectType::Report, object_id))?;
        let client: Option<String> = row.get(3);
        Ok(Report {
            id: id(row.get(0))?,
            created_date_time: time(row.get(1))?,
            modification_date_time: time(row.get(2))?,
            object_type: ObjectType::Report,
            client_id: client
                .map(|c| ClientId::new(c).map_err(|e| StorageError::Unavailable(e.to_string())))
                .transpose()?,
            content: decode(row.get(4))?,
        })
    }

    async fn list_reports(&self, query: &ReportQuery) -> Result<Vec<Report>, StorageError> {
        let filter = query.access.owner_filter();
        let owner = owner_sql(&filter, "o.client_id", 1);
        let mut n = owner.next;
        let mut sql = format!(
            "SELECT o.id, o.created_at, o.modified_at, o.client_id, o.doc FROM report o \
             JOIN event e ON e.id = o.event_id WHERE {}",
            owner.clause
        );
        if query.program_id.is_some() {
            sql.push_str(&format!(" AND e.program_id = ${n}"));
            n += 1;
        }
        if query.event_id.is_some() {
            sql.push_str(&format!(" AND o.event_id = ${n}"));
            n += 1;
        }
        if query.client_name.is_some() {
            sql.push_str(&format!(" AND o.client_name = ${n}"));
            n += 1;
        }
        sql.push_str(&format!(
            " ORDER BY o.created_at, o.id LIMIT ${n} OFFSET ${}",
            n + 1
        ));

        let mut q = sqlx::query(&sql);
        if let Some(client) = owner_bind(&filter) {
            q = q.bind(client.to_string());
        }
        if let Some(program_id) = &query.program_id {
            q = q.bind(program_id.to_string());
        }
        if let Some(event_id) = &query.event_id {
            q = q.bind(event_id.to_string());
        }
        if let Some(name) = &query.client_name {
            q = q.bind(name.to_string());
        }
        q = q.bind(query.page.limit as i64).bind(query.page.skip as i64);

        let rows = q.fetch_all(&self.pool).await.map_err(db)?;
        rows.iter()
            .map(|row| {
                let client: Option<String> = row.get(3);
                Ok(Report {
                    id: id(row.get(0))?,
                    created_date_time: time(row.get(1))?,
                    modification_date_time: time(row.get(2))?,
                    object_type: ObjectType::Report,
                    client_id: client
                        .map(|c| {
                            ClientId::new(c).map_err(|e| StorageError::Unavailable(e.to_string()))
                        })
                        .transpose()?,
                    content: decode(row.get(4))?,
                })
            })
            .collect()
    }

    async fn update_report(
        &self,
        object_id: &ObjectId,
        request: ReportRequest,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Report, StorageError> {
        let existing = self.get_report(object_id).await?;
        let modified = if existing.content == request {
            existing.modification_date_time
        } else {
            now
        };

        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query(
            "UPDATE report SET modified_at = $2, event_id = $3, client_name = $4, doc = $5 \
             WHERE id = $1",
        )
        .bind(object_id.as_str())
        .bind(stamp(modified))
        .bind(request.event_id.as_str())
        .bind(request.client_name.as_str())
        .bind(encode(&request)?)
        .execute(&mut *tx)
        .await
        .map_err(|e| match db(e) {
            StorageError::DanglingReference { .. } => StorageError::DanglingReference {
                field: "eventID",
                value: request.event_id.to_string(),
            },
            other => other,
        })?;

        let report = Report {
            id: object_id.clone(),
            created_date_time: existing.created_date_time,
            modification_date_time: modified,
            object_type: ObjectType::Report,
            client_id: existing.client_id,
            content: request,
        };
        Self::queue(
            &mut tx,
            fanout,
            AnyObject::Report(report.clone()),
            Operation::Update,
        )
        .await?;
        tx.commit().await.map_err(db)?;
        Ok(report)
    }

    async fn delete_report(
        &self,
        object_id: &ObjectId,
        fanout: &Fanout,
    ) -> Result<Report, StorageError> {
        let report = self.get_report(object_id).await?;
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query("DELETE FROM report WHERE id = $1")
            .bind(object_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        Self::queue(
            &mut tx,
            fanout,
            AnyObject::Report(report.clone()),
            Operation::Delete,
        )
        .await?;
        tx.commit().await.map_err(db)?;
        Ok(report)
    }

    // -- subscriptions -----------------------------------------------------

    async fn create_subscription(
        &self,
        request: SubscriptionRequest,
        owner: ClientId,
        owner_kind: OwnerKind,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Subscription, StorageError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let object_id = Self::next_id(&mut tx, ObjectType::Subscription).await?;
        sqlx::query(
            "INSERT INTO subscription (id, created_at, modified_at, client_id, owner_kind, \
             client_name, program_id, doc) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(object_id.as_str())
        .bind(stamp(now))
        .bind(stamp(now))
        .bind(owner.as_str())
        .bind(owner_kind.as_str())
        .bind(request.client_name.as_str())
        .bind(request.program_id.as_ref().map(|p| p.as_str()))
        .bind(encode(&request)?)
        .execute(&mut *tx)
        .await
        .map_err(|e| match db(e) {
            StorageError::DanglingReference { .. } => StorageError::DanglingReference {
                field: "programID",
                value: request
                    .program_id
                    .as_ref()
                    .map(|p| p.to_string())
                    .unwrap_or_default(),
            },
            other => other,
        })?;
        write_subscription_objects(&mut tx, &object_id, &request).await?;
        Self::write_targets(
            &mut tx,
            &object_id,
            ObjectType::Subscription,
            &request.targets,
        )
        .await?;

        let subscription = Subscription {
            id: object_id,
            created_date_time: now,
            modification_date_time: now,
            object_type: ObjectType::Subscription,
            client_id: owner,
            content: request,
        };
        Self::queue(
            &mut tx,
            fanout,
            AnyObject::Subscription(subscription.clone()),
            Operation::Create,
        )
        .await?;
        tx.commit().await.map_err(db)?;
        Ok(subscription)
    }

    async fn get_subscription(&self, object_id: &ObjectId) -> Result<Subscription, StorageError> {
        let row = sqlx::query(
            "SELECT id, created_at, modified_at, client_id, doc FROM subscription WHERE id = $1",
        )
        .bind(object_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?
        .ok_or_else(|| not_found(ObjectType::Subscription, object_id))?;
        let client: String = row.get(3);
        Ok(Subscription {
            id: id(row.get(0))?,
            created_date_time: time(row.get(1))?,
            modification_date_time: time(row.get(2))?,
            object_type: ObjectType::Subscription,
            client_id: ClientId::new(client)
                .map_err(|e| StorageError::Unavailable(e.to_string()))?,
            content: decode(row.get(4))?,
        })
    }

    async fn list_subscriptions(
        &self,
        query: &SubscriptionQuery,
    ) -> Result<Vec<Subscription>, StorageError> {
        let filter = query.access.owner_filter();
        let owner = owner_sql(&filter, "o.client_id", 1);
        let mut n = owner.next;
        let mut sql = format!(
            "SELECT o.id, o.created_at, o.modified_at, o.client_id, o.doc FROM subscription o \
             WHERE {}",
            owner.clause
        );
        if query.program_id.is_some() {
            sql.push_str(&format!(" AND o.program_id = ${n}"));
            n += 1;
        }
        if query.client_name.is_some() {
            sql.push_str(&format!(" AND o.client_name = ${n}"));
            n += 1;
        }
        if !query.objects.is_empty() {
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM subscription_object s \
                 WHERE s.subscription_id = o.id AND s.object_type = ANY(${n}))"
            ));
            n += 1;
        }
        sql.push_str(&format!(
            " ORDER BY o.created_at, o.id LIMIT ${n} OFFSET ${}",
            n + 1
        ));

        let mut q = sqlx::query(&sql);
        if let Some(client) = owner_bind(&filter) {
            q = q.bind(client.to_string());
        }
        if let Some(program_id) = &query.program_id {
            q = q.bind(program_id.to_string());
        }
        if let Some(name) = &query.client_name {
            q = q.bind(name.to_string());
        }
        if !query.objects.is_empty() {
            let objects: Vec<String> = query.objects.iter().map(|o| o.to_string()).collect();
            q = q.bind(objects);
        }
        q = q.bind(query.page.limit as i64).bind(query.page.skip as i64);

        let rows = q.fetch_all(&self.pool).await.map_err(db)?;
        rows.iter()
            .map(|row| {
                let client: String = row.get(3);
                Ok(Subscription {
                    id: id(row.get(0))?,
                    created_date_time: time(row.get(1))?,
                    modification_date_time: time(row.get(2))?,
                    object_type: ObjectType::Subscription,
                    client_id: ClientId::new(client)
                        .map_err(|e| StorageError::Unavailable(e.to_string()))?,
                    content: decode(row.get(4))?,
                })
            })
            .collect()
    }

    async fn subscribers(
        &self,
        object_types: &[ObjectType],
    ) -> Result<Vec<Subscriber>, StorageError> {
        if object_types.is_empty() {
            return Ok(Vec::new());
        }
        let objects: Vec<String> = object_types.iter().map(|o| o.to_string()).collect();
        let rows = sqlx::query(
            "SELECT o.id, o.created_at, o.modified_at, o.client_id, o.owner_kind, o.doc \
             FROM subscription o WHERE EXISTS (SELECT 1 FROM subscription_object s \
             WHERE s.subscription_id = o.id AND s.object_type = ANY($1)) \
             ORDER BY o.created_at, o.id",
        )
        .bind(objects)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.iter()
            .map(|row| {
                let client: String = row.get(3);
                let kind: String = row.get(4);
                Ok(Subscriber {
                    subscription: Subscription {
                        id: id(row.get(0))?,
                        created_date_time: time(row.get(1))?,
                        modification_date_time: time(row.get(2))?,
                        object_type: ObjectType::Subscription,
                        client_id: ClientId::new(client)
                            .map_err(|e| StorageError::Unavailable(e.to_string()))?,
                        content: decode(row.get(5))?,
                    },
                    owner: OwnerKind::from_str_or_ven(&kind),
                })
            })
            .collect()
    }

    async fn update_subscription(
        &self,
        object_id: &ObjectId,
        request: SubscriptionRequest,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Subscription, StorageError> {
        let existing = self.get_subscription(object_id).await?;
        let modified = if existing.content == request {
            existing.modification_date_time
        } else {
            now
        };

        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query(
            "UPDATE subscription SET modified_at = $2, client_name = $3, program_id = $4, \
             doc = $5 WHERE id = $1",
        )
        .bind(object_id.as_str())
        .bind(stamp(modified))
        .bind(request.client_name.as_str())
        .bind(request.program_id.as_ref().map(|p| p.as_str()))
        .bind(encode(&request)?)
        .execute(&mut *tx)
        .await
        .map_err(|e| match db(e) {
            StorageError::DanglingReference { .. } => StorageError::DanglingReference {
                field: "programID",
                value: request
                    .program_id
                    .as_ref()
                    .map(|p| p.to_string())
                    .unwrap_or_default(),
            },
            other => other,
        })?;
        write_subscription_objects(&mut tx, object_id, &request).await?;
        Self::write_targets(
            &mut tx,
            object_id,
            ObjectType::Subscription,
            &request.targets,
        )
        .await?;

        let subscription = Subscription {
            id: object_id.clone(),
            created_date_time: existing.created_date_time,
            modification_date_time: modified,
            object_type: ObjectType::Subscription,
            client_id: existing.client_id,
            content: request,
        };
        Self::queue(
            &mut tx,
            fanout,
            AnyObject::Subscription(subscription.clone()),
            Operation::Update,
        )
        .await?;
        tx.commit().await.map_err(db)?;
        Ok(subscription)
    }

    async fn delete_subscription(
        &self,
        object_id: &ObjectId,
        fanout: &Fanout,
    ) -> Result<Subscription, StorageError> {
        let subscription = self.get_subscription(object_id).await?;
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query("DELETE FROM object_target WHERE object_id = $1")
            .bind(object_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query("DELETE FROM subscription WHERE id = $1")
            .bind(object_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        Self::queue(
            &mut tx,
            fanout,
            AnyObject::Subscription(subscription.clone()),
            Operation::Delete,
        )
        .await?;
        tx.commit().await.map_err(db)?;
        Ok(subscription)
    }

    // -- vens --------------------------------------------------------------

    async fn create_ven(&self, mut ven: Ven, fanout: &Fanout) -> Result<Ven, StorageError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let object_id = Self::next_id(&mut tx, ObjectType::Ven).await?;
        sqlx::query(
            "INSERT INTO ven (id, created_at, modified_at, client_id, ven_name, doc) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(object_id.as_str())
        .bind(stamp(ven.created_date_time))
        .bind(stamp(ven.modification_date_time))
        .bind(ven.client_id.as_str())
        .bind(ven.ven_name.as_str())
        .bind(encode(&ven)?)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            conflict_value(e, |field| match field {
                "clientID" => ven.client_id.to_string(),
                _ => ven.ven_name.to_string(),
            })
        })?;
        Self::write_targets(&mut tx, &object_id, ObjectType::Ven, &ven.targets).await?;

        ven.id = object_id;
        Self::queue(
            &mut tx,
            fanout,
            AnyObject::Ven(ven.clone()),
            Operation::Create,
        )
        .await?;
        tx.commit().await.map_err(db)?;
        Ok(ven)
    }

    async fn get_ven(&self, object_id: &ObjectId) -> Result<Ven, StorageError> {
        let row = sqlx::query("SELECT id, created_at, modified_at, doc FROM ven WHERE id = $1")
            .bind(object_id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?
            .ok_or_else(|| not_found(ObjectType::Ven, object_id))?;
        let mut ven: Ven = decode(row.get(3))?;
        ven.id = id(row.get(0))?;
        ven.created_date_time = time(row.get(1))?;
        ven.modification_date_time = time(row.get(2))?;
        Ok(ven)
    }

    async fn get_ven_by_client(&self, client_id: &ClientId) -> Result<Option<Ven>, StorageError> {
        let row =
            sqlx::query("SELECT id, created_at, modified_at, doc FROM ven WHERE client_id = $1")
                .bind(client_id.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(db)?;
        row.map(|row| {
            let mut ven: Ven = decode(row.get(3))?;
            ven.id = id(row.get(0))?;
            ven.created_date_time = time(row.get(1))?;
            ven.modification_date_time = time(row.get(2))?;
            Ok(ven)
        })
        .transpose()
    }

    async fn list_vens(&self, query: &VenQuery) -> Result<Vec<Ven>, StorageError> {
        let filter = query.access.owner_filter();
        let owner = owner_sql(&filter, "o.client_id", 1);
        // `?targets=` on an owned collection is an ordinary additive filter, not the privacy rule:
        // `[Def §Object Privacy]` says target hiding is not performed on ven and resource objects.
        let requested = query.access.requested_filter();
        let targets = target_sql(&requested, ObjectType::Ven, "o", owner.next);
        let mut n = targets.next;
        let mut sql = format!(
            "SELECT o.id, o.created_at, o.modified_at, o.doc FROM ven o WHERE {} AND {}",
            owner.clause, targets.clause
        );
        if query.ven_name.is_some() {
            sql.push_str(&format!(" AND o.ven_name = ${n}"));
            n += 1;
        }
        sql.push_str(&format!(
            " ORDER BY o.created_at, o.id LIMIT ${n} OFFSET ${}",
            n + 1
        ));

        let mut q = sqlx::query(&sql);
        if let Some(client) = owner_bind(&filter) {
            q = q.bind(client.to_string());
        }
        if let Some(binds) = target_binds(&requested) {
            q = q.bind(binds);
        }
        if let Some(name) = &query.ven_name {
            q = q.bind(name.to_string());
        }
        q = q.bind(query.page.limit as i64).bind(query.page.skip as i64);

        let rows = q.fetch_all(&self.pool).await.map_err(db)?;
        rows.iter()
            .map(|row| {
                let mut ven: Ven = decode(row.get(3))?;
                ven.id = id(row.get(0))?;
                ven.created_date_time = time(row.get(1))?;
                ven.modification_date_time = time(row.get(2))?;
                Ok(ven)
            })
            .collect()
    }

    async fn update_ven(
        &self,
        object_id: &ObjectId,
        ven: Ven,
        fanout: &Fanout,
    ) -> Result<Ven, StorageError> {
        let existing = self.get_ven(object_id).await?;
        let unchanged = existing.client_id == ven.client_id
            && existing.ven_name == ven.ven_name
            && existing.targets == ven.targets
            && existing.attributes == ven.attributes;
        let modified = if unchanged {
            existing.modification_date_time
        } else {
            ven.modification_date_time
        };

        let stored = Ven {
            id: object_id.clone(),
            created_date_time: existing.created_date_time,
            modification_date_time: modified,
            ..ven
        };

        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query(
            "UPDATE ven SET modified_at = $2, client_id = $3, ven_name = $4, doc = $5 \
             WHERE id = $1",
        )
        .bind(object_id.as_str())
        .bind(stamp(modified))
        .bind(stored.client_id.as_str())
        .bind(stored.ven_name.as_str())
        .bind(encode(&stored)?)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            conflict_value(e, |field| match field {
                "clientID" => stored.client_id.to_string(),
                _ => stored.ven_name.to_string(),
            })
        })?;
        Self::write_targets(&mut tx, object_id, ObjectType::Ven, &stored.targets).await?;
        Self::queue(
            &mut tx,
            fanout,
            AnyObject::Ven(stored.clone()),
            Operation::Update,
        )
        .await?;
        tx.commit().await.map_err(db)?;
        Ok(stored)
    }

    async fn delete_ven(&self, object_id: &ObjectId, fanout: &Fanout) -> Result<Ven, StorageError> {
        let ven = self.get_ven(object_id).await?;
        let mut tx = self.pool.begin().await.map_err(db)?;
        let cascaded = if fanout.is_empty() {
            Vec::new()
        } else {
            Self::cascaded_from_ven(&mut tx, object_id).await?
        };
        sqlx::query(
            "DELETE FROM object_target WHERE object_id IN \
             (SELECT id FROM resource WHERE ven_id = $1)",
        )
        .bind(object_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query("DELETE FROM object_target WHERE object_id = $1")
            .bind(object_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query("DELETE FROM ven WHERE id = $1")
            .bind(object_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        for object in cascaded {
            Self::queue(&mut tx, fanout, object, Operation::Delete).await?;
        }
        Self::queue(
            &mut tx,
            fanout,
            AnyObject::Ven(ven.clone()),
            Operation::Delete,
        )
        .await?;
        tx.commit().await.map_err(db)?;
        Ok(ven)
    }

    // -- resources ---------------------------------------------------------

    async fn create_resource(
        &self,
        mut resource: Resource,
        fanout: &Fanout,
    ) -> Result<Resource, StorageError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let object_id = Self::next_id(&mut tx, ObjectType::Resource).await?;
        sqlx::query(
            "INSERT INTO resource (id, created_at, modified_at, ven_id, resource_name, doc) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(object_id.as_str())
        .bind(stamp(resource.created_date_time))
        .bind(stamp(resource.modification_date_time))
        .bind(resource.ven_id.as_str())
        .bind(resource.resource_name.as_str())
        .bind(encode(&resource)?)
        .execute(&mut *tx)
        .await
        .map_err(
            |e| match conflict_value(e, |_| resource.resource_name.to_string()) {
                StorageError::DanglingReference { .. } => StorageError::DanglingReference {
                    field: "venID",
                    value: resource.ven_id.to_string(),
                },
                other => other,
            },
        )?;
        Self::write_targets(&mut tx, &object_id, ObjectType::Resource, &resource.targets).await?;

        resource.id = object_id;
        Self::queue(
            &mut tx,
            fanout,
            AnyObject::Resource(resource.clone()),
            Operation::Create,
        )
        .await?;
        tx.commit().await.map_err(db)?;
        Ok(resource)
    }

    async fn get_resource(&self, object_id: &ObjectId) -> Result<Resource, StorageError> {
        let row =
            sqlx::query("SELECT id, created_at, modified_at, doc FROM resource WHERE id = $1")
                .bind(object_id.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(db)?
                .ok_or_else(|| not_found(ObjectType::Resource, object_id))?;
        let mut resource: Resource = decode(row.get(3))?;
        resource.id = id(row.get(0))?;
        resource.created_date_time = time(row.get(1))?;
        resource.modification_date_time = time(row.get(2))?;
        Ok(resource)
    }

    async fn list_resources(&self, query: &ResourceQuery) -> Result<Vec<Resource>, StorageError> {
        // A resource is owned through its VEN, so ownership is a join rather than a column.
        let filter = query.access.owner_filter();
        let owner = owner_sql(&filter, "v.client_id", 1);
        let requested = query.access.requested_filter();
        let targets = target_sql(&requested, ObjectType::Resource, "o", owner.next);
        let mut n = targets.next;
        let mut sql = format!(
            "SELECT o.id, o.created_at, o.modified_at, o.doc FROM resource o \
             JOIN ven v ON v.id = o.ven_id WHERE {} AND {}",
            owner.clause, targets.clause
        );
        if query.ven_id.is_some() {
            sql.push_str(&format!(" AND o.ven_id = ${n}"));
            n += 1;
        }
        if query.resource_name.is_some() {
            sql.push_str(&format!(" AND o.resource_name = ${n}"));
            n += 1;
        }
        sql.push_str(&format!(
            " ORDER BY o.created_at, o.id LIMIT ${n} OFFSET ${}",
            n + 1
        ));

        let mut q = sqlx::query(&sql);
        if let Some(client) = owner_bind(&filter) {
            q = q.bind(client.to_string());
        }
        if let Some(binds) = target_binds(&requested) {
            q = q.bind(binds);
        }
        if let Some(ven_id) = &query.ven_id {
            q = q.bind(ven_id.to_string());
        }
        if let Some(name) = &query.resource_name {
            q = q.bind(name.to_string());
        }
        q = q.bind(query.page.limit as i64).bind(query.page.skip as i64);

        let rows = q.fetch_all(&self.pool).await.map_err(db)?;
        rows.iter()
            .map(|row| {
                let mut resource: Resource = decode(row.get(3))?;
                resource.id = id(row.get(0))?;
                resource.created_date_time = time(row.get(1))?;
                resource.modification_date_time = time(row.get(2))?;
                Ok(resource)
            })
            .collect()
    }

    async fn update_resource(
        &self,
        object_id: &ObjectId,
        resource: Resource,
        fanout: &Fanout,
    ) -> Result<Resource, StorageError> {
        let existing = self.get_resource(object_id).await?;
        let unchanged = existing.resource_name == resource.resource_name
            && existing.ven_id == resource.ven_id
            && existing.targets == resource.targets
            && existing.attributes == resource.attributes;
        let stored = Resource {
            id: object_id.clone(),
            created_date_time: existing.created_date_time,
            modification_date_time: if unchanged {
                existing.modification_date_time
            } else {
                resource.modification_date_time
            },
            ..resource
        };

        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query(
            "UPDATE resource SET modified_at = $2, ven_id = $3, resource_name = $4, doc = $5 \
             WHERE id = $1",
        )
        .bind(object_id.as_str())
        .bind(stamp(stored.modification_date_time))
        .bind(stored.ven_id.as_str())
        .bind(stored.resource_name.as_str())
        .bind(encode(&stored)?)
        .execute(&mut *tx)
        .await
        .map_err(
            |e| match conflict_value(e, |_| stored.resource_name.to_string()) {
                StorageError::DanglingReference { .. } => StorageError::DanglingReference {
                    field: "venID",
                    value: stored.ven_id.to_string(),
                },
                other => other,
            },
        )?;
        Self::write_targets(&mut tx, object_id, ObjectType::Resource, &stored.targets).await?;
        Self::queue(
            &mut tx,
            fanout,
            AnyObject::Resource(stored.clone()),
            Operation::Update,
        )
        .await?;
        tx.commit().await.map_err(db)?;
        Ok(stored)
    }

    async fn delete_resource(
        &self,
        object_id: &ObjectId,
        fanout: &Fanout,
    ) -> Result<Resource, StorageError> {
        let resource = self.get_resource(object_id).await?;
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query("DELETE FROM object_target WHERE object_id = $1")
            .bind(object_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query("DELETE FROM resource WHERE id = $1")
            .bind(object_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        Self::queue(
            &mut tx,
            fanout,
            AnyObject::Resource(resource.clone()),
            Operation::Delete,
        )
        .await?;
        tx.commit().await.map_err(db)?;
        Ok(resource)
    }

    // -- privacy support ---------------------------------------------------

    async fn grant_for(&self, client_id: &ClientId) -> Result<Grant, StorageError> {
        // A client's grant is its VEN's targets plus every one of its resources' targets.
        let rows = sqlx::query(
            "SELECT t.target FROM ven v \
             JOIN object_target t ON t.object_id = v.id AND t.object_type = 'VEN' \
             WHERE v.client_id = $1 \
             UNION \
             SELECT t.target FROM ven v \
             JOIN resource r ON r.ven_id = v.id \
             JOIN object_target t ON t.object_id = r.id AND t.object_type = 'RESOURCE' \
             WHERE v.client_id = $1",
        )
        .bind(client_id.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;

        Ok(Grant::from_targets(rows.iter().filter_map(|row| {
            Target::new(row.get::<String, _>(0)).ok()
        })))
    }

    async fn dead_letters(&self, limit: usize) -> Result<Vec<DeadLetter>, StorageError> {
        let rows = sqlx::query(
            "SELECT id, enqueued_at, attempts, last_error, subscription_id, object_type, \
                    object_id, callback_url, topic \
             FROM outbox WHERE next_attempt_at IS NULL ORDER BY id DESC LIMIT $1",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;

        rows.iter()
            .map(|row| {
                let subscription: Option<String> = row.get(4);
                let object_type: String = row.get(5);
                let callback_url: Option<String> = row.get(7);
                let topic: Option<String> = row.get(8);
                Ok(DeadLetter {
                    id: row.get(0),
                    enqueued_at: time(row.get(1))?,
                    // `attempts` is INTEGER here and INTEGER means 32 bits in Postgres.
                    attempts: row.get::<i32, _>(2) as u32,
                    last_error: row.get(3),
                    subscription_id: subscription.as_deref().map(id).transpose()?,
                    object_type: object_type.parse().map_err(|_| {
                        StorageError::Unavailable(format!("unknown object type {object_type:?}"))
                    })?,
                    object_id: id(row.get(6))?,
                    destination: callback_url.or(topic).unwrap_or_default(),
                })
            })
            .collect()
    }

    async fn revive_dead(&self, now: Timestamp) -> Result<u64, StorageError> {
        // The attempt counter is reset with the entry: those attempts were spent against a receiver
        // that was broken, and charging them to a fixed one abandons the entry on the first try.
        let mut tx = self.pool.begin().await.map_err(db)?;
        let result = sqlx::query(
            "UPDATE outbox SET next_attempt_at = $1, attempts = 0, lease_until = NULL, \
                    lease_owner = NULL \
             WHERE next_attempt_at IS NULL",
        )
        .bind(stamp(now))
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        if result.rows_affected() > 0 {
            // The same announcement a write makes. An operator who has just fixed a receiver and
            // pressed retry is watching; waiting out the idle interval for something that is
            // already due is latency with no reason behind it.
            announce(&mut tx).await?;
        }
        tx.commit().await.map_err(db)?;
        Ok(result.rows_affected())
    }

    async fn note_delivery(
        &self,
        subscription: &ObjectId,
        abandoned: bool,
        now: Timestamp,
        policy: &BreakerPolicy,
        error: Option<&str>,
    ) -> Result<bool, StorageError> {
        if !abandoned {
            // Health is the absence of a row, so a delivery that worked is a delete.
            sqlx::query("DELETE FROM subscriber_health WHERE subscription_id = $1")
                .bind(subscription.as_str())
                .execute(&self.pool)
                .await
                .map_err(db)?;
            return Ok(false);
        }

        let mut tx = self.pool.begin().await.map_err(db)?;
        // Count and read the current state in one statement, so two dispatchers abandoning
        // notifications for the same dead subscriber cannot both believe they opened the breaker.
        let row = sqlx::query(
            "INSERT INTO subscriber_health (subscription_id, consecutive_failures, last_error) \
             VALUES ($1, 1, $2) \
             ON CONFLICT (subscription_id) DO UPDATE SET \
                consecutive_failures = subscriber_health.consecutive_failures + 1, \
                last_error = excluded.last_error \
             RETURNING consecutive_failures, (cut_off_since IS NULL)::int",
        )
        .bind(subscription.as_str())
        .bind(error)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        // `INTEGER` is 32 bits here, as it is for the outbox's `attempts`.
        let failures = u32::try_from(row.get::<i32, _>(0)).unwrap_or(u32::MAX);
        let was_closed: i32 = row.get(1);

        let mut opened = false;
        if policy.trips(failures) {
            opened = was_closed != 0;
            // `COALESCE` so `cut_off_since` says when this outage started, not when it was last
            // extended — which is the number an operator reads.
            sqlx::query(
                "UPDATE subscriber_health \
                 SET retry_at = $2, cut_off_since = COALESCE(cut_off_since, $3) \
                 WHERE subscription_id = $1",
            )
            .bind(subscription.as_str())
            .bind(cooldown_end(now, policy))
            .bind(stamp(now))
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        }
        tx.commit().await.map_err(db)?;
        Ok(opened)
    }

    async fn subscriber_health(&self) -> Result<Vec<SubscriberHealth>, StorageError> {
        let rows = sqlx::query(
            "SELECT subscription_id, consecutive_failures, cut_off_since, retry_at, last_error \
             FROM subscriber_health ORDER BY consecutive_failures DESC, subscription_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.iter()
            .map(|row| {
                Ok(SubscriberHealth {
                    subscription_id: id(row.get(0))?,
                    consecutive_failures: u32::try_from(row.get::<i32, _>(1)).unwrap_or(u32::MAX),
                    cut_off_since: row
                        .get::<Option<String>, _>(2)
                        .map(|s| time(&s))
                        .transpose()?,
                    retry_at: row
                        .get::<Option<String>, _>(3)
                        .map(|s| time(&s))
                        .transpose()?,
                    last_error: row.get(4),
                })
            })
            .collect()
    }

    async fn clear_subscriber_health(&self) -> Result<u64, StorageError> {
        let result = sqlx::query("DELETE FROM subscriber_health")
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(result.rows_affected())
    }

    async fn purge_reports(&self, before: Timestamp, limit: usize) -> Result<u64, StorageError> {
        // `FOR UPDATE SKIP LOCKED`, like the outbox claim: several VTN instances may run a
        // retention task, and without it they select the same oldest rows and one of them does the
        // work twice. `created_at` is `stamp`ed under `COLLATE "C"`, so text ordering is
        // chronological ordering.
        let result = sqlx::query(
            "DELETE FROM report WHERE id IN \
             (SELECT id FROM report WHERE created_at < $1 ORDER BY created_at, id \
              LIMIT $2 FOR UPDATE SKIP LOCKED)",
        )
        .bind(stamp(before))
        .bind(limit as i64)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(result.rows_affected())
    }

    async fn report_stats(&self, now: Timestamp) -> Result<ReportStats, StorageError> {
        let row = sqlx::query("SELECT COUNT(*), MIN(created_at) FROM report")
            .fetch_one(&self.pool)
            .await
            .map_err(db)?;
        let count: i64 = row.get(0);
        let oldest: Option<String> = row.get(1);
        let oldest = oldest.as_deref().map(time).transpose()?;
        Ok(ReportStats {
            count: count.max(0) as u64,
            oldest_seconds: support::age_seconds(now, oldest),
        })
    }

    async fn all_grants(&self) -> Result<Vec<(ObjectId, ClientId, Grant)>, StorageError> {
        let rows = sqlx::query(
            "SELECT v.id, v.client_id, t.target FROM ven v \
             LEFT JOIN object_target t ON t.object_id = v.id AND t.object_type = 'VEN' \
             UNION ALL \
             SELECT v.id, v.client_id, t.target FROM ven v \
             JOIN resource r ON r.ven_id = v.id \
             JOIN object_target t ON t.object_id = r.id AND t.object_type = 'RESOURCE'",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;

        let mut index = GrantIndex::new();
        let mut vens: Vec<(ObjectId, ClientId)> = Vec::new();
        for row in &rows {
            let ven_id = id(row.get(0))?;
            let client = ClientId::new(row.get::<String, _>(1))
                .map_err(|e| StorageError::Unavailable(e.to_string()))?;
            if !vens.iter().any(|(v, _)| v == &ven_id) {
                vens.push((ven_id, client.clone()));
            }
            if let Some(raw) = row.get::<Option<String>, _>(2)
                && let Ok(target) = Target::new(raw)
            {
                index.add(&client, [target]);
            }
        }
        vens.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        Ok(vens
            .into_iter()
            .map(|(ven_id, client)| {
                let grant = index.get(&client);
                (ven_id, client, grant)
            })
            .collect())
    }

    // -- outbox ------------------------------------------------------------

    async fn enqueue(&self, deliveries: Vec<Delivery>, now: Timestamp) -> Result<(), StorageError> {
        if deliveries.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(db)?;
        insert_deliveries(&mut tx, &deliveries, now).await?;
        announce(&mut tx).await?;
        tx.commit().await.map_err(db)?;
        Ok(())
    }

    async fn claim_due(
        &self,
        now: Timestamp,
        limit: usize,
        lease: core::time::Duration,
        owner: &str,
    ) -> Result<Vec<Queued>, StorageError> {
        let lease_until = now
            .checked_add(jiff::Span::new().seconds(lease.as_secs() as i64))
            .unwrap_or(now);

        // `FOR UPDATE SKIP LOCKED` is the whole reason this backend exists for a busy VTN: N
        // dispatchers claim N disjoint batches without any of them waiting on the others. The lease
        // columns are still written, but only so that an entry held by a dispatcher that died is
        // reclaimed rather than stuck — they are no longer how a claim is made.
        let rows = sqlx::query(
            "UPDATE outbox SET lease_until = $2, lease_owner = $3 \
             WHERE id IN ( \
                 SELECT id FROM outbox \
                 WHERE next_attempt_at IS NOT NULL AND next_attempt_at <= $1 \
                   AND (lease_until IS NULL OR lease_until <= $1) \
                 ORDER BY id LIMIT $4 \
                 FOR UPDATE SKIP LOCKED \
             ) \
             RETURNING id, attempts, subscription_id, callback_url, bearer_token, topic, \
                       notification",
        )
        .bind(stamp(now))
        .bind(stamp(lease_until))
        .bind(owner)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;

        let mut claimed: Vec<Queued> = rows
            .iter()
            .map(|row| {
                let subscription_id: Option<String> = row.get(2);
                Ok(Queued {
                    id: OutboxId(row.get::<i64, _>(0)),
                    attempts: row.get::<i32, _>(1) as u32,
                    delivery: Delivery {
                        subscription_id: subscription_id
                            .map(|s| {
                                ObjectId::new(s)
                                    .map_err(|e| StorageError::Unavailable(e.to_string()))
                            })
                            .transpose()?,
                        route: route_from_columns(row.get(3), row.get(4), row.get(5))?,
                        notification: decode(row.get(6))?,
                    },
                })
            })
            .collect::<Result<_, StorageError>>()?;
        // `RETURNING` has no defined order; the queue is meant to drain in enqueue order.
        claimed.sort_by_key(|q| q.id);
        Ok(claimed)
    }

    async fn complete(&self, id: OutboxId) -> Result<(), StorageError> {
        sqlx::query("DELETE FROM outbox WHERE id = $1")
            .bind(id.0)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn record_failure(
        &self,
        id: OutboxId,
        failure: &crate::vtn::notify::DeliveryFailure,
        now: Timestamp,
        policy: &RetryPolicy,
    ) -> Result<bool, StorageError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let Some(row) = sqlx::query("SELECT attempts FROM outbox WHERE id = $1 FOR UPDATE")
            .bind(id.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?
        else {
            return Ok(false);
        };
        let attempts = row.get::<i32, _>(0) as u32 + 1;
        let next = failure
            .retriable
            .then(|| support::next_attempt(now, attempts, policy))
            .flatten();

        sqlx::query(
            "UPDATE outbox SET attempts = $2, last_error = $3, next_attempt_at = $4, \
             lease_until = NULL, lease_owner = NULL WHERE id = $1",
        )
        .bind(id.0)
        .bind(attempts as i32)
        .bind(failure.message.as_str())
        .bind(next.map(stamp))
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(next.is_none())
    }

    async fn outbox_stats(&self, now: Timestamp) -> Result<OutboxStats, StorageError> {
        let row = sqlx::query(
            "SELECT \
                COUNT(*) FILTER (WHERE next_attempt_at IS NOT NULL), \
                COUNT(*) FILTER (WHERE next_attempt_at IS NULL), \
                MIN(enqueued_at) FILTER (WHERE next_attempt_at IS NOT NULL) \
             FROM outbox",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;

        let oldest: Option<String> = row.get(2);
        let oldest = oldest.as_deref().map(time).transpose()?;
        Ok(OutboxStats {
            pending: row.get::<i64, _>(0) as u64,
            dead: row.get::<i64, _>(1) as u64,
            oldest_pending_seconds: support::age_seconds(now, oldest),
        })
    }

    /// Wait for a write to announce itself, rather than polling.
    ///
    /// A curtailment instruction should not sit in the queue for a poll interval because nothing
    /// was happening a moment ago. `LISTEN` costs one connection and removes that delay entirely.
    ///
    /// The timeout is still honoured, and is what makes this safe: if the listening connection is
    /// lost, or a notification is missed, the dispatcher still wakes up and looks. A missed wake-up
    /// costs latency, never a delivery.
    async fn await_outbox(&self, timeout: core::time::Duration) {
        let mut guard = self.listener.lock().await;
        if guard.is_none() {
            match sqlx::postgres::PgListener::connect(&self.url).await {
                Ok(mut listener) => {
                    if let Err(e) = listener.listen(OUTBOX_CHANNEL).await {
                        tracing::warn!(error = %e, "could not LISTEN; falling back to polling");
                    } else {
                        *guard = Some(listener);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not open a listener; falling back to polling")
                }
            }
        }

        match guard.as_mut() {
            Some(listener) => match tokio::time::timeout(timeout, listener.recv()).await {
                // A notification, or the timeout: either way, go and look.
                Ok(Ok(_)) | Err(_) => {}
                Ok(Err(e)) => {
                    // The listening connection failed. Drop it so the next pass reconnects, and
                    // fall back to the timeout in the meantime — a lost listener costs latency,
                    // never a delivery.
                    tracing::warn!(error = %e, "outbox listener failed; will reconnect");
                    *guard = None;
                }
            },
            None => tokio::time::sleep(timeout).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clean store in a database named after the test that asked for it.
    async fn fresh(test: &str) -> PostgresStorage {
        let url = test_database_for(test).await.expect("a Postgres server");
        let store = PostgresStorage::open(&url).await.unwrap();
        store.truncate_all().await.unwrap();
        store
    }

    fn now() -> Timestamp {
        "2026-01-01T00:00:00Z".parse().unwrap()
    }

    fn delivery(n: usize) -> Delivery {
        use crate::model::notification::Notification;
        Delivery {
            subscription_id: None,
            route: crate::vtn::notify::Route::Webhook {
                callback_url: format!("https://example.com/{n}"),
                bearer_token: None,
            },
            notification: Notification::new(
                Operation::Create,
                AnyObject::Program(Program {
                    id: "prg-1".parse().unwrap(),
                    created_date_time: now(),
                    modification_date_time: now(),
                    object_type: ObjectType::Program,
                    content: ProgramRequest::new("p".parse().unwrap()),
                }),
            ),
        }
    }

    /// The reason this backend exists for a busy VTN.
    #[tokio::test]
    async fn two_dispatchers_claim_disjoint_batches() {
        if test_server().await.is_none() {
            return;
        }
        let store = fresh("dispatchers").await;
        store
            .enqueue((0..8).map(delivery).collect(), now())
            .await
            .unwrap();

        let lease = core::time::Duration::from_secs(60);
        // Concurrently, so the two claims genuinely overlap rather than running in sequence.
        let (a, b) = tokio::join!(
            store.claim_due(now(), 4, lease, "d1"),
            store.claim_due(now(), 4, lease, "d2"),
        );
        let (a, b) = (a.unwrap(), b.unwrap());

        assert_eq!(a.len() + b.len(), 8, "every entry should have been claimed");
        for entry in &a {
            assert!(
                !b.iter().any(|other| other.id == entry.id),
                "two dispatchers claimed the same entry"
            );
        }
        // And nothing is left for a third.
        assert!(
            store
                .claim_due(now(), 8, lease, "d3")
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A write should wake a dispatcher, not make it wait for the poll interval.
    #[tokio::test]
    async fn a_write_wakes_a_waiting_dispatcher() {
        if test_server().await.is_none() {
            return;
        }
        let store = Arc::new(fresh("listen_notify").await);

        // Prime the listener, so the wait under test is not measuring a connection setup.
        store
            .await_outbox(core::time::Duration::from_millis(50))
            .await;

        let writer = {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                tokio::time::sleep(core::time::Duration::from_millis(100)).await;
                store.enqueue(vec![delivery(0)], now()).await.unwrap();
            })
        };

        let started = std::time::Instant::now();
        // A generous bound: if `LISTEN` did nothing, this would take the full ten seconds.
        store
            .await_outbox(core::time::Duration::from_secs(10))
            .await;
        let waited = started.elapsed();
        writer.await.unwrap();

        assert!(
            waited < core::time::Duration::from_secs(2),
            "the dispatcher waited {waited:?}, so it polled rather than being told"
        );
        assert_eq!(store.outbox_stats(now()).await.unwrap().pending, 1);
    }

    /// A Postgres to run the conformance suite against, or `None` when there is none to be had.
    ///
    /// `OPENADR_TEST_POSTGRES` first — CI provides a service container, and starting a second one
    /// inside it is slower and no more truthful — then a container started here, so that an
    /// ordinary `cargo test` *runs* this backend rather than skipping it. The suite is the only
    /// thing holding three backends to one set of behaviours (D-032); skipping one quietly reduces
    /// that to two. Started once and reused, with `truncate_all` between behaviours.
    /// The PostgreSQL the storage suite runs against, matching `.github/workflows/ci.yml`.
    const POSTGRES_TAG: &str = "17-alpine";

    /// A database of this test's own, on whatever server `test_server` found.
    ///
    /// Every Postgres test here starts from an empty database, and emptying one means
    /// `TRUNCATE … CASCADE`, which takes an `ACCESS EXCLUSIVE` lock on every table it names. Two
    /// tests sharing a database therefore do not merely interfere — they deadlock, and PostgreSQL
    /// picks one to kill. Cargo runs tests in parallel, so sharing is the default rather than the
    /// accident, and the failure arrives as `deadlock detected` in whichever test lost.
    ///
    /// Created once per name and reused, so a suite that builds a backend per behaviour pays for it
    /// once. `CREATE DATABASE` on one that exists is the reuse, not an error.
    async fn test_database_for(name: &str) -> Option<String> {
        use sqlx::Connection as _;

        let server = test_server().await?;
        let database = format!("openadr_test_{name}");
        let mut conn = sqlx::PgConnection::connect(&server).await.ok()?;
        // Racing another test on the same name is not possible — each names itself — but racing the
        // *server's* catalogue with another `CREATE DATABASE` is, so a duplicate is reuse.
        let _ = sqlx::query(&format!("CREATE DATABASE {database}"))
            .execute(&mut conn)
            .await;
        let _ = conn.close().await;

        let (base, _) = server.rsplit_once('/')?;
        Some(format!("{base}/{database}"))
    }

    /// The PostgreSQL server, from the environment or from a container started here.
    async fn test_server() -> Option<String> {
        use tokio::sync::OnceCell;

        if let Ok(url) = std::env::var("OPENADR_TEST_POSTGRES") {
            return Some(url);
        }

        // The container is kept in a `static` so it outlives every behaviour; testcontainers' own
        // reaper removes it when the test process ends.
        static CONTAINER: OnceCell<
            Option<(
                testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
                String,
            )>,
        > = OnceCell::const_new();

        CONTAINER
            .get_or_init(|| async {
                use testcontainers::ImageExt as _;
                use testcontainers::runners::AsyncRunner as _;
                // Pinned to the version CI runs. The module defaults to Postgres 11, so leaving it
                // alone would test one major version locally and another in CI — a difference that
                // shows up only when a query uses something the older one does not have.
                let container = testcontainers_modules::postgres::Postgres::default()
                    .with_tag(POSTGRES_TAG)
                    .start()
                    .await
                    .inspect_err(|e| {
                        eprintln!(
                            "no Docker for a Postgres container ({e}); set \
                             OPENADR_TEST_POSTGRES to run this backend"
                        )
                    })
                    .ok()?;
                let port = container.get_host_port_ipv4(5432).await.ok()?;
                let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
                Some((container, url))
            })
            .await
            .as_ref()
            .map(|(_, url)| url.clone())
    }

    /// A cascade leaves no target rows behind.
    ///
    /// The same assertion the SQLite backend makes, for the same reason: `object_target` is
    /// polymorphic, so it carries no foreign key and no cascade can reach it — every kind of row a
    /// cascade removes has to be cleared by hand, and the programme-scoped subscriptions were not
    /// (D-123). The storage suite structurally cannot state this: the rows are unreachable through
    /// the trait, and the in-memory backend has no such table.
    #[tokio::test]
    async fn deleting_a_programme_leaves_no_orphaned_target_rows() {
        if test_server().await.is_none() {
            return;
        }
        let store = fresh("orphans").await;

        let program_id = super::super::suite::seed_a_cascade_of_targeted_children(&store).await;
        store
            .delete_program(&program_id, &Fanout::none())
            .await
            .unwrap();

        let orphans: i64 = sqlx::query_scalar(super::super::suite::ORPHANED_TARGETS)
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            orphans, 0,
            "a programme's cascade left {orphans} target rows pointing at objects that are gone"
        );
    }

    // The conformance suite, against a real Postgres — the one CI provides, or one started here.
    super::super::suite::run_suite!(@optional async {
        let url = test_database_for("suite").await?;
        let store = PostgresStorage::open(&url).await.unwrap();
        // Each behaviour starts from an empty database, exactly as the other backends do.
        store.truncate_all().await.unwrap();
        Some(Arc::new(store) as SharedStorage)
    });
}
