//! MCP elicitation: a server asking the user for input in the middle of a
//! tool call (`elicitation/create`), as Codex and Claude Code answer it.
//!
//! Koda tells servers it can elicit, in form mode (fields from a flat JSON
//! Schema) and URL mode (a page to visit). A request goes to the session's
//! [`Elicitor`], which the host fills: it asks the client
//! (`mcpServer/elicitation/request`), or declines when nobody can answer, as
//! in `koda exec`. Without an elicitor, Koda declines.

use std::sync::{Arc, PoisonError, RwLock};

use rmcp::model::{
    ClientCapabilities, ClientConfig, ElicitRequestParams, ElicitResult, ElicitationAction,
    ElicitationCapability, FormElicitationCapability, UrlElicitationCapability,
};
use rmcp::service::{RequestContext, RoleClient};
use rmcp::{ClientHandler, ErrorData as McpError};

/// What a server asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct ElicitationRequest {
    /// The server's name.
    pub server: String,
    /// What it says it needs.
    pub message: String,
    /// Form mode: the fields, as a JSON Schema object.
    pub requested_schema: Option<serde_json::Value>,
    /// URL mode: the page to visit.
    pub url: Option<String>,
}

/// What the user did.
#[derive(Debug, Clone, PartialEq)]
pub enum ElicitationAnswer {
    /// Gave the fields' values (form) or visited the page (URL).
    Accept(Option<serde_json::Value>),
    /// Said no; the tool goes on without it.
    Decline,
    /// Dismissed it; the operation stops.
    Cancel,
}

/// Who answers a server's request for input.
#[async_trait::async_trait]
pub trait Elicitor: Send + Sync {
    async fn elicit(&self, request: ElicitationRequest) -> ElicitationAnswer;
}

/// The session's elicitor, shared with every client so one the host sets
/// after a server started still answers it.
#[derive(Clone, Default)]
pub struct ElicitorSlot(Arc<RwLock<Option<Arc<dyn Elicitor>>>>);

impl ElicitorSlot {
    pub fn set(&self, elicitor: Arc<dyn Elicitor>) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = Some(elicitor);
    }

    fn get(&self) -> Option<Arc<dyn Elicitor>> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Koda's side of the MCP connection: its client info, and the answer to a
/// server's `elicitation/create`.
#[derive(Clone)]
pub(crate) struct KodaClientHandler {
    pub(crate) server: String,
    pub(crate) info: ClientConfig,
    pub(crate) elicitor: ElicitorSlot,
}

impl KodaClientHandler {
    /// The handler for `server`, saying it can elicit in both modes.
    pub(crate) fn new(server: String, mut info: ClientConfig, elicitor: ElicitorSlot) -> Self {
        let mut capabilities: ClientCapabilities = info.capabilities.clone();
        capabilities.elicitation = Some(
            ElicitationCapability::new()
                .with_form(FormElicitationCapability::new())
                .with_url(UrlElicitationCapability::default()),
        );
        info.capabilities = capabilities;
        Self {
            server,
            info,
            elicitor,
        }
    }
}

impl ClientHandler for KodaClientHandler {
    fn get_info(&self) -> ClientConfig {
        self.info.clone()
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, McpError> {
        let request = match request {
            ElicitRequestParams::FormElicitationParams {
                message,
                requested_schema,
                ..
            } => ElicitationRequest {
                server: self.server.clone(),
                message,
                requested_schema: serde_json::to_value(requested_schema).ok(),
                url: None,
            },
            ElicitRequestParams::UrlElicitationParams { message, url, .. } => ElicitationRequest {
                server: self.server.clone(),
                message,
                requested_schema: None,
                url: Some(url),
            },
            // A mode newer than Koda knows: declined.
            _ => return Ok(ElicitResult::new(ElicitationAction::Decline)),
        };
        let answer = match self.elicitor.get() {
            Some(elicitor) => elicitor.elicit(request).await,
            None => ElicitationAnswer::Decline,
        };
        Ok(match answer {
            ElicitationAnswer::Accept(content) => {
                let mut result = ElicitResult::new(ElicitationAction::Accept);
                result.content = content;
                result
            }
            ElicitationAnswer::Decline => ElicitResult::new(ElicitationAction::Decline),
            ElicitationAnswer::Cancel => ElicitResult::new(ElicitationAction::Cancel),
        })
    }
}
