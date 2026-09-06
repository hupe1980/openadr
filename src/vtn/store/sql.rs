//! Encoding shared by the SQL backends.
//!
//! Small, but deliberately in one place. `stamp` in particular decides whether text ordering means
//! chronological ordering, and two copies of it are two chances for one to drift — which would
//! reorder pages on one backend and not the other, silently, in a way the conformance suite would
//! catch only if it happened to be the backend under test.

use crate::model::{ObjectId, ObjectType, Timestamp};
use crate::vtn::notify::Route;

use super::StorageError;

/// The three nullable outbox columns a [`Route`] occupies.
///
/// The columns predate the type and are kept, because they are what an operator greps. The
/// invariant they cannot express — that exactly one route is present — lives in `Route`, so it is
/// asserted once here rather than assumed at every read.
pub(crate) fn route_columns(route: &Route) -> (Option<&str>, Option<&str>, Option<&str>) {
    match route {
        Route::Webhook {
            callback_url,
            bearer_token,
        } => (Some(callback_url.as_str()), bearer_token.as_deref(), None),
        Route::Topic { topic } => (None, None, Some(topic.as_str())),
    }
}

/// Rebuild a [`Route`] from those columns.
///
/// A row that names neither is a corrupt row, and it is reported as such: silently dropping it
/// would remove a notification from the queue with nothing anywhere saying so.
pub(crate) fn route_from_columns(
    callback_url: Option<String>,
    bearer_token: Option<String>,
    topic: Option<String>,
) -> Result<Route, StorageError> {
    match (callback_url, topic) {
        (Some(callback_url), None) => Ok(Route::Webhook {
            callback_url,
            bearer_token,
        }),
        (None, Some(topic)) => Ok(Route::Topic { topic }),
        (callback_url, topic) => Err(StorageError::Unavailable(format!(
            "outbox row names {} route(s); exactly one is required",
            usize::from(callback_url.is_some()) + usize::from(topic.is_some())
        ))),
    }
}

/// Serialise an object's document column.
pub(crate) fn encode(value: &impl serde::Serialize) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|e| StorageError::Unavailable(e.to_string()))
}

/// Read an object's document column back.
pub(crate) fn decode<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, StorageError> {
    serde_json::from_str(raw).map_err(|e| StorageError::Unavailable(e.to_string()))
}

/// Read a timestamp column.
pub(crate) fn time(raw: &str) -> Result<Timestamp, StorageError> {
    raw.parse()
        .map_err(|_| StorageError::Unavailable(format!("unreadable timestamp {raw:?}")))
}

/// Render a timestamp so that text ordering *is* chronological ordering.
///
/// `Timestamp`'s own `Display` prints the fewest fractional digits it needs, so `…:00.5Z` and
/// `…:00.55Z` come out with different widths — and compared as text the longer one sorts *first*,
/// which is backwards. Every timestamp column here is ordered and range-compared as text, so a
/// variable-width rendering silently reorders pages and skews the `?active=` window. Nine digits,
/// always.
pub(crate) fn stamp(t: Timestamp) -> String {
    t.strftime("%Y-%m-%dT%H:%M:%S.%9fZ").to_string()
}

/// When a cut-off subscriber's next probe is allowed, as a stored timestamp.
///
/// `None` only if the cooldown cannot be added to `now`, which needs a clock at the end of the
/// representable range. A row with no `retry_at` reads as *closed*, which is the safe fallback: it
/// costs deliveries rather than losing them.
pub(crate) fn cooldown_end(
    now: Timestamp,
    policy: &super::BreakerPolicy,
) -> Option<crate::std_shim::String> {
    let seconds = i64::try_from(policy.cooldown.as_secs()).unwrap_or(i64::MAX);
    now.checked_add(jiff::Span::new().try_seconds(seconds).ok()?)
        .ok()
        .map(stamp)
}

/// Read an identifier column.
pub(crate) fn id(raw: &str) -> Result<ObjectId, StorageError> {
    ObjectId::new(raw).map_err(|e| StorageError::Unavailable(e.to_string()))
}

/// The error for an object that is not there.
pub(crate) fn not_found(object_type: ObjectType, id: &ObjectId) -> StorageError {
    StorageError::NotFound {
        object_type,
        id: id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_ordering_of_stamps_matches_chronological_ordering() {
        // The exact case that was wrong: `Display` gives "…00.5Z" and "…00.55Z", and as text the
        // second sorts first.
        let mut stamps: Vec<Timestamp> = [
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:00.5Z",
            "2026-01-01T00:00:00.55Z",
            "2026-01-01T00:00:00.123456789Z",
            "2026-01-01T00:00:01Z",
        ]
        .iter()
        .map(|s| s.parse().unwrap())
        .collect();
        stamps.sort();

        let mut rendered: Vec<String> = stamps.iter().copied().map(stamp).collect();
        let expected = rendered.clone();
        rendered.sort();
        assert_eq!(rendered, expected, "text order diverged from time order");

        // And every rendering is the same width, which is what makes that true.
        assert!(rendered.iter().all(|s| s.len() == expected[0].len()));
    }

    #[test]
    fn a_stamp_round_trips_exactly() {
        for raw in [
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:00.123456789Z",
            "1970-01-01T00:00:00Z",
        ] {
            let t: Timestamp = raw.parse().unwrap();
            assert_eq!(time(&stamp(t)).unwrap(), t, "{raw} did not survive storage");
        }
    }
}
