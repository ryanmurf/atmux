use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)] // Independent ledger verification requirements.
pub struct Job {
    pub id: String,
    pub channel: String,
    pub session_key: String,
    pub goal: String,
    pub completion_criteria: String,
    pub folder: String,
    pub repo_remote: String,
    pub source_url: String,
    pub project_item_id: Option<String>,
    pub require_pr: bool,
    pub require_ci: bool,
    pub require_tests: bool,
    pub require_merged: bool,
    pub state: String,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobFact {
    Goal,
    CompletionCriteria,
    Folder,
    SourceUrl,
}
impl Job {
    pub(crate) fn fact(&self, fact: JobFact) -> &str {
        match fact {
            JobFact::Goal => &self.goal,
            JobFact::CompletionCriteria => &self.completion_criteria,
            JobFact::Folder => &self.folder,
            JobFact::SourceUrl => &self.source_url,
        }
    }
    pub(crate) fn active(&self) -> bool {
        matches!(
            self.state.as_str(),
            "CLAIMED" | "RUNNING" | "claimed" | "running"
        )
    }
}
/// No free-form model text can become an answer or permission approval.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "lowercase", deny_unknown_fields)]
pub enum Classification {
    Continue,
    Answer { fact: JobFact },
    Startup,
    Escalate,
}

/// # Errors
/// Rejects prose, fences, additional keys, unknown actions and oversized output.
pub fn classify(raw: &str) -> Result<Classification> {
    if raw.len() > 2048 {
        bail!("classification exceeds bounds");
    }
    let value: serde_json::Value = serde_json::from_str(raw)?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("classification must be an object"))?;
    let expected = if object.get("action").and_then(serde_json::Value::as_str) == Some("answer") {
        2
    } else {
        1
    };
    if object.len() != expected
        || object
            .keys()
            .any(|k| k != "action" && (expected != 2 || k != "fact"))
    {
        bail!("classification has unexpected fields");
    }
    Ok(serde_json::from_value(value)?)
}

/// Permissions default to escalation. Only an explicit, simple command in the
/// actual visible prompt can pass; model assertions of safety have no authority.
#[must_use]
pub fn permission_allowed(job: &Job, cwd: &str, prompt: &str) -> bool {
    let root = Path::new(&job.folder);
    let cwd = Path::new(cwd);
    if !root.is_absolute()
        || !cwd.is_absolute()
        || !cwd.starts_with(root)
        || [root, cwd]
            .iter()
            .any(|p| p.components().any(|c| matches!(c, Component::ParentDir)))
    {
        return false;
    }
    let lower = prompt.to_lowercase();
    if !lower.contains("allow") && !lower.contains("permission") {
        return false;
    }
    if [
        "force",
        "delete",
        "remove",
        "deploy",
        "production",
        "rm ",
        "rmdir",
        "exec ",
        "bash",
        "sh ",
        "python",
        "credential",
        "secret",
        "sudo",
        "keycloak",
        "kubectl",
        "terraform",
        "branch -d",
        "git push",
        "git reset",
        "git clean",
        "ignore previous",
        "system prompt",
        "supervisor",
        "curl",
        "wget",
    ]
    .iter()
    .any(|word| lower.contains(word))
    {
        return false;
    }
    let commands: Vec<_> = prompt
        .lines()
        .filter_map(|line| {
            let line = line
                .trim()
                .trim_start_matches("$ ")
                .trim_start_matches("Command: ");
            let words = shell_words::split(line).ok()?;
            let first = words.first()?;
            ["cargo", "git", "node", "pytest", "rg", "pwd"]
                .contains(&first.as_str())
                .then_some((line, words))
        })
        .collect();
    let [(line, words)] = commands.as_slice() else {
        return false;
    };
    if line.contains([';', '|', '&', '>', '<', '`', '$', '\\', '\n', '\r'])
        || words
            .iter()
            .any(|w| w.contains("..") || w.starts_with('/') || w.starts_with('~'))
    {
        return false;
    }
    // Fixed grammars avoid cargo aliases, git aliases, arbitrary scripts and
    // interpreter code. No write/launch/production commands are recognized.
    match words
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["cargo", "test" | "check" | "build" | "clippy", flags @ ..] => flags.iter().all(|w| {
            [
                "--all-features",
                "--all-targets",
                "--workspace",
                "--locked",
                "--offline",
                "--release",
            ]
            .contains(w)
        }),
        ["cargo", "fmt", "--check"] | ["pwd"] | ["git", "status" | "diff" | "log"] => true,
        _ => false,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Completion {
    Done {
        outcome: String,
        pr_url: Option<String>,
    },
    Blocked(String),
}
/// Only a top-level line in the latest main-agent turn matching this job counts.
#[must_use]
pub fn completion(turn: &str, job_id: &str) -> Option<Completion> {
    let done = format!("JOB DONE {job_id}: ");
    let blocked = format!("JOB BLOCKED {job_id}: ");
    let mut found = None;
    for line in turn.lines() {
        let candidate = if let Some(text) = line.strip_prefix(&done) {
            if text.is_empty() || text.len() > 2048 {
                return None;
            }
            let urls: Vec<_> = text
                .split_whitespace()
                .filter(|v| v.starts_with("https://github.com/"))
                .collect();
            if urls.len() > 1 {
                return None;
            }
            Some(Completion::Done {
                outcome: text.into(),
                pr_url: urls.first().map(|s| (*s).into()),
            })
        } else if let Some(text) = line.strip_prefix(&blocked) {
            if text.is_empty() || text.len() > 2048 {
                return None;
            }
            Some(Completion::Blocked(text.into()))
        } else {
            None
        };
        if candidate.is_some() {
            if found.is_some() {
                return None;
            }
            found = candidate;
        }
    }
    found
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PullRequest {
    pub url: String,
    pub repo_remote: String,
    pub state: String,
    pub ci_green: bool,
}
/// # Errors
/// Requires ledger criteria and GitHub evidence to agree. A closed unmerged PR
/// and pending/missing CI never count as done.
pub fn verify(
    job: &Job,
    outcome: &str,
    pr_url: Option<&str>,
    pr: Option<&PullRequest>,
) -> Result<()> {
    if job.require_tests && !outcome.contains("TESTS PASSED") {
        bail!("tests were not reported (TESTS PASSED required)");
    }
    if job.require_pr || job.require_ci || job.require_merged || pr_url.is_some() {
        let pr = pr.ok_or_else(|| anyhow::anyhow!("PR verification unavailable"))?;
        if pr_url != Some(pr.url.as_str())
            || repo(&job.repo_remote).is_none()
            || repo(&job.repo_remote) != repo(&pr.repo_remote)
            || !matches!(pr.state.as_str(), "OPEN" | "MERGED")
            || (job.require_merged && pr.state != "MERGED")
            || (job.require_ci && !pr.ci_green)
        {
            bail!("PR repository, state or CI does not meet criteria");
        }
    }
    Ok(())
}
pub(crate) fn repo(remote: &str) -> Option<String> {
    let path = if let Some(path) = remote.strip_prefix("git@github.com:") {
        path
    } else {
        remote.strip_prefix("https://github.com/")?
    };
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|s| {
            s.is_empty()
                || !s
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
    {
        return None;
    }
    Some(path.to_ascii_lowercase())
}
