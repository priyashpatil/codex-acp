use std::sync::LazyLock;

use super::*;

pub(super) static APPROVAL_PRESETS: LazyLock<Vec<ApprovalPreset>> =
    LazyLock::new(builtin_approval_presets);

pub(super) const CODEX_READ_ONLY_PROFILE_ID: &str = ":read-only";
pub(super) const CODEX_WORKSPACE_PROFILE_ID: &str = ":workspace";
pub(super) const CODEX_DANGER_NO_SANDBOX_PROFILE_ID: &str = ":danger-no-sandbox";

fn session_mode_id_for_active_profile(profile_id: &str) -> Option<&'static str> {
    match profile_id {
        CODEX_READ_ONLY_PROFILE_ID => Some("read-only"),
        CODEX_WORKSPACE_PROFILE_ID => Some("auto"),
        CODEX_DANGER_NO_SANDBOX_PROFILE_ID => Some("full-access"),
        _ => None,
    }
}

pub(super) fn active_profile_id_for_session_mode(mode_id: &str) -> Option<&'static str> {
    match mode_id {
        "read-only" => Some(CODEX_READ_ONLY_PROFILE_ID),
        "auto" => Some(CODEX_WORKSPACE_PROFILE_ID),
        "full-access" => Some(CODEX_DANGER_NO_SANDBOX_PROFILE_ID),
        _ => None,
    }
}

pub(super) fn default_permissions_for_session_mode(mode_id: &str) -> Option<&'static str> {
    active_profile_id_for_session_mode(mode_id)
}

fn approval_matches_current_config(preset: &ApprovalPreset, config: &Config) -> bool {
    std::mem::discriminant(&preset.approval)
        == std::mem::discriminant(config.permissions.approval_policy.get())
}

fn untrusted_read_only_mode_id(config: &Config) -> Option<SessionModeId> {
    // When the project is untrusted, the approval policy won't match since
    // AskForApproval::UnlessTrusted is not part of the default presets.
    // However, we still want to show the mode selector, which allows the user
    // to choose a different mode and trust the project.
    config
        .active_project
        .is_untrusted()
        .then(|| SessionModeId::new("read-only"))
}

fn semantic_session_mode_id_for_permission_profile(config: &Config) -> Option<&'static str> {
    let permission_profile = config.permissions.permission_profile.get();

    match permission_profile {
        PermissionProfile::Managed { .. } => {
            let workspace_preset = APPROVAL_PRESETS.iter().find(|preset| preset.id == "auto")?;
            if permission_profile.network_sandbox_policy()
                != workspace_preset.permission_profile.network_sandbox_policy()
            {
                return None;
            }

            let file_system = permission_profile.file_system_sandbox_policy();
            let cwd = config.cwd.as_path();
            if file_system.has_full_disk_read_access()
                && !file_system.has_full_disk_write_access()
                && file_system.can_write_path_with_cwd(cwd, cwd)
            {
                Some("auto")
            } else {
                None
            }
        }
        PermissionProfile::Disabled => Some("full-access"),
        PermissionProfile::External { .. } => None,
    }
}

pub(super) fn current_session_mode_id(config: &Config) -> Option<SessionModeId> {
    if let Some(active_profile) = config.permissions.active_permission_profile().as_ref() {
        if let Some(mode_id) = session_mode_id_for_active_profile(&active_profile.id) {
            return Some(SessionModeId::new(mode_id));
        }
    }

    if let Some(preset) = APPROVAL_PRESETS.iter().find(|preset| {
        approval_matches_current_config(preset, config)
            && &preset.permission_profile == config.permissions.permission_profile.get()
    }) {
        return Some(SessionModeId::new(preset.id));
    }

    semantic_session_mode_id_for_permission_profile(config)
        .map(SessionModeId::new)
        .or_else(|| untrusted_read_only_mode_id(config))
}

pub(super) fn mode_trusts_project(mode_id: &str) -> bool {
    matches!(mode_id, "auto" | "full-access")
}

pub(super) fn format_service_tier_name(service_tier: Option<ServiceTier>) -> &'static str {
    match service_tier {
        Some(ServiceTier::Fast) => "Fast",
        Some(ServiceTier::Flex) => "Flex",
        None => "Standard",
    }
}
