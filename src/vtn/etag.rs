//! HTTP caching for pollers.
//!
//! OpenADR has no delta-sync mechanism: a client that wants to know whether anything changed
//! re-fetches the collection. Real deployments do exactly that on a timer — a Dutch charge-point
//! operator re-reads a rolling 48-hour window, a Belgian DER client polls its schedule, a
//! Californian appliance re-reads hourly prices — so the same bytes cross the wire again and again.
//!
//! `ETag` plus `If-None-Match` turns that into a `304` with an empty body. It is invisible to a
//! client that does not implement it, and it is plain HTTP rather than a protocol extension.
//!
//! ## Why the tag is weak
//!
//! The tag is computed here, over the JSON, and the response then passes through a compression
//! layer that rewrites the body. A *strong* validator has to change whenever the representation
//! changes, and a content-coding is part of the representation (RFC 9110 §8.8.1) — so the same
//! strong tag on a 7 kB body and on the 650-byte gzip of it is a validator that lies. `Vary:
//! Accept-Encoding` keeps a *conformant* shared cache out of trouble; it does nothing for a client
//! that stores the compressed bytes under that tag and later revalidates asking for identity, which
//! gets a `304` and decodes gzip as JSON.
//!
//! So the tag is weak, which is what nginx does for the same reason when it compresses. Nothing is
//! lost: `If-None-Match` is defined to use the weak comparison function (RFC 9110 §13.1.2), so
//! every `304` still happens. The only thing a strong validator additionally buys is `If-Range`,
//! and this API serves no ranges. The `Vary` header itself is
//! `tower_http::compression::CompressionLayer`'s, which appends `accept-encoding` to every
//! response it would compress — so the claim above has a caller rather than being a wish.
//!
//! ## Switching it off
//!
//! [`VtnConfig::http_caching`](crate::vtn::VtnConfig::http_caching) `= false` removes the `ETag`
//! entirely rather than merely ignoring `If-None-Match`. A validator a server will not honour is
//! worse than none: the client stores it, revalidates for ever and never gets a `304`.

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

/// The opaque part of an entity tag: a quoted hash of the body, without the weakness prefix.
///
/// Kept separate from the header value because comparison and rendering want different things. A
/// client may send `"abc"` or `W/"abc"` for the same tag, so matching happens on this form and the
/// `W/` is added once, on the way out.
pub fn entity_tag(body: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(body);
    // 128 bits is ample for cache validation and keeps the header short.
    let mut tag = String::with_capacity(34);
    tag.push('"');
    for byte in &digest[..16] {
        tag.push(HEX[usize::from(byte >> 4)] as char);
        tag.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    tag.push('"');
    tag
}

/// The `ETag` header value for an opaque tag.
fn header_value(tag: &str) -> String {
    crate::std_shim::format!("W/{tag}")
}

/// Whether the client already holds this representation.
///
/// Handles the `*` wildcard and comma-separated lists, and compares weakly — which is what
/// `If-None-Match` is defined to do (RFC 9110 §13.1.2), and is why `W/"x"` and `"x"` are the same
/// tag here.
pub fn matches_if_none_match(headers: &HeaderMap, tag: &str) -> bool {
    let Some(value) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    if value.trim() == "*" {
        return true;
    }
    value
        .split(',')
        .map(|candidate| candidate.trim().trim_start_matches("W/"))
        .any(|candidate| candidate == tag)
}

/// A JSON response carrying an entity tag, collapsing to `304` when the client is up to date.
///
/// With caching switched off the response carries **no** `ETag` at all, rather than one the VTN
/// then refuses to honour. Emitting a validator and ignoring it is worse than emitting none: a
/// client stores the tag, revalidates for ever, and never gets a `304` — so it pays the extra
/// header on every request in exchange for nothing, and an operator reading the response would
/// reasonably conclude that caching was on.
#[derive(Debug)]
pub struct Cached {
    body: Vec<u8>,
    etag: Option<String>,
    not_modified: bool,
}

impl Cached {
    /// Build from a serializable value.
    pub fn json<T: serde::Serialize>(
        value: &T,
        request_headers: &HeaderMap,
        enabled: bool,
    ) -> Result<Self, serde_json::Error> {
        let body = serde_json::to_vec(value)?;
        if !enabled {
            return Ok(Self {
                body,
                etag: None,
                not_modified: false,
            });
        }
        let etag = entity_tag(&body);
        let not_modified = matches_if_none_match(request_headers, &etag);
        Ok(Self {
            body,
            etag: Some(etag),
            not_modified,
        })
    }

    /// The opaque entity tag, quoted and without the `W/` prefix; `None` when caching is off.
    pub fn etag(&self) -> Option<&str> {
        self.etag.as_deref()
    }
}

/// What every read says about caching.
///
/// `private` because the body is a function of the caller: object privacy hides targets a *reader*
/// was not granted, so one VEN's page of events and another's differ at the same URL. RFC 9111 §3.5
/// already stops a shared cache storing a response to a request that carried `Authorization`, but a
/// VTN in public-tariff mode is read without one — and there the header is the only thing between
/// two anonymous readers with different `?targets=` and a proxy that thinks a URL is a key.
///
/// `no-cache` does *not* mean "do not store": it means "revalidate before reuse", which is exactly
/// what the `ETag` makes cheap. A poll that would have transferred the whole collection transfers
/// a conditional request and a `304`.
const CACHE_CONTROL: &str = "private, no-cache";

impl IntoResponse for Cached {
    fn into_response(self) -> Response {
        let mut headers = HeaderMap::new();
        if let Some(etag) = &self.etag
            && let Ok(v) = HeaderValue::from_str(&header_value(etag))
        {
            headers.insert(header::ETAG, v);
        }
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(CACHE_CONTROL),
        );
        // Belt as well as braces: `private` keeps a conformant shared cache out, and `Vary` keeps
        // one that stores the response anyway from serving it to a different identity.
        headers.insert(header::VARY, HeaderValue::from_static("authorization"));

        if self.not_modified {
            // RFC 9110: a 304 carries the validators and no body.
            return (StatusCode::NOT_MODIFIED, headers).into_response();
        }

        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        (StatusCode::OK, headers, self.body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(if_none_match: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_str(if_none_match).unwrap(),
        );
        h
    }

    #[test]
    fn identical_bodies_produce_identical_tags() {
        assert_eq!(entity_tag(b"{\"a\":1}"), entity_tag(b"{\"a\":1}"));
        assert_ne!(entity_tag(b"{\"a\":1}"), entity_tag(b"{\"a\":2}"));
    }

    #[test]
    fn tags_are_quoted_and_short() {
        let tag = entity_tag(b"body");
        assert!(tag.starts_with('"') && tag.ends_with('"'));
        assert_eq!(tag.len(), 34);
    }

    #[test]
    fn if_none_match_handles_lists_wildcards_and_weak_tags() {
        let tag = entity_tag(b"body");
        assert!(matches_if_none_match(&headers(&tag), &tag));
        assert!(matches_if_none_match(&headers("*"), &tag));
        assert!(matches_if_none_match(
            &headers(&format!("\"other\", {tag}")),
            &tag
        ));
        assert!(matches_if_none_match(&headers(&format!("W/{tag}")), &tag));
        assert!(!matches_if_none_match(&headers("\"stale\""), &tag));
        assert!(!matches_if_none_match(&HeaderMap::new(), &tag));
    }

    #[test]
    fn a_matching_tag_yields_304_with_no_body() {
        let value = serde_json::json!({"programName": "tou"});
        let first = Cached::json(&value, &HeaderMap::new(), true).unwrap();
        let tag = first.etag().expect("caching is on").to_string();

        let second = Cached::json(&value, &headers(&tag), true).unwrap();
        let response = second.into_response();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    }

    #[test]
    fn the_header_is_weak_and_the_client_may_send_either_form() {
        // The response passes through a compression layer that rewrites the body, so a strong tag
        // would claim two different representations are byte-identical. Weak comparison is what
        // `If-None-Match` uses anyway, so no `304` is lost.
        let value = serde_json::json!({"programName": "tou"});
        let cached = Cached::json(&value, &HeaderMap::new(), true).unwrap();
        let tag = cached.etag().expect("caching is on").to_string();
        let response = cached.into_response();
        let sent = response
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(sent, format!("W/{tag}"));

        // Both forms revalidate.
        for candidate in [tag.clone(), format!("W/{tag}")] {
            let response = Cached::json(&value, &headers(&candidate), true)
                .unwrap()
                .into_response();
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{candidate}");
        }
    }

    #[test]
    fn caching_off_means_no_validator_at_all() {
        // Not merely "no 304". A tag the VTN would never honour costs the client a header on every
        // request and buys it nothing, and it tells anyone reading the response that caching is on.
        let value = serde_json::json!({"programName": "tou"});
        let tag = entity_tag(&serde_json::to_vec(&value).unwrap());
        let cached = Cached::json(&value, &headers(&tag), false).unwrap();
        assert_eq!(cached.etag(), None);
        let response = cached.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers().get(header::ETAG).is_none(),
            "an ETag was sent by a VTN that ignores If-None-Match"
        );
    }
}
