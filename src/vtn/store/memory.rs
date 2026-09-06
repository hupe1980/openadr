//! The in-memory backend.
//!
//! Complete and fully consistent, which makes it the right backend for tests, for a single-site
//! gateway that can rebuild its state on start, and for a public price server fed from an upstream
//! feed. It does not survive a restart.

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use crate::core::{Grant, GrantIndex, active_window};
use crate::model::{
    ClientId, Event, EventRequest, ObjectId, ObjectType, Operation, Program, ProgramRequest,
    Report, ReportRequest, Resource, Subscription, SubscriptionRequest, Timestamp, Ven,
};

use super::outbox::support;
use super::{
    BreakerPolicy, DeadLetter, EventQuery, OutboxId, OutboxStats, OwnerKind, ProgramQuery, Queued,
    ReportQuery, ReportStats, ResourceQuery, RetryPolicy, SharedStorage, Storage, StorageError,
    Subscriber, SubscriberHealth, SubscriptionQuery, VenQuery, order_by_creation, sequenced_id,
};
use crate::model::notification::AnyObject;
use crate::vtn::notify::{Delivery, Fanout};

/// An in-memory store.
///
/// Complete and fully consistent. It does not survive a restart, which is why the binary refuses to
/// pair it with a non-loopback bind address without an explicit acknowledgement.
#[derive(Debug, Default)]
pub struct MemoryStorage {
    inner: RwLock<Inner>,
    /// One counter per object kind, matching the SQL backends' `id_sequence` table.
    id_seq: std::sync::Mutex<BTreeMap<ObjectType, i64>>,
}

/// An event plus the window it is live over, resolved once at write time.
///
/// `?active=true` then costs two timestamp comparisons instead of expanding every event's intervals
/// on every read — and it is exactly the pair of columns a SQL backend would index.
#[derive(Debug, Clone)]
struct StoredEvent {
    event: Event,
    /// `None` for a report-only event, which never elapses.
    window: Option<(Timestamp, Option<Timestamp>)>,
}

impl StoredEvent {
    fn new(event: Event, now: Timestamp) -> Self {
        // An event whose intervals cannot be resolved is accepted here and rejected at the API
        // boundary; storage does not get to reinterpret it.
        let window = active_window(&event.content, now).ok().flatten();
        Self { event, window }
    }

    fn is_active_at(&self, at: Timestamp) -> bool {
        match self.window {
            None => true,
            Some((_, None)) => true,
            Some((_, Some(end))) => end > at,
        }
    }
}

/// A queued delivery and its attempt state.
#[derive(Debug, Clone)]
struct OutboxRow {
    delivery: Delivery,
    enqueued_at: Timestamp,
    attempts: u32,
    /// When it may next be attempted. `None` means abandoned.
    next_attempt_at: Option<Timestamp>,
    lease_until: Option<Timestamp>,
    last_error: Option<String>,
}

/// Where a delivery was headed, for a diagnostic listing.
fn destination(delivery: &Delivery) -> String {
    match &delivery.route {
        crate::vtn::notify::Route::Webhook { callback_url, .. } => callback_url.clone(),
        crate::vtn::notify::Route::Topic { topic } => topic.clone(),
    }
}

#[derive(Debug, Default)]
struct Inner {
    outbox: BTreeMap<i64, OutboxRow>,
    outbox_seq: i64,
    programs: BTreeMap<ObjectId, Program>,
    events: BTreeMap<ObjectId, StoredEvent>,
    reports: BTreeMap<ObjectId, Report>,
    subscriptions: BTreeMap<ObjectId, Subscription>,
    /// What kind of client created each subscription. Beside the object rather than on it,
    /// because `Subscription` is a wire type and the specification gives it nowhere to say so.
    subscription_owners: BTreeMap<ObjectId, OwnerKind>,
    vens: BTreeMap<ObjectId, Ven>,
    resources: BTreeMap<ObjectId, Resource>,
    /// Delivery health, keyed by subscription. Only the unhealthy have an entry.
    health: BTreeMap<ObjectId, SubscriberHealth>,
}

impl Inner {
    /// Add one delivery to the queue.
    fn push(&mut self, delivery: Delivery, now: Timestamp) {
        self.outbox_seq += 1;
        let id = self.outbox_seq;
        self.outbox.insert(
            id,
            OutboxRow {
                delivery,
                enqueued_at: now,
                attempts: 0,
                next_attempt_at: Some(now),
                lease_until: None,
                last_error: None,
            },
        );
    }

    /// Queue the notifications a change produces.
    ///
    /// Called with the write lock still held, which is this backend's equivalent of "in the same
    /// transaction": nothing can observe the change without also observing that it must be
    /// announced.
    fn queue(&mut self, fanout: &Fanout, object: AnyObject, operation: Operation) {
        for delivery in fanout.deliveries(&object, operation) {
            self.push(delivery, fanout.now());
        }
    }
}

impl MemoryStorage {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// A shared handle to an empty store.
    pub fn shared() -> SharedStorage {
        Arc::new(Self::new())
    }

    /// Mint an identifier.
    ///
    /// One counter **per kind**, starting at 1, through the same [`sequenced_id`] the SQL backends
    /// use — so the same sequence of writes produces the same identifiers whichever backend a
    /// fixture was written against. A durable backend would use a UUIDv7 for the same time-ordering
    /// property across restarts.
    fn next_id(&self, kind: ObjectType) -> ObjectId {
        let n = self
            .id_seq
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(kind)
            .and_modify(|n| *n += 1)
            .or_insert(1)
            .to_owned();
        sequenced_id(kind, n)
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }
}

#[async_trait]
impl Storage for MemoryStorage {
    async fn healthy(&self) -> bool {
        true
    }

    async fn create_program(
        &self,
        request: ProgramRequest,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Program, StorageError> {
        let mut inner = self.write();
        if inner
            .programs
            .values()
            .any(|p| p.content.program_name == request.program_name)
        {
            return Err(StorageError::Conflict {
                object_type: ObjectType::Program,
                field: "programName",
                value: request.program_name.to_string(),
            });
        }
        let program = Program {
            id: self.next_id(ObjectType::Program),
            created_date_time: now,
            modification_date_time: now,
            object_type: ObjectType::Program,
            content: request,
        };
        inner.programs.insert(program.id.clone(), program.clone());
        inner.queue(
            fanout,
            AnyObject::Program(program.clone()),
            Operation::Create,
        );
        Ok(program)
    }

    async fn get_program(&self, id: &ObjectId) -> Result<Program, StorageError> {
        self.read()
            .programs
            .get(id)
            .cloned()
            .ok_or_else(|| StorageError::NotFound {
                object_type: ObjectType::Program,
                id: id.clone(),
            })
    }

    async fn list_programs(&self, query: &ProgramQuery) -> Result<Vec<Program>, StorageError> {
        let inner = self.read();
        let matched: Vec<Program> = inner
            .programs
            .values()
            .filter(|p| {
                query
                    .program_name
                    .as_ref()
                    .is_none_or(|n| &p.content.program_name == n)
            })
            .filter(|p| query.access.admits(&p.content.targets))
            .cloned()
            .collect();
        let mut matched = matched;
        order_by_creation(&mut matched, |p| (p.created_date_time, &p.id));
        Ok(query.page.apply(&matched))
    }

    async fn update_program(
        &self,
        id: &ObjectId,
        request: ProgramRequest,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Program, StorageError> {
        let mut inner = self.write();
        if inner
            .programs
            .values()
            .any(|p| &p.id != id && p.content.program_name == request.program_name)
        {
            return Err(StorageError::Conflict {
                object_type: ObjectType::Program,
                field: "programName",
                value: request.program_name.to_string(),
            });
        }
        let existing = inner
            .programs
            .get_mut(id)
            .ok_or_else(|| StorageError::NotFound {
                object_type: ObjectType::Program,
                id: id.clone(),
            })?;
        // Only bump the modification time when something actually changed, or a client polling
        // on it sees a change that did not happen.
        if existing.content != request {
            existing.content = request;
            existing.modification_date_time = now;
        }
        let updated = existing.clone();
        inner.queue(
            fanout,
            AnyObject::Program(updated.clone()),
            Operation::Update,
        );
        Ok(updated)
    }

    async fn delete_program(
        &self,
        id: &ObjectId,
        fanout: &Fanout,
    ) -> Result<Program, StorageError> {
        let mut inner = self.write();
        let program = inner
            .programs
            .remove(id)
            .ok_or_else(|| StorageError::NotFound {
                object_type: ObjectType::Program,
                id: id.clone(),
            })?;
        // Events belong to exactly one programme; deleting the parent deletes them, and the reports
        // that answer them.
        let orphaned: Vec<ObjectId> = inner
            .events
            .values()
            .filter(|e| &e.event.content.program_id == id)
            .map(|e| e.event.id.clone())
            .collect();
        let mut cascaded = Vec::new();
        for event_id in &orphaned {
            if let Some(stored) = inner.events.remove(event_id) {
                cascaded.push(AnyObject::Event(stored.event));
            }
            let (gone, kept) = inner
                .reports
                .values()
                .cloned()
                .partition::<Vec<_>, _>(|r| &r.content.event_id == event_id);
            cascaded.extend(gone.into_iter().map(AnyObject::Report));
            inner.reports = kept.into_iter().map(|r| (r.id.clone(), r)).collect();
        }
        // A subscription scoped to this programme goes with it too: `programID` is a foreign key,
        // and leaving one dangling would give a VEN a subscription that can never match anything.
        let (gone, kept) = inner
            .subscriptions
            .values()
            .cloned()
            .partition::<Vec<_>, _>(|s| s.content.program_id.as_ref() == Some(id));
        cascaded.extend(gone.into_iter().map(AnyObject::Subscription));
        inner.subscriptions = kept.into_iter().map(|s| (s.id.clone(), s)).collect();

        // A cascade deletes objects a subscriber asked to hear about. Announcing only the parent
        // would leave a VEN holding an event that no longer exists and no way to find out — and a
        // cancelled dispatch instruction is the notification that matters most.
        for object in cascaded {
            inner.queue(fanout, object, Operation::Delete);
        }
        inner.queue(
            fanout,
            AnyObject::Program(program.clone()),
            Operation::Delete,
        );
        Ok(program)
    }

    async fn create_event(
        &self,
        request: EventRequest,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Event, StorageError> {
        let mut inner = self.write();
        if !inner.programs.contains_key(&request.program_id) {
            return Err(StorageError::DanglingReference {
                field: "programID",
                value: request.program_id.to_string(),
            });
        }
        let event = Event {
            id: self.next_id(ObjectType::Event),
            created_date_time: now,
            modification_date_time: now,
            object_type: ObjectType::Event,
            content: request,
        };
        inner
            .events
            .insert(event.id.clone(), StoredEvent::new(event.clone(), now));
        inner.queue(fanout, AnyObject::Event(event.clone()), Operation::Create);
        Ok(event)
    }

    async fn get_event(&self, id: &ObjectId) -> Result<Event, StorageError> {
        self.read()
            .events
            .get(id)
            .map(|e| e.event.clone())
            .ok_or_else(|| StorageError::NotFound {
                object_type: ObjectType::Event,
                id: id.clone(),
            })
    }

    async fn list_events(&self, query: &EventQuery) -> Result<Vec<Event>, StorageError> {
        let inner = self.read();
        let matched: Vec<Event> = inner
            .events
            .values()
            .filter(|e| {
                query
                    .program_id
                    .as_ref()
                    .is_none_or(|p| &e.event.content.program_id == p)
            })
            .filter(|e| query.active_at.is_none_or(|at| e.is_active_at(at)))
            .filter(|e| query.access.admits(&e.event.content.targets))
            .map(|e| e.event.clone())
            .collect();
        let mut matched = matched;
        order_by_creation(&mut matched, |e| (e.created_date_time, &e.id));
        Ok(query.page.apply(&matched))
    }

    async fn update_event(
        &self,
        id: &ObjectId,
        request: EventRequest,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Event, StorageError> {
        let mut inner = self.write();
        if !inner.programs.contains_key(&request.program_id) {
            return Err(StorageError::DanglingReference {
                field: "programID",
                value: request.program_id.to_string(),
            });
        }
        let existing = inner
            .events
            .get_mut(id)
            .ok_or_else(|| StorageError::NotFound {
                object_type: ObjectType::Event,
                id: id.clone(),
            })?;
        if existing.event.content != request {
            existing.event.content = request;
            existing.event.modification_date_time = now;
            // The active window is derived from the intervals, so it is recomputed with them.
            *existing = StoredEvent::new(existing.event.clone(), now);
        }
        let updated = existing.event.clone();
        inner.queue(fanout, AnyObject::Event(updated.clone()), Operation::Update);
        Ok(updated)
    }

    async fn delete_event(&self, id: &ObjectId, fanout: &Fanout) -> Result<Event, StorageError> {
        let mut inner = self.write();
        let event = inner
            .events
            .remove(id)
            .ok_or_else(|| StorageError::NotFound {
                object_type: ObjectType::Event,
                id: id.clone(),
            })?;
        // The reports go with it, and they are announced. A VEN that filed a compliance report is
        // subscribed to its deletion; taking it away silently leaves the VEN believing it is still
        // on record, and OpenADR has no way to say otherwise afterwards.
        let cascaded: Vec<Report> = if fanout.is_empty() {
            Vec::new()
        } else {
            let mut cascaded: Vec<Report> = inner
                .reports
                .values()
                .filter(|r| &r.content.event_id == id)
                .cloned()
                .collect();
            cascaded.sort_by(|a, b| {
                a.created_date_time
                    .cmp(&b.created_date_time)
                    .then_with(|| a.id.cmp(&b.id))
            });
            cascaded
        };
        inner.reports.retain(|_, r| &r.content.event_id != id);
        for report in cascaded {
            inner.queue(fanout, AnyObject::Report(report), Operation::Delete);
        }
        inner.queue(
            fanout,
            AnyObject::Event(event.event.clone()),
            Operation::Delete,
        );
        Ok(event.event)
    }

    async fn create_report(
        &self,
        request: ReportRequest,
        owner: Option<ClientId>,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Report, StorageError> {
        let mut inner = self.write();
        if !inner.events.contains_key(&request.event_id) {
            return Err(StorageError::DanglingReference {
                field: "eventID",
                value: request.event_id.to_string(),
            });
        }
        let report = Report {
            id: self.next_id(ObjectType::Report),
            created_date_time: now,
            modification_date_time: now,
            object_type: ObjectType::Report,
            client_id: owner,
            content: request,
        };
        inner.reports.insert(report.id.clone(), report.clone());
        inner.queue(fanout, AnyObject::Report(report.clone()), Operation::Create);
        Ok(report)
    }

    async fn get_report(&self, id: &ObjectId) -> Result<Report, StorageError> {
        self.read()
            .reports
            .get(id)
            .cloned()
            .ok_or_else(|| StorageError::NotFound {
                object_type: ObjectType::Report,
                id: id.clone(),
            })
    }

    async fn list_reports(&self, query: &ReportQuery) -> Result<Vec<Report>, StorageError> {
        let inner = self.read();
        let matched: Vec<Report> = inner
            .reports
            .values()
            .filter(|r| {
                query
                    .event_id
                    .as_ref()
                    .is_none_or(|e| &r.content.event_id == e)
            })
            .filter(|r| {
                query
                    .client_name
                    .as_ref()
                    .is_none_or(|c| &r.content.client_name == c)
            })
            .filter(|r| {
                // `programID` reaches reports through their event.
                query.program_id.as_ref().is_none_or(|p| {
                    inner
                        .events
                        .get(&r.content.event_id)
                        .is_some_and(|e| &e.event.content.program_id == p)
                })
            })
            .filter(|r| query.access.owns(r.client_id.as_ref()))
            .cloned()
            .collect();
        let mut matched = matched;
        order_by_creation(&mut matched, |r| (r.created_date_time, &r.id));
        Ok(query.page.apply(&matched))
    }

    async fn update_report(
        &self,
        id: &ObjectId,
        request: ReportRequest,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Report, StorageError> {
        let mut inner = self.write();
        if !inner.events.contains_key(&request.event_id) {
            return Err(StorageError::DanglingReference {
                field: "eventID",
                value: request.event_id.to_string(),
            });
        }
        let existing = inner
            .reports
            .get_mut(id)
            .ok_or_else(|| StorageError::NotFound {
                object_type: ObjectType::Report,
                id: id.clone(),
            })?;
        if existing.content != request {
            existing.content = request;
            existing.modification_date_time = now;
        }
        let updated = existing.clone();
        inner.queue(
            fanout,
            AnyObject::Report(updated.clone()),
            Operation::Update,
        );
        Ok(updated)
    }

    async fn delete_report(&self, id: &ObjectId, fanout: &Fanout) -> Result<Report, StorageError> {
        let mut inner = self.write();
        let report = inner
            .reports
            .remove(id)
            .ok_or_else(|| StorageError::NotFound {
                object_type: ObjectType::Report,
                id: id.clone(),
            })?;
        inner.queue(fanout, AnyObject::Report(report.clone()), Operation::Delete);
        Ok(report)
    }

    async fn create_subscription(
        &self,
        request: SubscriptionRequest,
        owner: ClientId,
        owner_kind: OwnerKind,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Subscription, StorageError> {
        let mut inner = self.write();
        if let Some(program_id) = &request.program_id
            && !inner.programs.contains_key(program_id)
        {
            return Err(StorageError::DanglingReference {
                field: "programID",
                value: program_id.to_string(),
            });
        }
        let subscription = Subscription {
            id: self.next_id(ObjectType::Subscription),
            created_date_time: now,
            modification_date_time: now,
            object_type: ObjectType::Subscription,
            client_id: owner,
            content: request,
        };
        inner
            .subscription_owners
            .insert(subscription.id.clone(), owner_kind);
        inner
            .subscriptions
            .insert(subscription.id.clone(), subscription.clone());
        inner.queue(
            fanout,
            AnyObject::Subscription(subscription.clone()),
            Operation::Create,
        );
        Ok(subscription)
    }

    async fn get_subscription(&self, id: &ObjectId) -> Result<Subscription, StorageError> {
        self.read()
            .subscriptions
            .get(id)
            .cloned()
            .ok_or_else(|| StorageError::NotFound {
                object_type: ObjectType::Subscription,
                id: id.clone(),
            })
    }

    async fn list_subscriptions(
        &self,
        query: &SubscriptionQuery,
    ) -> Result<Vec<Subscription>, StorageError> {
        let inner = self.read();
        let matched: Vec<Subscription> = inner
            .subscriptions
            .values()
            .filter(|s| {
                query
                    .program_id
                    .as_ref()
                    .is_none_or(|p| s.content.program_id.as_ref() == Some(p))
            })
            .filter(|s| {
                query
                    .client_name
                    .as_ref()
                    .is_none_or(|c| &s.content.client_name == c)
            })
            .filter(|s| {
                query.objects.is_empty()
                    || s.content
                        .object_operations
                        .iter()
                        .any(|op| op.objects.iter().any(|o| query.objects.contains(o)))
            })
            // A subscription carries its subscriber's callback URL and bearer token. Ownership is
            // the only thing standing between one VEN and another's credentials.
            .filter(|s| query.access.owns(Some(&s.client_id)))
            .cloned()
            .collect();
        let mut matched = matched;
        order_by_creation(&mut matched, |s| (s.created_date_time, &s.id));
        Ok(query.page.apply(&matched))
    }

    async fn subscribers(
        &self,
        object_types: &[ObjectType],
    ) -> Result<Vec<Subscriber>, StorageError> {
        let inner = self.read();
        let mut matched: Vec<Subscriber> = inner
            .subscriptions
            .values()
            .filter(|s| {
                s.content
                    .object_operations
                    .iter()
                    .any(|op| op.objects.iter().any(|o| object_types.contains(o)))
            })
            .map(|s| Subscriber {
                subscription: s.clone(),
                // A subscription whose owner kind was never recorded is a VEN: fail closed.
                owner: inner
                    .subscription_owners
                    .get(&s.id)
                    .copied()
                    .unwrap_or(OwnerKind::Ven),
            })
            .collect();
        order_by_creation(&mut matched, |s| {
            (s.subscription.created_date_time, &s.subscription.id)
        });
        Ok(matched)
    }

    async fn update_subscription(
        &self,
        id: &ObjectId,
        request: SubscriptionRequest,
        now: Timestamp,
        fanout: &Fanout,
    ) -> Result<Subscription, StorageError> {
        let mut inner = self.write();
        if let Some(program_id) = &request.program_id
            && !inner.programs.contains_key(program_id)
        {
            return Err(StorageError::DanglingReference {
                field: "programID",
                value: program_id.to_string(),
            });
        }
        let existing = inner
            .subscriptions
            .get_mut(id)
            .ok_or_else(|| StorageError::NotFound {
                object_type: ObjectType::Subscription,
                id: id.clone(),
            })?;
        if existing.content != request {
            existing.content = request;
            existing.modification_date_time = now;
        }
        let updated = existing.clone();
        inner.queue(
            fanout,
            AnyObject::Subscription(updated.clone()),
            Operation::Update,
        );
        Ok(updated)
    }

    async fn delete_subscription(
        &self,
        id: &ObjectId,
        fanout: &Fanout,
    ) -> Result<Subscription, StorageError> {
        let mut inner = self.write();
        let subscription =
            inner
                .subscriptions
                .remove(id)
                .ok_or_else(|| StorageError::NotFound {
                    object_type: ObjectType::Subscription,
                    id: id.clone(),
                })?;
        inner.subscription_owners.remove(id);
        inner.queue(
            fanout,
            AnyObject::Subscription(subscription.clone()),
            Operation::Delete,
        );
        Ok(subscription)
    }

    async fn create_ven(&self, mut ven: Ven, fanout: &Fanout) -> Result<Ven, StorageError> {
        let mut inner = self.write();
        if inner.vens.values().any(|v| v.ven_name == ven.ven_name) {
            return Err(StorageError::Conflict {
                object_type: ObjectType::Ven,
                field: "venName",
                value: ven.ven_name.to_string(),
            });
        }
        // One VEN object per client: a VEN-written request identifies itself only by its token, so
        // a second object for the same client would make `PUT /vens/{id}` ambiguous.
        if inner.vens.values().any(|v| v.client_id == ven.client_id) {
            return Err(StorageError::Conflict {
                object_type: ObjectType::Ven,
                field: "clientID",
                value: ven.client_id.to_string(),
            });
        }
        ven.id = self.next_id(ObjectType::Ven);
        inner.vens.insert(ven.id.clone(), ven.clone());
        inner.queue(fanout, AnyObject::Ven(ven.clone()), Operation::Create);
        Ok(ven)
    }

    async fn get_ven(&self, id: &ObjectId) -> Result<Ven, StorageError> {
        self.read()
            .vens
            .get(id)
            .cloned()
            .ok_or_else(|| StorageError::NotFound {
                object_type: ObjectType::Ven,
                id: id.clone(),
            })
    }

    async fn get_ven_by_client(&self, client_id: &ClientId) -> Result<Option<Ven>, StorageError> {
        Ok(self
            .read()
            .vens
            .values()
            .find(|v| &v.client_id == client_id)
            .cloned())
    }

    async fn list_vens(&self, query: &VenQuery) -> Result<Vec<Ven>, StorageError> {
        let inner = self.read();
        let matched: Vec<Ven> = inner
            .vens
            .values()
            .filter(|v| query.ven_name.as_ref().is_none_or(|n| &v.ven_name == n))
            .filter(|v| query.access.requested_filter().admits(&v.targets))
            .filter(|v| query.access.owns(Some(&v.client_id)))
            .cloned()
            .collect();
        let mut matched = matched;
        order_by_creation(&mut matched, |v| (v.created_date_time, &v.id));
        Ok(query.page.apply(&matched))
    }

    async fn update_ven(
        &self,
        id: &ObjectId,
        ven: Ven,
        fanout: &Fanout,
    ) -> Result<Ven, StorageError> {
        let mut inner = self.write();
        if inner
            .vens
            .values()
            .any(|v| &v.id != id && v.ven_name == ven.ven_name)
        {
            return Err(StorageError::Conflict {
                object_type: ObjectType::Ven,
                field: "venName",
                value: ven.ven_name.to_string(),
            });
        }
        let existing = inner
            .vens
            .get_mut(id)
            .ok_or_else(|| StorageError::NotFound {
                object_type: ObjectType::Ven,
                id: id.clone(),
            })?;
        let replacement = Ven {
            id: id.clone(),
            created_date_time: existing.created_date_time,
            // A `PUT` that changes nothing must not wake every subscriber.
            modification_date_time: if content_eq(existing, &ven) {
                existing.modification_date_time
            } else {
                ven.modification_date_time
            },
            ..ven
        };
        *existing = replacement;
        let updated = existing.clone();
        inner.queue(fanout, AnyObject::Ven(updated.clone()), Operation::Update);
        Ok(updated)
    }

    async fn delete_ven(&self, id: &ObjectId, fanout: &Fanout) -> Result<Ven, StorageError> {
        let mut inner = self.write();
        let ven = inner
            .vens
            .remove(id)
            .ok_or_else(|| StorageError::NotFound {
                object_type: ObjectType::Ven,
                id: id.clone(),
            })?;
        let (gone, kept) = inner
            .resources
            .values()
            .cloned()
            .partition::<Vec<_>, _>(|r| &r.ven_id == id);
        inner.resources = kept.into_iter().map(|r| (r.id.clone(), r)).collect();
        for resource in gone {
            inner.queue(fanout, AnyObject::Resource(resource), Operation::Delete);
        }
        inner.queue(fanout, AnyObject::Ven(ven.clone()), Operation::Delete);
        Ok(ven)
    }

    async fn create_resource(
        &self,
        mut resource: Resource,
        fanout: &Fanout,
    ) -> Result<Resource, StorageError> {
        let mut inner = self.write();
        if !inner.vens.contains_key(&resource.ven_id) {
            return Err(StorageError::DanglingReference {
                field: "venID",
                value: resource.ven_id.to_string(),
            });
        }
        if inner
            .resources
            .values()
            .any(|r| r.ven_id == resource.ven_id && r.resource_name == resource.resource_name)
        {
            return Err(StorageError::Conflict {
                object_type: ObjectType::Resource,
                field: "resourceName",
                value: resource.resource_name.to_string(),
            });
        }
        resource.id = self.next_id(ObjectType::Resource);
        inner
            .resources
            .insert(resource.id.clone(), resource.clone());
        inner.queue(
            fanout,
            AnyObject::Resource(resource.clone()),
            Operation::Create,
        );
        Ok(resource)
    }

    async fn get_resource(&self, id: &ObjectId) -> Result<Resource, StorageError> {
        self.read()
            .resources
            .get(id)
            .cloned()
            .ok_or_else(|| StorageError::NotFound {
                object_type: ObjectType::Resource,
                id: id.clone(),
            })
    }

    async fn list_resources(&self, query: &ResourceQuery) -> Result<Vec<Resource>, StorageError> {
        let inner = self.read();
        let matched: Vec<Resource> = inner
            .resources
            .values()
            .filter(|r| query.ven_id.as_ref().is_none_or(|v| &r.ven_id == v))
            .filter(|r| {
                query
                    .resource_name
                    .as_ref()
                    .is_none_or(|n| &r.resource_name == n)
            })
            .filter(|r| query.access.requested_filter().admits(&r.targets))
            // A resource is owned through its VEN, so the parent decides.
            .filter(|r| {
                query
                    .access
                    .owns(inner.vens.get(&r.ven_id).map(|v| &v.client_id))
            })
            .cloned()
            .collect();
        let mut matched = matched;
        order_by_creation(&mut matched, |r| (r.created_date_time, &r.id));
        Ok(query.page.apply(&matched))
    }

    async fn update_resource(
        &self,
        id: &ObjectId,
        resource: Resource,
        fanout: &Fanout,
    ) -> Result<Resource, StorageError> {
        let mut inner = self.write();
        if !inner.vens.contains_key(&resource.ven_id) {
            return Err(StorageError::DanglingReference {
                field: "venID",
                value: resource.ven_id.to_string(),
            });
        }
        if inner.resources.values().any(|r| {
            &r.id != id && r.ven_id == resource.ven_id && r.resource_name == resource.resource_name
        }) {
            return Err(StorageError::Conflict {
                object_type: ObjectType::Resource,
                field: "resourceName",
                value: resource.resource_name.to_string(),
            });
        }
        let existing = inner
            .resources
            .get_mut(id)
            .ok_or_else(|| StorageError::NotFound {
                object_type: ObjectType::Resource,
                id: id.clone(),
            })?;
        let replacement = Resource {
            id: id.clone(),
            created_date_time: existing.created_date_time,
            modification_date_time: if resource_content_eq(existing, &resource) {
                existing.modification_date_time
            } else {
                resource.modification_date_time
            },
            ..resource
        };
        *existing = replacement;
        let updated = existing.clone();
        inner.queue(
            fanout,
            AnyObject::Resource(updated.clone()),
            Operation::Update,
        );
        Ok(updated)
    }

    async fn delete_resource(
        &self,
        id: &ObjectId,
        fanout: &Fanout,
    ) -> Result<Resource, StorageError> {
        let mut inner = self.write();
        let resource = inner
            .resources
            .remove(id)
            .ok_or_else(|| StorageError::NotFound {
                object_type: ObjectType::Resource,
                id: id.clone(),
            })?;
        inner.queue(
            fanout,
            AnyObject::Resource(resource.clone()),
            Operation::Delete,
        );
        Ok(resource)
    }

    async fn grant_for(&self, client_id: &ClientId) -> Result<Grant, StorageError> {
        let inner = self.read();
        let mut grant = Grant::empty();
        for ven in inner.vens.values().filter(|v| &v.client_id == client_id) {
            grant.extend(ven.targets.iter().cloned());
            for resource in inner.resources.values().filter(|r| r.ven_id == ven.id) {
                grant.extend(resource.targets.iter().cloned());
            }
        }
        Ok(grant)
    }

    async fn enqueue(&self, deliveries: Vec<Delivery>, now: Timestamp) -> Result<(), StorageError> {
        let mut inner = self.write();
        for delivery in deliveries {
            inner.push(delivery, now);
        }
        Ok(())
    }

    async fn claim_due(
        &self,
        now: Timestamp,
        limit: usize,
        lease: core::time::Duration,
        _owner: &str,
    ) -> Result<Vec<Queued>, StorageError> {
        let lease_until = now
            .checked_add(jiff::Span::new().seconds(lease.as_secs() as i64))
            .unwrap_or(now);

        let mut inner = self.write();
        let due: Vec<i64> = inner
            .outbox
            .iter()
            .filter(|(_, row)| {
                row.next_attempt_at.is_some_and(|at| at <= now)
                    // An unexpired lease means another dispatcher already has it.
                    && row.lease_until.is_none_or(|until| until <= now)
            })
            .map(|(id, _)| *id)
            .take(limit)
            .collect();

        let mut claimed = Vec::with_capacity(due.len());
        for id in due {
            if let Some(row) = inner.outbox.get_mut(&id) {
                row.lease_until = Some(lease_until);
                claimed.push(Queued {
                    id: OutboxId(id),
                    attempts: row.attempts,
                    delivery: row.delivery.clone(),
                });
            }
        }
        Ok(claimed)
    }

    async fn complete(&self, id: OutboxId) -> Result<(), StorageError> {
        self.write().outbox.remove(&id.0);
        Ok(())
    }

    async fn record_failure(
        &self,
        id: OutboxId,
        failure: &crate::vtn::notify::DeliveryFailure,
        now: Timestamp,
        policy: &RetryPolicy,
    ) -> Result<bool, StorageError> {
        let mut inner = self.write();
        let Some(row) = inner.outbox.get_mut(&id.0) else {
            return Ok(false);
        };
        row.attempts += 1;
        row.last_error = Some(failure.message.clone());
        row.lease_until = None;
        row.next_attempt_at = if failure.retriable {
            support::next_attempt(now, row.attempts, policy)
        } else {
            None
        };
        Ok(row.next_attempt_at.is_none())
    }

    async fn outbox_stats(&self, now: Timestamp) -> Result<OutboxStats, StorageError> {
        let inner = self.read();
        let pending = inner
            .outbox
            .values()
            .filter(|r| r.next_attempt_at.is_some())
            .count() as u64;
        let dead = inner.outbox.len() as u64 - pending;
        let oldest = inner
            .outbox
            .values()
            .filter(|r| r.next_attempt_at.is_some())
            .map(|r| r.enqueued_at)
            .min();
        Ok(OutboxStats {
            pending,
            dead,
            oldest_pending_seconds: support::age_seconds(now, oldest),
        })
    }

    async fn dead_letters(&self, limit: usize) -> Result<Vec<DeadLetter>, StorageError> {
        let inner = self.read();
        Ok(inner
            .outbox
            .iter()
            .rev()
            .filter(|(_, r)| r.next_attempt_at.is_none())
            .take(limit)
            .map(|(id, r)| DeadLetter {
                id: *id,
                enqueued_at: r.enqueued_at,
                attempts: r.attempts,
                last_error: r.last_error.clone(),
                subscription_id: r.delivery.subscription_id.clone(),
                object_type: r.delivery.notification.object.object_type(),
                object_id: r.delivery.notification.object.id().clone(),
                destination: destination(&r.delivery),
            })
            .collect())
    }

    async fn revive_dead(&self, now: Timestamp) -> Result<u64, StorageError> {
        let mut inner = self.write();
        let mut revived = 0;
        for row in inner.outbox.values_mut() {
            if row.next_attempt_at.is_none() {
                row.next_attempt_at = Some(now);
                row.attempts = 0;
                row.lease_until = None;
                revived += 1;
            }
        }
        Ok(revived)
    }

    async fn note_delivery(
        &self,
        subscription: &ObjectId,
        abandoned: bool,
        now: Timestamp,
        policy: &BreakerPolicy,
        error: Option<&str>,
    ) -> Result<bool, StorageError> {
        let mut inner = self.write();
        if !abandoned {
            // Health is the absence of a row, so a success is a removal.
            inner.health.remove(subscription);
            return Ok(false);
        }
        let entry = inner
            .health
            .entry(subscription.clone())
            .or_insert_with(|| SubscriberHealth {
                subscription_id: subscription.clone(),
                consecutive_failures: 0,
                cut_off_since: None,
                retry_at: None,
                last_error: None,
            });
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        entry.last_error = error.map(crate::std_shim::ToOwned::to_owned);
        if !policy.trips(entry.consecutive_failures) {
            return Ok(false);
        }
        let opening = entry.retry_at.is_none();
        if opening {
            entry.cut_off_since = Some(now);
        }
        entry.retry_at = now
            .checked_add(
                jiff::Span::new()
                    .try_seconds(policy.cooldown.as_secs().min(i64::MAX as u64) as i64)
                    .unwrap_or_default(),
            )
            .ok();
        Ok(opening)
    }

    async fn subscriber_health(&self) -> Result<Vec<SubscriberHealth>, StorageError> {
        let mut all: Vec<SubscriberHealth> = self.read().health.values().cloned().collect();
        all.sort_by(|a, b| {
            b.consecutive_failures
                .cmp(&a.consecutive_failures)
                .then_with(|| a.subscription_id.cmp(&b.subscription_id))
        });
        Ok(all)
    }

    async fn clear_subscriber_health(&self) -> Result<u64, StorageError> {
        let mut inner = self.write();
        let cleared = inner.health.len() as u64;
        inner.health.clear();
        Ok(cleared)
    }

    async fn purge_reports(&self, before: Timestamp, limit: usize) -> Result<u64, StorageError> {
        let mut inner = self.write();
        // Oldest first, so a bounded sweep makes progress from the far end rather than removing an
        // arbitrary `limit` and leaving the oldest rows behind for ever.
        let mut expired: Vec<(Timestamp, ObjectId)> = inner
            .reports
            .values()
            .filter(|r| r.created_date_time < before)
            .map(|r| (r.created_date_time, r.id.clone()))
            .collect();
        expired.sort();
        expired.truncate(limit);
        for (_, id) in &expired {
            inner.reports.remove(id);
        }
        Ok(expired.len() as u64)
    }

    async fn report_stats(&self, now: Timestamp) -> Result<ReportStats, StorageError> {
        let inner = self.read();
        let oldest = inner.reports.values().map(|r| r.created_date_time).min();
        Ok(ReportStats {
            count: inner.reports.len() as u64,
            oldest_seconds: super::outbox::support::age_seconds(now, oldest),
        })
    }

    async fn all_grants(&self) -> Result<Vec<(ObjectId, ClientId, Grant)>, StorageError> {
        let inner = self.read();
        let mut index = GrantIndex::new();
        for ven in inner.vens.values() {
            index.add(&ven.client_id, ven.targets.iter().cloned());
            for resource in inner.resources.values().filter(|r| r.ven_id == ven.id) {
                index.add(&ven.client_id, resource.targets.iter().cloned());
            }
        }
        Ok(inner
            .vens
            .values()
            .map(|v| (v.id.clone(), v.client_id.clone(), index.get(&v.client_id)))
            .collect())
    }
}

/// Compare two VENs ignoring the fields the VTN owns.
fn content_eq(a: &Ven, b: &Ven) -> bool {
    a.client_id == b.client_id
        && a.ven_name == b.ven_name
        && a.targets == b.targets
        && a.attributes == b.attributes
}

/// Compare two resources ignoring the fields the VTN owns.
fn resource_content_eq(a: &Resource, b: &Resource) -> bool {
    a.resource_name == b.resource_name
        && a.ven_id == b.ven_id
        && a.targets == b.targets
        && a.attributes == b.attributes
}

#[cfg(test)]
mod tests {
    use super::*;

    super::super::suite::run_suite!(async { MemoryStorage::shared() });
}
