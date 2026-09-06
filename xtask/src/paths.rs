//! `check-paths` — the VTN's endpoint surface against `openadr3.yaml`'s.
//!
//! The companion to [`check-model`](crate::model), and the half it cannot reach. `check-model`
//! guards the *objects*; this guards the *paths*, the methods on them, and — the part that has
//! actually gone wrong here — which scopes each one demands.
//!
//! It does not read a table describing the router. It drives the **real router**, in process,
//! through `tower::Service`: every path the document declares is requested with a business-logic
//! credential and then with a VEN's, and the answers are compared with what the document's
//! `security` block says should happen. A table would be a second copy of the routing rules, which
//! is the shape three defects in this repository have already had; a request is the thing itself.
//!
//! Two questions, and D-049 is the second one:
//!
//! * **Is it routed at all?** A path the document declares and the VTN answers `no-such-route` or
//!   `method-not-allowed` for is an endpoint a conformant client will call and get nothing from.
//!   Anything else — `400` for a missing body, `404` for an object that does not exist, `501` for
//!   an optional feature — means the route exists, which is all this question asks.
//! * **Does the scope match?** The document's `security` block names the scopes for every
//!   operation. Where they are business logic's alone, a VEN must be refused; where they include a
//!   VEN's, a VEN must not be refused *for lack of scope*. Getting that backwards on three notifier
//!   endpoints is exactly what D-049 was, and reading twelve `security` blocks by hand is how it
//!   happened.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_yaml_ng::Value as Yaml;
use tower::ServiceExt;

/// The security scheme whose scopes this crate implements.
///
/// The document also lists `bearerAuth: []` on every operation, which is a scheme with no scopes —
/// present so the Alliance's own reference UI can call the API — and says nothing about
/// authorization. Reading it as "no scopes required" would make every check below vacuous.
const OAUTH_SCHEME: &str = "oAuth2ClientCredentials";

/// The scopes a VEN credential holds, per `Scope::VEN`.
const VEN_SCOPES: &[&str] = &[
    "read_targets",
    "read_ven_objects",
    "write_reports",
    "write_subscriptions",
    "write_vens",
];

const BL_TOKEN: &str = "check-paths-bl";
const VEN_TOKEN: &str = "check-paths-ven";
const VEN_CLIENT: &str = "check-paths-ven-client";

/// A scope requirement this crate deliberately does not enforce, with the reason.
///
/// `(path, method, why)`. Checked the same way [`model::ACCEPTED`](crate::model) is, and guarded
/// the same way: an entry that never fires is reported as stale, because an exception nothing needs
/// is D-044 wearing a fourth hat.
const ACCEPTED_SCOPES: &[(&str, &str, &str)] = &[(
    "/notifiers",
    "GET",
    "the document marks it `read_all`, which is business logic's, but `/notifiers` is how a client \
     learns the broker URI and 3.1's VEN-scoped topics exist for VENs — so a VEN that cannot read \
     it can never use the feature added for it. It carries no object data. Every *topic* endpoint \
     keeps the document's scope exactly. See D-049 and concepts/SPEC.md.",
)];

/// A path the document does not declare but this VTN serves, with the reason.
///
/// Kept short for the same reason `model::ACCEPTED` is: a long list means the router and the
/// document have drifted apart rather than that the document is incomplete.
const EXTENSIONS: &[(&str, &str)] = &[
    (
        "/notifiers/mqtt/topics/vens/{venID}/reports",
        "the fan-out publishes a VEN's own reports to its private topic and 3.1.0 names no endpoint \
         that discloses the topic — see concepts/SPEC.md",
    ),
    (
        "/notifiers/mqtt/topics/vens/{venID}/subscriptions",
        "the same, for a VEN's own subscriptions",
    ),
];

/// One operation the document declares.
#[derive(Debug)]
struct Operation {
    path: String,
    method: String,
    /// Scopes from the `oAuth2ClientCredentials` requirement, empty when the operation is
    /// unauthenticated (`security: - {}`) or names no OAuth2 requirement at all.
    scopes: Vec<String>,
    /// Whether the document marks the operation as needing no credential.
    unauthenticated: bool,
}

impl Operation {
    /// Whether the declared scopes are business logic's alone.
    fn business_logic_only(&self) -> bool {
        !self.scopes.is_empty() && !self.scopes.iter().any(|s| VEN_SCOPES.contains(&s.as_str()))
    }
}

pub fn check(spec: &Yaml) -> Result<()> {
    let operations = read_operations(spec)?;
    if operations.is_empty() {
        bail!("openadr3.yaml declares no paths");
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let problems = runtime.block_on(probe(&operations))?;

    if !problems.is_empty() {
        let mut message = format!(
            "the VTN's endpoint surface no longer matches openadr3.yaml ({} difference(s)):\n",
            problems.len()
        );
        for problem in &problems {
            message.push_str("  - ");
            message.push_str(problem);
            message.push('\n');
        }
        message.push_str(
            "\nEach one is a route to add, a scope to correct, or — if the document is the thing \
             that changed — a deliberate departure that belongs in xtask/src/paths.rs with the \
             reason.",
        );
        bail!(message);
    }

    println!(
        "the VTN routes all {} operations openadr3.yaml declares, with matching scopes \
         ({} documented extension(s))",
        operations.len(),
        EXTENSIONS.len()
    );
    Ok(())
}

/// Every `(path, method)` under `paths:`, with its OAuth2 scopes.
fn read_operations(spec: &Yaml) -> Result<Vec<Operation>> {
    let paths = spec
        .get("paths")
        .and_then(Yaml::as_mapping)
        .ok_or_else(|| anyhow::anyhow!("openadr3.yaml has no `paths`"))?;

    let mut out = Vec::new();
    for (path, item) in paths {
        let Some(path) = path.as_str() else { continue };
        let Some(item) = item.as_mapping() else {
            continue;
        };
        for (method, operation) in item {
            let Some(method) = method.as_str() else {
                continue;
            };
            if !matches!(method, "get" | "post" | "put" | "delete" | "patch") {
                continue;
            }
            let (scopes, unauthenticated) = security_of(operation);
            out.push(Operation {
                path: path.to_string(),
                method: method.to_uppercase(),
                scopes,
                unauthenticated,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.method.cmp(&b.method)));
    Ok(out)
}

/// The OAuth2 scopes one operation's `security` block names.
fn security_of(operation: &Yaml) -> (Vec<String>, bool) {
    let Some(requirements) = operation.get("security").and_then(Yaml::as_sequence) else {
        return (Vec::new(), false);
    };
    let mut scopes = Vec::new();
    let mut unauthenticated = false;
    for requirement in requirements {
        let Some(map) = requirement.as_mapping() else {
            continue;
        };
        // `- {}` is "no credential required".
        if map.is_empty() {
            unauthenticated = true;
            continue;
        }
        if let Some(list) = map
            .get(Yaml::from(OAUTH_SCHEME))
            .and_then(Yaml::as_sequence)
        {
            scopes.extend(list.iter().filter_map(|s| s.as_str()).map(str::to_string));
        }
    }
    (scopes, unauthenticated)
}

/// Drive the real router and collect every disagreement.
async fn probe(operations: &[Operation]) -> Result<Vec<String>> {
    use openadr::vtn::{Vtn, VtnConfig, auth::StaticTokenAuth, store::MemoryStorage};

    let vtn = Vtn::builder()
        .storage(MemoryStorage::shared())
        .authenticator(std::sync::Arc::new(
            StaticTokenAuth::new("http://check-paths/auth/token")
                .with_business_logic(BL_TOKEN, "check-paths-bl-client".parse()?)
                .with_ven(VEN_TOKEN, VEN_CLIENT.parse()?),
        ))
        // Without a broker every `notifiers/mqtt/topics/...` path answers `501` before it looks at
        // a scope, which would make the authorization half of this check pass by not running.
        .mqtt(openadr::model::MqttNotifierBinding {
            uris: vec!["mqtts://broker.invalid:8883".into()],
            serialization: openadr::model::Serialization::Json,
            authentication: openadr::model::MqttAuthentication::Anonymous,
        })
        .config(VtnConfig {
            base_path: openadr::DEFAULT_BASE_PATH.to_string(),
            ..Default::default()
        })
        .build();
    let router = vtn.router();

    let mut problems = Vec::new();
    let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
    // Which accepted departures were actually needed. An entry nothing fires on is a claim with a
    // name: the departure was fixed, or the document changed, and the list now says something that
    // is no longer true.
    let mut fired = vec![false; ACCEPTED_SCOPES.len()];
    for operation in operations {
        seen.insert(operation.path.as_str(), ());

        let (status, slug) = call(&router, operation, Some(BL_TOKEN)).await?;
        if slug == "no-such-route" {
            problems.push(format!(
                "{} {} is declared and not routed: the VTN answered 404 no-such-route",
                operation.method, operation.path
            ));
            continue;
        }
        if slug == "method-not-allowed" {
            problems.push(format!(
                "{} {} is declared and the VTN routes the path but not the method",
                operation.method, operation.path
            ));
            continue;
        }
        if status == StatusCode::UNAUTHORIZED || slug == "missing-scope" {
            problems.push(format!(
                "{} {} refused a business-logic credential ({status}, {slug:?}); the document asks \
                 for {:?}",
                operation.method, operation.path, operation.scopes
            ));
        }

        if operation.unauthenticated {
            let (status, slug) = call(&router, operation, None).await?;
            if status == StatusCode::UNAUTHORIZED || slug == "missing-scope" {
                problems.push(format!(
                    "{} {} is marked as needing no credential and the VTN refused one without \
                     ({status})",
                    operation.method, operation.path
                ));
            }
            continue;
        }

        // The D-049 question, asked of the document rather than of a reading of it.
        let (ven_status, ven_slug) = call(&router, operation, Some(VEN_TOKEN)).await?;
        let refused = ven_status == StatusCode::FORBIDDEN
            || ven_status == StatusCode::UNAUTHORIZED
            // A VEN-scoped path hides another VEN's objects as 404 rather than 403, deliberately,
            // so that ids cannot be enumerated. That is a refusal too.
            || ven_slug == "not-found";
        if operation.business_logic_only() && !refused && !accept(operation, &mut fired) {
            problems.push(format!(
                "{} {} demands {:?} — business logic's alone — and a VEN credential was not \
                 refused ({ven_status}, {ven_slug:?})",
                operation.method, operation.path, operation.scopes
            ));
        }
        if !operation.business_logic_only() && ven_slug == "missing-scope" {
            problems.push(format!(
                "{} {} allows {:?}, which a VEN holds, and the VTN refused a VEN for lack of scope",
                operation.method, operation.path, operation.scopes
            ));
        }
    }

    for (index, used) in fired.iter().enumerate() {
        if !used {
            let (path, method, _) = ACCEPTED_SCOPES[index];
            problems.push(format!(
                "{method} {path} is listed as an accepted scope departure and the VTN now enforces \
                 the document's scope — delete the entry"
            ));
        }
    }

    for (path, why) in EXTENSIONS {
        if seen.contains_key(path) {
            problems.push(format!(
                "{path} is listed as an extension but the document now declares it — delete the \
                 entry ({why})"
            ));
        }
    }
    Ok(problems)
}

/// Whether this operation's scope departure has been examined, marking it as having fired.
fn accept(operation: &Operation, fired: &mut [bool]) -> bool {
    match ACCEPTED_SCOPES
        .iter()
        .position(|(p, m, _)| *p == operation.path && *m == operation.method)
    {
        Some(index) => {
            fired[index] = true;
            true
        }
        None => false,
    }
}

/// One request against the router, returning the status and the `problem` type's slug.
async fn call(
    router: &axum::Router,
    operation: &Operation,
    token: Option<&str>,
) -> Result<(StatusCode, String)> {
    // `{programID}` and friends. A placeholder that no object has, so a routed path answers 404
    // `not-found` — which is a different slug from `no-such-route` and is what tells the two apart.
    let path = substitute(&operation.path);
    let mut request = Request::builder()
        .method(operation.method.as_str())
        .uri(format!("{}{path}", openadr::DEFAULT_BASE_PATH));
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    // An empty body is enough: every handler checks the scope before it looks at one, so a body
    // would only change a 400 into a 201 and leave objects behind.
    let request = request
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::empty())?;

    let response = router.clone().oneshot(request).await?;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024).await?;
    let slug = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| v["type"].as_str().map(str::to_string))
        .and_then(|t| t.rsplit('/').next().map(str::to_string))
        .unwrap_or_default();
    Ok((status, slug))
}

/// Replace every `{param}` with a placeholder identifier.
fn substitute(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}') else {
            break;
        };
        out.push_str("check-paths-no-such-object");
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_parameters_become_a_placeholder_no_object_has() {
        assert_eq!(substitute("/programs"), "/programs");
        assert_eq!(
            substitute("/programs/{programID}"),
            "/programs/check-paths-no-such-object"
        );
        assert_eq!(
            substitute("/notifiers/mqtt/topics/vens/{venID}/events"),
            "/notifiers/mqtt/topics/vens/check-paths-no-such-object/events"
        );
    }

    #[test]
    fn only_the_oauth2_requirement_is_read() {
        // Every operation also lists `bearerAuth: []`, a scheme with no scopes, present so the
        // Alliance's own reference UI can call the API. Reading that as "no scopes required" would
        // make every authorization check below vacuous — which is the failure mode of a check that
        // reads a document too generously.
        let operation: Yaml = serde_yaml_ng::from_str(
            "security:\n  - oAuth2ClientCredentials: [read_bl]\n  - bearerAuth: []\n",
        )
        .unwrap();
        let (scopes, unauthenticated) = security_of(&operation);
        assert_eq!(scopes, vec!["read_bl".to_string()]);
        assert!(!unauthenticated);
    }

    #[test]
    fn an_empty_requirement_means_no_credential() {
        let operation: Yaml = serde_yaml_ng::from_str("security:\n  - {}\n").unwrap();
        let (scopes, unauthenticated) = security_of(&operation);
        assert!(scopes.is_empty());
        assert!(unauthenticated);
    }

    #[test]
    fn a_scope_a_ven_holds_makes_an_operation_not_business_logics() {
        let bl_only = Operation {
            path: "/x".into(),
            method: "GET".into(),
            scopes: vec!["read_bl".into()],
            unauthenticated: false,
        };
        assert!(bl_only.business_logic_only());

        let shared = Operation {
            scopes: vec!["read_all".into(), "read_targets".into()],
            ..bl_only
        };
        assert!(!shared.business_logic_only());
    }
}
