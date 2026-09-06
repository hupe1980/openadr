//! # openadr — OpenADR 3.1 in Rust
//!
//! An implementation of the [OpenADR 3](https://www.openadr.org/) demand-response protocol: a VTN
//! server, a VEN runtime and a typed client, over a shared wire model and a deterministic domain
//! core — one crate, sliced with Cargo features.
//!
//! OpenADR is how a utility asks flexible load to move. A **VTN** publishes programmes and events;
//! a **VEN** — a charge point, a battery, a heat pump — reads them, acts, and reports back.
//! Narrative documentation, including an introduction to the protocol itself, is at
//! <https://hupe1980.github.io/openadr>.
//!
//! ## Layers
//!
//! | Module | Feature | What |
//! |---|---|---|
//! | [`model`] | always on (`no_std` + `alloc`) | wire types, newtypes with invariants, spec sentinels |
//! | [`schema`] | always on | payload typing from the Alliance enumeration schemas |
//! | [`core`] | always on | interval expansion, timeline, object privacy, report scheduling |
//! | [`client`] | `client` | business-logic and VEN HTTP clients, role-checked at compile time |
//! | [`ven`] | `ven` | the VEN loop: registration, event sync, timeline, report scheduling |
//! | [`conformance`] | `conformance` | a black-box suite that measures **any** VTN against the specification |
//! | [`vtn`] | `vtn` | axum VTN server, storage, authentication, the notification outbox |
//! | [`webhook`] | `webhook-signature` | the webhook signature scheme, both ends of it (`no_std`) |
//! | [`discovery`] | always on; `mdns` for the socket | finding a local VTN over mDNS/DNS-SD, both ends |
//!
//! Storage backends are `sqlite` and `postgres`; authentication backends are `internal-auth` and
//! `external-auth`; push transports are `webhook` and `mqtt`. Every flag is read by a `cfg`.
//! `webhook-signature` is the only one that does not imply a role: the signature scheme's receiving
//! half belongs to a subscriber, and a subscriber is not a server.
//!
//! ## Serving
//!
//! ```no_run
//! use openadr::vtn::{Vtn, store::MemoryStorage};
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let vtn = Vtn::builder().storage(MemoryStorage::shared()).build();
//! vtn.serve("0.0.0.0:3000").await?;
//! # Ok(())
//! # }
//! ```
//!
//! [`Vtn::builder`](vtn::Vtn::builder) is a typestate: `build()` exists only once a storage backend
//! has been given, so a VTN with nowhere to put an event is a compile error. Without an
//! authenticator it serves unauthenticated readers, which is read-only by construction — every
//! write requires a scope, and an anonymous principal holds none.
//!
//! ## Reading a schedule
//!
//! ```no_run
//! use openadr::prelude::*;
//! # fn run(event: &EventRequest) -> Result<(), Box<dyn std::error::Error>> {
//! // Resolve an event's declared timing into absolute windows: inherited periods, the `now`
//! // sentinel, unbounded durations, and multi-value payloads that subdivide their interval.
//! let intervals = IntervalExpander::at(Timestamp::now()).expand(event)?;
//!
//! // Or the whole sequence, including the intervals the event never lists: a tariff that loops
//! // for ever, and a forecast whose structure is implied by its `intervalPeriod` alone.
//! let sequence = IntervalExpander::at(Timestamp::now()).sequence(event)?;
//! # Ok(())
//! # }
//! ```
//!
//! [`core::Timeline`] then merges a programme's concurrent events by priority, splitting the loser
//! *around* the winner so a long price curve resumes when a short curtailment ends.
//! [`ven::VenRuntime`] is that loop with registration, conditional polling and reporting around it.
//!
//! ## Spec version
//!
//! [`SPEC_VERSION`] is the release this crate is written from. There is no 3.0 compatibility in the
//! wire model — 3.1 is not backwards compatible with it — and peers that bend the schema are served
//! by request/response adapters at the edge instead (see [`model::adapt`]).
//!
//! ## `no_std`
//!
//! `default-features = false` yields a `no_std` + `alloc` build containing [`model`], [`schema`]
//! and [`core`] — the layers an appliance-class VEN needs, and they build for
//! `thumbv7em-none-eabihf` and `wasm32-unknown-unknown`. `no_std` here means no operating system,
//! not no heap: the wire model is `String`s and `Vec`s, so an allocator is assumed.
#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_code)]
#![deny(rustdoc::broken_intra_doc_links)]
#![warn(missing_debug_implementations)]

// Always: the wire model is Strings and Vecs, so an allocator is a requirement rather than a
// choice. `no_std` here means "no operating system", not "no heap".
extern crate alloc;

pub mod core;
pub mod model;
pub mod schema;

#[cfg(feature = "client")]
#[cfg_attr(docsrs, doc(cfg(feature = "client")))]
pub mod client;

#[cfg(feature = "conformance")]
#[cfg_attr(docsrs, doc(cfg(feature = "conformance")))]
pub mod conformance;

#[cfg(feature = "ven")]
#[cfg_attr(docsrs, doc(cfg(feature = "ven")))]
pub mod ven;

#[cfg(feature = "vtn")]
#[cfg_attr(docsrs, doc(cfg(feature = "vtn")))]
pub mod vtn;

#[cfg(feature = "webhook-signature")]
#[cfg_attr(docsrs, doc(cfg(feature = "webhook-signature")))]
pub mod webhook;

pub mod discovery;

/// Broker connection options, shared by the VTN's publisher and the VEN's subscriber.
///
/// `mqtt` implies no role by design, so `--features mqtt` alone enables *neither* — and compiling
/// this module then would be a module with no caller, which `-D warnings` rightly refuses.
#[cfg(all(feature = "mqtt", any(feature = "vtn", feature = "ven")))]
mod mqtt;

/// One `rustls` decision, shared by everything here that speaks TLS.
#[cfg(any(
    feature = "client",
    feature = "external-auth",
    feature = "tls",
    feature = "webhook",
    all(feature = "mqtt", any(feature = "vtn", feature = "ven")),
))]
mod crypto;

#[cfg(any(
    feature = "client",
    feature = "external-auth",
    feature = "tls",
    feature = "webhook",
    all(feature = "mqtt", any(feature = "vtn", feature = "ven")),
))]
pub use crypto::install_crypto_provider;

/// `std`/`no_std` bridge. Internal, but public so generated code and examples can use it.
#[doc(hidden)]
pub mod std_shim {
    #[cfg(not(feature = "std"))]
    pub use alloc::{
        borrow::ToOwned,
        boxed::Box,
        collections::BTreeMap,
        format,
        string::{String, ToString},
        vec,
        vec::Vec,
    };
    #[cfg(feature = "std")]
    pub use std::{
        borrow::ToOwned,
        boxed::Box,
        collections::BTreeMap,
        format,
        string::{String, ToString},
        vec,
        vec::Vec,
    };
}

/// Everything a typical user wants in scope.
pub mod prelude {
    pub use crate::core::{
        Access, Clock, ExpandedInterval, Grant, IntervalExpander, IntervalSequence, ReportSchedule,
        Role, Timeline,
    };
    pub use crate::model::{
        ClientId, ClientName, Duration, Event, EventPayloadDescriptor, EventRequest, Interval,
        IntervalPeriod, ObjectId, ObjectType, Priority, Problem, Program, ProgramRequest, Report,
        ReportDescriptor, ReportRequest, Resource, ResourceName, StartTime, Subscription,
        SubscriptionRequest, Target, Timestamp, Unit, Value, ValuesMap, Ven, VenName,
    };
}

/// The OpenADR specification release this crate is written from.
///
/// Also the single source of truth for `cargo xtask codegen`, which generates the payload table
/// from this version's enumeration files — so the table and the crate cannot describe different
/// releases.
pub const SPEC_VERSION: &str = "3.1.0";

/// Default base path a VTN is mounted under (`/openadr3/3.1.0`).
pub const DEFAULT_BASE_PATH: &str = "/openadr3/3.1.0";
