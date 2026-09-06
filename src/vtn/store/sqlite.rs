//! The SQLite backend.
//!
//! A site controller, a single-tenant pilot or a home gateway wants one binary and one file, not a
//! database cluster. That deployment is also where the `no_std` and mDNS work pays off, and it is
//! the one a VTN that requires a database server cannot reach at all.
//!
//! ## Shape
//!
//! One table per object, each holding the client-provided request body as JSON plus the columns the
//! API actually queries. Documents keep the wire model authoritative: nothing has to be re-derived
//! when the specification adds a field. Columns keep the queries indexable.
//!
//! Two things the schema does that the in-memory backend has to do by hand:
//!
//! * **Cascades.** Deleting a programme removes its events and their reports; deleting a VEN removes
//!   its resources. Declared once, enforced by the database.
//! * **Uniqueness.** Programme and VEN names, one VEN per `clientID`, resource names within a VEN.
//!
//! ## Filtering
//!
//! Object privacy reaches the `WHERE` clause as *data*: `Access::target_filter` and
//! `Access::owner_filter` reduce the rule to a shape a query can render, so this backend never
//! re-derives it. Filtering therefore happens before pagination, which is the whole point.

use async_trait::async_trait;
use sqlx::{Row, SqlitePool, sqlite::SqlitePoolOptions};
use std::sync::Arc;

use crate::core::{Grant, GrantIndex, OwnerFilter, TargetFilter, active_window};
use crate::model::{
    ClientId, Event, EventRequest, ObjectId, ObjectType, Operation, Program, ProgramRequest,
    Report, ReportRequest, Resource, Subscription, SubscriptionRequest, Target, Timestamp, Ven,
};

use super::outbox::support;
use super::sql::{
    cooldown_end, decode, encode, id, not_found, route_columns, route_from_columns, stamp, time,
};
use super::{
    BreakerPolicy, DeadLetter, EventQuery, OutboxId, OutboxStats, OwnerKind, ProgramQuery, Queued,
    ReportQuery, ReportStats, ResourceQuery, RetryPolicy, SharedStorage, Storage, StorageError,
    Subscriber, SubscriberHealth, SubscriptionQuery, VenQuery, sequenced_id,
};
use crate::model::notification::AnyObject;
use crate::vtn::notify::Delivery;
use crate::vtn::notify::Fanout;

/// The schema.
///
/// Applied on connect. The project is pre-release and the schema is authoritative rather than
/// historical: there are no migrations to run, and a change here is a change to the file format.
const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS program (
    id            TEXT PRIMARY KEY,
    created_at    TEXT NOT NULL,
    modified_at   TEXT NOT NULL,
    program_name  TEXT NOT NULL UNIQUE,
    doc           TEXT NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS event (
    id           TEXT PRIMARY KEY,
    created_at   TEXT NOT NULL,
    modified_at  TEXT NOT NULL,
    program_id   TEXT NOT NULL REFERENCES program(id) ON DELETE CASCADE,
    -- The window the event is live over, resolved once at write time, so `?active=` is a
    -- comparison of two columns rather than an interval expansion per event per request.
    -- NULL `active_from` means "nothing to elapse"; NULL `active_to` means "never ends".
    active_from  TEXT,
    active_to    TEXT,
    doc          TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS event_program ON event(program_id);
CREATE INDEX IF NOT EXISTS event_active ON event(active_to);

CREATE TABLE IF NOT EXISTS report (
    id           TEXT PRIMARY KEY,
    created_at   TEXT NOT NULL,
    modified_at  TEXT NOT NULL,
    event_id     TEXT NOT NULL REFERENCES event(id) ON DELETE CASCADE,
    client_id    TEXT,
    client_name  TEXT NOT NULL,
    doc          TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS report_event ON report(event_id);
CREATE INDEX IF NOT EXISTS report_client ON report(client_id);
-- Retention sweeps oldest-first over the whole table, which without this is a full scan on every
-- pass of a task that runs for ever.
CREATE INDEX IF NOT EXISTS report_created ON report(created_at);

CREATE TABLE IF NOT EXISTS subscription (
    id           TEXT PRIMARY KEY,
    created_at   TEXT NOT NULL,
    modified_at  TEXT NOT NULL,
    client_id    TEXT NOT NULL,
    -- 'BL' or 'VEN'. The fan-out runs long after the request that created the row and has no
    -- credential to ask, and a business-logic subscriber evaluated as a VEN with an empty grant
    -- is told about no targeted object at all.
    owner_kind   TEXT NOT NULL DEFAULT 'VEN',
    client_name  TEXT NOT NULL,
    program_id   TEXT REFERENCES program(id) ON DELETE CASCADE,
    doc          TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS subscription_client ON subscription(client_id);

-- Which object types a subscription watches, so `?objects=` is an indexed join rather than a
-- JSON scan.
CREATE TABLE IF NOT EXISTS subscription_object (
    subscription_id TEXT NOT NULL REFERENCES subscription(id) ON DELETE CASCADE,
    object_type     TEXT NOT NULL,
    PRIMARY KEY (subscription_id, object_type)
) STRICT;
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
    created_at   TEXT NOT NULL,
    modified_at  TEXT NOT NULL,
    client_id    TEXT NOT NULL UNIQUE,
    ven_name     TEXT NOT NULL UNIQUE,
    doc          TEXT NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS resource (
    id             TEXT PRIMARY KEY,
    created_at     TEXT NOT NULL,
    modified_at    TEXT NOT NULL,
    ven_id         TEXT NOT NULL REFERENCES ven(id) ON DELETE CASCADE,
    resource_name  TEXT NOT NULL,
    doc            TEXT NOT NULL,
    UNIQUE (ven_id, resource_name)
) STRICT;

-- Targets for every kind of object that carries them, in one table so the privacy predicate is one
-- join whatever it is filtering.
CREATE TABLE IF NOT EXISTS object_target (
    object_id    TEXT NOT NULL,
    object_type  TEXT NOT NULL,
    target       TEXT NOT NULL,
    PRIMARY KEY (object_id, object_type, target)
) STRICT;
CREATE INDEX IF NOT EXISTS object_target_lookup ON object_target(target, object_type);

-- Monotonic identifiers, so ordering by (created_at, id) is stable and pages never reshuffle.
CREATE TABLE IF NOT EXISTS id_sequence (
    name  TEXT PRIMARY KEY,
    next  INTEGER NOT NULL
) STRICT;

-- The transactional outbox. One row per (change, recipient), written in the same transaction as the
-- change itself, so a notification cannot be lost to a crash between the two.
-- The subscriber circuit breaker. Only the unhealthy have a row, so the table's size is the size
-- of the problem and a working VTN reads nothing here.
CREATE TABLE IF NOT EXISTS subscriber_health (
    subscription_id      TEXT PRIMARY KEY REFERENCES subscription(id) ON DELETE CASCADE,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    -- When the breaker last opened, and when one probe will be let through. NULL means closed.
    cut_off_since        TEXT,
    retry_at             TEXT,
    last_error           TEXT
) STRICT;

CREATE TABLE IF NOT EXISTS outbox (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    enqueued_at      TEXT NOT NULL,
    subscription_id  TEXT,
    -- What the notification is about. Not needed to deliver it — the payload already carries the
    -- object — but an operator looking at a stuck or abandoned row needs to know which event or
    -- report it concerns, and digging that out of a JSON column is not an answer.
    object_type      TEXT NOT NULL,
    object_id        TEXT NOT NULL,
    callback_url     TEXT,
    bearer_token     TEXT,
    topic            TEXT,
    notification     TEXT NOT NULL,
    attempts         INTEGER NOT NULL DEFAULT 0,
    -- NULL means abandoned: attempts are exhausted and it will not be tried again.
    next_attempt_at  TEXT,
    -- A lease held by a dispatcher. Expired or NULL means the entry is available.
    lease_until      TEXT,
    lease_owner      TEXT,
    last_error       TEXT
) STRICT;
CREATE INDEX IF NOT EXISTS outbox_due ON outbox(next_attempt_at, lease_until);
"#;

/// A durable store backed by SQLite.
#[derive(Debug, Clone)]
pub struct SqliteStorage {
    pool: SqlitePool,
}

impl SqliteStorage {
    /// Open (creating if absent) a database at a path, or `:memory:`.
    pub async fn open(url: &str) -> Result<Self, StorageError> {
        let url = if url.starts_with("sqlite:") {
            url.to_string()
        } else {
            format!("sqlite://{url}?mode=rwc")
        };
        let options = url
            .parse::<sqlx::sqlite::SqliteConnectOptions>()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?
            // Write-ahead logging, so readers do not block the writer. Without it a `GET` taken
            // while the dispatcher is completing a delivery is a lock wait.
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            // SQLite has one writer. Two concurrent writes are normal here — a `POST /reports` and
            // the dispatcher recording a delivery — and without a busy timeout the loser gets
            // `SQLITE_BUSY` immediately, which would surface as a 500 for a request that merely
            // needed to wait a moment.
            .busy_timeout(std::time::Duration::from_secs(5))
            // NORMAL is the documented safe pairing with WAL: durable across process crashes,
            // fsync only at checkpoints rather than every commit.
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);

        let pool = SqlitePoolOptions::new()
            // SQLite takes a database-wide write lock, so a larger pool buys nothing on writes and
            // costs lock contention. Reads are served from the same connections.
            .max_connections(4)
            .connect_with(options)
            .await
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;

        let store = Self { pool };
        store.apply_schema().await?;
        Ok(store)
    }

    /// Open and wrap in an `Arc`, ready for [`crate::vtn::VtnBuilder::storage`].
    pub async fn shared(url: &str) -> Result<SharedStorage, StorageError> {
        Ok(Arc::new(Self::open(url).await?))
    }

    async fn apply_schema(&self) -> Result<(), StorageError> {
        // `raw_sql` runs the whole script. Splitting on `;` would be wrong: a semicolon inside a
        // comment or a string literal is not a statement boundary.
        sqlx::raw_sql(SCHEMA)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    /// Mint an identifier that sorts in creation order.
    async fn next_id(
        &self,
        tx: &mut sqlx::SqliteConnection,
        kind: ObjectType,
    ) -> Result<ObjectId, StorageError> {
        let name = kind.collection();
        sqlx::query(
            "INSERT INTO id_sequence (name, next) VALUES (?1, 1)
             ON CONFLICT(name) DO UPDATE SET next = next + 1",
        )
        .bind(name)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        let n: i64 = sqlx::query("SELECT next FROM id_sequence WHERE name = ?1")
            .bind(name)
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?
            .get(0);

        Ok(sequenced_id(kind, n))
    }
}

/// Map a database failure onto a storage error, recognising the constraint violations that the API
/// reports as `409` rather than `500`.
fn db(e: sqlx::Error) -> StorageError {
    if let sqlx::Error::Database(err) = &e {
        let message = err.message().to_ascii_lowercase();
        if message.contains("unique constraint failed") {
            let (object_type, field) = classify_unique(&message);
            return StorageError::Conflict {
                object_type,
                field,
                value: message.clone(),
            };
        }
        if message.contains("foreign key constraint failed") {
            return StorageError::DanglingReference {
                field: "reference",
                value: message.clone(),
            };
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

/// Recover which constraint a SQLite uniqueness message refers to.
///
/// SQLite names the columns (`UNIQUE constraint failed: ven.client_id`), which is enough to report
/// the field the client can act on rather than a generic conflict.
fn classify_unique(message: &str) -> (ObjectType, &'static str) {
    if message.contains("program.program_name") {
        (ObjectType::Program, "programName")
    } else if message.contains("ven.client_id") {
        (ObjectType::Ven, "clientID")
    } else if message.contains("ven.ven_name") {
        (ObjectType::Ven, "venName")
    } else if message.contains("resource.resource_name") || message.contains("resource.ven_id") {
        (ObjectType::Resource, "resourceName")
    } else {
        (ObjectType::Program, "unknown")
    }
}

/// The `WHERE` fragment and bindings for a target filter.
///
/// Rendered from [`TargetFilter`], which is derived from [`Access`] — so the predicate here means
/// exactly what the in-memory backend evaluates, and the conformance suite proves it.
struct TargetSql {
    clause: String,
    binds: Vec<Target>,
}

fn target_sql(filter: &TargetFilter, object_type: ObjectType, alias: &str) -> TargetSql {
    let untargeted = format!(
        "NOT EXISTS (SELECT 1 FROM object_target t \
         WHERE t.object_id = {alias}.id AND t.object_type = '{}')",
        object_type.as_str()
    );
    match filter {
        TargetFilter::Any => TargetSql {
            clause: "1 = 1".into(),
            binds: Vec::new(),
        },
        TargetFilter::Untargeted => TargetSql {
            clause: untargeted,
            binds: Vec::new(),
        },
        TargetFilter::UntargetedOrAnyOf(allowed) if allowed.is_empty() => TargetSql {
            clause: untargeted,
            binds: Vec::new(),
        },
        TargetFilter::AnyOf(allowed) if allowed.is_empty() => TargetSql {
            clause: "0 = 1".into(),
            binds: Vec::new(),
        },
        TargetFilter::UntargetedOrAnyOf(allowed) => TargetSql {
            clause: format!("({untargeted} OR {})", carries(alias, object_type, allowed)),
            binds: allowed.clone(),
        },
        TargetFilter::AnyOf(allowed) => TargetSql {
            clause: carries(alias, object_type, allowed),
            binds: allowed.clone(),
        },
    }
}

/// `EXISTS` over the target table, with one placeholder per target.
fn carries(alias: &str, object_type: ObjectType, allowed: &[Target]) -> String {
    let placeholders = vec!["?"; allowed.len()].join(", ");
    format!(
        "EXISTS (SELECT 1 FROM object_target t \
         WHERE t.object_id = {alias}.id AND t.object_type = '{}' \
         AND t.target IN ({placeholders}))",
        object_type.as_str()
    )
}

/// The `WHERE` fragment for an ownership filter.
fn owner_sql(filter: &OwnerFilter, column: &str) -> (String, Option<ClientId>) {
    match filter {
        OwnerFilter::Any => ("1 = 1".into(), None),
        OwnerFilter::Only(client) => (format!("{column} = ?"), Some(client.clone())),
        OwnerFilter::Nothing => ("1 = 0".into(), None),
    }
}

impl SqliteStorage {
    /// Queue the notifications a change produces, inside the transaction that made it.
    ///
    /// This is the whole point of threading a [`Fanout`] through the mutating methods: the change
    /// and the record that it must be announced commit together, so a crash cannot land between
    /// them and lose a dispatch instruction.
    async fn queue(
        tx: &mut sqlx::SqliteConnection,
        fanout: &Fanout,
        object: AnyObject,
        operation: Operation,
    ) -> Result<(), StorageError> {
        insert_deliveries(tx, &fanout.deliveries(&object, operation), fanout.now()).await
    }

    /// The objects a cascade is about to take with it.
    ///
    /// A cascade deletes objects a subscriber asked to hear about, and the database will not
    /// announce them. Reading them first costs two queries on a `DELETE` — and only when something
    /// is actually subscribed — and it is the difference between a VEN learning that its dispatch
    /// was cancelled and holding an event that no longer exists.
    async fn cascaded_from_program(
        tx: &mut sqlx::SqliteConnection,
        program_id: &ObjectId,
    ) -> Result<Vec<AnyObject>, StorageError> {
        let mut out = Vec::new();
        let events = sqlx::query(
            "SELECT id, created_at, modified_at, doc FROM event WHERE program_id = ?1 \
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
             JOIN event e ON e.id = r.event_id WHERE e.program_id = ?1 ORDER BY r.created_at, r.id",
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
             WHERE program_id = ?1 ORDER BY created_at, id",
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
        tx: &mut sqlx::SqliteConnection,
        event_id: &ObjectId,
    ) -> Result<Vec<AnyObject>, StorageError> {
        let rows = sqlx::query(
            "SELECT id, created_at, modified_at, client_id, doc FROM report WHERE event_id = ?1 \
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
        tx: &mut sqlx::SqliteConnection,
        ven_id: &ObjectId,
    ) -> Result<Vec<AnyObject>, StorageError> {
        let rows = sqlx::query(
            "SELECT id, created_at, modified_at, doc FROM resource WHERE ven_id = ?1 \
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
        tx: &mut sqlx::SqliteConnection,
        object_id: &ObjectId,
        object_type: ObjectType,
        targets: &[Target],
    ) -> Result<(), StorageError> {
        sqlx::query("DELETE FROM object_target WHERE object_id = ?1 AND object_type = ?2")
            .bind(object_id.as_str())
            .bind(object_type.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        for target in targets {
            sqlx::query(
                "INSERT OR IGNORE INTO object_target (object_id, object_type, target) \
                 VALUES (?1, ?2, ?3)",
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

#[async_trait]
impl Storage for SqliteStorage {
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
        let object_id = self.next_id(&mut tx, ObjectType::Program).await?;
        sqlx::query(
            "INSERT INTO program (id, created_at, modified_at, program_name, doc) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
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
        let row = sqlx::query("SELECT id, created_at, modified_at, doc FROM program WHERE id = ?1")
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
        let targets = target_sql(&query.access.target_filter(), ObjectType::Program, "o");
        let mut sql = format!(
            "SELECT o.id, o.created_at, o.modified_at, o.doc FROM program o WHERE {}",
            targets.clause
        );
        if query.program_name.is_some() {
            sql.push_str(" AND o.program_name = ?");
        }
        sql.push_str(" ORDER BY o.created_at, o.id LIMIT ? OFFSET ?");

        let mut q = sqlx::query(&sql);
        for target in &targets.binds {
            q = q.bind(target.as_str());
        }
        if let Some(name) = &query.program_name {
            q = q.bind(name.as_str());
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
        // A `PUT` that changes nothing must not wake every subscriber: a client that polls on
        // `modificationDateTime` would see a change that did not happen.
        let modified = if existing.content == request {
            existing.modification_date_time
        } else {
            now
        };

        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query(
            "UPDATE program SET modified_at = ?2, program_name = ?3, doc = ?4 WHERE id = ?1",
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
        // Events and reports go with it by cascade; their target rows are not reachable from a
        // cascade, so they are cleared here.
        sqlx::query(
            "DELETE FROM object_target WHERE object_id IN \
             (SELECT id FROM event WHERE program_id = ?1)",
        )
        .bind(object_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query("DELETE FROM object_target WHERE object_id = ?1")
            .bind(object_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query("DELETE FROM program WHERE id = ?1")
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
        let object_id = self.next_id(&mut tx, ObjectType::Event).await?;
        sqlx::query(
            "INSERT INTO event (id, created_at, modified_at, program_id, active_from, active_to, doc) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
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
        let row = sqlx::query("SELECT id, created_at, modified_at, doc FROM event WHERE id = ?1")
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
        let targets = target_sql(&query.access.target_filter(), ObjectType::Event, "o");
        let mut sql = format!(
            "SELECT o.id, o.created_at, o.modified_at, o.doc FROM event o WHERE {}",
            targets.clause
        );
        if query.program_id.is_some() {
            sql.push_str(" AND o.program_id = ?");
        }
        if query.active_at.is_some() {
            // `active_from IS NULL` is a report-only event: nothing to elapse, so always live.
            sql.push_str(" AND (o.active_from IS NULL OR o.active_to IS NULL OR o.active_to > ?)");
        }
        sql.push_str(" ORDER BY o.created_at, o.id LIMIT ? OFFSET ?");

        let mut q = sqlx::query(&sql);
        for target in &targets.binds {
            q = q.bind(target.as_str());
        }
        if let Some(program_id) = &query.program_id {
            q = q.bind(program_id.as_str());
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
            "UPDATE event SET modified_at = ?2, program_id = ?3, active_from = ?4, \
             active_to = ?5, doc = ?6 WHERE id = ?1",
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
        sqlx::query("DELETE FROM object_target WHERE object_id = ?1")
            .bind(object_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query("DELETE FROM event WHERE id = ?1")
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
        let report = {
            let object_id = self.next_id(&mut tx, ObjectType::Report).await?;
            sqlx::query(
                "INSERT INTO report (id, created_at, modified_at, event_id, client_id, \
                 client_name, doc) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
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
            Report {
                id: object_id,
                created_date_time: now,
                modification_date_time: now,
                object_type: ObjectType::Report,
                client_id: owner,
                content: request,
            }
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
            "SELECT id, created_at, modified_at, client_id, doc FROM report WHERE id = ?1",
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
        let (owner_clause, owner_bind) = owner_sql(&query.access.owner_filter(), "o.client_id");
        let mut sql = format!(
            "SELECT o.id, o.created_at, o.modified_at, o.client_id, o.doc FROM report o \
             JOIN event e ON e.id = o.event_id WHERE {owner_clause}"
        );
        if query.program_id.is_some() {
            sql.push_str(" AND e.program_id = ?");
        }
        if query.event_id.is_some() {
            sql.push_str(" AND o.event_id = ?");
        }
        if query.client_name.is_some() {
            sql.push_str(" AND o.client_name = ?");
        }
        sql.push_str(" ORDER BY o.created_at, o.id LIMIT ? OFFSET ?");

        let mut q = sqlx::query(&sql);
        if let Some(client) = &owner_bind {
            q = q.bind(client.as_str());
        }
        if let Some(program_id) = &query.program_id {
            q = q.bind(program_id.as_str());
        }
        if let Some(event_id) = &query.event_id {
            q = q.bind(event_id.as_str());
        }
        if let Some(name) = &query.client_name {
            q = q.bind(name.as_str());
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
            "UPDATE report SET modified_at = ?2, event_id = ?3, client_name = ?4, doc = ?5 \
             WHERE id = ?1",
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
        sqlx::query("DELETE FROM report WHERE id = ?1")
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
        let object_id = self.next_id(&mut tx, ObjectType::Subscription).await?;
        sqlx::query(
            "INSERT INTO subscription (id, created_at, modified_at, client_id, owner_kind, \
             client_name, program_id, doc) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
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
            "SELECT id, created_at, modified_at, client_id, doc FROM subscription WHERE id = ?1",
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
        let (owner_clause, owner_bind) = owner_sql(&query.access.owner_filter(), "o.client_id");
        let mut sql = format!(
            "SELECT o.id, o.created_at, o.modified_at, o.client_id, o.doc FROM subscription o \
             WHERE {owner_clause}"
        );
        if query.program_id.is_some() {
            sql.push_str(" AND o.program_id = ?");
        }
        if query.client_name.is_some() {
            sql.push_str(" AND o.client_name = ?");
        }
        if !query.objects.is_empty() {
            let placeholders = vec!["?"; query.objects.len()].join(", ");
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM subscription_object s \
                 WHERE s.subscription_id = o.id AND s.object_type IN ({placeholders}))"
            ));
        }
        sql.push_str(" ORDER BY o.created_at, o.id LIMIT ? OFFSET ?");

        let mut q = sqlx::query(&sql);
        if let Some(client) = &owner_bind {
            q = q.bind(client.as_str());
        }
        if let Some(program_id) = &query.program_id {
            q = q.bind(program_id.as_str());
        }
        if let Some(name) = &query.client_name {
            q = q.bind(name.as_str());
        }
        for object_type in &query.objects {
            q = q.bind(object_type.as_str());
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
        let placeholders = vec!["?"; object_types.len()].join(", ");
        let sql = format!(
            "SELECT o.id, o.created_at, o.modified_at, o.client_id, o.owner_kind, o.doc \
             FROM subscription o WHERE EXISTS (SELECT 1 FROM subscription_object s \
             WHERE s.subscription_id = o.id AND s.object_type IN ({placeholders})) \
             ORDER BY o.created_at, o.id"
        );
        let mut q = sqlx::query(&sql);
        for object_type in object_types {
            q = q.bind(object_type.as_str());
        }
        let rows = q.fetch_all(&self.pool).await.map_err(db)?;
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
            "UPDATE subscription SET modified_at = ?2, client_name = ?3, program_id = ?4, \
             doc = ?5 WHERE id = ?1",
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
        sqlx::query("DELETE FROM object_target WHERE object_id = ?1")
            .bind(object_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query("DELETE FROM subscription WHERE id = ?1")
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
        let object_id = self.next_id(&mut tx, ObjectType::Ven).await?;
        sqlx::query(
            "INSERT INTO ven (id, created_at, modified_at, client_id, ven_name, doc) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
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
        let row = sqlx::query("SELECT id, created_at, modified_at, doc FROM ven WHERE id = ?1")
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
            sqlx::query("SELECT id, created_at, modified_at, doc FROM ven WHERE client_id = ?1")
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
        let (owner_clause, owner_bind) = owner_sql(&query.access.owner_filter(), "o.client_id");
        // `?targets=` on an owned collection is an ordinary additive filter, not the privacy rule:
        // `[Def §Object Privacy]` says target hiding is not performed on ven and resource objects.
        let targets = target_sql(&query.access.requested_filter(), ObjectType::Ven, "o");
        let mut sql = format!(
            "SELECT o.id, o.created_at, o.modified_at, o.doc FROM ven o \
             WHERE {owner_clause} AND {}",
            targets.clause
        );
        if query.ven_name.is_some() {
            sql.push_str(" AND o.ven_name = ?");
        }
        sql.push_str(" ORDER BY o.created_at, o.id LIMIT ? OFFSET ?");

        let mut q = sqlx::query(&sql);
        if let Some(client) = &owner_bind {
            q = q.bind(client.as_str());
        }
        for target in &targets.binds {
            q = q.bind(target.as_str());
        }
        if let Some(name) = &query.ven_name {
            q = q.bind(name.as_str());
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
            "UPDATE ven SET modified_at = ?2, client_id = ?3, ven_name = ?4, doc = ?5 \
             WHERE id = ?1",
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
             (SELECT id FROM resource WHERE ven_id = ?1)",
        )
        .bind(object_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query("DELETE FROM object_target WHERE object_id = ?1")
            .bind(object_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query("DELETE FROM ven WHERE id = ?1")
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
        let object_id = self.next_id(&mut tx, ObjectType::Resource).await?;
        sqlx::query(
            "INSERT INTO resource (id, created_at, modified_at, ven_id, resource_name, doc) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
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
            sqlx::query("SELECT id, created_at, modified_at, doc FROM resource WHERE id = ?1")
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
        let (owner_clause, owner_bind) = owner_sql(&query.access.owner_filter(), "v.client_id");
        let targets = target_sql(&query.access.requested_filter(), ObjectType::Resource, "o");
        let mut sql = format!(
            "SELECT o.id, o.created_at, o.modified_at, o.doc FROM resource o \
             JOIN ven v ON v.id = o.ven_id WHERE {owner_clause} AND {}",
            targets.clause
        );
        if query.ven_id.is_some() {
            sql.push_str(" AND o.ven_id = ?");
        }
        if query.resource_name.is_some() {
            sql.push_str(" AND o.resource_name = ?");
        }
        sql.push_str(" ORDER BY o.created_at, o.id LIMIT ? OFFSET ?");

        let mut q = sqlx::query(&sql);
        if let Some(client) = &owner_bind {
            q = q.bind(client.as_str());
        }
        for target in &targets.binds {
            q = q.bind(target.as_str());
        }
        if let Some(ven_id) = &query.ven_id {
            q = q.bind(ven_id.as_str());
        }
        if let Some(name) = &query.resource_name {
            q = q.bind(name.as_str());
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
            "UPDATE resource SET modified_at = ?2, ven_id = ?3, resource_name = ?4, doc = ?5 \
             WHERE id = ?1",
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
        sqlx::query("DELETE FROM object_target WHERE object_id = ?1")
            .bind(object_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query("DELETE FROM resource WHERE id = ?1")
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
             WHERE v.client_id = ?1 \
             UNION \
             SELECT t.target FROM ven v \
             JOIN resource r ON r.ven_id = v.id \
             JOIN object_target t ON t.object_id = r.id AND t.object_type = 'RESOURCE' \
             WHERE v.client_id = ?1",
        )
        .bind(client_id.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;

        Ok(Grant::from_targets(rows.iter().filter_map(|row| {
            Target::new(row.get::<String, _>(0)).ok()
        })))
    }

    // -- outbox ------------------------------------------------------------

    async fn enqueue(&self, deliveries: Vec<Delivery>, now: Timestamp) -> Result<(), StorageError> {
        if deliveries.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(db)?;
        insert_deliveries(&mut tx, &deliveries, now).await?;
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

        // Claim and read back in one transaction, so two dispatchers cannot take the same rows.
        let mut tx = self.pool.begin().await.map_err(db)?;
        let rows = sqlx::query(
            "SELECT id, attempts, subscription_id, callback_url, bearer_token, topic, notification \
             FROM outbox \
             WHERE next_attempt_at IS NOT NULL AND next_attempt_at <= ?1 \
               AND (lease_until IS NULL OR lease_until <= ?1) \
             ORDER BY id LIMIT ?2",
        )
        .bind(stamp(now))
        .bind(limit as i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;

        let mut claimed = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: i64 = row.get(0);
            sqlx::query("UPDATE outbox SET lease_until = ?2, lease_owner = ?3 WHERE id = ?1")
                .bind(id)
                .bind(stamp(lease_until))
                .bind(owner)
                .execute(&mut *tx)
                .await
                .map_err(db)?;

            let subscription_id: Option<String> = row.get(2);
            claimed.push(Queued {
                id: OutboxId(id),
                attempts: row.get::<i64, _>(1) as u32,
                delivery: Delivery {
                    subscription_id: subscription_id
                        .map(|s| {
                            ObjectId::new(s).map_err(|e| StorageError::Unavailable(e.to_string()))
                        })
                        .transpose()?,
                    route: route_from_columns(row.get(3), row.get(4), row.get(5))?,
                    notification: decode(row.get(6))?,
                },
            });
        }
        tx.commit().await.map_err(db)?;
        Ok(claimed)
    }

    async fn complete(&self, id: OutboxId) -> Result<(), StorageError> {
        sqlx::query("DELETE FROM outbox WHERE id = ?1")
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
        let Some(row) = sqlx::query("SELECT attempts FROM outbox WHERE id = ?1")
            .bind(id.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?
        else {
            return Ok(false);
        };
        let attempts = row.get::<i64, _>(0) as u32 + 1;
        let next = failure
            .retriable
            .then(|| support::next_attempt(now, attempts, policy))
            .flatten();

        sqlx::query(
            "UPDATE outbox SET attempts = ?2, last_error = ?3, next_attempt_at = ?4, \
             lease_until = NULL, lease_owner = NULL WHERE id = ?1",
        )
        .bind(id.0)
        .bind(attempts as i64)
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

    async fn dead_letters(&self, limit: usize) -> Result<Vec<DeadLetter>, StorageError> {
        let rows = sqlx::query(
            "SELECT id, enqueued_at, attempts, last_error, subscription_id, object_type, \
                    object_id, callback_url, topic \
             FROM outbox WHERE next_attempt_at IS NULL ORDER BY id DESC LIMIT ?1",
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
                    attempts: row.get::<i64, _>(2) as u32,
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
        let result = sqlx::query(
            "UPDATE outbox SET next_attempt_at = ?1, attempts = 0, lease_until = NULL, \
                    lease_owner = NULL \
             WHERE next_attempt_at IS NULL",
        )
        .bind(stamp(now))
        .execute(&self.pool)
        .await
        .map_err(db)?;
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
            sqlx::query("DELETE FROM subscriber_health WHERE subscription_id = ?1")
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
             VALUES (?1, 1, ?2) \
             ON CONFLICT(subscription_id) DO UPDATE SET \
                consecutive_failures = subscriber_health.consecutive_failures + 1, \
                last_error = excluded.last_error \
             RETURNING consecutive_failures, cut_off_since IS NULL",
        )
        .bind(subscription.as_str())
        .bind(error)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        let failures = u32::try_from(row.get::<i64, _>(0)).unwrap_or(u32::MAX);
        let was_closed: i64 = row.get(1);

        let mut opened = false;
        if policy.trips(failures) {
            opened = was_closed != 0;
            // `COALESCE` so `cut_off_since` says when this outage started, not when it was last
            // extended — which is the number an operator reads.
            sqlx::query(
                "UPDATE subscriber_health \
                 SET retry_at = ?2, cut_off_since = COALESCE(cut_off_since, ?3) \
                 WHERE subscription_id = ?1",
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
                    consecutive_failures: u32::try_from(row.get::<i64, _>(1)).unwrap_or(u32::MAX),
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
        // Oldest first and bounded, so a first sweep over a year of data is many small deletes
        // rather than one that locks the table. `created_at` is `stamp`ed, so text ordering is
        // chronological ordering (see `sql::stamp`).
        let result = sqlx::query(
            "DELETE FROM report WHERE id IN \
             (SELECT id FROM report WHERE created_at < ? ORDER BY created_at, id LIMIT ?)",
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
}

/// Record which object types a subscription watches.
async fn write_subscription_objects(
    tx: &mut sqlx::SqliteConnection,
    subscription_id: &ObjectId,
    request: &SubscriptionRequest,
) -> Result<(), StorageError> {
    sqlx::query("DELETE FROM subscription_object WHERE subscription_id = ?1")
        .bind(subscription_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    for operation in &request.object_operations {
        for object_type in &operation.objects {
            sqlx::query(
                "INSERT OR IGNORE INTO subscription_object (subscription_id, object_type) \
                 VALUES (?1, ?2)",
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

/// Insert one queued delivery. Shared by [`Storage::enqueue`] and the in-transaction queue.
/// How many deliveries go into one `INSERT`.
///
/// Eight bound values per row. SQLite's compiled-in parameter limit is 999 on older builds and
/// 32 766 on newer ones, so 64 rows — 512 parameters — is comfortably inside the oldest of them and
/// removes 63 of every 64 statement round trips.
const INSERT_CHUNK: usize = 64;

/// Queue every delivery a write produced, in as few statements as the parameter limit allows.
///
/// One `INSERT` per delivery was the obvious shape and it is what the fan-out costs: a targeted
/// event with a thousand entitled subscribers is a thousand statements inside one transaction, which
/// `tests/load.rs` measured at 14 ms p50 on SQLite and 153 ms on PostgreSQL. The work per row is
/// trivial; preparing and executing the statement is not, and it is paid once per row `[D-116]`.
///
/// Order is preserved: `outbox.id` is `AUTOINCREMENT` and a multi-row insert assigns it in value
/// order, so the queue still drains in the order it was filled — which the storage conformance
/// suite asserts.
async fn insert_deliveries(
    tx: &mut sqlx::SqliteConnection,
    deliveries: &[Delivery],
    now: Timestamp,
) -> Result<(), StorageError> {
    let stamped = stamp(now);
    for chunk in deliveries.chunks(INSERT_CHUNK) {
        // `(?, …)` groups, one per row. Built rather than written out because the count varies, and
        // the *shape* is fixed: nine columns, the first and last of which are the same timestamp.
        let values = vec!["(?, ?, ?, ?, ?, ?, ?, ?, ?)"; chunk.len()].join(", ");
        let statement = crate::std_shim::format!(
            "INSERT INTO outbox (enqueued_at, subscription_id, object_type, object_id, \
             callback_url, bearer_token, topic, notification, next_attempt_at) VALUES {values}"
        );
        let mut query = sqlx::query(&statement);
        for delivery in chunk {
            let (object_type, object_id) = support::subject(delivery);
            let (callback_url, bearer_token, topic) = route_columns(&delivery.route);
            query = query
                .bind(stamped.clone())
                .bind(delivery.subscription_id.as_ref().map(|s| s.to_string()))
                .bind(object_type.to_string())
                .bind(object_id.to_string())
                .bind(callback_url.map(str::to_string))
                .bind(bearer_token.map(str::to_string))
                .bind(topic.map(str::to_string))
                .bind(encode(&delivery.notification)?)
                .bind(stamped.clone());
        }
        query.execute(&mut *tx).await.map_err(db)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    super::super::suite::run_suite!(async {
        // A file-backed database in a unique temporary directory, so each behaviour starts clean
        // and the schema is exercised exactly as it would be in production.
        let dir = std::env::temp_dir().join(format!(
            "openadr-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("openadr.sqlite");
        SqliteStorage::shared(path.to_str().unwrap()).await.unwrap()
    });
}
