//! `Exit` and `Cause`, encoded exactly as `Schema.toCodecJson(Schema.Exit(…))` does in
//! effect rc.115 (verified by encoding with the real library):
//!
//! ```text
//! {"_tag":"Success","value":<value | null for streams>}
//! {"_tag":"Failure","cause":[{"_tag":"Fail","error":<encoded error>}]}
//! {"_tag":"Failure","cause":[{"_tag":"Die","defect":<encoded defect>}]}
//! {"_tag":"Failure","cause":[{"_tag":"Interrupt","fiberId":<number | null>}]}
//! ```
//!
//! `fiberId` is required: decoding `{"_tag":"Interrupt"}` without it fails with
//! "Missing key", so an interrupt without a fiber is written with `null`.

use serde_json::{Map, Value};

/// How a request ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Exit {
    Success(Value),
    Failure(Vec<CauseReason>),
}

/// One reason of a `Cause` (Effect 4 flattens a cause into an ordered list).
#[derive(Clone, Debug, PartialEq)]
pub enum CauseReason {
    /// A typed failure, already encoded (`{"_tag":"SomeError",…}`).
    Fail(Value),
    /// A defect, already encoded (`Schema.Defect()`: `{name, message}` or any JSON).
    Die(Value),
    /// An interruption, with the interrupting fiber's id if there is one.
    Interrupt(Option<u64>),
}

impl Exit {
    pub fn success(value: Value) -> Self {
        Self::Success(value)
    }

    /// The end of a stream: `Success` with `null` (`Schema.Void`).
    pub fn stream_end() -> Self {
        Self::Success(Value::Null)
    }

    pub fn fail(error: Value) -> Self {
        Self::Failure(vec![CauseReason::Fail(error)])
    }

    pub fn die(defect: Value) -> Self {
        Self::Failure(vec![CauseReason::Die(defect)])
    }

    pub fn interrupt() -> Self {
        Self::Failure(vec![CauseReason::Interrupt(None)])
    }

    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success(_))
    }

    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        match self {
            Self::Success(value) => {
                map.insert("_tag".into(), "Success".into());
                map.insert("value".into(), value.clone());
            }
            Self::Failure(reasons) => {
                map.insert("_tag".into(), "Failure".into());
                map.insert("cause".into(), Value::Array(reasons.iter().map(CauseReason::to_json).collect()));
            }
        }
        Value::Object(map)
    }
}

impl CauseReason {
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        match self {
            Self::Fail(error) => {
                map.insert("_tag".into(), "Fail".into());
                map.insert("error".into(), error.clone());
            }
            Self::Die(defect) => {
                map.insert("_tag".into(), "Die".into());
                map.insert("defect".into(), defect.clone());
            }
            Self::Interrupt(fiber_id) => {
                map.insert("_tag".into(), "Interrupt".into());
                map.insert("fiberId".into(), fiber_id.map_or(Value::Null, Value::from));
            }
        }
        Value::Object(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::error_defect;
    use serde_json::json;

    // The expected strings are what `Schema.encodeSync(Schema.toCodecJson(Schema.Exit(
    // Schema.Void, MyErr, Schema.Defect())))` printed with effect rc.115.
    #[test]
    fn matches_effect_encoding() {
        assert_eq!(Exit::stream_end().to_json().to_string(), r#"{"_tag":"Success","value":null}"#);
        assert_eq!(
            Exit::interrupt().to_json().to_string(),
            r#"{"_tag":"Failure","cause":[{"_tag":"Interrupt","fiberId":null}]}"#
        );
        assert_eq!(
            Exit::Failure(vec![CauseReason::Interrupt(Some(5))]).to_json().to_string(),
            r#"{"_tag":"Failure","cause":[{"_tag":"Interrupt","fiberId":5}]}"#
        );
        assert_eq!(
            Exit::fail(json!({"_tag":"MyErr","message":"x"})).to_json().to_string(),
            r#"{"_tag":"Failure","cause":[{"_tag":"Fail","error":{"_tag":"MyErr","message":"x"}}]}"#
        );
        assert_eq!(
            Exit::die(error_defect("Error", "boom")).to_json().to_string(),
            r#"{"_tag":"Failure","cause":[{"_tag":"Die","defect":{"name":"Error","message":"boom"}}]}"#
        );
        assert_eq!(
            Exit::die(json!("str")).to_json().to_string(),
            r#"{"_tag":"Failure","cause":[{"_tag":"Die","defect":"str"}]}"#
        );
    }
}
