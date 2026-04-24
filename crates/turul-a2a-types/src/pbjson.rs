//! Conversions between `serde_json` values and `pbjson_types` (proto
//! `google.protobuf.Value` / `google.protobuf.Struct`).
//!
//! These are the conversions adopters need when populating
//! `Message.metadata`, `Part.metadata`, and other proto `Struct` fields
//! from ergonomic `serde_json::Value` input. Previously each adopter
//! wrote their own `json_to_pbjson` helper; this module lifts that
//! helper into the shared crate so it can be tested once and reused.
//!
//! All conversions are total (no errors) — `serde_json` and
//! `pbjson_types` share the same `google.protobuf.Value` semantics, so
//! every JSON value has a corresponding proto `Value`. Numeric
//! precision note: proto `Value::NumberValue` is `f64`, matching
//! `serde_json::Number::as_f64()`. Integers outside the `f64` mantissa
//! (`±2^53`) lose precision in both directions. Callers that need
//! lossless large-integer round-trip should encode them as strings
//! before handing them to this module.
//!
//! # Example
//!
//! ```
//! use std::collections::HashMap;
//! use turul_a2a_types::pbjson::{json_object_to_struct, struct_to_json_object};
//!
//! let mut fields: HashMap<String, serde_json::Value> = HashMap::new();
//! fields.insert("trigger_id".into(), serde_json::json!("trig-1"));
//! fields.insert("attempt".into(), serde_json::json!(3));
//!
//! let s = json_object_to_struct(fields.clone());
//! let round_tripped = struct_to_json_object(s);
//! assert_eq!(round_tripped.get("trigger_id").unwrap(), &serde_json::json!("trig-1"));
//! ```

use std::collections::HashMap;

pub use turul_a2a_proto::pbjson_types;

/// Convert a `serde_json::Value` into a proto `Value`.
///
/// Recursive: objects become `StructValue`, arrays become `ListValue`,
/// scalars map 1:1. Numbers that don't fit in `f64` silently lose
/// precision (proto `NumberValue` is `double`).
pub fn json_to_value(v: serde_json::Value) -> pbjson_types::Value {
    use pbjson_types::value::Kind;
    let kind = match v {
        serde_json::Value::Null => Kind::NullValue(0),
        serde_json::Value::Bool(b) => Kind::BoolValue(b),
        serde_json::Value::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(s) => Kind::StringValue(s),
        serde_json::Value::Array(a) => Kind::ListValue(pbjson_types::ListValue {
            values: a.into_iter().map(json_to_value).collect(),
        }),
        serde_json::Value::Object(o) => {
            let mut fields: HashMap<String, pbjson_types::Value> = HashMap::new();
            for (k, vv) in o {
                fields.insert(k, json_to_value(vv));
            }
            Kind::StructValue(pbjson_types::Struct { fields })
        }
    };
    pbjson_types::Value { kind: Some(kind) }
}

/// Convert a proto `Value` into a `serde_json::Value`.
///
/// Inverse of [`json_to_value`] (up to `f64` precision — see module
/// docs). A `Value` with `kind = None` maps to `Value::Null`.
pub fn value_to_json(v: pbjson_types::Value) -> serde_json::Value {
    use pbjson_types::value::Kind;
    match v.kind {
        None | Some(Kind::NullValue(_)) => serde_json::Value::Null,
        Some(Kind::BoolValue(b)) => serde_json::Value::Bool(b),
        Some(Kind::NumberValue(n)) => serde_json::Number::from_f64(n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Some(Kind::StringValue(s)) => serde_json::Value::String(s),
        Some(Kind::ListValue(list)) => {
            serde_json::Value::Array(list.values.into_iter().map(value_to_json).collect())
        }
        Some(Kind::StructValue(s)) => {
            let mut map = serde_json::Map::with_capacity(s.fields.len());
            for (k, vv) in s.fields {
                map.insert(k, value_to_json(vv));
            }
            serde_json::Value::Object(map)
        }
    }
}

/// Convenience: convert a `HashMap<String, serde_json::Value>` into a
/// proto `Struct`. This is the shape
/// [`crate::message::Message`]`.metadata` expects.
pub fn json_object_to_struct(fields: HashMap<String, serde_json::Value>) -> pbjson_types::Struct {
    let mut out: HashMap<String, pbjson_types::Value> = HashMap::with_capacity(fields.len());
    for (k, v) in fields {
        out.insert(k, json_to_value(v));
    }
    pbjson_types::Struct { fields: out }
}

/// Convenience: convert a proto `Struct` back into a flat
/// `HashMap<String, serde_json::Value>` for ergonomic read access.
pub fn struct_to_json_object(s: pbjson_types::Struct) -> HashMap<String, serde_json::Value> {
    let mut out: HashMap<String, serde_json::Value> = HashMap::with_capacity(s.fields.len());
    for (k, v) in s.fields {
        out.insert(k, value_to_json(v));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbjson_types::value::Kind;
    use serde_json::json;

    #[test]
    fn json_to_value_null() {
        match json_to_value(json!(null)).kind {
            Some(Kind::NullValue(_)) => {}
            other => panic!("expected NullValue, got {other:?}"),
        }
    }

    #[test]
    fn json_to_value_bool() {
        match json_to_value(json!(true)).kind {
            Some(Kind::BoolValue(true)) => {}
            other => panic!("expected BoolValue(true), got {other:?}"),
        }
    }

    #[test]
    fn json_to_value_number() {
        match json_to_value(json!(42)).kind {
            Some(Kind::NumberValue(n)) => assert_eq!(n, 42.0),
            other => panic!("expected NumberValue, got {other:?}"),
        }
        match json_to_value(json!(-2.5)).kind {
            Some(Kind::NumberValue(n)) => assert!((n - -2.5).abs() < 1e-12),
            other => panic!("expected NumberValue, got {other:?}"),
        }
    }

    #[test]
    fn json_to_value_string() {
        match json_to_value(json!("hello")).kind {
            Some(Kind::StringValue(s)) => assert_eq!(s, "hello"),
            other => panic!("expected StringValue, got {other:?}"),
        }
    }

    #[test]
    fn json_to_value_array() {
        match json_to_value(json!(["a", 1, true])).kind {
            Some(Kind::ListValue(list)) => {
                assert_eq!(list.values.len(), 3);
                assert!(matches!(list.values[0].kind, Some(Kind::StringValue(_))));
                assert!(matches!(list.values[1].kind, Some(Kind::NumberValue(_))));
                assert!(matches!(list.values[2].kind, Some(Kind::BoolValue(true))));
            }
            other => panic!("expected ListValue, got {other:?}"),
        }
    }

    #[test]
    fn json_to_value_nested_object() {
        let v = json_to_value(json!({"outer": {"inner": "value"}}));
        let outer_fields = match v.kind {
            Some(Kind::StructValue(s)) => s.fields,
            other => panic!("expected StructValue, got {other:?}"),
        };
        let outer = outer_fields.get("outer").expect("outer key");
        let inner_fields = match &outer.kind {
            Some(Kind::StructValue(s)) => &s.fields,
            other => panic!("expected nested StructValue, got {other:?}"),
        };
        let inner = inner_fields.get("inner").expect("inner key");
        match &inner.kind {
            Some(Kind::StringValue(s)) => assert_eq!(s, "value"),
            other => panic!("expected StringValue at leaf, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_preserves_non_numeric_shapes_exactly() {
        // Non-numeric shapes round-trip bit-identically.
        let inputs = [
            json!(null),
            json!(true),
            json!(false),
            json!("hello"),
            json!([]),
            json!({}),
            json!({"a": "str", "b": [true, null]}),
        ];
        for v in inputs {
            let round = value_to_json(json_to_value(v.clone()));
            assert_eq!(round, v, "roundtrip must preserve {v}");
        }
    }

    #[test]
    fn roundtrip_numeric_is_lossy_via_f64_but_value_preserving() {
        // Proto `Value::NumberValue` is `double`, so `serde_json`
        // integers (`Number::from(i64)`) collapse to floats
        // (`Number::from_f64(f64)`) after a round-trip. This is a
        // documented property — see the module docs. Compare
        // numerically via `as_f64()` instead of strict PartialEq.
        let inputs: &[serde_json::Value] =
            &[json!(0), json!(42), json!(-7), json!(1.5), json!(-2.5)];
        for v in inputs {
            let round = value_to_json(json_to_value(v.clone()));
            assert_eq!(
                round.as_f64().unwrap(),
                v.as_f64().unwrap(),
                "roundtrip must preserve numeric value of {v}"
            );
        }
    }

    #[test]
    fn json_object_to_struct_flat_hashmap() {
        let mut fields: HashMap<String, serde_json::Value> = HashMap::new();
        fields.insert("trigger_id".into(), json!("trig-1"));
        fields.insert("attempt".into(), json!(3));
        fields.insert("tags".into(), json!(["a", "b"]));

        let s = json_object_to_struct(fields);
        assert_eq!(s.fields.len(), 3);

        // Round-trip via struct_to_json_object gives back equivalent
        // *values*. Numbers collapse int → float through proto
        // `NumberValue` (f64); other shapes are bit-identical.
        let round = struct_to_json_object(s);
        assert_eq!(round.get("trigger_id").unwrap(), &json!("trig-1"));
        assert_eq!(
            round.get("attempt").unwrap().as_f64().unwrap(),
            3.0,
            "attempt is a number; exact integer representation not preserved"
        );
        assert_eq!(round.get("tags").unwrap(), &json!(["a", "b"]));
    }

    #[test]
    fn empty_struct_survives_roundtrip() {
        let empty = HashMap::<String, serde_json::Value>::new();
        let s = json_object_to_struct(empty);
        assert!(s.fields.is_empty());
        let round = struct_to_json_object(s);
        assert!(round.is_empty());
    }

    #[test]
    fn value_with_kind_none_decodes_as_null() {
        let v = pbjson_types::Value { kind: None };
        assert_eq!(value_to_json(v), serde_json::Value::Null);
    }

    #[test]
    fn nan_or_infinity_becomes_null_on_reverse() {
        // `serde_json::Number::from_f64` rejects NaN and infinity, so a
        // proto Value carrying a non-finite NumberValue decodes to Null.
        // This is intentional — JSON does not represent NaN/Inf.
        let v = pbjson_types::Value {
            kind: Some(Kind::NumberValue(f64::NAN)),
        };
        assert_eq!(value_to_json(v), serde_json::Value::Null);
    }
}
