use std::path::Path;

use super::*;

pub(super) fn builtin_commands() -> Vec<AvailableCommand> {
    vec![
        AvailableCommand::new("review", "Review my current changes and find issues").input(
            AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(
                "optional custom review instructions",
            )),
        ),
        AvailableCommand::new(
            "review-branch",
            "Review the code changes against a specific branch",
        )
        .input(AvailableCommandInput::Unstructured(
            UnstructuredCommandInput::new("branch name"),
        )),
        AvailableCommand::new(
            "review-commit",
            "Review the code changes introduced by a commit",
        )
        .input(AvailableCommandInput::Unstructured(
            UnstructuredCommandInput::new("commit sha"),
        )),
        AvailableCommand::new(
            "init",
            "create an AGENTS.md file with instructions for Codex",
        ),
        AvailableCommand::new(
            "compact",
            "summarize conversation to prevent hitting the context limit",
        ),
        AvailableCommand::new("logout", "logout of Codex"),
        AvailableCommand::new("fast", "toggle fast mode for this session").input(
            AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(
                "optional: on|off|status",
            )),
        ),
        AvailableCommand::new("diff", "show git diff including untracked files"),
        AvailableCommand::new(
            "status",
            "show current session configuration and token usage",
        ),
        AvailableCommand::new("stop", "stop all background terminals"),
        AvailableCommand::new("mention", "send a file mention to Codex").input(
            AvailableCommandInput::Unstructured(UnstructuredCommandInput::new("file path")),
        ),
        AvailableCommand::new("feedback", "show feedback guidance for this ACP agent"),
        AvailableCommand::new("mcp", "list configured MCP servers"),
        AvailableCommand::new("skills", "list available Codex skills"),
        AvailableCommand::new(
            "debug-config",
            "show session configuration details for debugging",
        ),
    ]
}

pub(super) fn skill_commands(skills: &[SkillMetadata]) -> Vec<AvailableCommand> {
    skills
        .iter()
        .filter(|skill| skill.enabled)
        .map(|skill| {
            AvailableCommand::new(
                format!("skills:{}", skill.name),
                skill
                    .short_description
                    .clone()
                    .or_else(|| {
                        skill
                            .interface
                            .as_ref()
                            .and_then(|interface| interface.short_description.clone())
                    })
                    .unwrap_or_else(|| skill.description.clone()),
            )
            .input(AvailableCommandInput::Unstructured(
                UnstructuredCommandInput::new("optional additional instructions"),
            ))
        })
        .collect()
}

pub(super) fn format_skills(skills: &[SkillMetadata]) -> String {
    if skills.is_empty() {
        return "No Codex skills available.\n".to_string();
    }

    let mut output = String::from("## Codex Skills\n\n");
    for skill in skills.iter().sorted_by_key(|skill| skill.name.as_str()) {
        let state = if skill.enabled { "enabled" } else { "disabled" };
        output.push_str(&format!(
            "- `{}` ({state}) - {}\n",
            skill.name,
            skill.path.display()
        ));
    }
    output
}

pub(super) async fn run_git_diff(cwd: &Path) -> String {
    let cwd = cwd.to_path_buf();
    match tokio::task::spawn_blocking(move || {
        let mut output = String::new();

        append_git_output(&mut output, &cwd, "Staged Changes", &["diff", "--cached"]);
        append_git_output(&mut output, &cwd, "Unstaged Changes", &["diff"]);

        if let Ok(result) = std::process::Command::new("git")
            .args(["ls-files", "--others", "--exclude-standard"])
            .current_dir(&cwd)
            .output()
        {
            let untracked = String::from_utf8_lossy(&result.stdout);
            if !untracked.trim().is_empty() {
                output.push_str("## Untracked Files\n\n");
                for file in untracked.lines() {
                    output.push_str(&format!("- {file}\n"));
                }
                output.push('\n');
            }
        }

        if output.trim().is_empty() {
            "No git changes.\n".to_string()
        } else {
            output
        }
    })
    .await
    {
        Ok(output) => output,
        Err(err) => format!("Failed to collect git diff: {err}\n"),
    }
}

fn append_git_output(output: &mut String, cwd: &Path, title: &str, args: &[&str]) {
    let Ok(result) = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
    else {
        return;
    };

    let stdout = String::from_utf8_lossy(&result.stdout);
    if stdout.trim().is_empty() {
        return;
    }

    output.push_str(&format!("## {title}\n\n```diff\n{stdout}```\n\n"));
}

/// Checks if a prompt is slash command.
pub(super) fn extract_slash_command(content: &[UserInput]) -> Option<(&str, &str)> {
    let line = content.first().and_then(|block| match block {
        UserInput::Text { text, .. } => Some(text),
        _ => None,
    })?;
    let stripped = line.strip_prefix('/')?;
    let mut name_end = stripped.len();
    for (idx, ch) in stripped.char_indices() {
        if ch.is_whitespace() {
            name_end = idx;
            break;
        }
    }
    let name = &stripped[..name_end];
    if name.is_empty() {
        return None;
    }
    let rest = stripped[name_end..].trim_start();
    Some((name, rest))
}
