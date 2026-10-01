//! The envelopes of Effect RPC with `RpcSerialization.layerJson` (plan §1.3).
//!
//! A text frame holds one JSON value, which is either one message or an array of
//! messages. Client → server: `Request`, `Ack`, `Interrupt`, `Ping`, `Eof`.
//! Server → client: `Chunk`, `Exit`, `Defect`, `Pong`. Sources:
//! `effect/dist/unstable/rpc/RpcMessage.js`, `RpcServer.js` (`make`, the main loop).

use serde::{Serialize, Serializer};
use serde_json::{Map, Number, Value};

use crate::exit::Exit;

/// A request id, echoed back with the same JSON type: the client keys its pending
/// requests by the raw value, so `1` and `"1"` are different ids.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RequestId {
    Number(Number),
    String(String),
}

impl RequestId {
    /// Ids are numbers or strings; anything else is not an id.
    pub fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Number(n) => Some(Self::Number(n.clone())),
            Value::String(s) => Some(Self::String(s.clone())),
            _ => None,
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            Self::Number(n) => Value::Number(n.clone()),
            Self::String(s) => Value::String(s.clone()),
        }
    }
}

impl From<u64> for RequestId {
    fn from(n: u64) -> Self {
        Self::Number(n.into())
    }
}

impl From<&str> for RequestId {
    fn from(s: &str) -> Self {
        Self::String(s.to_owned())
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Number(n) => write!(f, "{n}"),
            Self::String(s) => f.write_str(s),
        }
    }
}

impl Serialize for RequestId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Number(n) => n.serialize(serializer),
            Self::String(s) => serializer.serialize_str(s),
        }
    }
}

/// One decoded client message.
#[derive(Clone, Debug, PartialEq)]
pub enum ClientMessage {
    /// `traceId`, `spanId`, `sampled`, `headers` and `isNotification` are ignored: the
    /// TS server runs with `disableTracing: true`.
    Request {
        id: RequestId,
        tag: String,
        /// The encoded payload; `Value::Null` when the key is absent.
        payload: Value,
    },
    Ack {
        request_id: RequestId,
    },
    Interrupt {
        request_id: RequestId,
    },
    Ping,
    Eof,
    /// An `Ack` or `Interrupt` whose `requestId` is not an id: it matches no request, so
    /// the TS server drops it silently.
    Ignored,
}

/// Why a message could not be decoded. Each one becomes a connection-wide `Defect`
/// frame, exactly like the TS server (`RpcServer.js` main loop and `makeSocketProtocol`).
#[derive(Clone, Debug, PartialEq)]
pub enum ProtocolError {
    /// The frame is not JSON (the TS server reports the `SyntaxError`).
    Syntax(String),
    /// `Request` whose `id` is neither a number nor a string.
    InvalidRequestId(String),
    /// A message whose `_tag` is not a client message.
    UnknownMessageTag(String),
}

impl ProtocolError {
    /// The encoded defect for the `Defect` frame, shaped as `Schema.Defect()` encodes
    /// it: an `Error` becomes `{name, message}`, a string stays a string.
    pub fn defect(&self) -> Value {
        match self {
            Self::Syntax(message) => error_defect("SyntaxError", message),
            Self::InvalidRequestId(raw) => Value::String(format!("Invalid request id: {raw}")),
            Self::UnknownMessageTag(raw) => Value::String(format!("Unknown request tag: {raw}")),
        }
    }
}

/// `{"name": name, "message": message}`, the encoding of a JS `Error` defect.
pub fn error_defect(name: &str, message: &str) -> Value {
    let mut map = Map::new();
    map.insert("name".into(), Value::String(name.to_owned()));
    map.insert("message".into(), Value::String(message.to_owned()));
    Value::Object(map)
}

/// How JavaScript's `String(value)` renders a JSON value, for error texts that the TS
/// server builds with template literals (`undefined` when absent).
pub fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                other => js_string(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// Decodes one frame into messages. A frame that is not JSON fails as a whole; inside
/// an array, each message decodes on its own, so one bad message does not hide the
/// others (the TS server also handles them one by one).
pub fn decode_frame(text: &str) -> Result<Vec<Result<ClientMessage, ProtocolError>>, ProtocolError> {
    let value: Value = serde_json::from_str(text).map_err(|e| ProtocolError::Syntax(e.to_string()))?;
    Ok(match value {
        Value::Array(items) => items.iter().map(decode_message).collect(),
        other => vec![decode_message(&other)],
    })
}

/// Decodes one message value.
pub fn decode_message(value: &Value) -> Result<ClientMessage, ProtocolError> {
    let object = value.as_object();
    let tag = object.and_then(|o| o.get("_tag"));
    let tag_str = tag.and_then(Value::as_str);
    let field = |name: &str| object.and_then(|o| o.get(name));
    match tag_str {
        Some("Request") => {
            let raw_id = field("id");
            let id = raw_id
                .and_then(RequestId::from_value)
                .ok_or_else(|| ProtocolError::InvalidRequestId(js_string(raw_id)))?;
            // `Object.hasOwn(request, "tag") ? request.tag : ""`, then a group lookup:
            // a non-string tag simply names no method.
            let tag = match field("tag") {
                None => String::new(),
                Some(Value::String(s)) => s.clone(),
                Some(other) => js_string(Some(other)),
            };
            Ok(ClientMessage::Request {
                id,
                tag,
                payload: field("payload").cloned().unwrap_or(Value::Null),
            })
        }
        Some("Ack") => Ok(field("requestId")
            .and_then(RequestId::from_value)
            .map_or(ClientMessage::Ignored, |request_id| ClientMessage::Ack { request_id })),
        Some("Interrupt") => Ok(field("requestId")
            .and_then(RequestId::from_value)
            .map_or(ClientMessage::Ignored, |request_id| ClientMessage::Interrupt { request_id })),
        Some("Ping") => Ok(ClientMessage::Ping),
        Some("Eof") => Ok(ClientMessage::Eof),
        _ => Err(ProtocolError::UnknownMessageTag(js_string(tag))),
    }
}

/// One server message, ready to encode.
#[derive(Clone, Debug, PartialEq)]
pub enum ServerMessage {
    /// `values` is never empty.
    Chunk {
        request_id: RequestId,
        values: Vec<Value>,
    },
    Exit {
        request_id: RequestId,
        exit: Exit,
    },
    /// Fails every pending request of the connection on the client: only for protocol
    /// errors.
    Defect {
        defect: Value,
    },
    Pong,
}

impl ServerMessage {
    /// The JSON text of the frame, keys in the order the TS server writes them.
    pub fn encode(&self) -> String {
        let mut map = Map::new();
        match self {
            Self::Chunk { request_id, values } => {
                debug_assert!(!values.is_empty(), "a Chunk carries at least one value");
                map.insert("_tag".into(), "Chunk".into());
                map.insert("requestId".into(), request_id.to_value());
                map.insert("values".into(), Value::Array(values.clone()));
            }
            Self::Exit { request_id, exit } => {
                map.insert("_tag".into(), "Exit".into());
                map.insert("requestId".into(), request_id.to_value());
                map.insert("exit".into(), exit.to_json());
            }
            Self::Defect { defect } => {
                map.insert("_tag".into(), "Defect".into());
                map.insert("defect".into(), defect.clone());
            }
            Self::Pong => {
                map.insert("_tag".into(), "Pong".into());
            }
        }
        Value::Object(map).to_string()
    }
}

/// The encoded Chunk frame, built without cloning the values.
pub(crate) fn encode_chunk(request_id: &RequestId, values: Vec<Value>) -> String {
    let mut map = Map::new();
    map.insert("_tag".into(), "Chunk".into());
    map.insert("requestId".into(), request_id.to_value());
    map.insert("values".into(), Value::Array(values));
    Value::Object(map).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_a_request_like_the_web_client_sends_it() {
        let frame = r#"{"_tag":"Request","id":0,"tag":"subscribeServerConfig","payload":{"environmentThemes":true},"traceId":"5479","spanId":"72e2","sampled":true,"headers":[]}"#;
        let messages = decode_frame(frame).unwrap();
        assert_eq!(
            messages,
            vec![Ok(ClientMessage::Request {
                id: RequestId::from(0),
                tag: "subscribeServerConfig".into(),
                payload: json!({"environmentThemes": true}),
            })]
        );
    }

    #[test]
    fn accepts_an_array_of_messages_in_one_frame() {
        let frame = r#"[{"_tag":"Ping"},{"_tag":"Ack","requestId":"7"},{"_tag":"Interrupt","requestId":3},{"_tag":"Eof"}]"#;
        let messages: Vec<_> = decode_frame(frame).unwrap().into_iter().map(Result::unwrap).collect();
        assert_eq!(
            messages,
            vec![
                ClientMessage::Ping,
                ClientMessage::Ack {
                    request_id: RequestId::from("7")
                },
                ClientMessage::Interrupt {
                    request_id: RequestId::from(3)
                },
                ClientMessage::Eof,
            ]
        );
    }

    #[test]
    fn missing_payload_and_odd_tags() {
        let messages = decode_frame(r#"{"_tag":"Request","id":"a"}"#).unwrap();
        assert_eq!(
            messages,
            vec![Ok(ClientMessage::Request {
                id: RequestId::from("a"),
                tag: String::new(),
                payload: Value::Null
            })]
        );
        let messages = decode_frame(r#"{"_tag":"Request","id":1,"tag":5,"payload":{}}"#).unwrap();
        assert!(matches!(&messages[0], Ok(ClientMessage::Request { tag, .. }) if tag == "5"));
    }

    #[test]
    fn protocol_errors_carry_the_ts_defect_texts() {
        let err = decode_frame("{nope").unwrap_err();
        assert!(matches!(err, ProtocolError::Syntax(_)));
        assert_eq!(err.defect()["name"], "SyntaxError");

        let bad_id = decode_frame(r#"{"_tag":"Request","id":true,"tag":"x"}"#).unwrap();
        assert_eq!(bad_id[0].clone().unwrap_err().defect(), json!("Invalid request id: true"));
        let no_id = decode_frame(r#"{"_tag":"Request","tag":"x"}"#).unwrap();
        assert_eq!(no_id[0].clone().unwrap_err().defect(), json!("Invalid request id: undefined"));

        let ack = decode_frame(r#"{"_tag":"Ack","requestId":null}"#).unwrap();
        assert_eq!(ack, vec![Ok(ClientMessage::Ignored)]);

        let unknown = decode_frame(r#"{"_tag":"Chunk"}"#).unwrap();
        assert_eq!(unknown[0].clone().unwrap_err().defect(), json!("Unknown request tag: Chunk"));
        let not_object = decode_frame("42").unwrap();
        assert_eq!(not_object[0].clone().unwrap_err().defect(), json!("Unknown request tag: undefined"));
    }

    #[test]
    fn ids_keep_their_json_type() {
        let n = ServerMessage::Exit {
            request_id: RequestId::from(12),
            exit: Exit::success(Value::Null),
        };
        assert_eq!(n.encode(), r#"{"_tag":"Exit","requestId":12,"exit":{"_tag":"Success","value":null}}"#);
        let s = ServerMessage::Chunk {
            request_id: RequestId::from("12"),
            values: vec![json!(1)],
        };
        assert_eq!(s.encode(), r#"{"_tag":"Chunk","requestId":"12","values":[1]}"#);
        // Large and negative numbers round-trip as written.
        let big = decode_frame(r#"{"_tag":"Ack","requestId":9007199254740993}"#).unwrap();
        let Ok(ClientMessage::Ack { request_id }) = &big[0] else { panic!() };
        assert_eq!(serde_json::to_string(request_id).unwrap(), "9007199254740993");
    }

    #[test]
    fn server_frames() {
        assert_eq!(ServerMessage::Pong.encode(), r#"{"_tag":"Pong"}"#);
        assert_eq!(ServerMessage::Defect { defect: json!("x") }.encode(), r#"{"_tag":"Defect","defect":"x"}"#);
        assert_eq!(
            encode_chunk(&RequestId::from(1), vec![json!({"b":1,"a":2})]),
            r#"{"_tag":"Chunk","requestId":1,"values":[{"b":1,"a":2}]}"#
        );
    }
}
