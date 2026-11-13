//! Messages from Koda to a client: notifications, and requests the client
//! answers (approvals, questions, the loop cap).
//!
//! Each is declared once, in [`server_notifications!`] or
//! [`server_requests!`], as Codex declares its own.

use schemars::{JsonSchema, SchemaGenerator};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::rpc::{self, RequestId};
use crate::turn::{TurnError, Usage};

macro_rules! server_notifications {
    ($(
        $(#[doc = $doc:literal])*
        $variant:ident => $wire:literal ($params:ty)
    ),* $(,)?) => {
        /// A notification from Koda, decoded by method.
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "method", content = "params")]
        pub enum ServerNotification {
            $(
                $(#[doc = $doc])*
                #[serde(rename = $wire)]
                $variant($params),
            )*
        }

        impl ServerNotification {
            /// Every notification's wire name.
            pub const METHODS: &'static [&'static str] = &[$($wire),*];

            /// The method's wire name.
            pub const fn method(&self) -> &'static str {
                match self {
                    $(Self::$variant(_) => $wire,)*
                }
            }
        }

        /// The schema's notification table: each one's parameters.
        pub(crate) fn notification_schemas(generator: &mut SchemaGenerator) -> Map<String, Value> {
            let mut table = Map::new();
            $(
                table.insert(
                    $wire.to_string(),
                    serde_json::json!({ "params": generator.subschema_for::<$params>() }),
                );
            )*
            table
        }
    };
}

macro_rules! server_requests {
    ($(
        $(#[doc = $doc:literal])*
        $variant:ident => $wire:literal {
            params: $params:ty,
            response: $response:ty $(,)?
        }
    ),* $(,)?) => {
        /// A request from Koda, decoded by method.
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "method")]
        pub enum ServerRequest {
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

        impl ServerRequest {
            /// Every request's wire name.
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

        /// The schema's server-request table: each one's parameters and
        /// result.
        pub(crate) fn server_request_schemas(generator: &mut SchemaGenerator) -> Map<String, Value> {
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

server_notifications! {
    /// A session is ready.
    SessionStarted => "session/started" (crate::session::SessionStartedNotification),
    /// A turn began.
    TurnStarted => "turn/started" (crate::turn::TurnStartedNotification),
    /// A turn is over, however it ended.
    TurnCompleted => "turn/completed" (crate::turn::TurnCompletedNotification),
    /// The model rewrote its plan.
    TurnPlanUpdated => "turn/plan/updated" (crate::turn::TurnPlanUpdatedNotification),
    /// The model kept repeating itself.
    TurnLoopGuard => "turn/loopGuard" (crate::turn::TurnLoopGuardNotification),
    /// An item began.
    ItemStarted => "item/started" (crate::item::ItemStartedNotification),
    /// An item finished.
    ItemCompleted => "item/completed" (crate::item::ItemCompletedNotification),
    /// More of the model's reply.
    AgentMessageDelta => "item/agentMessage/delta" (crate::item::TextDeltaNotification),
    /// More of the model's reasoning.
    ReasoningTextDelta => "item/reasoning/textDelta" (crate::item::TextDeltaNotification),
    /// A line of a running command's output.
    ToolCallOutputDelta => "item/toolCall/outputDelta" (crate::item::ToolCallOutputDeltaNotification),
    /// A request was answered, by this client or another, or by Koda.
    ServerRequestResolved => "serverRequest/resolved" (ServerRequestResolvedNotification),
    /// Something failed. With `willRetry`, Koda tries again and the turn
    /// goes on.
    Error => "error" (ErrorNotification),
    /// Something a person should know.
    Warning => "warning" (WarningNotification),
    /// `koda exec`'s last line: the run as a whole.
    ExecSummary => "exec/summary" (ExecSummary),
}

server_requests! {
    /// A tool call needs a yes before it runs.
    ToolCallApproval => "item/toolCall/requestApproval" {
        params: crate::item::ToolCallApprovalParams,
        response: crate::item::ToolCallApprovalResponse,
    },
    /// The model asked the user a question.
    UserInput => "item/tool/requestUserInput" {
        params: crate::item::UserInputParams,
        response: crate::item::UserInputResponse,
    },
    /// The turn reached its iteration cap.
    LoopCap => "turn/loopCap/requestDecision" {
        params: crate::turn::LoopCapParams,
        response: crate::turn::LoopCapResponse,
    },
}

impl ServerNotification {
    /// The notification as it goes on the wire.
    pub fn to_rpc(&self) -> serde_json::Result<rpc::Notification> {
        let mut value = serde_json::to_value(self)?;
        let params = value.get_mut("params").map(Value::take);
        Ok(rpc::Notification {
            jsonrpc: rpc::Version,
            method: self.method().to_string(),
            params,
        })
    }
}

impl TryFrom<rpc::Notification> for ServerNotification {
    type Error = serde_json::Error;

    fn try_from(notification: rpc::Notification) -> Result<Self, serde_json::Error> {
        let rpc::Notification { method, params, .. } = notification;
        serde_json::from_value(serde_json::json!({ "method": method, "params": params }))
    }
}

impl ServerRequest {
    /// The request as it goes on the wire.
    pub fn to_rpc(&self) -> serde_json::Result<rpc::Request> {
        let mut value = serde_json::to_value(self)?;
        let params = value.get_mut("params").map(Value::take);
        Ok(rpc::Request::new(self.id().clone(), self.method(), params))
    }
}

impl TryFrom<rpc::Request> for ServerRequest {
    type Error = serde_json::Error;

    fn try_from(request: rpc::Request) -> Result<Self, Self::Error> {
        let rpc::Request {
            id, method, params, ..
        } = request;
        serde_json::from_value(serde_json::json!({ "id": id, "method": method, "params": params }))
    }
}

/// `serverRequest/resolved`: a request needs no answer any more. Other
/// clients dismiss their prompt. In `koda exec`, Koda answers its own
/// requests and sends this right after each.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServerRequestResolvedNotification {
    /// The session.
    pub session_id: String,
    /// The request.
    pub request_id: RequestId,
}

/// `error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ErrorNotification {
    /// The session.
    pub session_id: String,
    /// The turn, when it happened in one.
    pub turn_id: Option<String>,
    /// What went wrong.
    pub error: TurnError,
    /// Whether Koda will try again. The turn goes on if so.
    pub will_retry: bool,
    /// The retry, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<Retry>,
}

/// A model call that will be tried again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Retry {
    /// Why.
    pub reason: RetryReason,
    /// The attempt that failed, counting from 1.
    pub attempt: u32,
    /// The attempts the call gets in all.
    pub max_attempts: u32,
    /// The wait before the next one.
    pub delay_ms: u64,
}

/// Why a model call is tried again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum RetryReason {
    /// The provider said to slow down.
    RateLimit,
    /// A connection failed or timed out.
    Network,
    /// The provider answered with a server error.
    ServerError,
    /// The reply's stream failed partway; what it had shown is discarded.
    StreamFailed,
    /// The context was too long; it was compacted first.
    ContextOverflow,
    /// The model returned nothing.
    EmptyResponse,
}

/// `warning`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WarningNotification {
    /// The session, when it concerns one.
    pub session_id: Option<String>,
    /// One sentence for a person.
    pub message: String,
}

/// `exec/summary`: the run as a whole. Only `koda exec` sends it, last.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecSummary {
    /// The session.
    pub session_id: String,
    /// Whether the run succeeded. An interrupted turn is not a failure.
    pub success: bool,
    /// Why it failed.
    pub error: Option<String>,
    /// The provider.
    pub provider: String,
    /// The model.
    pub model: String,
    /// The last reply.
    pub response: String,
    /// How the turn ended; `None` if it never did.
    pub turn_status: Option<crate::turn::TurnStatus>,
    /// Wall-clock time of the run.
    pub duration_ms: u64,
    /// Everything stored for the session, sub-agents and the earlier turns
    /// of a resumed session included.
    pub usage: Usage,
    /// `usage` by agent. The session's own calls are under `main`.
    pub usage_by_agent: std::collections::BTreeMap<String, Usage>,
    /// The agent's own tool calls, by tool.
    pub tool_calls: std::collections::BTreeMap<String, u64>,
    /// Sub-agents' tool calls, by tool.
    pub sub_agent_tool_calls: std::collections::BTreeMap<String, u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{Item, ItemStartedNotification, ToolCallStatus};
    use serde_json::json;

    #[test]
    fn a_notification_goes_on_the_wire_as_method_and_params() {
        let n = ServerNotification::ItemStarted(ItemStartedNotification {
            session_id: "s".into(),
            turn_id: "t".into(),
            parent_item_id: None,
            item: Item::ToolCall {
                id: "c".into(),
                tool: "Read".into(),
                arguments: json!({"filePath": "a"}),
                status: ToolCallStatus::InProgress,
                output: None,
                detail: None,
            },
        });
        let wire = serde_json::to_value(n.to_rpc().unwrap()).unwrap();
        assert_eq!(
            wire,
            json!({
                "jsonrpc": "2.0",
                "method": "item/started",
                "params": {
                    "sessionId": "s",
                    "turnId": "t",
                    "item": {
                        "type": "toolCall", "id": "c", "tool": "Read",
                        "arguments": {"filePath": "a"}, "status": "inProgress",
                        "output": null, "detail": null,
                    },
                },
            })
        );
        let back: rpc::Notification = serde_json::from_value(wire).unwrap();
        assert_eq!(ServerNotification::try_from(back).unwrap(), n);
    }

    #[test]
    fn a_server_request_carries_its_id() {
        let r = ServerRequest::UserInput {
            id: RequestId::Integer(3),
            params: crate::item::UserInputParams {
                session_id: "s".into(),
                turn_id: "t".into(),
                item_id: "c".into(),
                question: "Which?".into(),
                options: vec![],
            },
        };
        let wire = r.to_rpc().unwrap();
        assert_eq!(wire.id, RequestId::Integer(3));
        assert_eq!(wire.method, "item/tool/requestUserInput");
        assert_eq!(ServerRequest::try_from(wire).unwrap(), r);
    }

    #[test]
    fn every_name_is_listed_once() {
        let mut names: Vec<&str> = ServerNotification::METHODS
            .iter()
            .chain(ServerRequest::METHODS)
            .chain(crate::ClientRequest::METHODS)
            .copied()
            .collect();
        let all = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all);
    }
}
