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
    /// Open a session: a new one, or an earlier one by id. `session/started`
    /// follows.
    SessionStart => "session/start" {
        params: crate::session::SessionStartParams,
        response: crate::session::SessionStartResponse,
    },
    /// Stored sessions, newest first.
    SessionList => "session/list" {
        params: crate::session::SessionListParams,
        response: crate::session::SessionListResponse,
    },
    /// Delete a stored session, closing it first if it is open.
    /// `session/deleted` follows.
    SessionDelete => "session/delete" {
        params: crate::session::SessionDeleteParams,
        response: crate::session::SessionDeleteResponse,
    },
    /// Change what a session's next turns run with. A running turn keeps
    /// its own; `session/settings/updated` follows once applied.
    SessionSettingsUpdate => "session/settings/update" {
        params: crate::session::SessionSettingsUpdateParams,
        response: crate::session::SessionSettingsUpdateResponse,
    },
    /// Koda's saved settings.
    ConfigRead => "config/read" {
        params: crate::config::ConfigReadParams,
        response: crate::config::ConfigReadResponse,
    },
    /// Change saved settings, all or none.
    ConfigBatchWrite => "config/batchWrite" {
        params: crate::config::ConfigBatchWriteParams,
        response: crate::config::ConfigWriteResponse,
    },
    /// MCP servers and where each stands: a session's, or the stored ones.
    McpServerStatusList => "mcpServerStatus/list" {
        params: crate::mcp::ListMcpServerStatusParams,
        response: crate::mcp::ListMcpServerStatusResponse,
    },
    /// Every open session takes the stored MCP servers again before its
    /// next turn. Answered once the idle ones have.
    McpServerRefresh => "config/mcpServer/reload" {
        params: crate::mcp::McpServerRefreshParams,
        response: crate::mcp::McpServerRefreshResponse,
    },
    /// Store a provider's API key.
    ModelProviderApiKeySet => "modelProvider/apiKey/set" {
        params: crate::config::ModelProviderApiKeySetParams,
        response: crate::config::ModelProviderApiKeyResponse,
    },
    /// Forget a provider's stored API key.
    ModelProviderApiKeyRemove => "modelProvider/apiKey/remove" {
        params: crate::config::ModelProviderApiKeyRemoveParams,
        response: crate::config::ModelProviderApiKeyResponse,
    },
    /// The models a provider offers.
    ModelList => "model/list" {
        params: crate::model::ModelListParams,
        response: crate::model::ModelListResponse,
    },
    /// The providers Koda offers, with their key status.
    ModelProviderList => "modelProvider/list" {
        params: crate::model::ModelProviderListParams,
        response: crate::model::ModelProviderListResponse,
    },
    /// Compact a session's history now. Answered at once; the compaction
    /// runs as a turn of its own, replacing a running turn.
    SessionCompactStart => "session/compact/start" {
        params: crate::session::SessionCompactStartParams,
        response: crate::session::SessionCompactStartResponse,
    },
    /// Close a session the client is done with, stopping its running turn.
    SessionUnsubscribe => "session/unsubscribe" {
        params: crate::session::SessionUnsubscribeParams,
        response: crate::session::SessionUnsubscribeResponse,
    },
    /// Start a turn. Answered at once with the turn in progress, before its
    /// first notification; everything after arrives as notifications, ending
    /// with `turn/completed`. Refused while a turn is running.
    TurnStart => "turn/start" {
        params: crate::turn::TurnStartParams,
        response: crate::turn::TurnStartResponse,
    },
    /// Stop the running turn. Answered once it has stopped, after its
    /// `turn/completed`.
    TurnInterrupt => "turn/interrupt" {
        params: crate::turn::TurnInterruptParams,
        response: crate::turn::TurnInterruptResponse,
    },
    /// Add text to the running turn. It reaches the model before its next
    /// call, as a `userMessage` item; text the turn ends without delivering
    /// gets no item.
    TurnSteer => "turn/steer" {
        params: crate::turn::TurnSteerParams,
        response: crate::turn::TurnSteerResponse,
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
