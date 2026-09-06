//! HTTP handlers.
//!
//! Handlers are thin on purpose: extract, authorize, call storage, apply object privacy, respond.
//! Every authorization decision routes through [`Ctx`] and [`crate::core::Access`], so the
//! rules cannot drift between one collection and the next.

use axum::{
    Json,
    body::Bytes,
    extract::{FromRequestParts, Path, RawQuery, State},
    http::{HeaderMap, Request, StatusCode, header, request::Parts},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::core::{Access, Grant, Role};
use crate::model::{AuthServerInfo, ObjectId, ObjectType, Problem, Target};
use crate::schema::{self, PayloadGroup, Policy};

use super::{
    ApiError, AppState,
    auth::{Scope, bearer_from_header},
    etag::Cached,
    notify::Fanout,
    store::Page,
};

pub mod broker;
pub mod events;
pub mod notifiers;
pub mod programs;
pub mod reports;
pub mod resources;
pub mod subscriptions;
pub mod vens;

/// The caller, resolved once per request.
///
/// The caller's *grant* — the union of the targets on its VEN and resource objects — is resolved
/// lazily, because most requests do not need it. Ownership is decided by `clientID` alone, and only
/// `program` and `event` reads are gated by targeting, so a `POST /reports` from a fleet of VENs
/// (the write path a real deployment saturates first) pays no lookup at all.
#[derive(Debug)]
pub struct Ctx {
    /// The authenticated principal.
    pub principal: super::auth::Principal,
    /// Whether this VTN serves unauthenticated readers.
    anonymous_allowed: bool,
    /// Everything needed to resolve the grant, if it turns out to be needed.
    state: AppState,
    role: tokio::sync::OnceCell<Role>,
}

impl Ctx {
    /// Whether the caller is business logic.
    ///
    /// From the scopes, which is where the specification puts the distinction — and which needs no
    /// database.
    pub fn is_business_logic(&self) -> bool {
        self.principal.is_business_logic()
    }

    /// The privacy role, resolving the caller's grant on first use.
    pub async fn role(&self) -> Role {
        self.role
            .get_or_init(|| async {
                match (&self.principal.client_id, self.is_business_logic()) {
                    (_, true) => Role::BusinessLogic,
                    (Some(client_id), false) => {
                        // Fail closed: a client whose grant cannot be read sees only untargeted
                        // objects, rather than seeing everything or nothing at random.
                        let grant = self
                            .state
                            .storage
                            .grant_for(client_id)
                            .await
                            .unwrap_or_else(|e| {
                                tracing::error!(
                                    error = %e, %client_id,
                                    "grant lookup failed; denying targets"
                                );
                                Grant::empty()
                            });
                        Role::Ven {
                            client_id: client_id.clone(),
                            grant,
                        }
                    }
                    (None, false) => Role::Anonymous,
                }
            })
            .await
            .clone()
    }

    /// Fail unless the caller holds a scope.
    pub fn require(&self, scope: Scope) -> Result<(), ApiError> {
        if self.principal.scopes.contains(scope) {
            Ok(())
        } else {
            Err(ApiError::MissingScope(scope))
        }
    }

    /// Fail unless the caller may read at all.
    ///
    /// Business logic reads with `read_all`; a VEN with `read_targets` or `read_ven_objects`; an
    /// anonymous caller only on a VTN that admits them.
    ///
    /// `read_bl` opens this gate and nothing beyond it. It is the notifier-metadata scope, not an
    /// identity: a credential carrying only `read_bl` passes here and is then read as an ordinary
    /// identified client, so object privacy applies to it in full (D-121). What it uniquely buys is
    /// [`Scope::ReadBl`] on the five collection-topic endpoints, which ask for it by name.
    pub fn require_read(&self) -> Result<(), ApiError> {
        if self.principal.scopes.contains_any(&[
            Scope::ReadAll,
            Scope::ReadBl,
            Scope::ReadTargets,
            Scope::ReadVenObjects,
        ]) {
            return Ok(());
        }
        if self.principal.client_id.is_none() && self.anonymous_allowed {
            return Ok(());
        }
        Err(ApiError::MissingScope(Scope::ReadTargets))
    }

    /// The caller's client id, or a 403 if it has none.
    pub fn client_id(&self) -> Result<&crate::model::ClientId, ApiError> {
        self.principal.client_id.as_ref().ok_or_else(|| {
            ApiError::Forbidden("this operation requires an identified client".into())
        })
    }

    /// Access for a collection read, to be handed to the storage query.
    pub async fn list_access(&self, requested: Vec<Target>) -> Access {
        Access::list(self.role().await, requested)
    }

    /// Access for a read by id.
    pub async fn id_access(&self) -> Access {
        Access::by_id(self.role().await)
    }

    /// Access for a read of an *owned* collection — `ven`, `resource`, `report`, `subscription`.
    ///
    /// Visibility there is decided by `clientID`, never by targeting `[Def §Object Privacy]`, so
    /// this needs no grant and issues no query. `?targets=` still filters, as an ordinary additive
    /// parameter.
    pub fn owned_access(&self, requested: Vec<Target>) -> Access {
        let role = match &self.principal.client_id {
            _ if self.is_business_logic() => Role::BusinessLogic,
            Some(client_id) => Role::Ven {
                client_id: client_id.clone(),
                grant: Grant::empty(),
            },
            None => Role::Anonymous,
        };
        Access::list(role, requested)
    }

    /// Access for a read of one owned object by id.
    pub fn owned_id_access(&self) -> Access {
        let mut access = self.owned_access(Vec::new());
        access = Access::by_id(access.role().clone());
        access
    }
}

impl FromRequestParts<AppState> for Ctx {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let bearer = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        let principal = state
            .authenticator
            .authenticate(bearer_from_header(bearer))
            .await?;

        Ok(Ctx {
            principal,
            anonymous_allowed: state.authenticator.allows_anonymous(),
            state: state.clone(),
            role: tokio::sync::OnceCell::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// Query parameters
// ---------------------------------------------------------------------------

/// A parsed query string.
///
/// Written by hand rather than derived because the specification's list-valued parameters arrive in
/// two shapes in the wild — `?targets=a&targets=b` and `?targets=a,b` — and both must work.
#[derive(Debug, Default, Clone)]
pub struct Params(Vec<(String, String)>);

impl Params {
    /// Parse a raw query string, or a form body, which use the same encoding.
    pub fn parse(raw: Option<&str>) -> Self {
        let Some(raw) = raw else {
            return Self::default();
        };
        Self(
            url::form_urlencoded::parse(raw.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
        )
    }

    /// The first value for a key.
    pub fn first(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Every value for a key, splitting comma-separated lists.
    pub fn all(&self, key: &str) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(k, _)| k == key)
            .flat_map(|(_, v)| v.split(','))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Parse a single value.
    pub fn parse_one<T>(&self, key: &str) -> Result<Option<T>, ApiError>
    where
        T: core::str::FromStr,
        T::Err: core::fmt::Display,
    {
        self.first(key)
            .map(|v| {
                v.parse::<T>()
                    .map_err(|e| ApiError::BadRequest(format!("invalid {key}: {e}")))
            })
            .transpose()
    }

    /// The `targets` parameter.
    pub fn targets(&self) -> Result<Vec<Target>, ApiError> {
        self.all("targets")
            .into_iter()
            .map(|t| {
                Target::new(t).map_err(|e| ApiError::BadRequest(format!("invalid target: {e}")))
            })
            .collect()
    }

    /// The `skip` and `limit` parameters, validated against the schema's bounds.
    pub fn page(&self) -> Result<Page, ApiError> {
        let skip = self.parse_one::<i64>("skip")?.unwrap_or(0);
        if skip < 0 {
            return Err(ApiError::BadRequest("skip must not be negative".into()));
        }
        let limit = self
            .parse_one::<i64>("limit")?
            .unwrap_or(Page::MAX_LIMIT as i64);
        if !(0..=Page::MAX_LIMIT as i64).contains(&limit) {
            return Err(ApiError::BadRequest(format!(
                "limit must be between 0 and {}",
                Page::MAX_LIMIT
            )));
        }
        Ok(Page {
            skip: skip as usize,
            limit: limit as usize,
        })
    }

    /// The `active` parameter.
    pub fn active(&self) -> Result<Option<bool>, ApiError> {
        self.parse_one::<bool>("active")
    }

    /// The `objects` parameter, for subscription queries.
    pub fn object_types(&self) -> Result<Vec<ObjectType>, ApiError> {
        self.all("objects")
            .into_iter()
            .map(|o| {
                o.parse::<ObjectType>()
                    .map_err(|_| ApiError::BadRequest(format!("unknown object type {o:?}")))
            })
            .collect()
    }
}

/// Extract the query parameters of a request.
pub fn params(raw: &Option<String>) -> Params {
    Params::parse(raw.as_deref())
}

// ---------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------

/// The media types a JSON request body may be labelled with.
///
/// `application/json` and the `+json` structured suffix (RFC 6839), because a profile may label a
/// body `application/openadr3+json` and it is still JSON.
pub const JSON_MEDIA_TYPES: &str = "application/json or application/*+json";

/// The media types `POST /auth/token` accepts.
///
/// RFC 6749 §4.4.2 says form encoding; enough clients send JSON that refusing it buys nothing, and
/// the endpoint already reads both `[D-103]`.
pub const TOKEN_MEDIA_TYPES: &str = "application/x-www-form-urlencoded or application/json";

/// Whether a `Content-Type` names a JSON body.
fn is_json_media_type(value: &str) -> bool {
    // Parameters are not part of the identity: `application/json; charset=utf-8` is JSON.
    let essence = value.split(';').next().unwrap_or("").trim();
    essence.eq_ignore_ascii_case("application/json")
        || essence.split_once('/').is_some_and(|(_, sub)| {
            sub.len() > 5 && sub[sub.len() - 5..].eq_ignore_ascii_case("+json")
        })
}

/// Refuse a body whose `Content-Type` this endpoint cannot read.
///
/// A *wrong* claim is refused; **no** claim is tolerated. Those are different mistakes: a body
/// labelled `application/x-www-form-urlencoded` was never going to parse as JSON and saying so is
/// the whole value of `415`, whereas a client that sent no `Content-Type` has asserted nothing and
/// refusing it costs interoperability to gain a lecture. RFC 9110 §8.3 leaves the unlabelled case
/// to the recipient for exactly this reason.
fn require_media_type(
    headers: &HeaderMap,
    accepted: &'static str,
    admits: fn(&str) -> bool,
) -> Result<(), ApiError> {
    let Some(value) = headers.get(header::CONTENT_TYPE) else {
        return Ok(());
    };
    let Ok(value) = value.to_str() else {
        return Err(ApiError::UnsupportedMediaType(format!(
            "the Content-Type header is not valid text; this endpoint reads {accepted}"
        )));
    };
    if value.trim().is_empty() || admits(value) {
        return Ok(());
    }
    Err(ApiError::UnsupportedMediaType(format!(
        "a {value:?} body cannot be read by this endpoint, which reads {accepted}"
    )))
}

/// Refuse a token request whose `Content-Type` is neither form encoding nor JSON.
pub fn require_token_media_type(headers: &HeaderMap) -> Result<(), ApiError> {
    require_media_type(headers, TOKEN_MEDIA_TYPES, |value| {
        let essence = value.split(';').next().unwrap_or("").trim();
        essence.eq_ignore_ascii_case("application/x-www-form-urlencoded")
            || is_json_media_type(value)
    })
}

/// A request body that must be labelled as JSON, or not labelled at all.
///
/// One implementation of the media-type rule, at the extractor rather than in twelve handlers —
/// which is the same reason `Access` is one type: a rule stated once per endpoint is a rule with
/// one endpoint's worth of chances to be forgotten, and the failure here is silent (a form-encoded
/// body reported as malformed JSON).
#[derive(Debug, Clone)]
pub struct JsonBody(Bytes);

impl JsonBody {
    /// Parse the body into a request type.
    pub fn parse<T: serde::de::DeserializeOwned>(&self) -> Result<T, ApiError> {
        parse_body(&self.0)
    }

    /// The raw bytes.
    pub fn bytes(&self) -> &Bytes {
        &self.0
    }
}

impl<S: Send + Sync> axum::extract::FromRequest<S> for JsonBody {
    type Rejection = ApiError;

    async fn from_request(
        request: Request<axum::body::Body>,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        require_media_type(request.headers(), JSON_MEDIA_TYPES, is_json_media_type)?;
        Bytes::from_request(request, state).await.map(Self).map_err(
            // The status the rejection carries, not a fixed one: a body that ran off the end of the
            // limited stream is a `413`, and reporting it as a `400` blames the JSON for a length.
            |e| match e.status() {
                StatusCode::PAYLOAD_TOO_LARGE => ApiError::PayloadTooLarge(
                    "the request body exceeds this VTN's limit".to_string(),
                ),
                _ => ApiError::BadRequest(format!("the request body could not be read: {e}")),
            },
        )
    }
}

/// Parse a request body, reporting the offending field rather than "invalid JSON".
///
/// `[Def §Required and optional properties]` and `[Def §Message validation]`: a body missing a
/// required property is a `400` and creates nothing. Both fall out of the request types having no
/// `Option` where the schema has no default — serde refuses the body, and the message names the
/// field it was looking for, which is the part a `400` is worth anything for.
pub fn parse_body<T: serde::de::DeserializeOwned>(bytes: &Bytes) -> Result<T, ApiError> {
    if bytes.is_empty() {
        return Err(ApiError::BadRequest("a request body is required".into()));
    }
    serde_json::from_slice(bytes).map_err(|e| {
        ApiError::BadRequest(format!(
            "malformed request body at line {} column {}: {}",
            e.line(),
            e.column(),
            e
        ))
    })
}

/// Check an optional attribute list, which is the shape `program`, `ven` and `resource` use.
///
/// A thin wrapper, and it exists so no handler has to remember that `attributes: null` and
/// `attributes: []` mean the same thing. Programme attributes and VEN/resource attributes are
/// separate enumerations, so the group says which.
pub fn check_attributes(
    state: &AppState,
    attributes: Option<&Vec<crate::model::ValuesMap>>,
    group: PayloadGroup,
) -> Result<(), ApiError> {
    match attributes {
        Some(values) => check_payloads(state, values, group),
        None => Ok(()),
    }
}

/// Check payload values against the enumerations, honouring the configured policy.
pub fn check_payloads(
    state: &AppState,
    payloads: &[crate::model::ValuesMap],
    group: PayloadGroup,
) -> Result<(), ApiError> {
    if state.config.payload_policy == Policy::Off {
        return Ok(());
    }
    let violations = schema::validate_all(payloads, group);
    if violations.is_empty() {
        return Ok(());
    }
    match state.config.payload_policy {
        Policy::Strict => Err(ApiError::InvalidPayload(violations)),
        _ => {
            for v in &violations {
                tracing::warn!(violation = %v, "payload does not match its enumeration");
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// A `200` with an entity tag.
pub fn ok<T: serde::Serialize>(
    state: &AppState,
    headers: &HeaderMap,
    value: &T,
) -> Result<Response, ApiError> {
    Ok(Cached::json(value, headers, state.config.http_caching)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .into_response())
}

/// A `201` with the created object.
pub fn created<T: serde::Serialize>(value: &T) -> Result<Response, ApiError> {
    Ok((StatusCode::CREATED, Json(value)).into_response())
}

// ---------------------------------------------------------------------------
// Privacy helpers
// ---------------------------------------------------------------------------

/// Rewrite the targets on objects the storage layer has already admitted.
///
/// Cardinality-preserving by construction: the storage query applied the same [`Access`] before it
/// paginated, so everything here is visible and only its target list is narrowed. Dropping records
/// at this point would silently shorten a page.
pub fn hide_targets<T, F, S>(access: &Access, items: &mut [T], get: F, set: S)
where
    F: Fn(&T) -> &[Target],
    S: Fn(&mut T, Vec<Target>),
{
    for item in items {
        if let Some(visible) = access.visible_targets(get(item)) {
            set(item, visible);
        }
    }
}

/// Apply target hiding to a single object, or report it as missing.
///
/// A hidden object is reported as `404`, never `403`: a `403` would confirm that the object exists,
/// which is exactly what targeting conceals.
pub fn require_visible<T, F, S>(
    access: &Access,
    mut item: T,
    object_type: ObjectType,
    id: &ObjectId,
    get: F,
    set: S,
) -> Result<T, ApiError>
where
    F: Fn(&T) -> &[Target],
    S: Fn(&mut T, Vec<Target>),
{
    match access.visible_targets(get(&item)) {
        Some(visible) => {
            set(&mut item, visible);
            Ok(item)
        }
        None => Err(ApiError::NotFound {
            object_type,
            id: id.clone(),
        }),
    }
}

/// Refuse an owned object that is not the caller's, as `404` for the same reason.
pub fn require_owner(
    access: &Access,
    owner: Option<&crate::model::ClientId>,
    object_type: ObjectType,
    id: &ObjectId,
) -> Result<(), ApiError> {
    if access.owns(owner) {
        Ok(())
    } else {
        Err(ApiError::NotFound {
            object_type,
            id: id.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// Notification plumbing
// ---------------------------------------------------------------------------

/// Take the snapshot a write to a **targeted** object needs — `program` and `event`.
///
/// Called *before* the write, and handed to the storage method, which computes the deliveries and
/// inserts them in the same transaction as the change. Doing it this way is what makes the
/// change and the record that it must be announced commit together: a crash cannot leave an event
/// created and nobody told, which is a failure OpenADR has no way to signal afterwards.
///
/// Whether a given VEN may hear about a targeted object is decided by *that VEN's* grant, so this
/// reading of the snapshot has to sweep the whole fleet. [`fanout_owned`] is the other reading, and
/// the difference between them is the difference between one indexed lookup and every VEN in the
/// VTN (D-124).
///
/// When there is nothing subscribed and no broker, [`Fanout::is_empty`] short-circuits everything
/// downstream.
pub async fn fanout(state: &AppState, object_type: ObjectType) -> Fanout {
    snapshot(state, object_type, Gate::Targeted).await
}

/// Take the snapshot a write to an **owned** object needs — `ven`, `resource`, `report` and
/// `subscription`.
///
/// Visibility of these is decided by `clientID` alone `[Def §Object Privacy]`, and no grant enters
/// into it: a webhook subscriber hears about one only if it owns it, and exactly one VEN topic
/// carries it — its owner's. So the snapshot is that one VEN, resolved by an indexed lookup, rather
/// than the fleet sweep [`fanout`] performs. `POST /reports` is the highest-rate write in the
/// system, and this is what keeps its cost independent of how many VENs the VTN has (D-124).
///
/// `owner` is `None` only where the object genuinely has none — a report filed before `clientID`
/// stamping — and such an object reaches no VEN topic, exactly as it reaches no VEN.
pub async fn fanout_owned(
    state: &AppState,
    object_type: ObjectType,
    owner: Option<&crate::model::ClientId>,
) -> Fanout {
    debug_assert!(
        matches!(
            object_type,
            ObjectType::Ven | ObjectType::Resource | ObjectType::Report | ObjectType::Subscription
        ),
        "{object_type} is gated by targeting, not by ownership; use `fanout`"
    );
    snapshot(state, object_type, Gate::Owned(owner)).await
}

/// Which rule will decide who hears, and therefore what the snapshot has to carry.
#[derive(Debug, Clone, Copy)]
enum Gate<'a> {
    /// Targeting: every VEN's grant is a possible admission, so all of them are needed.
    Targeted,
    /// Ownership: only this client's VEN is, so only its row is.
    Owned(Option<&'a crate::model::ClientId>),
}

async fn snapshot(state: &AppState, object_type: ObjectType, gate: Gate<'_>) -> Fanout {
    // Narrowed by the database — `objectOperations.objects` is indexed on both SQL backends — so a
    // `POST /reports` from a fleet of VENs does not walk every event subscription in the VTN.
    //
    // `announces` rather than `object_type`, because a delete announces its cascade too: dropping a
    // programme tells a subscriber watching only REPORT deletes about every report that went with
    // it. Narrowing to the written type alone would silence exactly those subscribers silently.
    //
    // Its own query rather than `list_subscriptions`, because it needs one thing the API does not:
    // what *kind* of client owns each subscription (`OwnerKind`).
    let subscriptions = match state.storage.subscribers(object_type.announces()).await {
        Ok(s) => s,
        Err(e) => {
            // Fail open on the fan-out rather than on the write: a subscriber that is not told is
            // recoverable by polling, a write that is refused is not.
            tracing::error!(error = %e, "could not load subscriptions for notification");
            Vec::new()
        }
    };

    let topics = state
        .config
        .mqtt
        .is_some()
        .then(|| super::notify::Topics::new(state.config.mqtt_topic_prefix.clone()));

    // Subscribers the breaker has cut off are dropped before anything else is done for them: a
    // permanently dead endpoint would otherwise cost `max_attempts` HTTP round trips per write for
    // ever, and abandonment is per-notification and cannot see the pattern (D-096). In a working
    // VTN this query returns no rows.
    let now = state.clock.now();
    let subscriptions = match state.storage.subscriber_health().await {
        Ok(health) if !health.is_empty() => {
            let cut_off: Vec<&ObjectId> = health
                .iter()
                .filter(|h| h.is_cut_off(now))
                .map(|h| &h.subscription_id)
                .collect();
            subscriptions
                .into_iter()
                .filter(|s| {
                    let suppressed = cut_off.contains(&&s.subscription.id);
                    if suppressed {
                        tracing::debug!(
                            subscription = %s.subscription.id,
                            "subscriber is cut off; not queueing"
                        );
                    }
                    !suppressed
                })
                .collect()
        }
        Ok(_) => subscriptions,
        Err(e) => {
            // Fail *open*: a breaker that cannot be read must not silence every subscriber.
            tracing::error!(error = %e, "could not read subscriber health; not suppressing anyone");
            subscriptions
        }
    };

    // The VEN index is only worth reading when something will read it: the broker fan-out walks it
    // by construction, and a webhook subscriber's visibility of a *targeted* object is decided by
    // its grant. With neither, a VTN that has no subscribers and no broker — which is most of them,
    // most of the time — pays nothing on the write path at all.
    if subscriptions.is_empty() && topics.is_none() {
        return Fanout::none();
    }

    let grants = match gate {
        // One sweep of the grants, not one lookup per subscriber.
        Gate::Targeted => state.storage.all_grants().await.unwrap_or_default(),
        // One row, or none. An owned object reaches its owner's VEN topic and no other, and no
        // grant decides anything about it — so the fleet is not merely unnecessary here, it is
        // unread.
        Gate::Owned(None) => Vec::new(),
        Gate::Owned(Some(client_id)) => match state.storage.get_ven_by_client(client_id).await {
            Ok(Some(ven)) => vec![(ven.id, ven.client_id, Grant::empty())],
            // A client with no VEN object owns no VEN topic. Its webhook subscriptions are
            // unaffected: ownership is decided from the object, not from this index.
            Ok(None) => Vec::new(),
            Err(e) => {
                tracing::error!(error = %e, %client_id, "could not resolve the owner's VEN");
                Vec::new()
            }
        },
    };
    let by_client: std::collections::BTreeMap<_, _> = grants
        .iter()
        .map(|(_, client_id, grant)| (client_id.clone(), grant.clone()))
        .collect();

    let subscribers = subscriptions
        .into_iter()
        .map(|subscriber| {
            let role = if subscriber.owner.is_business_logic() {
                // Business logic reads every object whatever its targets `[Def §Object Privacy]`,
                // and a subscription is a standing read. Evaluating it as a VEN gave it an empty
                // grant, which admits untargeted objects and nothing else — so a utility's own
                // integration heard about none of its own targeted events, silently.
                Role::BusinessLogic
            } else {
                Role::Ven {
                    client_id: subscriber.subscription.client_id.clone(),
                    grant: by_client
                        .get(&subscriber.subscription.client_id)
                        .cloned()
                        .unwrap_or_else(Grant::empty),
                }
            };
            (subscriber.subscription, role)
        })
        .collect();

    Fanout::new(subscribers, grants, topics, now)
}

// ---------------------------------------------------------------------------
// Standalone endpoints
// ---------------------------------------------------------------------------

/// `GET /health` — liveness, storage reachability and notification backlog.
///
/// Not a specification endpoint. It reports the outbox because that is the one piece of VTN state
/// whose degradation is otherwise invisible: an endpoint that stopped answering shows up as a
/// pending count that stops falling, and nothing else in the API would say so.
pub async fn health(State(state): State<AppState>) -> Response {
    if !state.storage.healthy().await {
        return ApiError::Storage(super::store::StorageError::Unavailable(
            "backend not reachable".into(),
        ))
        .into_response();
    }
    // Read through the same function `GET /metrics` uses, so the two cannot answer the same
    // question differently. A cut-off subscriber produces no queue entries at all, so it is
    // invisible in `outbox` by construction — counting it is what stops "the queue is empty" from
    // meaning two different things.
    let gauges = super::metrics::gauges(&state).await;
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::json!({
            "status": "ok",
            "outbox": gauges.outbox,
            "subscribersCutOff": gauges.subscribers_cut_off,
            // `report` is the only object that grows without bound, and the age of the oldest is
            // how an operator tells "retention is working" from "retention is configured and the
            // sweeper is not running".
            "reports": gauges.reports,
        })
        .to_string(),
    )
        .into_response()
}

/// `GET /admin/outbox` — the notifications the VTN gave up on.
///
/// Not a specification endpoint, and the smaller half of an operational loop `GET /health` only
/// starts: `dead: 4` is a number to alert on, and this is the list to act on. Business logic's,
/// because a dead letter names a subscriber's callback URL.
///
/// `?limit=` bounds the listing; the default is 50, the same page size the rest of the API uses.
pub async fn admin_outbox(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    ctx.require(Scope::ReadAll)?;
    let page = params(&raw).page()?;
    let entries = state.storage.dead_letters(page.limit.max(1)).await?;
    ok(&state, &headers, &entries)
}

/// `GET /admin/subscribers` — the subscriptions whose deliveries are failing.
///
/// The companion to `GET /admin/outbox`, and the one that answers *why* the dead letters stopped
/// arriving: a subscriber cut off by the circuit breaker produces no new queue entries at all, so
/// its silence is invisible from the outbox alone. Business logic's, because a row names a
/// subscription id and the error its endpoint returned.
pub async fn admin_subscribers(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ctx.require(Scope::ReadAll)?;
    let health = state.storage.subscriber_health().await?;
    ok(&state, &headers, &health)
}

/// `POST /admin/outbox/retry` — make every abandoned notification due again.
///
/// What an operator does after fixing the receiver that was refusing them. Without it the only way
/// back is an `UPDATE` by hand, and the alternative — retrying for ever — is how a permanently dead
/// endpoint costs `max_attempts` on every change in the VTN.
pub async fn admin_outbox_retry(
    State(state): State<AppState>,
    ctx: Ctx,
) -> Result<Response, ApiError> {
    ctx.require(Scope::WriteSubscriptions)?;
    if !ctx.is_business_logic() {
        return Err(ApiError::Forbidden(
            "the notification queue is business logic's".into(),
        ));
    }
    let revived = state.storage.revive_dead(state.clock.now()).await?;
    // And close every breaker. Reviving the backlog without doing so would revive it and then
    // immediately stop adding to it, which is half a recovery — and the operator pressing this
    // button is saying the endpoint is fixed.
    let restored = state.storage.clear_subscriber_health().await?;
    tracing::info!(
        revived,
        restored,
        "abandoned notifications made due again and every cut-off subscriber restored"
    );
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({ "revived": revived, "subscribersRestored": restored }).to_string(),
    )
        .into_response())
}

/// `GET /auth/server` — where to exchange credentials for a token.
///
/// Required even when the VTN issues no tokens itself: `[Def §Token endpoint discovery]` says a VTN
/// that expects its VENs to have been provisioned out of band **still** implements at least this
/// endpoint, and may implement `POST /auth/token` or not. So this is unconditional and the grant is
/// the optional half.
pub async fn auth_server(State(state): State<AppState>) -> Json<AuthServerInfo> {
    Json(AuthServerInfo {
        token_url: state.authenticator.token_url(),
    })
}

/// `POST /auth/token` — the OAuth2 client-credentials grant, when this VTN runs one.
///
/// Optional `[API /auth/token]`: a VTN that delegates to an external authorization service answers
/// `501` and points at `GET /auth/server`, which is what the default
/// [`Authenticator`](super::auth::Authenticator) does. With
/// [`InternalAuth`](super::auth::InternalAuth) configured, this is a real grant.
///
/// The body is `application/x-www-form-urlencoded` per RFC 6749, and JSON is accepted as well
/// because enough clients send it and rejecting it buys nothing.
///
/// Errors are RFC 6749 §5.2 `authError` bodies with a `400`, including `invalid_client`. §5.2
/// requires `401` only when the client authenticated through the `Authorization` header; here the
/// credentials are in the body, and `400` is what the OpenAPI document's `badRequestOAuth` names.
///
/// Every answer carries `Cache-Control: no-store`, which RFC 6749 §5.1 requires of *any* response
/// containing a token. Applied to the refusals too: the header is a property of the endpoint, and
/// one that appears only on success is one a proxy has to reason about.
pub async fn auth_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    no_store(auth_token_inner(state, headers, body).await)
}

/// Stamp `Cache-Control: no-store` on a token response `[RFC 6749 §5.1]`.
fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

async fn auth_token_inner(state: AppState, headers: HeaderMap, body: Bytes) -> Response {
    if !state.authenticator.issues_tokens() {
        return ApiError::NotImplemented(super::auth::TokenError::NotIssuing.to_string())
            .into_response();
    }
    if let Err(e) = require_token_media_type(&headers) {
        return e.into_response();
    }
    let Some(request) = parse_credentials(&body) else {
        return oauth_error(crate::model::OAuthError::new(
            crate::model::OAuthErrorKind::InvalidRequest,
            "expected grant_type, client_id and client_secret",
        ));
    };
    match state.authenticator.issue_token(&request).await {
        Ok(token) => (StatusCode::OK, Json(token)).into_response(),
        Err(super::auth::TokenError::NotIssuing) => {
            ApiError::NotImplemented(super::auth::TokenError::NotIssuing.to_string())
                .into_response()
        }
        Err(super::auth::TokenError::Refused(e)) => oauth_error(e),
        // A `503`, not a `400`: the credential may be perfectly good and the VTN could not look.
        // Telling a client its secret is wrong when the VTN is merely busy sends an operator
        // hunting for a credential problem that does not exist.
        Err(e @ super::auth::TokenError::Unavailable(_)) => {
            ApiError::Unavailable(e.to_string()).into_response()
        }
    }
}

/// RFC 6749 §5.2: an error body, not a `problem` body — the token endpoint is OAuth2's, not ours.
fn oauth_error(error: crate::model::OAuthError) -> Response {
    (StatusCode::BAD_REQUEST, Json(error)).into_response()
}

/// Read a client-credentials request from a form body, or from JSON.
fn parse_credentials(body: &Bytes) -> Option<crate::model::ClientCredentialRequest> {
    if let Ok(request) = serde_json::from_slice::<crate::model::ClientCredentialRequest>(body) {
        return Some(request);
    }
    let form = Params::parse(Some(core::str::from_utf8(body).ok()?));
    Some(crate::model::ClientCredentialRequest {
        grant_type: form.first("grant_type")?.to_string(),
        client_id: form.first("client_id")?.to_string(),
        client_secret: form.first("client_secret")?.to_string(),
        scope: form.first("scope").map(str::to_string),
    })
}

/// Fallback for unrouted paths.
pub async fn not_found() -> ApiError {
    ApiError::NoSuchRoute
}

/// Header carrying the id that ties a client's complaint to a line in the log.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// What a layer-produced status means, for the `problem` body written in its place.
///
/// Only statuses a *layer* can produce are here: a handler returns [`ApiError`], which already
/// carries its own slug and detail. Anything not listed falls back to the status' own reason
/// phrase, so a layer added tomorrow still produces a `problem` rather than a bare body.
fn layer_problem(status: StatusCode) -> (&'static str, &'static str) {
    match status {
        StatusCode::METHOD_NOT_ALLOWED => (
            "method-not-allowed",
            "this method is not defined for this resource",
        ),
        StatusCode::PAYLOAD_TOO_LARGE => (
            "payload-too-large",
            "the request body exceeds this VTN's limit",
        ),
        StatusCode::GATEWAY_TIMEOUT => (
            "timeout",
            "the VTN took too long to answer and gave up on this request",
        ),
        StatusCode::UNSUPPORTED_MEDIA_TYPE => (
            "unsupported-media-type",
            "this endpoint cannot read a body of that media type",
        ),
        _ => ("error", "the request could not be completed"),
    }
}

/// Make every error body a `problem`, and give each one an id an operator can grep for.
///
/// Two jobs, one pass over the response, because both need the same thing: the body as it is about
/// to be sent.
///
/// * A failure produced by a *layer* rather than a handler has whatever body that layer writes —
///   axum answers an unrouted method with a bare `405` and no body at all, and `tower-http` answers
///   an oversized one with the eleven ASCII bytes `length limit exceeded`. The specification
///   requires a `problem` on every 4xx and 5xx, so this writes one. Doing it per status was the
///   earlier shape and it covered exactly the one status somebody had met: the rule is *every*
///   error, so the test is the media type of the body, not a list of statuses `[D-104]`.
/// * `problem.instance` is "a URI reference that identifies the specific occurrence"
///   `[API problem]`. Left unset it is decoration; filled in with the request id — the same one the
///   response header and the tracing span carry — it is the one field that turns "it returned a
///   500" into a log query.
///
/// A body already carrying `application/json` is left alone: the token endpoint's RFC 6749 §5.2
/// `authError` is an error the *OAuth2* specification defines the shape of, and rewriting it as a
/// `problem` would break every client that reads `error`.
pub async fn problem_responses(request: Request<axum::body::Body>, next: Next) -> Response {
    let request_id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let response = next.run(request).await;
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let is_problem = content_type
        .as_deref()
        .is_some_and(|v| v.starts_with("application/problem+json"));
    let is_json = content_type
        .as_deref()
        .is_some_and(|v| v.starts_with("application/json"));

    if (status.is_client_error() || status.is_server_error()) && !is_problem && !is_json {
        let (slug, detail) = layer_problem(status);
        let title = status.canonical_reason().unwrap_or("Error");
        let mut problem = Problem::new(status.as_u16(), slug, title).with_detail(detail);
        if let Some(id) = request_id {
            problem = problem.with_instance(id);
        }
        // Keep the head — `WWW-Authenticate`, `Allow`, `Retry-After` and anything else a layer set
        // are part of the answer — and replace only the body and its type.
        let (mut parts, _) = response.into_parts();
        parts.headers.remove(header::CONTENT_LENGTH);
        parts.headers.insert(
            header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/problem+json"),
        );
        let body = serde_json::to_vec(&problem).unwrap_or_default();
        return Response::from_parts(parts, axum::body::Body::from(body));
    }

    let Some(id) = request_id else {
        return response;
    };
    if !is_problem {
        return response;
    }

    // Error bodies are small and rare, so reading one back to stamp it costs nothing that matters.
    let (parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Ok(problem) = serde_json::from_slice::<Problem>(&bytes) else {
        return Response::from_parts(parts, axum::body::Body::from(bytes));
    };
    let Ok(body) = serde_json::to_vec(&problem.with_instance(id)) else {
        return Response::from_parts(parts, axum::body::Body::from(bytes));
    };

    // Keep the original head — status, content type, `WWW-Authenticate` and anything a layer added —
    // and swap only the body. Rebuilding the head instead is how a header gets dropped, and
    // `Content-Length` in particular would now be a lie.
    let mut parts = parts;
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, axum::body::Body::from(body))
}

/// Shared extractor bundle for list endpoints.
pub type ListArgs = (State<AppState>, Ctx, HeaderMap, RawQuery);

/// Shared extractor bundle for single-object endpoints.
pub type ItemArgs = (State<AppState>, Ctx, HeaderMap, Path<String>, RawQuery);

/// Parse a path segment into an object id.
pub fn parse_id(raw: &str) -> Result<ObjectId, ApiError> {
    ObjectId::new(raw).map_err(|e| ApiError::BadRequest(format!("invalid object id: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The middleware slugs, held to the same published registry as the handler ones.
    #[test]
    fn layer_problems_mint_published_types() {
        use crate::model::problem::PROBLEM_TYPES;

        for status in [
            StatusCode::METHOD_NOT_ALLOWED,
            StatusCode::PAYLOAD_TOO_LARGE,
            StatusCode::GATEWAY_TIMEOUT,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            // The fallback arm, which any other status reaches.
            StatusCode::BAD_GATEWAY,
        ] {
            let (slug, _) = layer_problem(status);
            assert!(
                PROBLEM_TYPES.contains(&slug),
                "{slug} is minted but not published; add it to PROBLEM_TYPES and the registry page"
            );
        }
    }

    #[test]
    fn repeated_and_comma_separated_lists_both_parse() {
        let a = Params::parse(Some("targets=x&targets=y"));
        assert_eq!(a.all("targets"), vec!["x", "y"]);
        let b = Params::parse(Some("targets=x,y"));
        assert_eq!(b.all("targets"), vec!["x", "y"]);
    }

    #[test]
    fn percent_encoding_is_decoded() {
        let p = Params::parse(Some("programName=Time%20of%20Use"));
        assert_eq!(p.first("programName"), Some("Time of Use"));
    }

    #[test]
    fn pagination_is_bounded_by_the_schema() {
        assert_eq!(
            Params::parse(Some("limit=10&skip=5")).page().unwrap(),
            Page { skip: 5, limit: 10 }
        );
        assert!(Params::parse(Some("limit=51")).page().is_err());
        assert!(Params::parse(Some("skip=-1")).page().is_err());
        assert!(Params::parse(Some("limit=abc")).page().is_err());
        assert_eq!(Params::parse(None).page().unwrap(), Page::default());
    }

    #[test]
    fn invalid_targets_are_rejected_at_the_edge() {
        let long = "t".repeat(129);
        assert!(
            Params::parse(Some(&format!("targets={long}")))
                .targets()
                .is_err()
        );
    }

    #[test]
    fn empty_target_entries_are_dropped() {
        let p = Params::parse(Some("targets=a,,b"));
        assert_eq!(p.targets().unwrap().len(), 2);
    }

    #[test]
    fn an_empty_body_is_a_clear_error() {
        let err = parse_body::<crate::model::ProgramRequest>(&Bytes::new()).unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }
}
