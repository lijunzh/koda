//! JSON-RPC 2.0 framing.
//!
//! Every message carries `"jsonrpc": "2.0"`. Codex's app-server drops that
//! member ("we do not do true JSON-RPC 2.0"); Koda keeps it, so a standard
//! JSON-RPC library in another language reads Koda's messages unmodified.

use std::borrow::Cow;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

/// The only JSON-RPC version Koda speaks.
pub const JSONRPC_VERSION: &str = "2.0";

/// The `"jsonrpc"` member: always `"2.0"`, and a message with anything else
/// is rejected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Version;

impl Serialize for Version {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(JSONRPC_VERSION)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let version = String::deserialize(deserializer)?;
        if version == JSONRPC_VERSION {
            Ok(Version)
        } else {
            Err(de::Error::custom(format!(
                "unsupported JSON-RPC version {version:?}, expected \"2.0\""
            )))
        }
    }
}

impl JsonSchema for Version {
    fn schema_name() -> Cow<'static, str> {
        "JsonRpcVersion".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string", "const": JSONRPC_VERSION })
    }
}

/// A request id, chosen by whoever sends the request.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(untagged)]
pub enum RequestId {
    /// A numeric id. Koda's own clients use these.
    Integer(i64),
    /// A string id.
    String(String),
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer(id) => write!(f, "{id}"),
            Self::String(id) => f.write_str(id),
        }
    }
}

/// Any message on the wire, in either direction.
///
/// The variants are told apart by their members, as the specification
/// does: a request has `id` and `method`, a notification `method` only, a
/// response `result`, an error `error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Message {
    /// A call that expects a response.
    Request(Request),
    /// A one-way message.
    Notification(Notification),
    /// A successful response.
    Response(Response),
    /// A failed response.
    Error(ErrorResponse),
}

/// A call that expects a [`Response`] or an [`ErrorResponse`] with the same id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Always `"2.0"`.
    pub jsonrpc: Version,
    /// Echoed in the response.
    pub id: RequestId,
    /// The method name, such as `state/read`.
    pub method: String,
    /// The method's parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A one-way message: nothing answers it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    /// Always `"2.0"`.
    pub jsonrpc: Version,
    /// The notification's name.
    pub method: String,
    /// Its parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// The result of a request that succeeded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Response {
    /// Always `"2.0"`.
    pub jsonrpc: Version,
    /// The request's id.
    pub id: RequestId,
    /// The method's response.
    pub result: Value,
}

/// The answer to a request that failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ErrorResponse {
    /// Always `"2.0"`.
    pub jsonrpc: Version,
    /// The request's id; `null` when it couldn't be read.
    pub id: Option<RequestId>,
    /// What went wrong.
    pub error: ErrorObject,
}

/// What went wrong with a request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ErrorObject {
    /// One of the codes in [`codes`], or a Koda code.
    pub code: i64,
    /// One sentence for a person.
    pub message: String,
    /// Detail for a program, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorObject {
    /// An error with `code` and `message` and no data.
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

impl fmt::Display for ErrorObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

impl std::error::Error for ErrorObject {}

/// The error codes JSON-RPC 2.0 defines, which Koda uses as Codex does.
pub mod codes {
    /// The message isn't valid JSON.
    pub const PARSE_ERROR: i64 = -32700;
    /// The message isn't a valid request, or comes at the wrong time: a
    /// request before `initialize`, or a second `initialize`.
    pub const INVALID_REQUEST: i64 = -32600;
    /// No such method.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// The method exists but its parameters don't fit.
    pub const INVALID_PARAMS: i64 = -32602;
    /// The server failed.
    pub const INTERNAL_ERROR: i64 = -32603;
}

impl Request {
    /// A request for `method` with `params`.
    pub fn new(id: RequestId, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: Version,
            id,
            method: method.into(),
            params,
        }
    }
}

impl Response {
    /// The successful answer to request `id`.
    pub fn new(id: RequestId, result: Value) -> Self {
        Self {
            jsonrpc: Version,
            id,
            result,
        }
    }
}

impl ErrorResponse {
    /// The failed answer to request `id`.
    pub fn new(id: Option<RequestId>, error: ErrorObject) -> Self {
        Self {
            jsonrpc: Version,
            id,
            error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: Value) -> Message {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn every_message_carries_the_version() {
        let request = Request::new(RequestId::Integer(1), "state/read", None);
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({"jsonrpc": "2.0", "id": 1, "method": "state/read"})
        );
        let error = ErrorResponse::new(None, ErrorObject::new(codes::PARSE_ERROR, "bad"));
        assert_eq!(
            serde_json::to_value(&error).unwrap(),
            json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "bad"}})
        );
    }

    #[test]
    fn a_message_without_the_version_or_with_another_is_refused() {
        let missing = json!({"id": 1, "method": "state/read"});
        assert!(serde_json::from_value::<Message>(missing).is_err());
        let wrong = json!({"jsonrpc": "1.0", "id": 1, "method": "state/read"});
        assert!(serde_json::from_value::<Message>(wrong).is_err());
    }

    #[test]
    fn the_four_kinds_are_told_apart_by_their_members() {
        let request = parse(json!({"jsonrpc": "2.0", "id": "a", "method": "m", "params": {}}));
        assert!(matches!(request, Message::Request(r) if r.id == RequestId::String("a".into())));
        let notification = parse(json!({"jsonrpc": "2.0", "method": "m"}));
        assert!(matches!(notification, Message::Notification(_)));
        let response = parse(json!({"jsonrpc": "2.0", "id": 2, "result": null}));
        assert!(matches!(response, Message::Response(r) if r.result.is_null()));
        let error =
            parse(json!({"jsonrpc": "2.0", "id": 3, "error": {"code": -1, "message": "x"}}));
        assert!(matches!(error, Message::Error(e) if e.error.code == -1));
    }

    #[test]
    fn a_response_needs_its_result() {
        let no_result = json!({"jsonrpc": "2.0", "id": 2});
        assert!(serde_json::from_value::<Message>(no_result).is_err());
    }
}
