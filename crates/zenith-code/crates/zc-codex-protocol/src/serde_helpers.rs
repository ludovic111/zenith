//! `deserialize_with` / `with` helpers the generated types use.

use serde::{Deserialize, Deserializer};

/// A required key whose value may be `null`: the key must be present (a `deserialize_with`
/// without `default` makes serde report a missing field), `null` reads as `None`.
///
/// # Errors
/// When the value does not decode as `T`.
pub fn nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// An optional key of any JSON value: a present `null` is kept (`Some(Value::Null)`), only an
/// absent key is `None` (with `#[serde(default)]`).
///
/// # Errors
/// Only when the input is not JSON.
pub fn value_present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(deserializer).map(Some)
}

/// An optional key whose value may be `null`: absent → `None`, `null` → `Some(None)`, value →
/// `Some(Some(v))`. Use with `#[serde(default, skip_serializing_if = "Option::is_none")]`.
pub mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    /// Writes `null` for `Some(None)`.
    ///
    /// # Errors
    /// When the inner value fails to serialize.
    #[allow(clippy::ref_option)]
    pub fn serialize<S, T>(value: &Option<Option<T>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Serialize,
    {
        match value {
            Some(inner) => inner.serialize(serializer),
            None => serializer.serialize_none(),
        }
    }

    /// Reads a present key: `null` → `Some(None)`.
    ///
    /// # Errors
    /// When the value does not decode as `T`.
    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::<T>::deserialize(deserializer).map(Some)
    }
}
