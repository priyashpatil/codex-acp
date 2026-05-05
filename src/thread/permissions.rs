use super::*;

#[derive(Clone)]
pub(super) struct ResolvedMcpElicitation {
    pub(super) action: ElicitationAction,
    pub(super) content: Option<serde_json::Value>,
    pub(super) meta: Option<serde_json::Value>,
}

impl ResolvedMcpElicitation {
    pub(super) fn accept() -> Self {
        Self {
            action: ElicitationAction::Accept,
            content: None,
            meta: None,
        }
    }

    fn accept_with_persist(persist: &'static str) -> Self {
        Self {
            action: ElicitationAction::Accept,
            content: None,
            meta: Some(serde_json::json!({ "persist": persist })),
        }
    }

    pub(super) fn cancel() -> Self {
        Self {
            action: ElicitationAction::Cancel,
            content: None,
            meta: None,
        }
    }
}

pub(super) const MCP_TOOL_APPROVAL_ALLOW_OPTION_ID: &str = "approved";
pub(super) const MCP_TOOL_APPROVAL_CANCEL_OPTION_ID: &str = "cancel";

const MCP_TOOL_APPROVAL_KIND_KEY: &str = "codex_approval_kind";
const MCP_TOOL_APPROVAL_KIND_MCP_TOOL_CALL: &str = "mcp_tool_call";
const MCP_TOOL_APPROVAL_PERSIST_KEY: &str = "persist";
pub(super) const MCP_TOOL_APPROVAL_PERSIST_SESSION: &str = "session";
const MCP_TOOL_APPROVAL_PERSIST_ALWAYS: &str = "always";
const MCP_TOOL_APPROVAL_TOOL_TITLE_KEY: &str = "tool_title";
const MCP_TOOL_APPROVAL_TOOL_DESCRIPTION_KEY: &str = "tool_description";
const MCP_TOOL_APPROVAL_CONNECTOR_NAME_KEY: &str = "connector_name";
const MCP_TOOL_APPROVAL_CONNECTOR_DESCRIPTION_KEY: &str = "connector_description";
const MCP_TOOL_APPROVAL_TOOL_PARAMS_KEY: &str = "tool_params";
const MCP_TOOL_APPROVAL_TOOL_PARAMS_DISPLAY_KEY: &str = "tool_params_display";
pub(super) const MCP_TOOL_APPROVAL_REQUEST_ID_PREFIX: &str = "mcp_tool_call_approval_";
pub(super) const MCP_TOOL_APPROVAL_ALLOW_SESSION_OPTION_ID: &str = "approved-for-session";
pub(super) const MCP_TOOL_APPROVAL_ALLOW_ALWAYS_OPTION_ID: &str = "approved-always";

pub(super) struct SupportedMcpElicitationPermissionRequest {
    pub(super) request_key: String,
    pub(super) tool_call: ToolCallUpdate,
    pub(super) options: Vec<PermissionOption>,
    pub(super) option_map: HashMap<String, ResolvedMcpElicitation>,
}

pub(super) fn build_supported_mcp_elicitation_permission_request(
    server_name: &str,
    request_id: &codex_protocol::mcp::RequestId,
    request: &ElicitationRequest,
    raw_input: serde_json::Value,
) -> Option<SupportedMcpElicitationPermissionRequest> {
    let ElicitationRequest::Form {
        meta: Some(meta),
        message,
        requested_schema: _,
    } = request
    else {
        return None;
    };
    let meta = meta.as_object()?;
    if meta
        .get(MCP_TOOL_APPROVAL_KIND_KEY)
        .and_then(serde_json::Value::as_str)
        != Some(MCP_TOOL_APPROVAL_KIND_MCP_TOOL_CALL)
    {
        return None;
    }

    let (allow_session_remember, allow_persistent_approval) = mcp_tool_approval_persist_modes(meta);
    let mut options = vec![PermissionOption::new(
        MCP_TOOL_APPROVAL_ALLOW_OPTION_ID,
        "Allow",
        PermissionOptionKind::AllowOnce,
    )];
    let mut option_map = HashMap::from([(
        MCP_TOOL_APPROVAL_ALLOW_OPTION_ID.to_string(),
        ResolvedMcpElicitation::accept(),
    )]);

    if allow_session_remember {
        options.push(PermissionOption::new(
            MCP_TOOL_APPROVAL_ALLOW_SESSION_OPTION_ID,
            "Allow for this session",
            PermissionOptionKind::AllowAlways,
        ));
        option_map.insert(
            MCP_TOOL_APPROVAL_ALLOW_SESSION_OPTION_ID.to_string(),
            ResolvedMcpElicitation::accept_with_persist(MCP_TOOL_APPROVAL_PERSIST_SESSION),
        );
    }

    if allow_persistent_approval {
        options.push(PermissionOption::new(
            MCP_TOOL_APPROVAL_ALLOW_ALWAYS_OPTION_ID,
            "Allow and don't ask again",
            PermissionOptionKind::AllowAlways,
        ));
        option_map.insert(
            MCP_TOOL_APPROVAL_ALLOW_ALWAYS_OPTION_ID.to_string(),
            ResolvedMcpElicitation::accept_with_persist(MCP_TOOL_APPROVAL_PERSIST_ALWAYS),
        );
    }

    options.push(PermissionOption::new(
        MCP_TOOL_APPROVAL_CANCEL_OPTION_ID,
        "Cancel",
        PermissionOptionKind::RejectOnce,
    ));
    option_map.insert(
        MCP_TOOL_APPROVAL_CANCEL_OPTION_ID.to_string(),
        ResolvedMcpElicitation::cancel(),
    );

    let tool_call_id = mcp_tool_approval_call_id(request_id)
        .unwrap_or_else(|| format!("mcp-elicitation:{request_id}"));
    let title = meta
        .get(MCP_TOOL_APPROVAL_TOOL_TITLE_KEY)
        .and_then(serde_json::Value::as_str)
        .filter(|title| !title.trim().is_empty())
        .map(|title| format!("Approve {title}"))
        .unwrap_or_else(|| "Approve MCP tool call".to_string());
    let content = format_mcp_tool_approval_content(server_name, message, meta);

    Some(SupportedMcpElicitationPermissionRequest {
        request_key: mcp_elicitation_request_key(server_name, request_id),
        tool_call: ToolCallUpdate::new(
            ToolCallId::new(tool_call_id),
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Pending)
                .title(title)
                .content(vec![ToolCallContent::Content(Content::new(
                    ContentBlock::Text(TextContent::new(content)),
                ))])
                .raw_input(raw_input),
        ),
        options,
        option_map,
    })
}

pub(super) fn mcp_elicitation_request_key(
    server_name: &str,
    request_id: &codex_protocol::mcp::RequestId,
) -> String {
    format!("mcp-elicitation:{server_name}:{request_id}")
}

fn mcp_tool_approval_persist_modes(
    meta: &serde_json::Map<String, serde_json::Value>,
) -> (bool, bool) {
    match meta.get(MCP_TOOL_APPROVAL_PERSIST_KEY) {
        Some(serde_json::Value::String(persist)) => (
            persist == MCP_TOOL_APPROVAL_PERSIST_SESSION,
            persist == MCP_TOOL_APPROVAL_PERSIST_ALWAYS,
        ),
        Some(serde_json::Value::Array(values)) => (
            values
                .iter()
                .any(|value| value.as_str() == Some(MCP_TOOL_APPROVAL_PERSIST_SESSION)),
            values
                .iter()
                .any(|value| value.as_str() == Some(MCP_TOOL_APPROVAL_PERSIST_ALWAYS)),
        ),
        _ => (false, false),
    }
}

fn mcp_tool_approval_call_id(request_id: &codex_protocol::mcp::RequestId) -> Option<String> {
    match request_id {
        codex_protocol::mcp::RequestId::String(value) => value
            .strip_prefix(MCP_TOOL_APPROVAL_REQUEST_ID_PREFIX)
            .map(ToString::to_string),
        codex_protocol::mcp::RequestId::Integer(_) => None,
    }
}

fn format_mcp_tool_approval_content(
    server_name: &str,
    message: &str,
    meta: &serde_json::Map<String, serde_json::Value>,
) -> String {
    let mut sections = vec![message.trim().to_string()];

    let source = meta
        .get(MCP_TOOL_APPROVAL_CONNECTOR_NAME_KEY)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!("Source: {value}"))
        .unwrap_or_else(|| format!("Server: {server_name}"));
    sections.push(source);

    if let Some(description) = meta
        .get(MCP_TOOL_APPROVAL_CONNECTOR_DESCRIPTION_KEY)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        sections.push(description.to_string());
    }

    if let Some(description) = meta
        .get(MCP_TOOL_APPROVAL_TOOL_DESCRIPTION_KEY)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        sections.push(description.to_string());
    }

    if let Some(params) = format_mcp_tool_approval_params(meta) {
        sections.push(format!("Arguments:\n{params}"));
    }

    sections.join("\n\n")
}

fn format_mcp_tool_approval_params(
    meta: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    if let Some(serde_json::Value::Array(params)) =
        meta.get(MCP_TOOL_APPROVAL_TOOL_PARAMS_DISPLAY_KEY)
    {
        let params = params
            .iter()
            .filter_map(|param| {
                let object = param.as_object()?;
                let name = object
                    .get("display_name")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| object.get("name").and_then(serde_json::Value::as_str))?;
                let value = object.get("value")?;
                Some(format!(
                    "- {name}: {}",
                    format_mcp_tool_approval_value(value)
                ))
            })
            .collect::<Vec<_>>();
        if !params.is_empty() {
            return Some(params.join("\n"));
        }
    }

    meta.get(MCP_TOOL_APPROVAL_TOOL_PARAMS_KEY).map(|params| {
        serde_json::to_string_pretty(params)
            .unwrap_or_else(|_| format_mcp_tool_approval_value(params))
    })
}

fn format_mcp_tool_approval_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        _ => serde_json::to_string(value).unwrap_or_else(|_| value.to_string()),
    }
}

#[derive(Clone)]
pub(super) struct ExecPermissionOption {
    pub(super) option_id: &'static str,
    pub(super) permission_option: PermissionOption,
    pub(super) decision: ReviewDecision,
}

pub(super) fn build_exec_permission_options(
    available_decisions: &[ReviewDecision],
    network_approval_context: Option<&NetworkApprovalContext>,
    additional_permissions: Option<&AdditionalPermissionProfile>,
) -> Vec<ExecPermissionOption> {
    available_decisions
        .iter()
        .map(|decision| match decision {
            ReviewDecision::Approved => ExecPermissionOption {
                option_id: "approved",
                permission_option: PermissionOption::new(
                    "approved",
                    if network_approval_context.is_some() {
                        "Yes, just this once"
                    } else {
                        "Yes, proceed"
                    },
                    PermissionOptionKind::AllowOnce,
                ),
                decision: ReviewDecision::Approved,
            },
            ReviewDecision::ApprovedExecpolicyAmendment {
                proposed_execpolicy_amendment,
            } => {
                let command_prefix = proposed_execpolicy_amendment.command().join(" ");
                let label = if command_prefix.contains('\n')
                    || command_prefix.contains('\r')
                    || command_prefix.is_empty()
                {
                    "Yes, and remember this command pattern".to_string()
                } else {
                    format!(
                        "Yes, and don't ask again for commands that start with `{command_prefix}`"
                    )
                };
                ExecPermissionOption {
                    option_id: "approved-execpolicy-amendment",
                    permission_option: PermissionOption::new(
                        "approved-execpolicy-amendment",
                        label,
                        PermissionOptionKind::AllowAlways,
                    ),
                    decision: ReviewDecision::ApprovedExecpolicyAmendment {
                        proposed_execpolicy_amendment: proposed_execpolicy_amendment.clone(),
                    },
                }
            }
            ReviewDecision::ApprovedForSession => ExecPermissionOption {
                option_id: "approved-for-session",
                permission_option: PermissionOption::new(
                    "approved-for-session",
                    if network_approval_context.is_some() {
                        "Yes, and allow this host for this session"
                    } else if additional_permissions.is_some() {
                        "Yes, and allow these permissions for this session"
                    } else {
                        "Yes, and don't ask again for this command in this session"
                    },
                    PermissionOptionKind::AllowAlways,
                ),
                decision: ReviewDecision::ApprovedForSession,
            },
            ReviewDecision::NetworkPolicyAmendment {
                network_policy_amendment,
            } => {
                let (option_id, label, kind) = match network_policy_amendment.action {
                    NetworkPolicyRuleAction::Allow => (
                        "network-policy-amendment-allow",
                        "Yes, and allow this host in the future",
                        PermissionOptionKind::AllowAlways,
                    ),
                    NetworkPolicyRuleAction::Deny => (
                        "network-policy-amendment-deny",
                        "No, and block this host in the future",
                        PermissionOptionKind::RejectAlways,
                    ),
                };
                ExecPermissionOption {
                    option_id,
                    permission_option: PermissionOption::new(option_id, label, kind),
                    decision: ReviewDecision::NetworkPolicyAmendment {
                        network_policy_amendment: network_policy_amendment.clone(),
                    },
                }
            }
            ReviewDecision::Denied => ExecPermissionOption {
                option_id: "denied",
                permission_option: PermissionOption::new(
                    "denied",
                    "No, continue without running it",
                    PermissionOptionKind::RejectOnce,
                ),
                decision: ReviewDecision::Denied,
            },
            ReviewDecision::Abort => ExecPermissionOption {
                option_id: "abort",
                permission_option: PermissionOption::new(
                    "abort",
                    "No, and tell Codex what to do differently",
                    PermissionOptionKind::RejectOnce,
                ),
                decision: ReviewDecision::Abort,
            },
            ReviewDecision::TimedOut => ExecPermissionOption {
                option_id: "timed_out",
                permission_option: PermissionOption::new(
                    "timed_out",
                    "Time out, tell Codex what to do differently",
                    PermissionOptionKind::RejectOnce,
                ),
                decision: ReviewDecision::TimedOut,
            },
        })
        .collect()
}

pub(super) fn build_user_input_permission_options(
    question: &RequestUserInputQuestion,
) -> (Vec<PermissionOption>, HashMap<String, String>) {
    let mut option_map = HashMap::new();
    let mut options = Vec::new();

    if let Some(question_options) = question.options.as_ref() {
        for (index, option) in question_options.iter().enumerate() {
            let option_id = format!("answer-{index}");
            option_map.insert(option_id.clone(), option.label.clone());
            options.push(PermissionOption::new(
                option_id,
                option.label.clone(),
                PermissionOptionKind::AllowOnce,
            ));
        }
    }

    if question.is_other {
        let option_id = "answer-other".to_string();
        option_map.insert(option_id.clone(), "other".to_string());
        options.push(PermissionOption::new(
            option_id,
            "Other",
            PermissionOptionKind::AllowOnce,
        ));
    }

    if options.is_empty() {
        let option_id = "answer-continue".to_string();
        option_map.insert(option_id.clone(), "continue".to_string());
        options.push(PermissionOption::new(
            option_id,
            "Continue",
            PermissionOptionKind::AllowOnce,
        ));
    }

    options.push(PermissionOption::new(
        "cancel",
        "Cancel",
        PermissionOptionKind::RejectOnce,
    ));

    (options, option_map)
}
