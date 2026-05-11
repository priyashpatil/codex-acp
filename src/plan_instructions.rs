pub(crate) const ACP_TASK_PLAN_INSTRUCTIONS: &str = r#"ACP clients render `update_plan` calls as the visible task list. When you create or update a plan, keep that task list synchronized with actual progress: update statuses as work advances, and before sending a final answer emit one final `update_plan` call reflecting the final state. Do not leave completed work as `in_progress` or `pending`; only leave items pending when they are genuinely unresolved and called out in the final answer."#;

pub(crate) fn with_acp_task_plan_instructions(existing: Option<String>) -> String {
    match existing {
        Some(existing) if existing.contains(ACP_TASK_PLAN_INSTRUCTIONS) => existing,
        Some(existing) if existing.trim().is_empty() => ACP_TASK_PLAN_INSTRUCTIONS.to_string(),
        Some(existing) => format!("{}\n\n{}", existing.trim_end(), ACP_TASK_PLAN_INSTRUCTIONS),
        None => ACP_TASK_PLAN_INSTRUCTIONS.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_task_plan_instructions_to_existing_developer_instructions() {
        let merged = with_acp_task_plan_instructions(Some("Existing instructions.".to_string()));

        assert!(merged.starts_with("Existing instructions.\n\n"));
        assert!(merged.contains(ACP_TASK_PLAN_INSTRUCTIONS));
    }

    #[test]
    fn does_not_duplicate_task_plan_instructions() {
        let merged = with_acp_task_plan_instructions(Some(ACP_TASK_PLAN_INSTRUCTIONS.to_string()));

        assert_eq!(merged, ACP_TASK_PLAN_INSTRUCTIONS);
    }
}
