//! Wire adapters: making non-conformant peers parseable without weakening the model.
//!
//! Deployed profiles bend the schema. Fluvius' NetFlex profile, for instance, sends
//! `reportDescriptor.frequency` as an ISO 8601 duration where the schema says integer, and adds a
//! `required` flag that the schema does not define at all. Loosening [`ReportDescriptor`] to
//! accommodate that would weaken every other deployment.
//!
//! Instead, adapters rewrite the JSON *before* it reaches the strict model and *after* it leaves,
//! so a deviation is a named, tested, opt-in transformation rather than a hole in the type system.
//!
//! [`ReportDescriptor`]: super::ReportDescriptor

use crate::std_shim::{String, ToString, Vec, format};
use serde_json::Value as Json;

/// Prefix under which an adapter parks a value the canonical model cannot hold.
pub const PRESERVED_PREFIX: &str = "x-openadr-preserved-";

/// A bidirectional JSON rewrite applied at the edge of the API.
///
/// The two directions are named from the point of view of *this* process, not of a server: a body
/// arriving is inbound whether it is a response to a request we made or a request somebody made of
/// us. The only consumer today is [`Client`](crate::client::Client), where `inbound` runs on
/// responses and `outbound` on request bodies.
pub trait WireAdapter: core::fmt::Debug + Send + Sync {
    /// Stable name, for logs and configuration.
    fn name(&self) -> &'static str;

    /// Rewrite a body **arriving** from the peer into canonical shape.
    fn inbound(&self, _body: &mut Json) {}

    /// Rewrite a body **leaving** for the peer into the shape that peer expects.
    fn outbound(&self, _body: &mut Json) {}
}

/// The identity adapter: strict, canonical OpenADR 3.1.
#[derive(Debug, Clone, Copy, Default)]
pub struct Canonical;

impl WireAdapter for Canonical {
    fn name(&self) -> &'static str {
        "canonical"
    }
}

/// Adapter for Fluvius' NetFlex "OpenADR 3.1 Profile".
///
/// Handles two documented deviations:
///
/// * `reportDescriptor.frequency` as an ISO 8601 duration string (`"PT15M"`) instead of an interval
///   count. The string is preserved verbatim and the canonical field falls back to its default, so
///   no interval semantics are invented.
/// * `reportDescriptor.required`, a boolean the schema does not define.
///
/// Both survive a round trip: [`WireAdapter::outbound`] puts them back.
#[derive(Debug, Clone, Copy, Default)]
pub struct Fluvius;

impl Fluvius {
    const FREQUENCY: &'static str = "frequency";
    const REQUIRED: &'static str = "required";

    fn preserved_key(field: &str) -> String {
        format!("{PRESERVED_PREFIX}{field}")
    }

    fn for_each_report_descriptor(body: &mut Json, f: impl Fn(&mut serde_json::Map<String, Json>)) {
        // Report descriptors appear on events, and events appear alone, in arrays, and nested
        // inside a notification's `object`.
        fn walk(v: &mut Json, f: &(impl Fn(&mut serde_json::Map<String, Json>) + ?Sized)) {
            match v {
                Json::Array(items) => items.iter_mut().for_each(|i| walk(i, f)),
                Json::Object(map) => {
                    if let Some(Json::Array(descriptors)) = map.get_mut("reportDescriptors") {
                        for d in descriptors {
                            if let Json::Object(d) = d {
                                f(d);
                            }
                        }
                    }
                    for (_, child) in map.iter_mut() {
                        walk(child, f);
                    }
                }
                _ => {}
            }
        }
        walk(body, &f);
    }
}

impl WireAdapter for Fluvius {
    fn name(&self) -> &'static str {
        "fluvius-netflex"
    }

    fn inbound(&self, body: &mut Json) {
        Self::for_each_report_descriptor(body, |d| {
            // A string `frequency` is the profile's; move it aside so the integer field can default.
            if let Some(Json::String(s)) = d.get(Self::FREQUENCY) {
                let preserved = Json::String(s.clone());
                d.remove(Self::FREQUENCY);
                d.insert(Self::preserved_key(Self::FREQUENCY), preserved);
            }
            if let Some(flag) = d.remove(Self::REQUIRED) {
                d.insert(Self::preserved_key(Self::REQUIRED), flag);
            }
        });
    }

    fn outbound(&self, body: &mut Json) {
        Self::for_each_report_descriptor(body, |d| {
            for field in [Self::FREQUENCY, Self::REQUIRED] {
                if let Some(v) = d.remove(&Self::preserved_key(field)) {
                    d.insert(field.to_string(), v);
                }
            }
        });
    }
}

/// An ordered chain of adapters.
#[derive(Debug, Default)]
pub struct AdapterChain {
    adapters: Vec<crate::std_shim::Box<dyn WireAdapter>>,
}

impl AdapterChain {
    /// An empty chain, equivalent to [`Canonical`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an adapter.
    pub fn with(mut self, adapter: impl WireAdapter + 'static) -> Self {
        self.adapters.push(crate::std_shim::Box::new(adapter));
        self
    }

    /// Whether the chain would do anything.
    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }

    /// The adapters, in the order they were added.
    pub fn names(&self) -> impl Iterator<Item = &'static str> {
        self.adapters.iter().map(|a| a.name())
    }

    /// Apply every adapter to an arriving body, in **reverse** order.
    ///
    /// A chain is a pipeline: `[A, B]` means canonical `--A--> --B-->` wire, so undoing it is
    /// `B⁻¹` then `A⁻¹`. Applying the two directions in the same order composes the chain one way
    /// and un-composes it the other — which is invisible with a single adapter, and is exactly why
    /// this was the wrong way round until somebody wrote a test with two.
    pub fn inbound(&self, body: &mut Json) {
        for a in self.adapters.iter().rev() {
            a.inbound(body);
        }
    }

    /// Apply every adapter to a departing body, in the order they were added.
    pub fn outbound(&self, body: &mut Json) {
        for a in &self.adapters {
            a.outbound(body);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ReportDescriptor;

    /// An event as Fluvius' own documentation shows it (spec TAU341.4-K05 §5.4).
    fn fluvius_event() -> Json {
        serde_json::json!({
            "id": "event-2026-02-11-bat-001",
            "programID": "tflex-da-batteries",
            "targets": ["BATTERY-001"],
            "reportDescriptors": [
                { "payloadType": "EVENT_STATUS", "required": true },
                { "payloadType": "COMPLIANCE_STATUS", "required": true, "frequency": "PT15M" }
            ]
        })
    }

    #[test]
    fn fluvius_event_does_not_parse_canonically() {
        let d = &fluvius_event()["reportDescriptors"][1];
        assert!(
            serde_json::from_value::<ReportDescriptor>(d.clone()).is_err(),
            "a string frequency must not parse as the schema's integer"
        );
    }

    #[test]
    fn fluvius_adapter_makes_it_parse_without_inventing_semantics() {
        let mut body = fluvius_event();
        Fluvius.inbound(&mut body);

        let d: ReportDescriptor =
            serde_json::from_value(body["reportDescriptors"][1].clone()).unwrap();
        // The canonical field falls back to its schema default rather than to a guess.
        assert_eq!(d.frequency, -1);
        assert_eq!(d.payload_type.as_str(), "COMPLIANCE_STATUS");
    }

    #[test]
    fn deviations_survive_a_round_trip() {
        let original = fluvius_event();
        let mut body = original.clone();
        Fluvius.inbound(&mut body);
        Fluvius.outbound(&mut body);
        assert_eq!(body, original);
    }

    /// A chain is a pipeline, and a pipeline reverses when you run it backwards.
    ///
    /// With one adapter the two directions are indistinguishable, which is why this was the wrong
    /// way round: `inbound` and `outbound` both walked the list in the same direction, so `[A, B]`
    /// composed as `A∘B` on the way out and `A∘B` on the way back rather than `B⁻¹∘A⁻¹`. Two
    /// adapters whose rewrites touch the same field is the smallest thing that can see it.
    #[test]
    fn a_chain_of_two_undoes_itself_in_reverse() {
        /// Wraps a string field in a marker; `outbound` adds, `inbound` removes.
        #[derive(Debug)]
        struct Wrap(&'static str, &'static str);

        impl WireAdapter for Wrap {
            fn name(&self) -> &'static str {
                self.0
            }
            fn outbound(&self, body: &mut Json) {
                if let Some(Json::String(s)) = body.get_mut("v") {
                    *s = format!("{}({s})", self.1);
                }
            }
            fn inbound(&self, body: &mut Json) {
                if let Some(Json::String(s)) = body.get_mut("v") {
                    let open = format!("{}(", self.1);
                    if let Some(inner) = s.strip_prefix(&open).and_then(|r| r.strip_suffix(')')) {
                        *s = inner.to_string();
                    }
                }
            }
        }

        let chain = AdapterChain::new()
            .with(Wrap("a", "A"))
            .with(Wrap("b", "B"));
        assert_eq!(chain.names().collect::<Vec<_>>(), vec!["a", "b"]);

        // Out: A first, then B — so B is the outermost wrapper on the wire.
        let mut wire = serde_json::json!({ "v": "x" });
        chain.outbound(&mut wire);
        assert_eq!(wire["v"], "B(A(x))");

        // Back: B must come off first, or nothing comes off at all.
        chain.inbound(&mut wire);
        assert_eq!(wire["v"], "x", "the chain did not undo itself");
    }

    #[test]
    fn adapter_reaches_descriptors_nested_in_a_notification() {
        let mut body = serde_json::json!({
            "objectType": "EVENT",
            "operation": "CREATE",
            "object": fluvius_event(),
        });
        Fluvius.inbound(&mut body);
        assert!(
            body["object"]["reportDescriptors"][1]
                .get("x-openadr-preserved-frequency")
                .is_some()
        );
    }

    #[test]
    fn chain_applies_and_unapplies_symmetrically() {
        let chain = AdapterChain::new().with(Fluvius);
        let original = fluvius_event();
        let mut body = original.clone();
        chain.inbound(&mut body);
        chain.outbound(&mut body);
        assert_eq!(body, original);
    }
}
