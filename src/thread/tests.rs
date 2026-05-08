use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use agent_client_protocol::schema::{
    RequestPermissionResponse, SessionConfigKind, SessionConfigSelectOptions, TextContent,
};
use codex_core::{config::ConfigOverrides, test_support::all_model_presets};
use codex_protocol::{
    ThreadId,
    config_types::ModeKind,
    protocol::{CollabAgentRef, CollabAgentStatusEntry, ThreadGoal},
    request_user_input::RequestUserInputQuestionOption,
};
use tokio::sync::{Mutex, Notify, mpsc::UnboundedSender};

use super::*;

#[tokio::test]
async fn test_prompt() -> anyhow::Result<()> {
    let (session_id, client, _, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["Hi".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(matches!(
        &notifications[0].update,
        SessionUpdate::AgentMessageChunk(ContentChunk {
            content: ContentBlock::Text(TextContent { text, .. }),
            ..
        }) if text == "Hi"
    ));

    Ok(())
}

#[tokio::test]
async fn test_thread_goal_updated_is_sent_as_agent_message() -> anyhow::Result<()> {
    let (session_id, client, _, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["thread-goal-update".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert!(notifications.iter().any(|notification| {
        matches!(
            &notification.update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "Goal updated (active): Ship the goal update"
        )
    }));

    Ok(())
}

#[tokio::test]
async fn test_model_verification_and_lifecycle_events_are_visible() -> anyhow::Result<()> {
    let (session_id, client, _, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["lifecycle-events".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let text = client
        .agent_text_notifications()
        .into_iter()
        .map(|chunk| chunk.to_string())
        .join("\n");
    assert!(text.contains("Running hook: hook-a"));
    assert!(text.contains("Hook completed: hook-a"));
    assert!(text.contains("Background task completed"));
    assert!(text.contains("**Deprecation:** Old setting"));
    assert!(text.contains("Thread rolled back: 2 turns removed"));
    assert!(text.contains("trusted access for cyber"));
    assert!(text.contains("MCP server `docs` startup failed: boom."));
    assert!(text.contains("MCP startup completed with issues:"));

    let notifications = client.notifications.lock().unwrap();
    assert!(notifications.iter().any(|notification| {
        matches!(
            &notification.update,
            SessionUpdate::ToolCall(tool_call)
                if tool_call.title == "Generating image"
                    && tool_call.status == ToolCallStatus::InProgress
        )
    }));
    assert!(notifications.iter().any(|notification| {
        matches!(
            &notification.update,
            SessionUpdate::ToolCallUpdate(update)
                if update.tool_call_id.0.as_ref() == "image-a"
                    && update.fields.status == Some(ToolCallStatus::Completed)
        )
    }));

    Ok(())
}

#[tokio::test]
async fn test_orphaned_exec_events_do_not_panic() -> anyhow::Result<()> {
    let (session_id, _client, _, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["orphaned-exec-events".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    Ok(())
}

#[tokio::test]
async fn test_usage_update_edge_cases() -> anyhow::Result<()> {
    let (session_id, client, _, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["usage-events".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    let usage_updates = notifications
        .iter()
        .filter_map(|notification| match &notification.update {
            SessionUpdate::UsageUpdate(update) => Some(update),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(usage_updates.len(), 2);
    assert_eq!(usage_updates[0].size, 200);
    assert_eq!(usage_updates[0].used, 25);
    assert!(usage_updates[0].cost.is_some());
    assert_eq!(usage_updates[1].used, 50);
    assert!(usage_updates[1].cost.is_none());

    Ok(())
}

#[tokio::test]
async fn test_compact() -> anyhow::Result<()> {
    let (session_id, client, thread, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["/compact".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(matches!(
        &notifications[0].update,
        SessionUpdate::AgentMessageChunk(ContentChunk {
            content: ContentBlock::Text(TextContent { text, .. }),
            ..
        }) if text == "Compact task completed"
    ));
    let ops = thread.ops.lock().unwrap();
    assert_eq!(ops.as_slice(), &[Op::Compact]);

    Ok(())
}

#[tokio::test]
async fn test_builtin_commands_include_debug_feedback_and_mention() -> anyhow::Result<()> {
    let (_session_id, client, _, message_tx, _handle) = setup().await?;
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Load { response_tx })?;
    response_rx.await??;
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    let available = notifications
        .iter()
        .find_map(|notification| match &notification.update {
            SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate {
                available_commands,
                ..
            }) => Some(available_commands),
            _ => None,
        });
    let Some(available) = available else {
        anyhow::bail!("expected available command update");
    };
    for command in ["mention", "feedback", "debug-config"] {
        assert!(
            available.iter().any(|available| available.name == command),
            "missing /{command}"
        );
    }

    Ok(())
}

#[tokio::test]
async fn test_mention_command_submits_file_reference() -> anyhow::Result<()> {
    let (session_id, _client, thread, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["/mention src/lib.rs".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let ops = thread.ops.lock().unwrap();
    assert!(matches!(
        ops.as_slice(),
        [Op::UserInput { items, .. }]
            if matches!(
                items.as_slice(),
                [UserInput::Text { text, .. }] if text == "@src/lib.rs"
            )
    ));

    Ok(())
}

#[tokio::test]
async fn test_feedback_and_debug_config_commands_are_generic() -> anyhow::Result<()> {
    let (session_id, client, _, message_tx, _handle) = setup().await?;

    for command in ["/feedback", "/debug-config"] {
        let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();
        message_tx.send(ThreadMessage::Prompt {
            request: PromptRequest::new(session_id.clone(), vec![command.into()]),
            response_tx: prompt_response_tx,
        })?;
        let stop_reason = prompt_response_rx.await??.await??;
        assert_eq!(stop_reason, StopReason::EndTurn);
    }
    drop(message_tx);

    let text = client.agent_text_notifications().join("\n");
    assert!(text.contains("Feedback collection is not available through generic ACP"));
    assert!(text.contains("## Session Status"));

    Ok(())
}

#[test]
fn test_guardian_execve_summary_uses_argv_without_duplication() -> anyhow::Result<()> {
    let action = GuardianAssessmentAction::Execve {
        source: GuardianCommandSource::UnifiedExec,
        program: "/bin/ls".to_string(),
        argv: vec!["/bin/ls".to_string(), "-l".to_string()],
        cwd: std::env::current_dir()?.try_into()?,
    };

    assert_eq!(
        guardian_action_summary(&action),
        Some("exec /bin/ls -l".to_string())
    );

    Ok(())
}

#[tokio::test]
async fn modes_match_augmented_workspace_permission_profile() -> anyhow::Result<()> {
    let mut config =
        Config::load_with_cli_overrides_and_harness_overrides(vec![], ConfigOverrides::default())
            .await?;
    config
        .permissions
        .approval_policy
        .set(codex_protocol::protocol::AskForApproval::OnRequest)?;

    let workspace_profile = PermissionProfile::workspace_write();
    let extra_roots = vec![config.codex_home.as_path().join("memories").try_into()?];
    let file_system_policy = workspace_profile
        .file_system_sandbox_policy()
        .with_additional_writable_roots(config.cwd.as_path(), &extra_roots);
    let augmented_profile = PermissionProfile::from_runtime_permissions(
        &file_system_policy,
        workspace_profile.network_sandbox_policy(),
    );
    assert_ne!(augmented_profile, workspace_profile);

    config
        .permissions
        .set_permission_profile_with_active_profile(
            augmented_profile,
            Some(ActivePermissionProfile::new(CODEX_WORKSPACE_PROFILE_ID)),
        )?;

    let mode_id = current_session_mode_id(&config).expect("mode should be recognized");
    assert_eq!(mode_id.0.as_ref(), "auto");

    Ok(())
}

#[tokio::test]
async fn modes_match_legacy_augmented_workspace_permission_profile() -> anyhow::Result<()> {
    let mut config =
        Config::load_with_cli_overrides_and_harness_overrides(vec![], ConfigOverrides::default())
            .await?;
    config
        .permissions
        .approval_policy
        .set(codex_protocol::protocol::AskForApproval::OnRequest)?;

    let workspace_profile = PermissionProfile::workspace_write();
    let extra_roots = vec![config.codex_home.as_path().join("memories").try_into()?];
    let file_system_policy = workspace_profile
        .file_system_sandbox_policy()
        .with_additional_writable_roots(config.cwd.as_path(), &extra_roots);
    let augmented_profile = PermissionProfile::from_runtime_permissions(
        &file_system_policy,
        workspace_profile.network_sandbox_policy(),
    );
    assert_ne!(augmented_profile, workspace_profile);

    config
        .permissions
        .set_permission_profile(augmented_profile)?;
    assert!(config.permissions.active_permission_profile().is_none());

    let mode_id = current_session_mode_id(&config).expect("mode should be recognized");
    assert_eq!(mode_id.0.as_ref(), "auto");

    Ok(())
}

#[test]
fn read_only_mode_does_not_trust_project() {
    assert!(!mode_trusts_project("read-only"));
    assert!(mode_trusts_project("auto"));
    assert!(mode_trusts_project("full-access"));
}

#[tokio::test]
async fn test_init() -> anyhow::Result<()> {
    let (session_id, client, thread, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["/init".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(
        matches!(
            &notifications[0].update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }), ..
            }) if text == INIT_COMMAND_PROMPT // we echo the prompt
        ),
        "notifications don't match {notifications:?}"
    );
    let ops = thread.ops.lock().unwrap();
    assert_eq!(
        ops.as_slice(),
        &[Op::UserInput {
            items: vec![UserInput::Text {
                text: INIT_COMMAND_PROMPT.to_string(),
                text_elements: vec![]
            }],
            final_output_json_schema: None,
            environments: None,
            responsesapi_client_metadata: None,
        }],
        "ops don't match {ops:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_review() -> anyhow::Result<()> {
    let (session_id, client, thread, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["/review".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(
        matches!(
            &notifications[0].update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "current changes" // we echo the prompt
        ),
        "notifications don't match {notifications:?}"
    );

    let ops = thread.ops.lock().unwrap();
    assert_eq!(
        ops.as_slice(),
        &[Op::Review {
            review_request: ReviewRequest {
                user_facing_hint: Some(user_facing_hint(&ReviewTarget::UncommittedChanges)),
                target: ReviewTarget::UncommittedChanges,
            }
        }],
        "ops don't match {ops:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_custom_review() -> anyhow::Result<()> {
    let (session_id, client, thread, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();
    let instructions = "Review what we did in agents.md";

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(
            session_id.clone(),
            vec![format!("/review {instructions}").into()],
        ),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(
        matches!(
            &notifications[0].update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "Review what we did in agents.md" // we echo the prompt
        ),
        "notifications don't match {notifications:?}"
    );

    let ops = thread.ops.lock().unwrap();
    assert_eq!(
        ops.as_slice(),
        &[Op::Review {
            review_request: ReviewRequest {
                user_facing_hint: Some(user_facing_hint(&ReviewTarget::Custom {
                    instructions: instructions.to_owned()
                })),
                target: ReviewTarget::Custom {
                    instructions: instructions.to_owned()
                },
            }
        }],
        "ops don't match {ops:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_commit_review() -> anyhow::Result<()> {
    let (session_id, client, thread, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["/review-commit 123456".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(
        matches!(
            &notifications[0].update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "commit 123456" // we echo the prompt
        ),
        "notifications don't match {notifications:?}"
    );

    let ops = thread.ops.lock().unwrap();
    assert_eq!(
        ops.as_slice(),
        &[Op::Review {
            review_request: ReviewRequest {
                user_facing_hint: Some(user_facing_hint(&ReviewTarget::Commit {
                    sha: "123456".to_owned(),
                    title: None
                })),
                target: ReviewTarget::Commit {
                    sha: "123456".to_owned(),
                    title: None
                },
            }
        }],
        "ops don't match {ops:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_branch_review() -> anyhow::Result<()> {
    let (session_id, client, thread, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["/review-branch feature".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(
        matches!(
            &notifications[0].update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "changes against 'feature'" // we echo the prompt
        ),
        "notifications don't match {notifications:?}"
    );

    let ops = thread.ops.lock().unwrap();
    assert_eq!(
        ops.as_slice(),
        &[Op::Review {
            review_request: ReviewRequest {
                user_facing_hint: Some(user_facing_hint(&ReviewTarget::BaseBranch {
                    branch: "feature".to_owned()
                })),
                target: ReviewTarget::BaseBranch {
                    branch: "feature".to_owned()
                },
            }
        }],
        "ops don't match {ops:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_set_collaboration_mode_submits_override() -> anyhow::Result<()> {
    let (_session_id, _client, thread, message_tx, _handle) = setup().await?;
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::SetConfigOption {
        config_id: SessionConfigId::new("collaboration_mode"),
        value: SessionConfigOptionValue::ValueId {
            value: SessionConfigValueId::new("plan"),
        },
        response_tx,
    })?;

    response_rx.await??;
    drop(message_tx);

    let ops = thread.ops.lock().unwrap();
    assert!(matches!(
        ops.last(),
        Some(Op::OverrideTurnContext {
            collaboration_mode: Some(mode),
            ..
        }) if mode.mode == ModeKind::Plan
    ));

    Ok(())
}

#[tokio::test]
async fn test_set_session_mode_submits_collaboration_override() -> anyhow::Result<()> {
    let (_session_id, _client, thread, message_tx, _handle) = setup().await?;
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::SetMode {
        mode: SessionModeId::new("plan"),
        response_tx,
    })?;

    response_rx.await??;
    drop(message_tx);

    let ops = thread.ops.lock().unwrap();
    assert!(matches!(
        ops.last(),
        Some(Op::OverrideTurnContext {
            collaboration_mode: Some(mode),
            ..
        }) if mode.mode == ModeKind::Plan
    ));

    Ok(())
}

#[tokio::test]
async fn test_set_approval_session_mode_resets_collaboration_mode() -> anyhow::Result<()> {
    let (_session_id, _client, thread, message_tx, _handle) = setup().await?;
    let (plan_response_tx, plan_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::SetMode {
        mode: SessionModeId::new("plan"),
        response_tx: plan_response_tx,
    })?;
    plan_response_rx.await??;

    let (approval_response_tx, approval_response_rx) = tokio::sync::oneshot::channel();
    message_tx.send(ThreadMessage::SetMode {
        mode: SessionModeId::new("full-access"),
        response_tx: approval_response_tx,
    })?;
    approval_response_rx.await??;
    drop(message_tx);

    let ops = thread.ops.lock().unwrap();
    assert!(matches!(
        ops.last(),
        Some(Op::OverrideTurnContext {
            collaboration_mode: Some(mode),
            approval_policy: Some(_),
            permission_profile: Some(permission_profile),
            ..
        }) if mode.mode == ModeKind::Default && permission_profile == &PermissionProfile::Disabled
    ));

    Ok(())
}

#[tokio::test]
async fn test_load_exposes_combined_modes_in_mode_selector() -> anyhow::Result<()> {
    let (_session_id, _client, _thread, message_tx, _handle) = setup().await?;
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Load { response_tx })?;

    let response = response_rx.await??;
    drop(message_tx);

    let modes = response.modes.expect("mode selector should be present");
    assert_eq!(modes.current_mode_id.0.as_ref(), "full-access");
    assert_eq!(
        modes
            .available_modes
            .iter()
            .map(|mode| mode.id.0.as_ref())
            .collect::<Vec<_>>(),
        vec!["read-only", "auto", "full-access", "plan"]
    );
    assert_eq!(
        modes
            .available_modes
            .iter()
            .map(|mode| mode.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Read Only", "Default", "Full Access", "Plan"]
    );
    assert_eq!(
        modes
            .available_modes
            .iter()
            .find(|mode| mode.id.0.as_ref() == "plan")
            .and_then(|mode| mode.description.as_deref()),
        Some(PLAN_MODE_DESCRIPTION)
    );

    Ok(())
}

#[tokio::test]
async fn test_combined_modes_are_exposed_as_config_option() -> anyhow::Result<()> {
    let (_session_id, _client, _thread, message_tx, _handle) = setup().await?;
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::GetConfigOptions { response_tx })?;

    let options = response_rx.await??;
    drop(message_tx);

    let Some(primary_option) = options.first() else {
        anyhow::bail!("expected config options");
    };
    assert_eq!(primary_option.id.0.as_ref(), "mode");
    assert!(matches!(
        primary_option.category.as_ref(),
        Some(SessionConfigOptionCategory::Mode)
    ));
    assert!(
        options
            .iter()
            .all(|option| option.id.0.as_ref() != "collaboration_mode")
    );
    let SessionConfigKind::Select(select) = &primary_option.kind else {
        anyhow::bail!("expected mode config option to be a select");
    };
    assert_eq!(select.current_value.0.as_ref(), "full-access");
    let SessionConfigSelectOptions::Ungrouped(select_options) = &select.options else {
        anyhow::bail!("expected ungrouped mode options");
    };
    assert_eq!(
        select_options
            .iter()
            .map(|option| (
                option.value.0.as_ref(),
                option.name.as_str(),
                option.description.as_deref()
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "read-only",
                "Read Only",
                Some(
                    "Codex can read files in the current workspace. Approval is required to edit files or access the internet."
                ),
            ),
            (
                "auto",
                "Default",
                Some(
                    "Codex can read and edit files in the current workspace, and run commands. Approval is required to access the internet or edit other files. (Identical to Agent mode)"
                ),
            ),
            (
                "full-access",
                "Full Access",
                Some(
                    "Codex can edit files outside this workspace and access the internet without asking for approval. Exercise caution when using."
                ),
            ),
            ("plan", "Plan", Some(PLAN_MODE_DESCRIPTION)),
        ]
    );

    Ok(())
}

#[tokio::test]
async fn test_plan_completion_prompts_and_accept_submits_default_mode_implementation()
-> anyhow::Result<()> {
    let client = Arc::new(StubClient::with_permission_responses(vec![
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(PLAN_IMPLEMENTATION_ACCEPT_DEFAULT_OPTION_ID),
        )),
    ]));
    let (session_id, client, thread, message_tx, _handle) = setup_with_client(client).await?;

    let (mode_response_tx, mode_response_rx) = tokio::sync::oneshot::channel();
    message_tx.send(ThreadMessage::SetMode {
        mode: SessionModeId::new("plan"),
        response_tx: mode_response_tx,
    })?;
    mode_response_rx.await??;

    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();
    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id, vec!["plan-turn".into()]),
        response_tx: prompt_response_tx,
    })?;
    let stop_reason_rx = prompt_response_rx.await??;
    assert_eq!(stop_reason_rx.await??, StopReason::EndTurn);

    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let has_implementation = thread.ops.lock().unwrap().iter().any(|op| {
                matches!(
                    op,
                    Op::UserInputWithTurnContext {
                        items,
                        collaboration_mode: Some(mode),
                        approval_policy: Some(_),
                        permission_profile: Some(permission_profile),
                        ..
                    } if mode.mode == ModeKind::Default
                        && permission_profile == &PermissionProfile::workspace_write()
                        && matches!(
                            items.as_slice(),
                            [UserInput::Text { text, .. }]
                                if text == PLAN_IMPLEMENTATION_CODING_MESSAGE
                        )
                )
            });
            if has_implementation {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;

    let requests = client.permission_requests.lock().unwrap();
    let request = requests
        .iter()
        .find(|request| {
            request
                .tool_call
                .fields
                .title
                .as_ref()
                .is_some_and(|title| title == "Implement this plan?")
        })
        .expect("expected plan implementation permission request");
    assert!(matches!(
        request.tool_call.fields.content.as_deref(),
        Some([
            ToolCallContent::Content(Content {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            })
        ]) if text == "- Step 1\n- Step 2\n"
    ));
    assert_eq!(
        request
            .options
            .iter()
            .map(|option| (
                option.option_id.0.to_string(),
                option.name.as_str(),
                option.kind
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                PLAN_IMPLEMENTATION_ACCEPT_DEFAULT_OPTION_ID.to_string(),
                "Accept and continue with Default profile",
                PermissionOptionKind::AllowOnce,
            ),
            (
                PLAN_IMPLEMENTATION_ACCEPT_FULL_ACCESS_OPTION_ID.to_string(),
                "Accept and continue with Full Access profile",
                PermissionOptionKind::AllowOnce,
            ),
            (
                PLAN_IMPLEMENTATION_STAY_OPTION_ID.to_string(),
                "Reject and wait for my message",
                PermissionOptionKind::RejectOnce,
            ),
        ]
    );
    let notifications = client.notifications.lock().unwrap();
    assert!(notifications.iter().any(|notification| matches!(
        &notification.update,
        SessionUpdate::ToolCallUpdate(ToolCallUpdate {
            fields,
            ..
        }) if fields.status == Some(ToolCallStatus::Completed)
            && fields.title.as_deref() == Some("User accepted plan")
            && matches!(
                fields.content.as_deref(),
                Some([
                    ToolCallContent::Content(Content {
                        content: ContentBlock::Text(TextContent { text, .. }),
                        ..
                    })
                ]) if text == "- Step 1\n- Step 2\n"
            )
    )));
    assert!(
        notifications.iter().all(|notification| !matches!(
            &notification.update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "- Step 1\n- Step 2\n"
        )),
        "plan text should be shown in the implementation prompt, not pasted into the chat"
    );

    Ok(())
}

#[tokio::test]
async fn test_plan_completion_keeps_prompt_active_until_accept_decision() -> anyhow::Result<()> {
    let notify = Arc::new(Notify::new());
    let client = Arc::new(StubClient::with_blocked_permission_requests(
        vec![RequestPermissionResponse::new(
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                PLAN_IMPLEMENTATION_ACCEPT_DEFAULT_OPTION_ID,
            )),
        )],
        notify.clone(),
    ));
    let (session_id, _client, _thread, message_tx, _handle) = setup_with_client(client).await?;

    let (mode_response_tx, mode_response_rx) = tokio::sync::oneshot::channel();
    message_tx.send(ThreadMessage::SetMode {
        mode: SessionModeId::new("plan"),
        response_tx: mode_response_tx,
    })?;
    mode_response_rx.await??;

    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();
    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id, vec!["plan-turn".into()]),
        response_tx: prompt_response_tx,
    })?;
    let mut stop_reason_rx = prompt_response_rx.await??;

    tokio::select! {
        result = &mut stop_reason_rx => {
            panic!("plan prompt ended before the accept/reject decision: {result:?}");
        }
        _ = tokio::time::sleep(Duration::from_millis(50)) => {}
    }

    notify.notify_waiters();
    assert_eq!(stop_reason_rx.await??, StopReason::EndTurn);

    Ok(())
}

#[tokio::test]
async fn test_plan_completion_accept_can_continue_with_full_access_profile() -> anyhow::Result<()> {
    let client = Arc::new(StubClient::with_permission_responses(vec![
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(PLAN_IMPLEMENTATION_ACCEPT_FULL_ACCESS_OPTION_ID),
        )),
    ]));
    let (session_id, _client, thread, message_tx, _handle) = setup_with_client(client).await?;

    let (mode_response_tx, mode_response_rx) = tokio::sync::oneshot::channel();
    message_tx.send(ThreadMessage::SetMode {
        mode: SessionModeId::new("plan"),
        response_tx: mode_response_tx,
    })?;
    mode_response_rx.await??;

    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();
    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id, vec!["plan-turn".into()]),
        response_tx: prompt_response_tx,
    })?;
    let stop_reason_rx = prompt_response_rx.await??;
    assert_eq!(stop_reason_rx.await??, StopReason::EndTurn);

    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let has_full_access_implementation = thread.ops.lock().unwrap().iter().any(|op| {
                matches!(
                    op,
                    Op::UserInputWithTurnContext {
                        collaboration_mode: Some(mode),
                        permission_profile: Some(permission_profile),
                        ..
                    } if mode.mode == ModeKind::Default
                        && permission_profile == &PermissionProfile::Disabled
                )
            });
            if has_full_access_implementation {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;

    Ok(())
}

#[tokio::test]
async fn test_plan_completion_stay_in_plan_does_not_submit_implementation() -> anyhow::Result<()> {
    let client = Arc::new(StubClient::with_permission_responses(vec![
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(PLAN_IMPLEMENTATION_STAY_OPTION_ID),
        )),
    ]));
    let (session_id, client, thread, message_tx, _handle) = setup_with_client(client).await?;

    let (mode_response_tx, mode_response_rx) = tokio::sync::oneshot::channel();
    message_tx.send(ThreadMessage::SetMode {
        mode: SessionModeId::new("plan"),
        response_tx: mode_response_tx,
    })?;
    mode_response_rx.await??;

    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();
    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id, vec!["plan-turn".into()]),
        response_tx: prompt_response_tx,
    })?;
    let stop_reason_rx = prompt_response_rx.await??;
    assert_eq!(stop_reason_rx.await??, StopReason::EndTurn);

    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = thread.ops.lock().unwrap();
    assert!(
        !ops.iter()
            .any(|op| matches!(op, Op::UserInputWithTurnContext { .. })),
        "stay in Plan mode should not submit implementation, got {ops:?}"
    );
    drop(ops);

    let notifications = client.notifications.lock().unwrap();
    assert!(notifications.iter().any(|notification| matches!(
        &notification.update,
        SessionUpdate::ToolCallUpdate(ToolCallUpdate {
            fields,
            ..
        }) if fields.status == Some(ToolCallStatus::Completed)
            && fields.title.as_deref() == Some("Waiting for user response")
            && matches!(
                fields.content.as_deref(),
                Some([
                    ToolCallContent::Content(Content {
                        content: ContentBlock::Text(TextContent { text, .. }),
                        ..
                    })
                ]) if text == "- Step 1\n- Step 2\n"
            )
    )));
    assert!(
        notifications.iter().all(|notification| !matches!(
            &notification.update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "- Step 1\n- Step 2\n"
        )),
        "rejected plan should stay in the tool-call UI, not be pasted into chat"
    );

    Ok(())
}

#[tokio::test]
async fn test_plan_updates_do_not_regress_after_context_compaction() -> anyhow::Result<()> {
    let (session_id, client, _conversation, _message_tx, _handle) = setup().await?;

    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    _message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id, vec!["plan-update-after-compact".into()]),
        response_tx,
    })?;
    response_rx.await??.await??;

    let notifications = client.notifications.lock().unwrap();
    let plans = notifications
        .iter()
        .filter_map(|notification| match &notification.update {
            SessionUpdate::Plan(plan) => Some(plan),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(
        plans.len(),
        2,
        "empty plan update should not clear the ACP plan: {plans:?}"
    );
    let final_entries = &plans.last().unwrap().entries;
    assert_eq!(final_entries.len(), 3);
    assert_eq!(final_entries[0].content, "Inspect current plan handling");
    assert_eq!(final_entries[0].status, PlanEntryStatus::Completed);
    assert_eq!(final_entries[1].content, "Patch compaction handling");
    assert_eq!(final_entries[1].status, PlanEntryStatus::InProgress);
    assert_eq!(final_entries[2].content, "Run tests");
    assert_eq!(final_entries[2].status, PlanEntryStatus::Pending);

    Ok(())
}

async fn setup() -> anyhow::Result<(
    SessionId,
    Arc<StubClient>,
    Arc<StubCodexThread>,
    UnboundedSender<ThreadMessage>,
    tokio::task::JoinHandle<()>,
)> {
    setup_with_client(Arc::new(StubClient::new())).await
}

async fn setup_with_client(
    client: Arc<StubClient>,
) -> anyhow::Result<(
    SessionId,
    Arc<StubClient>,
    Arc<StubCodexThread>,
    UnboundedSender<ThreadMessage>,
    tokio::task::JoinHandle<()>,
)> {
    let session_id = SessionId::new("test");
    let session_client =
        SessionClient::with_client(session_id.clone(), client.clone(), Arc::default());
    let conversation = Arc::new(StubCodexThread::new());
    let models_manager = Arc::new(StubModelsManager);
    let config =
        Config::load_with_cli_overrides_and_harness_overrides(vec![], ConfigOverrides::default())
            .await?;
    let (message_tx, message_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resolution_tx, resolution_rx) = tokio::sync::mpsc::unbounded_channel();

    let actor = ThreadActor::new(
        StubAuth,
        session_client,
        conversation.clone(),
        models_manager,
        config,
        message_rx,
        resolution_tx,
        resolution_rx,
    );

    let handle = tokio::spawn(actor.spawn());
    Ok((session_id, client, conversation, message_tx, handle))
}

struct StubAuth;

impl Auth for StubAuth {
    async fn logout(&self) -> Result<bool, Error> {
        Ok(true)
    }
}

struct StubModelsManager;

impl ModelsManagerImpl for StubModelsManager {
    fn get_model(
        &self,
        _model_id: &Option<String>,
    ) -> Pin<Box<dyn Future<Output = String> + Send + '_>> {
        Box::pin(async { all_model_presets()[0].to_owned().id })
    }

    fn list_models(&self) -> Pin<Box<dyn Future<Output = Vec<ModelPreset>> + Send + '_>> {
        Box::pin(async { all_model_presets().to_owned() })
    }

    fn list_collaboration_modes(
        &self,
    ) -> Pin<Box<dyn Future<Output = Vec<CollaborationModeMask>> + Send + '_>> {
        Box::pin(async {
            vec![
                CollaborationModeMask {
                    name: "Default".to_string(),
                    mode: Some(ModeKind::Default),
                    model: None,
                    reasoning_effort: None,
                    developer_instructions: None,
                },
                CollaborationModeMask {
                    name: "Plan".to_string(),
                    mode: Some(ModeKind::Plan),
                    model: None,
                    reasoning_effort: None,
                    developer_instructions: None,
                },
            ]
        })
    }
}

struct StubCodexThread {
    current_id: AtomicUsize,
    active_prompt_id: std::sync::Mutex<Option<String>>,
    ops: std::sync::Mutex<Vec<Op>>,
    op_tx: mpsc::UnboundedSender<Event>,
    op_rx: Mutex<mpsc::UnboundedReceiver<Event>>,
}

impl StubCodexThread {
    fn new() -> Self {
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        StubCodexThread {
            current_id: AtomicUsize::new(0),
            active_prompt_id: std::sync::Mutex::default(),
            ops: std::sync::Mutex::default(),
            op_tx,
            op_rx: Mutex::new(op_rx),
        }
    }
}

impl CodexThreadImpl for StubCodexThread {
    fn submit(
        &self,
        op: Op,
    ) -> Pin<Box<dyn Future<Output = Result<String, CodexErr>> + Send + '_>> {
        Box::pin(async move {
            let id = self
                .current_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

            self.ops.lock().unwrap().push(op.clone());

            match op {
                Op::UserInput { items, .. } | Op::UserInputWithTurnContext { items, .. } => {
                    *self.active_prompt_id.lock().unwrap() = Some(id.to_string());
                    let prompt = items
                        .into_iter()
                        .map(|i| match i {
                            UserInput::Text { text, .. } => text,
                            UserInput::Skill { name, .. } => format!("${name}"),
                            _ => unimplemented!(),
                        })
                        .join("\n");

                    if prompt == "parallel-exec" {
                        // Emit interleaved exec events: Begin A, Begin B, End A, End B
                        let turn_id = id.to_string();
                        let cwd = std::env::current_dir().unwrap();
                        let send = |msg| {
                            self.op_tx
                                .send(Event {
                                    id: id.to_string(),
                                    msg,
                                })
                                .unwrap();
                        };
                        send(EventMsg::ExecCommandBegin(ExecCommandBeginEvent {
                            call_id: "call-a".into(),
                            process_id: None,
                            turn_id: turn_id.clone(),
                            command: vec!["echo".into(), "a".into()],
                            cwd: cwd.clone().try_into()?,
                            parsed_cmd: vec![ParsedCommand::Unknown {
                                cmd: "echo a".into(),
                            }],
                            source: Default::default(),
                            interaction_input: None,
                            started_at_ms: 0,
                        }));
                        send(EventMsg::ExecCommandBegin(ExecCommandBeginEvent {
                            call_id: "call-b".into(),
                            process_id: None,
                            turn_id: turn_id.clone(),
                            command: vec!["echo".into(), "b".into()],
                            cwd: cwd.clone().try_into()?,
                            parsed_cmd: vec![ParsedCommand::Unknown {
                                cmd: "echo b".into(),
                            }],
                            source: Default::default(),
                            interaction_input: None,
                            started_at_ms: 0,
                        }));
                        send(EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                            call_id: "call-a".into(),
                            process_id: None,
                            turn_id: turn_id.clone(),
                            command: vec!["echo".into(), "a".into()],
                            cwd: cwd.clone().try_into()?,
                            parsed_cmd: vec![],
                            source: Default::default(),
                            interaction_input: None,
                            stdout: "a\n".into(),
                            stderr: String::new(),
                            aggregated_output: "a\n".into(),
                            exit_code: 0,
                            duration: std::time::Duration::from_millis(10),
                            formatted_output: "a\n".into(),
                            status: ExecCommandStatus::Completed,
                            completed_at_ms: 0,
                        }));
                        send(EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                            call_id: "call-b".into(),
                            process_id: None,
                            turn_id: turn_id.clone(),
                            command: vec!["echo".into(), "b".into()],
                            cwd: cwd.clone().try_into()?,
                            parsed_cmd: vec![],
                            source: Default::default(),
                            interaction_input: None,
                            stdout: "b\n".into(),
                            stderr: String::new(),
                            aggregated_output: "b\n".into(),
                            exit_code: 0,
                            duration: std::time::Duration::from_millis(10),
                            formatted_output: "b\n".into(),
                            status: ExecCommandStatus::Completed,
                            completed_at_ms: 0,
                        }));
                        send(EventMsg::TurnComplete(TurnCompleteEvent {
                            last_agent_message: None,
                            turn_id,
                            completed_at: None,
                            duration_ms: None,
                            time_to_first_token_ms: None,
                        }));
                    } else if prompt == "thread-goal-update" {
                        let turn_id = id.to_string();
                        let thread_id = ThreadId::default();
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::ThreadGoalUpdated(ThreadGoalUpdatedEvent {
                                    thread_id,
                                    turn_id: Some(turn_id.clone()),
                                    goal: ThreadGoal {
                                        thread_id,
                                        objective: "Ship the goal update".to_string(),
                                        status: ThreadGoalStatus::Active,
                                        token_budget: Some(100),
                                        tokens_used: 10,
                                        time_used_seconds: 2,
                                        created_at: 1,
                                        updated_at: 2,
                                    },
                                }),
                            })
                            .unwrap();
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                                    last_agent_message: None,
                                    turn_id,
                                    completed_at: None,
                                    duration_ms: None,
                                    time_to_first_token_ms: None,
                                }),
                            })
                            .unwrap();
                    } else if prompt == "approval-block" {
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
                                    call_id: "call-id".to_string(),
                                    approval_id: Some("approval-id".to_string()),
                                    turn_id: id.to_string(),
                                    command: vec!["echo".to_string(), "hi".to_string()],
                                    cwd: std::env::current_dir().unwrap().try_into().unwrap(),
                                    reason: None,
                                    network_approval_context: None,
                                    proposed_execpolicy_amendment: None,
                                    proposed_network_policy_amendments: None,
                                    additional_permissions: None,
                                    available_decisions: Some(vec![
                                        ReviewDecision::Approved,
                                        ReviewDecision::Abort,
                                    ]),
                                    parsed_cmd: vec![ParsedCommand::Unknown {
                                        cmd: "echo hi".to_string(),
                                    }],
                                }),
                            })
                            .unwrap();
                    } else if prompt == "plan-turn" {
                        let turn_id = id.to_string();
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::TurnStarted(TurnStartedEvent {
                                    model_context_window: None,
                                    collaboration_mode_kind: ModeKind::Plan,
                                    turn_id: turn_id.clone(),
                                    started_at: None,
                                }),
                            })
                            .unwrap();
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::ItemCompleted(ItemCompletedEvent {
                                    thread_id: codex_protocol::ThreadId::new(),
                                    turn_id: turn_id.clone(),
                                    item: TurnItem::Plan(codex_protocol::items::PlanItem {
                                        id: "plan-item".to_string(),
                                        text: "- Step 1\n- Step 2\n".to_string(),
                                    }),
                                    completed_at_ms: 0,
                                }),
                            })
                            .unwrap();
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                                    last_agent_message: None,
                                    turn_id,
                                    completed_at: None,
                                    duration_ms: None,
                                    time_to_first_token_ms: None,
                                }),
                            })
                            .unwrap();
                    } else if prompt == "plan-update-after-compact" {
                        let turn_id = id.to_string();
                        let thread_id = codex_protocol::ThreadId::new();
                        let compact_item = codex_protocol::items::ContextCompactionItem::new();
                        let send = |msg| {
                            self.op_tx
                                .send(Event {
                                    id: id.to_string(),
                                    msg,
                                })
                                .unwrap();
                        };

                        send(EventMsg::TurnStarted(TurnStartedEvent {
                            model_context_window: None,
                            collaboration_mode_kind: ModeKind::Default,
                            turn_id: turn_id.clone(),
                            started_at: None,
                        }));
                        send(EventMsg::PlanUpdate(UpdatePlanArgs {
                            explanation: None,
                            plan: vec![
                                PlanItemArg {
                                    step: "Inspect current plan handling".to_string(),
                                    status: StepStatus::Completed,
                                },
                                PlanItemArg {
                                    step: "Patch compaction handling".to_string(),
                                    status: StepStatus::InProgress,
                                },
                                PlanItemArg {
                                    step: "Run tests".to_string(),
                                    status: StepStatus::Pending,
                                },
                            ],
                        }));
                        send(EventMsg::ItemStarted(ItemStartedEvent {
                            thread_id,
                            turn_id: turn_id.clone(),
                            item: TurnItem::ContextCompaction(compact_item.clone()),
                            started_at_ms: 0,
                        }));
                        send(EventMsg::ItemCompleted(ItemCompletedEvent {
                            thread_id,
                            turn_id: turn_id.clone(),
                            item: TurnItem::ContextCompaction(compact_item),
                            completed_at_ms: 0,
                        }));
                        send(EventMsg::PlanUpdate(UpdatePlanArgs {
                            explanation: None,
                            plan: Vec::new(),
                        }));
                        send(EventMsg::PlanUpdate(UpdatePlanArgs {
                            explanation: None,
                            plan: vec![
                                PlanItemArg {
                                    step: "Inspect current plan handling".to_string(),
                                    status: StepStatus::InProgress,
                                },
                                PlanItemArg {
                                    step: "Patch compaction handling".to_string(),
                                    status: StepStatus::Pending,
                                },
                                PlanItemArg {
                                    step: "Run tests".to_string(),
                                    status: StepStatus::Pending,
                                },
                            ],
                        }));
                        send(EventMsg::TurnComplete(TurnCompleteEvent {
                            last_agent_message: None,
                            turn_id,
                            completed_at: None,
                            duration_ms: None,
                            time_to_first_token_ms: None,
                        }));
                    } else if prompt == "subagents" {
                        let turn_id = id.to_string();
                        let sender_thread_id = codex_protocol::ThreadId::new();
                        let receiver_thread_id = codex_protocol::ThreadId::new();
                        let failed_thread_id = codex_protocol::ThreadId::new();
                        let send = |msg| {
                            self.op_tx
                                .send(Event {
                                    id: id.to_string(),
                                    msg,
                                })
                                .unwrap();
                        };
                        send(EventMsg::CollabAgentSpawnBegin(
                            CollabAgentSpawnBeginEvent {
                                call_id: "spawn-a".to_string(),
                                sender_thread_id: sender_thread_id.clone(),
                                prompt: "Inspect the parser module".to_string(),
                                model: "gpt-test".to_string(),
                                reasoning_effort: ReasoningEffort::Medium,
                                started_at_ms: 0,
                            },
                        ));
                        send(EventMsg::CollabAgentSpawnEnd(CollabAgentSpawnEndEvent {
                            call_id: "spawn-a".to_string(),
                            sender_thread_id: sender_thread_id.clone(),
                            new_thread_id: Some(receiver_thread_id.clone()),
                            new_agent_nickname: Some("atlas".to_string()),
                            new_agent_role: Some("explorer".to_string()),
                            prompt: "Inspect the parser module".to_string(),
                            model: "gpt-test".to_string(),
                            reasoning_effort: ReasoningEffort::Medium,
                            status: AgentStatus::Running,
                            completed_at_ms: 0,
                        }));
                        send(EventMsg::CollabAgentInteractionBegin(
                            CollabAgentInteractionBeginEvent {
                                call_id: "message-a".to_string(),
                                sender_thread_id: sender_thread_id.clone(),
                                receiver_thread_id: receiver_thread_id.clone(),
                                prompt: "Check the failing path".to_string(),
                                started_at_ms: 0,
                            },
                        ));
                        send(EventMsg::CollabAgentInteractionEnd(
                            CollabAgentInteractionEndEvent {
                                call_id: "message-a".to_string(),
                                sender_thread_id: sender_thread_id.clone(),
                                receiver_thread_id: receiver_thread_id.clone(),
                                receiver_agent_nickname: Some("atlas".to_string()),
                                receiver_agent_role: Some("explorer".to_string()),
                                prompt: "Check the failing path".to_string(),
                                status: AgentStatus::Completed(Some("done".to_string())),
                                completed_at_ms: 0,
                            },
                        ));
                        send(EventMsg::CollabWaitingBegin(CollabWaitingBeginEvent {
                            sender_thread_id: sender_thread_id.clone(),
                            receiver_thread_ids: vec![
                                receiver_thread_id.clone(),
                                failed_thread_id.clone(),
                            ],
                            receiver_agents: vec![CollabAgentRef {
                                thread_id: receiver_thread_id.clone(),
                                agent_nickname: Some("atlas".to_string()),
                                agent_role: Some("explorer".to_string()),
                            }],
                            call_id: "wait-a".to_string(),
                            started_at_ms: 0,
                        }));
                        send(EventMsg::CollabWaitingEnd(CollabWaitingEndEvent {
                            sender_thread_id: sender_thread_id.clone(),
                            call_id: "wait-a".to_string(),
                            agent_statuses: vec![CollabAgentStatusEntry {
                                thread_id: receiver_thread_id.clone(),
                                agent_nickname: Some("atlas".to_string()),
                                agent_role: Some("explorer".to_string()),
                                status: AgentStatus::Completed(Some("done".to_string())),
                            }],
                            statuses: HashMap::from([(
                                failed_thread_id.clone(),
                                AgentStatus::Errored("boom".to_string()),
                            )]),
                            completed_at_ms: 0,
                        }));
                        send(EventMsg::CollabResumeBegin(CollabResumeBeginEvent {
                            call_id: "resume-a".to_string(),
                            sender_thread_id: sender_thread_id.clone(),
                            receiver_thread_id: receiver_thread_id.clone(),
                            receiver_agent_nickname: Some("atlas".to_string()),
                            receiver_agent_role: Some("explorer".to_string()),
                            started_at_ms: 0,
                        }));
                        send(EventMsg::CollabResumeEnd(CollabResumeEndEvent {
                            call_id: "resume-a".to_string(),
                            sender_thread_id: sender_thread_id.clone(),
                            receiver_thread_id: receiver_thread_id.clone(),
                            receiver_agent_nickname: Some("atlas".to_string()),
                            receiver_agent_role: Some("explorer".to_string()),
                            status: AgentStatus::Running,
                            completed_at_ms: 0,
                        }));
                        send(EventMsg::CollabCloseBegin(CollabCloseBeginEvent {
                            call_id: "close-a".to_string(),
                            sender_thread_id: sender_thread_id.clone(),
                            receiver_thread_id: receiver_thread_id.clone(),
                            started_at_ms: 0,
                        }));
                        send(EventMsg::CollabCloseEnd(CollabCloseEndEvent {
                            call_id: "close-a".to_string(),
                            sender_thread_id,
                            receiver_thread_id,
                            receiver_agent_nickname: Some("atlas".to_string()),
                            receiver_agent_role: Some("explorer".to_string()),
                            status: AgentStatus::Shutdown,
                            completed_at_ms: 0,
                        }));
                        send(EventMsg::TurnComplete(TurnCompleteEvent {
                            last_agent_message: None,
                            turn_id,
                            completed_at: None,
                            duration_ms: None,
                            time_to_first_token_ms: None,
                        }));
                    } else if prompt == "raw-tool-items" {
                        let turn_id = id.to_string();
                        let send = |msg| {
                            self.op_tx
                                .send(Event {
                                    id: id.to_string(),
                                    msg,
                                })
                                .unwrap();
                        };
                        send(EventMsg::RawResponseItem(RawResponseItemEvent {
                            item: ResponseItem::ToolSearchCall {
                                id: None,
                                call_id: Some("tool-search-a".to_string()),
                                status: Some("in_progress".to_string()),
                                execution: "server".to_string(),
                                arguments: serde_json::json!({ "query": "repo" }),
                            },
                        }));
                        send(EventMsg::RawResponseItem(RawResponseItemEvent {
                            item: ResponseItem::ToolSearchOutput {
                                call_id: Some("tool-search-a".to_string()),
                                status: "completed".to_string(),
                                execution: "server".to_string(),
                                tools: vec![serde_json::json!({ "name": "repo-search" })],
                            },
                        }));
                        send(EventMsg::RawResponseItem(RawResponseItemEvent {
                            item: ResponseItem::ImageGenerationCall {
                                id: "image-a".to_string(),
                                status: "completed".to_string(),
                                revised_prompt: Some("A compact interface mockup".to_string()),
                                result: "image-bytes".to_string(),
                            },
                        }));
                        send(EventMsg::TurnComplete(TurnCompleteEvent {
                            last_agent_message: None,
                            turn_id,
                            completed_at: None,
                            duration_ms: None,
                            time_to_first_token_ms: None,
                        }));
                    } else if prompt == "lifecycle-events" {
                        let turn_id = id.to_string();
                        let source_path = std::env::current_dir()?.try_into()?;
                        let hook_run = codex_protocol::protocol::HookRunSummary {
                            id: "hook-a".to_string(),
                            event_name: codex_protocol::protocol::HookEventName::PostToolUse,
                            handler_type: codex_protocol::protocol::HookHandlerType::Command,
                            execution_mode: codex_protocol::protocol::HookExecutionMode::Sync,
                            scope: codex_protocol::protocol::HookScope::Turn,
                            source_path,
                            source: codex_protocol::protocol::HookSource::User,
                            display_order: 0,
                            status: codex_protocol::protocol::HookRunStatus::Completed,
                            status_message: Some("ok".to_string()),
                            started_at: 1,
                            completed_at: Some(2),
                            duration_ms: Some(1),
                            entries: Vec::new(),
                        };
                        let send = |msg| {
                            self.op_tx
                                .send(Event {
                                    id: id.to_string(),
                                    msg,
                                })
                                .unwrap();
                        };
                        send(EventMsg::HookStarted(HookStartedEvent {
                            turn_id: Some(turn_id.clone()),
                            run: hook_run.clone(),
                        }));
                        send(EventMsg::HookCompleted(HookCompletedEvent {
                            turn_id: Some(turn_id.clone()),
                            run: hook_run,
                        }));
                        send(EventMsg::ImageGenerationBegin(ImageGenerationBeginEvent {
                            call_id: "image-a".to_string(),
                        }));
                        send(EventMsg::ImageGenerationEnd(ImageGenerationEndEvent {
                            call_id: "image-a".to_string(),
                            status: "success".to_string(),
                            revised_prompt: None,
                            result: "image saved".to_string(),
                            saved_path: None,
                        }));
                        send(EventMsg::Warning(WarningEvent {
                            message: "Background task completed".to_string(),
                        }));
                        send(EventMsg::DeprecationNotice(DeprecationNoticeEvent {
                            summary: "Old setting".to_string(),
                            details: Some("Use the new setting.".to_string()),
                        }));
                        send(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
                            num_turns: 2,
                        }));
                        send(EventMsg::ModelVerification(ModelVerificationEvent {
                            verifications: vec![ModelVerification::TrustedAccessForCyber],
                        }));
                        send(EventMsg::McpStartupUpdate(McpStartupUpdateEvent {
                            server: "docs".to_string(),
                            status: McpStartupStatus::Failed {
                                error: "boom".to_string(),
                            },
                        }));
                        send(EventMsg::McpStartupComplete(McpStartupCompleteEvent {
                            ready: vec!["fs".to_string()],
                            failed: vec![codex_protocol::protocol::McpStartupFailure {
                                server: "docs".to_string(),
                                error: "boom".to_string(),
                            }],
                            cancelled: vec!["calendar".to_string()],
                        }));
                        send(EventMsg::TurnComplete(TurnCompleteEvent {
                            last_agent_message: None,
                            turn_id,
                            completed_at: None,
                            duration_ms: None,
                            time_to_first_token_ms: None,
                        }));
                    } else if prompt == "orphaned-exec-events" {
                        let turn_id = id.to_string();
                        let cwd = std::env::current_dir().unwrap();
                        let send = |msg| {
                            self.op_tx
                                .send(Event {
                                    id: id.to_string(),
                                    msg,
                                })
                                .unwrap();
                        };
                        send(EventMsg::ExecCommandOutputDelta(
                            ExecCommandOutputDeltaEvent {
                                call_id: "missing".to_string(),
                                stream: codex_protocol::protocol::ExecOutputStream::Stdout,
                                chunk: b"hello".to_vec(),
                            },
                        ));
                        send(EventMsg::TerminalInteraction(TerminalInteractionEvent {
                            call_id: "missing".to_string(),
                            process_id: "123".to_string(),
                            stdin: "input".to_string(),
                        }));
                        send(EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                            call_id: "missing".to_string(),
                            process_id: None,
                            turn_id: turn_id.clone(),
                            command: vec!["echo".into(), "missing".into()],
                            cwd: cwd.try_into()?,
                            parsed_cmd: vec![],
                            source: Default::default(),
                            interaction_input: None,
                            stdout: "hello".into(),
                            stderr: String::new(),
                            aggregated_output: "hello".into(),
                            exit_code: 0,
                            duration: std::time::Duration::from_millis(10),
                            formatted_output: "hello".into(),
                            status: ExecCommandStatus::Completed,
                            completed_at_ms: 0,
                        }));
                        send(EventMsg::TurnComplete(TurnCompleteEvent {
                            last_agent_message: None,
                            turn_id,
                            completed_at: None,
                            duration_ms: None,
                            time_to_first_token_ms: None,
                        }));
                    } else if prompt == "usage-events" {
                        let turn_id = id.to_string();
                        let send = |msg| {
                            self.op_tx
                                .send(Event {
                                    id: id.to_string(),
                                    msg,
                                })
                                .unwrap();
                        };
                        send(EventMsg::TokenCount(TokenCountEvent {
                            info: None,
                            rate_limits: None,
                        }));
                        send(EventMsg::TokenCount(TokenCountEvent {
                            info: Some(codex_protocol::protocol::TokenUsageInfo {
                                total_token_usage: codex_protocol::protocol::TokenUsage {
                                    total_tokens: 25,
                                    ..Default::default()
                                },
                                last_token_usage: codex_protocol::protocol::TokenUsage {
                                    total_tokens: 25,
                                    ..Default::default()
                                },
                                model_context_window: Some(200),
                            }),
                            rate_limits: Some(codex_protocol::protocol::RateLimitSnapshot {
                                limit_id: None,
                                limit_name: None,
                                primary: None,
                                secondary: None,
                                credits: Some(codex_protocol::protocol::CreditsSnapshot {
                                    has_credits: true,
                                    unlimited: false,
                                    balance: Some("1.25".to_string()),
                                }),
                                plan_type: None,
                                rate_limit_reached_type: None,
                            }),
                        }));
                        send(EventMsg::TokenCount(TokenCountEvent {
                            info: Some(codex_protocol::protocol::TokenUsageInfo {
                                total_token_usage: codex_protocol::protocol::TokenUsage {
                                    total_tokens: 50,
                                    ..Default::default()
                                },
                                last_token_usage: codex_protocol::protocol::TokenUsage {
                                    total_tokens: 25,
                                    ..Default::default()
                                },
                                model_context_window: Some(200),
                            }),
                            rate_limits: Some(codex_protocol::protocol::RateLimitSnapshot {
                                limit_id: None,
                                limit_name: None,
                                primary: None,
                                secondary: None,
                                credits: Some(codex_protocol::protocol::CreditsSnapshot {
                                    has_credits: true,
                                    unlimited: false,
                                    balance: Some("not-a-number".to_string()),
                                }),
                                plan_type: None,
                                rate_limit_reached_type: None,
                            }),
                        }));
                        send(EventMsg::TurnComplete(TurnCompleteEvent {
                            last_agent_message: None,
                            turn_id,
                            completed_at: None,
                            duration_ms: None,
                            time_to_first_token_ms: None,
                        }));
                    } else {
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::AgentMessageContentDelta(
                                    AgentMessageContentDeltaEvent {
                                        thread_id: id.to_string(),
                                        turn_id: id.to_string(),
                                        item_id: id.to_string(),
                                        delta: prompt.clone(),
                                    },
                                ),
                            })
                            .unwrap();
                        // Send non-delta event (should be deduplicated, but handled by deduplication)
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::AgentMessage(AgentMessageEvent {
                                    message: prompt,
                                    phase: None,
                                    memory_citation: None,
                                }),
                            })
                            .unwrap();
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                                    last_agent_message: None,
                                    turn_id: id.to_string(),
                                    completed_at: None,
                                    duration_ms: None,
                                    time_to_first_token_ms: None,
                                }),
                            })
                            .unwrap();
                    }
                }
                Op::Compact => {
                    self.op_tx
                        .send(Event {
                            id: id.to_string(),
                            msg: EventMsg::TurnStarted(TurnStartedEvent {
                                model_context_window: None,
                                collaboration_mode_kind: ModeKind::default(),
                                turn_id: id.to_string(),
                                started_at: None,
                            }),
                        })
                        .unwrap();
                    self.op_tx
                        .send(Event {
                            id: id.to_string(),
                            msg: EventMsg::AgentMessage(AgentMessageEvent {
                                message: "Compact task completed".to_string(),
                                phase: None,
                                memory_citation: None,
                            }),
                        })
                        .unwrap();
                    self.op_tx
                        .send(Event {
                            id: id.to_string(),
                            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                                last_agent_message: None,
                                turn_id: id.to_string(),
                                completed_at: None,
                                duration_ms: None,
                                time_to_first_token_ms: None,
                            }),
                        })
                        .unwrap();
                }
                Op::Review { review_request } => {
                    self.op_tx
                        .send(Event {
                            id: id.to_string(),
                            msg: EventMsg::EnteredReviewMode(review_request.clone()),
                        })
                        .unwrap();
                    self.op_tx
                        .send(Event {
                            id: id.to_string(),
                            msg: EventMsg::ExitedReviewMode(ExitedReviewModeEvent {
                                review_output: Some(ReviewOutputEvent {
                                    findings: vec![],
                                    overall_correctness: String::new(),
                                    overall_explanation: review_request
                                        .user_facing_hint
                                        .clone()
                                        .unwrap_or_default(),
                                    overall_confidence_score: 1.,
                                }),
                            }),
                        })
                        .unwrap();
                    self.op_tx
                        .send(Event {
                            id: id.to_string(),
                            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                                last_agent_message: None,
                                turn_id: id.to_string(),
                                completed_at: None,
                                duration_ms: None,
                                time_to_first_token_ms: None,
                            }),
                        })
                        .unwrap();
                }
                Op::ExecApproval { .. }
                | Op::ResolveElicitation { .. }
                | Op::RequestPermissionsResponse { .. }
                | Op::PatchApproval { .. }
                | Op::UserInputAnswer { .. }
                | Op::OverrideTurnContext { .. }
                | Op::Interrupt => {}
                Op::Shutdown => {
                    if let Some(active_prompt_id) = self.active_prompt_id.lock().unwrap().take() {
                        self.op_tx
                            .send(Event {
                                id: active_prompt_id.clone(),
                                msg: EventMsg::TurnAborted(TurnAbortedEvent {
                                    turn_id: Some(active_prompt_id),
                                    reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
                                    completed_at: None,
                                    duration_ms: None,
                                }),
                            })
                            .unwrap();
                    }
                }
                _ => {
                    unimplemented!()
                }
            }
            Ok(id.to_string())
        })
    }

    fn next_event(&self) -> Pin<Box<dyn Future<Output = Result<Event, CodexErr>> + Send + '_>> {
        Box::pin(async {
            let Some(event) = self.op_rx.lock().await.recv().await else {
                return Err(CodexErr::InternalAgentDied);
            };
            Ok(event)
        })
    }
}

struct StubClient {
    notifications: std::sync::Mutex<Vec<SessionNotification>>,
    permission_requests: std::sync::Mutex<Vec<RequestPermissionRequest>>,
    permission_responses: std::sync::Mutex<VecDeque<RequestPermissionResponse>>,
    block_permission_requests: Option<Arc<Notify>>,
}

impl StubClient {
    fn new() -> Self {
        StubClient {
            notifications: std::sync::Mutex::default(),
            permission_requests: std::sync::Mutex::default(),
            permission_responses: std::sync::Mutex::default(),
            block_permission_requests: None,
        }
    }

    fn with_permission_responses(responses: Vec<RequestPermissionResponse>) -> Self {
        StubClient {
            notifications: std::sync::Mutex::default(),
            permission_requests: std::sync::Mutex::default(),
            permission_responses: std::sync::Mutex::new(responses.into()),
            block_permission_requests: None,
        }
    }

    fn with_blocked_permission_requests(
        responses: Vec<RequestPermissionResponse>,
        notify: Arc<Notify>,
    ) -> Self {
        StubClient {
            notifications: std::sync::Mutex::default(),
            permission_requests: std::sync::Mutex::default(),
            permission_responses: std::sync::Mutex::new(responses.into()),
            block_permission_requests: Some(notify),
        }
    }

    fn agent_text_notifications(&self) -> Vec<String> {
        self.notifications
            .lock()
            .unwrap()
            .iter()
            .filter_map(|notification| match &notification.update {
                SessionUpdate::AgentMessageChunk(ContentChunk {
                    content: ContentBlock::Text(TextContent { text, .. }),
                    ..
                }) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }
}

impl ClientSender for StubClient {
    fn send_session_notification(&self, args: SessionNotification) -> Result<(), Error> {
        self.notifications.lock().unwrap().push(args);
        Ok(())
    }

    fn request_permission(
        &self,
        args: RequestPermissionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<RequestPermissionResponse, Error>> + Send + '_>> {
        Box::pin(async move {
            self.permission_requests.lock().unwrap().push(args);
            if let Some(notify) = &self.block_permission_requests {
                notify.notified().await;
            }
            Ok(self
                .permission_responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
                }))
        })
    }
}

#[tokio::test]
async fn test_parallel_exec_commands() -> anyhow::Result<()> {
    let (session_id, client, _, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["parallel-exec".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();

    // Collect all ToolCall (begin) notifications keyed by their tool_call_id prefix.
    let tool_calls: Vec<_> = notifications
        .iter()
        .filter_map(|n| match &n.update {
            SessionUpdate::ToolCall(tc) => Some(tc.clone()),
            _ => None,
        })
        .collect();

    // Collect all ToolCallUpdate notifications that carry a terminal status.
    let completed_updates: Vec<_> = notifications
        .iter()
        .filter_map(|n| match &n.update {
            SessionUpdate::ToolCallUpdate(update) => {
                if update.fields.status == Some(ToolCallStatus::Completed) {
                    Some(update.clone())
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect();

    // Both commands A and B should have produced a ToolCall (begin).
    assert_eq!(
        tool_calls.len(),
        2,
        "expected 2 ToolCall begin notifications, got {tool_calls:?}"
    );

    // Both commands A and B should have produced a completed ToolCallUpdate.
    assert_eq!(
        completed_updates.len(),
        2,
        "expected 2 completed ToolCallUpdate notifications, got {completed_updates:?}"
    );

    // The completed updates should reference the same tool_call_ids as the begins.
    let begin_ids: std::collections::HashSet<_> = tool_calls
        .iter()
        .map(|tc| tc.tool_call_id.clone())
        .collect();
    let end_ids: std::collections::HashSet<_> = completed_updates
        .iter()
        .map(|u| u.tool_call_id.clone())
        .collect();
    assert_eq!(
        begin_ids, end_ids,
        "completed update tool_call_ids should match begin tool_call_ids"
    );

    Ok(())
}

#[tokio::test]
async fn test_subagent_events_surface_as_tool_calls() -> anyhow::Result<()> {
    let (session_id, client, _, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["subagents".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    let tool_calls: Vec<_> = notifications
        .iter()
        .filter_map(|n| match &n.update {
            SessionUpdate::ToolCall(tc) => Some(tc.clone()),
            _ => None,
        })
        .collect();
    let tool_updates: Vec<_> = notifications
        .iter()
        .filter_map(|n| match &n.update {
            SessionUpdate::ToolCallUpdate(update) => Some(update.clone()),
            _ => None,
        })
        .collect();

    assert!(
        tool_calls.iter().any(|tool_call| {
            tool_call.tool_call_id.0.as_ref() == "spawn-a"
                && tool_call.title.starts_with("Spawning subagent")
                && tool_call.status == ToolCallStatus::InProgress
                && tool_call.raw_input.is_some()
        }),
        "expected spawn ToolCall, got {tool_calls:?}"
    );
    assert!(
        tool_calls.iter().any(|tool_call| {
            tool_call.tool_call_id.0.as_ref() == "message-a"
                && tool_call.title.starts_with("Messaging subagent")
        }),
        "expected interaction ToolCall, got {tool_calls:?}"
    );
    assert!(
        tool_calls.iter().any(|tool_call| {
            tool_call.tool_call_id.0.as_ref() == "wait-a"
                && tool_call.title == "Waiting for 2 subagents"
        }),
        "expected wait ToolCall, got {tool_calls:?}"
    );
    assert!(
        tool_calls.iter().any(|tool_call| {
            tool_call.tool_call_id.0.as_ref() == "resume-a"
                && tool_call.title == "Resuming subagent: atlas (explorer)"
        }),
        "expected resume ToolCall, got {tool_calls:?}"
    );
    assert!(
        tool_calls.iter().any(|tool_call| {
            tool_call.tool_call_id.0.as_ref() == "close-a" && tool_call.title == "Closing subagent"
        }),
        "expected close ToolCall, got {tool_calls:?}"
    );

    assert!(
        tool_updates.iter().any(|update| {
            update.tool_call_id.0.as_ref() == "spawn-a"
                && update.fields.status == Some(ToolCallStatus::InProgress)
                && update.fields.title.as_deref() == Some("Spawned subagent: atlas (explorer)")
                && update.fields.raw_output.is_some()
        }),
        "expected spawn update, got {tool_updates:?}"
    );
    assert!(
        tool_updates.iter().any(|update| {
            update.tool_call_id.0.as_ref() == "message-a"
                && update.fields.status == Some(ToolCallStatus::Completed)
                && update.fields.title.as_deref() == Some("Subagent replied: atlas (explorer)")
        }),
        "expected interaction completion update, got {tool_updates:?}"
    );
    assert!(
        tool_updates.iter().any(|update| {
            update.tool_call_id.0.as_ref() == "wait-a"
                && update.fields.status == Some(ToolCallStatus::Failed)
                && update.fields.title.as_deref() == Some("Subagent wait completed")
        }),
        "expected failed wait update from errored subagent, got {tool_updates:?}"
    );
    assert!(
        tool_updates.iter().any(|update| {
            update.tool_call_id.0.as_ref() == "resume-a"
                && update.fields.status == Some(ToolCallStatus::InProgress)
                && update.fields.title.as_deref() == Some("Resumed subagent: atlas (explorer)")
        }),
        "expected resume update, got {tool_updates:?}"
    );
    assert!(
        tool_updates.iter().any(|update| {
            update.tool_call_id.0.as_ref() == "close-a"
                && update.fields.status == Some(ToolCallStatus::Completed)
                && update.fields.title.as_deref() == Some("Closed subagent: atlas (explorer)")
        }),
        "expected close update, got {tool_updates:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_raw_response_items_surface_missing_tool_calls() -> anyhow::Result<()> {
    let (session_id, client, _, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["raw-tool-items".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert!(notifications.iter().any(|notification| matches!(
        &notification.update,
        SessionUpdate::ToolCall(tool_call)
            if tool_call.tool_call_id.0.as_ref() == "tool-search-a"
                && tool_call.title == "Search tools via server"
                && tool_call.status == ToolCallStatus::InProgress
    )));
    assert!(notifications.iter().any(|notification| matches!(
        &notification.update,
        SessionUpdate::ToolCallUpdate(update)
            if update.tool_call_id.0.as_ref() == "tool-search-a"
                && update.fields.status == Some(ToolCallStatus::Completed)
                && update.fields.title.as_deref() == Some("Tool search completed via server")
    )));
    assert!(notifications.iter().any(|notification| matches!(
        &notification.update,
        SessionUpdate::ToolCall(tool_call)
            if tool_call.tool_call_id.0.as_ref() == "image-a"
                && tool_call.title == "Generate image"
                && tool_call.status == ToolCallStatus::Completed
                && matches!(
                    tool_call.content.as_slice(),
                    [ToolCallContent::Content(Content {
                        content: ContentBlock::Text(TextContent { text, .. }),
                        ..
                    })] if text == "A compact interface mockup"
                )
    )));

    Ok(())
}

#[test]
fn test_agent_status_maps_to_tool_status() {
    assert_eq!(
        agent_status_to_tool_status(&AgentStatus::PendingInit),
        ToolCallStatus::InProgress
    );
    assert_eq!(
        agent_status_to_tool_status(&AgentStatus::Running),
        ToolCallStatus::InProgress
    );
    assert_eq!(
        agent_status_to_tool_status(&AgentStatus::Interrupted),
        ToolCallStatus::InProgress
    );
    assert_eq!(
        agent_status_to_tool_status(&AgentStatus::Completed(None)),
        ToolCallStatus::Completed
    );
    assert_eq!(
        agent_status_to_tool_status(&AgentStatus::Shutdown),
        ToolCallStatus::Completed
    );
    assert_eq!(
        agent_status_to_tool_status(&AgentStatus::Errored("boom".to_string())),
        ToolCallStatus::Failed
    );
    assert_eq!(
        agent_status_to_tool_status(&AgentStatus::NotFound),
        ToolCallStatus::Failed
    );
}

#[tokio::test]
async fn test_exec_approval_uses_available_decisions() -> anyhow::Result<()> {
    let session_id = SessionId::new("test");
    let client = Arc::new(StubClient::with_permission_responses(vec![
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new("denied"),
        )),
    ]));
    let session_client = SessionClient::with_client(session_id, client.clone(), Arc::default());
    let thread = Arc::new(StubCodexThread::new());
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let (message_tx, mut message_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut prompt_state = PromptState::new(
        "submission-id".to_string(),
        thread.clone(),
        message_tx,
        response_tx,
    );

    prompt_state.exec_approval(
        &session_client,
        ExecApprovalRequestEvent {
            call_id: "call-id".to_string(),
            approval_id: Some("approval-id".to_string()),
            turn_id: "turn-id".to_string(),
            command: vec!["echo".to_string(), "hi".to_string()],
            cwd: std::env::current_dir()?.try_into()?,
            reason: None,
            network_approval_context: None,
            proposed_execpolicy_amendment: None,
            proposed_network_policy_amendments: None,
            additional_permissions: None,
            available_decisions: Some(vec![ReviewDecision::Approved, ReviewDecision::Denied]),
            parsed_cmd: vec![ParsedCommand::Unknown {
                cmd: "echo hi".to_string(),
            }],
        },
    )?;

    let ThreadMessage::PermissionRequestResolved {
        submission_id,
        request_key,
        response,
    } = message_rx.recv().await.unwrap()
    else {
        panic!("expected permission resolution message");
    };
    assert_eq!(submission_id, "submission-id");
    prompt_state
        .handle_permission_request_resolved(&session_client, request_key, response)
        .await?;

    let requests = client.permission_requests.lock().unwrap();
    let request = requests.last().unwrap();
    let option_ids = request
        .options
        .iter()
        .map(|option| option.option_id.0.to_string())
        .collect::<Vec<_>>();
    assert_eq!(option_ids, vec!["approved", "denied"]);

    let ops = thread.ops.lock().unwrap();
    assert!(matches!(
        ops.last(),
        Some(Op::ExecApproval {
            id,
            turn_id,
            decision: ReviewDecision::Denied,
        }) if id == "approval-id" && turn_id.as_deref() == Some("turn-id")
    ));

    Ok(())
}

#[tokio::test]
async fn test_request_user_input_submits_user_input_answer() -> anyhow::Result<()> {
    let session_id = SessionId::new("test");
    let client = Arc::new(StubClient::with_permission_responses(vec![
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new("answer-0"),
        )),
    ]));
    let session_client = SessionClient::with_client(session_id, client.clone(), Arc::default());
    let thread = Arc::new(StubCodexThread::new());
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let (message_tx, mut message_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut prompt_state = PromptState::new(
        "submission-id".to_string(),
        thread.clone(),
        message_tx,
        response_tx,
    );

    prompt_state
        .request_user_input(
            &session_client,
            RequestUserInputEvent {
                call_id: "call-id".to_string(),
                turn_id: "turn-id".to_string(),
                questions: vec![RequestUserInputQuestion {
                    id: "confirm_path".to_string(),
                    header: "Confirm".to_string(),
                    question: "Continue?".to_string(),
                    is_other: false,
                    is_secret: false,
                    options: Some(vec![
                        codex_protocol::request_user_input::RequestUserInputQuestionOption {
                            label: "yes".to_string(),
                            description: "Continue".to_string(),
                        },
                        codex_protocol::request_user_input::RequestUserInputQuestionOption {
                            label: "no".to_string(),
                            description: "Stop".to_string(),
                        },
                    ]),
                }],
            },
        )
        .await?;

    let ThreadMessage::PermissionRequestResolved {
        submission_id,
        request_key,
        response,
    } = message_rx.recv().await.unwrap()
    else {
        panic!("expected permission resolution message");
    };
    assert_eq!(submission_id, "submission-id");

    prompt_state
        .handle_permission_request_resolved(&session_client, request_key, response)
        .await?;

    let ops = thread.ops.lock().unwrap();
    assert!(matches!(
        ops.last(),
        Some(Op::UserInputAnswer { id, response })
            if id == "turn-id"
                && response
                    .answers
                    .get("confirm_path")
                    .is_some_and(|answer| answer.answers == vec!["yes".to_string()])
    ));

    let notifications = client.notifications.lock().unwrap();
    assert!(notifications.iter().any(|notification| {
        matches!(
            &notification.update,
            SessionUpdate::ToolCallUpdate(update)
                if update.tool_call_id.0.as_ref() == "call-id"
                    && matches!(
                        update.fields.status,
                        Some(ToolCallStatus::Completed)
                    )
        )
    }));

    Ok(())
}

#[tokio::test]
async fn test_request_user_input_reject_interrupts_and_waits_for_message() -> anyhow::Result<()> {
    let session_id = SessionId::new("test");
    let client = Arc::new(StubClient::with_permission_responses(vec![
        RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled),
    ]));
    let session_client = SessionClient::with_client(session_id, client.clone(), Arc::default());
    let thread = Arc::new(StubCodexThread::new());
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    let (message_tx, mut message_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut prompt_state = PromptState::new(
        "submission-id".to_string(),
        thread.clone(),
        message_tx,
        response_tx,
    );

    prompt_state
        .request_user_input(
            &session_client,
            RequestUserInputEvent {
                call_id: "call-id".to_string(),
                turn_id: "turn-id".to_string(),
                questions: vec![RequestUserInputQuestion {
                    id: "confirm_path".to_string(),
                    header: "Confirm".to_string(),
                    question: "Continue?".to_string(),
                    is_other: false,
                    is_secret: false,
                    options: Some(vec![RequestUserInputQuestionOption {
                        label: "yes".to_string(),
                        description: "Continue".to_string(),
                    }]),
                }],
            },
        )
        .await?;

    let ThreadMessage::PermissionRequestResolved {
        submission_id,
        request_key,
        response,
    } = message_rx.recv().await.unwrap()
    else {
        panic!("expected permission resolution message");
    };
    assert_eq!(submission_id, "submission-id");

    prompt_state
        .handle_permission_request_resolved(&session_client, request_key, response)
        .await?;

    assert_eq!(response_rx.await??, StopReason::EndTurn);
    let ops = thread.ops.lock().unwrap();
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops.last(), Some(Op::Interrupt)));
    assert!(
        !ops.iter()
            .any(|op| matches!(op, Op::UserInputAnswer { .. })),
        "rejected questions should not submit empty answers"
    );
    drop(ops);

    let notifications = client.notifications.lock().unwrap();
    assert!(notifications.iter().any(|notification| matches!(
        &notification.update,
        SessionUpdate::ToolCallUpdate(ToolCallUpdate { fields, .. })
            if fields.status == Some(ToolCallStatus::Completed)
                && fields.title.as_deref() == Some("Waiting for user response")
    )));

    Ok(())
}

#[tokio::test]
async fn test_request_user_input_chains_questions_before_submitting_answers() -> anyhow::Result<()>
{
    let session_id = SessionId::new("test");
    let client = Arc::new(StubClient::with_permission_responses(vec![
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new("answer-0"),
        )),
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new("answer-1"),
        )),
    ]));
    let session_client = SessionClient::with_client(session_id, client.clone(), Arc::default());
    let thread = Arc::new(StubCodexThread::new());
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let (message_tx, mut message_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut prompt_state = PromptState::new(
        "submission-id".to_string(),
        thread.clone(),
        message_tx,
        response_tx,
    );

    prompt_state
        .request_user_input(
            &session_client,
            RequestUserInputEvent {
                call_id: "call-id".to_string(),
                turn_id: "turn-id".to_string(),
                questions: vec![
                    RequestUserInputQuestion {
                        id: "target".to_string(),
                        header: "Target".to_string(),
                        question: "Which file?".to_string(),
                        is_other: false,
                        is_secret: false,
                        options: Some(vec![
                            RequestUserInputQuestionOption {
                                label: "root".to_string(),
                                description: "Use the root file".to_string(),
                            },
                            RequestUserInputQuestionOption {
                                label: "npm".to_string(),
                                description: "Use the npm package file".to_string(),
                            },
                        ]),
                    },
                    RequestUserInputQuestion {
                        id: "depth".to_string(),
                        header: "Depth".to_string(),
                        question: "How detailed?".to_string(),
                        is_other: false,
                        is_secret: false,
                        options: Some(vec![
                            RequestUserInputQuestionOption {
                                label: "brief".to_string(),
                                description: "Keep it short".to_string(),
                            },
                            RequestUserInputQuestionOption {
                                label: "detailed".to_string(),
                                description: "Include more detail".to_string(),
                            },
                        ]),
                    },
                ],
            },
        )
        .await?;

    for expected_request_key in ["user-input:call-id:0", "user-input:call-id:1"] {
        let ThreadMessage::PermissionRequestResolved {
            submission_id,
            request_key,
            response,
        } = message_rx.recv().await.unwrap()
        else {
            panic!("expected permission resolution message");
        };
        assert_eq!(submission_id, "submission-id");
        assert_eq!(request_key, expected_request_key);

        prompt_state
            .handle_permission_request_resolved(&session_client, request_key, response)
            .await?;
    }

    let ops = thread.ops.lock().unwrap();
    assert_eq!(ops.len(), 1);
    assert!(matches!(
        ops.last(),
        Some(Op::UserInputAnswer { id, response })
            if id == "turn-id"
                && response
                    .answers
                    .get("target")
                    .is_some_and(|answer| answer.answers == vec!["root".to_string()])
                && response
                    .answers
                    .get("depth")
                    .is_some_and(|answer| answer.answers == vec!["detailed".to_string()])
    ));
    drop(ops);

    let requests = client.permission_requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.tool_call.fields.title.as_deref())
            .collect::<Vec<_>>(),
        vec![
            Some("Need user input: Target"),
            Some("Need user input: Depth"),
        ]
    );

    Ok(())
}

#[tokio::test]
async fn test_mcp_tool_approval_elicitation_routes_to_permission_request() -> anyhow::Result<()> {
    let session_id = SessionId::new("test");
    let client = Arc::new(StubClient::with_permission_responses(vec![
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(MCP_TOOL_APPROVAL_ALLOW_SESSION_OPTION_ID),
        )),
    ]));
    let session_client = SessionClient::with_client(session_id, client.clone(), Arc::default());
    let thread = Arc::new(StubCodexThread::new());
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let (message_tx, mut message_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut prompt_state = PromptState::new(
        "submission-id".to_string(),
        thread.clone(),
        message_tx,
        response_tx,
    );

    let request_id = format!("{MCP_TOOL_APPROVAL_REQUEST_ID_PREFIX}call-123");
    prompt_state
        .mcp_elicitation(
            &session_client,
            ElicitationRequestEvent {
                turn_id: Some("turn-id".to_string()),
                server_name: "test-server".to_string(),
                id: codex_protocol::mcp::RequestId::String(request_id.clone()),
                request: ElicitationRequest::Form {
                    meta: Some(serde_json::json!({
                        "codex_approval_kind": "mcp_tool_call",
                        "persist": ["session", "always"],
                        "connector_name": "Docs",
                        "tool_title": "search_docs",
                        "tool_description": "Search project documentation",
                        "tool_params_display": [
                            {
                                "display_name": "Query",
                                "name": "query",
                                "value": "approval flow"
                            }
                        ]
                    })),
                    message: "Allow Docs to run tool \"search_docs\"?".to_string(),
                    requested_schema: serde_json::json!({
                        "type": "object",
                        "properties": {}
                    }),
                },
            },
        )
        .await?;

    let ThreadMessage::PermissionRequestResolved {
        submission_id,
        request_key,
        response,
    } = message_rx.recv().await.unwrap()
    else {
        panic!("expected permission resolution message");
    };
    assert_eq!(submission_id, "submission-id");

    {
        let requests = client.permission_requests.lock().unwrap();
        let request = requests.last().unwrap();
        assert_eq!(request.tool_call.tool_call_id.0.as_ref(), "call-123");
        assert_eq!(
            request
                .options
                .iter()
                .map(|option| option.option_id.0.to_string())
                .collect::<Vec<_>>(),
            vec![
                MCP_TOOL_APPROVAL_ALLOW_OPTION_ID.to_string(),
                MCP_TOOL_APPROVAL_ALLOW_SESSION_OPTION_ID.to_string(),
                MCP_TOOL_APPROVAL_ALLOW_ALWAYS_OPTION_ID.to_string(),
                MCP_TOOL_APPROVAL_CANCEL_OPTION_ID.to_string(),
            ]
        );
    }

    prompt_state
        .handle_permission_request_resolved(&session_client, request_key, response)
        .await?;

    let op = thread.ops.lock().unwrap().last().cloned().unwrap();
    match op {
        Op::ResolveElicitation {
            server_name,
            request_id: codex_protocol::mcp::RequestId::String(id),
            decision,
            content,
            meta,
        } => {
            assert_eq!(server_name, "test-server");
            assert_eq!(id, request_id);
            assert_eq!(decision, ElicitationAction::Accept);
            assert!(content.is_none());
            assert_eq!(
                meta.as_ref()
                    .and_then(|value| value.get("persist"))
                    .and_then(serde_json::Value::as_str),
                Some(MCP_TOOL_APPROVAL_PERSIST_SESSION)
            );
        }
        other => panic!("unexpected op: {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn test_mcp_elicitation_declines_unsupported_form_requests() -> anyhow::Result<()> {
    let session_id = SessionId::new("test");
    let client = Arc::new(StubClient::with_permission_responses(vec![
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new("decline"),
        )),
    ]));
    let session_client = SessionClient::with_client(session_id, client.clone(), Arc::default());
    let thread = Arc::new(StubCodexThread::new());
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let (message_tx, _message_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut prompt_state = PromptState::new(
        "submission-id".to_string(),
        thread.clone(),
        message_tx,
        response_tx,
    );

    prompt_state
        .mcp_elicitation(
            &session_client,
            ElicitationRequestEvent {
                turn_id: Some("turn-id".to_string()),
                server_name: "test-server".to_string(),
                id: codex_protocol::mcp::RequestId::String("request-id".to_string()),
                request: ElicitationRequest::Form {
                    meta: None,
                    message: "Need some structured input".to_string(),
                    requested_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" }
                        }
                    }),
                },
            },
        )
        .await?;

    let requests = client.permission_requests.lock().unwrap();
    assert!(
        requests.is_empty(),
        "unsupported MCP elicitations should be auto-declined"
    );

    let ops = thread.ops.lock().unwrap();
    assert!(matches!(
        ops.last(),
        Some(Op::ResolveElicitation {
            server_name,
            request_id: codex_protocol::mcp::RequestId::String(request_id),
            decision: ElicitationAction::Decline,
            content: None,
            meta: None,
        }) if server_name == "test-server" && request_id == "request-id"
    ));

    Ok(())
}

#[tokio::test]
async fn test_blocked_approval_does_not_block_followup_events() -> anyhow::Result<()> {
    let session_id = SessionId::new("test");
    let client = Arc::new(StubClient::with_blocked_permission_requests(
        vec![],
        Arc::new(Notify::new()),
    ));
    let session_client = SessionClient::with_client(session_id, client.clone(), Arc::default());
    let thread = Arc::new(StubCodexThread::new());
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let (message_tx, _message_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut prompt_state =
        PromptState::new("submission-id".to_string(), thread, message_tx, response_tx);

    prompt_state
        .handle_event(
            &session_client,
            EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
                call_id: "call-id".to_string(),
                approval_id: Some("approval-id".to_string()),
                turn_id: "turn-id".to_string(),
                command: vec!["echo".to_string(), "hi".to_string()],
                cwd: std::env::current_dir()?.try_into()?,
                reason: None,
                network_approval_context: None,
                proposed_execpolicy_amendment: None,
                proposed_network_policy_amendments: None,
                additional_permissions: None,
                available_decisions: Some(vec![ReviewDecision::Approved, ReviewDecision::Abort]),
                parsed_cmd: vec![ParsedCommand::Unknown {
                    cmd: "echo hi".to_string(),
                }],
            }),
        )
        .await;

    prompt_state
        .handle_event(
            &session_client,
            EventMsg::AgentMessage(AgentMessageEvent {
                message: "still flowing".to_string(),
                phase: None,
                memory_citation: None,
            }),
        )
        .await;

    let notifications = client.notifications.lock().unwrap();
    assert!(notifications.iter().any(|notification| {
        matches!(
            &notification.update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "still flowing"
        )
    }));

    drop(notifications);
    prompt_state.abort_pending_interactions();

    Ok(())
}

#[tokio::test]
async fn test_thread_shutdown_bypasses_blocked_permission_request() -> anyhow::Result<()> {
    let session_id = SessionId::new("test");
    let client = Arc::new(StubClient::with_blocked_permission_requests(
        vec![RequestPermissionResponse::new(
            RequestPermissionOutcome::Cancelled,
        )],
        Arc::new(Notify::new()),
    ));
    let session_client =
        SessionClient::with_client(session_id.clone(), client.clone(), Arc::default());
    let conversation = Arc::new(StubCodexThread::new());
    let models_manager = Arc::new(StubModelsManager);
    let config =
        Config::load_with_cli_overrides_and_harness_overrides(vec![], ConfigOverrides::default())
            .await?;
    let (message_tx, message_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resolution_tx, resolution_rx) = tokio::sync::mpsc::unbounded_channel();
    let actor = ThreadActor::new(
        StubAuth,
        session_client,
        conversation.clone(),
        models_manager,
        config,
        message_rx,
        resolution_tx,
        resolution_rx,
    );

    let handle = tokio::spawn(actor.spawn());
    let thread = Thread {
        thread: conversation.clone(),
        message_tx,
        _handle: handle,
    };

    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();
    thread.message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id, vec!["approval-block".into()]),
        response_tx: prompt_response_tx,
    })?;
    let stop_reason_rx = prompt_response_rx.await??;

    tokio::time::timeout(Duration::from_millis(100), async {
        loop {
            if !client.permission_requests.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;

    tokio::time::timeout(Duration::from_millis(100), thread.shutdown()).await??;
    let stop_reason = tokio::time::timeout(Duration::from_millis(100), stop_reason_rx).await??;
    assert_eq!(stop_reason?, StopReason::Cancelled);

    let ops = conversation.ops.lock().unwrap();
    assert!(matches!(ops.last(), Some(Op::Shutdown)));

    Ok(())
}
