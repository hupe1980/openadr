//! `/internal/mqtt/auth` and `/internal/mqtt/acl` — the broker's authorization callbacks.
//!
//! Per-VEN topics only keep a competitor's dispatch schedule private if the broker refuses to let
//! one VEN subscribe to another's. The specification is explicit that this is the VTN's problem and
//! declines to say how: *"A VTN **MUST** prevent a VEN from subscribing to topics that would expose
//! objects the VEN is not authorized to access"*, with the mechanism "an implementation detail"
//! `[Notifiers §9.4]`.
//!
//! Brokers already have the shape for it — EMQX's HTTP authentication and authorization backends,
//! and `mosquitto-go-auth`'s `http` backend, both call out to a URL per connection and per
//! subscription. These two endpoints are that URL. The VTN answers, because the VTN is the only
//! party that knows which VEN a `clientID` belongs to.
//!
//! ## The two questions
//!
//! **Who are you** (`/auth`) is the OpenADR token, presented as the MQTT password — the convention
//! the MQTT binding names `[Notifiers §12.2]`. It is checked through the same
//! [`Authenticator`](crate::vtn::auth::Authenticator) the REST API uses, so a revoked credential
//! stops working on both at once. The username must be the caller's own `clientID`, which is what
//! makes the *next* question answerable without a token.
//!
//! **May you subscribe to this** (`/acl`) carries a username and a topic and no token, because that
//! is all a broker has at subscribe time. So the answer is derived from the topic:
//!
//! * `<prefix>/{collection}/vens/{venID}/{operation}` — allowed when the VEN with that id has this
//!   username as its `clientID`. This is the same rule
//!   [`ven_by_id`](super::notifiers::ven_by_id) applies to the endpoint that hands out the name.
//! * anything else — a collection-wide topic, carrying every object with its full target set.
//!   Allowed only for a username in
//!   [`mqtt_business_logic_clients`](crate::vtn::VtnConfig::mqtt_business_logic_clients), which is
//!   empty by default.
//! * publishing — refused, unless the username is the VTN's own publisher
//!   ([`mqtt_publisher_client`](crate::vtn::VtnConfig::mqtt_publisher_client)), and then only under
//!   the configured topic prefix. A client that could publish could forge a dispatch instruction,
//!   which is the worst thing on this list — but a broker that delegates authentication *here*
//!   makes the VTN a client of its own broker, and a blanket refusal locks the fan-out out of it.
//!   One named identity is the smallest exception that works.
//!
//! ## Why this is not the REST authorization path
//!
//! It answers `{"result": "allow"}` rather than a `problem` body, and it lives under `/internal`
//! rather than the OpenADR base path, because it is not an OpenADR endpoint: it is a private
//! contract between this VTN and its broker. Expose it to the broker only.

use axum::{
    Json,
    extract::State,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use crate::model::{ClientId, ObjectType};

use super::super::{AppState, auth::bearer_from_header};

/// What the broker asks when a client connects.
///
/// Field names follow EMQX's default HTTP-authentication template so the common case is a URL and
/// nothing else. `mosquitto-go-auth` sends `username`/`password`/`clientid` too.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthRequest {
    /// The MQTT username. Must be the caller's OpenADR `clientID`.
    #[serde(default)]
    pub username: String,
    /// The MQTT password, which carries the OpenADR access token.
    #[serde(default)]
    pub password: String,
    /// The MQTT client identifier, for logs.
    #[serde(default, rename = "clientid")]
    pub client_id: Option<String>,
}

/// What the broker asks before a subscription.
#[derive(Debug, Clone, Deserialize)]
pub struct AclRequest {
    /// The MQTT username, already proven by [`AuthRequest`].
    #[serde(default)]
    pub username: String,
    /// `subscribe` or `publish`.
    #[serde(default)]
    pub action: String,
    /// The topic filter the client asked for.
    #[serde(default)]
    pub topic: String,
    /// The MQTT client identifier, for logs.
    #[serde(default, rename = "clientid")]
    pub client_id: Option<String>,
}

/// The answer, in the shape EMQX expects.
///
/// Always `200`: a non-2xx is a *broker* error to EMQX, which then applies its own fallback rather
/// than the decision this endpoint made. A refusal has to arrive as a successful "deny".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Decision {
    /// `allow` or `deny`.
    pub result: &'static str,
    /// Why, for the broker's log and for whoever is debugging a subscription that will not take.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Decision {
    /// Permit.
    pub fn allow() -> Self {
        Self {
            result: "allow",
            reason: None,
        }
    }

    /// Refuse, saying why.
    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            result: "deny",
            reason: Some(reason.into()),
        }
    }

    /// Whether this permits.
    pub fn is_allowed(&self) -> bool {
        self.result == "allow"
    }
}

impl IntoResponse for Decision {
    fn into_response(self) -> Response {
        (axum::http::StatusCode::OK, Json(self)).into_response()
    }
}

/// `POST /internal/mqtt/auth`
pub async fn authenticate(State(state): State<AppState>, body: axum::body::Bytes) -> Response {
    let Ok(request) = serde_json::from_slice::<AuthRequest>(&body) else {
        return Decision::deny("unreadable authentication request").into_response();
    };
    decide_auth(&state, &request).await.into_response()
}

/// `POST /internal/mqtt/acl`
pub async fn authorize(State(state): State<AppState>, body: axum::body::Bytes) -> Response {
    let Ok(request) = serde_json::from_slice::<AclRequest>(&body) else {
        return Decision::deny("unreadable authorization request").into_response();
    };
    decide_acl(&state, &request).await.into_response()
}

/// Refuse unless this VTN actually offers a broker.
fn broker_configured(state: &AppState) -> Result<(), Decision> {
    if state.config.mqtt.is_some() {
        Ok(())
    } else {
        Err(Decision::deny("this VTN offers no MQTT notifier"))
    }
}

async fn decide_auth(state: &AppState, request: &AuthRequest) -> Decision {
    if let Err(denied) = broker_configured(state) {
        return denied;
    }
    // The token arrives as the password, which is the convention the binding names. Accepting a
    // `Bearer `-prefixed value too costs nothing and saves an afternoon.
    let principal = match state
        .authenticator
        .authenticate(bearer_from_header(Some(&request.password)).or(Some(request.password.trim())))
        .await
    {
        Ok(principal) => principal,
        Err(e) => return Decision::deny(e.to_string()),
    };

    let Some(client_id) = principal.client_id else {
        // An anonymous principal has no identity to confine to a topic, so there is no per-VEN
        // privacy to enforce and nothing that could be enforced on its behalf.
        return Decision::deny("this credential identifies no client");
    };
    if client_id.as_str() != request.username {
        // The ACL endpoint has only the username to go on, so it has to be the identity the token
        // proved. Letting the two differ is letting a VEN pick which VEN it is.
        return Decision::deny(format!(
            "the MQTT username must be the credential's clientID ({client_id})"
        ));
    }
    Decision::allow()
}

async fn decide_acl(state: &AppState, request: &AclRequest) -> Decision {
    if let Err(denied) = broker_configured(state) {
        return denied;
    }
    let Ok(username) = ClientId::new(&request.username) else {
        return Decision::deny("the MQTT username is not a valid clientID");
    };

    // Publishing is the VTN's alone. A client that could publish could forge a dispatch
    // instruction, which is the worst thing on this list.
    //
    // The exception exists because a broker that authenticates through `/auth` authenticates the
    // VTN too: the fan-out is a client of its own broker, and a blanket refusal leaves it
    // reconnecting for ever against `NotAuthorized`. So exactly one configured identity may
    // publish, it presented a real credential to `/auth` to get here, and it may publish only
    // under this VTN's own topic prefix — a BL client that borrowed the name still cannot reach
    // another deployment's topics on a shared broker.
    if !request.action.eq_ignore_ascii_case("subscribe") {
        let is_publisher = state
            .config
            .mqtt_publisher_client
            .as_ref()
            .is_some_and(|id| *id == username);
        let prefix = state.config.mqtt_topic_prefix.trim_end_matches('/');
        let under_prefix = prefix.is_empty() || request.topic.starts_with(&format!("{prefix}/"));
        return match (is_publisher, under_prefix) {
            (true, true) => Decision::allow(),
            (true, false) => Decision::deny(
                "the VTN's publisher may only publish under this VTN's own topic prefix",
            ),
            (false, _) => Decision::deny("only the VTN publishes to these topics"),
        };
    }

    match ven_scope(&state.config.mqtt_topic_prefix, &request.topic) {
        Some(ven_id) => {
            // The same question `GET /notifiers/mqtt/topics/vens/{venID}/…` answers, asked of the
            // same store, so a VEN can never subscribe to a topic it could not have discovered.
            let Ok(id) = crate::model::ObjectId::new(&ven_id) else {
                return Decision::deny("the topic names no valid venID");
            };
            match state.storage.get_ven(&id).await {
                Ok(ven) if ven.client_id == username => Decision::allow(),
                Ok(_) => Decision::deny("that VEN belongs to another client"),
                // Fail closed. A store that cannot answer is not a reason to hand out a topic.
                Err(_) => Decision::deny("no such VEN"),
            }
        }
        None => {
            if state.config.mqtt_business_logic_clients.contains(&username) {
                Decision::allow()
            } else {
                Decision::deny(
                    "collection-wide topics carry every object's full target set and are business \
                     logic's",
                )
            }
        }
    }
}

/// The `venID` a topic is scoped to, if it is one of the per-VEN topics.
///
/// Two shapes, after the configured prefix, and they are exactly the two
/// [`Topics::ven_scoped`](crate::vtn::notify::Topics::ven_scoped) produces:
///
/// * `{collection}/vens/{venID}/{operation}` for an object that belongs to a VEN;
/// * `vens/{venID}/{operation}` for the VEN object itself, which is addressed by its own id.
///
/// Matched by exact segment count rather than by scanning, because a loose match reads the
/// collection topic `vens/create` as "the VEN whose id is `create`" — which fails closed, but fails
/// closed on a topic business logic is entitled to.
fn ven_scope(prefix: &str, topic: &str) -> Option<String> {
    let prefix = prefix.trim_end_matches('/');
    let rest = if prefix.is_empty() {
        topic
    } else {
        topic.strip_prefix(prefix)?.strip_prefix('/')?
    };
    let parts: Vec<&str> = rest.split('/').collect();
    let ven_id = match parts.as_slice() {
        // The VEN object's own topic.
        [collection, ven_id, _operation] if *collection == ObjectType::Ven.collection() => ven_id,
        // Anything else belonging to a VEN.
        [collection, "vens", ven_id, _operation]
            if ObjectType::ALL
                .iter()
                .any(|t| t.collection() == *collection) =>
        {
            ven_id
        }
        _ => return None,
    };
    // A wildcard where the id belongs is one filter for everybody's topics — the subscription that
    // would undo the entire design.
    if ven_id.contains('+') || ven_id.contains('#') || ven_id.is_empty() {
        return None;
    }
    Some((*ven_id).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ven_scoped_topic_yields_its_ven() {
        assert_eq!(
            ven_scope("openadr3", "openadr3/events/vens/ven-1/create").as_deref(),
            Some("ven-1")
        );
        assert_eq!(
            ven_scope("openadr3", "openadr3/events/vens/ven-1/+").as_deref(),
            Some("ven-1")
        );
        assert_eq!(
            ven_scope("openadr3", "openadr3/resources/vens/ven-1/update").as_deref(),
            Some("ven-1")
        );
        // The VEN object's own topic is `vens/{venID}/…`, which is what both the endpoint and the
        // fan-out produce.
        assert_eq!(
            ven_scope("openadr3", "openadr3/vens/ven-1/update").as_deref(),
            Some("ven-1")
        );
        assert_eq!(
            ven_scope("", "events/vens/ven-1/create").as_deref(),
            Some("ven-1")
        );
    }

    #[test]
    fn a_collection_topic_is_not_ven_scoped() {
        assert_eq!(ven_scope("openadr3", "openadr3/events/create"), None);
        assert_eq!(ven_scope("openadr3", "openadr3/events/+"), None);
        assert_eq!(
            ven_scope("openadr3", "openadr3/events/programs/prg-1/create"),
            None
        );
        assert_eq!(
            ven_scope("openadr3", "other/events/vens/ven-1/create"),
            None
        );
        // `vens/create` is the collection topic, not "the VEN called create". Read loosely it is
        // the second, and business logic is then refused a topic it is entitled to.
        assert_eq!(ven_scope("openadr3", "openadr3/vens/create"), None);
        assert_eq!(ven_scope("openadr3", "openadr3/vens/+"), None);
    }

    #[test]
    fn a_wildcard_where_the_ven_id_belongs_is_not_a_ven_scope() {
        // The subscription that would defeat the whole design: one filter, everybody's topics.
        assert_eq!(ven_scope("openadr3", "openadr3/events/vens/+/create"), None);
        assert_eq!(ven_scope("openadr3", "openadr3/events/vens/#"), None);
        // And neither is a filter that reaches past the operation segment.
        assert_eq!(
            ven_scope("openadr3", "openadr3/events/vens/ven-1/create/extra"),
            None
        );
    }
}
