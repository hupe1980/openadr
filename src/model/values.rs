//! Payload values: `valuesMap`, `point`, units, and the numeric policy.
//!
//! ## Why `Decimal` and not `f64`
//!
//! Payload values are prices, fees, capacities and percentages. Adding them up in binary floating
//! point produces the classic `0.1 + 0.2 != 0.3` artefacts, which in this domain means a settlement
//! discrepancy. [`Value::Number`] therefore holds a [`rust_decimal::Decimal`] and all arithmetic in
//! this crate is exact base-10 arithmetic.
//!
//! On the wire a value stays a JSON number (never a string — that would break every other
//! implementation). The JSON bridge is exact for up to 15 significant digits, which is far beyond
//! anything the enumerations describe; [`Value::Number`] documents the boundary.

use crate::std_shim::{String, ToString, Vec, format};
use core::{fmt, str::FromStr};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, MapAccess, Visitor},
    ser::SerializeStruct,
};

use super::ids::IdentifierError;

/// A `point`: a pair of coordinates, used for volt-var and volt-watt curves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Point {
    /// Value on the x axis.
    #[serde(with = "decimal_as_number")]
    pub x: Decimal,
    /// Value on the y axis.
    #[serde(with = "decimal_as_number")]
    pub y: Decimal,
}

impl Point {
    /// Build a point.
    pub fn new(x: Decimal, y: Decimal) -> Self {
        Self { x, y }
    }
}

/// One entry of a `valuesMap.values` array.
///
/// The specification types this as `anyOf: [number, integer, string, boolean, point]`. Which
/// variant is legal depends on the payload `type`; see [`crate::schema`].
///
/// # Equality
///
/// Equality is **semantic, not representational**: `Integer(60)` equals `Number(60)`. JSON has a
/// single number type, so `60` and `60.0` are the same value, and which variant a parse produces
/// depends only on how the peer happened to write it. Comparing representations instead would make
/// a round trip through JSON fail to equal itself.
#[derive(Debug, Clone)]
pub enum Value {
    /// A JSON integer.
    Integer(i64),
    /// A JSON number, held as an exact decimal.
    ///
    /// Exact end to end for anything JSON itself represents exactly, which is every price, capacity
    /// and percentage the enumerations describe. The bound is the format's, not the type's: a JSON
    /// number crosses the wire as an IEEE-754 double, so **beyond about 15 significant digits a
    /// round trip rounds**. Arithmetic on the parsed value is exact base-10 arithmetic — which is
    /// the point, since that is where `0.1 + 0.2 != 0.3` would otherwise bite.
    ///
    /// Exchanging longer values would mean `serde_json/arbitrary_precision`, a global feature that
    /// changes `serde_json::Value` for every crate in the dependency graph.
    Number(Decimal),
    /// A JSON boolean.
    Boolean(bool),
    /// A JSON string — an enumerated tag such as `"BAD"`, or a private extension value.
    String(String),
    /// A curve point.
    Point(Point),
}

impl Value {
    /// Interpret as a decimal, widening integers.
    pub fn as_decimal(&self) -> Option<Decimal> {
        match self {
            Value::Integer(i) => Some(Decimal::from(*i)),
            Value::Number(d) => Some(*d),
            _ => None,
        }
    }

    /// Interpret as an integer, accepting decimals with no fractional part.
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Value::Integer(i) => Some(*i),
            Value::Number(d) if d.fract().is_zero() => d.to_i64(),
            _ => None,
        }
    }

    /// Borrow as a string, if this is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// A short name for the JSON type, for error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Integer(_) => "integer",
            Value::Number(_) => "number",
            Value::Boolean(_) => "boolean",
            Value::String(_) => "string",
            Value::Point(_) => "point",
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            // Numeric variants compare by value, across the integer/number split.
            (Value::Integer(_) | Value::Number(_), Value::Integer(_) | Value::Number(_)) => {
                self.as_decimal() == other.as_decimal()
            }
            (Value::Boolean(a), Value::Boolean(b)) => a == b,
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Point(a), Value::Point(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for Value {}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Integer(v)
    }
}
impl From<Decimal> for Value {
    fn from(v: Decimal) -> Self {
        Value::Number(v)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Boolean(v)
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::String(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::String(v.to_string())
    }
}
impl From<Point> for Value {
    fn from(v: Point) -> Self {
        Value::Point(v)
    }
}

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Value::Integer(i) => s.serialize_i64(*i),
            Value::Number(d) => decimal_as_number::serialize(d, s),
            Value::Boolean(b) => s.serialize_bool(*b),
            Value::String(v) => s.serialize_str(v),
            Value::Point(p) => {
                let mut st = s.serialize_struct("point", 2)?;
                st.serialize_field("x", &DecimalNumber(p.x))?;
                st.serialize_field("y", &DecimalNumber(p.y))?;
                st.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;

        impl<'de> Visitor<'de> for V {
            type Value = Value;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a number, integer, string, boolean or point")
            }

            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
                Ok(Value::Boolean(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
                Ok(Value::Integer(v))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
                i64::try_from(v).map(Value::Integer).map_err(|_| {
                    E::custom(format!(
                        "integer {v} does not fit in a signed 64-bit integer"
                    ))
                })
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
                decimal_from_f64::<E>(v).map(Value::Number)
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
                Ok(Value::String(v.to_string()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
                Ok(Value::String(v))
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Value, M::Error> {
                let mut x: Option<Decimal> = None;
                let mut y: Option<Decimal> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "x" => x = Some(map.next_value::<DecimalNumber>()?.0),
                        "y" => y = Some(map.next_value::<DecimalNumber>()?.0),
                        // Unknown members are ignored, per the model-extension rule.
                        _ => {
                            let _ = map.next_value::<de::IgnoredAny>()?;
                        }
                    }
                }
                match (x, y) {
                    (Some(x), Some(y)) => Ok(Value::Point(Point { x, y })),
                    _ => Err(de::Error::custom("a point requires both `x` and `y`")),
                }
            }
        }

        d.deserialize_any(V)
    }
}

/// A `Decimal` that serializes as a JSON number rather than a string.
#[derive(Debug, Clone, Copy, PartialEq)]
struct DecimalNumber(Decimal);

impl Serialize for DecimalNumber {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        decimal_as_number::serialize(&self.0, s)
    }
}

impl<'de> Deserialize<'de> for DecimalNumber {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        decimal_as_number::deserialize(d).map(DecimalNumber)
    }
}

fn decimal_from_f64<E: de::Error>(v: f64) -> Result<Decimal, E> {
    Decimal::try_from(v).map_err(|_| E::custom(format!("{v} is not representable as a decimal")))
}

/// Serialize a [`Decimal`] as a JSON number.
///
/// `rust_decimal` serializes as a *string* by default, which no other OpenADR implementation would
/// accept, so every decimal field routes through here.
///
/// A value with no fractional part is emitted as an integer, because that is what the worked
/// examples show; anything else goes through `f64`, which is where the fifteen-digit bound in
/// [`Value::Number`] comes from.
pub(crate) mod decimal_as_number {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(d: &Decimal, s: S) -> Result<S::Ok, S::Error> {
        if d.fract().is_zero()
            && let Some(i) = d.to_i64()
        {
            return s.serialize_i64(i);
        }
        let f = d.to_f64().ok_or_else(|| {
            serde::ser::Error::custom(format!("{d} is not representable in JSON"))
        })?;
        s.serialize_f64(f)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Decimal, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Decimal;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON number")
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Decimal, E> {
                Ok(Decimal::from(v))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Decimal, E> {
                Ok(Decimal::from(v))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Decimal, E> {
                decimal_from_f64(v)
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Decimal, E> {
                // Tolerated on input for interoperability with implementations that quote numbers.
                Decimal::from_str_exact(v)
                    .map_err(|_| E::custom(format!("{v:?} is not a decimal number")))
            }
        }
        d.deserialize_any(V)
    }
}

/// The `type` of a `valuesMap` entry, or the `payloadType` of a descriptor.
///
/// Kept as a validated string rather than a closed enum: the specification explicitly allows
/// privately agreed values here (Definitions §Private Strings). [`crate::schema`] maps the known
/// names to their value constraints.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct PayloadType(String);

impl PayloadType {
    /// Validate and wrap.
    pub fn new(value: impl Into<String>) -> Result<Self, IdentifierError> {
        let value = value.into();
        let len = value.len();
        if !(1..=128).contains(&len) {
            return Err(IdentifierError::Length {
                kind: "PayloadType",
                len,
                min: 1,
                max: 128,
            });
        }
        Ok(Self(value))
    }

    /// Borrow as `&str`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for PayloadType {
    type Err = IdentifierError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl fmt::Display for PayloadType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for PayloadType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PayloadType({:?})", self.0)
    }
}

impl<'de> Deserialize<'de> for PayloadType {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

/// Unit of measure for a payload descriptor (`eventPayloadDescriptor.units`).
///
/// The enumeration is closed in `units.schema.yaml` but the Definitions permit private strings, so
/// an unrecognised value becomes [`Unit::Private`] rather than an error.
///
/// The variants are spelled out here for the ergonomics of `Unit::Kwh`, and a test asserts that
/// they are exactly [`schema::units`](crate::schema::units) — which `cargo xtask codegen` generates
/// from the Alliance's own file. A unit added upstream therefore fails CI here instead of silently
/// degrading to `Private`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[allow(missing_docs)]
pub enum Unit {
    /// Kilowatt-hours.
    Kwh,
    /// Therms (100k BTU).
    Therms,
    /// Greenhouse gas emissions, g/kWh.
    Ghg,
    Volts,
    Amps,
    Celsius,
    Fahrenheit,
    /// A fraction in -1.0..=1.0 representing -100%..=+100%.
    Percent,
    /// Kilowatts.
    Kw,
    Kvah,
    Kvarh,
    Kva,
    Kvar,
    /// A privately agreed unit.
    Private(String),
}

impl Unit {
    /// The wire spelling.
    pub fn as_str(&self) -> &str {
        match self {
            Unit::Kwh => "KWH",
            Unit::Therms => "THERMS",
            Unit::Ghg => "GHG",
            Unit::Volts => "VOLTS",
            Unit::Amps => "AMPS",
            Unit::Celsius => "CELSIUS",
            Unit::Fahrenheit => "FAHRENHEIT",
            Unit::Percent => "PERCENT",
            Unit::Kw => "KW",
            Unit::Kvah => "KVAH",
            Unit::Kvarh => "KVARH",
            Unit::Kva => "KVA",
            Unit::Kvar => "KVAR",
            Unit::Private(s) => s,
        }
    }

    /// Whether this unit is one the specification enumerates.
    pub fn is_standard(&self) -> bool {
        !matches!(self, Unit::Private(_))
    }

    /// Every unit the specification enumerates.
    pub fn standard() -> impl Iterator<Item = Unit> {
        crate::schema::units().iter().map(|s| Unit::from(*s))
    }
}

impl FromStr for Unit {
    type Err = core::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Unit::from(s))
    }
}

impl fmt::Display for Unit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Unit {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl From<&str> for Unit {
    fn from(s: &str) -> Self {
        match s {
            "KWH" => Unit::Kwh,
            "THERMS" => Unit::Therms,
            "GHG" => Unit::Ghg,
            "VOLTS" => Unit::Volts,
            "AMPS" => Unit::Amps,
            "CELSIUS" => Unit::Celsius,
            "FAHRENHEIT" => Unit::Fahrenheit,
            "PERCENT" => Unit::Percent,
            "KW" => Unit::Kw,
            "KVAH" => Unit::Kvah,
            "KVARH" => Unit::Kvarh,
            "KVA" => Unit::Kva,
            "KVAR" => Unit::Kvar,
            other => Unit::Private(other.to_string()),
        }
    }
}

impl<'de> Deserialize<'de> for Unit {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Unit::from(String::deserialize(d)?.as_str()))
    }
}

/// How a reported value was obtained (`reportPayloadDescriptor.readingType`).
///
/// Open in the same way [`Unit`] is, and for the same reason: `reading-types.schema.yaml` closes
/// the list, the Definitions permit private strings, and a value outside the list is preserved
/// rather than refused. Before this was a type it was a bare `String`, so `DIRECT-READ` — a hyphen
/// where an underscore belongs — travelled to every VEN as a reading type nothing would match.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[allow(missing_docs)]
pub enum ReadingType {
    /// Measured directly from the resource.
    DirectRead,
    /// Estimated where no direct read was available.
    Estimated,
    /// The sum of several sources.
    Summed,
    Mean,
    Peak,
    /// A forecast rather than a measurement.
    Forecast,
    Average,
    /// Operating normally, following no demand-response directive.
    Normal,
    /// The resource reported an error or is unreachable.
    Error,
    /// A privately agreed reading type.
    Private(String),
}

impl ReadingType {
    /// The wire spelling.
    pub fn as_str(&self) -> &str {
        match self {
            ReadingType::DirectRead => "DIRECT_READ",
            ReadingType::Estimated => "ESTIMATED",
            ReadingType::Summed => "SUMMED",
            ReadingType::Mean => "MEAN",
            ReadingType::Peak => "PEAK",
            ReadingType::Forecast => "FORECAST",
            ReadingType::Average => "AVERAGE",
            ReadingType::Normal => "NORMAL",
            ReadingType::Error => "ERROR",
            ReadingType::Private(s) => s,
        }
    }

    /// Whether this is one the specification enumerates.
    pub fn is_standard(&self) -> bool {
        !matches!(self, ReadingType::Private(_))
    }

    /// Every reading type the specification enumerates.
    pub fn standard() -> impl Iterator<Item = ReadingType> {
        crate::schema::reading_types()
            .iter()
            .map(|s| ReadingType::from(*s))
    }
}

impl From<&str> for ReadingType {
    fn from(s: &str) -> Self {
        match s {
            "DIRECT_READ" => ReadingType::DirectRead,
            "ESTIMATED" => ReadingType::Estimated,
            "SUMMED" => ReadingType::Summed,
            "MEAN" => ReadingType::Mean,
            "PEAK" => ReadingType::Peak,
            "FORECAST" => ReadingType::Forecast,
            "AVERAGE" => ReadingType::Average,
            "NORMAL" => ReadingType::Normal,
            "ERROR" => ReadingType::Error,
            other => ReadingType::Private(other.to_string()),
        }
    }
}

impl FromStr for ReadingType {
    type Err = core::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(ReadingType::from(s))
    }
}

impl fmt::Display for ReadingType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ReadingType {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ReadingType {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(ReadingType::from(String::deserialize(d)?.as_str()))
    }
}

/// One or more values associated with a type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValuesMap {
    /// The nature of the values — an enumerated or private payload type.
    #[serde(rename = "type")]
    pub value_type: PayloadType,
    /// The data points. Most often a single value such as a price.
    pub values: Vec<Value>,
}

impl ValuesMap {
    /// Build a values map.
    pub fn new(value_type: PayloadType, values: Vec<Value>) -> Self {
        Self { value_type, values }
    }

    /// Build a values map holding a single value.
    pub fn single(value_type: PayloadType, value: impl Into<Value>) -> Self {
        Self {
            value_type,
            values: crate::std_shim::vec![value.into()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_round_trip_exactly() {
        for literal in ["0.17", "1234.56789", "0", "-3.5", "99999.999999"] {
            let v: Value = serde_json::from_str(literal).unwrap();
            assert_eq!(
                serde_json::to_string(&v).unwrap(),
                literal,
                "round trip of {literal}"
            );
        }
    }

    #[test]
    fn decimal_arithmetic_is_exact_where_floats_are_not() {
        let a = serde_json::from_str::<Value>("0.1")
            .unwrap()
            .as_decimal()
            .unwrap();
        let b = serde_json::from_str::<Value>("0.2")
            .unwrap()
            .as_decimal()
            .unwrap();
        let c = serde_json::from_str::<Value>("0.3")
            .unwrap()
            .as_decimal()
            .unwrap();
        assert_eq!(a + b, c);
        // The same sum in binary floating point does not hold:
        assert_ne!(0.1f64 + 0.2f64, 0.3f64);
    }

    #[test]
    fn a_number_and_an_integer_of_the_same_value_are_equal() {
        // JSON has one number type; `0` and `0.0` are the same value, and a round trip through the
        // wire may return either variant.
        assert_eq!(Value::Integer(60), Value::Number(Decimal::from(60)));
        assert_eq!(
            serde_json::from_str::<Value>("0").unwrap(),
            Value::Number(Decimal::ZERO)
        );
        assert_ne!(Value::Integer(60), Value::Number(Decimal::new(601, 1)));
        assert_ne!(Value::Integer(1), Value::Boolean(true));
    }

    #[test]
    fn integers_stay_integers() {
        let v: Value = serde_json::from_str("3").unwrap();
        assert_eq!(v, Value::Integer(3));
        assert_eq!(serde_json::to_string(&v).unwrap(), "3");
    }

    #[test]
    fn points_round_trip() {
        let json = r#"{"x":1.5,"y":-2.25}"#;
        let v: Value = serde_json::from_str(json).unwrap();
        assert!(matches!(v, Value::Point(_)));
        assert_eq!(serde_json::to_string(&v).unwrap(), json);
    }

    #[test]
    fn strings_and_booleans_are_distinguished() {
        assert_eq!(
            serde_json::from_str::<Value>("\"BAD\"").unwrap(),
            Value::String("BAD".into())
        );
        assert_eq!(
            serde_json::from_str::<Value>("true").unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn unknown_units_are_preserved_not_rejected() {
        let u: Unit = serde_json::from_str("\"FURLONGS_PER_FORTNIGHT\"").unwrap();
        assert!(!u.is_standard());
        assert_eq!(
            serde_json::to_string(&u).unwrap(),
            "\"FURLONGS_PER_FORTNIGHT\""
        );
    }

    #[test]
    fn the_unit_enum_is_exactly_the_generated_list() {
        // A hand-written enum beside a generated table is a second copy of a rule, and the only
        // thing that keeps the two from drifting is this. A unit the Alliance adds arrives in
        // `schema::units()` through `cargo xtask codegen` and fails here until it has a variant —
        // rather than silently becoming `Private` in every deployment.
        let generated: Vec<&str> = crate::schema::units().to_vec();
        let modelled: Vec<String> = Unit::standard().map(|u| u.as_str().to_string()).collect();
        assert_eq!(
            modelled, generated,
            "Unit and units.schema.yaml disagree; add or remove a variant in model::values"
        );
        assert!(
            Unit::standard().all(|u| u.is_standard()),
            "a generated unit did not map to a named variant"
        );
    }

    #[test]
    fn the_reading_type_enum_is_exactly_the_generated_list() {
        let generated: Vec<&str> = crate::schema::reading_types().to_vec();
        let modelled: Vec<String> = ReadingType::standard()
            .map(|r| r.as_str().to_string())
            .collect();
        assert_eq!(
            modelled, generated,
            "ReadingType and reading-types.schema.yaml disagree"
        );
        assert!(ReadingType::standard().all(|r| r.is_standard()));
    }

    #[test]
    fn unknown_reading_types_are_preserved_not_rejected() {
        let r: ReadingType = serde_json::from_str("\"CHICKEN_ENTRAILS\"").unwrap();
        assert!(!r.is_standard());
        assert_eq!(serde_json::to_string(&r).unwrap(), "\"CHICKEN_ENTRAILS\"");
    }

    #[test]
    fn values_map_round_trips() {
        let json = r#"{"type":"PRICE","values":[0.17]}"#;
        let vm: ValuesMap = serde_json::from_str(json).unwrap();
        assert_eq!(vm.value_type.as_str(), "PRICE");
        assert_eq!(serde_json::to_string(&vm).unwrap(), json);
    }
}
