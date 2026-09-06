//! `GET /openapi.json` — what *this* VTN serves, as a machine-readable document.
//!
//! The specification asks for it by name: a local VTN's mDNS record carries `openapi_url` "with
//! value being a URL to the OpenAPI specification for the VTN" `[Def §Discovery and Configuration
//! of Local VTNs]`, so a VEN that finds a VTN can read its API surface without being told anything
//! else — and a key naming a document nothing serves is a record that lies.
//!
//! The base document is `openadr3.yaml`, transcoded by `cargo xtask codegen` into `openapi.json` and
//! guarded by `check-drift` exactly as the payload table is. What is *served* is that copy narrowed
//! to this deployment: `servers` names its own base path, the MQTT topic paths are removed when
//! there is no broker — they answer `501`, and a generated client with a method that cannot work is
//! worse than no method — and the endpoints this VTN adds carry `x-openadr-extension`.
//!
//! Narrowing at serve time rather than editing the copy is what keeps the copy something
//! `check-drift` can compare.
//!
//! Unauthenticated, like `GET /auth/server`: it is what a client reads *before* it has a credential,
//! and it discloses no object.

use axum::{extract::State, http::HeaderMap, response::Response};

use super::{ApiError, AppState};

/// The Alliance's document, transcoded to JSON by `cargo xtask codegen`.
///
/// © OpenADR Alliance, published under Apache-2.0; see `specs/openadr3-specification/NOTICE`.
const DOCUMENT: &str = include_str!("openapi.json");

/// Paths that exist only when this VTN has an MQTT broker to point at.
const MQTT_PREFIX: &str = "/notifiers/mqtt/";

/// `GET /openapi.json`
pub async fn describe(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let document = document(&state.config);
    super::api::ok(&state, &headers, &document)
}

/// The document as this VTN serves it.
///
/// Public so a deployment embedding the router can serve it somewhere else, and so the test that
/// holds it against the real router can read it without HTTP.
pub fn document(config: &super::VtnConfig) -> serde_json::Value {
    let mut document: serde_json::Value =
        serde_json::from_str(DOCUMENT).expect("the generated document is valid JSON");
    let Some(root) = document.as_object_mut() else {
        return document;
    };

    // A relative server URL resolves against the document's own location, so it is right whatever
    // hostname, scheme or reverse proxy the deployment is behind — and needs no configuration that
    // could be wrong. The alternative, reading `Host` and `X-Forwarded-Proto`, is a guess about a
    // proxy this VTN cannot see.
    let base = config.base_path.trim_end_matches('/');
    root.insert(
        "servers".into(),
        serde_json::json!([{
            "url": if base.is_empty() { "/" } else { base },
            "description": "this VTN",
        }]),
    );

    if let Some(info) = root.get_mut("info").and_then(|i| i.as_object_mut()) {
        info.insert(
            "x-openadr-implementation".into(),
            serde_json::json!({
                "name": env!("CARGO_PKG_NAME"),
                "version": env!("CARGO_PKG_VERSION"),
                "specVersion": crate::SPEC_VERSION,
            }),
        );
    }

    let Some(paths) = root.get_mut("paths").and_then(|p| p.as_object_mut()) else {
        return document;
    };

    if config.mqtt.is_none() {
        paths.retain(|path, _| !path.starts_with(MQTT_PREFIX));
    } else {
        for (path, summary) in EXTENSION_TOPICS {
            paths.insert((*path).into(), topic_operation(summary));
        }
    }

    if config.program_name_lookup {
        add_program_name_parameter(paths);
    }

    document
}

/// Topic endpoints this VTN serves that 3.1.0 does not declare.
///
/// The fan-out publishes a VEN's own reports and subscriptions to its private topic, and the
/// document names no endpoint that discloses either — so a VEN could receive on a topic it had no
/// way to learn the name of. `cargo xtask check-paths` carries the same pair as documented
/// extensions; this is the half a client can read.
const EXTENSION_TOPICS: &[(&str, &str)] = &[
    (
        "/notifiers/mqtt/topics/vens/{venID}/reports",
        "topic names for operations on this VEN's own reports",
    ),
    (
        "/notifiers/mqtt/topics/vens/{venID}/subscriptions",
        "topic names for operations on this VEN's own subscriptions",
    ),
];

fn topic_operation(summary: &str) -> serde_json::Value {
    serde_json::json!({
        "get": {
            "tags": ["MQTT_notifier"],
            "summary": summary,
            "x-openadr-extension": "not declared by openadr3.yaml; see the VTN's spec notes",
            "parameters": [{
                "name": "venID",
                "in": "path",
                "required": true,
                "schema": { "$ref": "#/components/schemas/objectID" },
            }],
            "security": [{ "oAuth2ClientCredentials": ["read_ven_objects"] }],
            "responses": {
                "200": {
                    "description": "OK",
                    "content": {
                        "application/json": {
                            "schema": { "$ref": "#/components/schemas/notifierOperationsTopics" },
                        },
                    },
                },
                "403": { "$ref": "#/components/responses/forbidden" },
                "404": { "$ref": "#/components/responses/notFound" },
            },
        },
    })
}

/// `?programName=` on `/programs`, which the document does not declare.
///
/// Finding one tariff among hundreds otherwise means paging the whole collection, so this VTN
/// accepts it — and says so here, marked as an extension, rather than leaving it to be discovered.
fn add_program_name_parameter(paths: &mut serde_json::Map<String, serde_json::Value>) {
    let Some(parameters) = paths
        .get_mut("/programs")
        .and_then(|p| p.get_mut("get"))
        .and_then(|g| g.get_mut("parameters"))
        .and_then(|p| p.as_array_mut())
    else {
        return;
    };
    parameters.insert(
        0,
        serde_json::json!({
            "name": "programName",
            "in": "query",
            "required": false,
            "x-openadr-extension": "not declared by openadr3.yaml; see the VTN's spec notes",
            "schema": { "$ref": "#/components/schemas/programName" },
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vtn::VtnConfig;

    fn config() -> VtnConfig {
        VtnConfig {
            base_path: "/openadr3/3.1.0".into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_document_names_this_vtn_rather_than_a_mock() {
        let document = document(&config());
        assert_eq!(document["servers"][0]["url"], "/openadr3/3.1.0");
        // The published file points at SwaggerHub. A generated client that talked to it would work
        // perfectly and reach somebody else's data.
        assert!(
            !document.to_string().contains("swaggerhub"),
            "the mock server survived into the served document"
        );
    }

    #[test]
    fn a_vtn_with_no_broker_describes_no_broker_endpoints() {
        let document = document(&config());
        let paths = document["paths"].as_object().unwrap();
        assert!(
            paths.keys().all(|p| !p.starts_with(MQTT_PREFIX)),
            "a VTN that answers 501 for every topic endpoint described them anyway"
        );
        // And the rest is still there.
        assert!(paths.contains_key("/programs"));
        assert!(paths.contains_key("/notifiers"));
    }

    #[test]
    fn a_vtn_with_a_broker_describes_its_extensions_as_extensions() {
        let config = VtnConfig {
            mqtt: Some(crate::model::MqttNotifierBinding {
                uris: vec!["mqtts://broker.test:8883".into()],
                serialization: crate::model::Serialization::Json,
                authentication: crate::model::MqttAuthentication::Anonymous,
            }),
            ..config()
        };
        let document = super::document(&config);
        let paths = document["paths"].as_object().unwrap();
        assert!(paths.contains_key("/notifiers/mqtt/topics/programs"));
        let extension = &paths["/notifiers/mqtt/topics/vens/{venID}/reports"]["get"];
        assert!(
            extension["x-openadr-extension"].is_string(),
            "an endpoint the document does not declare was served as if it did"
        );
    }

    #[test]
    fn an_extension_parameter_is_marked_as_one() {
        let document = document(&config());
        let parameters = document["paths"]["/programs"]["get"]["parameters"]
            .as_array()
            .unwrap();
        let program_name = parameters
            .iter()
            .find(|p| p["name"] == "programName")
            .expect("?programName= is accepted and must be described");
        assert!(program_name["x-openadr-extension"].is_string());

        // And it is absent when the VTN does not accept it.
        let off = super::document(&VtnConfig {
            program_name_lookup: false,
            ..config()
        });
        assert!(
            !off["paths"]["/programs"]["get"]["parameters"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["name"] == "programName")
        );
    }

    #[test]
    fn the_document_says_where_it_came_from() {
        let document = document(&config());
        let source = document["info"]["x-openadr-source"].as_str().unwrap();
        assert!(source.contains("OpenADR Alliance"), "{source}");
        assert!(source.contains("Apache-2.0"), "{source}");
        assert_eq!(
            document["info"]["x-openadr-implementation"]["specVersion"],
            crate::SPEC_VERSION
        );
    }
}
