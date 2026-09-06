//! Payload typing: what values a payload type may carry.
//!
//! The Definitions document tabulates, for every enumerated payload type, how many values it takes
//! and of what kind — `SIMPLE` is one integer in `0..=3`, `PRICE` is one number, `CURVE` is a list of
//! points, `DATA_QUALITY` is one of four strings. The Alliance ships that table as JSON Schema
//! files; the `table` module is those files compiled into Rust by `cargo xtask codegen`.
//!
//! All six enumeration files are read, not two. Four of them describe a `valuesMap` — event
//! interval payloads, report payloads, programme attributes, and the attributes a `ven` or
//! `resource` carries — and become [`PayloadSpec`]s in one table keyed by [`PayloadGroup`]. The
//! other two constrain a *descriptor field* rather than a payload, and become the string tables
//! behind [`units`] and [`reading_types`]. Nothing here is hand-maintained: a value the Alliance
//! adds or a bound it tightens reaches this crate through `cargo xtask codegen`, and CI fails if it
//! has not.
//!
//! Validation is deliberately *not* mandatory. The Definitions say content validation is the
//! client's business, and privately agreed payload types are explicitly legal. So an unknown type is
//! [`Validity::Unknown`], never an error, and a VTN chooses its own [`Policy`].

use crate::std_shim::{String, ToString, Vec, format};
use core::fmt;
use rust_decimal::Decimal;

use crate::model::{Value, ValuesMap};

mod table;

/// Which family a payload type belongs to.
///
/// One per `valuesMap`-shaped enumeration file the Alliance publishes. The group is what makes
/// `USAGE` in an event interval a violation rather than a private extension: the name is known, and
/// it is known to belong somewhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PayloadGroup {
    /// Legal in an event interval (`event-interval-payloads`).
    Event,
    /// Legal in a report interval (`report-payloads`).
    Report,
    /// Legal in `program.attributes` (`program-attributes`).
    ProgramAttribute,
    /// Legal in `ven.attributes` and `resource.attributes` (`ven-resource-attributes`).
    VenAttribute,
}

impl PayloadGroup {
    /// The name used in error messages.
    pub const fn as_str(self) -> &'static str {
        match self {
            PayloadGroup::Event => "event interval",
            PayloadGroup::Report => "report interval",
            PayloadGroup::ProgramAttribute => "program attribute",
            PayloadGroup::VenAttribute => "ven or resource attribute",
        }
    }
}

impl fmt::Display for PayloadGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The set of JSON value kinds a payload accepts, as a bit set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueKinds(u8);

impl ValueKinds {
    /// Integers.
    pub const INTEGER: ValueKinds = ValueKinds(1 << 0);
    /// Numbers (integers are accepted where a number is expected).
    pub const NUMBER: ValueKinds = ValueKinds(1 << 1);
    /// Strings.
    pub const STRING: ValueKinds = ValueKinds(1 << 2);
    /// Booleans.
    pub const BOOLEAN: ValueKinds = ValueKinds(1 << 3);
    /// Curve points.
    pub const POINT: ValueKinds = ValueKinds(1 << 4);
    /// Any of the above.
    pub const ANY: ValueKinds = ValueKinds(0b11111);

    /// Union of two sets.
    pub const fn or(self, other: ValueKinds) -> ValueKinds {
        ValueKinds(self.0 | other.0)
    }

    /// Whether a value is of an accepted kind.
    pub fn accepts(self, value: &Value) -> bool {
        let bit = match value {
            // An integer satisfies a `number` constraint; JSON does not distinguish `1` from `1.0`.
            Value::Integer(_) => Self::INTEGER.0 | Self::NUMBER.0,
            Value::Number(_) => Self::NUMBER.0,
            Value::String(_) => Self::STRING.0,
            Value::Boolean(_) => Self::BOOLEAN.0,
            Value::Point(_) => Self::POINT.0,
        };
        self.0 & bit != 0
    }

    /// Human-readable list, for error messages.
    pub fn describe(self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        for (bit, name) in [
            (Self::INTEGER, "integer"),
            (Self::NUMBER, "number"),
            (Self::STRING, "string"),
            (Self::BOOLEAN, "boolean"),
            (Self::POINT, "point"),
        ] {
            if self.0 & bit.0 != 0 {
                parts.push(name);
            }
        }
        parts.join(" or ")
    }
}

/// What the specification says about one payload type.
#[derive(Debug, Clone, Copy)]
pub struct PayloadSpec {
    /// The enumerated name.
    pub name: &'static str,
    /// Which family it belongs to.
    pub group: PayloadGroup,
    /// Accepted value kinds.
    pub kinds: ValueKinds,
    /// Minimum number of values.
    pub min_items: usize,
    /// Maximum number of values, if bounded.
    pub max_items: Option<usize>,
    /// Inclusive lower bound on numeric values.
    pub minimum: Option<Decimal>,
    /// Inclusive upper bound on numeric values.
    pub maximum: Option<Decimal>,
    /// Inclusive lower bound on the length of a string value.
    pub min_length: Option<usize>,
    /// Inclusive upper bound on the length of a string value.
    pub max_length: Option<usize>,
    /// Permitted string values, when the payload is an enumeration.
    pub allowed: &'static [&'static str],
}

impl PayloadSpec {
    /// Whether this payload holds exactly one value.
    ///
    /// This is what decides whether extra values mean *sub-intervals* rather than a malformed
    /// payload: a `PRICE` interval carrying three values covers three equal sub-intervals, while a
    /// `CURVE` carrying three points is one interval with a three-point curve (User Guide §7.3).
    pub fn is_scalar(&self) -> bool {
        self.max_items == Some(1)
    }
}

/// Look up a payload type by name.
///
/// Names are unique across the four groups, which a test asserts, so a name identifies both the
/// constraints and the place the value belongs.
pub fn lookup(name: &str) -> Option<&'static PayloadSpec> {
    table::PAYLOAD_SPECS.iter().find(|s| s.name == name)
}

/// Every known payload specification.
pub fn all() -> &'static [PayloadSpec] {
    table::PAYLOAD_SPECS
}

/// Every unit of measure `eventPayloadDescriptor.units` enumerates.
///
/// Generated from `units.schema.yaml`, so [`Unit`](crate::model::Unit) cannot quietly fall behind
/// the Alliance's list. A unit outside it is legal — the Definitions permit private strings — and
/// is reported as [`Unit::Private`](crate::model::Unit::Private) rather than refused.
pub fn units() -> &'static [&'static str] {
    table::UNITS
}

/// Every reading type `reportPayloadDescriptor.readingType` enumerates.
///
/// Generated from `reading-types.schema.yaml`. As with [`units`], a value outside the list is a
/// private extension rather than an error.
pub fn reading_types() -> &'static [&'static str] {
    table::READING_TYPES
}

/// How many sub-intervals a payload implies.
///
/// Returns 1 for anything that is not a scalar payload carrying several values.
pub fn subinterval_count(values: &ValuesMap) -> usize {
    match lookup(values.value_type.as_str()) {
        // Event intervals only: subdivision is a property of an event's timing, and an attribute
        // carrying several values divides nothing.
        Some(spec)
            if spec.group == PayloadGroup::Event && spec.is_scalar() && values.values.len() > 1 =>
        {
            values.values.len()
        }
        _ => 1,
    }
}

/// The outcome of checking a payload.
#[derive(Debug, Clone, PartialEq)]
pub enum Validity {
    /// The payload matches its specification.
    Valid,
    /// The payload type is privately agreed; nothing to check against.
    Unknown,
    /// The payload contradicts its specification.
    Invalid(Vec<PayloadViolation>),
}

impl Validity {
    /// Whether this outcome should be rejected under a policy.
    ///
    /// Only a payload that contradicts its enumeration under [`Policy::Strict`] is ever rejected.
    /// An unknown payload type never is: the Definitions make private strings legal.
    pub fn is_rejected(&self, policy: Policy) -> bool {
        matches!(self, Validity::Invalid(_)) && policy == Policy::Strict
    }

    /// The violations, if any.
    pub fn violations(&self) -> &[PayloadViolation] {
        match self {
            Validity::Invalid(v) => v,
            _ => &[],
        }
    }
}

/// A single way in which a payload contradicts its specification.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PayloadViolation {
    /// Wrong number of values.
    #[error("{payload_type} takes {min}..{} values but {actual} were given", max.map(|m| m.to_string()).unwrap_or_else(|| "∞".to_string()))]
    Cardinality {
        /// The payload type.
        payload_type: String,
        /// Minimum permitted.
        min: usize,
        /// Maximum permitted, if bounded.
        max: Option<usize>,
        /// Number given.
        actual: usize,
    },
    /// A value of the wrong JSON kind.
    #[error("{payload_type}[{index}] must be {expected} but is a {actual}")]
    Kind {
        /// The payload type.
        payload_type: String,
        /// Index of the offending value.
        index: usize,
        /// What the specification permits.
        expected: String,
        /// What was given.
        actual: &'static str,
    },
    /// A numeric value outside its bounds.
    #[error("{payload_type}[{index}] = {value} is outside {bound}")]
    Range {
        /// The payload type.
        payload_type: String,
        /// Index of the offending value.
        index: usize,
        /// The value.
        value: String,
        /// A description of the bound.
        bound: String,
    },
    /// A string outside its length bounds.
    #[error("{payload_type}[{index}] is {actual} characters, outside {bound}")]
    Length {
        /// The payload type.
        payload_type: String,
        /// Index of the offending value.
        index: usize,
        /// The length that was given.
        actual: usize,
        /// A description of the bound.
        bound: String,
    },
    /// A string outside the payload's enumeration.
    #[error("{payload_type}[{index}] = {value:?} is not one of the permitted values")]
    NotEnumerated {
        /// The payload type.
        payload_type: String,
        /// Index of the offending value.
        index: usize,
        /// The value.
        value: String,
    },
    /// The payload type exists but not in this position.
    #[error("{payload_type} is a {actual} payload and cannot appear in a {expected}")]
    WrongGroup {
        /// The payload type.
        payload_type: String,
        /// Where it does belong.
        actual: PayloadGroup,
        /// Where it was found.
        expected: PayloadGroup,
    },
}

/// How strictly a VTN enforces payload typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Policy {
    /// Accept everything; do not even look.
    Off,
    /// Check and report, but accept. The default: the specification places content validation on
    /// the client, and rejecting a peer's private extension would be worse than logging it.
    #[default]
    Warn,
    /// Reject payloads that contradict the enumeration.
    Strict,
}

impl fmt::Display for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Policy::Off => "off",
            Policy::Warn => "warn",
            Policy::Strict => "strict",
        })
    }
}

impl core::str::FromStr for Policy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "off" => Ok(Policy::Off),
            "warn" => Ok(Policy::Warn),
            "strict" => Ok(Policy::Strict),
            other => Err(format!("unknown payload validation policy {other:?}")),
        }
    }
}

/// Check one payload against its specification.
pub fn validate(values: &ValuesMap, group: PayloadGroup) -> Validity {
    let name = values.value_type.as_str();
    let Some(spec) = lookup(name) else {
        return Validity::Unknown;
    };

    let mut violations = Vec::new();

    if spec.group != group {
        violations.push(PayloadViolation::WrongGroup {
            payload_type: name.to_string(),
            actual: spec.group,
            expected: group,
        });
        // Cardinality of the other family is not meaningful here; report just this.
        return Validity::Invalid(violations);
    }

    // A scalar payload carrying several values is the multi-value sub-interval form, which is legal;
    // the count constraint then applies per sub-interval rather than to the array. The question is
    // asked of `subinterval_count` rather than answered again here, because the expander asks it
    // too and two readings of "is this a subdivision" would let one accept what the other split.
    let count = values.values.len();
    let treat_as_subintervals = subinterval_count(values) > 1;
    if !treat_as_subintervals
        && (count < spec.min_items || spec.max_items.is_some_and(|max| count > max))
    {
        violations.push(PayloadViolation::Cardinality {
            payload_type: name.to_string(),
            min: spec.min_items,
            max: spec.max_items,
            actual: count,
        });
    }

    for (index, value) in values.values.iter().enumerate() {
        if !spec.kinds.accepts(value) {
            violations.push(PayloadViolation::Kind {
                payload_type: name.to_string(),
                index,
                expected: spec.kinds.describe(),
                actual: value.type_name(),
            });
            continue;
        }
        if let Some(d) = value.as_decimal() {
            if let Some(min) = spec.minimum
                && d < min
            {
                violations.push(PayloadViolation::Range {
                    payload_type: name.to_string(),
                    index,
                    value: d.to_string(),
                    bound: format!(">= {min}"),
                });
            }
            if let Some(max) = spec.maximum
                && d > max
            {
                violations.push(PayloadViolation::Range {
                    payload_type: name.to_string(),
                    index,
                    value: d.to_string(),
                    bound: format!("<= {max}"),
                });
            }
        }
        if let Some(s) = value.as_str() {
            // Character count, not bytes: `maxLength` in JSON Schema counts code points, and a
            // byte length would refuse a legal accented description one character short of the
            // bound.
            let length = s.chars().count();
            if let Some(min) = spec.min_length
                && length < min
            {
                violations.push(PayloadViolation::Length {
                    payload_type: name.to_string(),
                    index,
                    actual: length,
                    bound: format!(">= {min}"),
                });
            }
            if let Some(max) = spec.max_length
                && length > max
            {
                violations.push(PayloadViolation::Length {
                    payload_type: name.to_string(),
                    index,
                    actual: length,
                    bound: format!("<= {max}"),
                });
            }
            if !spec.allowed.is_empty() && !spec.allowed.contains(&s) {
                violations.push(PayloadViolation::NotEnumerated {
                    payload_type: name.to_string(),
                    index,
                    value: s.to_string(),
                });
            }
        }
    }

    if violations.is_empty() {
        Validity::Valid
    } else {
        Validity::Invalid(violations)
    }
}

/// Check a list of payloads, collecting every violation.
pub fn validate_all(payloads: &[ValuesMap], group: PayloadGroup) -> Vec<PayloadViolation> {
    payloads
        .iter()
        .flat_map(|p| validate(p, group).violations().to_vec())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Point, Value};
    use crate::std_shim::vec;
    use rust_decimal::Decimal;
    use rust_decimal::prelude::FromPrimitive;

    fn vm(t: &str, values: Vec<Value>) -> ValuesMap {
        ValuesMap::new(t.parse().unwrap(), values)
    }

    #[test]
    fn the_table_covers_every_enumeration_file() {
        // 38 event payload types, 24 report payload types, 8 programme attributes and 5
        // ven/resource attributes — every `valuesMap`-shaped definition the Alliance publishes.
        let count = |g| all().iter().filter(|s| s.group == g).count();
        assert_eq!(count(PayloadGroup::Event), 38);
        assert_eq!(count(PayloadGroup::Report), 24);
        assert_eq!(count(PayloadGroup::ProgramAttribute), 8);
        assert_eq!(count(PayloadGroup::VenAttribute), 5);
        assert_eq!(all().len(), 75);

        assert!(lookup("PRICE").is_some());
        assert!(lookup("IMPORT_CAPACITY_LIMIT").is_some());
        assert!(lookup("CTA2045_REBOOT").is_some());
        assert!(lookup("USAGE").is_some());
        assert!(lookup("RETAILER_NAME").is_some());
        assert!(lookup("MAX_POWER_CONSUMPTION").is_some());

        // And the descriptor enumerations, which constrain a field rather than a `valuesMap`.
        assert!(units().contains(&"KWH"));
        assert!(reading_types().contains(&"DIRECT_READ"));
    }

    #[test]
    fn payload_names_are_unique_across_groups() {
        // `lookup` is keyed by name alone, so a collision would make one group's constraints
        // silently answer for another's. Nothing in the specification promises this, so it is
        // asserted rather than assumed.
        let mut names: Vec<&str> = all().iter().map(|s| s.name).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), before, "two payload types share a name");
    }

    #[test]
    fn a_program_attribute_is_not_an_event_payload() {
        // The whole point of reading the other two files: `RETAILER_NAME` used to be an unknown
        // private string wherever it appeared, so putting it in an event interval was silently
        // fine and misspelling it in a programme was silently fine too.
        assert_eq!(
            validate(
                &vm("RETAILER_NAME", vec![Value::from("Acme Energy")]),
                PayloadGroup::ProgramAttribute
            ),
            Validity::Valid
        );
        let misplaced = validate(
            &vm("RETAILER_NAME", vec![Value::from("Acme Energy")]),
            PayloadGroup::Event,
        );
        assert!(matches!(
            misplaced.violations().first(),
            Some(PayloadViolation::WrongGroup { .. })
        ));
    }

    #[test]
    fn a_location_attribute_is_two_numbers_within_range() {
        let ok = vm(
            "LOCATION",
            vec![
                Value::Number(Decimal::from_i32(4).unwrap()),
                Value::Number(Decimal::from_i32(52).unwrap()),
            ],
        );
        assert_eq!(validate(&ok, PayloadGroup::VenAttribute), Validity::Valid);

        // One coordinate is not a location.
        let short = vm("LOCATION", vec![Value::Integer(4)]);
        assert!(matches!(
            validate(&short, PayloadGroup::VenAttribute)
                .violations()
                .first(),
            Some(PayloadViolation::Cardinality { .. })
        ));

        // And 999 is not a longitude.
        let wild = vm("LOCATION", vec![Value::Integer(999), Value::Integer(52)]);
        assert!(matches!(
            validate(&wild, PayloadGroup::VenAttribute)
                .violations()
                .first(),
            Some(PayloadViolation::Range { .. })
        ));
    }

    #[test]
    fn string_length_bounds_are_enforced() {
        // `maxLength: 128` on `ALERT_GRID_EMERGENCY` was in the schema file and in nothing else.
        let long = "x".repeat(129);
        let v = validate(
            &vm("ALERT_GRID_EMERGENCY", vec![Value::from(long.as_str())]),
            PayloadGroup::Event,
        );
        assert!(matches!(
            v.violations().first(),
            Some(PayloadViolation::Length { .. })
        ));
        assert_eq!(
            validate(
                &vm("ALERT_GRID_EMERGENCY", vec![Value::from("grid is failing")]),
                PayloadGroup::Event
            ),
            Validity::Valid
        );
    }

    #[test]
    fn an_attribute_carrying_several_values_is_not_subdivided() {
        // Sub-interval splitting is a property of an event's *timing*. A programme attribute has
        // none, so several values there are a cardinality violation and never a subdivision.
        let attribute = vm(
            "PROGRAM_LONG_NAME",
            vec![Value::from("a"), Value::from("b")],
        );
        assert_eq!(subinterval_count(&attribute), 1);
        assert!(matches!(
            validate(&attribute, PayloadGroup::ProgramAttribute)
                .violations()
                .first(),
            Some(PayloadViolation::Cardinality { .. })
        ));
    }

    #[test]
    fn simple_is_bounded_to_zero_through_three() {
        assert_eq!(
            validate(&vm("SIMPLE", vec![Value::Integer(2)]), PayloadGroup::Event),
            Validity::Valid
        );
        let bad = validate(&vm("SIMPLE", vec![Value::Integer(9)]), PayloadGroup::Event);
        assert!(matches!(
            bad.violations().first(),
            Some(PayloadViolation::Range { .. })
        ));
    }

    #[test]
    fn price_rejects_a_string() {
        let v = validate(
            &vm("PRICE", vec![Value::from("cheap")]),
            PayloadGroup::Event,
        );
        assert!(matches!(
            v.violations().first(),
            Some(PayloadViolation::Kind { .. })
        ));
    }

    #[test]
    fn an_integer_satisfies_a_number_constraint() {
        assert_eq!(
            validate(&vm("PRICE", vec![Value::Integer(0)]), PayloadGroup::Event),
            Validity::Valid
        );
    }

    #[test]
    fn private_payload_types_are_unknown_not_invalid() {
        let v = validate(
            &vm("PRIVATE_ALGORITHM", vec![Value::from("whatever")]),
            PayloadGroup::Event,
        );
        assert_eq!(v, Validity::Unknown);
        assert!(!v.is_rejected(Policy::Strict));
    }

    #[test]
    fn multi_value_scalar_payloads_are_sub_intervals_not_errors() {
        let three_prices = vm(
            "PRICE",
            vec![
                Value::Number(Decimal::from_f64(0.17).unwrap()),
                Value::Number(Decimal::from_f64(0.03).unwrap()),
                Value::Number(Decimal::from_f64(0.11).unwrap()),
            ],
        );
        assert_eq!(
            validate(&three_prices, PayloadGroup::Event),
            Validity::Valid
        );
        assert_eq!(subinterval_count(&three_prices), 3);
    }

    #[test]
    fn a_curve_with_many_points_is_one_interval() {
        let curve = vm(
            "CURVE",
            vec![
                Value::Point(Point::new(Decimal::from(1), Decimal::from(2))),
                Value::Point(Point::new(Decimal::from(3), Decimal::from(4))),
            ],
        );
        assert_eq!(validate(&curve, PayloadGroup::Event), Validity::Valid);
        assert_eq!(subinterval_count(&curve), 1, "a curve is not sub-divided");
    }

    #[test]
    fn enumerated_string_payloads_are_checked() {
        assert_eq!(
            validate(
                &vm("DATA_QUALITY", vec![Value::from("BAD")]),
                PayloadGroup::Report
            ),
            Validity::Valid
        );
        let bad = validate(
            &vm("DATA_QUALITY", vec![Value::from("TERRIBLE")]),
            PayloadGroup::Report,
        );
        assert!(matches!(
            bad.violations().first(),
            Some(PayloadViolation::NotEnumerated { .. })
        ));
    }

    #[test]
    fn a_report_payload_in_an_event_is_flagged() {
        let v = validate(&vm("USAGE", vec![Value::Integer(1)]), PayloadGroup::Event);
        assert!(matches!(
            v.violations().first(),
            Some(PayloadViolation::WrongGroup { .. })
        ));
    }

    #[test]
    fn policy_decides_whether_an_invalid_payload_is_fatal() {
        let invalid = validate(&vm("PRICE", vec![Value::from("x")]), PayloadGroup::Event);
        assert!(invalid.is_rejected(Policy::Strict));
        assert!(!invalid.is_rejected(Policy::Warn));
        assert!(!invalid.is_rejected(Policy::Off));
    }
}
