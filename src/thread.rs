use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
};

use agent_client_protocol::{
    Client, ConnectionTo, Error,
    schema::{
        AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, ClientCapabilities,
        ConfigOptionUpdate, Content, ContentBlock, ContentChunk, Cost, Diff, EmbeddedResource,
        EmbeddedResourceResource, LoadSessionResponse, Meta, ModelId, ModelInfo, PermissionOption,
        PermissionOptionKind, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus, PromptRequest,
        RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
        ResourceLink, SelectedPermissionOutcome, SessionConfigId, SessionConfigOption,
        SessionConfigOptionCategory, SessionConfigOptionValue, SessionConfigSelectOption,
        SessionConfigValueId, SessionId, SessionInfoUpdate, SessionMode, SessionModeId,
        SessionModeState, SessionModelState, SessionNotification, SessionUpdate, StopReason,
        Terminal, TextContent, TextResourceContents, ToolCall, ToolCallContent, ToolCallId,
        ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
        UnstructuredCommandInput, UsageUpdate,
    },
};
use codex_apply_patch::parse_patch;
use codex_core::{
    CodexThread,
    config::{Config, set_project_trust_level},
    review_format::format_review_findings_block,
    review_prompts::user_facing_hint,
    util::normalize_thread_name,
};
use codex_login::auth::AuthManager;
use codex_models_manager::manager::{ModelsManager, RefreshStrategy};
use codex_protocol::{
    approvals::{
        ElicitationRequest, ElicitationRequestEvent, GuardianAssessmentAction,
        GuardianCommandSource,
    },
    config_types::{
        CollaborationMode, CollaborationModeMask, ModeKind, ServiceTier, Settings, TrustLevel,
    },
    dynamic_tools::{DynamicToolCallOutputContentItem, DynamicToolCallRequest},
    error::CodexErr,
    items::TurnItem,
    mcp::CallToolResult,
    models::{
        ActivePermissionProfile, AdditionalPermissionProfile, PermissionProfile, ResponseItem,
        WebSearchAction,
    },
    openai_models::{ModelPreset, ReasoningEffort},
    parse_command::ParsedCommand,
    permissions::{
        FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry, FileSystemSpecialPath,
    },
    plan_tool::{PlanItemArg, StepStatus, UpdatePlanArgs},
    protocol::{
        AgentMessageContentDeltaEvent, AgentMessageEvent, AgentReasoningEvent,
        AgentReasoningRawContentEvent, AgentReasoningSectionBreakEvent, AgentStatus,
        ApplyPatchApprovalRequestEvent, BackgroundEventEvent, CollabAgentInteractionBeginEvent,
        CollabAgentInteractionEndEvent, CollabAgentSpawnBeginEvent, CollabAgentSpawnEndEvent,
        CollabCloseBeginEvent, CollabCloseEndEvent, CollabResumeBeginEvent, CollabResumeEndEvent,
        CollabWaitingBeginEvent, CollabWaitingEndEvent, DeprecationNoticeEvent,
        DynamicToolCallResponseEvent, ElicitationAction, ErrorEvent, Event, EventMsg,
        ExecApprovalRequestEvent, ExecCommandBeginEvent, ExecCommandEndEvent,
        ExecCommandOutputDeltaEvent, ExecCommandStatus, ExitedReviewModeEvent, FileChange,
        GuardianAssessmentEvent, GuardianAssessmentStatus, HookCompletedEvent, HookStartedEvent,
        ImageGenerationBeginEvent, ImageGenerationEndEvent, ItemCompletedEvent, ItemStartedEvent,
        ListSkillsResponseEvent, McpInvocation, McpStartupCompleteEvent, McpStartupUpdateEvent,
        McpToolCallBeginEvent, McpToolCallEndEvent, ModelRerouteEvent, NetworkApprovalContext,
        NetworkPolicyRuleAction, Op, PatchApplyBeginEvent, PatchApplyEndEvent, PatchApplyStatus,
        PatchApplyUpdatedEvent, PlanDeltaEvent, RawResponseItemEvent, ReasoningContentDeltaEvent,
        ReasoningRawContentDeltaEvent, ReviewDecision, ReviewOutputEvent, ReviewRequest,
        ReviewTarget, RolloutItem, SkillMetadata, SkillsListEntry, StreamErrorEvent,
        TerminalInteractionEvent, ThreadGoalStatus, ThreadGoalUpdatedEvent, ThreadRolledBackEvent,
        TokenCountEvent, TurnAbortedEvent, TurnCompleteEvent, TurnStartedEvent, UserMessageEvent,
        ViewImageToolCallEvent, WarningEvent, WebSearchBeginEvent, WebSearchEndEvent,
    },
    request_permissions::{
        PermissionGrantScope, RequestPermissionProfile, RequestPermissionsEvent,
        RequestPermissionsResponse,
    },
    request_user_input::{
        RequestUserInputAnswer, RequestUserInputEvent, RequestUserInputQuestion,
        RequestUserInputResponse,
    },
    user_input::UserInput,
};
use codex_shell_command::parse_command::parse_command;
use codex_utils_approval_presets::{ApprovalPreset, builtin_approval_presets};
use heck::ToTitleCase;
use itertools::Itertools;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

mod client;
mod commands;
mod config;
mod permissions;
mod tools;

#[cfg(test)]
use client::ClientSender;
use client::SessionClient;
use commands::{
    builtin_commands, extract_slash_command, format_skills, run_git_diff, skill_commands,
    skills_for_cwd,
};
#[cfg(test)]
use config::CODEX_WORKSPACE_PROFILE_ID;
use config::{
    APPROVAL_PRESETS, active_profile_id_for_session_mode, current_session_mode_id,
    format_service_tier_name, mode_trusts_project, persist_approval_preset_default,
    persist_model_default, persist_service_tier_default,
};
#[cfg(test)]
use permissions::{
    MCP_TOOL_APPROVAL_ALLOW_ALWAYS_OPTION_ID, MCP_TOOL_APPROVAL_ALLOW_OPTION_ID,
    MCP_TOOL_APPROVAL_ALLOW_SESSION_OPTION_ID, MCP_TOOL_APPROVAL_CANCEL_OPTION_ID,
    MCP_TOOL_APPROVAL_PERSIST_SESSION, MCP_TOOL_APPROVAL_REQUEST_ID_PREFIX,
};
use permissions::{
    ResolvedMcpElicitation, build_exec_permission_options,
    build_supported_mcp_elicitation_permission_request, build_user_input_permission_options,
};
#[cfg(test)]
use tools::guardian_action_summary;
use tools::{
    ParseCommandToolCall, agent_status_to_tool_status, aggregate_agent_statuses,
    extract_tool_call_content_from_changes, format_file_system_entries, generate_fallback_id,
    guardian_assessment_content, guardian_assessment_tool_call_id,
    guardian_assessment_tool_call_status, parse_command_tool_call,
    response_item_status_to_tool_status, web_search_action_to_title_and_id,
};

const INIT_COMMAND_PROMPT: &str = include_str!("./prompt_for_init_command.md");

/// Trait for abstracting over the `CodexThread` to make testing easier.
pub trait CodexThreadImpl: Send + Sync {
    fn submit(&self, op: Op)
    -> Pin<Box<dyn Future<Output = Result<String, CodexErr>> + Send + '_>>;
    fn next_event(&self) -> Pin<Box<dyn Future<Output = Result<Event, CodexErr>> + Send + '_>>;
}

impl CodexThreadImpl for CodexThread {
    fn submit(
        &self,
        op: Op,
    ) -> Pin<Box<dyn Future<Output = Result<String, CodexErr>> + Send + '_>> {
        Box::pin(self.submit(op))
    }

    fn next_event(&self) -> Pin<Box<dyn Future<Output = Result<Event, CodexErr>> + Send + '_>> {
        Box::pin(self.next_event())
    }
}

pub trait ModelsManagerImpl: Send + Sync {
    fn get_model(
        &self,
        model_id: &Option<String>,
    ) -> Pin<Box<dyn Future<Output = String> + Send + '_>>;
    fn list_models(&self) -> Pin<Box<dyn Future<Output = Vec<ModelPreset>> + Send + '_>>;
    fn list_collaboration_modes(
        &self,
    ) -> Pin<Box<dyn Future<Output = Vec<CollaborationModeMask>> + Send + '_>>;
}

impl ModelsManagerImpl for Arc<dyn ModelsManager> {
    fn get_model(
        &self,
        model_id: &Option<String>,
    ) -> Pin<Box<dyn Future<Output = String> + Send + '_>> {
        let model_id = model_id.clone();
        Box::pin(async move {
            self.get_default_model(&model_id, RefreshStrategy::OnlineIfUncached)
                .await
        })
    }

    fn list_models(&self) -> Pin<Box<dyn Future<Output = Vec<ModelPreset>> + Send + '_>> {
        Box::pin(async move {
            ModelsManager::list_models(self.as_ref(), RefreshStrategy::OnlineIfUncached).await
        })
    }

    fn list_collaboration_modes(
        &self,
    ) -> Pin<Box<dyn Future<Output = Vec<CollaborationModeMask>> + Send + '_>> {
        Box::pin(async move { self.as_ref().list_collaboration_modes() })
    }
}

pub trait Auth {
    fn logout(&self) -> impl Future<Output = Result<bool, Error>> + Send;
}

impl Auth for Arc<AuthManager> {
    async fn logout(&self) -> Result<bool, Error> {
        self.as_ref()
            .logout()
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))
    }
}

enum ThreadMessage {
    Load {
        response_tx: oneshot::Sender<Result<LoadSessionResponse, Error>>,
    },
    SkillsLoaded {
        skills: Option<Vec<SkillMetadata>>,
    },
    GetConfigOptions {
        response_tx: oneshot::Sender<Result<Vec<SessionConfigOption>, Error>>,
    },
    Prompt {
        request: PromptRequest,
        response_tx: oneshot::Sender<Result<oneshot::Receiver<Result<StopReason, Error>>, Error>>,
    },
    SetMode {
        mode: SessionModeId,
        response_tx: oneshot::Sender<Result<(), Error>>,
    },
    SetModel {
        model: ModelId,
        response_tx: oneshot::Sender<Result<(), Error>>,
    },
    SetConfigOption {
        config_id: SessionConfigId,
        value: SessionConfigOptionValue,
        response_tx: oneshot::Sender<Result<(), Error>>,
    },
    Cancel {
        response_tx: oneshot::Sender<Result<(), Error>>,
    },
    Shutdown {
        response_tx: oneshot::Sender<Result<(), Error>>,
    },
    ReplayHistory {
        history: Vec<RolloutItem>,
        response_tx: oneshot::Sender<Result<(), Error>>,
    },
    SubmitPlanImplementation {
        approval_preset_id: String,
        response_tx: Option<oneshot::Sender<Result<StopReason, Error>>>,
    },
    PermissionRequestResolved {
        submission_id: String,
        request_key: String,
        response: Result<RequestPermissionResponse, Error>,
    },
}

pub struct Thread {
    /// Direct handle to the underlying Codex thread for out-of-band shutdown.
    thread: Arc<dyn CodexThreadImpl>,
    /// A sender for interacting with the thread.
    message_tx: mpsc::UnboundedSender<ThreadMessage>,
    /// Keep the actor task alive for the lifetime of the thread wrapper.
    _handle: tokio::task::JoinHandle<()>,
}

impl Thread {
    pub fn new(
        session_id: SessionId,
        thread: Arc<dyn CodexThreadImpl>,
        auth: Arc<AuthManager>,
        models_manager: Arc<dyn ModelsManagerImpl>,
        client_capabilities: Arc<Mutex<ClientCapabilities>>,
        config: Config,
        cx: ConnectionTo<Client>,
    ) -> Self {
        let (message_tx, message_rx) = mpsc::unbounded_channel();
        let (resolution_tx, resolution_rx) = mpsc::unbounded_channel();

        let actor = ThreadActor::new(
            auth,
            SessionClient::new(session_id, cx, client_capabilities),
            thread.clone(),
            models_manager,
            config,
            message_rx,
            resolution_tx,
            resolution_rx,
        );
        let handle = tokio::spawn(actor.spawn());

        Self {
            thread,
            message_tx,
            _handle: handle,
        }
    }

    pub async fn load(&self) -> Result<LoadSessionResponse, Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::Load { response_tx };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn config_options(&self) -> Result<Vec<SessionConfigOption>, Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::GetConfigOptions { response_tx };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn prompt(&self, request: PromptRequest) -> Result<StopReason, Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::Prompt {
            request,
            response_tx,
        };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))??
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn set_mode(&self, mode: SessionModeId) -> Result<(), Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::SetMode { mode, response_tx };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn set_model(&self, model: ModelId) -> Result<(), Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::SetModel { model, response_tx };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn set_config_option(
        &self,
        config_id: SessionConfigId,
        value: SessionConfigOptionValue,
    ) -> Result<(), Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::SetConfigOption {
            config_id,
            value,
            response_tx,
        };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn cancel(&self) -> Result<(), Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::Cancel { response_tx };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn replay_history(&self, history: Vec<RolloutItem>) -> Result<(), Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::ReplayHistory {
            history,
            response_tx,
        };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn shutdown(&self) -> Result<(), Error> {
        let (response_tx, response_rx) = oneshot::channel();
        let message = ThreadMessage::Shutdown { response_tx };

        if self.message_tx.send(message).is_err() {
            self.thread
                .submit(Op::Shutdown)
                .await
                .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
        } else {
            response_rx
                .await
                .map_err(|e| Error::internal_error().data(e.to_string()))??;
        }
        // Let the actor drain the resulting turn-aborted/shutdown events so any in-flight
        // prompt callers observe a clean cancellation instead of a dropped response channel.
        Ok(())
    }
}

enum PendingPermissionRequest {
    Exec {
        approval_id: String,
        turn_id: String,
        option_map: HashMap<String, ReviewDecision>,
    },
    Patch {
        call_id: String,
        option_map: HashMap<String, ReviewDecision>,
    },
    RequestPermissions {
        call_id: String,
        permissions: RequestPermissionProfile,
    },
    McpElicitation {
        server_name: String,
        request_id: codex_protocol::mcp::RequestId,
        option_map: HashMap<String, ResolvedMcpElicitation>,
    },
    PlanImplementation,
    UserInput {
        turn_id: String,
        call_id: String,
        questions: Vec<RequestUserInputQuestion>,
        question_index: usize,
        answers: HashMap<String, RequestUserInputAnswer>,
        option_map: HashMap<String, String>,
    },
}

struct PendingPermissionInteraction {
    request: PendingPermissionRequest,
    task: tokio::task::JoinHandle<()>,
}

fn exec_request_key(call_id: &str) -> String {
    format!("exec:{call_id}")
}

fn patch_request_key(call_id: &str) -> String {
    format!("patch:{call_id}")
}

fn permissions_request_key(call_id: &str) -> String {
    format!("permissions:{call_id}")
}

fn user_input_request_key(call_id: &str, question_index: usize) -> String {
    format!("user-input:{call_id}:{question_index}")
}

fn plan_implementation_request_key(submission_id: &str) -> String {
    format!("plan-implementation:{submission_id}")
}

fn context_compaction_call_id(item_id: &str) -> String {
    format!("context-compaction:{item_id}")
}

fn context_compaction_status_text(status: ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Completed => "Context compacted.",
        ToolCallStatus::Failed => "Context compaction did not complete.",
        ToolCallStatus::Pending => "Context compaction pending.",
        ToolCallStatus::InProgress => "Context compaction still running.",
        _ => "Context compaction state updated.",
    }
}

fn mode_kind_as_id(mode: ModeKind) -> &'static str {
    match mode {
        ModeKind::Plan => "plan",
        ModeKind::Default => "default",
        ModeKind::PairProgramming => "pair_programming",
        ModeKind::Execute => "execute",
    }
}

fn collaboration_mode_description(mode: ModeKind) -> Option<&'static str> {
    match mode {
        ModeKind::Plan => Some(PLAN_MODE_DESCRIPTION),
        ModeKind::Default | ModeKind::PairProgramming | ModeKind::Execute => None,
    }
}

const PLAN_MODE_DESCRIPTION: &str =
    "Codex can help create and refine a plan before implementation.";
const PLAN_IMPLEMENTATION_ACCEPT_DEFAULT_OPTION_ID: &str = "accept-plan-default";
const PLAN_IMPLEMENTATION_ACCEPT_FULL_ACCESS_OPTION_ID: &str = "accept-plan-full-access";
const PLAN_IMPLEMENTATION_STAY_OPTION_ID: &str = "stay-in-plan";
const PLAN_IMPLEMENTATION_CODING_MESSAGE: &str = "Implement the plan.";

fn truncate_for_title(text: &str) -> String {
    const MAX_GRAPHEMES: usize = 80;
    let single_line = text.split_whitespace().join(" ");
    if single_line.graphemes(true).count() <= MAX_GRAPHEMES {
        return single_line;
    }

    format!(
        "{}...",
        single_line
            .graphemes(true)
            .take(MAX_GRAPHEMES.saturating_sub(3))
            .collect::<String>()
    )
}

fn subagent_title(prefix: &str, nickname: Option<&str>, role: Option<&str>) -> String {
    match (nickname, role) {
        (Some(nickname), Some(role)) => format!("{prefix}: {nickname} ({role})"),
        (Some(nickname), None) => format!("{prefix}: {nickname}"),
        (None, Some(role)) => format!("{prefix}: {role}"),
        (None, None) => prefix.to_string(),
    }
}

fn format_thread_goal_update(event: &ThreadGoalUpdatedEvent) -> String {
    let status = match event.goal.status {
        ThreadGoalStatus::Active => "active",
        ThreadGoalStatus::Paused => "paused",
        ThreadGoalStatus::BudgetLimited => "budget limited",
        ThreadGoalStatus::Complete => "complete",
    };

    let objective = event.goal.objective.trim();
    if objective.contains('\n') {
        format!("Goal updated ({status}):\n{objective}")
    } else {
        format!("Goal updated ({status}): {objective}")
    }
}

#[expect(clippy::large_enum_variant)]
enum SubmissionState {
    /// Loading skills for the current workspace.
    Skills(SkillsState),
    /// User prompts, including slash commands like /init, /review, /compact, /undo.
    Prompt(PromptState),
    Rename(RenameState),
}

impl SubmissionState {
    fn is_active(&self) -> bool {
        match self {
            Self::Skills(state) => state.is_active(),
            Self::Prompt(state) => state.is_active(),
            Self::Rename(state) => state.is_active(),
        }
    }

    async fn handle_event(&mut self, client: &SessionClient, event: EventMsg) {
        match self {
            Self::Skills(state) => state.handle_event(event),
            Self::Prompt(state) => state.handle_event(client, event).await,
            Self::Rename(state) => state.handle_event(client, event),
        }
    }

    async fn handle_permission_request_resolved(
        &mut self,
        client: &SessionClient,
        request_key: String,
        response: Result<RequestPermissionResponse, Error>,
    ) -> Result<(), Error> {
        match self {
            Self::Skills(..) => Ok(()),
            Self::Prompt(state) => {
                state
                    .handle_permission_request_resolved(client, request_key, response)
                    .await
            }
            Self::Rename(_) => {
                warn!("Ignoring permission response for rename submission");
                Ok(())
            }
        }
    }

    fn abort_pending_interactions(&mut self) {
        match self {
            Self::Prompt(state) => {
                state.abort_pending_interactions();
            }
            Self::Skills(_) => {}
            Self::Rename(_) => {}
        }
    }

    fn fail(&mut self, err: Error) {
        match self {
            Self::Skills(state) => {
                if let Some(response_tx) = state.response_tx.take() {
                    drop(response_tx.send(Err(err)));
                }
            }
            Self::Prompt(state) => {
                if let Some(response_tx) = state.response_tx.take() {
                    drop(response_tx.send(Err(err)));
                }
            }
            Self::Rename(state) => state.fail(err),
        }
    }
}

struct SkillsState {
    response_tx: Option<oneshot::Sender<Result<Vec<SkillsListEntry>, Error>>>,
}

impl SkillsState {
    fn new(response_tx: oneshot::Sender<Result<Vec<SkillsListEntry>, Error>>) -> Self {
        Self {
            response_tx: Some(response_tx),
        }
    }

    fn is_active(&self) -> bool {
        let Some(response_tx) = &self.response_tx else {
            return false;
        };
        !response_tx.is_closed()
    }

    fn handle_event(&mut self, event: EventMsg) {
        match event {
            EventMsg::ListSkillsResponse(ListSkillsResponseEvent { skills }) => {
                if let Some(tx) = self.response_tx.take() {
                    drop(tx.send(Ok(skills)));
                }
            }
            event => {
                warn!("Unexpected event: {event:?}");
            }
        }
    }
}

struct ActiveCommand {
    tool_call_id: ToolCallId,
    terminal_output: bool,
    output: String,
    file_extension: Option<String>,
}

#[derive(Debug, Clone)]
struct ActiveSubagent {
    tool_call_id: ToolCallId,
    thread_id: Option<String>,
}

#[derive(Default)]
struct AccumulatedUsage {
    input_tokens: i64,
    cached_input_tokens: i64,
    output_tokens: i64,
    reasoning_output_tokens: i64,
}

impl AccumulatedUsage {
    fn add(&mut self, usage: &codex_protocol::protocol::TokenUsage) {
        self.input_tokens += usage.input_tokens;
        self.cached_input_tokens += usage.cached_input_tokens;
        self.output_tokens += usage.output_tokens;
        self.reasoning_output_tokens += usage.reasoning_output_tokens;
    }
}

struct PromptState {
    submission_id: String,
    active_commands: HashMap<String, ActiveCommand>,
    active_web_searches: HashSet<String>,
    active_context_compactions: HashSet<String>,
    active_patch_applies: HashSet<String>,
    active_guardian_assessments: HashSet<String>,
    active_subagents_by_call: HashMap<String, ActiveSubagent>,
    active_subagent_calls_by_thread: HashMap<String, ToolCallId>,
    accumulated_usage: AccumulatedUsage,
    thread: Arc<dyn CodexThreadImpl>,
    resolution_tx: mpsc::UnboundedSender<ThreadMessage>,
    pending_permission_interactions: HashMap<String, PendingPermissionInteraction>,
    event_count: usize,
    response_tx: Option<oneshot::Sender<Result<StopReason, Error>>>,
    seen_message_deltas: bool,
    seen_reasoning_deltas: bool,
    turn_complete: bool,
    turn_collaboration_mode_kind: ModeKind,
    saw_plan_output: bool,
    plan_output_text: Option<String>,
    prompted_for_plan_implementation: bool,
}

struct RenameState {
    response_tx: Option<oneshot::Sender<Result<StopReason, Error>>>,
}

impl RenameState {
    fn new(response_tx: oneshot::Sender<Result<StopReason, Error>>) -> Self {
        Self {
            response_tx: Some(response_tx),
        }
    }

    fn is_active(&self) -> bool {
        self.response_tx
            .as_ref()
            .is_some_and(|response_tx| !response_tx.is_closed())
    }

    fn fail(&mut self, err: Error) {
        if let Some(response_tx) = self.response_tx.take() {
            drop(response_tx.send(Err(err)));
        }
    }

    fn handle_event(&mut self, client: &SessionClient, event: EventMsg) {
        match event {
            EventMsg::ThreadNameUpdated(event) => {
                if let Some(title) = event.thread_name {
                    send_session_title_update(client, Some(title.clone()));
                    client.send_agent_text(format!("Thread renamed to: {title}\n"));
                    if let Some(response_tx) = self.response_tx.take() {
                        drop(response_tx.send(Ok(StopReason::EndTurn)));
                    }
                }
            }
            EventMsg::Error(ErrorEvent {
                message,
                codex_error_info,
            }) => {
                if let Some(response_tx) = self.response_tx.take() {
                    drop(response_tx.send(Err(Error::internal_error().data(
                        json!({ "message": message, "codex_error_info": codex_error_info }),
                    ))));
                }
            }
            EventMsg::StreamError(StreamErrorEvent {
                message,
                codex_error_info,
                additional_details,
            }) => {
                error!(
                    "Rename failed during stream: {message} {codex_error_info:?} {additional_details:?}"
                );
                if let Some(response_tx) = self.response_tx.take() {
                    drop(response_tx.send(Err(Error::internal_error().data(
                        json!({ "message": message, "codex_error_info": codex_error_info }),
                    ))));
                }
            }
            event => {
                debug!("Ignoring event for rename submission: {event:?}");
            }
        }
    }
}

impl PromptState {
    fn new(
        submission_id: String,
        thread: Arc<dyn CodexThreadImpl>,
        resolution_tx: mpsc::UnboundedSender<ThreadMessage>,
        response_tx: oneshot::Sender<Result<StopReason, Error>>,
    ) -> Self {
        Self {
            submission_id,
            active_commands: HashMap::new(),
            active_web_searches: HashSet::new(),
            active_context_compactions: HashSet::new(),
            active_patch_applies: HashSet::new(),
            active_guardian_assessments: HashSet::new(),
            active_subagents_by_call: HashMap::new(),
            active_subagent_calls_by_thread: HashMap::new(),
            accumulated_usage: AccumulatedUsage::default(),
            thread,
            resolution_tx,
            pending_permission_interactions: HashMap::new(),
            event_count: 0,
            response_tx: Some(response_tx),
            seen_message_deltas: false,
            seen_reasoning_deltas: false,
            turn_complete: false,
            turn_collaboration_mode_kind: ModeKind::Default,
            saw_plan_output: false,
            plan_output_text: None,
            prompted_for_plan_implementation: false,
        }
    }

    fn is_active(&self) -> bool {
        if !self.turn_complete || !self.pending_permission_interactions.is_empty() {
            return true;
        }
        let Some(response_tx) = &self.response_tx else {
            return false;
        };
        !response_tx.is_closed()
    }

    fn abort_pending_interactions(&mut self) {
        for (_, interaction) in self.pending_permission_interactions.drain() {
            interaction.task.abort();
        }
    }

    fn start_context_compaction(&mut self, client: &SessionClient, item_id: &str) {
        let call_id = context_compaction_call_id(item_id);
        if !self.active_context_compactions.insert(call_id.clone()) {
            return;
        }

        client.send_tool_call(
            ToolCall::new(call_id, "Compacting context")
                .kind(ToolKind::Think)
                .status(ToolCallStatus::InProgress)
                .content(vec![
                    "Condensing earlier conversation so the next turn can continue."
                        .to_string()
                        .into(),
                ]),
        );
    }

    fn settle_context_compaction(
        &mut self,
        client: &SessionClient,
        item_id: &str,
        status: ToolCallStatus,
    ) {
        let call_id = context_compaction_call_id(item_id);
        let content = context_compaction_status_text(status).to_string();

        if self.active_context_compactions.remove(&call_id) {
            client.send_tool_call_update(ToolCallUpdate::new(
                call_id,
                ToolCallUpdateFields::new()
                    .status(status)
                    .content(vec![content.into()]),
            ));
        } else if matches!(status, ToolCallStatus::Completed | ToolCallStatus::Failed) {
            client.send_tool_call(
                ToolCall::new(call_id, "Compacting context")
                    .kind(ToolKind::Think)
                    .status(status)
                    .content(vec![content.into()]),
            );
        }
    }

    fn settle_all_context_compactions(&mut self, client: &SessionClient, status: ToolCallStatus) {
        for call_id in self.active_context_compactions.drain().collect::<Vec<_>>() {
            client.send_tool_call_update(ToolCallUpdate::new(
                call_id,
                ToolCallUpdateFields::new().status(status).content(vec![
                    context_compaction_status_text(status).to_string().into(),
                ]),
            ));
        }
    }

    fn settle_all_patch_applies(&mut self, client: &SessionClient, status: ToolCallStatus) {
        let content = match status {
            ToolCallStatus::Completed => "Edit completed.",
            ToolCallStatus::Failed => "Edit interrupted before completion.",
            ToolCallStatus::Pending => "Edit pending.",
            ToolCallStatus::InProgress => "Edit still running.",
            _ => "Edit status unknown.",
        }
        .to_string();

        for call_id in self.active_patch_applies.drain().collect::<Vec<_>>() {
            client.send_tool_call_update(ToolCallUpdate::new(
                call_id,
                ToolCallUpdateFields::new()
                    .status(status)
                    .content(vec![content.clone().into()]),
            ));
        }
    }

    fn spawn_permission_request(
        &mut self,
        client: &SessionClient,
        request_key: String,
        pending_request: PendingPermissionRequest,
        tool_call: ToolCallUpdate,
        options: Vec<PermissionOption>,
    ) {
        let client = client.clone();
        let resolution_tx = self.resolution_tx.clone();
        let submission_id = self.submission_id.clone();
        let resolved_request_key = request_key.clone();
        let handle = tokio::spawn(async move {
            let response = client.request_permission(tool_call, options).await;
            drop(
                resolution_tx.send(ThreadMessage::PermissionRequestResolved {
                    submission_id,
                    request_key: resolved_request_key,
                    response,
                }),
            );
        });

        if let Some(interaction) = self.pending_permission_interactions.insert(
            request_key,
            PendingPermissionInteraction {
                request: pending_request,
                task: handle,
            },
        ) {
            interaction.task.abort();
        }
    }

    fn spawn_plan_implementation_request(&mut self, client: &SessionClient) {
        self.prompted_for_plan_implementation = true;
        let request_key = plan_implementation_request_key(&self.submission_id);
        let call_id = request_key.clone();
        let prompt_content = self.plan_output_text.clone().unwrap_or_else(|| {
            "The plan is ready. Choose whether to switch to Default mode and start implementation, or stay in Plan mode to keep discussing it.".to_string()
        });
        self.spawn_permission_request(
            client,
            request_key,
            PendingPermissionRequest::PlanImplementation,
            ToolCallUpdate::new(
                ToolCallId::new(call_id),
                ToolCallUpdateFields::new()
                    .kind(ToolKind::Think)
                    .status(ToolCallStatus::Pending)
                    .title("Implement this plan?")
                    .raw_input(serde_json::json!({
                        "request_type": "plan_implementation",
                    }))
                    .content(vec![ToolCallContent::Content(Content::new(
                        ContentBlock::Text(TextContent::new(prompt_content)),
                    ))]),
            ),
            vec![
                PermissionOption::new(
                    PLAN_IMPLEMENTATION_ACCEPT_DEFAULT_OPTION_ID,
                    "Accept and continue with Default profile",
                    PermissionOptionKind::AllowOnce,
                ),
                PermissionOption::new(
                    PLAN_IMPLEMENTATION_ACCEPT_FULL_ACCESS_OPTION_ID,
                    "Accept and continue with Full Access profile",
                    PermissionOptionKind::AllowOnce,
                ),
                PermissionOption::new(
                    PLAN_IMPLEMENTATION_STAY_OPTION_ID,
                    "Reject and continue planning",
                    PermissionOptionKind::RejectOnce,
                ),
            ],
        );
    }

    async fn submit_user_input_answers(
        &self,
        turn_id: String,
        answers: HashMap<String, RequestUserInputAnswer>,
    ) -> Result<(), Error> {
        self.thread
            .submit(Op::UserInputAnswer {
                id: turn_id,
                response: RequestUserInputResponse { answers },
            })
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
        Ok(())
    }

    fn complete_user_input_tool_call(
        &self,
        client: &SessionClient,
        call_id: impl Into<ToolCallId>,
    ) {
        client.send_tool_call_update(ToolCallUpdate::new(
            call_id,
            ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
        ));
    }

    fn mark_plan_implementation_decision(
        &self,
        client: &SessionClient,
        call_id: impl Into<ToolCallId>,
        status: ToolCallStatus,
        title: &'static str,
        decision: &'static str,
    ) {
        let content = self.plan_output_text.as_ref().map(|text| {
            vec![ToolCallContent::Content(Content::new(ContentBlock::Text(
                TextContent::new(text.clone()),
            )))]
        });
        client.send_tool_call_update(ToolCallUpdate::new(
            call_id,
            ToolCallUpdateFields::new()
                .status(status)
                .title(title)
                .content(content)
                .raw_output(serde_json::json!({
                    "decision": decision,
                })),
        ));
    }

    async fn finalize_user_input_answers(
        &self,
        client: &SessionClient,
        call_id: String,
        turn_id: String,
        answers: HashMap<String, RequestUserInputAnswer>,
    ) -> Result<(), Error> {
        self.submit_user_input_answers(turn_id, answers).await?;
        self.complete_user_input_tool_call(client, call_id);
        Ok(())
    }

    fn spawn_user_input_question_request(
        &mut self,
        client: &SessionClient,
        turn_id: String,
        call_id: String,
        questions: Vec<RequestUserInputQuestion>,
        question_index: usize,
        answers: HashMap<String, RequestUserInputAnswer>,
    ) {
        let Some(question) = questions.get(question_index).cloned() else {
            let thread = self.thread.clone();
            let client = client.clone();
            tokio::spawn(async move {
                if let Err(err) = thread
                    .submit(Op::UserInputAnswer {
                        id: turn_id,
                        response: RequestUserInputResponse { answers },
                    })
                    .await
                {
                    warn!("Failed to submit UserInputAnswer fallback: {err}");
                    return;
                }

                client.send_tool_call_update(ToolCallUpdate::new(
                    ToolCallId::new(call_id),
                    ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
                ));
            });
            return;
        };

        let (options, option_map) = build_user_input_permission_options(&question);
        let mut content_lines = vec![question.question.clone()];

        if let Some(question_options) = question.options.as_ref() {
            content_lines.extend(
                question_options
                    .iter()
                    .map(|option| format!("- {}: {}", option.label, option.description)),
            );
        }
        if question.is_other {
            content_lines.push(
                "- Other: custom answer will be available when the client UI supports structured input"
                    .to_string(),
            );
        }

        let title = if question.header.is_empty() {
            "Need user input".to_string()
        } else {
            format!("Need user input: {}", question.header)
        };

        let request_key = user_input_request_key(&call_id, question_index);
        self.spawn_permission_request(
            client,
            request_key,
            PendingPermissionRequest::UserInput {
                turn_id,
                call_id: call_id.clone(),
                questions,
                question_index,
                answers,
                option_map,
            },
            ToolCallUpdate::new(
                ToolCallId::new(call_id),
                ToolCallUpdateFields::new()
                    .kind(ToolKind::Think)
                    .status(ToolCallStatus::Pending)
                    .title(title)
                    .raw_input(serde_json::json!({
                        "request_type": "request_user_input",
                        "question": question,
                        "fallback": "session/request_permission",
                    }))
                    .content(vec![ToolCallContent::Content(Content::new(
                        ContentBlock::Text(TextContent::new(content_lines.join("\n"))),
                    ))]),
            ),
            options,
        );
    }

    async fn handle_permission_request_resolved(
        &mut self,
        client: &SessionClient,
        request_key: String,
        response: Result<RequestPermissionResponse, Error>,
    ) -> Result<(), Error> {
        let Some(interaction) = self.pending_permission_interactions.remove(&request_key) else {
            warn!("Ignoring permission response for unknown request key: {request_key}");
            return Ok(());
        };
        let pending_request = interaction.request;
        let response = response?;

        match pending_request {
            PendingPermissionRequest::Exec {
                approval_id,
                turn_id,
                option_map,
            } => {
                let decision = match response.outcome {
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                        option_id,
                        ..
                    }) => option_map
                        .get(option_id.0.as_ref())
                        .cloned()
                        .unwrap_or(ReviewDecision::Abort),
                    RequestPermissionOutcome::Cancelled | _ => ReviewDecision::Abort,
                };

                self.thread
                    .submit(Op::ExecApproval {
                        id: approval_id,
                        turn_id: Some(turn_id),
                        decision,
                    })
                    .await
                    .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
            }
            PendingPermissionRequest::Patch {
                call_id,
                option_map,
            } => {
                let decision = match response.outcome {
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                        option_id,
                        ..
                    }) => option_map
                        .get(option_id.0.as_ref())
                        .cloned()
                        .unwrap_or(ReviewDecision::Abort),
                    RequestPermissionOutcome::Cancelled | _ => ReviewDecision::Abort,
                };

                self.thread
                    .submit(Op::PatchApproval {
                        id: call_id,
                        decision,
                    })
                    .await
                    .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
            }
            PendingPermissionRequest::RequestPermissions {
                call_id,
                permissions,
            } => {
                let response = match response.outcome {
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                        option_id,
                        ..
                    }) => match option_id.0.as_ref() {
                        "approved-for-session" => RequestPermissionsResponse {
                            permissions: permissions.clone(),
                            scope: PermissionGrantScope::Session,
                            strict_auto_review: false,
                        },
                        "approved" => RequestPermissionsResponse {
                            permissions: permissions.clone(),
                            scope: PermissionGrantScope::Turn,
                            strict_auto_review: false,
                        },
                        _ => RequestPermissionsResponse {
                            permissions: RequestPermissionProfile::default(),
                            scope: PermissionGrantScope::Turn,
                            strict_auto_review: true,
                        },
                    },
                    RequestPermissionOutcome::Cancelled | _ => RequestPermissionsResponse {
                        permissions: RequestPermissionProfile::default(),
                        scope: PermissionGrantScope::Turn,
                        strict_auto_review: true,
                    },
                };

                self.thread
                    .submit(Op::RequestPermissionsResponse {
                        id: call_id,
                        response,
                    })
                    .await
                    .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
            }
            PendingPermissionRequest::McpElicitation {
                server_name,
                request_id,
                option_map,
            } => {
                let response = match response.outcome {
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                        option_id,
                        ..
                    }) => option_map
                        .get(option_id.0.as_ref())
                        .cloned()
                        .unwrap_or_else(ResolvedMcpElicitation::cancel),
                    RequestPermissionOutcome::Cancelled | _ => ResolvedMcpElicitation::cancel(),
                };

                self.thread
                    .submit(Op::ResolveElicitation {
                        server_name,
                        request_id,
                        decision: response.action,
                        content: response.content,
                        meta: response.meta,
                    })
                    .await
                    .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
            }
            PendingPermissionRequest::PlanImplementation => {
                let selected_option_id = match response.outcome {
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                        option_id,
                        ..
                    }) => Some(option_id.0.to_string()),
                    RequestPermissionOutcome::Cancelled | _ => None,
                };

                let approval_preset_id = match selected_option_id.as_deref() {
                    Some(PLAN_IMPLEMENTATION_ACCEPT_DEFAULT_OPTION_ID) => Some("auto"),
                    Some(PLAN_IMPLEMENTATION_ACCEPT_FULL_ACCESS_OPTION_ID) => Some("full-access"),
                    _ => None,
                };

                if let Some(approval_preset_id) = approval_preset_id {
                    self.mark_plan_implementation_decision(
                        client,
                        request_key,
                        ToolCallStatus::Completed,
                        "User accepted plan",
                        "accept_plan",
                    );
                    let response_tx = self.response_tx.take();
                    drop(
                        self.resolution_tx
                            .send(ThreadMessage::SubmitPlanImplementation {
                                approval_preset_id: approval_preset_id.to_string(),
                                response_tx,
                            }),
                    );
                } else {
                    self.mark_plan_implementation_decision(
                        client,
                        request_key,
                        ToolCallStatus::Failed,
                        "Plan rejected",
                        "stay_in_plan",
                    );
                    if let Some(response_tx) = self.response_tx.take() {
                        response_tx.send(Ok(StopReason::EndTurn)).ok();
                    }
                }
            }
            PendingPermissionRequest::UserInput {
                turn_id,
                call_id,
                questions,
                question_index,
                mut answers,
                option_map,
            } => {
                let Some(question) = questions.get(question_index) else {
                    self.finalize_user_input_answers(client, call_id, turn_id, answers)
                        .await?;
                    return Ok(());
                };

                let selected_answer = match response.outcome {
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                        option_id,
                        ..
                    }) => option_map.get(option_id.0.as_ref()).cloned(),
                    RequestPermissionOutcome::Cancelled | _ => None,
                };

                let Some(answer) = selected_answer else {
                    self.finalize_user_input_answers(client, call_id, turn_id, answers)
                        .await?;
                    return Ok(());
                };

                answers.insert(
                    question.id.clone(),
                    RequestUserInputAnswer {
                        answers: vec![answer],
                    },
                );

                let next_question_index = question_index + 1;
                if next_question_index >= questions.len() {
                    self.finalize_user_input_answers(client, call_id, turn_id, answers)
                        .await?;
                    return Ok(());
                }

                self.spawn_user_input_question_request(
                    client,
                    turn_id,
                    call_id,
                    questions,
                    next_question_index,
                    answers,
                );
            }
        }

        Ok(())
    }

    #[expect(clippy::too_many_lines)]
    async fn handle_event(&mut self, client: &SessionClient, event: EventMsg) {
        self.event_count += 1;

        // Complete any previous web search before starting a new one
        match &event {
            EventMsg::Error(..)
            | EventMsg::StreamError(..)
            | EventMsg::WebSearchBegin(..)
            | EventMsg::UserMessage(..)
            | EventMsg::ExecApprovalRequest(..)
            | EventMsg::ExecCommandBegin(..)
            | EventMsg::ExecCommandOutputDelta(..)
            | EventMsg::ExecCommandEnd(..)
            | EventMsg::McpToolCallBegin(..)
            | EventMsg::McpToolCallEnd(..)
            | EventMsg::ApplyPatchApprovalRequest(..)
            | EventMsg::PatchApplyBegin(..)
            | EventMsg::PatchApplyEnd(..)
            | EventMsg::TurnStarted(..)
            | EventMsg::TurnComplete(..)
            | EventMsg::TurnDiff(..)
            | EventMsg::TurnAborted(..)
            | EventMsg::EnteredReviewMode(..)
            | EventMsg::ExitedReviewMode(..)
            | EventMsg::RequestUserInput(..)
            | EventMsg::ShutdownComplete => {
                self.complete_web_search(client);
            }
            _ => {}
        }

        match event {
            EventMsg::TurnStarted(TurnStartedEvent {
                model_context_window,
                collaboration_mode_kind,
                turn_id,
                started_at: _,
            }) => {
                info!("Task started with context window of {turn_id} {model_context_window:?} {collaboration_mode_kind:?}");
                self.turn_complete = false;
                self.turn_collaboration_mode_kind = collaboration_mode_kind;
                self.saw_plan_output = false;
                self.plan_output_text = None;
                self.prompted_for_plan_implementation = false;
            }
            EventMsg::TokenCount(TokenCountEvent { info, rate_limits }) => {
                if let Some(info) = info
                    && let Some(size) = info.model_context_window {
                        self.accumulated_usage.add(&info.last_token_usage);
                        let used = info.total_token_usage.tokens_in_context_window().max(0) as u64;
                        let mut update = UsageUpdate::new(used, size as u64);
                        if let Some(rate_limits) = rate_limits
                            && let Some(credits) = rate_limits.credits
                            && let Some(balance) = credits.balance
                            && let Ok(amount) = balance.parse::<f64>()
                        {
                            update = update.cost(Cost::new(amount, "USD"));
                        }
                        client.send_notification(SessionUpdate::UsageUpdate(update));
                    }
            }
            EventMsg::ItemStarted(ItemStartedEvent { thread_id, turn_id, item }) => {
                info!("Item started with thread_id: {thread_id}, turn_id: {turn_id}, item: {item:?}");
                if let TurnItem::ContextCompaction(item) = item {
                    self.start_context_compaction(client, &item.id);
                }
            }
            EventMsg::UserMessage(UserMessageEvent {
                message,
                images: _,
                text_elements: _,
                local_images: _,
            }) => {
                info!("User message: {message:?}");
            }
            EventMsg::AgentMessageContentDelta(AgentMessageContentDeltaEvent {
                thread_id,
                turn_id,
                item_id,
                delta,
            }) => {
                info!("Agent message content delta received: thread_id: {thread_id}, turn_id: {turn_id}, item_id: {item_id}, delta: {delta:?}");
                self.seen_message_deltas = true;
                client.send_agent_text(delta);
            }
            EventMsg::ReasoningContentDelta(ReasoningContentDeltaEvent {
                thread_id,
                turn_id,
                item_id,
                delta,
                summary_index: index,
            })
            | EventMsg::ReasoningRawContentDelta(ReasoningRawContentDeltaEvent {
                thread_id,
                turn_id,
                item_id,
                delta,
                content_index: index,
            }) => {
                info!("Agent reasoning content delta received: thread_id: {thread_id}, turn_id: {turn_id}, item_id: {item_id}, index: {index}, delta: {delta:?}");
                self.seen_reasoning_deltas = true;
                client.send_agent_thought(delta);
            }
            EventMsg::AgentReasoningSectionBreak(AgentReasoningSectionBreakEvent {
                item_id,
                summary_index,
            }) => {
                info!("Agent reasoning section break received:  item_id: {item_id}, index: {summary_index}");
                // Make sure the section heading actually get spacing
                self.seen_reasoning_deltas = true;
                client.send_agent_thought("\n\n");
            }
            EventMsg::AgentMessage(AgentMessageEvent { message , phase: _, memory_citation: _ }) => {
                info!("Agent message (non-delta) received: {message:?}");
                // We didn't receive this message via streaming
                if !std::mem::take(&mut self.seen_message_deltas) {
                    client.send_agent_text(message);
                }
            }
            EventMsg::AgentReasoning(AgentReasoningEvent { text }) => {
                info!("Agent reasoning (non-delta) received: {text:?}");
                // We didn't receive this message via streaming
                if !std::mem::take(&mut self.seen_reasoning_deltas) {
                    client.send_agent_thought(text);
                }
            }
            EventMsg::AgentReasoningRawContent(AgentReasoningRawContentEvent { text }) => {
                info!("Agent reasoning raw content received");
                client.send_agent_thought(text);
            }
            EventMsg::ThreadNameUpdated(event) => {
                info!("Thread name updated: {:?}", event.thread_name);
                send_session_title_update(client, event.thread_name);
            }
            EventMsg::SessionConfigured(event) => {
                info!("Session configured with thread name: {:?}", event.thread_name);
                send_session_title_update(client, event.thread_name);
            }
            EventMsg::ThreadGoalUpdated(event) => {
                info!("Thread goal updated: {:?}", event.goal.objective);
                client.send_agent_text(format_thread_goal_update(&event));
            }
            EventMsg::PlanUpdate(UpdatePlanArgs { explanation, plan }) => {
                // Send this to the client via session/update notification
                info!("Agent plan updated. Explanation: {:?}", explanation);
                if !plan.is_empty() {
                    self.saw_plan_output = true;
                }
                client.update_plan(plan);
            }
            EventMsg::PlanDelta(PlanDeltaEvent {
                thread_id: _,
                turn_id: _,
                item_id: _,
                delta,
            }) => {
                client.send_agent_thought(delta);
            }
            EventMsg::WebSearchBegin(WebSearchBeginEvent { call_id }) => {
                info!("Web search started: call_id={}", call_id);
                // Create a ToolCall notification for the search beginning
                self.start_web_search(client, call_id);
            }
            EventMsg::WebSearchEnd(WebSearchEndEvent {
                call_id,
                query,
                action,
            }) => {
                info!("Web search query received: call_id={call_id}, query={query}");
                // Send update that the search is in progress with the query
                // (WebSearchEnd just means we have the query, not that results are ready)
                self.update_web_search_query(client, call_id, query, action);
                // The actual search results will come through AgentMessage events
                // We mark as completed when a new tool call begins
            }
            EventMsg::ExecApprovalRequest(event) => {
                info!(
                    "Command execution started: call_id={}, command={:?}",
                    event.call_id, event.command
                );
                if let Err(err) = self.exec_approval(client, event)
                    && let Some(response_tx) = self.response_tx.take()
                {
                    drop(response_tx.send(Err(err)));
                }
            }
            EventMsg::ExecCommandBegin(event) => {
                info!(
                    "Command execution started: call_id={}, command={:?}",
                    event.call_id, event.command
                );
                self.exec_command_begin(client, event);
            }
            EventMsg::ExecCommandOutputDelta(delta_event) => {
                self.exec_command_output_delta(client, delta_event);
            }
            EventMsg::ExecCommandEnd(end_event) => {
                info!(
                    "Command execution ended: call_id={}, exit_code={}",
                    end_event.call_id, end_event.exit_code
                );
                self.exec_command_end(client, end_event);
            }
            EventMsg::TerminalInteraction(event) => {
                info!(
                    "Terminal interaction: call_id={}, process_id={}, stdin={}",
                    event.call_id, event.process_id, event.stdin
                );
                self.terminal_interaction(client, event);
            }
            EventMsg::DynamicToolCallRequest(DynamicToolCallRequest { call_id, turn_id, namespace, tool, arguments }) => {
                info!("Dynamic tool call request: call_id={call_id}, turn_id={turn_id}, namespace={namespace:?}, tool={tool}");
                self.start_dynamic_tool_call(client, call_id, tool, arguments);
            }
            EventMsg::DynamicToolCallResponse(event) => {
                info!(
                    "Dynamic tool call response: call_id={}, turn_id={}, tool={}",
                    event.call_id, event.turn_id, event.tool
                );
                self.end_dynamic_tool_call(client, event);
            }
            EventMsg::CollabAgentSpawnBegin(event) => {
                info!("Subagent spawn begin: call_id={}", event.call_id);
                self.start_subagent_spawn(client, event);
            }
            EventMsg::CollabAgentSpawnEnd(event) => {
                info!(
                    "Subagent spawn end: call_id={}, new_thread_id={:?}, status={:?}",
                    event.call_id, event.new_thread_id, event.status
                );
                self.end_subagent_spawn(client, event);
            }
            EventMsg::CollabAgentInteractionBegin(event) => {
                info!(
                    "Subagent interaction begin: call_id={}, receiver_thread_id={}",
                    event.call_id, event.receiver_thread_id
                );
                self.start_subagent_interaction(client, event);
            }
            EventMsg::CollabAgentInteractionEnd(event) => {
                info!(
                    "Subagent interaction end: call_id={}, receiver_thread_id={}, status={:?}",
                    event.call_id, event.receiver_thread_id, event.status
                );
                self.end_subagent_interaction(client, event);
            }
            EventMsg::CollabWaitingBegin(event) => {
                info!("Subagent wait begin: call_id={}", event.call_id);
                self.start_subagent_wait(client, event);
            }
            EventMsg::CollabWaitingEnd(event) => {
                info!("Subagent wait end: call_id={}", event.call_id);
                self.end_subagent_wait(client, event);
            }
            EventMsg::CollabResumeBegin(event) => {
                info!(
                    "Subagent resume begin: call_id={}, receiver_thread_id={}",
                    event.call_id, event.receiver_thread_id
                );
                self.start_subagent_resume(client, event);
            }
            EventMsg::CollabResumeEnd(event) => {
                info!(
                    "Subagent resume end: call_id={}, receiver_thread_id={}, status={:?}",
                    event.call_id, event.receiver_thread_id, event.status
                );
                self.end_subagent_resume(client, event);
            }
            EventMsg::CollabCloseBegin(event) => {
                info!(
                    "Subagent close begin: call_id={}, receiver_thread_id={}",
                    event.call_id, event.receiver_thread_id
                );
                self.start_subagent_close(client, event);
            }
            EventMsg::CollabCloseEnd(event) => {
                info!(
                    "Subagent close end: call_id={}, receiver_thread_id={}, status={:?}",
                    event.call_id, event.receiver_thread_id, event.status
                );
                self.end_subagent_close(client, event);
            }
            EventMsg::McpToolCallBegin(McpToolCallBeginEvent {
                call_id,
                invocation,
                mcp_app_resource_uri: _
            }) => {
                info!(
                    "MCP tool call begin: call_id={call_id}, invocation={} {}",
                    invocation.server, invocation.tool
                );
                self.start_mcp_tool_call(client, call_id, invocation);
            }
            EventMsg::McpToolCallEnd(McpToolCallEndEvent {
                call_id,
                invocation,
                duration,
                result,
                mcp_app_resource_uri: _,
            }) => {
                info!(
                    "MCP tool call ended: call_id={call_id}, invocation={} {}, duration={duration:?}",
                    invocation.server, invocation.tool
                );
                self.end_mcp_tool_call(client, call_id, result);
            }
            EventMsg::ApplyPatchApprovalRequest(event) => {
                info!(
                    "Apply patch approval request: call_id={}, reason={:?}",
                    event.call_id, event.reason
                );
                if let Err(err) = self.patch_approval(client, event)
                    && let Some(response_tx) = self.response_tx.take()
                {
                    drop(response_tx.send(Err(err)));
                }
            }
            EventMsg::PatchApplyBegin(event) => {
                info!(
                    "Patch apply begin: call_id={}, auto_approved={}",
                    event.call_id, event.auto_approved
                );
                self.start_patch_apply(client, event);
            }
            EventMsg::PatchApplyUpdated(event) => {
                info!(
                    "Patch apply updated: call_id={}, change_count={}",
                    event.call_id,
                    event.changes.len()
                );
                self.update_patch_apply(client, event);
            }
            EventMsg::PatchApplyEnd(event) => {
                info!(
                    "Patch apply end: call_id={}, success={}",
                    event.call_id, event.success
                );
                self.end_patch_apply(client, event);
            }
            EventMsg::HookStarted(HookStartedEvent { turn_id: _, run }) => {
                info!("Hook started: {} ({:?})", run.id, run.event_name);
                client.send_agent_text(format!(
                    "Running hook: {} ({:?})...\n",
                    run.id, run.event_name
                ));
            }
            EventMsg::HookCompleted(HookCompletedEvent { turn_id: _, run }) => {
                let status_msg = run.status_message.as_deref().unwrap_or("");
                info!(
                    "Hook completed: {} ({:?}) {}",
                    run.id, run.status, status_msg
                );
                client.send_agent_text(format!(
                    "Hook completed: {} ({:?}){}\n",
                    run.id,
                    run.status,
                    if status_msg.is_empty() {
                        String::new()
                    } else {
                        format!(": {status_msg}")
                    }
                ));
            }
            EventMsg::ImageGenerationBegin(ImageGenerationBeginEvent { call_id }) => {
                info!("Image generation started: call_id={call_id}");
                client.send_tool_call(
                    ToolCall::new(call_id, "Generating image")
                        .kind(ToolKind::Other)
                        .status(ToolCallStatus::InProgress),
                );
            }
            EventMsg::ImageGenerationEnd(ImageGenerationEndEvent {
                call_id,
                status,
                revised_prompt: _,
                result,
                saved_path: _,
            }) => {
                info!("Image generation ended: call_id={call_id}, status={status}");
                let tool_status = if status == "success" {
                    ToolCallStatus::Completed
                } else {
                    ToolCallStatus::Failed
                };
                client.send_tool_call_update(ToolCallUpdate::new(
                    call_id,
                    ToolCallUpdateFields::new()
                        .status(tool_status)
                        .content(vec![result.into()]),
                ));
            }
            EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id,
                turn_id,
                item,
            }) => {
                info!("Item completed: thread_id={}, turn_id={}, item={:?}", thread_id, turn_id, item);
                match item {
                    TurnItem::Plan(plan_item) => {
                        self.saw_plan_output = true;
                        self.plan_output_text = Some(plan_item.text);
                    }
                    TurnItem::ContextCompaction(item) => {
                        self.settle_context_compaction(
                            client,
                            &item.id,
                            ToolCallStatus::Completed,
                        );
                    }
                    _ => {}
                }
            }
            EventMsg::TurnComplete(TurnCompleteEvent { last_agent_message, turn_id, completed_at: _, duration_ms: _, time_to_first_token_ms: _, }) => {
                info!(
                    "Task {turn_id} completed successfully after {} events. Last agent message: {last_agent_message:?}",
                    self.event_count
                );
                self.settle_all_context_compactions(client, ToolCallStatus::Completed);
                self.settle_all_patch_applies(client, ToolCallStatus::Completed);
                self.abort_pending_interactions();
                self.turn_complete = true;
                if self.turn_collaboration_mode_kind == ModeKind::Plan
                    && self.saw_plan_output
                    && !self.prompted_for_plan_implementation
                {
                    self.spawn_plan_implementation_request(client);
                } else if let Some(response_tx) = self.response_tx.take() {
                    response_tx.send(Ok(StopReason::EndTurn)).ok();
                }
            }
            EventMsg::UndoStarted(event) => {
                client.send_agent_text(
                    event
                        .message
                        .unwrap_or_else(|| "Undo in progress...".to_string()),
                );
            }
            EventMsg::UndoCompleted(event) => {
                let fallback = if event.success {
                    "Undo completed.".to_string()
                } else {
                    "Undo failed.".to_string()
                };
                client.send_agent_text(event.message.unwrap_or(fallback));
            }
            EventMsg::StreamError(StreamErrorEvent {
                message,
                codex_error_info,
                additional_details,
            }) => {
                error!(
                    "Handled error during turn: {message} {codex_error_info:?} {additional_details:?}"
                );
                self.turn_complete = true;
            }
            EventMsg::Error(ErrorEvent {
                message,
                codex_error_info,
            }) => {
                error!("Unhandled error during turn: {message} {codex_error_info:?}");
                self.settle_all_context_compactions(client, ToolCallStatus::Failed);
                self.settle_all_patch_applies(client, ToolCallStatus::Failed);
                self.abort_pending_interactions();
                self.turn_complete = true;
                if let Some(response_tx) = self.response_tx.take() {
                    response_tx
                        .send(Err(Error::internal_error().data(
                            json!({ "message": message, "codex_error_info": codex_error_info }),
                        )))
                        .ok();
                }
            }
            EventMsg::TurnAborted(TurnAbortedEvent { reason, turn_id, completed_at: _, duration_ms: _ }) => {
                info!("Turn {turn_id:?} aborted: {reason:?}");
                self.settle_all_context_compactions(client, ToolCallStatus::Failed);
                self.settle_all_patch_applies(client, ToolCallStatus::Failed);
                self.abort_pending_interactions();
                self.turn_complete = true;
                if let Some(response_tx) = self.response_tx.take() {
                    response_tx.send(Ok(StopReason::Cancelled)).ok();
                }
            }
            EventMsg::ShutdownComplete => {
                info!("Agent shutting down");
                self.settle_all_context_compactions(client, ToolCallStatus::Failed);
                self.settle_all_patch_applies(client, ToolCallStatus::Failed);
                self.abort_pending_interactions();
                self.turn_complete = true;
                if let Some(response_tx) = self.response_tx.take() {
                    response_tx.send(Ok(StopReason::Cancelled)).ok();
                }
            }
            EventMsg::ViewImageToolCall(ViewImageToolCallEvent { call_id, path }) => {
                info!("ViewImageToolCallEvent received");
                let display_path = path.display().to_string();
                client.send_notification(
                    SessionUpdate::ToolCall(
                        ToolCall::new(call_id, format!("View Image {display_path}"))
                            .kind(ToolKind::Read).status(ToolCallStatus::Completed)
                            .content(vec![ToolCallContent::Content(Content::new(ContentBlock::ResourceLink(ResourceLink::new(display_path.clone(), display_path.clone())
                        )
                    )
                )]).locations(vec![ToolCallLocation::new(path)])));
            }
            EventMsg::EnteredReviewMode(review_request) => {
                info!("Review begin: request={review_request:?}");
            }
            EventMsg::ExitedReviewMode(event) => {
                info!("Review end: output={event:?}");
                if let Err(err) = self.review_mode_exit(client, event)
                    && let Some(response_tx) = self.response_tx.take()
                {
                    drop(response_tx.send(Err(err)));
                }
            }
            EventMsg::Warning(WarningEvent { message })
            | EventMsg::GuardianWarning(WarningEvent { message }) => {
                warn!("Warning: {message}");
                // Forward warnings to the client as agent messages so users see
                // informational notices (e.g., the post-compact advisory message).
                client.send_agent_text(message);
            }
            EventMsg::McpStartupUpdate(McpStartupUpdateEvent { server, status }) => {
                info!("MCP startup update: server={server}, status={status:?}");
            }
            EventMsg::McpStartupComplete(McpStartupCompleteEvent {
                ready,
                failed,
                cancelled,
            }) => {
                info!(
                    "MCP startup complete: ready={ready:?}, failed={failed:?}, cancelled={cancelled:?}"
                );
            }
            EventMsg::ElicitationRequest(event) => {
                info!("Elicitation request: server={}, id={:?}", event.server_name, event.id);
                if let Err(err) = self.mcp_elicitation(client, event).await
                    && let Some(response_tx) = self.response_tx.take()
                {
                    drop(response_tx.send(Err(err)));
                }
            }
            EventMsg::ModelReroute(ModelRerouteEvent { from_model, to_model, reason }) => {
                info!("Model reroute: from={from_model}, to={to_model}, reason={reason:?}");
            }
            EventMsg::ModelVerification(event) => {
                info!("Model verification requested: {event:?}");
            }

            EventMsg::ContextCompacted(..) => {
                info!("Context compacted");
                client.send_agent_text("Context compacted\n".to_string());
            }
            EventMsg::ThreadRolledBack(ThreadRolledBackEvent { num_turns }) => {
                info!("Thread rolled back: {num_turns} turns removed");
                let suffix = if num_turns == 1 { "" } else { "s" };
                client.send_agent_text(format!(
                    "Thread rolled back: {num_turns} turn{suffix} removed from context.\n"
                ));
            }
            EventMsg::BackgroundEvent(BackgroundEventEvent { message }) => {
                info!("Background event: {message}");
                client.send_agent_text(format!("{message}\n"));
            }
            EventMsg::DeprecationNotice(DeprecationNoticeEvent { summary, details }) => {
                warn!("Deprecation notice: {summary}");
                let message = if let Some(details) = details {
                    format!("**Deprecation:** {summary}\n{details}\n")
                } else {
                    format!("**Deprecation:** {summary}\n")
                };
                client.send_agent_text(message);
            }
            EventMsg::RequestPermissions(event) => {
                info!("Request permissions: {} {}", event.call_id, event.turn_id);
                if let Err(err) = self.request_permissions(client, event)
                    && let Some(response_tx) = self.response_tx.take()
                {
                    drop(response_tx.send(Err(err)));
                }
            }
            EventMsg::RequestUserInput(event) => {
                info!(
                    "Request user input: call_id={}, turn_id={}, questions={}",
                    event.call_id,
                    event.turn_id,
                    event.questions.len()
                );
                if let Err(err) = self.request_user_input(client, event).await
                    && let Some(response_tx) = self.response_tx.take()
                {
                    drop(response_tx.send(Err(err)));
                }
            }
            EventMsg::GuardianAssessment(event) => {
                info!(
                    "Guardian assessment: id={}, status={:?}, turn_id={}",
                    event.id, event.status, event.turn_id
                );
                self.guardian_assessment(client, event);
            }
            EventMsg::RawResponseItem(event) => {
                self.raw_response_item(client, event);
            }

            // Ignore these events
            EventMsg::TurnDiff(..)
            | EventMsg::SkillsUpdateAvailable
            // Old events
            | EventMsg::AgentMessageDelta(..)
            | EventMsg::AgentReasoningDelta(..)
            | EventMsg::AgentReasoningRawContentDelta(..)
            | EventMsg::RealtimeConversationStarted(..)
            | EventMsg::RealtimeConversationRealtime(..)
            | EventMsg::RealtimeConversationClosed(..)
            | EventMsg::RealtimeConversationSdp(..)=> {}
            e @ (EventMsg::McpListToolsResponse(..)
            | EventMsg::ListSkillsResponse(..)
            | EventMsg::RealtimeConversationListVoicesResponse(..)
            // Used for returning a single history entry
            | EventMsg::GetHistoryEntryResponse(..)) => {
                warn!("Unexpected event: {:?}", e);
            }
        }
    }

    async fn request_user_input(
        &mut self,
        client: &SessionClient,
        event: RequestUserInputEvent,
    ) -> Result<(), Error> {
        let RequestUserInputEvent {
            call_id,
            turn_id,
            questions,
        } = event;

        if questions.is_empty() {
            self.finalize_user_input_answers(client, call_id, turn_id, HashMap::new())
                .await?;
            return Ok(());
        }

        self.spawn_user_input_question_request(
            client,
            turn_id,
            call_id,
            questions,
            0,
            HashMap::new(),
        );

        Ok(())
    }

    async fn mcp_elicitation(
        &mut self,
        client: &SessionClient,
        event: ElicitationRequestEvent,
    ) -> Result<(), Error> {
        let raw_input = serde_json::json!(&event);
        let ElicitationRequestEvent {
            server_name,
            id,
            request,
            turn_id: _,
        } = event;
        if let Some(supported_request) = build_supported_mcp_elicitation_permission_request(
            &server_name,
            &id,
            &request,
            raw_input,
        ) {
            info!(
                "Routing MCP tool approval elicitation through ACP permission request: server={}, id={:?}",
                server_name, id
            );
            self.spawn_permission_request(
                client,
                supported_request.request_key,
                PendingPermissionRequest::McpElicitation {
                    server_name,
                    request_id: id,
                    option_map: supported_request.option_map,
                },
                supported_request.tool_call,
                supported_request.options,
            );
            return Ok(());
        }

        let request_kind = match &request {
            ElicitationRequest::Form { .. } => "form",
            ElicitationRequest::Url { .. } => "url",
        };

        info!(
            "Auto-declining unsupported MCP elicitation: server={}, id={:?}, kind={request_kind}",
            server_name, id
        );

        self.thread
            .submit(Op::ResolveElicitation {
                server_name,
                request_id: id,
                decision: ElicitationAction::Decline,
                content: None,
                meta: None,
            })
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;

        Ok(())
    }

    fn review_mode_exit(
        &self,
        client: &SessionClient,
        event: ExitedReviewModeEvent,
    ) -> Result<(), Error> {
        let ExitedReviewModeEvent { review_output } = event;
        let Some(ReviewOutputEvent {
            findings,
            overall_correctness: _,
            overall_explanation,
            overall_confidence_score: _,
        }) = review_output
        else {
            return Ok(());
        };

        let text = if findings.is_empty() {
            let explanation = overall_explanation.trim();
            if explanation.is_empty() {
                "Reviewer failed to output a response"
            } else {
                explanation
            }
            .to_string()
        } else {
            format_review_findings_block(&findings, None)
        };

        client.send_agent_text(&text);
        Ok(())
    }

    fn patch_approval(
        &mut self,
        client: &SessionClient,
        event: ApplyPatchApprovalRequestEvent,
    ) -> Result<(), Error> {
        let raw_input = serde_json::json!(&event);
        let ApplyPatchApprovalRequestEvent {
            call_id,
            changes,
            reason,
            // grant_root doesn't seem to be set anywhere on the codex side
            grant_root: _,
            turn_id: _,
        } = event;
        let (title, locations, content) = extract_tool_call_content_from_changes(changes);
        let request_key = patch_request_key(&call_id);
        let options = vec![
            PermissionOption::new("approved", "Yes", PermissionOptionKind::AllowOnce),
            PermissionOption::new(
                "abort",
                "No, provide feedback",
                PermissionOptionKind::RejectOnce,
            ),
        ];
        self.spawn_permission_request(
            client,
            request_key,
            PendingPermissionRequest::Patch {
                call_id: call_id.clone(),
                option_map: HashMap::from([
                    ("approved".to_string(), ReviewDecision::Approved),
                    ("abort".to_string(), ReviewDecision::Abort),
                ]),
            },
            ToolCallUpdate::new(
                call_id,
                ToolCallUpdateFields::new()
                    .kind(ToolKind::Edit)
                    .status(ToolCallStatus::Pending)
                    .title(title)
                    .locations(locations)
                    .content(content.chain(reason.map(|r| r.into())).collect::<Vec<_>>())
                    .raw_input(raw_input),
            ),
            options,
        );
        Ok(())
    }

    fn start_patch_apply(&mut self, client: &SessionClient, event: PatchApplyBeginEvent) {
        let raw_input = serde_json::json!(&event);
        let PatchApplyBeginEvent {
            call_id,
            auto_approved: _,
            changes,
            turn_id: _,
        } = event;

        let (title, locations, content) = extract_tool_call_content_from_changes(changes);
        self.active_patch_applies.insert(call_id.clone());

        client.send_tool_call(
            ToolCall::new(call_id, title)
                .kind(ToolKind::Edit)
                .status(ToolCallStatus::InProgress)
                .locations(locations)
                .content(content.collect())
                .raw_input(raw_input),
        );
    }

    fn update_patch_apply(&self, client: &SessionClient, event: PatchApplyUpdatedEvent) {
        let raw_input = serde_json::json!(&event);
        let PatchApplyUpdatedEvent { call_id, changes } = event;

        if changes.is_empty() {
            return;
        }

        let (title, locations, content) = extract_tool_call_content_from_changes(changes);

        client.send_tool_call_update(ToolCallUpdate::new(
            call_id,
            ToolCallUpdateFields::new()
                .kind(ToolKind::Edit)
                .status(ToolCallStatus::InProgress)
                .title(title)
                .locations(locations)
                .content(content.collect::<Vec<_>>())
                .raw_input(raw_input),
        ));
    }

    fn end_patch_apply(&mut self, client: &SessionClient, event: PatchApplyEndEvent) {
        let raw_output = serde_json::json!(&event);
        let PatchApplyEndEvent {
            call_id,
            stdout: _,
            stderr: _,
            success,
            changes,
            turn_id: _,
            status,
        } = event;

        let (title, locations, content) = if !changes.is_empty() {
            let (title, locations, content) = extract_tool_call_content_from_changes(changes);
            (Some(title), Some(locations), Some(content.collect()))
        } else {
            (None, None, None)
        };

        let status = match status {
            PatchApplyStatus::Completed => ToolCallStatus::Completed,
            _ if success => ToolCallStatus::Completed,
            PatchApplyStatus::Failed | PatchApplyStatus::Declined => ToolCallStatus::Failed,
        };
        self.active_patch_applies.remove(&call_id);

        client.send_tool_call_update(ToolCallUpdate::new(
            call_id,
            ToolCallUpdateFields::new()
                .status(status)
                .raw_output(raw_output)
                .title(title)
                .locations(locations)
                .content(content),
        ));
    }

    fn start_dynamic_tool_call(
        &self,
        client: &SessionClient,
        call_id: String,
        tool: String,
        arguments: serde_json::Value,
    ) {
        client.send_tool_call(
            ToolCall::new(call_id, format!("Tool: {tool}"))
                .status(ToolCallStatus::InProgress)
                .raw_input(serde_json::json!(&arguments)),
        );
    }

    fn start_subagent_spawn(&mut self, client: &SessionClient, event: CollabAgentSpawnBeginEvent) {
        let raw_input = serde_json::json!(&event);
        let CollabAgentSpawnBeginEvent {
            call_id, prompt, ..
        } = event;

        let title = if prompt.is_empty() {
            "Spawning subagent".to_string()
        } else {
            format!("Spawning subagent: {}", truncate_for_title(&prompt))
        };

        let tool_call_id = ToolCallId::new(call_id.clone());
        self.active_subagents_by_call.insert(
            call_id.clone(),
            ActiveSubagent {
                tool_call_id: tool_call_id.clone(),
                thread_id: None,
            },
        );

        client.send_tool_call(
            ToolCall::new(tool_call_id, title)
                .status(ToolCallStatus::InProgress)
                .raw_input(raw_input),
        );
    }

    fn end_subagent_spawn(&mut self, client: &SessionClient, event: CollabAgentSpawnEndEvent) {
        let raw_output = serde_json::json!(&event);
        let CollabAgentSpawnEndEvent {
            call_id,
            sender_thread_id: _,
            new_thread_id,
            new_agent_nickname,
            new_agent_role,
            prompt: _,
            model: _,
            reasoning_effort: _,
            status,
        } = event;

        let thread_id = new_thread_id.map(|thread_id| thread_id.to_string());
        let tool_call_id = if let Some(active) = self.active_subagents_by_call.get_mut(&call_id) {
            active.thread_id.clone_from(&thread_id);
            active.tool_call_id.clone()
        } else {
            ToolCallId::new(call_id.clone())
        };

        if let Some(thread_id) = thread_id {
            self.active_subagent_calls_by_thread
                .insert(thread_id, tool_call_id.clone());
        }

        client.send_tool_call_update(ToolCallUpdate::new(
            tool_call_id,
            ToolCallUpdateFields::new()
                .status(agent_status_to_tool_status(&status))
                .title(subagent_title(
                    "Spawned subagent",
                    new_agent_nickname.as_deref(),
                    new_agent_role.as_deref(),
                ))
                .raw_output(raw_output),
        ));
    }

    fn start_subagent_interaction(
        &mut self,
        client: &SessionClient,
        event: CollabAgentInteractionBeginEvent,
    ) {
        let raw_input = serde_json::json!(&event);
        let CollabAgentInteractionBeginEvent {
            call_id,
            sender_thread_id: _,
            receiver_thread_id,
            prompt,
        } = event;
        let receiver_thread_id = receiver_thread_id.to_string();
        let tool_call_id = ToolCallId::new(call_id.clone());

        self.active_subagents_by_call.insert(
            call_id.clone(),
            ActiveSubagent {
                tool_call_id: tool_call_id.clone(),
                thread_id: Some(receiver_thread_id.clone()),
            },
        );
        self.active_subagent_calls_by_thread
            .insert(receiver_thread_id, tool_call_id.clone());

        client.send_tool_call(
            ToolCall::new(
                tool_call_id,
                format!("Messaging subagent: {}", truncate_for_title(&prompt)),
            )
            .status(ToolCallStatus::InProgress)
            .raw_input(raw_input),
        );
    }

    fn end_subagent_interaction(
        &mut self,
        client: &SessionClient,
        event: CollabAgentInteractionEndEvent,
    ) {
        let raw_output = serde_json::json!(&event);
        let CollabAgentInteractionEndEvent {
            call_id,
            sender_thread_id: _,
            receiver_thread_id,
            receiver_agent_nickname,
            receiver_agent_role,
            prompt: _,
            status,
        } = event;
        let receiver_thread_id = receiver_thread_id.to_string();
        let tool_call_id = self
            .active_subagents_by_call
            .get(&call_id)
            .map(|active| active.tool_call_id.clone())
            .or_else(|| {
                self.active_subagent_calls_by_thread
                    .get(&receiver_thread_id)
                    .cloned()
            })
            .unwrap_or_else(|| ToolCallId::new(call_id.clone()));

        self.active_subagent_calls_by_thread
            .insert(receiver_thread_id, tool_call_id.clone());

        client.send_tool_call_update(ToolCallUpdate::new(
            tool_call_id,
            ToolCallUpdateFields::new()
                .status(agent_status_to_tool_status(&status))
                .title(subagent_title(
                    "Subagent replied",
                    receiver_agent_nickname.as_deref(),
                    receiver_agent_role.as_deref(),
                ))
                .raw_output(raw_output),
        ));
    }

    fn start_subagent_wait(&mut self, client: &SessionClient, event: CollabWaitingBeginEvent) {
        let raw_input = serde_json::json!(&event);
        let CollabWaitingBeginEvent {
            sender_thread_id: _,
            receiver_thread_ids,
            receiver_agents,
            call_id,
        } = event;
        let title = format!(
            "Waiting for {} subagent{}",
            receiver_thread_ids.len(),
            if receiver_thread_ids.len() == 1 {
                ""
            } else {
                "s"
            }
        );
        let tool_call_id = ToolCallId::new(call_id.clone());

        self.active_subagents_by_call.insert(
            call_id,
            ActiveSubagent {
                tool_call_id: tool_call_id.clone(),
                thread_id: None,
            },
        );
        for thread_id in receiver_thread_ids {
            self.active_subagent_calls_by_thread
                .insert(thread_id.to_string(), tool_call_id.clone());
        }
        for agent in receiver_agents {
            self.active_subagent_calls_by_thread
                .insert(agent.thread_id.to_string(), tool_call_id.clone());
        }

        client.send_tool_call(
            ToolCall::new(tool_call_id, title)
                .status(ToolCallStatus::InProgress)
                .raw_input(raw_input),
        );
    }

    fn end_subagent_wait(&mut self, client: &SessionClient, event: CollabWaitingEndEvent) {
        let raw_output = serde_json::json!(&event);
        let CollabWaitingEndEvent {
            sender_thread_id: _,
            call_id,
            agent_statuses,
            statuses,
        } = event;
        let tool_call_id = self
            .active_subagents_by_call
            .get(&call_id)
            .map(|active| active.tool_call_id.clone())
            .unwrap_or_else(|| ToolCallId::new(call_id.clone()));
        let status = aggregate_agent_statuses(
            agent_statuses
                .iter()
                .map(|entry| &entry.status)
                .chain(statuses.values()),
        );

        client.send_tool_call_update(ToolCallUpdate::new(
            tool_call_id,
            ToolCallUpdateFields::new()
                .status(status)
                .title("Subagent wait completed")
                .raw_output(raw_output),
        ));
    }

    fn start_subagent_close(&mut self, client: &SessionClient, event: CollabCloseBeginEvent) {
        let raw_input = serde_json::json!(&event);
        let CollabCloseBeginEvent {
            call_id,
            sender_thread_id: _,
            receiver_thread_id,
        } = event;
        let receiver_thread_id = receiver_thread_id.to_string();
        let tool_call_id = ToolCallId::new(call_id.clone());

        self.active_subagents_by_call.insert(
            call_id,
            ActiveSubagent {
                tool_call_id: tool_call_id.clone(),
                thread_id: Some(receiver_thread_id.clone()),
            },
        );
        self.active_subagent_calls_by_thread
            .insert(receiver_thread_id, tool_call_id.clone());

        client.send_tool_call(
            ToolCall::new(tool_call_id, "Closing subagent")
                .status(ToolCallStatus::InProgress)
                .raw_input(raw_input),
        );
    }

    fn end_subagent_close(&mut self, client: &SessionClient, event: CollabCloseEndEvent) {
        let raw_output = serde_json::json!(&event);
        let CollabCloseEndEvent {
            call_id,
            sender_thread_id: _,
            receiver_thread_id,
            receiver_agent_nickname,
            receiver_agent_role,
            status,
        } = event;
        let receiver_thread_id = receiver_thread_id.to_string();
        let tool_call_id = self
            .active_subagents_by_call
            .remove(&call_id)
            .map(|active| active.tool_call_id)
            .or_else(|| {
                self.active_subagent_calls_by_thread
                    .remove(&receiver_thread_id)
            })
            .unwrap_or_else(|| ToolCallId::new(call_id));

        client.send_tool_call_update(ToolCallUpdate::new(
            tool_call_id,
            ToolCallUpdateFields::new()
                .status(agent_status_to_tool_status(&status))
                .title(subagent_title(
                    "Closed subagent",
                    receiver_agent_nickname.as_deref(),
                    receiver_agent_role.as_deref(),
                ))
                .raw_output(raw_output),
        ));
    }

    fn start_subagent_resume(&mut self, client: &SessionClient, event: CollabResumeBeginEvent) {
        let raw_input = serde_json::json!(&event);
        let CollabResumeBeginEvent {
            call_id,
            sender_thread_id: _,
            receiver_thread_id,
            receiver_agent_nickname,
            receiver_agent_role,
        } = event;
        let receiver_thread_id = receiver_thread_id.to_string();
        let tool_call_id = ToolCallId::new(call_id.clone());

        self.active_subagents_by_call.insert(
            call_id,
            ActiveSubagent {
                tool_call_id: tool_call_id.clone(),
                thread_id: Some(receiver_thread_id.clone()),
            },
        );
        self.active_subagent_calls_by_thread
            .insert(receiver_thread_id, tool_call_id.clone());

        client.send_tool_call(
            ToolCall::new(
                tool_call_id,
                subagent_title(
                    "Resuming subagent",
                    receiver_agent_nickname.as_deref(),
                    receiver_agent_role.as_deref(),
                ),
            )
            .status(ToolCallStatus::InProgress)
            .raw_input(raw_input),
        );
    }

    fn end_subagent_resume(&mut self, client: &SessionClient, event: CollabResumeEndEvent) {
        let raw_output = serde_json::json!(&event);
        let CollabResumeEndEvent {
            call_id,
            sender_thread_id: _,
            receiver_thread_id,
            receiver_agent_nickname,
            receiver_agent_role,
            status,
        } = event;
        let receiver_thread_id = receiver_thread_id.to_string();
        let tool_call_id = self
            .active_subagents_by_call
            .get(&call_id)
            .map(|active| active.tool_call_id.clone())
            .or_else(|| {
                self.active_subagent_calls_by_thread
                    .get(&receiver_thread_id)
                    .cloned()
            })
            .unwrap_or_else(|| ToolCallId::new(call_id));

        client.send_tool_call_update(ToolCallUpdate::new(
            tool_call_id,
            ToolCallUpdateFields::new()
                .status(agent_status_to_tool_status(&status))
                .title(subagent_title(
                    "Resumed subagent",
                    receiver_agent_nickname.as_deref(),
                    receiver_agent_role.as_deref(),
                ))
                .raw_output(raw_output),
        ));
    }

    fn start_mcp_tool_call(
        &self,
        client: &SessionClient,
        call_id: String,
        invocation: McpInvocation,
    ) {
        let title = format!("Tool: {}/{}", invocation.server, invocation.tool);
        client.send_tool_call(
            ToolCall::new(call_id, title)
                .status(ToolCallStatus::InProgress)
                .raw_input(serde_json::json!(&invocation)),
        );
    }

    fn end_dynamic_tool_call(&self, client: &SessionClient, event: DynamicToolCallResponseEvent) {
        let raw_output = serde_json::json!(event);
        let DynamicToolCallResponseEvent {
            call_id,
            turn_id: _,
            tool: _,
            arguments: _,
            namespace: _,
            content_items,
            success,
            error,
            duration: _,
        } = event;

        client.send_tool_call_update(ToolCallUpdate::new(
            call_id,
            ToolCallUpdateFields::new()
                .status(if success {
                    ToolCallStatus::Completed
                } else {
                    ToolCallStatus::Failed
                })
                .raw_output(raw_output)
                .content(
                    content_items
                        .into_iter()
                        .map(|item| match item {
                            DynamicToolCallOutputContentItem::InputText { text } => {
                                ToolCallContent::Content(Content::new(text))
                            }
                            DynamicToolCallOutputContentItem::InputImage { image_url } => {
                                ToolCallContent::Content(Content::new(ContentBlock::ResourceLink(
                                    ResourceLink::new(image_url.clone(), image_url),
                                )))
                            }
                        })
                        .chain(error.map(|e| ToolCallContent::Content(Content::new(e))))
                        .collect::<Vec<_>>(),
                ),
        ));
    }

    fn end_mcp_tool_call(
        &self,
        client: &SessionClient,
        call_id: String,
        result: Result<CallToolResult, String>,
    ) {
        let is_error = match result.as_ref() {
            Ok(result) => result.is_error.unwrap_or_default(),
            Err(_) => true,
        };
        let raw_output = match result.as_ref() {
            Ok(result) => serde_json::json!(result),
            Err(err) => serde_json::json!(err),
        };

        client.send_tool_call_update(ToolCallUpdate::new(
            call_id,
            ToolCallUpdateFields::new()
                .status(if is_error {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                })
                .raw_output(raw_output)
                .content(
                    result
                        .ok()
                        .filter(|result| !result.content.is_empty())
                        .map(|result| {
                            result
                                .content
                                .into_iter()
                                .filter_map(|content| {
                                    serde_json::from_value::<ContentBlock>(content).ok()
                                })
                                .map(|content| ToolCallContent::Content(Content::new(content)))
                                .collect()
                        }),
                ),
        ));
    }

    fn exec_approval(
        &mut self,
        client: &SessionClient,
        event: ExecApprovalRequestEvent,
    ) -> Result<(), Error> {
        let available_decisions = event.effective_available_decisions();
        let raw_input = serde_json::json!(&event);
        let ExecApprovalRequestEvent {
            call_id,
            command: _,
            turn_id,
            cwd,
            reason,
            parsed_cmd,
            proposed_execpolicy_amendment,
            approval_id,
            network_approval_context,
            additional_permissions,
            available_decisions: _,
            proposed_network_policy_amendments,
        } = event;

        // Create a new tool call for the command execution
        let tool_call_id = ToolCallId::new(call_id.clone());
        let ParseCommandToolCall {
            title,
            terminal_output,
            file_extension,
            locations,
            kind,
        } = parse_command_tool_call(parsed_cmd, &cwd);
        self.active_commands.insert(
            call_id.clone(),
            ActiveCommand {
                terminal_output,
                tool_call_id: tool_call_id.clone(),
                output: String::new(),
                file_extension,
            },
        );

        let mut content = vec![];

        if let Some(reason) = reason {
            content.push(reason);
        }
        if let Some(amendment) = proposed_execpolicy_amendment.as_ref() {
            content.push(format!(
                "Proposed Amendment: {}",
                amendment.command().join("\n")
            ));
        }
        if let Some(policy) = network_approval_context.as_ref() {
            let NetworkApprovalContext { host, protocol } = policy;
            content.push(format!("Network Approval Context: {:?} {}", protocol, host));
        }
        if let Some(permissions) = additional_permissions.as_ref() {
            content.push(format!(
                "Additional Permissions: {}",
                serde_json::to_string_pretty(&permissions)?
            ));
        }
        content.push(format!(
            "Available Decisions: {}",
            available_decisions.iter().map(|d| d.to_string()).join("\n")
        ));
        if let Some(amendments) = proposed_network_policy_amendments.as_ref() {
            content.push(format!(
                "Proposed Network Policy Amendments: {}",
                amendments
                    .iter()
                    .map(|amendment| format!("{:?} {:?}", amendment.action, amendment.host))
                    .join("\n")
            ));
        }

        let content = if content.is_empty() {
            None
        } else {
            Some(vec![content.join("\n").into()])
        };
        let permission_options = build_exec_permission_options(
            &available_decisions,
            network_approval_context.as_ref(),
            additional_permissions.as_ref(),
        );

        self.spawn_permission_request(
            client,
            exec_request_key(&call_id),
            PendingPermissionRequest::Exec {
                approval_id: approval_id.unwrap_or(call_id.clone()),
                turn_id,
                option_map: permission_options
                    .iter()
                    .map(|option| (option.option_id.to_string(), option.decision.clone()))
                    .collect(),
            },
            ToolCallUpdate::new(
                tool_call_id,
                ToolCallUpdateFields::new()
                    .kind(kind)
                    .status(ToolCallStatus::Pending)
                    .title(title)
                    .raw_input(raw_input)
                    .content(content)
                    .locations(if locations.is_empty() {
                        None
                    } else {
                        Some(locations)
                    }),
            ),
            permission_options
                .into_iter()
                .map(|option| option.permission_option)
                .collect(),
        );

        Ok(())
    }

    fn exec_command_begin(&mut self, client: &SessionClient, event: ExecCommandBeginEvent) {
        let raw_input = serde_json::json!(&event);
        let ExecCommandBeginEvent {
            turn_id: _,
            source: _,
            interaction_input: _,
            call_id,
            command: _,
            cwd,
            parsed_cmd,
            process_id: _,
        } = event;
        // Create a new tool call for the command execution
        let tool_call_id = ToolCallId::new(call_id.clone());
        let ParseCommandToolCall {
            title,
            file_extension,
            locations,
            terminal_output,
            kind,
        } = parse_command_tool_call(parsed_cmd, &cwd);

        let active_command = ActiveCommand {
            tool_call_id: tool_call_id.clone(),
            output: String::new(),
            file_extension,
            terminal_output,
        };
        let (content, meta) = if client.supports_terminal_output(&active_command) {
            let content = vec![ToolCallContent::Terminal(Terminal::new(call_id.clone()))];
            let meta = Some(Meta::from_iter([(
                "terminal_info".to_owned(),
                serde_json::json!({
                    "terminal_id": call_id,
                    "cwd": cwd
                }),
            )]));
            (content, meta)
        } else {
            (vec![], None)
        };

        self.active_commands.insert(call_id.clone(), active_command);

        client.send_tool_call(
            ToolCall::new(tool_call_id, title)
                .kind(kind)
                .status(ToolCallStatus::InProgress)
                .locations(locations)
                .raw_input(raw_input)
                .content(content)
                .meta(meta),
        );
    }

    fn exec_command_output_delta(
        &mut self,
        client: &SessionClient,
        event: ExecCommandOutputDeltaEvent,
    ) {
        let ExecCommandOutputDeltaEvent {
            call_id,
            chunk,
            stream: _,
        } = event;
        // Stream output bytes to the display-only terminal via ToolCallUpdate meta.
        if let Some(active_command) = self.active_commands.get_mut(&call_id) {
            let data_str = String::from_utf8_lossy(&chunk).to_string();

            let update = if client.supports_terminal_output(active_command) {
                ToolCallUpdate::new(
                    active_command.tool_call_id.clone(),
                    ToolCallUpdateFields::new(),
                )
                .meta(Meta::from_iter([(
                    "terminal_output".to_owned(),
                    serde_json::json!({
                        "terminal_id": call_id,
                        "data": data_str
                    }),
                )]))
            } else {
                active_command.output.push_str(&data_str);
                let content = match active_command.file_extension.as_deref() {
                    Some("md") => active_command.output.clone(),
                    Some(ext) => format!(
                        "```{ext}\n{}\n```\n",
                        active_command.output.trim_end_matches('\n')
                    ),
                    None => format!(
                        "```sh\n{}\n```\n",
                        active_command.output.trim_end_matches('\n')
                    ),
                };
                ToolCallUpdate::new(
                    active_command.tool_call_id.clone(),
                    ToolCallUpdateFields::new().content(vec![content.into()]),
                )
            };

            client.send_tool_call_update(update);
        }
    }

    fn exec_command_end(&mut self, client: &SessionClient, event: ExecCommandEndEvent) {
        let raw_output = serde_json::json!(&event);
        let ExecCommandEndEvent {
            turn_id: _,
            command: _,
            cwd: _,
            parsed_cmd: _,
            source: _,
            interaction_input: _,
            call_id,
            exit_code,
            stdout: _,
            stderr: _,
            aggregated_output: _,
            duration: _,
            formatted_output: _,
            process_id: _,
            status,
        } = event;
        if let Some(active_command) = self.active_commands.remove(&call_id) {
            let is_success = exit_code == 0;

            let status = match status {
                ExecCommandStatus::Completed => ToolCallStatus::Completed,
                _ if is_success => ToolCallStatus::Completed,
                ExecCommandStatus::Failed | ExecCommandStatus::Declined => ToolCallStatus::Failed,
            };

            client.send_tool_call_update(
                ToolCallUpdate::new(
                    active_command.tool_call_id.clone(),
                    ToolCallUpdateFields::new()
                        .status(status)
                        .raw_output(raw_output),
                )
                .meta(client.supports_terminal_output(&active_command).then(
                    || {
                        Meta::from_iter([(
                            "terminal_exit".into(),
                            serde_json::json!({
                                "terminal_id": call_id,
                                "exit_code": exit_code,
                                "signal": null
                            }),
                        )])
                    },
                )),
            );
        }
    }

    fn terminal_interaction(&mut self, client: &SessionClient, event: TerminalInteractionEvent) {
        let TerminalInteractionEvent {
            call_id,
            process_id: _,
            stdin,
        } = event;

        let stdin = format!("\n{stdin}\n");
        // Stream output bytes to the display-only terminal via ToolCallUpdate meta.
        if let Some(active_command) = self.active_commands.get_mut(&call_id) {
            let update = if client.supports_terminal_output(active_command) {
                ToolCallUpdate::new(
                    active_command.tool_call_id.clone(),
                    ToolCallUpdateFields::new(),
                )
                .meta(Meta::from_iter([(
                    "terminal_output".to_owned(),
                    serde_json::json!({
                        "terminal_id": call_id,
                        "data": stdin
                    }),
                )]))
            } else {
                active_command.output.push_str(&stdin);
                let content = match active_command.file_extension.as_deref() {
                    Some("md") => active_command.output.clone(),
                    Some(ext) => format!(
                        "```{ext}\n{}\n```\n",
                        active_command.output.trim_end_matches('\n')
                    ),
                    None => format!(
                        "```sh\n{}\n```\n",
                        active_command.output.trim_end_matches('\n')
                    ),
                };
                ToolCallUpdate::new(
                    active_command.tool_call_id.clone(),
                    ToolCallUpdateFields::new().content(vec![content.into()]),
                )
            };

            client.send_tool_call_update(update);
        }
    }

    fn start_web_search(&mut self, client: &SessionClient, call_id: String) {
        self.active_web_searches.insert(call_id.clone());
        client.send_tool_call(ToolCall::new(call_id, "Searching the Web").kind(ToolKind::Fetch));
    }

    fn update_web_search_query(
        &self,
        client: &SessionClient,
        call_id: String,
        query: String,
        action: WebSearchAction,
    ) {
        let title = match &action {
            WebSearchAction::Search { query, queries } => queries
                .as_ref()
                .map(|q| format!("Searching for: {}", q.join(", ")))
                .or_else(|| query.as_ref().map(|q| format!("Searching for: {q}")))
                .unwrap_or_else(|| "Web search".to_string()),
            WebSearchAction::OpenPage { url } => url
                .as_ref()
                .map(|u| format!("Opening: {u}"))
                .unwrap_or_else(|| "Open page".to_string()),
            WebSearchAction::FindInPage { pattern, url } => match (pattern, url) {
                (Some(p), Some(u)) => format!("Finding: {p} in {u}"),
                (Some(p), None) => format!("Finding: {p}"),
                (None, Some(u)) => format!("Find in page: {u}"),
                (None, None) => "Find in page".to_string(),
            },
            WebSearchAction::Other => "Web search".to_string(),
        };

        client.send_tool_call_update(ToolCallUpdate::new(
            call_id,
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::InProgress)
                .title(title)
                .raw_input(serde_json::json!({
                    "query": query,
                    "action": action
                })),
        ));
    }

    fn complete_web_search(&mut self, client: &SessionClient) {
        for call_id in self.active_web_searches.drain().collect::<Vec<_>>() {
            client.send_tool_call_update(ToolCallUpdate::new(
                call_id,
                ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
            ));
        }
    }

    fn request_permissions(
        &mut self,
        client: &SessionClient,
        event: RequestPermissionsEvent,
    ) -> Result<(), Error> {
        let raw_input = serde_json::json!(&event);
        let RequestPermissionsEvent {
            call_id,
            turn_id: _,
            reason,
            permissions,
            cwd: _,
        } = event;

        // Create a new tool call for the command execution
        let tool_call_id = ToolCallId::new(call_id.clone());

        let mut content = vec![];

        if let Some(reason) = reason.as_ref() {
            content.push(reason.clone());
        }
        if let Some(file_system) = permissions.file_system.as_ref() {
            let reads = format_file_system_entries(
                file_system
                    .entries
                    .iter()
                    .filter(|entry| entry.access == FileSystemAccessMode::Read),
            );
            if !reads.is_empty() {
                content.push(format!("File System Read Access: {reads}"));
            }
            let writes = format_file_system_entries(
                file_system
                    .entries
                    .iter()
                    .filter(|entry| entry.access == FileSystemAccessMode::Write),
            );
            if !writes.is_empty() {
                content.push(format!("File System Write Access: {writes}"));
            }
            let denies = format_file_system_entries(
                file_system
                    .entries
                    .iter()
                    .filter(|entry| entry.access == FileSystemAccessMode::None),
            );
            if !denies.is_empty() {
                content.push(format!("File System Denied Access: {denies}"));
            }
        }
        if let Some(network) = permissions.network.as_ref()
            && let Some(enabled) = network.enabled
        {
            content.push(format!("Network Access: {enabled}"));
        }

        let content = if content.is_empty() {
            None
        } else {
            Some(vec![content.join("\n").into()])
        };

        self.spawn_permission_request(
            client,
            permissions_request_key(&call_id),
            PendingPermissionRequest::RequestPermissions {
                call_id,
                permissions,
            },
            ToolCallUpdate::new(
                tool_call_id,
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Pending)
                    .title(reason.unwrap_or_else(|| "Permissions Request".to_string()))
                    .raw_input(raw_input)
                    .content(content),
            ),
            vec![
                PermissionOption::new(
                    "approved-for-session",
                    "Yes, for session",
                    PermissionOptionKind::AllowAlways,
                ),
                PermissionOption::new("approved", "Yes", PermissionOptionKind::AllowOnce),
                PermissionOption::new("abort", "No", PermissionOptionKind::RejectOnce),
            ],
        );

        Ok(())
    }

    fn guardian_assessment(&mut self, client: &SessionClient, event: GuardianAssessmentEvent) {
        let call_id = guardian_assessment_tool_call_id(&event.id);
        let status = guardian_assessment_tool_call_status(&event.status);
        let content = guardian_assessment_content(&event);
        let raw_event = serde_json::json!(&event);

        match event.status {
            GuardianAssessmentStatus::InProgress => {
                if self.active_guardian_assessments.insert(event.id.clone()) {
                    client.send_tool_call(
                        ToolCall::new(call_id, "Guardian Review")
                            .kind(ToolKind::Think)
                            .status(status)
                            .content(content)
                            .raw_input(raw_event),
                    );
                } else {
                    client.send_tool_call_update(ToolCallUpdate::new(
                        call_id,
                        ToolCallUpdateFields::new()
                            .status(status)
                            .content(content)
                            .raw_output(raw_event),
                    ));
                }
            }
            GuardianAssessmentStatus::TimedOut
            | GuardianAssessmentStatus::Approved
            | GuardianAssessmentStatus::Denied
            | GuardianAssessmentStatus::Aborted => {
                if self.active_guardian_assessments.remove(&event.id) {
                    client.send_tool_call_update(ToolCallUpdate::new(
                        call_id,
                        ToolCallUpdateFields::new()
                            .status(status)
                            .content(content)
                            .raw_output(raw_event),
                    ));
                } else {
                    client.send_tool_call(
                        ToolCall::new(call_id, "Guardian Review")
                            .kind(ToolKind::Think)
                            .status(status)
                            .content(content)
                            .raw_input(raw_event),
                    );
                }
            }
        }
    }

    fn raw_response_item(&self, client: &SessionClient, event: RawResponseItemEvent) {
        match event.item {
            ResponseItem::ToolSearchCall {
                call_id,
                status,
                execution,
                arguments,
                ..
            } => {
                let call_id = call_id.unwrap_or_else(|| generate_fallback_id("tool_search"));
                client.send_tool_call(
                    ToolCall::new(call_id, format!("Search tools via {execution}"))
                        .kind(ToolKind::Search)
                        .status(
                            status
                                .as_deref()
                                .map(response_item_status_to_tool_status)
                                .unwrap_or(ToolCallStatus::InProgress),
                        )
                        .raw_input(arguments),
                );
            }
            ResponseItem::ToolSearchOutput {
                call_id: Some(call_id),
                status,
                execution,
                tools,
            } => {
                client.send_tool_call_update(ToolCallUpdate::new(
                    call_id,
                    ToolCallUpdateFields::new()
                        .status(response_item_status_to_tool_status(&status))
                        .title(format!("Tool search completed via {execution}"))
                        .raw_output(serde_json::json!({
                            "status": status,
                            "execution": execution,
                            "tools": tools,
                        })),
                ));
            }
            ResponseItem::ImageGenerationCall {
                id,
                status,
                revised_prompt,
                result,
            } => {
                let content = revised_prompt.as_ref().map(|prompt| {
                    vec![ToolCallContent::Content(Content::new(ContentBlock::Text(
                        TextContent::new(prompt.clone()),
                    )))]
                });
                let mut tool_call = ToolCall::new(id, "Generate image")
                    .kind(ToolKind::Other)
                    .status(response_item_status_to_tool_status(&status))
                    .raw_output(serde_json::json!({
                        "status": status,
                        "revised_prompt": revised_prompt,
                        "result": result,
                    }));
                if let Some(content) = content {
                    tool_call = tool_call.content(content);
                }
                client.send_tool_call(tool_call);
            }
            _ => {}
        }
    }
}

struct ThreadActor<A> {
    /// Allows for logging out from slash commands
    auth: A,
    /// Used for sending messages back to the client.
    client: SessionClient,
    /// The thread associated with this task.
    thread: Arc<dyn CodexThreadImpl>,
    /// The configuration for the thread.
    config: Config,
    /// The models available for this thread.
    models_manager: Arc<dyn ModelsManagerImpl>,
    /// Internal message sender used to route spawned interaction results back to the actor.
    resolution_tx: mpsc::UnboundedSender<ThreadMessage>,
    /// A sender for each interested `Op` submission that needs events routed.
    submissions: HashMap<String, SubmissionState>,
    /// A receiver for incoming thread messages.
    message_rx: mpsc::UnboundedReceiver<ThreadMessage>,
    /// A receiver for spawned interaction results.
    resolution_rx: mpsc::UnboundedReceiver<ThreadMessage>,
    /// Last config options state we emitted to the client, used for deduping updates.
    last_sent_config_options: Option<Vec<SessionConfigOption>>,
    /// Current collaboration mode kind for this session.
    current_collaboration_mode_kind: ModeKind,
    /// Skills discovered for the current working directory.
    skills: Vec<SkillMetadata>,
}

impl<A: Auth> ThreadActor<A> {
    #[expect(clippy::too_many_arguments)]
    fn new(
        auth: A,
        client: SessionClient,
        thread: Arc<dyn CodexThreadImpl>,
        models_manager: Arc<dyn ModelsManagerImpl>,
        config: Config,
        message_rx: mpsc::UnboundedReceiver<ThreadMessage>,
        resolution_tx: mpsc::UnboundedSender<ThreadMessage>,
        resolution_rx: mpsc::UnboundedReceiver<ThreadMessage>,
    ) -> Self {
        Self {
            auth,
            client,
            thread,
            config,
            models_manager,
            resolution_tx,
            submissions: HashMap::new(),
            message_rx,
            resolution_rx,
            last_sent_config_options: None,
            current_collaboration_mode_kind: ModeKind::Default,
            skills: Vec::new(),
        }
    }

    async fn spawn(mut self) {
        let mut message_rx_open = true;
        loop {
            tokio::select! {
                biased;
                message = self.message_rx.recv(), if message_rx_open => match message {
                    Some(message) => self.handle_message(message).await,
                    None => message_rx_open = false,
                },
                message = self.resolution_rx.recv() => if let Some(message) = message {
                    self.handle_message(message).await
                },
                event = self.thread.next_event() => match event {
                    Ok(event) => self.handle_event(event).await,
                    Err(e) => {
                        error!("Error getting next event: {:?}", e);
                        break;
                    }
                }
            }
            // Litter collection of senders with no receivers
            self.submissions
                .retain(|_, submission| submission.is_active());

            if !message_rx_open && self.submissions.is_empty() {
                break;
            }
        }
    }

    async fn handle_message(&mut self, message: ThreadMessage) {
        match message {
            ThreadMessage::Load { response_tx } => {
                let result = self.handle_load().await;
                drop(response_tx.send(result));
                self.refresh_skills(true).await;
            }
            ThreadMessage::SkillsLoaded { skills } => {
                if let Some(skills) = skills {
                    self.skills = skills;
                }
                self.send_available_commands_update();
            }
            ThreadMessage::GetConfigOptions { response_tx } => {
                let result = self.config_options().await;
                drop(response_tx.send(result));
            }
            ThreadMessage::Prompt {
                request,
                response_tx,
            } => {
                let result = self.handle_prompt(request).await;
                drop(response_tx.send(result));
            }
            ThreadMessage::SetMode { mode, response_tx } => {
                let result = self.handle_set_mode(mode).await;
                drop(response_tx.send(result));
                self.maybe_emit_config_options_update().await;
            }
            ThreadMessage::SetModel { model, response_tx } => {
                let result = self.handle_set_model(model).await;
                drop(response_tx.send(result));
                self.maybe_emit_config_options_update().await;
            }
            ThreadMessage::SetConfigOption {
                config_id,
                value,
                response_tx,
            } => {
                let result = self.handle_set_config_option(config_id, value).await;
                drop(response_tx.send(result));
            }
            ThreadMessage::Cancel { response_tx } => {
                let result = self.handle_cancel().await;
                drop(response_tx.send(result));
            }
            ThreadMessage::Shutdown { response_tx } => {
                let result = self.handle_shutdown().await;
                drop(response_tx.send(result));
            }
            ThreadMessage::ReplayHistory {
                history,
                response_tx,
            } => {
                let result = self.handle_replay_history(history);
                drop(response_tx.send(result));
            }
            ThreadMessage::SubmitPlanImplementation {
                approval_preset_id,
                response_tx,
            } => {
                if let Err(err) = self
                    .submit_plan_implementation(&approval_preset_id, response_tx)
                    .await
                {
                    error!("Failed to submit accepted plan implementation: {err:?}");
                    self.client
                        .send_agent_text(format!("Failed to start implementation: {err}"));
                }
                self.maybe_emit_config_options_update().await;
            }
            ThreadMessage::PermissionRequestResolved {
                submission_id,
                request_key,
                response,
            } => {
                let Some(submission) = self.submissions.get_mut(&submission_id) else {
                    warn!(
                        "Ignoring permission response for unknown submission ID: {submission_id}"
                    );
                    return;
                };

                if let Err(err) = submission
                    .handle_permission_request_resolved(&self.client, request_key, response)
                    .await
                {
                    submission.abort_pending_interactions();
                    submission.fail(err);
                }
            }
        }
    }

    fn available_commands(&self) -> Vec<AvailableCommand> {
        let mut commands = builtin_commands();
        commands.extend(skill_commands(&self.skills));
        commands
    }

    fn send_available_commands_update(&self) {
        self.client
            .send_notification(SessionUpdate::AvailableCommandsUpdate(
                AvailableCommandsUpdate::new(self.available_commands()),
            ));
    }

    async fn load_skills(
        &mut self,
        force_reload: bool,
    ) -> oneshot::Receiver<Result<Vec<SkillsListEntry>, Error>> {
        let (response_tx, response_rx) = oneshot::channel();
        let submission_id = match self
            .thread
            .submit(Op::ListSkills {
                cwds: Vec::new(),
                force_reload,
            })
            .await
        {
            Ok(id) => id,
            Err(error) => {
                drop(response_tx.send(Err(Error::internal_error().data(error.to_string()))));
                return response_rx;
            }
        };

        self.submissions.insert(
            submission_id,
            SubmissionState::Skills(SkillsState::new(response_tx)),
        );

        response_rx
    }

    async fn refresh_skills(&mut self, force_reload: bool) {
        let load_skills = self.load_skills(force_reload).await;
        let resolution_tx = self.resolution_tx.clone();
        let cwd = self.config.cwd.clone();

        tokio::spawn(async move {
            let skills = match load_skills.await {
                Ok(Ok(entries)) => Some(skills_for_cwd(cwd.as_path(), &entries)),
                Ok(Err(error)) => {
                    error!("Failed to refresh skills: {error:?}");
                    None
                }
                Err(error) => {
                    error!("Failed to receive skills response: {error:?}");
                    None
                }
            };

            drop(resolution_tx.send(ThreadMessage::SkillsLoaded { skills }));
        });
    }

    fn resolve_skill_command(&self, name: &str) -> Option<SkillMetadata> {
        let skill_name = name.strip_prefix("skills:")?;
        self.skills
            .iter()
            .find(|skill| skill.name == skill_name)
            .cloned()
    }

    async fn modes(&self) -> Option<SessionModeState> {
        let approval_mode_id = current_session_mode_id(&self.config)?;
        let mut available_modes: Vec<SessionMode> = APPROVAL_PRESETS
            .iter()
            .map(|preset| SessionMode::new(preset.id, preset.label).description(preset.description))
            .collect();

        let collaboration_modes = self.models_manager.list_collaboration_modes().await;
        for mask in collaboration_modes {
            let Some(mode) = mask.mode else {
                continue;
            };
            if !mode.is_tui_visible() || mode == ModeKind::Default {
                continue;
            }
            let mode_id = mode_kind_as_id(mode);
            if available_modes
                .iter()
                .any(|available_mode: &SessionMode| available_mode.id.0.as_ref() == mode_id)
            {
                continue;
            }
            let mut session_mode = SessionMode::new(mode_id, mask.name);
            if let Some(description) = collaboration_mode_description(mode) {
                session_mode = session_mode.description(description);
            }
            available_modes.push(session_mode);
        }
        if !available_modes
            .iter()
            .any(|mode| mode.id.0.as_ref() == mode_kind_as_id(ModeKind::Plan))
        {
            available_modes.push(
                SessionMode::new(mode_kind_as_id(ModeKind::Plan), "Plan")
                    .description(PLAN_MODE_DESCRIPTION),
            );
        }

        let current_mode_id = if self.current_collaboration_mode_kind == ModeKind::Default {
            approval_mode_id
        } else {
            SessionModeId::new(mode_kind_as_id(self.current_collaboration_mode_kind))
        };

        Some(SessionModeState::new(current_mode_id, available_modes))
    }

    async fn find_current_model(&self) -> Option<ModelId> {
        let model_presets = self.models_manager.list_models().await;
        let config_model = self.get_current_model().await;
        let preset = model_presets
            .iter()
            .find(|preset| preset.model == config_model)?;

        let effort = self
            .config
            .model_reasoning_effort
            .and_then(|effort| {
                preset
                    .supported_reasoning_efforts
                    .iter()
                    .find_map(|e| (e.effort == effort).then_some(effort))
            })
            .unwrap_or(preset.default_reasoning_effort);

        Some(Self::model_id(&preset.id, effort))
    }

    fn model_id(id: &str, effort: ReasoningEffort) -> ModelId {
        ModelId::new(format!("{id}/{effort}"))
    }

    fn parse_model_id(id: &ModelId) -> Option<(String, ReasoningEffort)> {
        let (model, reasoning) = id.0.split_once('/')?;
        let reasoning = serde_json::from_value(reasoning.into()).ok()?;
        Some((model.to_owned(), reasoning))
    }

    fn current_service_tier_id(&self) -> &'static str {
        match self.config.service_tier {
            Some(ServiceTier::Fast) => "fast",
            Some(ServiceTier::Flex) => "flex",
            None => "standard",
        }
    }

    async fn config_options(&self) -> Result<Vec<SessionConfigOption>, Error> {
        let mut options = Vec::new();

        if let Some(modes) = self.modes().await {
            let select_options = modes
                .available_modes
                .into_iter()
                .map(|m| SessionConfigSelectOption::new(m.id.0, m.name).description(m.description))
                .collect::<Vec<_>>();

            options.push(
                SessionConfigOption::select(
                    "mode",
                    "Mode",
                    modes.current_mode_id.0,
                    select_options,
                )
                .category(SessionConfigOptionCategory::Mode)
                .description("Choose a session mode or approval preset"),
            );
        }

        let presets = self.models_manager.list_models().await;

        let current_model = self.get_current_model().await;
        let current_preset = presets.iter().find(|p| p.model == current_model).cloned();

        let mut model_select_options = Vec::new();

        if current_preset.is_none() {
            // If no preset found, return the current model string as-is
            model_select_options.push(SessionConfigSelectOption::new(
                current_model.clone(),
                current_model.clone(),
            ));
        };

        model_select_options.extend(
            presets
                .into_iter()
                .filter(|model| model.show_in_picker || model.model == current_model)
                .map(|preset| {
                    SessionConfigSelectOption::new(preset.id, preset.display_name)
                        .description(preset.description)
                }),
        );

        options.push(
            SessionConfigOption::select("model", "Model", current_model, model_select_options)
                .category(SessionConfigOptionCategory::Model)
                .description("Choose which model Codex should use"),
        );

        options.push(
            SessionConfigOption::select(
                "service_tier",
                "Service Tier",
                self.current_service_tier_id(),
                vec![
                    SessionConfigSelectOption::new("standard", "Standard")
                        .description("Use the default response tier"),
                    SessionConfigSelectOption::new("fast", "Fast")
                        .description("Prefer the fast response tier"),
                ],
            )
            .category(SessionConfigOptionCategory::Model)
            .description("Choose the response service tier for this session"),
        );

        // Reasoning effort selector (only if the current preset exists and has >1 supported effort)
        if let Some(preset) = current_preset
            && preset.supported_reasoning_efforts.len() > 1
        {
            let supported = &preset.supported_reasoning_efforts;

            let current_effort = self
                .config
                .model_reasoning_effort
                .and_then(|effort| {
                    supported
                        .iter()
                        .find_map(|e| (e.effort == effort).then_some(effort))
                })
                .unwrap_or(preset.default_reasoning_effort);

            let effort_select_options = supported
                .iter()
                .map(|e| {
                    SessionConfigSelectOption::new(
                        e.effort.to_string(),
                        e.effort.to_string().to_title_case(),
                    )
                    .description(e.description.clone())
                })
                .collect::<Vec<_>>();

            options.push(
                SessionConfigOption::select(
                    "reasoning_effort",
                    "Reasoning Effort",
                    current_effort.to_string(),
                    effort_select_options,
                )
                .category(SessionConfigOptionCategory::ThoughtLevel)
                .description("Choose how much reasoning effort the model should use"),
            );
        }

        Ok(options)
    }

    async fn maybe_emit_config_options_update(&mut self) {
        let config_options = self.config_options().await.unwrap_or_default();

        if self
            .last_sent_config_options
            .as_ref()
            .is_some_and(|prev| prev == &config_options)
        {
            return;
        }

        self.last_sent_config_options = Some(config_options.clone());

        self.client
            .send_notification(SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
                config_options,
            )));
    }

    async fn handle_set_config_option(
        &mut self,
        config_id: SessionConfigId,
        value: SessionConfigOptionValue,
    ) -> Result<(), Error> {
        let SessionConfigOptionValue::ValueId { value } = value else {
            return Err(Error::invalid_params().data("Unsupported config option value"));
        };
        match config_id.0.as_ref() {
            "mode" => self.handle_set_mode(SessionModeId::new(value.0)).await,
            "collaboration_mode" => self.handle_set_collaboration_mode(value).await,
            "model" => self.handle_set_config_model(value).await,
            "service_tier" => self.handle_set_config_service_tier(value).await,
            "reasoning_effort" => self.handle_set_config_reasoning_effort(value).await,
            _ => Err(Error::invalid_params().data("Unsupported config option")),
        }
    }

    async fn handle_set_collaboration_mode(
        &mut self,
        value: SessionConfigValueId,
    ) -> Result<(), Error> {
        let mode: ModeKind = serde_json::from_value(value.0.as_ref().into())
            .map_err(|_| Error::invalid_params().data("Unsupported collaboration mode"))?;
        let next_mode = self.collaboration_mode_for(mode).await?;

        self.thread
            .submit(Op::OverrideTurnContext {
                cwd: None,
                approval_policy: None,
                sandbox_policy: None,
                model: None,
                effort: None,
                summary: None,
                collaboration_mode: Some(next_mode.clone()),
                personality: None,
                windows_sandbox_level: None,
                service_tier: None,
                approvals_reviewer: None,
                permission_profile: None,
            })
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;

        self.apply_collaboration_mode(next_mode);

        Ok(())
    }

    async fn collaboration_mode_for(&self, mode: ModeKind) -> Result<CollaborationMode, Error> {
        if !mode.is_tui_visible() {
            return Err(Error::invalid_params().data("Unsupported collaboration mode"));
        }

        let masks = self.models_manager.list_collaboration_modes().await;
        let Some(mask) = masks.iter().find(|mask| mask.mode == Some(mode)) else {
            return Err(Error::invalid_params().data("Collaboration mode is unavailable"));
        };

        let current_mode = CollaborationMode {
            mode: self.current_collaboration_mode_kind,
            settings: Settings {
                model: self.get_current_model().await,
                reasoning_effort: self.config.model_reasoning_effort,
                developer_instructions: self.config.developer_instructions.clone(),
            },
        };

        Ok(current_mode.apply_mask(mask))
    }

    fn apply_collaboration_mode(&mut self, next_mode: CollaborationMode) {
        self.current_collaboration_mode_kind = next_mode.mode;
        self.config.model = Some(next_mode.settings.model);
        self.config.model_reasoning_effort = next_mode.settings.reasoning_effort;
        self.config.developer_instructions = next_mode.settings.developer_instructions;
    }

    async fn set_service_tier(&mut self, service_tier: Option<ServiceTier>) -> Result<(), Error> {
        self.thread
            .submit(Op::OverrideTurnContext {
                cwd: None,
                approval_policy: None,
                sandbox_policy: None,
                model: None,
                effort: None,
                summary: None,
                collaboration_mode: None,
                personality: None,
                windows_sandbox_level: None,
                service_tier: Some(service_tier),
                approvals_reviewer: None,
                permission_profile: None,
            })
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;

        self.config.service_tier = service_tier;
        persist_service_tier_default(&self.config, service_tier).await?;

        Ok(())
    }

    async fn format_session_status(&self) -> String {
        let model = self.get_current_model().await;
        let mode = self
            .modes()
            .await
            .and_then(|modes| {
                modes
                    .available_modes
                    .into_iter()
                    .find(|mode| mode.id == modes.current_mode_id)
                    .map(|mode| mode.name)
            })
            .unwrap_or_else(|| "unknown".to_string());
        let reasoning = self
            .config
            .model_reasoning_effort
            .map(|effort| effort.to_string())
            .unwrap_or_else(|| "default".to_string());

        format!(
            "## Session Status\n\n**Model:** {model}\n**Reasoning Effort:** {reasoning}\n**Mode:** {mode}\n**Service Tier:** {}\n**Working Directory:** {}\n",
            format_service_tier_name(self.config.service_tier),
            self.config.cwd.display()
        )
    }

    fn format_mcp_servers(&self) -> String {
        let servers = self.config.mcp_servers.get();
        if servers.is_empty() {
            return "No MCP servers configured.\n".to_string();
        }

        let mut output = String::from("## MCP Servers\n\n");
        for (name, server) in servers.iter().sorted_by_key(|(name, _)| name.as_str()) {
            let transport = match &server.transport {
                codex_config::McpServerTransportConfig::Stdio { command, .. } => {
                    format!("stdio: {command}")
                }
                codex_config::McpServerTransportConfig::StreamableHttp { url, .. } => {
                    format!("http: {url}")
                }
            };
            let state = if server.enabled {
                "enabled"
            } else {
                "disabled"
            };
            output.push_str(&format!("- `{name}` ({state}) - {transport}\n"));
        }
        output
    }

    async fn handle_set_config_service_tier(
        &mut self,
        value: SessionConfigValueId,
    ) -> Result<(), Error> {
        let service_tier = match value.0.as_ref() {
            "standard" => None,
            "fast" => Some(ServiceTier::Fast),
            _ => return Err(Error::invalid_params().data("Unsupported service tier")),
        };

        self.set_service_tier(service_tier).await
    }

    async fn handle_set_config_model(&mut self, value: SessionConfigValueId) -> Result<(), Error> {
        let model_id = value.0;

        let presets = self.models_manager.list_models().await;
        let preset = presets.iter().find(|p| p.id.as_str() == &*model_id);

        let model_to_use = preset
            .map(|p| p.model.clone())
            .unwrap_or_else(|| model_id.to_string());

        if model_to_use.is_empty() {
            return Err(Error::invalid_params().data("No model selected"));
        }

        let effort_to_use = if let Some(preset) = preset {
            if let Some(effort) = self.config.model_reasoning_effort
                && preset
                    .supported_reasoning_efforts
                    .iter()
                    .any(|e| e.effort == effort)
            {
                Some(effort)
            } else {
                Some(preset.default_reasoning_effort)
            }
        } else {
            // If the user selected a raw model string (not a known preset), don't invent a default.
            // Keep whatever was previously configured (or leave unset) so Codex can decide.
            self.config.model_reasoning_effort
        };

        self.thread
            .submit(Op::OverrideTurnContext {
                cwd: None,
                approval_policy: None,
                sandbox_policy: None,
                model: Some(model_to_use.clone()),
                effort: Some(effort_to_use),
                summary: None,
                collaboration_mode: None,
                personality: None,
                windows_sandbox_level: None,
                service_tier: None,
                approvals_reviewer: None,
                permission_profile: None,
            })
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;

        self.config.model = Some(model_to_use);
        self.config.model_reasoning_effort = effort_to_use;
        persist_model_default(
            &self.config,
            self.config.model.as_deref().unwrap_or_default(),
            self.config.model_reasoning_effort,
        )
        .await?;

        Ok(())
    }

    async fn handle_set_config_reasoning_effort(
        &mut self,
        value: SessionConfigValueId,
    ) -> Result<(), Error> {
        let effort: ReasoningEffort =
            serde_json::from_value(value.0.as_ref().into()).map_err(|_| Error::invalid_params())?;

        let current_model = self.get_current_model().await;
        let presets = self.models_manager.list_models().await;
        let Some(preset) = presets.iter().find(|p| p.model == current_model) else {
            return Err(Error::invalid_params()
                .data("Reasoning effort can only be set for known model presets"));
        };

        if !preset
            .supported_reasoning_efforts
            .iter()
            .any(|e| e.effort == effort)
        {
            return Err(
                Error::invalid_params().data("Unsupported reasoning effort for selected model")
            );
        }

        self.thread
            .submit(Op::OverrideTurnContext {
                cwd: None,
                approval_policy: None,
                sandbox_policy: None,
                model: None,
                effort: Some(Some(effort)),
                summary: None,
                collaboration_mode: None,
                personality: None,
                windows_sandbox_level: None,
                service_tier: None,
                approvals_reviewer: None,
                permission_profile: None,
            })
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;

        self.config.model_reasoning_effort = Some(effort);
        let current_model = self.get_current_model().await;
        persist_model_default(
            &self.config,
            &current_model,
            self.config.model_reasoning_effort,
        )
        .await?;

        Ok(())
    }

    async fn models(&self) -> Result<SessionModelState, Error> {
        let mut available_models = Vec::new();
        let config_model = self.get_current_model().await;

        let current_model_id = if let Some(model_id) = self.find_current_model().await {
            model_id
        } else {
            // If no preset found, return the current model string as-is
            let model_id = ModelId::new(self.get_current_model().await);
            available_models.push(ModelInfo::new(model_id.clone(), model_id.to_string()));
            model_id
        };

        available_models.extend(
            self.models_manager
                .list_models()
                .await
                .iter()
                .filter(|model| model.show_in_picker || model.model == config_model)
                .flat_map(|preset| {
                    preset.supported_reasoning_efforts.iter().map(|effort| {
                        ModelInfo::new(
                            Self::model_id(&preset.id, effort.effort),
                            format!("{} ({})", preset.display_name, effort.effort),
                        )
                        .description(format!("{} {}", preset.description, effort.description))
                    })
                }),
        );

        Ok(SessionModelState::new(current_model_id, available_models))
    }

    async fn handle_load(&mut self) -> Result<LoadSessionResponse, Error> {
        let response = LoadSessionResponse::new()
            .models(self.models().await?)
            .modes(self.modes().await)
            .config_options(self.config_options().await?);

        self.send_available_commands_update();
        self.maybe_emit_config_options_update().await;

        Ok(response)
    }

    async fn handle_prompt(
        &mut self,
        request: PromptRequest,
    ) -> Result<oneshot::Receiver<Result<StopReason, Error>>, Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let items = build_prompt_items(request.prompt);
        let op;
        if let Some((name, rest)) = extract_slash_command(&items) {
            if let Some(skill) = self.resolve_skill_command(name) {
                let mut skill_items = vec![UserInput::Skill {
                    name: skill.name,
                    path: skill.path.to_path_buf(),
                }];
                let instructions = rest.trim();
                if !instructions.is_empty() {
                    skill_items.push(UserInput::Text {
                        text: instructions.to_owned(),
                        text_elements: vec![],
                    });
                }
                op = Op::UserInput {
                    items: skill_items,
                    final_output_json_schema: None,
                    environments: None,
                    responsesapi_client_metadata: None,
                };
            } else {
                match name {
                    "compact" => op = Op::Compact,
                    "undo" => op = Op::Undo,
                    "init" => {
                        op = Op::UserInput {
                            items: vec![UserInput::Text {
                                text: INIT_COMMAND_PROMPT.into(),
                                text_elements: vec![],
                            }],
                            final_output_json_schema: None,
                            environments: None,
                            responsesapi_client_metadata: None,
                        }
                    }
                    "review" => {
                        let instructions = rest.trim();
                        let target = if instructions.is_empty() {
                            ReviewTarget::UncommittedChanges
                        } else {
                            ReviewTarget::Custom {
                                instructions: instructions.to_owned(),
                            }
                        };

                        op = Op::Review {
                            review_request: ReviewRequest {
                                user_facing_hint: Some(user_facing_hint(&target)),
                                target,
                            },
                        }
                    }
                    "review-branch" if !rest.is_empty() => {
                        let target = ReviewTarget::BaseBranch {
                            branch: rest.trim().to_owned(),
                        };
                        op = Op::Review {
                            review_request: ReviewRequest {
                                user_facing_hint: Some(user_facing_hint(&target)),
                                target,
                            },
                        }
                    }
                    "review-commit" if !rest.is_empty() => {
                        let target = ReviewTarget::Commit {
                            sha: rest.trim().to_owned(),
                            title: None,
                        };
                        op = Op::Review {
                            review_request: ReviewRequest {
                                user_facing_hint: Some(user_facing_hint(&target)),
                                target,
                            },
                        }
                    }
                    "logout" => {
                        self.auth.logout().await?;
                        return Err(Error::auth_required());
                    }
                    "fast" => {
                        let tier = match rest.trim().to_ascii_lowercase().as_str() {
                            "" => {
                                if matches!(self.config.service_tier, Some(ServiceTier::Fast)) {
                                    None
                                } else {
                                    Some(ServiceTier::Fast)
                                }
                            }
                            "on" => Some(ServiceTier::Fast),
                            "off" => None,
                            "status" => {
                                let status = if matches!(
                                    self.config.service_tier,
                                    Some(ServiceTier::Fast)
                                ) {
                                    "on"
                                } else {
                                    "off"
                                };
                                self.client
                                    .send_agent_text(format!("Fast mode is {status}.\n"));
                                response_tx.send(Ok(StopReason::EndTurn)).ok();
                                return Ok(response_rx);
                            }
                            _ => {
                                self.client
                                    .send_agent_text("Usage: /fast [on|off|status]\n".to_string());
                                response_tx.send(Ok(StopReason::EndTurn)).ok();
                                return Ok(response_rx);
                            }
                        };

                        self.set_service_tier(tier).await?;
                        self.maybe_emit_config_options_update().await;
                        let status = if matches!(tier, Some(ServiceTier::Fast)) {
                            "on"
                        } else {
                            "off"
                        };
                        self.client
                            .send_agent_text(format!("Fast mode is {status}.\n"));
                        response_tx.send(Ok(StopReason::EndTurn)).ok();
                        return Ok(response_rx);
                    }
                    "diff" => {
                        let output = run_git_diff(&self.config.cwd).await;
                        self.client.send_agent_text(output);
                        response_tx.send(Ok(StopReason::EndTurn)).ok();
                        return Ok(response_rx);
                    }
                    "status" => {
                        let status = self.format_session_status().await;
                        self.client.send_agent_text(status);
                        response_tx.send(Ok(StopReason::EndTurn)).ok();
                        return Ok(response_rx);
                    }
                    "stop" => op = Op::CleanBackgroundTerminals,
                    "mcp" => {
                        self.client.send_agent_text(self.format_mcp_servers());
                        response_tx.send(Ok(StopReason::EndTurn)).ok();
                        return Ok(response_rx);
                    }
                    "skills" => {
                        self.client.send_agent_text(format_skills(&self.skills));
                        response_tx.send(Ok(StopReason::EndTurn)).ok();
                        return Ok(response_rx);
                    }
                    "rename" if !rest.is_empty() => {
                        let name = normalize_thread_name(rest).ok_or_else(Error::invalid_params)?;
                        let submission_id = self
                            .thread
                            .submit(Op::SetThreadName { name })
                            .await
                            .map_err(|e| Error::internal_error().data(e.to_string()))?;
                        self.submissions.insert(
                            submission_id,
                            SubmissionState::Rename(RenameState::new(response_tx)),
                        );
                        return Ok(response_rx);
                    }
                    _ => {
                        op = Op::UserInput {
                            items,
                            final_output_json_schema: None,
                            environments: None,
                            responsesapi_client_metadata: None,
                        }
                    }
                }
            }
        } else {
            op = Op::UserInput {
                items,
                final_output_json_schema: None,
                environments: None,
                responsesapi_client_metadata: None,
            }
        }

        let submission_id = self
            .thread
            .submit(op.clone())
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;

        info!("Submitted prompt with submission_id: {submission_id}");
        info!("Starting to wait for conversation events for submission_id: {submission_id}");

        let state = SubmissionState::Prompt(PromptState::new(
            submission_id.clone(),
            self.thread.clone(),
            self.resolution_tx.clone(),
            response_tx,
        ));

        self.submissions.insert(submission_id, state);

        Ok(response_rx)
    }

    async fn submit_plan_implementation(
        &mut self,
        approval_preset_id: &str,
        mut response_tx: Option<oneshot::Sender<Result<StopReason, Error>>>,
    ) -> Result<(), Error> {
        let preset = match Self::approval_preset(approval_preset_id) {
            Ok(preset) => preset,
            Err(err) => {
                if let Some(response_tx) = response_tx.take() {
                    response_tx.send(Err(err.clone())).ok();
                }
                return Err(err);
            }
        };
        let collaboration_mode = match self.collaboration_mode_for(ModeKind::Default).await {
            Ok(collaboration_mode) => collaboration_mode,
            Err(err) => {
                if let Some(response_tx) = response_tx.take() {
                    response_tx.send(Err(err.clone())).ok();
                }
                return Err(err);
            }
        };
        let op = Op::UserInputWithTurnContext {
            items: vec![UserInput::Text {
                text: PLAN_IMPLEMENTATION_CODING_MESSAGE.to_string(),
                text_elements: vec![],
            }],
            environments: None,
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            cwd: None,
            approval_policy: Some(preset.approval),
            approvals_reviewer: None,
            sandbox_policy: None,
            permission_profile: Some(preset.permission_profile.clone()),
            active_permission_profile: active_profile_id_for_session_mode(preset.id)
                .map(ActivePermissionProfile::new),
            windows_sandbox_level: None,
            model: None,
            effort: None,
            summary: None,
            service_tier: None,
            collaboration_mode: Some(collaboration_mode.clone()),
            personality: None,
        };

        let submission_id = match self.thread.submit(op).await {
            Ok(submission_id) => submission_id,
            Err(error) => {
                let err = Error::internal_error().data(error.to_string());
                if let Some(response_tx) = response_tx.take() {
                    response_tx.send(Err(err.clone())).ok();
                }
                return Err(err);
            }
        };

        let response_tx = response_tx.unwrap_or_else(|| {
            let (response_tx, _response_rx) = oneshot::channel();
            response_tx
        });
        let state = SubmissionState::Prompt(PromptState::new(
            submission_id.clone(),
            self.thread.clone(),
            self.resolution_tx.clone(),
            response_tx,
        ));

        self.submissions.insert(submission_id, state);
        self.apply_collaboration_mode(collaboration_mode);
        self.apply_approval_preset(preset)?;
        persist_approval_preset_default(&self.config, preset).await?;

        Ok(())
    }

    async fn handle_set_mode(&mut self, mode: SessionModeId) -> Result<(), Error> {
        if APPROVAL_PRESETS
            .iter()
            .any(|preset| mode.0.as_ref() == preset.id)
        {
            return self.handle_set_approval_preset(mode).await;
        }

        self.handle_set_collaboration_mode(SessionConfigValueId::new(mode.0))
            .await
    }

    async fn handle_set_approval_preset(&mut self, mode: SessionModeId) -> Result<(), Error> {
        let preset = Self::approval_preset(mode.0.as_ref())?;
        let collaboration_mode = self.collaboration_mode_for(ModeKind::Default).await?;

        self.thread
            .submit(Op::OverrideTurnContext {
                cwd: None,
                approval_policy: Some(preset.approval),
                permission_profile: Some(preset.permission_profile.clone()),
                sandbox_policy: None,
                model: None,
                effort: None,
                summary: None,
                collaboration_mode: Some(collaboration_mode.clone()),
                personality: None,
                windows_sandbox_level: None,
                service_tier: None,
                approvals_reviewer: None,
            })
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;

        self.apply_collaboration_mode(collaboration_mode);
        self.apply_approval_preset(preset)?;
        persist_approval_preset_default(&self.config, preset).await?;

        Ok(())
    }

    fn approval_preset(mode: &str) -> Result<&'static ApprovalPreset, Error> {
        APPROVAL_PRESETS
            .iter()
            .find(|preset| mode == preset.id)
            .ok_or_else(Error::invalid_params)
    }

    fn apply_approval_preset(&mut self, preset: &ApprovalPreset) -> Result<(), Error> {
        self.config
            .permissions
            .approval_policy
            .set(preset.approval)
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
        self.config
            .permissions
            .set_permission_profile_with_active_profile(
                preset.permission_profile.clone(),
                active_profile_id_for_session_mode(preset.id).map(ActivePermissionProfile::new),
            )
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;

        if mode_trusts_project(preset.id) {
            set_project_trust_level(
                &self.config.codex_home,
                &self.config.cwd,
                TrustLevel::Trusted,
            )?;
        }

        Ok(())
    }

    async fn get_current_model(&self) -> String {
        self.models_manager.get_model(&self.config.model).await
    }

    async fn handle_set_model(&mut self, model: ModelId) -> Result<(), Error> {
        // Try parsing as preset format, otherwise use as-is, fallback to config
        let (model_to_use, effort_to_use) = if let Some((m, e)) = Self::parse_model_id(&model) {
            (m, Some(e))
        } else {
            let model_str = model.0.to_string();
            let fallback = if !model_str.is_empty() {
                model_str
            } else {
                self.get_current_model().await
            };
            (fallback, self.config.model_reasoning_effort)
        };

        if model_to_use.is_empty() {
            return Err(Error::invalid_params().data("No model parsed or configured"));
        }

        self.thread
            .submit(Op::OverrideTurnContext {
                cwd: None,
                approval_policy: None,
                sandbox_policy: None,
                model: Some(model_to_use.clone()),
                effort: Some(effort_to_use),
                summary: None,
                collaboration_mode: None,
                personality: None,
                windows_sandbox_level: None,
                service_tier: None,
                approvals_reviewer: None,
                permission_profile: None,
            })
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;

        self.config.model = Some(model_to_use);
        self.config.model_reasoning_effort = effort_to_use;

        Ok(())
    }

    async fn handle_cancel(&mut self) -> Result<(), Error> {
        self.abort_pending_interactions();
        self.thread
            .submit(Op::Interrupt)
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
        Ok(())
    }

    async fn handle_shutdown(&mut self) -> Result<(), Error> {
        self.abort_pending_interactions();
        self.thread
            .submit(Op::Shutdown)
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
        Ok(())
    }

    fn abort_pending_interactions(&mut self) {
        for submission in self.submissions.values_mut() {
            submission.abort_pending_interactions();
        }
    }

    /// Replay conversation history to the client via session/update notifications.
    /// This is called when loading a session to stream all prior messages.
    ///
    /// We process both `EventMsg` and `ResponseItem`:
    /// - `EventMsg` for user/agent messages and reasoning (like the TUI does)
    /// - `ResponseItem` for tool calls only (not persisted as EventMsg)
    fn handle_replay_history(&mut self, history: Vec<RolloutItem>) -> Result<(), Error> {
        for item in history {
            match item {
                RolloutItem::EventMsg(event_msg) => {
                    self.replay_event_msg(&event_msg);
                }
                RolloutItem::ResponseItem(response_item) => {
                    self.replay_response_item(&response_item);
                }
                // Skip SessionMeta, TurnContext, Compacted
                _ => {}
            }
        }
        Ok(())
    }

    /// Convert and send an EventMsg as ACP notification(s) during replay.
    /// Handles messages and reasoning - mirrors the live event handling in PromptState.
    fn replay_event_msg(&self, msg: &EventMsg) {
        match msg {
            EventMsg::UserMessage(UserMessageEvent { message, .. }) => {
                self.client.send_user_message(message.clone());
            }
            EventMsg::AgentMessage(AgentMessageEvent {
                message,
                phase: _,
                memory_citation: _,
            }) => {
                self.client.send_agent_text(message.clone());
            }
            EventMsg::AgentReasoning(AgentReasoningEvent { text }) => {
                self.client.send_agent_thought(text.clone());
            }
            EventMsg::AgentReasoningRawContent(AgentReasoningRawContentEvent { text }) => {
                self.client.send_agent_thought(text.clone());
            }
            EventMsg::SessionConfigured(event) => {
                send_session_title_update(&self.client, event.thread_name.clone());
            }
            EventMsg::ThreadNameUpdated(event) => {
                send_session_title_update(&self.client, event.thread_name.clone());
            }
            EventMsg::ThreadGoalUpdated(event) => {
                self.client
                    .send_agent_text(format_thread_goal_update(event));
            }
            // Skip other event types during replay - they either:
            // - Are transient (deltas, turn lifecycle)
            // - Don't have direct ACP equivalents
            // - Are handled via ResponseItem instead
            _ => {}
        }
    }

    /// Parse apply_patch call input to extract patch content for display.
    /// Returns (title, locations, content) if successful.
    /// For CustomToolCall, the input is the patch string directly.
    fn parse_apply_patch_call(
        &self,
        input: &str,
    ) -> Option<(String, Vec<ToolCallLocation>, Vec<ToolCallContent>)> {
        // Try to parse the patch using codex-apply-patch parser
        let parsed = parse_patch(input).ok()?;

        let mut locations = Vec::new();
        let mut file_names = Vec::new();
        let mut content = Vec::new();

        for hunk in &parsed.hunks {
            match hunk {
                codex_apply_patch::Hunk::AddFile { path, contents } => {
                    let full_path = self.config.cwd.as_path().join(path);
                    file_names.push(path.display().to_string());
                    locations.push(ToolCallLocation::new(full_path.clone()));
                    // New file: no old_text, new_text is the contents
                    content.push(ToolCallContent::Diff(Diff::new(
                        full_path,
                        contents.clone(),
                    )));
                }
                codex_apply_patch::Hunk::DeleteFile { path } => {
                    let full_path = self.config.cwd.as_path().join(path);
                    file_names.push(path.display().to_string());
                    locations.push(ToolCallLocation::new(full_path.clone()));
                    // Delete file: old_text would be original content, new_text is empty
                    content.push(ToolCallContent::Diff(
                        Diff::new(full_path, "").old_text("[file deleted]"),
                    ));
                }
                codex_apply_patch::Hunk::UpdateFile {
                    path,
                    move_path,
                    chunks,
                } => {
                    let full_path = self.config.cwd.as_path().join(path);
                    let dest_path = move_path
                        .as_ref()
                        .map(|p| self.config.cwd.as_path().join(p))
                        .unwrap_or_else(|| full_path.clone());
                    file_names.push(path.display().to_string());
                    locations.push(ToolCallLocation::new(dest_path.clone()));

                    // Build old and new text from chunks
                    let old_lines: Vec<String> = chunks
                        .iter()
                        .flat_map(|c| c.old_lines.iter().cloned())
                        .collect();
                    let new_lines: Vec<String> = chunks
                        .iter()
                        .flat_map(|c| c.new_lines.iter().cloned())
                        .collect();

                    content.push(ToolCallContent::Diff(
                        Diff::new(dest_path, new_lines.join("\n")).old_text(old_lines.join("\n")),
                    ));
                }
            }
        }

        let title = if file_names.is_empty() {
            "Apply patch".to_string()
        } else {
            format!("Edit {}", file_names.join(", "))
        };

        Some((title, locations, content))
    }

    /// Parse shell function call arguments to extract command info for rich display.
    /// Returns (title, kind, locations) if successful.
    ///
    /// Handles both:
    /// - `shell` / `container.exec`: `command` is `Vec<String>`
    /// - `shell_command`: `command` is a `String` (shell script)
    fn parse_shell_function_call(
        &self,
        name: &str,
        arguments: &str,
    ) -> Option<(String, ToolKind, Vec<ToolCallLocation>)> {
        // Extract command and workdir based on tool type
        let (command_vec, workdir): (Vec<String>, Option<String>) = if name == "shell_command" {
            // shell_command: command is a string (shell script)
            #[derive(serde::Deserialize)]
            struct ShellCommandArgs {
                command: String,
                #[serde(default)]
                workdir: Option<String>,
            }
            let args: ShellCommandArgs = serde_json::from_str(arguments).ok()?;
            // Wrap in bash -lc for parsing
            (
                vec!["bash".to_string(), "-lc".to_string(), args.command],
                args.workdir,
            )
        } else {
            // shell / container.exec: command is Vec<String>
            #[derive(serde::Deserialize)]
            struct ShellArgs {
                command: Vec<String>,
                #[serde(default)]
                workdir: Option<String>,
            }
            let args: ShellArgs = serde_json::from_str(arguments).ok()?;
            (args.command, args.workdir)
        };

        let cwd = workdir
            .map(PathBuf::from)
            .unwrap_or_else(|| self.config.cwd.clone().into());

        let parsed_cmd = parse_command(&command_vec);
        let ParseCommandToolCall {
            title,
            file_extension: _,
            terminal_output: _,
            locations,
            kind,
        } = parse_command_tool_call(parsed_cmd, &cwd);

        Some((title, kind, locations))
    }

    /// Convert and send a single ResponseItem as ACP notification(s) during replay.
    /// Only handles tool calls - messages/reasoning are handled via EventMsg.
    fn replay_response_item(&self, item: &ResponseItem) {
        match item {
            // Skip Message and Reasoning - these are handled via EventMsg
            ResponseItem::Message { .. } | ResponseItem::Reasoning { .. } => {}
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                // Check if this is a shell command - parse it like we do for LocalShellCall
                if matches!(name.as_str(), "shell" | "container.exec" | "shell_command")
                    && let Some((title, kind, locations)) =
                        self.parse_shell_function_call(name, arguments)
                {
                    self.client.send_tool_call(
                        ToolCall::new(call_id.clone(), title)
                            .kind(kind)
                            .status(ToolCallStatus::Completed)
                            .locations(locations)
                            .raw_input(serde_json::from_str::<serde_json::Value>(arguments).ok()),
                    );
                    return;
                }

                // Fall through to generic function call handling
                self.client.send_completed_tool_call(
                    call_id.clone(),
                    name.clone(),
                    ToolKind::Other,
                    serde_json::from_str(arguments).ok(),
                );
            }
            ResponseItem::FunctionCallOutput { call_id, output } => {
                self.client
                    .send_tool_call_completed(call_id.clone(), serde_json::to_value(output).ok());
            }
            ResponseItem::LocalShellCall {
                call_id: Some(call_id),
                action,
                status,
                ..
            } => {
                let codex_protocol::models::LocalShellAction::Exec(exec) = action;
                let cwd = exec
                    .working_directory
                    .as_ref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.config.cwd.clone().into());

                // Parse the command to get rich info like the live event handler does
                let parsed_cmd = parse_command(&exec.command);
                let ParseCommandToolCall {
                    title,
                    file_extension: _,
                    terminal_output: _,
                    locations,
                    kind,
                } = parse_command_tool_call(parsed_cmd, &cwd);

                let tool_status = match status {
                    codex_protocol::models::LocalShellStatus::Completed => {
                        ToolCallStatus::Completed
                    }
                    codex_protocol::models::LocalShellStatus::InProgress
                    | codex_protocol::models::LocalShellStatus::Incomplete => {
                        ToolCallStatus::Failed
                    }
                };
                self.client.send_tool_call(
                    ToolCall::new(call_id.clone(), title)
                        .kind(kind)
                        .status(tool_status)
                        .locations(locations),
                );
            }
            ResponseItem::CustomToolCall {
                name,
                input,
                call_id,
                ..
            } => {
                // Check if this is an apply_patch call - show the patch content
                if name == "apply_patch"
                    && let Some((title, locations, content)) = self.parse_apply_patch_call(input)
                {
                    self.client.send_tool_call(
                        ToolCall::new(call_id.clone(), title)
                            .kind(ToolKind::Edit)
                            .status(ToolCallStatus::Completed)
                            .locations(locations)
                            .content(content)
                            .raw_input(serde_json::from_str::<serde_json::Value>(input).ok()),
                    );
                    return;
                }

                // Fall through to generic custom tool call handling
                self.client.send_completed_tool_call(
                    call_id.clone(),
                    name.clone(),
                    ToolKind::Other,
                    serde_json::from_str(input).ok(),
                );
            }
            ResponseItem::CustomToolCallOutput {
                name: _,
                call_id,
                output,
            } => {
                self.client
                    .send_tool_call_completed(call_id.clone(), Some(serde_json::json!(output)));
            }
            ResponseItem::WebSearchCall { id, action, .. } => {
                let (title, call_id) = if let Some(action) = action {
                    web_search_action_to_title_and_id(id, action)
                } else {
                    ("Web Search".into(), generate_fallback_id("web_search"))
                };
                self.client.send_tool_call(
                    ToolCall::new(call_id, title)
                        .kind(ToolKind::Search)
                        .status(ToolCallStatus::Completed),
                );
            }
            ResponseItem::ToolSearchCall {
                call_id,
                status,
                execution,
                arguments,
                ..
            } => {
                self.client.send_tool_call(
                    ToolCall::new(
                        call_id
                            .clone()
                            .unwrap_or_else(|| generate_fallback_id("tool_search")),
                        format!("Search tools via {execution}"),
                    )
                    .kind(ToolKind::Search)
                    .status(
                        status
                            .as_deref()
                            .map(response_item_status_to_tool_status)
                            .unwrap_or(ToolCallStatus::Completed),
                    )
                    .raw_input(arguments.clone()),
                );
            }
            ResponseItem::ToolSearchOutput {
                call_id: Some(call_id),
                status,
                execution,
                tools,
            } => {
                self.client.send_tool_call_update(ToolCallUpdate::new(
                    call_id.clone(),
                    ToolCallUpdateFields::new()
                        .status(response_item_status_to_tool_status(status))
                        .title(format!("Tool search completed via {execution}"))
                        .raw_output(serde_json::json!({
                            "status": status,
                            "execution": execution,
                            "tools": tools,
                        })),
                ));
            }
            ResponseItem::ImageGenerationCall {
                id,
                status,
                revised_prompt,
                result,
            } => {
                let content = revised_prompt.as_ref().map(|prompt| {
                    vec![ToolCallContent::Content(Content::new(ContentBlock::Text(
                        TextContent::new(prompt.clone()),
                    )))]
                });
                let mut tool_call = ToolCall::new(id.clone(), "Generate image")
                    .kind(ToolKind::Other)
                    .status(response_item_status_to_tool_status(status))
                    .raw_output(serde_json::json!({
                        "status": status,
                        "revised_prompt": revised_prompt,
                        "result": result,
                    }));
                if let Some(content) = content {
                    tool_call = tool_call.content(content);
                }
                self.client.send_tool_call(tool_call);
            }
            // Skip GhostSnapshot, Compaction, Other, LocalShellCall without call_id
            _ => {}
        }
    }

    async fn handle_event(&mut self, Event { id, msg }: Event) {
        if matches!(msg, EventMsg::SkillsUpdateAvailable) {
            self.refresh_skills(true).await;
            return;
        }

        if let EventMsg::TurnStarted(TurnStartedEvent {
            collaboration_mode_kind,
            ..
        }) = &msg
        {
            self.current_collaboration_mode_kind = *collaboration_mode_kind;
        }

        if let Some(submission) = self.submissions.get_mut(&id) {
            submission.handle_event(&self.client, msg).await;
        } else if matches!(
            &msg,
            EventMsg::SessionConfigured(..) | EventMsg::ThreadNameUpdated(..)
        ) {
            match msg {
                EventMsg::SessionConfigured(event) => {
                    send_session_title_update(&self.client, event.thread_name);
                }
                EventMsg::ThreadNameUpdated(event) => {
                    send_session_title_update(&self.client, event.thread_name);
                }
                _ => unreachable!(),
            }
        } else {
            warn!("Received event for unknown submission ID: {id} {msg:?}");
        }
    }
}

fn send_session_title_update(client: &SessionClient, title: Option<String>) {
    if let Some(title) = title {
        client.send_notification(SessionUpdate::SessionInfoUpdate(
            SessionInfoUpdate::new().title(title),
        ));
    }
}

fn build_prompt_items(prompt: Vec<ContentBlock>) -> Vec<UserInput> {
    prompt
        .into_iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text_block) => Some(UserInput::Text {
                text: text_block.text,
                text_elements: vec![],
            }),
            ContentBlock::Image(image_block) => Some(UserInput::Image {
                image_url: format!("data:{};base64,{}", image_block.mime_type, image_block.data),
            }),
            ContentBlock::ResourceLink(ResourceLink { name, uri, .. }) => Some(UserInput::Text {
                text: format_uri_as_link(Some(name), uri),
                text_elements: vec![],
            }),
            ContentBlock::Resource(EmbeddedResource {
                resource:
                    EmbeddedResourceResource::TextResourceContents(TextResourceContents {
                        text,
                        uri,
                        ..
                    }),
                ..
            }) => Some(UserInput::Text {
                text: format!(
                    "{}\n<context ref=\"{uri}\">\n{text}\n</context>",
                    format_uri_as_link(None, uri.clone())
                ),
                text_elements: vec![],
            }),
            // Skip other content types for now
            ContentBlock::Audio(..) | ContentBlock::Resource(..) | _ => None,
        })
        .collect()
}

fn format_uri_as_link(name: Option<String>, uri: String) -> String {
    if let Some(name) = name
        && !name.is_empty()
    {
        format!("[@{name}]({uri})")
    } else if let Some(path) = uri.strip_prefix("file://") {
        let name = path.split('/').next_back().unwrap_or(path);
        format!("[@{name}]({uri})")
    } else if uri.starts_with("zed://") {
        let name = uri.split('/').next_back().unwrap_or(&uri);
        format!("[@{name}]({uri})")
    } else {
        uri
    }
}

#[cfg(test)]
mod tests;
