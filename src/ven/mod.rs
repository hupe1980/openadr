//! The VEN runtime: registration, event synchronisation, a maintained timeline, and reporting.
//!
//! The [`client`](crate::client) module is the transport; this is the loop a real VEN runs on top
//! of it. Everything a VEN must do that is not business logic lives here, and nothing that *is*
//! business logic does: what the load actually does with a price and what a meter reads are the
//! user's, and they arrive through [`Meter`].
//!
//! ```no_run
//! use openadr::ven::{VenConfig, VenRuntime};
//! use openadr::client::{Client, VirtualEndNode};
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let client = Client::<VirtualEndNode>::builder("https://vtn.example.com/openadr3/3.1.0")?
//!     .bearer_token("…")
//!     .build()?;
//!
//! let ven = VenRuntime::new(client, VenConfig::new("charge-point-42".parse()?));
//! ven.register().await?;
//!
//! loop {
//!     ven.sync().await?;
//!     if let Some(segment) = ven.active_at(ven.now()) {
//!         println!("in force until {:?}: {:?}", segment.end, segment.payloads);
//!     }
//!     ven.submit_due_reports().await?;
//!     ven.wait_for_work().await;
//! }
//! # }
//! ```
//!
//! ## Why polling
//!
//! Because it is what deployments do. A VEN inside a residential appliance cannot receive an
//! inbound `POST`, an MQTT session is a connection it may not be able to hold, and every profile
//! surveyed — Dutch grid-aware charging, Belgian NetFlex, Californian price servers — polls. So the
//! loop is a poll, and it is a *conditional* one: [`VenRuntime::sync`] carries the previous `ETag`
//! and a cycle in which nothing changed costs a `304` with no body.
//!
//! Push is an optimisation on top and not a replacement: a VEN told about a change still has to
//! `GET` the object, and a VEN that hears nothing must not conclude that nothing happened. So it is
//! present, and it is present as a *hint*: [`MqttPush`] subscribes to the VEN's own topics and calls
//! [`Waker::wake`], which shortens the wait in [`VenRuntime::wait_for_work`]. The payload is never
//! read. What a broker can do to a VEN here is make it sync sooner than it needed to.
//!
//! ## What the runtime holds
//!
//! One [`Timeline`] per programme, rebuilt on every change, plus the report
//! schedule each event's descriptors imply. Both are derived — they are never the source of truth,
//! which is the VTN — so losing them costs one sync and nothing else. What is *not* derivable is
//! which reports have already been sent, and that is the one piece of state
//! [`VenRuntime::exported_state`] hands out for a VEN that persists across restarts.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};
use std::time::Duration as StdDuration;

use async_trait::async_trait;

use crate::client::{Client, ClientError, Query, VirtualEndNode};
use crate::core::{
    Clock, IntervalExpander, ReportSchedule, ScheduleOptions, Segment, SystemClock, Timeline,
};
use crate::model::{
    ClientName, Duration, Event, Interval, ObjectId, PayloadType, Program, ProgramName, Report,
    ReportPayloadDescriptor, ReportRequest, ReportResource, Resource, ResourceName,
    ResourceRequest, Target, Timestamp, Unit, Ven, VenName, VenRequest, VenResourceRequest,
    VenVenRequest,
};

#[cfg(feature = "mqtt")]
#[cfg_attr(docsrs, doc(cfg(feature = "mqtt")))]
mod push;

#[cfg(feature = "mqtt")]
#[cfg_attr(docsrs, doc(cfg(feature = "mqtt")))]
pub use push::{MqttPush, MqttPushConfig};

/// Why the runtime could not do something.
#[derive(Debug, thiserror::Error)]
pub enum VenError {
    /// The VTN refused, or could not be reached.
    #[error(transparent)]
    Client(#[from] ClientError),
    /// An operation needed a registration that has not happened.
    #[error("this VEN has not registered yet; call register() first")]
    NotRegistered,
    /// The VTN's clock and this one disagree by more than the configured tolerance.
    ///
    /// Fatal on purpose. Every interval in OpenADR is an absolute instant, so a VEN whose clock is
    /// wrong curtails at the wrong time — and does so confidently, reporting compliance it did not
    /// achieve. Fluvius' NetFlex profile requires drift within five seconds for exactly this
    /// reason.
    #[error(
        "the VTN's clock is {skew_seconds}s away from ours, which exceeds the {tolerance}s tolerance"
    )]
    ClockSkew {
        /// Measured difference, VTN minus local.
        skew_seconds: i64,
        /// The configured tolerance.
        tolerance: u64,
    },
    /// A meter could not produce the readings a due report needs.
    #[error("could not read the meter: {0}")]
    Meter(String),
    /// The VTN answered, but with something the runtime cannot act on.
    #[error("the VTN's answer cannot be used: {0}")]
    Protocol(String),
}

/// What a VEN is, and how it behaves.
#[derive(Debug, Clone)]
pub struct VenConfig {
    /// The name this VEN registers under. Unique within the VTN.
    pub ven_name: VenName,
    /// The name reports are filed under. Defaults to the VEN name.
    pub client_name: ClientName,
    /// Resources to make sure exist under this VEN.
    ///
    /// Reconciled on [`VenRuntime::register`]: missing ones are created, existing ones are left
    /// alone, and extra ones are *not* deleted — another operator may have created them, and a
    /// runtime that removes what it did not put there is a runtime nobody can share a VEN with.
    pub resources: Vec<ResourceName>,
    /// Targets to name on programme and event reads.
    ///
    /// Object privacy makes this load-bearing rather than a filter: a VEN that names no targets
    /// sees only untargeted objects `[Def §Object Privacy]`. Whatever business logic granted this
    /// VEN belongs here.
    pub targets: Vec<Target>,
    /// Only follow these programmes. Empty means every programme the VEN can see.
    pub programs: Vec<ProgramName>,
    /// How long a quiet cycle waits before polling again.
    pub poll_interval: StdDuration,
    /// How far the VTN's clock may be from this one before the runtime refuses to act.
    pub max_clock_skew: StdDuration,
    /// Seed for `randomizeStart`.
    ///
    /// The offset is derived from this and the interval's identity rather than drawn fresh, so a
    /// VEN that restarts mid-event resumes with the same offset instead of jumping. A fleet
    /// deployed from one image must give each unit a different seed, or they will all randomize
    /// identically and the randomization will have achieved nothing.
    pub randomization_seed: u64,
    /// The reading of the ambiguous corners of §7.5.
    pub schedule: ScheduleOptions,
    /// How far back a cycle looks for reports it owes.
    ///
    /// A report stays on offer until it is filed, so this only has to cover downtime: a VEN that
    /// was away for an hour still owes the reports that fell due while it was. It is not a licence
    /// to refile — [`VenState::reported`] is what stops that, and persisting it across a restart is
    /// what stops a restarted VEN sending a day of duplicates into somebody's settlement data.
    pub report_catchup: StdDuration,
}

impl VenConfig {
    /// A configuration for a VEN registering under a name.
    pub fn new(ven_name: VenName) -> Self {
        let client_name =
            ClientName::new(ven_name.as_str()).expect("a VEN name is a valid client name");
        Self {
            ven_name,
            client_name,
            resources: Vec::new(),
            targets: Vec::new(),
            programs: Vec::new(),
            poll_interval: StdDuration::from_secs(60),
            max_clock_skew: StdDuration::from_secs(30),
            randomization_seed: 0,
            schedule: ScheduleOptions::default(),
            report_catchup: StdDuration::from_secs(24 * 60 * 60),
        }
    }

    /// Register these resources.
    pub fn with_resources(mut self, names: impl IntoIterator<Item = ResourceName>) -> Self {
        self.resources = names.into_iter().collect();
        self
    }

    /// Name these targets when reading programmes and events.
    pub fn with_targets(mut self, targets: impl IntoIterator<Item = Target>) -> Self {
        self.targets = targets.into_iter().collect();
        self
    }

    /// Follow only these programmes.
    pub fn with_programs(mut self, programs: impl IntoIterator<Item = ProgramName>) -> Self {
        self.programs = programs.into_iter().collect();
        self
    }

    /// Set the idle poll interval.
    pub fn with_poll_interval(mut self, interval: StdDuration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Set how far the VTN's clock may be from this one.
    pub fn with_max_clock_skew(mut self, tolerance: StdDuration) -> Self {
        self.max_clock_skew = tolerance;
        self
    }

    /// Set the randomization seed. Give every unit in a fleet a different one.
    pub fn with_randomization_seed(mut self, seed: u64) -> Self {
        self.randomization_seed = seed;
        self
    }
}

/// One report the VTN has asked for and the VEN now owes.
///
/// Everything a meter needs to answer, and nothing about how to answer it.
#[derive(Debug, Clone, PartialEq)]
pub struct DueReport {
    /// The event whose `reportDescriptor` asked for this.
    pub event_id: ObjectId,
    /// The programme that event belongs to.
    pub program_id: ObjectId,
    /// Which descriptor on that event, by position.
    pub descriptor: usize,
    /// Which report in that descriptor's sequence, counted from the event's first interval.
    pub sequence: u64,
    /// What to measure.
    pub payload_type: PayloadType,
    /// How to measure it, if the descriptor said.
    pub reading_type: Option<crate::model::ReadingType>,
    /// The unit, if the descriptor said.
    pub units: Option<Unit>,
    /// Report one aggregated series rather than one per resource `[UG §7.7]`.
    ///
    /// A [`Meter`] may ignore this and return one series per resource: the runtime sums them and
    /// names the result `AGGREGATED_REPORT`. Return a single series already carrying that name to
    /// aggregate it yourself, which a deployment must whenever the sum is not a sum of the numbers
    /// the VEN holds.
    pub aggregate: bool,
    /// The interval ids to quote, empty when the VEN chooses its own.
    pub interval_ids: Vec<i32>,
    /// Start of the covered range.
    pub covers_from: Timestamp,
    /// End of the covered range, if bounded.
    pub covers_to: Option<Timestamp>,
}

impl DueReport {
    /// The key under which this report is remembered as sent.
    fn key(&self) -> (ObjectId, usize, u64) {
        (self.event_id.clone(), self.descriptor, self.sequence)
    }
}

/// Where a report's numbers come from.
///
/// The one thing the runtime cannot supply: what a meter read is business logic, and every
/// deployment's is different. Implement this and the runtime handles the rest — when a report is
/// due, which intervals it covers, and not sending the same one twice.
#[async_trait]
pub trait Meter: Send + Sync + 'static {
    /// Produce the resource series for one due report.
    ///
    /// Returning an empty vector skips the report *without* marking it sent, so a meter that is
    /// briefly unavailable gets asked again on the next cycle rather than losing the window.
    async fn read(&self, due: &DueReport) -> Result<Vec<ReportResource>, VenError>;
}

/// A meter with nothing to say.
///
/// The default, and the right one for a VEN that only follows prices: it reports nothing, which is
/// what a `reportDescriptor`-free programme asks for anyway.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoMeter;

#[async_trait]
impl Meter for NoMeter {
    async fn read(&self, _due: &DueReport) -> Result<Vec<ReportResource>, VenError> {
        Ok(Vec::new())
    }
}

/// What one [`VenRuntime::sync`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncOutcome {
    /// Whether anything moved at all. `false` is the `304` case.
    pub changed: bool,
    /// Events that appeared.
    pub added: Vec<ObjectId>,
    /// Events whose `modificationDateTime` advanced.
    pub updated: Vec<ObjectId>,
    /// Events that are gone — cancelled, in OpenADR's vocabulary.
    pub removed: Vec<ObjectId>,
}

impl SyncOutcome {
    /// Whether the schedule this VEN is acting on has changed.
    pub fn is_quiet(&self) -> bool {
        !self.changed
    }
}

/// The part of a VEN's state that a restart would otherwise lose.
///
/// Everything else — the events, the timeline, the schedule — is re-derived from the VTN on the
/// next sync and is not worth persisting. This is not: a report already filed and filed again is a
/// duplicate in the utility's settlement data.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VenState {
    /// The VEN object's id, once registered.
    pub ven_id: Option<ObjectId>,
    /// Reports already submitted, as `eventID:descriptor:sequence`.
    pub reported: BTreeSet<String>,
}

/// The runtime.
#[derive(Debug)]
pub struct VenRuntime<M = NoMeter> {
    client: Client<VirtualEndNode>,
    config: VenConfig,
    clock: Arc<dyn Clock>,
    meter: M,
    inner: RwLock<Inner>,
    /// Shortens the next wait when something tells the VEN not to bother sleeping.
    ///
    /// `Notify` and not a channel because the signal carries nothing and needs no queue: the
    /// question a hint answers is "is it worth syncing now", and two hints are the same hint. It
    /// does hold one permit, so a hint that arrives *during* a sync is not lost — which matters,
    /// because that is exactly when a notification about the write you are reading turns up.
    wake: Arc<tokio::sync::Notify>,
}

#[derive(Debug, Default)]
struct Inner {
    ven_id: Option<ObjectId>,
    resources: BTreeMap<ResourceName, ObjectId>,
    events_etag: Option<String>,
    events: BTreeMap<ObjectId, Event>,
    /// The instant each event's `0001-01-01` sentinel resolves to: when this version of it was
    /// first read. See [`Timeline::build_with`] — re-resolving it on a rebuild slides a "do it
    /// now" event forward for ever.
    anchors: BTreeMap<ObjectId, Timestamp>,
    programs: BTreeMap<ObjectId, Program>,
    timelines: BTreeMap<ObjectId, Timeline>,
    /// When the timelines were last built, which is what makes them expire.
    built_at: Option<Timestamp>,
    reported: BTreeSet<(ObjectId, usize, u64)>,
}

impl Inner {
    /// The expander to resolve one event against: anchored where it was first read.
    fn expander(&self, event_id: &ObjectId, fallback: Timestamp) -> IntervalExpander {
        IntervalExpander::at(self.anchors.get(event_id).copied().unwrap_or(fallback))
    }

    /// Whether the timelines no longer reach far enough ahead to act on.
    ///
    /// They are built over a fixed horizon from the instant of the last build, so a schedule that
    /// simply never changes — a tariff looping for ever is the ordinary case, and a VTN answers
    /// every poll for one with a `304` — would otherwise run off the end of that horizon and leave
    /// the VEN with nothing in force, silently.
    fn timelines_are_stale(&self, now: Timestamp) -> bool {
        match self.built_at {
            None => !self.events.is_empty(),
            Some(built) => now < built || now.as_second() - built.as_second() >= TIMELINE_REFRESH,
        }
    }

    /// Rebuild every programme's timeline against the current anchors.
    fn rebuild_timelines(&mut self, now: Timestamp, poll_interval: StdDuration) {
        self.timelines = build_timelines(&self.events, &self.anchors, now, poll_interval);
        self.built_at = Some(now);
    }
}

/// Shortens a VEN's next wait.
///
/// The whole of the push story in this crate, deliberately. A notification says *something
/// changed*, and a VEN told that still has to `GET` the object — so the useful content of a push
/// message is its arrival, not its body. Acting on the body would mean trusting a broker, or a
/// callback endpoint, with a dispatch instruction that the VTN is the only authority on; waking and
/// re-reading costs one conditional `GET`, which is a `304` when the hint was spurious.
///
/// That also makes a push source unable to do harm: the worst a compromised or misconfigured one
/// achieves is a VEN that polls more often than it needs to.
///
/// Cheap to clone, and safe to call from anywhere.
#[derive(Clone, Debug)]
pub struct Waker(Arc<tokio::sync::Notify>);

impl Waker {
    /// Cut the current wait short.
    ///
    /// One permit is stored if nobody is waiting, so a hint that arrives mid-sync is honoured on
    /// the next wait rather than lost. Further hints before that wait collapse into it, which is
    /// right: two "something changed" signals are one sync.
    pub fn wake(&self) {
        self.0.notify_one();
    }
}

impl VenRuntime<NoMeter> {
    /// A runtime that follows a schedule and files no reports.
    pub fn new(client: Client<VirtualEndNode>, config: VenConfig) -> Self {
        Self::with_meter(client, config, NoMeter)
    }
}

impl<M: Meter> VenRuntime<M> {
    /// A runtime that reports what a meter reads.
    pub fn with_meter(client: Client<VirtualEndNode>, config: VenConfig, meter: M) -> Self {
        Self {
            client,
            config,
            clock: Arc::new(SystemClock),
            meter,
            inner: RwLock::new(Inner::default()),
            wake: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Override the clock, for deterministic tests.
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Restore the state a previous process exported.
    pub fn restore(&self, state: VenState) {
        let mut inner = self.write();
        inner.ven_id = state.ven_id;
        inner.reported = state
            .reported
            .iter()
            .filter_map(|key| {
                let mut parts = key.rsplitn(3, ':');
                let sequence = parts.next()?.parse().ok()?;
                let descriptor = parts.next()?.parse().ok()?;
                let event = ObjectId::new(parts.next()?).ok()?;
                Some((event, descriptor, sequence))
            })
            .collect();
    }

    /// The state worth persisting.
    pub fn exported_state(&self) -> VenState {
        let inner = self.read();
        VenState {
            ven_id: inner.ven_id.clone(),
            reported: inner
                .reported
                .iter()
                .map(|(event, descriptor, sequence)| format!("{event}:{descriptor}:{sequence}"))
                .collect(),
        }
    }

    /// This VEN's own clock.
    pub fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// The registered VEN object's id, once [`VenRuntime::register`] has run.
    /// The client this runtime talks to.
    ///
    /// For wiring something alongside the loop — a push subscriber, a second reader — onto the same
    /// credentials and base URL rather than a second configuration of them.
    pub fn client(&self) -> &Client<VirtualEndNode> {
        &self.client
    }

    /// The VEN object's id, once registered.
    pub fn ven_id(&self) -> Option<ObjectId> {
        self.read().ven_id.clone()
    }

    // -- registration ------------------------------------------------------

    /// Make sure this VEN and its resources exist at the VTN.
    ///
    /// Idempotent, and deliberately so: a VEN restarts, and re-registering must not create a second
    /// object or fail because the first one exists. The VEN object is found by name — the
    /// specification requires `venName` to be unique within a VTN — and created only if absent.
    ///
    /// Targets are never written here. Only business logic may grant them `[Def §Object Privacy]`,
    /// and `VEN_VEN_REQUEST` has no `targets` member to write them with, so a VEN that tried could
    /// not express it.
    pub async fn register(&self) -> Result<Ven, VenError> {
        self.check_clock().await?;

        let existing = self
            .client
            .vens()
            .list_with(&Query::new().ven_name(&self.config.ven_name))
            .await?
            .into_iter()
            .find(|v: &Ven| v.ven_name == self.config.ven_name);

        let ven = match existing {
            Some(ven) => ven,
            None => {
                self.client
                    .vens()
                    .create(&VenRequest::Ven(VenVenRequest {
                        ven_name: self.config.ven_name.clone(),
                        attributes: None,
                    }))
                    .await?
            }
        };

        self.write().ven_id = Some(ven.id.clone());
        self.reconcile_resources(&ven.id).await?;
        Ok(ven)
    }

    /// Re-run resource reconciliation without re-reading the VEN object.
    ///
    /// For a VEN whose set of resources changes while it runs — a gateway that has just discovered
    /// another appliance behind it.
    pub async fn sync_resources(&self) -> Result<(), VenError> {
        let ven_id = self.read().ven_id.clone().ok_or(VenError::NotRegistered)?;
        self.reconcile_resources(&ven_id).await
    }

    /// The ids of the resources this runtime knows about, by name.
    pub fn resources(&self) -> BTreeMap<ResourceName, ObjectId> {
        self.read().resources.clone()
    }

    /// Create the configured resources that do not exist yet.
    async fn reconcile_resources(&self, ven_id: &ObjectId) -> Result<(), VenError> {
        if self.config.resources.is_empty() {
            return Ok(());
        }
        let existing: Vec<Resource> = self
            .client
            .resources()
            .list_all(&Query::new().ven(ven_id))
            .await?;
        let mut by_name: BTreeMap<ResourceName, ObjectId> = existing
            .iter()
            .map(|r| (r.resource_name.clone(), r.id.clone()))
            .collect();

        for name in &self.config.resources {
            if by_name.contains_key(name) {
                continue;
            }
            let created = self
                .client
                .resources()
                .create(&ResourceRequest::Ven(VenResourceRequest {
                    resource_name: name.clone(),
                    // 3.1.0 wanted the parent id in the body and 3.1.1 removed it. Sending it is
                    // accepted by both; omitting it is refused by the first.
                    ven_id: Some(ven_id.clone()),
                    attributes: None,
                }))
                .await?;
            by_name.insert(name.clone(), created.id);
        }
        self.write().resources = by_name;
        Ok(())
    }

    // -- synchronisation ---------------------------------------------------

    /// Re-read the schedule, conditionally.
    ///
    /// One `GET /events` carrying the previous `ETag`. A cycle in which nothing changed is a `304`
    /// with no body, which is the whole reason a VEN can poll a 48-hour window every minute without
    /// anybody minding.
    ///
    /// Rebuilds the timelines when — and only when — something moved.
    pub async fn sync(&self) -> Result<SyncOutcome, VenError> {
        let etag = self.read().events_etag.clone();
        let query = self.event_query();

        let Some(page) = self
            .client
            .events()
            .list_if_changed(&query, etag.as_deref())
            .await?
        else {
            // Nothing at the VTN moved. The timelines still age: they reach a fixed distance ahead
            // of the instant they were built at, and a schedule that never changes answers every
            // poll with a `304` for as long as it runs.
            let now = self.clock.now();
            let mut inner = self.write();
            if inner.timelines_are_stale(now) {
                inner.rebuild_timelines(now, self.config.poll_interval);
            }
            return Ok(SyncOutcome::default());
        };

        // `list_if_changed` reads one page. A VEN following more events than fit in one needs the
        // whole set, and the tag is only meaningful for the page it came from — so the tag is kept
        // when one page was enough and dropped when it was not.
        //
        // "Was that page full?" is only answerable because [`VenRuntime::event_query`] *named* the
        // limit. The schema caps `limit` and gives it no default `[API /events limit]`, so a VTN
        // sent none may answer with a page of any size it likes; comparing an unrequested page's
        // length against this crate's own default would read a peer that pages at twenty as a VTN
        // with twenty events, and the VEN would follow a truncated schedule with nothing anywhere
        // reporting an error.
        let full_page = page.value.len() >= crate::model::MAX_PAGE_LIMIT;
        let events: Vec<Event> = if full_page {
            self.client.events().list_all(&query).await?
        } else {
            page.value
        };

        let mut inner = self.write();
        inner.events_etag = if full_page { None } else { page.etag };

        let incoming: BTreeMap<ObjectId, Event> =
            events.into_iter().map(|e| (e.id.clone(), e)).collect();
        let mut outcome = SyncOutcome::default();
        for (id, event) in &incoming {
            match inner.events.get(id) {
                None => outcome.added.push(id.clone()),
                Some(existing)
                    if existing.modification_date_time != event.modification_date_time =>
                {
                    outcome.updated.push(id.clone())
                }
                Some(_) => {}
            }
        }
        for id in inner.events.keys() {
            if !incoming.contains_key(id) {
                outcome.removed.push(id.clone());
            }
        }
        outcome.changed =
            !(outcome.added.is_empty() && outcome.updated.is_empty() && outcome.removed.is_empty());

        let now = self.clock.now();
        if outcome.changed {
            // Anchor every event this cycle brought or changed. An event whose timing is unchanged
            // keeps the instant it was first read, so its `0001-01-01` start stays where it was.
            for id in outcome.added.iter().chain(&outcome.updated) {
                inner.anchors.insert(id.clone(), now);
            }
            inner.events = incoming;
            let live: BTreeSet<ObjectId> = inner.events.keys().cloned().collect();
            inner.anchors.retain(|id, _| live.contains(id));
            inner.rebuild_timelines(now, self.config.poll_interval);
            // A cancelled event's reports are moot; forgetting them keeps the persisted state from
            // growing for the lifetime of the VEN.
            for id in &outcome.removed {
                inner.reported.retain(|(event, _, _)| event != id);
            }
        } else if inner.timelines_are_stale(now) {
            inner.rebuild_timelines(now, self.config.poll_interval);
        }
        Ok(outcome)
    }

    /// Re-read the programmes this VEN follows.
    ///
    /// Separate from [`VenRuntime::sync`] because programmes change far less often than events do,
    /// and a VEN polling its schedule every minute has no reason to re-read the tariff with it.
    pub async fn sync_programs(&self) -> Result<Vec<Program>, VenError> {
        let mut query = Query::new().targets(&self.config.targets);
        if let [only] = self.config.programs.as_slice() {
            query = query.program_name(only);
        }
        let programs: Vec<Program> = self.client.programs().list_all(&query).await?;
        let wanted: Vec<&ProgramName> = self.config.programs.iter().collect();
        let programs: Vec<Program> = programs
            .into_iter()
            .filter(|p| wanted.is_empty() || wanted.contains(&&p.content.program_name))
            .collect();
        self.write().programs = programs.iter().map(|p| (p.id.clone(), p.clone())).collect();
        Ok(programs)
    }

    fn event_query(&self) -> Query {
        // The limit is named rather than left to the VTN. See `sync`: the page's length is only
        // evidence about the collection if the reader chose the page size.
        let mut query = Query::new()
            .targets(&self.config.targets)
            .limit(crate::model::MAX_PAGE_LIMIT);
        // Events whose intervals have all elapsed are not worth carrying: the VTN can drop them
        // inside the query, which is cheaper than transferring them to be ignored here.
        query = query.active(true);
        if self.config.programs.len() == 1
            && let Some((id, _)) = self
                .read()
                .programs
                .iter()
                .find(|(_, p)| Some(&p.content.program_name) == self.config.programs.first())
        {
            query = query.program(id);
        }
        query
    }

    // -- the schedule ------------------------------------------------------

    /// The events currently held, as last synchronised.
    pub fn events(&self) -> Vec<Event> {
        self.read().events.values().cloned().collect()
    }

    /// The conflict-resolved timeline for one programme.
    pub fn timeline(&self, program_id: &ObjectId) -> Option<Timeline> {
        self.read().timelines.get(program_id).cloned()
    }

    /// What is in force at an instant, across every programme.
    ///
    /// Priority resolves overlaps *within* a programme, which is what the specification defines. A
    /// VEN enrolled in two programmes at once holds two schedules and the choice between them is
    /// its own; this returns the highest-priority segment across both, and
    /// [`VenRuntime::active_segments`] returns them all for a VEN that would rather decide.
    pub fn active_at(&self, at: Timestamp) -> Option<Segment> {
        self.active_segments(at).into_iter().min_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then_with(|| a.event_id.as_str().cmp(b.event_id.as_str()))
        })
    }

    /// Every programme's segment in force at an instant.
    pub fn active_segments(&self, at: Timestamp) -> Vec<Segment> {
        let inner = self.read();
        inner
            .timelines
            .values()
            .filter_map(|t| t.at(at).cloned())
            .map(|segment| self.randomized(segment))
            .collect()
    }

    /// When the schedule next changes, across every programme.
    pub fn next_change(&self, after: Timestamp) -> Option<Timestamp> {
        let inner = self.read();
        inner
            .timelines
            .values()
            .filter_map(|t| t.next_change(after))
            .min()
    }

    /// How long to sleep: until the schedule moves, or until the next poll, whichever is sooner.
    ///
    /// Bounded by the poll interval rather than by the next change alone, because a VEN that slept
    /// until its own next transition would never learn about an event created in the meantime.
    ///
    /// Measured to the nanosecond, not to the second. Truncating both instants to whole seconds
    /// before subtracting gets the answer wrong in both directions and neither is benign: a
    /// transition 1 ms away rounds to a wait of zero, and the loop then spins through real HTTP
    /// syncs until the second turns over; a transition 999 ms away also rounds to a whole second,
    /// and the VEN curtails a second late. Both are invisible in a test whose fixtures land on
    /// whole seconds, which is every fixture in this crate (D-122).
    pub fn time_to_next_wakeup(&self) -> StdDuration {
        let now = self.clock.now();
        let until_change = self.next_change(now).map(|t| {
            let nanos = (t.as_nanosecond() - now.as_nanosecond()).max(0);
            StdDuration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
        });
        match until_change {
            Some(d) => d.min(self.config.poll_interval),
            None => self.config.poll_interval,
        }
    }

    /// Sleep until there is something to do.
    ///
    /// [`VenRuntime::time_to_next_wakeup`] says how long that is *if nothing interrupts*; this is
    /// that sleep raced against a [`Waker`]. It is what a loop should await, and a loop that awaits
    /// the bare sleep instead simply never notices a hint.
    ///
    /// A hint is not information. It shortens a wait that would have happened anyway, so a VEN with
    /// no push source behaves exactly as it did, and one whose push source is wrong, silent or
    /// hostile still syncs on its poll interval and still learns the truth from the VTN.
    pub async fn wait_for_work(&self) {
        let delay = self.time_to_next_wakeup();
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = self.wake.notified() => {}
        }
    }

    /// A handle that shortens the next [`VenRuntime::wait_for_work`].
    ///
    /// Hand one to anything that learns about a change sooner than the poll interval would — an
    /// MQTT subscriber ([`MqttPush`]), a webhook endpoint of your own, a button on the front panel.
    pub fn waker(&self) -> Waker {
        Waker(self.wake.clone())
    }

    /// Apply `randomizeStart` to a segment.
    ///
    /// Derived from the seed and the segment's identity, never drawn: a VEN that restarted
    /// mid-event and re-randomized would jump, which is worse than not randomizing at all. The
    /// specification puts this on the VEN — a VTN that applied it would defeat the purpose, which
    /// is that a fleet does not all switch on the same second.
    fn randomized(&self, mut segment: Segment) -> Segment {
        let Some(window) = segment.randomize_start.as_ref().and_then(Duration::as_secs) else {
            return segment;
        };
        if window <= 0 {
            return segment;
        }
        let offset = deterministic_offset(
            self.config.randomization_seed,
            segment.event_id.as_str(),
            segment.interval_id,
            window,
        );
        let shift = jiff::Span::new().seconds(offset);
        if let Ok(start) = segment.start.checked_add(shift) {
            segment.start = start;
        }
        segment
    }

    // -- reporting ---------------------------------------------------------

    /// The reports that are due now and have not been filed.
    ///
    /// Pure: it reads the synchronised events and the runtime's memory of what it has sent, and
    /// touches neither the network nor that memory. [`VenRuntime::submit_due_reports`] is the one
    /// that acts.
    pub fn due_reports(&self, at: Timestamp) -> Vec<DueReport> {
        let inner = self.read();
        let mut due = Vec::new();

        // How far back to look. A report is offered again on every cycle until it is filed, so the
        // window only has to cover downtime: a VEN that comes back after an outage still owes the
        // reports that fell due while it was away.
        let from = at
            .checked_sub(
                jiff::Span::new()
                    .seconds(self.config.report_catchup.as_secs().min(i64::MAX as u64) as i64),
            )
            .unwrap_or(Timestamp::MIN);

        for event in inner.events.values() {
            let Some(descriptors) = event.content.report_descriptors.as_ref() else {
                continue;
            };
            // The event's own anchor, for the same reason the timeline uses one: a report schedule
            // computed against a sliding `0001-01-01` would owe its first report again on every
            // cycle, for ever.
            let expander = inner.expander(&event.id, at);
            let Ok(sequence) = expander.sequence(&event.content) else {
                // An event that cannot be placed in time cannot be reported on either, and one
                // malformed event must not stop the others being reported.
                continue;
            };
            for (index, descriptor) in descriptors.iter().enumerate() {
                let schedule = ReportSchedule::compute_with(
                    descriptor,
                    &sequence,
                    from,
                    at,
                    self.config.schedule,
                );
                for report in schedule.overdue(at, false) {
                    let entry = DueReport {
                        event_id: event.id.clone(),
                        program_id: event.content.program_id.clone(),
                        descriptor: index,
                        sequence: report.sequence,
                        payload_type: descriptor.payload_type.clone(),
                        reading_type: descriptor.reading_type.clone(),
                        units: descriptor.units.clone(),
                        aggregate: descriptor.aggregate,
                        interval_ids: report.interval_ids.clone(),
                        covers_from: report.covers_from,
                        covers_to: report.covers_to,
                    };
                    if !inner.reported.contains(&entry.key()) {
                        due.push(entry);
                    }
                }
            }
        }
        due
    }

    /// File every due report the meter can answer.
    ///
    /// A report the meter declines to fill in is left due rather than marked sent, so a meter that
    /// was briefly unavailable gets asked again next cycle instead of losing the window. A report
    /// the VTN accepts is remembered, because filing one twice is a duplicate in somebody's
    /// settlement data.
    pub async fn submit_due_reports(&self) -> Result<Vec<Report>, VenError> {
        let now = self.clock.now();
        let mut filed = Vec::new();
        for due in self.due_reports(now) {
            let resources = self.meter.read(&due).await?;
            if resources.is_empty() {
                continue;
            }
            // `[UG §7.7]`: a descriptor asking to aggregate is asking for **one** series named
            // `AGGREGATED_REPORT`, and "aggregation means the data from a set of resources are
            // summed". The arithmetic is the specification's, so it is the runtime's; what the
            // meter owes is the readings. A meter that has already aggregated — because a sum
            // across resources is not always a sum of the numbers a VEN can see — passes through
            // untouched.
            let resources = if due.aggregate {
                crate::core::aggregate(resources).map_err(|e| VenError::Meter(e.to_string()))?
            } else {
                resources
            };
            let mut request = ReportRequest::new(
                due.event_id.clone(),
                self.config.client_name.clone(),
                resources,
            );
            request.report_name = Some(format!(
                "{}-{}-{}",
                due.payload_type.as_str(),
                due.descriptor,
                due.sequence
            ));
            // `[UG §7.6]`: "the values in payload with type of PRICE are simply numbers, and an
            // accompanying payloadDescriptor supplies the units … necessary to fully interpret"
            // them — and "reports contain payloadDescriptors".
            //
            // The runtime has all three parts already: the requesting `reportDescriptor` named the
            // payload type, and optionally the reading type and the unit, and `DueReport` carries
            // them. Filing without one sends a series of bare numbers whose unit is a guess, which
            // is a thing this project's own conformance suite fails a peer for.
            let mut descriptor = ReportPayloadDescriptor::new(due.payload_type.clone());
            descriptor.reading_type = due.reading_type.clone();
            descriptor.units = due.units.clone();
            request.payload_descriptors = Some(vec![descriptor]);
            let report = self.client.reports().create(&request).await?;
            self.write().reported.insert(due.key());
            filed.push(report);
        }
        Ok(filed)
    }

    /// Build the interval series a meter usually wants: one per covered interval, values supplied.
    ///
    /// A convenience rather than a rule — a meter is free to build its own — but it gets the timing
    /// right, which is the part that is easy to get wrong: report intervals quote the *event's*
    /// interval ids so the VTN can correlate them `[UG §7.5]`.
    pub fn intervals_for(
        &self,
        due: &DueReport,
        mut values: impl FnMut(i32) -> Vec<crate::model::Value>,
    ) -> Vec<Interval> {
        due.interval_ids
            .iter()
            .map(|id| {
                Interval::new(
                    *id,
                    crate::std_shim::vec![crate::model::ValuesMap::new(
                        due.payload_type.clone(),
                        values(*id),
                    )],
                )
            })
            .collect()
    }

    // -- the clock ---------------------------------------------------------

    /// Compare this VEN's clock with the VTN's.
    ///
    /// Every interval in OpenADR is an absolute instant, so a VEN whose clock is wrong acts at the
    /// wrong time — and reports compliance it did not achieve, confidently. The VTN's `Date` header
    /// is the only reference available over the protocol, and it is enough: the question is whether
    /// the two are seconds apart, not milliseconds.
    pub async fn check_clock(&self) -> Result<i64, VenError> {
        let Some(server) = self.client.server_time().await? else {
            // A VTN behind a proxy that strips `Date` cannot be checked, and refusing to run
            // against one would be refusing to run against a correct deployment.
            return Ok(0);
        };
        let skew = server.as_second() - self.clock.now().as_second();
        let tolerance = self.config.max_clock_skew.as_secs();
        if skew.unsigned_abs() > tolerance {
            return Err(VenError::ClockSkew {
                skew_seconds: skew,
                tolerance,
            });
        }
        Ok(skew)
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }
}

/// How far ahead a timeline is built, in hours.
///
/// Far enough to schedule against, not so far that a repeating event expands for ever.
const TIMELINE_HORIZON_HOURS: i64 = 48;

/// How much of that horizon may elapse before the timelines are rebuilt, in seconds.
///
/// Half of it, so a timeline always reaches at least a day ahead of the instant it is read at.
/// The rebuild is arithmetic over the events already held — no request, no allocation per poll —
/// so the only reason not to do it every cycle is that there is no reason to.
const TIMELINE_REFRESH: i64 = TIMELINE_HORIZON_HOURS * 3600 / 2;

/// One timeline per programme, over a window that reaches past the next poll.
///
/// The window matters: a timeline built only over "now" has no `next_change` to sleep until, and a
/// VEN that slept on that would wake only when its poll interval said so — turning a 15-minute
/// curtailment that starts in 20 seconds into one that starts a minute late.
///
/// `anchors` says what `0001-01-01` means for each event: the instant this VEN first read it, not
/// the instant of this rebuild.
fn build_timelines(
    events: &BTreeMap<ObjectId, Event>,
    anchors: &BTreeMap<ObjectId, Timestamp>,
    now: Timestamp,
    poll_interval: StdDuration,
) -> BTreeMap<ObjectId, Timeline> {
    let horizon = now
        .checked_add(jiff::Span::new().hours(TIMELINE_HORIZON_HOURS))
        .unwrap_or(Timestamp::MAX);
    // Back to the start of the current interval, so an event already running is still in the
    // timeline rather than having been missed by a window that began after it did.
    let from = now
        .checked_sub(jiff::Span::new().seconds(poll_interval.as_secs().max(1) as i64))
        .unwrap_or(now);

    let mut by_program: BTreeMap<ObjectId, Vec<&Event>> = BTreeMap::new();
    for event in events.values() {
        by_program
            .entry(event.content.program_id.clone())
            .or_default()
            .push(event);
    }
    by_program
        .into_iter()
        .map(|(program_id, events)| {
            let timeline = Timeline::build_with(
                events.iter().map(|e| {
                    let anchor = anchors.get(&e.id).copied().unwrap_or(now);
                    (&e.id, &e.content, IntervalExpander::at(anchor))
                }),
                from,
                horizon,
            );
            (program_id, timeline)
        })
        .collect()
}

/// A stable offset in `[-window, +window]` for one interval of one event.
///
/// Deterministic on purpose — see [`VenRuntime::randomized`]. FNV-1a because it is four lines and
/// the requirement is "spread out", not "unpredictable": an adversary who can guess a charge
/// point's start offset has learned nothing worth having.
fn deterministic_offset(seed: u64, event_id: &str, interval_id: i32, window: i64) -> i64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325 ^ seed;
    for byte in event_id.as_bytes().iter().chain(&interval_id.to_le_bytes()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    let span = window.saturating_mul(2).saturating_add(1).max(1);
    (hash % span as u64) as i64 - window
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runtime's futures must be `Send`: a real VEN spawns its loop.
    ///
    /// Worth asserting rather than assuming. The state is behind a `std::sync::RwLock`, and a guard
    /// held across a single `await` anywhere in this module would make the whole future `!Send` —
    /// at which point `tokio::spawn` stops compiling for every user, in their code rather than in
    /// this file.
    #[allow(dead_code)]
    fn futures_are_send() {
        fn assert_send<T: Send>(_: T) {}
        fn check(runtime: VenRuntime) {
            assert_send(runtime.sync());
            assert_send(runtime.register());
            assert_send(runtime.submit_due_reports());
            assert_send(runtime.sync_programs());
            assert_send(runtime.check_clock());
        }
        let _ = check;
    }

    #[test]
    fn a_randomized_offset_is_stable_and_inside_its_window() {
        // Stability is the property that matters. A VEN that restarts mid-event and draws a fresh
        // offset jumps, which is worse for the grid than not randomizing at all.
        for interval in 0..64 {
            let a = deterministic_offset(7, "evt-1", interval, 300);
            let b = deterministic_offset(7, "evt-1", interval, 300);
            assert_eq!(a, b, "the same input gave two offsets");
            assert!((-300..=300).contains(&a), "{a} is outside the window");
        }
    }

    #[test]
    fn different_seeds_spread_a_fleet() {
        // A fleet deployed from one image with one seed randomizes identically, which is a fleet
        // that has not randomized. The seed is what makes each unit different.
        let offsets: BTreeSet<i64> = (0..32)
            .map(|seed| deterministic_offset(seed, "evt-1", 0, 600))
            .collect();
        assert!(
            offsets.len() > 20,
            "32 seeds produced only {} distinct offsets",
            offsets.len()
        );
    }

    #[test]
    fn state_survives_a_round_trip_through_its_serialised_form() {
        let state = VenState {
            ven_id: Some("ven-1".parse().unwrap()),
            reported: ["evt-1:0:3".to_string(), "evt-2:1:0".to_string()]
                .into_iter()
                .collect(),
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: VenState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn timelines_are_built_per_programme() {
        use crate::model::{EventRequest, IntervalPeriod, ObjectType, StartTime, Value, ValuesMap};
        let now: Timestamp = "2026-02-11T06:00:00Z".parse().unwrap();
        let make = |id: &str, program: &str| {
            let mut content = EventRequest::new(program.parse().unwrap());
            content.interval_period = Some(IntervalPeriod::new(
                StartTime::At(now),
                "PT1H".parse().unwrap(),
            ));
            content.intervals = Some(crate::std_shim::vec![Interval::new(
                0,
                crate::std_shim::vec![ValuesMap::single(
                    "PRICE".parse().unwrap(),
                    Value::Integer(1)
                )],
            )]);
            Event {
                id: id.parse().unwrap(),
                created_date_time: now,
                modification_date_time: now,
                object_type: ObjectType::Event,
                content,
            }
        };
        let events: BTreeMap<ObjectId, Event> = [make("evt-1", "prg-1"), make("evt-2", "prg-2")]
            .into_iter()
            .map(|e| (e.id.clone(), e))
            .collect();

        let timelines = build_timelines(&events, &BTreeMap::new(), now, StdDuration::from_secs(60));
        assert_eq!(timelines.len(), 2, "one timeline per programme");
        for timeline in timelines.values() {
            assert_eq!(timeline.segments().len(), 1);
        }
    }

    // -- the wake-up ------------------------------------------------------

    fn runtime() -> VenRuntime {
        let client = Client::<VirtualEndNode>::builder("http://vtn.test/openadr3/3.1.0")
            .unwrap()
            .bearer_token("t")
            .build()
            .unwrap();
        VenRuntime::new(client, VenConfig::new("ven-1".parse().unwrap()))
    }

    /// A VEN wakes *at* the transition, not at the second boundary nearest it.
    ///
    /// Every fixture in this crate lands on a whole second, which is exactly why the arithmetic
    /// went unexamined: truncating both instants before subtracting is invisible until an event
    /// does not. It is wrong in both directions and neither is benign — a transition 1 ms away
    /// rounds to a wait of zero and the loop spins through real syncs until the second turns over,
    /// and one 999 ms away rounds to a full second late (D-122).
    #[test]
    fn the_wait_is_measured_to_the_transition_not_to_the_second() {
        use crate::core::FixedClock;
        use crate::model::{EventRequest, IntervalPeriod, ObjectType, StartTime, Value, ValuesMap};

        // `now` and the transition are inside the same second, in that order.
        let now: Timestamp = "2026-02-11T06:00:00.100Z".parse().unwrap();
        let starts_at: Timestamp = "2026-02-11T06:00:00.900Z".parse().unwrap();

        let mut content = EventRequest::new("prg-1".parse().unwrap());
        content.interval_period = Some(IntervalPeriod::new(
            StartTime::At(starts_at),
            "PT1H".parse().unwrap(),
        ));
        content.intervals = Some(crate::std_shim::vec![Interval::new(
            0,
            crate::std_shim::vec![ValuesMap::single(
                "PRICE".parse().unwrap(),
                Value::Integer(1)
            )],
        )]);
        let event = Event {
            id: "evt-1".parse().unwrap(),
            created_date_time: now,
            modification_date_time: now,
            object_type: ObjectType::Event,
            content,
        };

        let ven = runtime().with_clock(Arc::new(FixedClock::new(now)));
        {
            let mut inner = ven.write();
            inner.events.insert(event.id.clone(), event);
            inner.rebuild_timelines(now, ven.config.poll_interval);
        }

        assert_eq!(
            ven.next_change(now),
            Some(starts_at),
            "the transition should be the interval's start"
        );
        assert_eq!(
            ven.time_to_next_wakeup(),
            StdDuration::from_millis(800),
            "a sub-second transition became a wait of zero, which is a spin, or of a whole \
             second, which is late"
        );
    }

    #[tokio::test]
    async fn a_hint_that_arrives_while_the_ven_is_busy_is_not_lost() {
        // The case this exists for: a notification about the very write the VEN is in the middle of
        // reading. If the hint only counted when somebody was already waiting, the VEN would sync,
        // miss it, and sleep out the poll interval on data it had just been told was stale.
        let ven = runtime();
        ven.waker().wake();
        tokio::time::timeout(StdDuration::from_millis(50), ven.wait_for_work())
            .await
            .expect("a hint delivered before the wait should end it immediately");
    }

    #[tokio::test]
    async fn several_hints_collapse_into_one_sync() {
        // Two "something changed" signals are one sync, so the second wait sleeps.
        let ven = runtime();
        for _ in 0..5 {
            ven.waker().wake();
        }
        tokio::time::timeout(StdDuration::from_millis(50), ven.wait_for_work())
            .await
            .expect("the first wait is ended by the hint");
        assert!(
            tokio::time::timeout(StdDuration::from_millis(50), ven.wait_for_work())
                .await
                .is_err(),
            "five hints woke the VEN more than once"
        );
    }

    #[tokio::test]
    async fn a_hint_ends_a_wait_already_in_progress() {
        let ven = std::sync::Arc::new(runtime());
        let waker = ven.waker();
        let waiting = tokio::spawn({
            let ven = ven.clone();
            async move { tokio::time::timeout(StdDuration::from_secs(5), ven.wait_for_work()).await }
        });
        tokio::time::sleep(StdDuration::from_millis(20)).await;
        waker.wake();
        waiting
            .await
            .unwrap()
            .expect("the VEN slept through a hint delivered mid-wait");
    }
}
