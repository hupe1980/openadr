//! `/_inspect` — a read-only view of what this VTN is doing, for the person learning or debugging it.
//!
//! Three static files compiled into the binary and nothing else: no endpoint, no state, no data in
//! the page. The script signs in through the ordinary token endpoint with credentials the user
//! supplies and reads the ordinary API — so what it shows, including which events a VEN can and
//! cannot see, is the VTN's own enforcement rather than a picture of it.
//!
//! Every response forbids inline script, foreign origins and framing; the script inserts what the
//! VTN returns as text, never as markup (spec 006, SR-001).

use axum::{
    Router,
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
    routing::get,
};

const PAGE: &str = include_str!("inspector/index.html");
const SCRIPT: &str = include_str!("inspector/app.js");
const STYLE: &str = include_str!("inspector/style.css");

/// The policy every inspector response carries.
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
                       connect-src 'self'; img-src 'self' data:; base-uri 'none'; \
                       form-action 'none'; frame-ancestors 'none'";

/// The routes, for a router that has decided to serve them.
pub fn routes<S: Clone + Send + Sync + 'static>(base_path: &str) -> Router<S> {
    // The page needs to know where the API is. The base path is the operator's configuration, but
    // it is escaped anyway: it lands in an attribute.
    let page = PAGE.replace("__BASE_PATH__", &escape_attribute(base_path));
    Router::new()
        .route(
            "/_inspect",
            get(move || async move { asset("text/html; charset=utf-8", page) }),
        )
        .route(
            "/_inspect/app.js",
            get(|| async { asset("text/javascript; charset=utf-8", SCRIPT.to_string()) }),
        )
        .route(
            "/_inspect/style.css",
            get(|| async { asset("text/css; charset=utf-8", STYLE.to_string()) }),
        )
}

fn asset(content_type: &'static str, body: String) -> Response {
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn escape_attribute(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    // inspector/SR-001: nothing in the script turns VTN data into markup or code.
    #[test]
    fn the_script_never_inserts_markup_or_evaluates_text() {
        for sink in [
            "innerHTML",
            "outerHTML",
            "insertAdjacentHTML",
            "document.write",
            "eval(",
            "new Function",
        ] {
            assert!(!SCRIPT.contains(sink), "app.js uses {sink}");
        }
        assert!(
            !PAGE.contains("<script>"),
            "the page carries an inline script"
        );
        // No `onclick=`, `onload=` or any other inline handler.
        let inline_handler = PAGE.match_indices(" on").any(|(i, _)| {
            let rest = &PAGE[i + 3..];
            let name: String = rest.chars().take_while(char::is_ascii_lowercase).collect();
            !name.is_empty() && rest[name.len()..].starts_with('=')
        });
        assert!(!inline_handler, "the page carries an inline event handler");
    }

    // inspector/SR-002: credentials come from the fragment and leave the address bar at once.
    #[test]
    fn credentials_are_read_from_the_fragment_and_cleared() {
        assert!(SCRIPT.contains("location.hash"));
        assert!(SCRIPT.contains("history.replaceState(null, \"\", location.pathname)"));
        assert!(
            !SCRIPT.contains("location.search"),
            "credentials must never ride in a query"
        );
    }

    #[test]
    fn the_policy_forbids_inline_script_foreign_origins_and_framing() {
        for directive in [
            "default-src 'none'",
            "script-src 'self'",
            "frame-ancestors 'none'",
        ] {
            assert!(CSP.contains(directive), "{directive}");
        }
        assert!(!CSP.contains("unsafe-inline") && !CSP.contains("unsafe-eval"));
    }

    #[test]
    fn the_base_path_lands_escaped() {
        let router_page = PAGE.replace("__BASE_PATH__", &escape_attribute("/x\"><b>"));
        assert!(router_page.contains("/x&quot;&gt;&lt;b&gt;"));
    }
}
