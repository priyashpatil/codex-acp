use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

use super::*;

/// Abstraction over the ACP connection for sending notifications and requests
/// back to the client. This replaces the old `Client` trait usage.
pub(super) trait ClientSender: Send + Sync + 'static {
    fn send_session_notification(&self, notif: SessionNotification) -> Result<(), Error>;
    fn request_permission(
        &self,
        req: RequestPermissionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<RequestPermissionResponse, Error>> + Send + '_>>;
}

/// Production implementation that wraps a `ConnectionTo<Client>`.
struct AcpConnection(ConnectionTo<Client>);

impl ClientSender for AcpConnection {
    fn send_session_notification(&self, notif: SessionNotification) -> Result<(), Error> {
        self.0.send_notification(notif)
    }

    fn request_permission(
        &self,
        req: RequestPermissionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<RequestPermissionResponse, Error>> + Send + '_>> {
        Box::pin(async move { self.0.send_request(req).block_task().await })
    }
}

#[derive(Clone)]
pub(super) struct SessionClient {
    session_id: SessionId,
    client: Arc<dyn ClientSender>,
    client_capabilities: Arc<Mutex<ClientCapabilities>>,
}

impl SessionClient {
    pub(super) fn new(
        session_id: SessionId,
        cx: ConnectionTo<Client>,
        client_capabilities: Arc<Mutex<ClientCapabilities>>,
    ) -> Self {
        Self {
            session_id,
            client: Arc::new(AcpConnection(cx)),
            client_capabilities,
        }
    }

    #[cfg(test)]
    pub(super) fn with_client(
        session_id: SessionId,
        client: Arc<dyn ClientSender>,
        client_capabilities: Arc<Mutex<ClientCapabilities>>,
    ) -> Self {
        Self {
            session_id,
            client,
            client_capabilities,
        }
    }

    pub(super) fn supports_terminal_output(&self, active_command: &ActiveCommand) -> bool {
        active_command.terminal_output
            && self
                .client_capabilities
                .lock()
                .unwrap()
                .meta
                .as_ref()
                .is_some_and(|v| {
                    v.get("terminal_output")
                        .is_some_and(|v| v.as_bool().unwrap_or_default())
                })
    }

    pub(super) fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub(super) fn send_notification(&self, update: SessionUpdate) {
        if let Err(e) = self
            .client
            .send_session_notification(SessionNotification::new(self.session_id.clone(), update))
        {
            error!("Failed to send session notification: {:?}", e);
        }
    }

    pub(super) fn send_user_message(&self, text: impl Into<String>) {
        self.send_notification(SessionUpdate::UserMessageChunk(ContentChunk::new(
            text.into().into(),
        )));
    }

    pub(super) fn send_agent_text(&self, text: impl Into<String>) {
        self.send_notification(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            text.into().into(),
        )));
    }

    pub(super) fn send_agent_thought(&self, text: impl Into<String>) {
        self.send_notification(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            text.into().into(),
        )));
    }

    pub(super) fn send_tool_call(&self, tool_call: ToolCall) {
        self.send_notification(SessionUpdate::ToolCall(tool_call));
    }

    pub(super) fn send_tool_call_update(&self, update: ToolCallUpdate) {
        self.send_notification(SessionUpdate::ToolCallUpdate(update));
    }

    /// Send a completed tool call (used for replay and simple cases)
    pub(super) fn send_completed_tool_call(
        &self,
        call_id: impl Into<ToolCallId>,
        title: impl Into<String>,
        kind: ToolKind,
        raw_input: Option<serde_json::Value>,
    ) {
        let mut tool_call = ToolCall::new(call_id, title)
            .kind(kind)
            .status(ToolCallStatus::Completed);
        if let Some(input) = raw_input {
            tool_call = tool_call.raw_input(input);
        }
        self.send_tool_call(tool_call);
    }

    /// Send a tool call completion update (used for replay)
    pub(super) fn send_tool_call_completed(
        &self,
        call_id: impl Into<ToolCallId>,
        raw_output: Option<serde_json::Value>,
    ) {
        let mut fields = ToolCallUpdateFields::new().status(ToolCallStatus::Completed);
        if let Some(output) = raw_output {
            fields = fields.raw_output(output);
        }
        self.send_tool_call_update(ToolCallUpdate::new(call_id, fields));
    }

    pub(super) fn update_plan(&self, plan: Vec<PlanItemArg>) {
        self.send_notification(SessionUpdate::Plan(Plan::new(
            plan.into_iter()
                .map(|entry| {
                    PlanEntry::new(
                        entry.step,
                        PlanEntryPriority::Medium,
                        match entry.status {
                            StepStatus::Pending => PlanEntryStatus::Pending,
                            StepStatus::InProgress => PlanEntryStatus::InProgress,
                            StepStatus::Completed => PlanEntryStatus::Completed,
                        },
                    )
                })
                .collect(),
        )));
    }

    pub(super) async fn request_permission(
        &self,
        tool_call: ToolCallUpdate,
        options: Vec<PermissionOption>,
    ) -> Result<RequestPermissionResponse, Error> {
        self.client
            .request_permission(RequestPermissionRequest::new(
                self.session_id.clone(),
                tool_call,
                options,
            ))
            .await
    }
}
