//! Decode literal JSON objects without interpreting Serde's private marker keys.

use std::fmt;

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Number, Value};

struct Literal(Value);

/// Parse one JSON value while preserving literal object keys across build features.
///
/// This avoids interpreting Serde's private marker keys as encoded JSON. Numeric
/// parsing uses the same round-trip mode in default and embedded builds. Callers
/// bound input sizes; the parser retains its normal recursion limit.
///
/// # Examples
/// ```
/// let value = codemode::parse_json(r#"{"$serde_json::private::RawValue":"literal"}"#)?;
/// assert_eq!(value["$serde_json::private::RawValue"], "literal");
/// # Ok::<(), serde_json::Error>(())
/// ```
///
/// # Errors
/// Returns a JSON syntax, trailing-input, or recursion-limit error.
pub fn parse_json(text: &str) -> Result<Value, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let parsed = value(&mut deserializer)?;
    deserializer.end()?;
    Ok(parsed)
}

impl<'de> Deserialize<'de> for Literal {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct JsonVisitor;

        impl<'de> Visitor<'de> for JsonVisitor {
            type Value = Literal;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON value")
            }

            fn visit_unit<E: de::Error>(self) -> Result<Literal, E> {
                Ok(Literal(Value::Null))
            }

            fn visit_none<E: de::Error>(self) -> Result<Literal, E> {
                self.visit_unit()
            }

            fn visit_some<D: Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> Result<Literal, D::Error> {
                Literal::deserialize(deserializer)
            }

            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Literal, E> {
                Ok(Literal(Value::Bool(value)))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Literal, E> {
                Ok(Literal(Value::from(value)))
            }

            fn visit_i128<E: de::Error>(self, value: i128) -> Result<Literal, E> {
                Number::deserialize(de::value::I128Deserializer::<E>::new(value))
                    .map(|number| Literal(Value::Number(number)))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Literal, E> {
                Ok(Literal(Value::from(value)))
            }

            fn visit_u128<E: de::Error>(self, value: u128) -> Result<Literal, E> {
                Number::deserialize(de::value::U128Deserializer::<E>::new(value))
                    .map(|number| Literal(Value::Number(number)))
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Literal, E> {
                Ok(Literal(
                    Number::from_f64(value).map_or(Value::Null, Value::Number),
                ))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Literal, E> {
                Ok(Literal(Value::String(value.to_owned())))
            }

            fn visit_string<E: de::Error>(self, value: String) -> Result<Literal, E> {
                Ok(Literal(Value::String(value)))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Literal, A::Error> {
                let mut values = Vec::new();
                while let Some(Literal(value)) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(Literal(Value::Array(values)))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut object: A) -> Result<Literal, A::Error> {
                let mut values = Map::new();
                while let Some((key, Literal(value))) = object.next_entry::<String, Literal>()? {
                    values.insert(key, value);
                }
                Ok(Literal(Value::Object(values)))
            }
        }

        deserializer.deserialize_any(JsonVisitor)
    }
}

/// Deserialize a JSON value while treating every object key literally.
///
/// Use as `#[serde(deserialize_with = "codemode::deserialize_json")]` on a
/// `serde_json::Value` field when dependency features must not reinterpret data.
///
/// # Errors
/// Returns the supplied deserializer's syntax, type, or number-range error.
pub fn value<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Value, D::Error> {
    Literal::deserialize(deserializer).map(|literal| literal.0)
}

pub(crate) fn values<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Value>, D::Error> {
    Vec::<Literal>::deserialize(deserializer)
        .map(|values| values.into_iter().map(|literal| literal.0).collect())
}

pub(crate) fn optional<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Option::<Literal>::deserialize(deserializer).map(|value| value.map(|literal| literal.0))
}
