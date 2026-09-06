//! Notifier discovery: `GET /notifiers` and the MQTT topic endpoints.
//!
//! OpenADR 3.1 generalised push delivery. A client asks the VTN which *notifier bindings* it
//! supports, then asks for the topic names it may subscribe to. Everything below the REST layer —
//! connecting to a broker, subscribing — happens in the binding's own protocol.

use crate::std_shim::{String, Vec};
use serde::{Deserialize, Serialize};

/// Message serialization used by a notifier binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Serialization {
    /// The only format the specification defines.
    #[default]
    Json,
}

/// How a client authenticates to the VTN's MQTT broker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method")]
pub enum MqttAuthentication {
    /// No authentication — appropriate for a public tariff server or a home LAN.
    #[serde(rename = "ANONYMOUS")]
    Anonymous,
    /// The OAuth2 access token, presented as the MQTT password.
    #[serde(rename = "OAUTH2_BEARER_TOKEN")]
    Oauth2BearerToken {
        /// Either the literal `{clientID}` — meaning "send your own client id" — or a fixed string.
        username: String,
    },
    /// Mutual TLS.
    #[serde(rename = "CERTIFICATE", rename_all = "camelCase")]
    Certificate {
        /// PEM of the certificate authority.
        ca_cert: String,
        /// PEM of the client certificate.
        client_cert: String,
        /// PEM of the client private key.
        client_key: String,
    },
}

impl MqttAuthentication {
    /// The username placeholder meaning "use your own client id".
    pub const CLIENT_ID_PLACEHOLDER: &'static str = "{clientID}";

    /// The username to send, given the caller's own client id if it knows one.
    ///
    /// `None` in the two cases a caller has to treat differently and cannot confuse here: a binding
    /// that wants no username at all, and one that wants the caller's `clientID` where the caller
    /// does not know its own — which is the pre-shared-token case, since the identity is inside the
    /// token and this crate's client never opens it.
    ///
    /// The placeholder rule lives here and nowhere else. It was written out a second time in the
    /// VEN's MQTT subscriber, which is how a rule with two implementations gets one of them wrong.
    pub fn resolve_username<'a>(&'a self, client_id: Option<&'a str>) -> Option<&'a str> {
        match self {
            MqttAuthentication::Oauth2BearerToken { username } => {
                if username == Self::CLIENT_ID_PLACEHOLDER {
                    client_id
                } else {
                    Some(username)
                }
            }
            _ => None,
        }
    }
}

/// Everything a client needs to reach the VTN's MQTT broker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MqttNotifierBinding {
    /// Broker URIs, e.g. `mqtts://broker.example.com:8883`.
    #[serde(rename = "URIS")]
    pub uris: Vec<String>,
    /// Message format.
    pub serialization: Serialization,
    /// Authentication method.
    pub authentication: MqttAuthentication,
}

/// Response of `GET /notifiers`.
///
/// `WEBHOOK` is required to be `true` by the specification; it exists so that a future revision can
/// make webhooks optional without changing the discovery shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifiersResponse {
    /// Whether webhook subscriptions are available. Must be `true`.
    #[serde(rename = "WEBHOOK")]
    pub webhook: bool,
    /// MQTT binding details, when the VTN has a broker.
    #[serde(rename = "MQTT", default, skip_serializing_if = "Option::is_none")]
    pub mqtt: Option<MqttNotifierBinding>,
}

impl Default for NotifiersResponse {
    fn default() -> Self {
        Self {
            webhook: true,
            mqtt: None,
        }
    }
}

/// Topic names for the operations on one subscribable object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "UPPERCASE")]
pub struct NotifierTopics {
    /// Topic for creations. Absent for endpoints scoped to one object: a thing that does not exist
    /// yet cannot be watched for creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create: Option<String>,
    /// Topic for updates.
    pub update: String,
    /// Topic for deletions.
    pub delete: String,
    /// Wildcard topic covering every operation, when the binding supports wildcards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub all: Option<String>,
}

/// Response of a `/notifiers/{binding}/topics/...` endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopicsResponse {
    /// The topics.
    pub topics: NotifierTopics,
}

impl TopicsResponse {
    /// Build the standard `create`/`update`/`delete`/`+` set under a topic prefix.
    pub fn under(prefix: &str) -> Self {
        Self {
            topics: NotifierTopics {
                create: Some(crate::std_shim::format!("{prefix}/create")),
                update: crate::std_shim::format!("{prefix}/update"),
                delete: crate::std_shim::format!("{prefix}/delete"),
                all: Some(crate::std_shim::format!("{prefix}/+")),
            },
        }
    }

    /// The same, minus `create`, for a topic scoped to a single existing object.
    pub fn under_without_create(prefix: &str) -> Self {
        let mut r = Self::under(prefix);
        r.topics.create = None;
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifiers_response_uses_uppercase_binding_keys() {
        let r = NotifiersResponse {
            webhook: true,
            mqtt: Some(MqttNotifierBinding {
                uris: crate::std_shim::vec!["mqtts://broker.example.com".into()],
                serialization: Serialization::Json,
                authentication: MqttAuthentication::Anonymous,
            }),
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["WEBHOOK"], true);
        assert_eq!(v["MQTT"]["URIS"][0], "mqtts://broker.example.com");
        assert_eq!(v["MQTT"]["authentication"]["method"], "ANONYMOUS");
        assert_eq!(v["MQTT"]["serialization"], "JSON");
    }

    #[test]
    fn client_id_placeholder_resolves() {
        let a = MqttAuthentication::Oauth2BearerToken {
            username: "{clientID}".into(),
        };
        assert_eq!(a.resolve_username(Some("ven-7")), Some("ven-7"));
        assert_eq!(
            a.resolve_username(None),
            None,
            "a placeholder with no clientID resolves to nothing"
        );
        let b = MqttAuthentication::Oauth2BearerToken {
            username: "oauth2".into(),
        };
        assert_eq!(b.resolve_username(Some("ven-7")), Some("oauth2"));
        assert_eq!(
            b.resolve_username(None),
            Some("oauth2"),
            "a fixed username needs no clientID"
        );
    }

    #[test]
    fn topic_sets_match_the_documented_layout() {
        let t = TopicsResponse::under("events/programs/44");
        assert_eq!(
            t.topics.create.as_deref(),
            Some("events/programs/44/create")
        );
        assert_eq!(t.topics.all.as_deref(), Some("events/programs/44/+"));

        let scoped = TopicsResponse::under_without_create("programs/44");
        assert!(scoped.topics.create.is_none());
        assert_eq!(scoped.topics.update, "programs/44/update");
    }

    #[test]
    fn topics_serialise_with_uppercase_operation_keys() {
        let v = serde_json::to_value(TopicsResponse::under("programs")).unwrap();
        assert_eq!(v["topics"]["CREATE"], "programs/create");
        assert_eq!(v["topics"]["ALL"], "programs/+");
    }
}
