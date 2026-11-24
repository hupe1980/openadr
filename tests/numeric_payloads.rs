//! Consumer regressions for numeric payloads with either serde_json number representation.

use openadr::model::{Event, Point, Value};
use rust_decimal::Decimal;

#[test]
fn cap_and_curve_survive_a_flattened_event() {
    let json = r#"{
        "id":"e1","createdDateTime":"2026-02-10T16:00:00Z",
        "modificationDateTime":"2026-02-10T16:00:00Z","programID":"p1",
        "intervals":[{"id":0,"payloads":[
            {"type":"IMPORT_CAPACITY_LIMIT","values":[7.56]},
            {"type":"CURVE","values":[{"y":-2.25,"extra":[0],"x":1.5}]}
        ]}]
    }"#;
    let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
    for event in [
        serde_json::from_slice::<Event>(json.as_bytes()).unwrap(),
        serde_json::from_value::<Event>(parsed).unwrap(),
    ] {
        let payloads = &event.content.intervals.as_ref().unwrap()[0].payloads;
        assert_eq!(
            payloads[0].values[0].as_decimal(),
            Some(Decimal::new(756, 2))
        );
        assert_eq!(
            payloads[1].values[0],
            Value::Point(Point::new(Decimal::new(15, 1), Decimal::new(-225, 2)))
        );
    }
}

#[test]
fn scalar_numbers_and_coordinates_use_the_same_decimal_bridge() {
    for token in ["7.56", "756e-2", "0.756E+1"] {
        let cap: Value = serde_json::from_str(token).unwrap();
        assert!(matches!(cap, Value::Number(d) if d == Decimal::new(756, 2)));
        assert_eq!(serde_json::to_string(&cap).unwrap(), "7.56");
        let point: Point =
            serde_json::from_str(&format!(r#"{{"x":{token},"y":"-2.25"}}"#)).unwrap();
        assert_eq!(
            serde_json::to_string(&point).unwrap(),
            r#"{"x":7.56,"y":-2.25}"#
        );
    }
    assert!(matches!(
        serde_json::from_str::<Value>(r#""7.56""#).unwrap(),
        Value::String(_)
    ));
    assert!(matches!(
        serde_json::from_str::<Value>("7").unwrap(),
        Value::Integer(7)
    ));
    // 032-numeric-payloads/SR-002
    assert!(serde_json::from_str::<Value>("9223372036854775808").is_err());
    assert!(serde_json::from_str::<Value>("1e100").is_err());

    // When serde_json retains a wide integer token, both entry points can detect overflow.
    if let Some(wide) = serde_json::Number::from_u128(u128::from(u64::MAX) + 1) {
        let point = format!(r#"{{"x":{wide},"y":1}}"#);
        assert!(serde_json::from_str::<Point>(&point).is_err());
        assert!(serde_json::from_str::<Value>(&point).is_err());
        assert!(serde_json::from_str::<Value>(&wide.to_string()).is_err());
        assert!(serde_json::from_value::<Value>(wide.into()).is_err());
    }
}

#[test]
fn malformed_points_fail_for_both_public_types() {
    // 032-numeric-payloads/SR-001
    for json in [
        "{}",
        r#"{"x":1.5}"#,
        r#"{"y":-2.25}"#,
        r#"{"extra":"7.56","x":1.5}"#,
        r#"{"x":1,"x":2,"y":3}"#,
        r#"{"x":1,"y":2,"y":3}"#,
        r#"{"x":{},"y":2}"#,
        r#"{"x":1e100,"y":2}"#,
        r#"{"$serde_json::private::Number":"7.56","x":1}"#,
        r#"{"x":{"$serde_json::private::Number":"7.56","extra":1},"y":2}"#,
    ] {
        assert!(serde_json::from_str::<Value>(json).is_err(), "{json}");
        assert!(serde_json::from_str::<Point>(json).is_err(), "{json}");
    }
}
