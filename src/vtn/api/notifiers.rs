//! `/notifiers` — push-transport discovery.
//!
//! A client asks which notifier bindings the VTN supports, then asks for the topic names it may
//! subscribe to. The access control on these endpoints is the load-bearing part, and the scopes are
//! taken verbatim from the OpenAPI document's `security` blocks rather than inferred:
//!
//! | Endpoint | Scope |
//! |---|---|
//! | `mqtt/topics/programs`, `…/programs/{id}`, `…/programs/{id}/events` | `read_all` |
//! | `mqtt/topics/{events,reports,subscriptions,vens,resources}` | `read_bl` |
//! | `mqtt/topics/vens/{id}` and everything under it | `read_ven_objects`, and the VEN must be the
//!   caller's own |
//!
//! Every topic a non-`vens/{id}` endpoint names is collection-wide: subscribing to it yields every
//! object of that type with its full target set. Handing one to a VEN would undo object privacy on
//! the push path however carefully the broker were configured, which is why the programme-scoped
//! endpoints are business logic's even though a VEN would plausibly want them. A VEN reads its own
//! under `mqtt/topics/vens/{venID}/…`.
//!
//! `GET /notifiers` itself is the one deliberate departure: the document marks it `read_all`, but a
//! VEN that cannot read it can never learn the broker URI, which would make the VEN-scoped topics
//! 3.1 introduced unreachable by the only clients they exist for. It carries no object data.

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::Response,
};

use crate::model::{NotifiersResponse, ObjectType, TopicsResponse};

use super::super::{ApiError, AppState, auth::Scope, notify::Topics};
use super::{Ctx, ok, parse_id};

/// `GET /notifiers`
pub async fn describe(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ctx.require_read()?;
    // The response may legitimately differ per client — the specification says so — but what varies
    // here is only what this VTN can actually do.
    //
    // `WEBHOOK` reports whether a transport is installed, not the constant `true` the specification
    // expects of a conformant VTN. Reporting `true` on a VTN with no webhook transport would be a
    // lie that costs a subscriber real notifications: it would create a subscription, receive
    // nothing, and have no way to find out why. The binding key exists precisely so that a client
    // can be told (`[Notifiers §7.1]`).
    let response = NotifiersResponse {
        webhook: state.notifier.handles(crate::vtn::notify::Channel::Webhook),
        mqtt: state.config.mqtt.clone(),
    };
    ok(&state, &headers, &response)
}

fn topics(state: &AppState) -> Topics {
    Topics::new(state.config.mqtt_topic_prefix.clone())
}

/// Refuse unless the VTN actually has a broker to point at.
fn require_mqtt(state: &AppState) -> Result<(), ApiError> {
    if state.config.mqtt.is_some() {
        Ok(())
    } else {
        Err(ApiError::NotImplemented(
            "this VTN does not offer an MQTT notifier; see GET /notifiers".into(),
        ))
    }
}

macro_rules! collection_topics {
    ($name:ident, $object_type:expr, $scope:expr, $doc:literal) => {
        #[doc = $doc]
        pub async fn $name(
            State(state): State<AppState>,
            ctx: Ctx,
            headers: HeaderMap,
        ) -> Result<Response, ApiError> {
            require_mqtt(&state)?;
            // The scope, not the inferred role: an operator can mint a token that reads objects
            // without also being able to enumerate broker topics.
            ctx.require($scope)?;
            let t = topics(&state);
            ok(
                &state,
                &headers,
                &TopicsResponse::under(&t.collection($object_type)),
            )
        }
    };
}

collection_topics!(
    programs,
    ObjectType::Program,
    Scope::ReadAll,
    "`GET /notifiers/mqtt/topics/programs`"
);
collection_topics!(
    events,
    ObjectType::Event,
    Scope::ReadBl,
    "`GET /notifiers/mqtt/topics/events`"
);
collection_topics!(
    reports,
    ObjectType::Report,
    Scope::ReadBl,
    "`GET /notifiers/mqtt/topics/reports`"
);
collection_topics!(
    subscriptions,
    ObjectType::Subscription,
    Scope::ReadBl,
    "`GET /notifiers/mqtt/topics/subscriptions`"
);
collection_topics!(
    vens,
    ObjectType::Ven,
    Scope::ReadBl,
    "`GET /notifiers/mqtt/topics/vens`"
);
collection_topics!(
    resources,
    ObjectType::Resource,
    Scope::ReadBl,
    "`GET /notifiers/mqtt/topics/resources`"
);

/// `GET /notifiers/mqtt/topics/programs/{id}`
pub async fn program_by_id(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    require_mqtt(&state)?;
    ctx.require(Scope::ReadAll)?;
    let id = parse_id(&id)?;
    // Confirm the programme exists, so a caller cannot mint topic names for objects that do not.
    let _ = state.storage.get_program(&id).await?;
    let t = topics(&state);
    ok(
        &state,
        &headers,
        // No CREATE: a programme that does not exist yet has no id to watch.
        &TopicsResponse::under_without_create(&t.object(ObjectType::Program, &id)),
    )
}

/// `GET /notifiers/mqtt/topics/programs/{id}/events`
pub async fn program_events(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    require_mqtt(&state)?;
    ctx.require(Scope::ReadAll)?;
    let id = parse_id(&id)?;
    let _ = state.storage.get_program(&id).await?;
    let t = topics(&state);
    ok(
        &state,
        &headers,
        &TopicsResponse::under(&t.program_events(&id)),
    )
}

/// `GET /notifiers/mqtt/topics/vens/{id}`
pub async fn ven_by_id(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    require_mqtt(&state)?;
    let id = parse_id(&id)?;
    require_own_ven(&state, &ctx, &id).await?;
    let t = topics(&state);
    ok(
        &state,
        &headers,
        // The same call the fan-out makes, so the topic a VEN is told to subscribe to is the topic
        // the VTN publishes to. No CREATE: a VEN object's creation predates the snapshot that would
        // route it, and a client that just created one does not need telling.
        &TopicsResponse::under_without_create(&t.ven_scoped(ObjectType::Ven, &id)),
    )
}

macro_rules! ven_scoped_topics {
    ($name:ident, $object_type:expr, $doc:literal) => {
        #[doc = $doc]
        pub async fn $name(
            State(state): State<AppState>,
            ctx: Ctx,
            headers: HeaderMap,
            Path(id): Path<String>,
        ) -> Result<Response, ApiError> {
            require_mqtt(&state)?;
            let id = parse_id(&id)?;
            require_own_ven(&state, &ctx, &id).await?;
            let t = topics(&state);
            ok(
                &state,
                &headers,
                &TopicsResponse::under(&t.ven_scoped($object_type, &id)),
            )
        }
    };
}

ven_scoped_topics!(
    ven_events,
    ObjectType::Event,
    "`GET /notifiers/mqtt/topics/vens/{id}/events`"
);
ven_scoped_topics!(
    ven_programs,
    ObjectType::Program,
    "`GET /notifiers/mqtt/topics/vens/{id}/programs`"
);
ven_scoped_topics!(
    ven_resources,
    ObjectType::Resource,
    "`GET /notifiers/mqtt/topics/vens/{id}/resources`"
);
// Beyond the document, and deliberately. The fan-out already publishes a VEN's own reports and
// subscriptions to its private topics — they are owned objects, so ownership decides and the copy
// goes to exactly one VEN — but 3.1.0 defines no endpoint that names those two topics. The choice
// was between publishing to a name no client can discover and naming it; a VEN that wants to know
// its subscription was deleted has no other way to hear.
ven_scoped_topics!(
    ven_reports,
    ObjectType::Report,
    "`GET /notifiers/mqtt/topics/vens/{id}/reports}` — an extension; see the module note."
);
ven_scoped_topics!(
    ven_subscriptions,
    ObjectType::Subscription,
    "`GET /notifiers/mqtt/topics/vens/{id}/subscriptions` — an extension; see the module note."
);

/// A VEN may ask for its own topics; business logic may ask for anyone's.
///
/// Reported as `404` rather than `403` so that a caller cannot enumerate VEN ids.
async fn require_own_ven(
    state: &AppState,
    ctx: &Ctx,
    ven_id: &crate::model::ObjectId,
) -> Result<(), ApiError> {
    // `read_ven_objects` is the scope the document puts on these paths. Ownership is checked below;
    // the scope is checked first so that a token without it is refused before any lookup happens.
    if !ctx.is_business_logic() {
        ctx.require(Scope::ReadVenObjects)?;
    }
    let ven = state.storage.get_ven(ven_id).await?;
    if ctx.is_business_logic() {
        return Ok(());
    }
    if ctx.principal.client_id.as_ref() == Some(&ven.client_id) {
        return Ok(());
    }
    Err(ApiError::Storage(
        super::super::store::StorageError::NotFound {
            object_type: ObjectType::Ven,
            id: ven_id.clone(),
        },
    ))
}
