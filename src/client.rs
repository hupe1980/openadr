//! HTTP clients for the two roles.
//!
//! The role is a type parameter, so the compiler enforces what the specification's scopes describe:
//! [`Client<BusinessLogic>`] can create events, [`Client<VirtualEndNode>`] cannot, and asking for
//! the wrong one is a compile error rather than a `403` in production.
//!
//! Guide, including a VEN polling loop: <https://hupe1980.github.io/openadr/docs/client/>.
//!
//! ```no_run
//! use openadr::client::{Client, BusinessLogic, Credentials};
//! # async fn run() -> Result<(), openadr::client::ClientError> {
//! let client = Client::<BusinessLogic>::builder("https://vtn.example.com/openadr3/3.1.0")?
//!     .credentials(Credentials::new("client-id", "client-secret"))
//!     .build()?;
//!
//! let programs = client.programs().list().await?;
//! # Ok(())
//! # }
//! ```

use std::{
    marker::PhantomData,
    sync::{Arc, Mutex},
    time::{Duration as StdDuration, Instant},
};

use reqwest::{Method, StatusCode, header};
use serde::{Serialize, de::DeserializeOwned};
use url::Url;

use crate::model::{
    AuthServerInfo, ClientCredentialResponse, Event, EventRequest, NotifiersResponse, ObjectId,
    Problem, Program, ProgramRequest, Report, ReportRequest, Resource, ResourceRequest,
    Subscription, SubscriptionRequest, Target, TopicsResponse, Ven, VenRequest,
    adapt::{AdapterChain, WireAdapter},
};

/// Why a request failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The base URL could not be parsed or joined.
    #[error("invalid URL: {0}")]
    Url(#[from] url::ParseError),
    /// The transport failed.
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    /// The VTN returned a problem.
    #[error("{} {}: {}", status, problem.title.as_deref().unwrap_or("error"),
            problem.detail.as_deref().unwrap_or("no detail"))]
    Api {
        /// The HTTP status.
        status: StatusCode,
        /// The problem body.
        problem: Box<Problem>,
    },
    /// The VTN returned something unparseable.
    #[error("could not decode the response: {0}")]
    Decode(String),
    /// Obtaining an access token failed.
    #[error("authentication failed: {0}")]
    Auth(String),
}

impl ClientError {
    /// The HTTP status, when the VTN answered at all.
    ///
    /// `None` for a transport failure, which is the distinction that matters: a `404` is the VTN
    /// saying "not here", and a refused connection is the VTN saying nothing.
    pub fn status(&self) -> Option<u16> {
        match self {
            ClientError::Api { status, .. } => Some(status.as_u16()),
            _ => None,
        }
    }
}

/// OAuth2 client-credentials, plus where to redeem them.
#[derive(Clone)]
pub struct Credentials {
    client_id: String,
    client_secret: String,
    token_url: Option<Url>,
    scopes: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the secret, not even in a panic message.
        f.debug_struct("Credentials")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("token_url", &self.token_url)
            .finish()
    }
}

impl Credentials {
    /// The `clientID` half, which is not a secret.
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Credentials whose token endpoint will be discovered from `GET /auth/server`.
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            token_url: None,
            scopes: None,
        }
    }

    /// Pin the token endpoint instead of discovering it.
    pub fn with_token_url(mut self, url: Url) -> Self {
        self.token_url = Some(url);
        self
    }

    /// Request specific scopes.
    pub fn with_scopes(mut self, scopes: impl Into<String>) -> Self {
        self.scopes = Some(scopes.into());
        self
    }
}

/// A cached access token.
#[derive(Debug)]
struct CachedToken {
    value: String,
    /// When to stop using it. Refreshed early so a request never races the expiry.
    refresh_at: Instant,
}

/// Marks a client that acts as business logic.
#[derive(Debug, Clone, Copy)]
pub struct BusinessLogic;

/// Marks a client that acts as a virtual end node.
#[derive(Debug, Clone, Copy)]
pub struct VirtualEndNode;

/// Sealed marker for the two roles.
pub trait Role: Send + Sync + 'static {}
impl Role for BusinessLogic {}
impl Role for VirtualEndNode {}

/// Builds a [`Client`].
#[derive(Debug)]
pub struct ClientBuilder<R> {
    base: Url,
    credentials: Option<Credentials>,
    bearer_token: Option<String>,
    timeout: StdDuration,
    user_agent: String,
    adapters: AdapterChain,
    role: PhantomData<R>,
}

impl<R: Role> ClientBuilder<R> {
    /// Provide credentials. Without them the client sends unauthenticated requests, which is what a
    /// public tariff server expects.
    ///
    /// `[Def §VEN enrollment]` names the three things a VEN must let an end user configure — the
    /// VTN URL, the `clientID` and the `clientSecret` — and they are the three arguments of
    /// [`Client::builder`] and this method. Nothing here is compiled in or read from a fixed path,
    /// which is what makes reconfiguration the caller's to expose rather than this crate's to
    /// prevent.
    pub fn credentials(mut self, credentials: Credentials) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// Present a fixed bearer token instead of minting one.
    ///
    /// For deployments where the credential does not come from an OAuth2 exchange: a pre-shared
    /// token, or a gateway that has already authenticated the caller — Fluvius' NetFlex profile, for
    /// instance, authenticates with mutual TLS and does not use OAuth2 at all.
    pub fn bearer_token(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(token.into());
        self
    }

    /// Per-request timeout.
    pub fn timeout(mut self, timeout: StdDuration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Override the `User-Agent`.
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }

    /// Speak to a peer that bends the schema.
    ///
    /// Deployed profiles deviate — Fluvius' NetFlex sends `reportDescriptor.frequency` as an ISO
    /// duration where the schema says integer. An [adapter](crate::model::adapt) rewrites the JSON
    /// on the way out and back, so the deviation is a named, tested transformation at the edge
    /// rather than a hole in [`ReportDescriptor`](crate::model::ReportDescriptor) that every other
    /// deployment would also have to live with.
    ///
    /// A chain is a pipeline: adapters apply in the order they were added on the way **out**
    /// (canonical → the peer's shape) and in reverse on the way **in**, so that the chain undoes
    /// itself exactly.
    pub fn adapter(mut self, adapter: impl WireAdapter + 'static) -> Self {
        self.adapters = std::mem::take(&mut self.adapters).with(adapter);
        self
    }

    /// Finish.
    pub fn build(self) -> Result<Client<R>, ClientError> {
        // `reqwest` is built without a crypto provider of its own, so the process default has to
        // exist before a client does. See `crate::crypto`.
        crate::crypto::install_crypto_provider();
        let http = reqwest::Client::builder()
            .timeout(self.timeout)
            .user_agent(self.user_agent)
            .build()?;
        Ok(Client {
            inner: Arc::new(Inner {
                base: self.base,
                http,
                credentials: self.credentials,
                bearer_token: self.bearer_token,
                token: Mutex::new(None),
                adapters: self.adapters,
            }),
            role: PhantomData,
        })
    }
}

#[derive(Debug)]
struct Inner {
    base: Url,
    http: reqwest::Client,
    credentials: Option<Credentials>,
    bearer_token: Option<String>,
    token: Mutex<Option<CachedToken>>,
    adapters: AdapterChain,
}

/// A VTN client.
#[derive(Debug)]
pub struct Client<R> {
    inner: Arc<Inner>,
    role: PhantomData<R>,
}

impl<R> Clone for Client<R> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            role: PhantomData,
        }
    }
}

impl<R: Role> Client<R> {
    /// Start building a client against a VTN base URL.
    ///
    /// A trailing slash is added if missing: without one, joining a relative path would replace the
    /// last segment rather than extend it, which silently drops `/openadr3/3.1.0` from the URL.
    pub fn builder(base_url: &str) -> Result<ClientBuilder<R>, ClientError> {
        let mut base = base_url.to_string();
        if !base.ends_with('/') {
            base.push('/');
        }
        Ok(ClientBuilder {
            base: Url::parse(&base)?,
            credentials: None,
            bearer_token: None,
            timeout: StdDuration::from_secs(30),
            user_agent: concat!("openadr-rs/", env!("CARGO_PKG_VERSION")).to_string(),
            adapters: AdapterChain::new(),
            role: PhantomData,
        })
    }

    /// The base URL.
    pub fn base_url(&self) -> &Url {
        &self.inner.base
    }

    /// The `clientID` these credentials authenticate as, if the client has any.
    ///
    /// `None` for a pre-shared bearer token: the identity is inside the token and this client never
    /// looks. Anything that has to *name* the identity out of band — the MQTT username, which the
    /// VTN's broker callback derives authorization from `[Notifiers §12.2]` — needs it configured
    /// explicitly rather than guessed.
    pub fn client_id(&self) -> Option<&str> {
        self.inner.credentials.as_ref().map(Credentials::client_id)
    }

    /// Where this VTN says tokens come from.
    ///
    /// Sent without a token: the specification marks this endpoint unauthenticated, and it has to
    /// be — it is how a client discovers where to get a token in the first place.
    pub async fn auth_server(&self) -> Result<AuthServerInfo, ClientError> {
        self.send(Method::GET, "auth/server", None::<&()>, &[], None)
            .await
    }

    /// The VTN's own clock, from the `Date` header of an unauthenticated request.
    ///
    /// `None` when the VTN — or a proxy in front of it — sends no `Date`, which is legal and which
    /// a caller must treat as "unknown" rather than as agreement.
    ///
    /// Exists because every interval in OpenADR is an absolute instant, so a client whose clock is
    /// wrong acts at the wrong time and reports compliance it did not achieve. `Date` is the only
    /// reference the protocol offers, and second precision is enough for the question being asked.
    pub async fn server_time(&self) -> Result<Option<crate::model::Timestamp>, ClientError> {
        let (_, headers, _) = self
            .send_raw(Method::GET, "auth/server", None::<&()>, &[], None, None)
            .await?;
        let Some(date) = headers.get(header::DATE).and_then(|v| v.to_str().ok()) else {
            return Ok(None);
        };
        Ok(parse_http_date(date))
    }

    /// What push transports this VTN offers.
    pub async fn notifiers(&self) -> Result<NotifiersResponse, ClientError> {
        self.request(Method::GET, "notifiers", None::<&()>, &[])
            .await
    }

    /// MQTT topic names for a collection or a scoped path, e.g. `vens/ven-1/events`.
    pub async fn mqtt_topics(&self, path: &str) -> Result<TopicsResponse, ClientError> {
        self.request(
            Method::GET,
            &format!("notifiers/mqtt/topics/{path}"),
            None::<&()>,
            &[],
        )
        .await
    }

    /// A `GET` against an arbitrary path under the base URL, decoded as JSON.
    ///
    /// The escape hatch under the typed collections. It exists for the two things a typed API
    /// cannot serve: a VTN extension this crate does not model, and a tool that wants the VTN's
    /// bytes rather than this crate's reading of them — which is what makes `openadr get` usable
    /// for comparing two implementations.
    pub async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &Query,
    ) -> Result<T, ClientError> {
        self.request(Method::GET, path, None::<&()>, &query.pairs)
            .await
    }

    /// A `POST`, `PUT` or `DELETE` against an arbitrary path, with an optional JSON body.
    pub async fn send_json<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T, ClientError> {
        let method = Method::from_bytes(method.as_bytes())
            .map_err(|e| ClientError::Decode(format!("invalid HTTP method: {e}")))?;
        self.request(method, path, body, &[]).await
    }

    /// One HTTP exchange, with nothing interpreted.
    ///
    /// Status, headers and bytes exactly as they arrived — a non-2xx is a *response*, not an error.
    /// Every other method on this client turns a `4xx` into [`ClientError::Api`], which is right for
    /// a program acting on the result and wrong for one *checking* it: a conformance run asserts on
    /// the status, the `problem` body and the headers, and a helpful error type has already thrown
    /// two of the three away.
    ///
    /// Carries the client's token, if it has one. Extra headers are appended verbatim.
    pub async fn exchange(
        &self,
        method: &str,
        path: &str,
        query: &Query,
        body: Option<&serde_json::Value>,
        headers: &[(String, String)],
    ) -> Result<RawResponse, ClientError> {
        let method = Method::from_bytes(method.as_bytes())
            .map_err(|e| ClientError::Decode(format!("invalid HTTP method: {e}")))?;
        let token = self.access_token().await?;

        let mut url = self.inner.base.join(path)?;
        if !query.pairs.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in &query.pairs {
                pairs.append_pair(k, v);
            }
        }

        let mut request = self.inner.http.request(method, url);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(body) = body {
            request = request.json(body);
        }

        let response = request.send().await?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.bytes().await?.to_vec();
        Ok(RawResponse {
            status,
            headers,
            body,
        })
    }

    /// A conditional `GET` against an arbitrary path, as JSON.
    ///
    /// `None` when the VTN answers `304`, exactly as [`Collection::list_if_changed`].
    pub async fn list_json_if_changed<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &Query,
        etag: Option<&str>,
    ) -> Result<Option<Tagged<T>>, ClientError> {
        self.request_if_changed(path, &query.pairs, etag).await
    }

    /// Programmes.
    pub fn programs(&self) -> Collection<'_, R, Program, ProgramRequest> {
        Collection::new(self, "programs")
    }

    /// Events.
    pub fn events(&self) -> Collection<'_, R, Event, EventRequest> {
        Collection::new(self, "events")
    }

    /// Reports.
    pub fn reports(&self) -> Collection<'_, R, Report, ReportRequest> {
        Collection::new(self, "reports")
    }

    /// Subscriptions.
    pub fn subscriptions(&self) -> Collection<'_, R, Subscription, SubscriptionRequest> {
        Collection::new(self, "subscriptions")
    }

    /// VENs.
    pub fn vens(&self) -> Collection<'_, R, Ven, VenRequest> {
        Collection::new(self, "vens")
    }

    /// Resources.
    pub fn resources(&self) -> Collection<'_, R, Resource, ResourceRequest> {
        Collection::new(self, "resources")
    }

    /// A bearer token, minted or reused.
    ///
    /// Public because OpenADR itself sends this credential somewhere other than an HTTP header: the
    /// MQTT binding presents the access token as the broker password `[Notifiers §12.2]`. A caller
    /// doing that gets token caching and renewal for free instead of running a second OAuth2 client
    /// beside this one — and, more to the point, gets the *same* token, so revoking the credential
    /// closes both surfaces at once.
    ///
    /// `None` when the client has neither a pre-shared token nor credentials, which is the
    /// anonymous reader of a public tariff server.
    pub async fn access_token(&self) -> Result<Option<String>, ClientError> {
        // A fixed token wins: it is what the operator explicitly configured.
        if let Some(token) = &self.inner.bearer_token {
            return Ok(Some(token.clone()));
        }
        let Some(credentials) = self.inner.credentials.as_ref() else {
            return Ok(None);
        };

        if let Some(cached) = self
            .inner
            .token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            && Instant::now() < cached.refresh_at
        {
            return Ok(Some(cached.value.clone()));
        }

        // Discover the token endpoint if it was not pinned.
        let token_url = match &credentials.token_url {
            Some(u) => u.clone(),
            None => {
                let info = self.auth_server().await?;
                Url::parse(&info.token_url)?
            }
        };

        let mut form = vec![
            ("grant_type", "client_credentials"),
            ("client_id", credentials.client_id.as_str()),
            ("client_secret", credentials.client_secret.as_str()),
        ];
        if let Some(scopes) = &credentials.scopes {
            form.push(("scope", scopes.as_str()));
        }

        let response = self.inner.http.post(token_url).form(&form).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(ClientError::Auth(format!("{status}: {body}")));
        }

        let token: ClientCredentialResponse = response
            .json()
            .await
            .map_err(|e| ClientError::Auth(e.to_string()))?;

        // Refresh at 90% of the lifetime, and never trust a lifetime of zero.
        let lifetime = token.expires_in.unwrap_or(3600).max(1);
        let refresh_in = StdDuration::from_secs(lifetime * 9 / 10).max(StdDuration::from_secs(1));

        *self.inner.token.lock().unwrap_or_else(|e| e.into_inner()) = Some(CachedToken {
            value: token.access_token.clone(),
            refresh_at: Instant::now() + refresh_in,
        });
        Ok(Some(token.access_token))
    }

    /// Issue an authenticated request and decode the response.
    async fn request<B: Serialize, T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
        query: &[(String, String)],
    ) -> Result<T, ClientError> {
        let token = self.access_token().await?;
        self.send(method, path, body, query, token).await
    }

    /// Decode a response body, running the inbound adapter chain first.
    ///
    /// Every path that turns bytes into a typed value goes through here, including the conditional
    /// reads, which decode bytes of their own. Adapting in only one of them would give a client
    /// speaking to a peer that bends the schema the adapters on `list()` and not on
    /// `list_if_changed()`.
    fn decode<T: DeserializeOwned>(&self, bytes: &[u8]) -> Result<T, ClientError> {
        if self.inner.adapters.is_empty() {
            return serde_json::from_slice(bytes).map_err(|e| ClientError::Decode(e.to_string()));
        }
        let mut json: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|e| ClientError::Decode(e.to_string()))?;
        self.inner.adapters.inbound(&mut json);
        serde_json::from_value(json).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Issue a conditional request, returning `None` when the VTN answers `304`.
    async fn request_if_changed<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(String, String)],
        since: Option<&str>,
    ) -> Result<Option<Tagged<T>>, ClientError> {
        let token = self.access_token().await?;
        let (status, headers, bytes) = self
            .send_raw(Method::GET, path, None::<&()>, query, token, since)
            .await?;
        if status == StatusCode::NOT_MODIFIED {
            return Ok(None);
        }
        let etag = headers
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let value = self.decode(&bytes)?;
        Ok(Some(Tagged { value, etag }))
    }

    /// Issue a request with an explicit (possibly absent) token.
    ///
    /// Kept separate from [`Client::request`] so that token discovery cannot recurse into itself.
    async fn send<B: Serialize, T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
        query: &[(String, String)],
        token: Option<String>,
    ) -> Result<T, ClientError> {
        let (_, _, bytes) = self
            .send_raw(method, path, body, query, token, None)
            .await?;
        self.decode(&bytes)
    }

    /// The transport itself: everything above decodes what this returns.
    async fn send_raw<B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
        query: &[(String, String)],
        token: Option<String>,
        if_none_match: Option<&str>,
    ) -> Result<(StatusCode, header::HeaderMap, Vec<u8>), ClientError> {
        let mut url = self.inner.base.join(path)?;
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in query {
                pairs.append_pair(k, v);
            }
        }

        let mut request = self.inner.http.request(method, url);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(tag) = if_none_match {
            request = request.header(header::IF_NONE_MATCH, tag);
        }
        if let Some(body) = body {
            if self.inner.adapters.is_empty() {
                request = request.json(body);
            } else {
                // Serialise, let the adapters rewrite, then send. Going through `serde_json::Value`
                // costs one extra pass and is only paid by a client that asked for an adapter.
                let mut json =
                    serde_json::to_value(body).map_err(|e| ClientError::Decode(e.to_string()))?;
                self.inner.adapters.outbound(&mut json);
                request = request.json(&json);
            }
        }

        let response = request.send().await?;
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.bytes().await?.to_vec();

        if status == StatusCode::NOT_MODIFIED {
            return Ok((status, headers, bytes));
        }
        if !status.is_success() {
            // Every error carries a problem body; fall back to a synthetic one if this peer does not.
            let problem = serde_json::from_slice::<Problem>(&bytes).unwrap_or_else(|_| {
                Problem::new(
                    status.as_u16(),
                    "unexpected",
                    status.canonical_reason().unwrap_or("Error"),
                )
                .with_detail(String::from_utf8_lossy(&bytes).to_string())
            });
            return Err(ClientError::Api {
                status,
                problem: Box::new(problem),
            });
        }
        Ok((status, headers, bytes))
    }
}

/// Parse an RFC 9110 `Date` header — `Sun, 06 Nov 1994 08:49:37 GMT`.
///
/// Hand-written because `jiff` parses RFC 2822 and RFC 3339 and this is neither: it is IMF-fixdate,
/// which is RFC 2822 with a fixed field order and `GMT` where an offset belongs. Swapping `GMT`
/// for `+0000` makes it the former, which is one substitution against a dependency.
fn parse_http_date(value: &str) -> Option<crate::model::Timestamp> {
    let trimmed = value.trim();
    let rfc2822 = trimmed
        .strip_suffix("GMT")
        .map(|head| format!("{head}+0000"));
    let candidate = rfc2822.as_deref().unwrap_or(trimmed);
    jiff::fmt::rfc2822::parse(candidate)
        .ok()
        .map(|zoned| zoned.timestamp())
}

/// One HTTP response, uninterpreted. See [`Client::exchange`].
#[derive(Debug, Clone)]
pub struct RawResponse {
    /// The status, whatever it was.
    pub status: StatusCode,
    /// Every response header.
    pub headers: header::HeaderMap,
    /// The body bytes, possibly empty.
    pub body: Vec<u8>,
}

impl RawResponse {
    /// The body as JSON, or `None` if it is empty or not JSON.
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_slice(&self.body).ok()
    }

    /// The body as a `problem`, or `None` if it is not one.
    pub fn problem(&self) -> Option<Problem> {
        serde_json::from_slice(&self.body).ok()
    }

    /// One header, as a string.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// Whether the response carries a `problem+json` content type.
    pub fn is_problem_json(&self) -> bool {
        self.header("content-type")
            .is_some_and(|v| v.starts_with("application/problem+json"))
    }
}

/// A representation and the tag that identifies it.
///
/// OpenADR has no delta sync: a client that wants to know whether anything changed re-reads the
/// collection, and every deployment does exactly that on a timer. Keeping the tag turns the next
/// poll into a `304` with no body — see [`Collection::list_if_changed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tagged<T> {
    /// The decoded body.
    pub value: T,
    /// The `ETag` the VTN returned. Hand it back on the next read.
    ///
    /// `None` from a VTN that does not implement caching, in which case every read returns a body.
    pub etag: Option<String>,
}

/// Filters shared by every list endpoint.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pairs: Vec<(String, String)>,
}

impl Query {
    /// An empty query.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a target. Repeat for several.
    pub fn target(mut self, target: &Target) -> Self {
        self.pairs.push(("targets".into(), target.to_string()));
        self
    }

    /// Add several targets.
    pub fn targets(mut self, targets: &[Target]) -> Self {
        for t in targets {
            self.pairs.push(("targets".into(), t.to_string()));
        }
        self
    }

    /// Restrict to one programme.
    pub fn program(mut self, id: &ObjectId) -> Self {
        self.pairs.push(("programID".into(), id.to_string()));
        self
    }

    /// Restrict to one event, for `GET /reports`.
    pub fn event(mut self, id: &ObjectId) -> Self {
        self.pairs.push(("eventID".into(), id.to_string()));
        self
    }

    /// Restrict to one VEN, for `GET /resources`.
    pub fn ven(mut self, id: &ObjectId) -> Self {
        self.pairs.push(("venID".into(), id.to_string()));
        self
    }

    /// Look a programme up by name.
    ///
    /// Not declared by `openadr3.yaml`. Finding one tariff among hundreds otherwise means paging
    /// the whole collection, so this crate's VTN accepts it; another VTN may not, and answers as it
    /// would to any unknown query parameter.
    pub fn program_name(mut self, name: &crate::model::ProgramName) -> Self {
        self.pairs.push(("programName".into(), name.to_string()));
        self
    }

    /// Look a VEN up by name.
    pub fn ven_name(mut self, name: &crate::model::VenName) -> Self {
        self.pairs.push(("venName".into(), name.to_string()));
        self
    }

    /// Look a resource up by name, within its VEN.
    pub fn resource_name(mut self, name: &crate::model::ResourceName) -> Self {
        self.pairs.push(("resourceName".into(), name.to_string()));
        self
    }

    /// Restrict to one reporting or subscribing client.
    pub fn client_name(mut self, name: &crate::model::ClientName) -> Self {
        self.pairs.push(("clientName".into(), name.to_string()));
        self
    }

    /// Restrict subscriptions to those watching an object type.
    pub fn watching(mut self, object: crate::model::ObjectType) -> Self {
        self.pairs
            .push(("objects".into(), object.as_str().to_string()));
        self
    }

    /// Drop events whose intervals have elapsed.
    pub fn active(mut self, active: bool) -> Self {
        self.pairs.push(("active".into(), active.to_string()));
        self
    }

    /// Skip records, for pagination.
    pub fn skip(mut self, skip: usize) -> Self {
        self.pairs.push(("skip".into(), skip.to_string()));
        self
    }

    /// Limit the page size, capped by the schema at
    /// [`MAX_PAGE_LIMIT`](crate::model::MAX_PAGE_LIMIT).
    ///
    /// Worth setting even when the default would do: the schema gives `limit` a *maximum* and no
    /// *default*, so a VTN sent no `limit` may answer with a page of any size. A caller that infers
    /// "that was the whole collection" from a page shorter than 50 is reading its own assumption
    /// rather than the VTN's answer.
    pub fn limit(mut self, limit: usize) -> Self {
        self.pairs.push(("limit".into(), limit.to_string()));
        self
    }

    /// An arbitrary parameter, for VTN extensions such as `programName`.
    pub fn param(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.pairs.push((key.into(), value.into()));
        self
    }

    /// The parameters as they will be sent.
    pub fn pairs(&self) -> &[(String, String)] {
        &self.pairs
    }

    /// The first value for a key.
    pub fn value(&self, key: &str) -> Option<String> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    /// Drop every occurrence of a key.
    pub fn without(mut self, key: &str) -> Self {
        self.pairs.retain(|(k, _)| k != key);
        self
    }

    fn with_page(&self, skip: usize, limit: usize) -> Vec<(String, String)> {
        let mut pairs: Vec<(String, String)> = self
            .pairs
            .iter()
            .filter(|(k, _)| k != "skip" && k != "limit")
            .cloned()
            .collect();
        pairs.push(("skip".into(), skip.to_string()));
        pairs.push(("limit".into(), limit.to_string()));
        pairs
    }
}

/// Typed access to one collection.
#[derive(Debug)]
pub struct Collection<'c, R, T, Q> {
    client: &'c Client<R>,
    path: &'static str,
    _types: PhantomData<(T, Q)>,
}

impl<'c, R: Role, T: DeserializeOwned, Q: Serialize> Collection<'c, R, T, Q> {
    fn new(client: &'c Client<R>, path: &'static str) -> Self {
        Self {
            client,
            path,
            _types: PhantomData,
        }
    }

    /// One page of objects.
    pub async fn list(&self) -> Result<Vec<T>, ClientError> {
        self.list_with(&Query::new()).await
    }

    /// One page, filtered.
    pub async fn list_with(&self, query: &Query) -> Result<Vec<T>, ClientError> {
        self.client
            .request(Method::GET, self.path, None::<&()>, &query.pairs)
            .await
    }

    /// Every matching object, following pagination to the end.
    ///
    /// The specification caps a page at 50, so a collection of any size needs this. Pages are
    /// fetched sequentially because `skip`/`limit` has no cursor: a parallel fetch could miss or
    /// duplicate a record if the collection changes underneath it.
    pub async fn list_all(&self, query: &Query) -> Result<Vec<T>, ClientError> {
        const PAGE: usize = crate::model::MAX_PAGE_LIMIT;
        let mut out = Vec::new();
        let mut skip = 0usize;
        loop {
            let page: Vec<T> = self
                .client
                .request(
                    Method::GET,
                    self.path,
                    None::<&()>,
                    &query.with_page(skip, PAGE),
                )
                .await?;
            let received = page.len();
            out.extend(page);
            // Sound only because `with_page` *named* the limit: the schema caps `limit` but states
            // no default, so a short page is evidence of the end only when the reader chose the
            // page size.
            if received < PAGE {
                return Ok(out);
            }
            skip += PAGE;
        }
    }

    /// One page, or `None` if nothing has changed since `etag`.
    ///
    /// The counterpart of the VTN's `ETag`: pass the tag from the previous read and a poll that
    /// finds nothing new costs a `304` with no body rather than the whole collection again. OpenADR
    /// has no delta sync, so for a client that polls this is the difference between a kilobyte and
    /// a megabyte per cycle.
    ///
    /// A VTN with caching switched off never answers `304`, so this simply always returns the page.
    pub async fn list_if_changed(
        &self,
        query: &Query,
        etag: Option<&str>,
    ) -> Result<Option<Tagged<Vec<T>>>, ClientError> {
        self.client
            .request_if_changed(self.path, &query.pairs, etag)
            .await
    }

    /// One object, or `None` if it has not changed since `etag`.
    pub async fn get_if_changed(
        &self,
        id: &ObjectId,
        etag: Option<&str>,
    ) -> Result<Option<Tagged<T>>, ClientError> {
        self.client
            .request_if_changed(&format!("{}/{id}", self.path), &[], etag)
            .await
    }

    /// Fetch one object.
    pub async fn get(&self, id: &ObjectId) -> Result<T, ClientError> {
        self.client
            .request(
                Method::GET,
                &format!("{}/{id}", self.path),
                None::<&()>,
                &[],
            )
            .await
    }

    /// Fetch one object, or `None` if the VTN says it does not exist.
    ///
    /// A targeted object the caller may not see also reads as absent, which is deliberate.
    pub async fn try_get(&self, id: &ObjectId) -> Result<Option<T>, ClientError> {
        match self.get(id).await {
            Ok(v) => Ok(Some(v)),
            Err(ClientError::Api { status, .. }) if status == StatusCode::NOT_FOUND => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Writes that both roles may perform.
impl<R: Role, T: DeserializeOwned, Q: Serialize> Collection<'_, R, T, Q> {
    async fn create_inner(&self, request: &Q) -> Result<T, ClientError> {
        self.client
            .request(Method::POST, self.path, Some(request), &[])
            .await
    }

    async fn update_inner(&self, id: &ObjectId, request: &Q) -> Result<T, ClientError> {
        self.client
            .request(
                Method::PUT,
                &format!("{}/{id}", self.path),
                Some(request),
                &[],
            )
            .await
    }

    async fn delete_inner(&self, id: &ObjectId) -> Result<T, ClientError> {
        self.client
            .request(
                Method::DELETE,
                &format!("{}/{id}", self.path),
                None::<&()>,
                &[],
            )
            .await
    }
}

/// Programmes and events are business logic's to write.
macro_rules! bl_writes {
    ($t:ty, $q:ty) => {
        impl Collection<'_, BusinessLogic, $t, $q> {
            /// Create.
            pub async fn create(&self, request: &$q) -> Result<$t, ClientError> {
                self.create_inner(request).await
            }
            /// Replace.
            pub async fn update(&self, id: &ObjectId, request: &$q) -> Result<$t, ClientError> {
                self.update_inner(id, request).await
            }
            /// Delete.
            pub async fn delete(&self, id: &ObjectId) -> Result<$t, ClientError> {
                self.delete_inner(id).await
            }
        }
    };
}

bl_writes!(Program, ProgramRequest);
bl_writes!(Event, EventRequest);

/// Reports and subscriptions are the VEN's to write.
macro_rules! ven_writes {
    ($t:ty, $q:ty) => {
        impl Collection<'_, VirtualEndNode, $t, $q> {
            /// Create.
            pub async fn create(&self, request: &$q) -> Result<$t, ClientError> {
                self.create_inner(request).await
            }
            /// Replace.
            pub async fn update(&self, id: &ObjectId, request: &$q) -> Result<$t, ClientError> {
                self.update_inner(id, request).await
            }
            /// Delete.
            pub async fn delete(&self, id: &ObjectId) -> Result<$t, ClientError> {
                self.delete_inner(id).await
            }
        }
    };
}

ven_writes!(Report, ReportRequest);
ven_writes!(Subscription, SubscriptionRequest);

/// VENs and resources are writable by both roles, with different request bodies.
macro_rules! both_writes {
    ($t:ty, $q:ty) => {
        impl<R: Role> Collection<'_, R, $t, $q> {
            /// Create.
            pub async fn create(&self, request: &$q) -> Result<$t, ClientError> {
                self.create_inner(request).await
            }
            /// Replace.
            pub async fn update(&self, id: &ObjectId, request: &$q) -> Result<$t, ClientError> {
                self.update_inner(id, request).await
            }
            /// Delete.
            pub async fn delete(&self, id: &ObjectId) -> Result<$t, ClientError> {
                self.delete_inner(id).await
            }
        }
    };
}

both_writes!(Ven, VenRequest);
both_writes!(Resource, ResourceRequest);

/// Business logic may also delete reports, which is how orphaned data is cleaned up.
impl Collection<'_, BusinessLogic, Report, ReportRequest> {
    /// Delete a report.
    pub async fn delete(&self, id: &ObjectId) -> Result<Report, ClientError> {
        self.delete_inner(id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_trailing_slash_does_not_eat_the_base_path() {
        // `Url::join` replaces the last segment when the base has no trailing slash, which silently
        // turns `/openadr3/3.1.0` into nothing. This is a real and easily-missed footgun.
        let client = Client::<BusinessLogic>::builder("https://vtn.example.com/openadr3/3.1.0")
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            client.base_url().join("programs").unwrap().as_str(),
            "https://vtn.example.com/openadr3/3.1.0/programs"
        );
    }

    #[test]
    fn a_trailing_slash_is_left_alone() {
        let client = Client::<BusinessLogic>::builder("https://vtn.example.com/openadr3/3.1.0/")
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            client.base_url().join("events").unwrap().as_str(),
            "https://vtn.example.com/openadr3/3.1.0/events"
        );
    }

    #[test]
    fn queries_accumulate_repeated_targets() {
        let q = Query::new()
            .target(&Target::new("group1").unwrap())
            .target(&Target::new("group2").unwrap())
            .active(true);
        assert_eq!(q.pairs.len(), 3);
        assert_eq!(q.pairs.iter().filter(|(k, _)| k == "targets").count(), 2);
    }

    #[test]
    fn paging_replaces_rather_than_appends_bounds() {
        let q = Query::new().limit(10).skip(0);
        let paged = q.with_page(50, 50);
        assert_eq!(paged.iter().filter(|(k, _)| k == "limit").count(), 1);
        assert_eq!(paged.iter().find(|(k, _)| k == "skip").unwrap().1, "50");
    }

    #[test]
    fn credentials_never_print_their_secret() {
        let c = Credentials::new("id", "hunter2");
        let rendered = format!("{c:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("id"));
    }
}
