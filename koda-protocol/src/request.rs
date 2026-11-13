//! Requests from a client to Koda.
//!
//! Each method is declared once, in [`client_requests!`], with its wire
//! name, parameters and response. The macro builds three things from that
//! list: [`ClientRequest`], the enum a server decodes; a marker type per
//! method in [`methods`], which a client names to send it and get the
//! response type back; and the method table in the schema. Codex's
//! `client_request_definitions!` is the model; the marker types are Koda's,
//! so a client can't decode a response as the wrong type.

use schemars::{JsonSchema, SchemaGenerator};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::rpc::{self, RequestId};

/// A method a client can call: its wire name, parameters and response.
pub trait Method {
    /// The wire name, such as `state/read`.
    const NAME: &'static str;
    /// What the client sends.
    type Params: Serialize + DeserializeOwned + JsonSchema;
    /// What the server answers.
    type Response: Serialize + DeserializeOwned + JsonSchema;
}

macro_rules! client_requests {
    ($(
        $(#[doc = $doc:literal])*
        $variant:ident => $wire:literal {
            params: $params:ty,
            response: $response:ty $(,)?
        }
    ),* $(,)?) => {
        /// A request from a client, decoded by method.
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "method")]
        pub enum ClientRequest {
            $(
                $(#[doc = $doc])*
                #[serde(rename = $wire)]
                $variant {
                    /// The request's id.
                    id: RequestId,
                    /// The method's parameters.
                    params: $params,
                },
            )*
        }

        impl ClientRequest {
            /// Every method's wire name.
            pub const METHODS: &'static [&'static str] = &[$($wire),*];

            /// The request's id.
            pub fn id(&self) -> &RequestId {
                match self {
                    $(Self::$variant { id, .. } => id,)*
                }
            }

            /// The method's wire name.
            pub const fn method(&self) -> &'static str {
                match self {
                    $(Self::$variant { .. } => $wire,)*
                }
            }
        }

        /// One marker type per method, for [`Method`].
        pub mod methods {
            $(
                $(#[doc = $doc])*
                #[derive(Debug, Clone, Copy)]
                pub struct $variant;

                impl super::Method for $variant {
                    const NAME: &'static str = $wire;
                    type Params = $params;
                    type Response = $response;
                }
            )*
        }

        /// The schema's method table: each method's parameters and result.
        pub(crate) fn method_schemas(generator: &mut SchemaGenerator) -> Map<String, Value> {
            let mut table = Map::new();
            $(
                table.insert(
                    $wire.to_string(),
                    serde_json::json!({
                        "params": generator.subschema_for::<$params>(),
                        "result": generator.subschema_for::<$response>(),
                    }),
                );
            )*
            table
        }
    };
}

client_requests! {
    /// The first request on a connection. Anything sent before it is
    /// refused, and so is a second one.
    Initialize => "initialize" {
        params: crate::initialize::InitializeParams,
        response: crate::initialize::InitializeResponse,
    },
    /// What a new session starts with: model, provider, context window,
    /// mode and sandbox.
    StateRead => "state/read" {
        params: crate::state::StateReadParams,
        response: crate::state::StateReadResponse,
    },
}

/// Why a [`rpc::Request`] isn't a [`ClientRequest`].
#[derive(Debug)]
pub enum DecodeError {
    /// No method has this name.
    UnknownMethod(String),
    /// The method exists; its parameters don't fit.
    InvalidParams {
        /// The method.
        method: String,
        /// What serde said.
        source: serde_json::Error,
    },
}

impl DecodeError {
    /// The JSON-RPC error to answer with.
    pub fn to_error_object(&self) -> rpc::ErrorObject {
        match self {
            Self::UnknownMethod(method) => rpc::ErrorObject::new(
                rpc::codes::METHOD_NOT_FOUND,
                format!("no method {method:?}"),
            ),
            Self::InvalidParams { method, source } => rpc::ErrorObject::new(
                rpc::codes::INVALID_PARAMS,
                format!("invalid params for {method}: {source}"),
            ),
        }
    }
}

impl TryFrom<rpc::Request> for ClientRequest {
    type Error = DecodeError;

    fn try_from(request: rpc::Request) -> Result<Self, Self::Error> {
        let rpc::Request {
            id, method, params, ..
        } = request;
        if !Self::METHODS.contains(&method.as_str()) {
            return Err(DecodeError::UnknownMethod(method));
        }
        // A method without parameters may be called with none at all.
        let params = params.unwrap_or_else(|| Value::Object(Map::new()));
        let value = serde_json::json!({ "id": id, "method": method, "params": params });
        serde_json::from_value(value)
            .map_err(|source| DecodeError::InvalidParams { method, source })
    }
}

/// The request for method `M` with `params`, as it goes on the wire.
pub fn encode<M: Method>(id: RequestId, params: &M::Params) -> serde_json::Result<rpc::Request> {
    Ok(rpc::Request::new(
        id,
        M::NAME,
        Some(serde_json::to_value(params)?),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::initialize;
    use serde_json::json;

    fn request(method: &str, params: Option<Value>) -> rpc::Request {
        rpc::Request::new(RequestId::Integer(7), method, params)
    }

    #[test]
    fn a_request_decodes_by_its_method() {
        let decoded = ClientRequest::try_from(request("state/read", Some(json!({})))).unwrap();
        assert_eq!(decoded.method(), "state/read");
        assert_eq!(decoded.id(), &RequestId::Integer(7));
        assert!(matches!(decoded, ClientRequest::StateRead { .. }));
    }

    #[test]
    fn a_method_without_parameters_may_omit_them() {
        let decoded = ClientRequest::try_from(request("state/read", None)).unwrap();
        assert!(matches!(decoded, ClientRequest::StateRead { .. }));
    }

    #[test]
    fn an_unknown_method_and_bad_params_get_their_own_codes() {
        let unknown = ClientRequest::try_from(request("thread/start", None)).unwrap_err();
        assert_eq!(unknown.to_error_object().code, rpc::codes::METHOD_NOT_FOUND);
        let bad = ClientRequest::try_from(request("initialize", Some(json!({"clientInfo": 3}))))
            .unwrap_err();
        assert_eq!(bad.to_error_object().code, rpc::codes::INVALID_PARAMS);
    }

    #[test]
    fn encode_and_decode_agree() {
        let params = initialize::InitializeParams {
            client_info: initialize::ClientInfo {
                name: "test".into(),
                version: "1".into(),
            },
        };
        let wire = encode::<methods::Initialize>(RequestId::Integer(1), &params).unwrap();
        assert_eq!(wire.method, "initialize");
        assert_eq!(
            wire.params,
            Some(json!({"clientInfo": {"name": "test", "version": "1"}}))
        );
        let ClientRequest::Initialize { params: back, .. } = ClientRequest::try_from(wire).unwrap()
        else {
            panic!("decoded as another method");
        };
        assert_eq!(back, params);
    }
}
