//! The VTN server.
//!
//! [`Vtn::builder`] assembles storage, authentication and a notification transport into an
//! [`axum::Router`]. The builder is a typestate: `build()` exists only once `storage()` has been
//! called, so a VTN with no backing store is a compile error rather than a start-up panic.
//!
//! Configuration, the endpoint surface and deployment shapes:
//! <https://hupe1980.github.io/openadr/docs/vtn/>.
//!
//! ```no_run
//! use openadr::vtn::{Vtn, auth::{StaticTokenAuth}, store::MemoryStorage};
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let vtn = Vtn::builder()
//!     .storage(MemoryStorage::shared())
//!     .authenticator(std::sync::Arc::new(
//!         StaticTokenAuth::new("http://localhost:3000/auth/token")
//!             .with_business_logic("bl-token", "bl".parse()?),
//!     ))
//!     .build();
//! vtn.serve("0.0.0.0:3000").await?;
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;

use axum::{
    Router,
    routing::{get, post},
};

use crate::core::{Clock, SystemClock};
use crate::model::notifier::MqttNotifierBinding;
use crate::schema::Policy;

pub mod api;
pub mod auth;
pub mod dispatch;
pub mod error;
pub mod etag;
pub mod metrics;
pub mod notify;
pub mod openapi;
pub mod retention;
pub mod store;

#[cfg(feature = "tls")]
#[cfg_attr(docsrs, doc(cfg(feature = "tls")))]
pub mod tls;

pub use dispatch::{DispatchConfig, Dispatcher};
pub use error::ApiError;
pub use metrics::Metrics;
pub use retention::{Retention, RetentionConfig};

use auth::SharedAuthenticator;
use store::SharedStorage;

/// How the VTN behaves.
#[derive(Debug, Clone)]
pub struct VtnConfig {
    /// Path the API is mounted under, e.g. `/openadr3/3.1.0`.
    pub base_path: String,
    /// How strictly payload values are checked against the enumerations.
    pub payload_policy: Policy,
    /// The MQTT binding to advertise from `GET /notifiers`, if any.
    pub mqtt: Option<MqttNotifierBinding>,
    /// Prefix for MQTT topic names, so one broker can serve several VTNs.
    pub mqtt_topic_prefix: String,
    /// Broker usernames that may subscribe to collection-wide MQTT topics.
    ///
    /// The broker asks `POST /internal/mqtt/acl` whether a client may subscribe to a topic, and the
    /// answer for a VEN falls out of the topic itself: `…/vens/{venID}/…` is its own if the VEN's
    /// `clientID` is the connecting username. A collection-wide topic carries every object with its
    /// full target set, so it needs a stated decision instead — and an empty list is the safe one.
    ///
    /// Stated rather than derived from the caller's scopes because the broker's authorization
    /// callback carries a username and no token, and a VTN behind a load balancer must not need to
    /// remember the connection that authenticated.
    pub mqtt_business_logic_clients: Vec<crate::model::ClientId>,
    /// The `clientID` the VTN's own publisher connects to the broker as.
    ///
    /// A VTN whose broker delegates authentication to `POST /internal/mqtt/auth` is a client of its
    /// own broker, and `POST /internal/mqtt/acl` refuses every publish — deliberately, because a
    /// client that could publish could forge a dispatch instruction. Without naming the publisher
    /// somewhere, that refusal includes the VTN, and the fan-out reconnects for ever against its own
    /// broker with `NotAuthorized`, which is a state nothing else reports.
    ///
    /// Naming it here is the smallest exception that works: **one** identity may publish, it must
    /// still present a valid credential to `/auth` like everybody else, and it may publish only
    /// under [`mqtt_topic_prefix`](VtnConfig::mqtt_topic_prefix). `None`, the default, keeps the
    /// blanket refusal — which is right for a broker that authenticates the VTN by some other
    /// means, such as its own user database or mutual TLS.
    pub mqtt_publisher_client: Option<crate::model::ClientId>,
    /// Whether `GET` responses carry `ETag` and honour `If-None-Match`.
    ///
    /// On by default. Real deployments poll — Dutch grid-aware charging re-reads a 48-hour window,
    /// Fluvius polls every few minutes, Californian price clients poll hourly — and a `304` is the
    /// difference between a kilobyte and a megabyte per client per poll.
    pub http_caching: bool,
    /// Whether `GET /programs?programName=` is accepted.
    ///
    /// Not in the specification; proposed as oadr3-org/specification#418.
    pub program_name_lookup: bool,
    /// Largest request body accepted, in bytes.
    ///
    /// An event carrying a year of quarter-hourly intervals is large but bounded; an unbounded body
    /// is a denial-of-service vector the specification's security chapter asks an API gateway to
    /// handle, which not every deployment has.
    pub max_body_bytes: usize,
    /// How long a request may take before it is abandoned.
    pub request_timeout: std::time::Duration,
    /// Which webhook callback URLs a subscription may name.
    ///
    /// The same policy the transport applies before every delivery, so a URL cannot be accepted
    /// here and refused for ever afterwards there.
    pub callback_policy: notify::CallbackPolicy,
    /// How the notification queue is drained.
    pub dispatch: DispatchConfig,
    /// What is aged out, and how often.
    ///
    /// Off by default: `report` is the only object that grows without bound, and it is also
    /// settlement data, so deleting it is a decision an operator makes rather than one this crate
    /// makes for them `[D-113]`.
    pub retention: RetentionConfig,
}

impl Default for VtnConfig {
    fn default() -> Self {
        Self {
            base_path: crate::DEFAULT_BASE_PATH.to_string(),
            payload_policy: Policy::default(),
            mqtt: None,
            mqtt_topic_prefix: String::new(),
            mqtt_business_logic_clients: Vec::new(),
            mqtt_publisher_client: None,
            http_caching: true,
            program_name_lookup: true,
            max_body_bytes: 8 * 1024 * 1024,
            request_timeout: std::time::Duration::from_secs(30),
            callback_policy: notify::CallbackPolicy::default(),
            dispatch: DispatchConfig::default(),
            retention: RetentionConfig::default(),
        }
    }
}

/// Everything a handler needs.
#[derive(Clone)]
pub struct AppState {
    /// The backing store.
    pub storage: SharedStorage,
    /// How credentials are resolved.
    pub authenticator: SharedAuthenticator,
    /// Where time comes from.
    pub clock: Arc<dyn Clock>,
    /// Server behaviour.
    pub config: Arc<VtnConfig>,
    /// Where notifications go.
    pub notifier: Arc<dyn notify::Notifier>,
    /// Counters and histograms, served at `GET /metrics`.
    pub metrics: Metrics,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// A configured VTN.
#[derive(Debug, Clone)]
pub struct Vtn {
    state: AppState,
    dispatcher: Dispatcher,
    retention: Retention,
}

/// Marks a builder that has not been given a storage backend yet.
#[derive(Debug, Clone, Copy)]
pub struct NeedsStorage;

/// Marks a builder that is ready to [`build`](VtnBuilder::build).
#[derive(Debug, Clone, Copy)]
pub struct Ready;

/// Builds a [`Vtn`].
///
/// The type parameter tracks whether a storage backend has been supplied: [`VtnBuilder::build`]
/// exists only on `VtnBuilder<Ready>`, so a VTN without a store is a compile error rather than a
/// panic at start-up.
pub struct VtnBuilder<State = NeedsStorage> {
    storage: Option<SharedStorage>,
    authenticator: Option<SharedAuthenticator>,
    clock: Arc<dyn Clock>,
    config: VtnConfig,
    /// What a granular setter asked for, applied *after* [`VtnBuilder::config`] regardless of the
    /// order the two were called in.
    ///
    /// Without this, `.mqtt(binding).config(VtnConfig::default())` silently discards the binding —
    /// the VTN starts, `GET /notifiers` reports no broker, every topic endpoint answers `501`, and
    /// nothing anywhere says why. It is the shape of D-045 in a builder: a value is accepted and
    /// then ignored.
    overrides: Overrides,
    notifier: Option<Arc<dyn notify::Notifier>>,
    state: core::marker::PhantomData<State>,
}

/// Configuration a granular setter supplied, kept apart so call order cannot lose it.
#[derive(Debug, Default, Clone)]
struct Overrides {
    base_path: Option<String>,
    payload_policy: Option<Policy>,
    mqtt: Option<MqttNotifierBinding>,
    mqtt_business_logic_clients: Option<Vec<crate::model::ClientId>>,
    mqtt_publisher_client: Option<crate::model::ClientId>,
    callback_policy: Option<notify::CallbackPolicy>,
    dispatch: Option<DispatchConfig>,
}

impl Overrides {
    /// Lay the explicit values over a configuration.
    fn apply(self, config: &mut VtnConfig) {
        if let Some(v) = self.base_path {
            config.base_path = v;
        }
        if let Some(v) = self.payload_policy {
            config.payload_policy = v;
        }
        if let Some(v) = self.mqtt {
            config.mqtt = Some(v);
        }
        if let Some(v) = self.mqtt_business_logic_clients {
            config.mqtt_business_logic_clients = v;
        }
        if let Some(v) = self.mqtt_publisher_client {
            config.mqtt_publisher_client = Some(v);
        }
        if let Some(v) = self.callback_policy {
            config.callback_policy = v;
        }
        if let Some(v) = self.dispatch {
            config.dispatch = v;
        }
    }
}

impl<S> std::fmt::Debug for VtnBuilder<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VtnBuilder")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Default for VtnBuilder<NeedsStorage> {
    fn default() -> Self {
        Self {
            storage: None,
            authenticator: None,
            clock: Arc::new(SystemClock),
            config: VtnConfig::default(),
            overrides: Overrides::default(),
            notifier: None,
            state: core::marker::PhantomData,
        }
    }
}

impl VtnBuilder<NeedsStorage> {
    /// Set the backing store, which is what makes the builder buildable.
    pub fn storage(self, storage: SharedStorage) -> VtnBuilder<Ready> {
        VtnBuilder {
            storage: Some(storage),
            authenticator: self.authenticator,
            clock: self.clock,
            config: self.config,
            overrides: self.overrides,
            notifier: self.notifier,
            state: core::marker::PhantomData,
        }
    }
}

impl<S> VtnBuilder<S> {
    /// Set the authenticator.
    pub fn authenticator(mut self, authenticator: SharedAuthenticator) -> Self {
        self.authenticator = Some(authenticator);
        self
    }

    /// Override the clock, for tests and deterministic replay.
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Replace the whole configuration.
    ///
    /// Anything a granular setter asked for still wins, whichever order the two were called in —
    /// see [`VtnBuilder::overrides`](VtnBuilder). Replacing them silently was a way to configure a
    /// broker and then serve a VTN that had none.
    pub fn config(mut self, config: VtnConfig) -> Self {
        self.config = config;
        self
    }

    /// Mount the API under a path.
    pub fn base_path(mut self, path: impl Into<String>) -> Self {
        self.overrides.base_path = Some(path.into());
        self
    }

    /// Set the payload validation policy.
    pub fn payload_policy(mut self, policy: Policy) -> Self {
        self.overrides.payload_policy = Some(policy);
        self
    }

    /// Advertise an MQTT notifier binding.
    ///
    /// This is what turns the broker fan-out on: `GET /notifiers` starts naming a broker, the topic
    /// endpoints start answering, and every write queues a copy per entitled VEN. A transport that
    /// [`handles`](notify::Notifier::handles) [`Channel::Mqtt`](notify::Channel::Mqtt) has to be
    /// installed alongside it, or those copies have nowhere to go — [`VtnBuilder::build`] says so.
    pub fn mqtt(mut self, binding: MqttNotifierBinding) -> Self {
        self.overrides.mqtt = Some(binding);
        self
    }

    /// Name the broker usernames allowed to subscribe to collection-wide MQTT topics.
    pub fn mqtt_business_logic_clients(
        mut self,
        clients: impl IntoIterator<Item = crate::model::ClientId>,
    ) -> Self {
        self.overrides.mqtt_business_logic_clients = Some(clients.into_iter().collect());
        self
    }

    /// Name the `clientID` the VTN's own publisher connects to the broker as.
    ///
    /// See [`VtnConfig::mqtt_publisher_client`]. Needed only when the broker authenticates the VTN
    /// through `POST /internal/mqtt/auth` like every other client.
    pub fn mqtt_publisher_client(mut self, client: crate::model::ClientId) -> Self {
        self.overrides.mqtt_publisher_client = Some(client);
        self
    }

    /// Set where notifications are delivered.
    pub fn notifier(mut self, notifier: Arc<dyn notify::Notifier>) -> Self {
        self.notifier = Some(notifier);
        self
    }

    /// Set which callback URLs subscriptions may name.
    pub fn callback_policy(mut self, policy: notify::CallbackPolicy) -> Self {
        self.overrides.callback_policy = Some(policy);
        self
    }

    /// Set how the notification queue is drained.
    pub fn dispatch(mut self, config: DispatchConfig) -> Self {
        self.overrides.dispatch = Some(config);
        self
    }
}

impl VtnBuilder<Ready> {
    /// Finish.
    ///
    /// An authenticator defaults to [`AnonymousAuth`](auth::AnonymousAuth), which yields a
    /// read-only public VTN — the specification's non-authenticating mode.
    pub fn build(mut self) -> Vtn {
        // Last, so `.mqtt(x).config(y)` and `.config(y).mqtt(x)` mean the same thing.
        self.overrides.apply(&mut self.config);
        let config = Arc::new(self.config);
        let authenticator = self.authenticator.unwrap_or_else(|| {
            Arc::new(auth::AnonymousAuth::new(format!(
                "{}/auth/token",
                config.base_path
            )))
        });
        let metrics = Metrics::new();
        let state = AppState {
            metrics: metrics.clone(),
            storage: self
                .storage
                .expect("VtnBuilder<Ready> is only reachable through `storage`"),
            authenticator,
            clock: self.clock,
            config: config.clone(),
            // An *empty* set of transports rather than `NullNotifier`. The difference is what the
            // VTN then says about itself: with no transport, `GET /notifiers` reports
            // `WEBHOOK: false` and `POST /subscriptions` refuses, instead of accepting a
            // subscription that could never be delivered. `NullNotifier` is still available for
            // an embedder that genuinely wants deliveries dropped, and says so by installing it.
            notifier: self
                .notifier
                .unwrap_or_else(|| Arc::new(notify::Notifiers::new())),
        };
        // A binding with no publisher behind it queues a copy per entitled VEN and has nowhere to
        // put any of them. They are not lost — they land in the dead count `GET /health` reports —
        // but the operator should hear it now rather than from a VEN that never woke up.
        if config.mqtt.is_some() && !state.notifier.handles(notify::Channel::Mqtt) {
            tracing::error!(
                notifier = state.notifier.name(),
                "an MQTT binding is advertised but no transport publishes to a broker; every \
                 broker notification will be queued and then abandoned. Install \
                 `MqttNotifier` (feature `mqtt`), or drop the binding."
            );
        }

        let dispatcher = Dispatcher::new(
            state.storage.clone(),
            state.notifier.clone(),
            state.clock.clone(),
            config.dispatch.clone(),
        )
        .with_metrics(metrics.clone());
        let retention = Retention::new(
            state.storage.clone(),
            state.clock.clone(),
            config.retention.clone(),
        )
        .with_metrics(metrics);
        Vtn {
            state,
            dispatcher,
            retention,
        }
    }
}

impl Vtn {
    /// Start building. Supply a storage backend to reach [`VtnBuilder::build`].
    pub fn builder() -> VtnBuilder<NeedsStorage> {
        VtnBuilder::default()
    }

    /// The application state, for tests and embedding.
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// The queue drainer.
    ///
    /// [`Vtn::serve`] spawns it. An embedder that runs the router itself must spawn it too, or
    /// nothing will ever be delivered — [`Dispatcher::spawn`] does that in one line. Tests
    /// generally want [`Dispatcher::drain`] instead, which is deterministic.
    pub fn dispatcher(&self) -> &Dispatcher {
        &self.dispatcher
    }

    /// The retention sweeper.
    ///
    /// [`Vtn::serve`] spawns it when `VtnConfig::retention` asks for anything; an embedder running
    /// the router itself must too. Tests want [`Retention::sweep`], which is deterministic.
    pub fn retention(&self) -> &Retention {
        &self.retention
    }

    /// The axum router, mounted under the configured base path.
    ///
    /// The API is also mounted at the root so that a client configured with a bare base URL — which
    /// the specification's enrollment flow permits — still works.
    pub fn router(&self) -> Router {
        let api = self.api_router();
        let base = self
            .state
            .config
            .base_path
            .trim_end_matches('/')
            .to_string();

        let mut router = Router::new()
            .route("/health", get(api::health))
            // Operational, not protocol: mounted at the root only, so it is not confused for part
            // of the OpenADR surface and is trivial to keep off a public listener.
            .route("/admin/outbox", get(api::admin_outbox))
            .route("/admin/outbox/retry", post(api::admin_outbox_retry))
            .route("/admin/subscribers", get(api::admin_subscribers))
            .route("/metrics", get(metrics::scrape))
            .merge(api.clone());

        if !base.is_empty() {
            router = router.nest(&base, api);
        }

        let config = &self.state.config;
        // The order below is a correctness property, not a preference, and reads inside-out: the
        // last `.layer` is the outermost.
        //
        // * The body limit and the timeout are *innermost* so that `problem_responses` sits outside
        //   them and can turn their bare bodies — `length limit exceeded`, and nothing at all — into
        //   `problem` documents. They were outside it for as long as the VTN answered `413` in
        //   plain text `[D-104]`.
        // * `problem_responses` is inside the compression layer, because it reads an error body back
        //   to stamp `problem.instance` on it. Outside, it would be reading gzip and silently
        //   leaving every problem unstamped for any client that accepts an encoding.
        // * The metrics layer is outside `problem_responses`, so a series counts the status the
        //   client saw, and inside the router, so the matched path is available to label it with.
        router
            .fallback(api::not_found)
            .layer(tower_http::limit::RequestBodyLimitLayer::new(
                config.max_body_bytes,
            ))
            // 504 rather than 408: the deadline was the server's, not the client's.
            .layer(tower_http::timeout::TimeoutLayer::with_status_code(
                axum::http::StatusCode::GATEWAY_TIMEOUT,
                config.request_timeout,
            ))
            .layer(axum::middleware::from_fn(api::problem_responses))
            .layer(axum::middleware::from_fn_with_state(
                self.state.clone(),
                metrics::track,
            ))
            // Compression is "encouraged" by the specification for bandwidth-constrained links,
            // and an event with a year of quarter-hourly intervals compresses by an order of
            // magnitude.
            .layer(tower_http::compression::CompressionLayer::new())
            .layer(
                tower_http::trace::TraceLayer::new_for_http().make_span_with(
                    |request: &axum::http::Request<_>| {
                        tracing::info_span!(
                            "request",
                            method = %request.method(),
                            path = %request.uri().path(),
                            request_id = request
                                .headers()
                                .get(api::REQUEST_ID_HEADER)
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("-"),
                        )
                    },
                ),
            )
            // The id is minted before anything else runs and echoed back on the way out, so the
            // `x-request-id` a client quotes, the `instance` in the problem body it received and
            // the span in the log are all the same string. An id supplied by a proxy is kept.
            .layer(tower_http::request_id::PropagateRequestIdLayer::x_request_id())
            .layer(tower_http::request_id::SetRequestIdLayer::x_request_id(
                tower_http::request_id::MakeRequestUuid,
            ))
            .with_state(self.state.clone())
    }

    fn api_router(&self) -> Router<AppState> {
        Router::new()
            // Under the base path, not only at the root: a deployment behind a proxy that exposes
            // `/openadr3/3.1.0/*` and nothing else still has to be able to describe itself, and
            // `openapi_url` in the mDNS record points at a document a VEN must be able to fetch.
            // Unauthenticated, like `/auth/server`: it is what a client reads before it has a
            // credential, and it discloses no object.
            .route("/openapi.json", get(openapi::describe))
            .route(
                "/programs",
                get(api::programs::list).post(api::programs::create),
            )
            .route(
                "/programs/{id}",
                get(api::programs::get)
                    .put(api::programs::update)
                    .delete(api::programs::delete),
            )
            .route("/events", get(api::events::list).post(api::events::create))
            .route(
                "/events/{id}",
                get(api::events::get)
                    .put(api::events::update)
                    .delete(api::events::delete),
            )
            .route(
                "/reports",
                get(api::reports::list).post(api::reports::create),
            )
            .route(
                "/reports/{id}",
                get(api::reports::get)
                    .put(api::reports::update)
                    .delete(api::reports::delete),
            )
            .route(
                "/subscriptions",
                get(api::subscriptions::list).post(api::subscriptions::create),
            )
            .route(
                "/subscriptions/{id}",
                get(api::subscriptions::get)
                    .put(api::subscriptions::update)
                    .delete(api::subscriptions::delete),
            )
            .route("/vens", get(api::vens::list).post(api::vens::create))
            .route(
                "/vens/{id}",
                get(api::vens::get)
                    .put(api::vens::update)
                    .delete(api::vens::delete),
            )
            .route(
                "/resources",
                get(api::resources::list).post(api::resources::create),
            )
            .route(
                "/resources/{id}",
                get(api::resources::get)
                    .put(api::resources::update)
                    .delete(api::resources::delete),
            )
            // Private to the VTN and its broker, not an OpenADR endpoint — see `api::broker`.
            .route("/internal/mqtt/auth", post(api::broker::authenticate))
            .route("/internal/mqtt/acl", post(api::broker::authorize))
            .route("/auth/server", get(api::auth_server))
            .route("/auth/token", post(api::auth_token))
            .route("/notifiers", get(api::notifiers::describe))
            .route(
                "/notifiers/mqtt/topics/programs",
                get(api::notifiers::programs),
            )
            .route(
                "/notifiers/mqtt/topics/programs/{id}",
                get(api::notifiers::program_by_id),
            )
            .route("/notifiers/mqtt/topics/events", get(api::notifiers::events))
            .route(
                "/notifiers/mqtt/topics/programs/{id}/events",
                get(api::notifiers::program_events),
            )
            .route(
                "/notifiers/mqtt/topics/reports",
                get(api::notifiers::reports),
            )
            .route(
                "/notifiers/mqtt/topics/subscriptions",
                get(api::notifiers::subscriptions),
            )
            .route("/notifiers/mqtt/topics/vens", get(api::notifiers::vens))
            .route(
                "/notifiers/mqtt/topics/vens/{id}",
                get(api::notifiers::ven_by_id),
            )
            .route(
                "/notifiers/mqtt/topics/resources",
                get(api::notifiers::resources),
            )
            .route(
                "/notifiers/mqtt/topics/vens/{id}/events",
                get(api::notifiers::ven_events),
            )
            .route(
                "/notifiers/mqtt/topics/vens/{id}/programs",
                get(api::notifiers::ven_programs),
            )
            .route(
                "/notifiers/mqtt/topics/vens/{id}/resources",
                get(api::notifiers::ven_resources),
            )
            // Extensions: see `api::notifiers`.
            .route(
                "/notifiers/mqtt/topics/vens/{id}/reports",
                get(api::notifiers::ven_reports),
            )
            .route(
                "/notifiers/mqtt/topics/vens/{id}/subscriptions",
                get(api::notifiers::ven_subscriptions),
            )
    }

    /// Bind and serve until shut down.
    pub async fn serve(&self, addr: impl tokio::net::ToSocketAddrs) -> std::io::Result<()> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!(
            addr = ?listener.local_addr()?,
            base_path = %self.state.config.base_path,
            scheme = "http",
            "VTN listening"
        );
        let dispatcher = self.dispatcher.clone().spawn();
        let sweeper = self.retention.clone().spawn();
        let result = axum::serve(listener, self.router())
            .with_graceful_shutdown(shutdown_signal())
            .await;
        // Whatever is still queued stays queued: it is durable, and the next process to start will
        // pick it up. Aborting beats waiting on a delivery to a dead endpoint during shutdown.
        dispatcher.abort();
        if let Some(sweeper) = sweeper {
            sweeper.abort();
        }
        result
    }

    /// The same, over TLS.
    ///
    /// With a client CA in the [`TlsConfig`](tls::TlsConfig) this is also the network gate: a peer
    /// whose certificate that CA did not issue never reaches a handler. It is *not* an identity —
    /// see [`tls`] for why the two are kept apart.
    #[cfg(feature = "tls")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tls")))]
    pub async fn serve_tls(
        &self,
        addr: impl tokio::net::ToSocketAddrs,
        config: tls::TlsConfig,
    ) -> std::io::Result<()> {
        // Before binding: a certificate `rustls` refuses is a configuration mistake, and reporting
        // it after the port is open makes it look like a runtime failure.
        let server_config = config
            .server_config()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!(
            addr = ?listener.local_addr()?,
            base_path = %self.state.config.base_path,
            scheme = "https",
            client_certificates = config.asks_for_client_certificates(),
            "VTN listening"
        );
        let dispatcher = self.dispatcher.clone().spawn();
        let result = tls::serve(listener, self.router(), server_config, shutdown_signal()).await;
        dispatcher.abort();
        result
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MqttAuthentication, Serialization};

    #[test]
    fn a_granular_setter_survives_a_whole_configuration_being_replaced() {
        // `.mqtt(x).config(y)` used to discard `x` silently: the VTN started, `GET /notifiers`
        // reported no broker, every topic endpoint answered `501`, and nothing said why. It is
        // D-045's shape in a builder — a value accepted and then ignored — and `cargo xtask
        // check-paths` is what tripped over it.
        let binding = MqttNotifierBinding {
            uris: vec!["mqtts://broker.test:8883".into()],
            serialization: Serialization::Json,
            authentication: MqttAuthentication::Anonymous,
        };

        for (order, vtn) in [
            (
                "setter first",
                Vtn::builder()
                    .storage(store::MemoryStorage::shared())
                    .mqtt(binding.clone())
                    .base_path("/custom")
                    .config(VtnConfig::default())
                    .build(),
            ),
            (
                "config first",
                Vtn::builder()
                    .storage(store::MemoryStorage::shared())
                    .config(VtnConfig::default())
                    .mqtt(binding.clone())
                    .base_path("/custom")
                    .build(),
            ),
        ] {
            assert_eq!(
                vtn.state().config.mqtt.as_ref(),
                Some(&binding),
                "the broker binding was lost ({order})"
            );
            assert_eq!(vtn.state().config.base_path, "/custom", "({order})");
        }

        // And a configuration nothing overrides is used exactly as given.
        let vtn = Vtn::builder()
            .storage(store::MemoryStorage::shared())
            .config(VtnConfig {
                base_path: "/from-config".into(),
                ..Default::default()
            })
            .build();
        assert_eq!(vtn.state().config.base_path, "/from-config");
    }
}
