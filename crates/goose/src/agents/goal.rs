//! Durable, evidence-backed goals for interactive coding sessions.
//!
//! Goal state lives in the Goose session rather than in an `Agent` mutex so it
//! survives process restarts and session resume.  Completion is deliberately a
//! two-key decision: the model must declare `GOAL_STATUS: complete`, and the
//! persisted conversation must contain a successful verification tool result.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::conversation::message::{Message, MessageContent, ToolRequest};
use crate::conversation::Conversation;
use crate::session::{ExtensionData, ExtensionState, Session};

const MAX_COMMAND_CHARS: usize = 500;
const VERIFICATION_SNAPSHOT_OPERATION: &str = "exactcode_goal";
const VERIFICATION_SNAPSHOT_KEY: &str = "verification_snapshots_v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Verified,
    Blocked,
    Abandoned,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct GoalBaseline {
    pub working_dir: String,
    pub git_root: Option<String>,
    pub base_sha: Option<String>,
    pub branch: Option<String>,
    pub dirty: bool,
    pub captured_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationEvidence {
    pub tool: String,
    pub command: Option<String>,
    pub passed: bool,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub error: Option<String>,
    /// Canonical repository and worktree state captured when the tool returned.
    /// Optional fields preserve compatibility with sessions written before v1
    /// bound verification evidence to a repository snapshot.
    #[serde(default)]
    pub git_root: Option<String>,
    #[serde(default)]
    pub worktree_sha256: Option<String>,
    pub recorded_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
struct VerificationSnapshot {
    #[serde(default)]
    git_root: Option<String>,
    #[serde(default)]
    worktree_sha256: Option<String>,
    captured_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct GoalEvidence {
    pub changed_files: Vec<String>,
    pub diff_sha256: Option<String>,
    pub verification: Vec<VerificationEvidence>,
    #[serde(default)]
    pub verification_diff_sha256: Option<String>,
    #[serde(default)]
    pub stale_reason: Option<String>,
    pub checked_at: Option<i64>,
}

impl GoalEvidence {
    fn effective_verification(&self) -> Vec<&VerificationEvidence> {
        let mut seen = HashSet::new();
        let mut latest = self
            .verification
            .iter()
            .rev()
            .filter(|check| seen.insert((check.tool.as_str(), check.command.as_deref())))
            .collect::<Vec<_>>();
        latest.reverse();
        latest
    }

    pub fn objective_ok(&self) -> bool {
        let checks = self.effective_verification();
        self.stale_reason.is_none() && !checks.is_empty() && checks.iter().all(|check| check.passed)
    }

    pub fn failure_reason(&self) -> String {
        if let Some(reason) = &self.stale_reason {
            return reason.clone();
        }
        let checks = self.effective_verification();
        if checks.is_empty() {
            return "no successful test, lint, typecheck, or build tool result was recorded"
                .to_string();
        }
        let failed = checks
            .into_iter()
            .filter(|check| !check.passed)
            .map(|check| {
                check.error.as_deref().map_or_else(
                    || match check.exit_code {
                        Some(code) => format!("{} (exit {code})", check.tool),
                        None => check.tool.clone(),
                    },
                    |error| format!("{} ({error})", check.tool),
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("verification failed: {failed}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GoalState {
    pub objective: String,
    pub status: GoalStatus,
    #[serde(default)]
    pub baseline: GoalBaseline,
    #[serde(default)]
    pub evidence: GoalEvidence,
    #[serde(default)]
    pub started_at: i64,
    #[serde(default)]
    pub finished_at: Option<i64>,
    #[serde(default)]
    pub completion_attempts: u32,
    #[serde(default)]
    pub unresolved: Vec<String>,
}

impl ExtensionState for GoalState {
    const EXTENSION_NAME: &'static str = "exactcode_goal";
    const VERSION: &'static str = "v1";
}

impl GoalState {
    pub fn start(objective: impl Into<String>, working_dir: &Path) -> Self {
        let started_at = Utc::now().timestamp();
        Self {
            objective: objective.into(),
            status: GoalStatus::Active,
            baseline: capture_baseline(working_dir, started_at),
            evidence: GoalEvidence::default(),
            started_at,
            finished_at: None,
            completion_attempts: 0,
            unresolved: Vec::new(),
        }
    }

    pub fn from_session(session: &Session) -> Option<Self> {
        Self::from_extension_data(&session.extension_data)
    }

    pub fn write_to(&self, extension_data: &mut ExtensionData) -> anyhow::Result<()> {
        self.to_extension_data(extension_data)
    }

    pub fn is_active(&self) -> bool {
        self.status == GoalStatus::Active
    }

    pub fn pause(&mut self) -> bool {
        if self.status != GoalStatus::Active {
            return false;
        }
        self.status = GoalStatus::Paused;
        true
    }

    pub fn resume(&mut self) -> bool {
        if self.status != GoalStatus::Paused {
            return false;
        }
        self.status = GoalStatus::Active;
        self.finished_at = None;
        true
    }

    pub fn edit(&mut self, objective: impl Into<String>, working_dir: &Path) {
        let started_at = Utc::now().timestamp();
        self.objective = objective.into();
        self.status = GoalStatus::Active;
        self.baseline = capture_baseline(working_dir, started_at);
        self.evidence = GoalEvidence::default();
        self.started_at = started_at;
        self.finished_at = None;
        self.completion_attempts = 0;
        self.unresolved.clear();
    }

    pub fn invalidate_if_worktree_changed(&mut self, working_dir: &Path) -> bool {
        if self.status != GoalStatus::Verified {
            return false;
        }
        if !same_repository(&self.baseline, working_dir) {
            self.status = GoalStatus::Active;
            self.finished_at = None;
            self.evidence.stale_reason = Some(
                "verification is stale because the session moved to a different repository"
                    .to_string(),
            );
            self.unresolved = vec![self.evidence.failure_reason()];
            return true;
        }
        let (_, current_diff) = changed_files_and_hash(working_dir);
        if self.baseline.git_root.is_some()
            && current_diff == self.evidence.verification_diff_sha256
        {
            return false;
        }
        self.status = GoalStatus::Active;
        self.finished_at = None;
        self.evidence.stale_reason = Some(if self.baseline.git_root.is_some() {
            "verification is stale because the working tree changed after it passed".to_string()
        } else {
            "verification cannot be revalidated after resume outside a git repository".to_string()
        });
        self.unresolved = vec![self.evidence.failure_reason()];
        true
    }

    pub fn abandon(&mut self) {
        self.status = GoalStatus::Abandoned;
        self.finished_at = Some(Utc::now().timestamp());
    }

    pub fn block(&mut self, reason: impl Into<String>) {
        self.status = GoalStatus::Blocked;
        self.finished_at = Some(Utc::now().timestamp());
        self.unresolved = vec![reason.into()];
    }

    pub fn evaluate_completion(&mut self, working_dir: &Path, conversation: &Conversation) -> bool {
        self.completion_attempts = self.completion_attempts.saturating_add(1);
        self.evidence = collect_evidence(working_dir, conversation, self);
        if !same_repository(&self.baseline, working_dir) {
            self.evidence.stale_reason = Some(
                "completion evidence came from a different repository than the goal baseline"
                    .to_string(),
            );
            self.unresolved = vec![self.evidence.failure_reason()];
            return false;
        }
        if self.evidence.objective_ok() {
            self.status = GoalStatus::Verified;
            self.finished_at = Some(Utc::now().timestamp());
            self.unresolved.clear();
            true
        } else {
            self.unresolved = vec![self.evidence.failure_reason()];
            false
        }
    }

    pub fn render(&self) -> String {
        let status = match self.status {
            GoalStatus::Active => "ACTIVE",
            GoalStatus::Paused => "PAUSED",
            GoalStatus::Verified => "VERIFIED",
            GoalStatus::Blocked => "BLOCKED",
            GoalStatus::Abandoned => "ABANDONED",
        };
        let base = self
            .baseline
            .base_sha
            .as_deref()
            .unwrap_or("not a git repository");
        let effective_checks = self.evidence.effective_verification();
        let checks = if effective_checks.is_empty() {
            "none".to_string()
        } else {
            effective_checks
                .into_iter()
                .map(|check| match (check.passed, check.exit_code) {
                    (true, Some(code)) => format!("{} (pass, exit {code})", check.tool),
                    (false, Some(code)) => format!("{} (fail, exit {code})", check.tool),
                    (true, None) => format!("{} (pass)", check.tool),
                    (false, None) => format!("{} (fail)", check.tool),
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let files = if self.evidence.changed_files.is_empty() {
            "none".to_string()
        } else {
            self.evidence.changed_files.join(", ")
        };
        let unresolved = if self.unresolved.is_empty() {
            "none".to_string()
        } else {
            self.unresolved.join("; ")
        };
        format!(
            "**Verified goal**\n\n\
             - Status: {status}\n\
             - Objective: {}\n\
             - Baseline: {base}\n\
             - Changed files: {files}\n\
             - Verification: {checks}\n\
             - Diff SHA-256: {}\n\
             - Unresolved: {unresolved}",
            self.objective,
            self.evidence
                .diff_sha256
                .as_deref()
                .unwrap_or("not captured")
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalVerdict {
    Complete,
    Blocked,
    Continue,
    Unspecified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalCommand<'a> {
    Inspect,
    Start(&'a str),
    Edit(&'a str),
    Pause,
    Resume,
    Abandon,
    Clear,
    Invalid(&'static str),
}

pub fn parse_goal_command(params: &str) -> GoalCommand<'_> {
    let params = params.trim();
    if params.is_empty() {
        return GoalCommand::Inspect;
    }
    match params {
        "pause" => GoalCommand::Pause,
        "resume" => GoalCommand::Resume,
        "off" | "none" => GoalCommand::Abandon,
        "clear" => GoalCommand::Clear,
        "edit" => GoalCommand::Invalid("usage: /goal edit <new objective>"),
        _ => params
            .strip_prefix("edit ")
            .map(str::trim)
            .filter(|objective| !objective.is_empty())
            .map_or(GoalCommand::Start(params), GoalCommand::Edit),
    }
}

pub fn goal_command_starts_turn(params: &str) -> bool {
    matches!(parse_goal_command(params), GoalCommand::Start(_))
}

pub fn parse_verdict(text: &str) -> GoalVerdict {
    for line in text.lines().rev() {
        let line = line.trim();
        let Some((label, value)) = line.split_once(':') else {
            continue;
        };
        if label.trim().to_ascii_lowercase().replace([' ', '-'], "_") != "goal_status" {
            continue;
        }
        match value.trim().to_ascii_lowercase().as_str() {
            "complete" => return GoalVerdict::Complete,
            "blocked" => return GoalVerdict::Blocked,
            "continue" => return GoalVerdict::Continue,
            _ => {}
        }
    }
    GoalVerdict::Unspecified
}

pub fn kickoff_prompt(goal: &GoalState) -> String {
    format!(
        "Begin the ExactCode verified-execution goal now.\n\n\
         **Goal:** {}\n\n\
         Work from current repository evidence, preserve unrelated changes, and use the \
         available ExactCode tools for edits. Before claiming completion, run the relevant \
         test, lint, typecheck, or build tool. End each attempted finish with exactly one of:\n\
         `GOAL_STATUS: complete`, `GOAL_STATUS: continue`, or `GOAL_STATUS: blocked`.\n\n\
         A completion claim without a successful verification result will be rejected.",
        goal.objective
    )
}

pub fn verification_nudge(goal: &GoalState) -> String {
    let gap = goal
        .unresolved
        .first()
        .cloned()
        .unwrap_or_else(|| "completion has not been proven".to_string());
    format!(
        "The verified-execution goal is still ACTIVE.\n\n\
         **Goal:** {}\n\n\
         Evidence gap: {gap}. Continue working or run an appropriate verification tool. \
         Do not claim completion until objective evidence passes. End with a `GOAL_STATUS` line.",
        goal.objective
    )
}

fn run_git(working_dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let mut command = crate::subprocess::git_command();
    let output = command
        .arg("-C")
        .arg(working_dir)
        .args(args)
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

fn trimmed_utf8(bytes: Vec<u8>) -> Option<String> {
    let value = String::from_utf8_lossy(&bytes).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn canonical_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn git_root(working_dir: &Path) -> Option<PathBuf> {
    run_git(working_dir, &["rev-parse", "--show-toplevel"])
        .and_then(trimmed_utf8)
        .map(PathBuf::from)
        .map(|root| canonical_path(&root))
}

fn same_repository(baseline: &GoalBaseline, working_dir: &Path) -> bool {
    let baseline_root = baseline
        .git_root
        .as_deref()
        .map(Path::new)
        .map(canonical_path);
    baseline_root == git_root(working_dir)
}

fn capture_baseline(working_dir: &Path, captured_at: i64) -> GoalBaseline {
    let git_root = git_root(working_dir).map(|root| root.to_string_lossy().to_string());
    let base_sha = run_git(working_dir, &["rev-parse", "HEAD"]).and_then(trimmed_utf8);
    let branch = run_git(working_dir, &["branch", "--show-current"]).and_then(trimmed_utf8);
    let dirty = run_git(working_dir, &["status", "--porcelain=v1"])
        .is_some_and(|status| !status.is_empty());
    GoalBaseline {
        working_dir: working_dir.to_string_lossy().to_string(),
        git_root,
        base_sha,
        branch,
        dirty,
        captured_at,
    }
}

fn changed_files_and_hash(working_dir: &Path) -> (Vec<String>, Option<String>) {
    let Some(root) = run_git(working_dir, &["rev-parse", "--show-toplevel"])
        .and_then(trimmed_utf8)
        .map(PathBuf::from)
    else {
        return (Vec::new(), None);
    };
    let Some(status) = run_git(&root, &["status", "--porcelain=v1", "-z"]) else {
        return (Vec::new(), None);
    };
    let tracked = run_git(&root, &["diff", "--name-only", "-z", "HEAD"]).unwrap_or_default();
    let untracked =
        run_git(&root, &["ls-files", "--others", "--exclude-standard", "-z"]).unwrap_or_default();
    let mut files = tracked
        .split(|byte| *byte == 0)
        .chain(untracked.split(|byte| *byte == 0))
        .filter(|entry| !entry.is_empty())
        .map(|entry| String::from_utf8_lossy(entry).to_string())
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();

    let mut hasher = Sha256::new();
    if let Some(head) = run_git(&root, &["rev-parse", "HEAD"]) {
        hasher.update(b"HEAD\0");
        hasher.update(head);
    }
    if let Some(branch) = run_git(&root, &["symbolic-ref", "--quiet", "HEAD"]) {
        hasher.update(b"BRANCH\0");
        hasher.update(branch);
    }
    hasher.update(b"STATUS\0");
    hasher.update(&status);
    if let Some(diff) = run_git(&root, &["diff", "--no-ext-diff", "--binary", "HEAD"]) {
        hasher.update(b"DIFF\0");
        hasher.update(diff);
    }
    for name in untracked
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        hasher.update(name);
        let path = root.join(String::from_utf8_lossy(name).as_ref());
        if path.is_file() {
            let _ = hash_file(&path, &mut hasher);
        }
    }
    let digest = hasher.finalize();
    let digest = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    (files, Some(digest))
}

fn verification_snapshot(working_dir: &Path) -> VerificationSnapshot {
    VerificationSnapshot {
        git_root: git_root(working_dir).map(|root| root.to_string_lossy().to_string()),
        worktree_sha256: changed_files_and_hash(working_dir).1,
        captured_at: Utc::now().timestamp(),
    }
}

/// Bind a verification response to the exact repository state in which it ran.
///
/// This must be called as soon as the tool result is received. Capturing during
/// goal evaluation would allow an unrelated process to mutate the worktree in
/// the interval between the check returning and the completion claim.
pub fn bind_verification_snapshot(
    working_dir: &Path,
    request: &ToolRequest,
    response: &mut Message,
) {
    if is_verification_request(request).is_none()
        || !response
            .get_tool_response_ids()
            .contains(&request.id.as_str())
    {
        return;
    }
    let mut snapshots = response
        .metadata
        .operation_note(VERIFICATION_SNAPSHOT_OPERATION, VERIFICATION_SNAPSHOT_KEY)
        .and_then(|value| {
            serde_json::from_value::<HashMap<String, VerificationSnapshot>>(value.clone()).ok()
        })
        .unwrap_or_default();
    snapshots.insert(request.id.clone(), verification_snapshot(working_dir));
    if let Ok(value) = serde_json::to_value(snapshots) {
        response.metadata.set_operation_note(
            VERIFICATION_SNAPSHOT_OPERATION,
            VERIFICATION_SNAPSHOT_KEY,
            value,
        );
    }
}

fn response_verification_snapshot(
    message: &Message,
    response_id: &str,
) -> Option<VerificationSnapshot> {
    let value = message
        .metadata
        .operation_note(VERIFICATION_SNAPSHOT_OPERATION, VERIFICATION_SNAPSHOT_KEY)?;
    serde_json::from_value::<HashMap<String, VerificationSnapshot>>(value.clone())
        .ok()?
        .remove(response_id)
}

fn hash_file(path: &Path, hasher: &mut Sha256) -> io::Result<()> {
    let mut file = File::open(path)?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        hasher.update(&buffer[..read]);
    }
}

fn shell_command(request: &ToolRequest) -> Option<&str> {
    let call = request.tool_call.as_ref().ok()?;
    call.arguments
        .as_ref()?
        .get("command")
        .and_then(serde_json::Value::as_str)
}

fn executable_basename(token: &str) -> &str {
    token.rsplit(['/', '\\']).next().unwrap_or(token)
}

fn strip_executable_suffix(token: &str) -> &str {
    executable_basename(token)
        .strip_suffix(".exe")
        .unwrap_or(executable_basename(token))
}

fn is_assignment(token: &str) -> bool {
    token.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name
                .chars()
                .all(|character| character == '_' || character.is_ascii_alphanumeric())
    })
}

fn is_verification_segment(tokens: &[String]) -> bool {
    let mut at = 0;
    while tokens.get(at).is_some_and(|token| is_assignment(token)) {
        at += 1;
    }
    while let Some(wrapper) = tokens.get(at).map(|token| strip_executable_suffix(token)) {
        match wrapper {
            "command" | "exec" => at += 1,
            "env" => {
                at += 1;
                while tokens
                    .get(at)
                    .is_some_and(|token| token.starts_with('-') || is_assignment(token))
                {
                    at += 1;
                }
            }
            "timeout" => {
                at += 1;
                while tokens.get(at).is_some_and(|token| {
                    token.starts_with('-')
                        || token.chars().all(|character| {
                            character.is_ascii_digit() || ".smhd".contains(character)
                        })
                }) {
                    at += 1;
                }
            }
            "uv" | "poetry" | "pipenv"
                if tokens.get(at + 1).is_some_and(|value| value == "run") =>
            {
                at += 2
            }
            _ => break,
        }
    }
    let Some(program) = tokens
        .get(at)
        .map(|token| strip_executable_suffix(token).to_ascii_lowercase())
    else {
        return false;
    };
    let args = &tokens[at + 1..];
    if args.iter().any(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "--help"
                | "-h"
                | "--version"
                | "--collect-only"
                | "--co"
                | "--dry-run"
                | "--just-print"
                | "--question"
        )
    }) {
        return false;
    }
    let arg = |index: usize, expected: &str| {
        args.get(index)
            .is_some_and(|value| value.eq_ignore_ascii_case(expected))
    };
    let verification_target = |value: &str| {
        let basename = executable_basename(value);
        let stem = [".sh", ".py", ".js", ".ts", ".rb"]
            .iter()
            .find_map(|suffix| basename.strip_suffix(suffix))
            .unwrap_or(basename);
        stem.split([':', '-', '_']).any(|part| {
            matches!(
                part.to_ascii_lowercase().as_str(),
                "test" | "tests" | "check" | "verify" | "lint" | "typecheck" | "build"
            )
        })
    };
    match program.as_str() {
        "pytest" | "py.test" => !args.iter().any(|value| value == "-V"),
        "vitest" | "jest" => true,
        "tsc" => !args
            .iter()
            .any(|value| matches!(value.as_str(), "-v" | "-V" | "--showConfig")),
        "cargo" => {
            arg(0, "test")
                || arg(0, "clippy")
                || (arg(0, "fmt") && args.iter().any(|value| value == "--check"))
        }
        "go" => arg(0, "test"),
        "npm" | "pnpm" | "yarn" | "bun" => {
            arg(0, "test")
                || (arg(0, "run") && args.get(1).is_some_and(|value| verification_target(value)))
        }
        "ruff" => arg(0, "check"),
        "python" | "python3" | "python3.11" | "python3.12" => {
            (arg(0, "-m")
                && args.get(1).is_some_and(|value| {
                    matches!(
                        value.as_str(),
                        "pytest" | "compileall" | "mypy" | "tox" | "nox"
                    )
                }))
                || args.first().is_some_and(|value| {
                    (value.contains('/') || value.contains('\\')) && verification_target(value)
                })
        }
        "make" | "just" | "task" => {
            args.first().is_some_and(|value| verification_target(value))
                && !args.iter().any(|value| value == "-n")
        }
        "mise" => arg(0, "run") && args.get(1).is_some_and(|value| verification_target(value)),
        "tox" | "nox" => true,
        "gradle" | "gradlew" => args.iter().any(|value| verification_target(value)),
        "mvn" | "mvnw" => args.iter().any(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "test" | "verify" | "package"
            )
        }),
        "dotnet" => arg(0, "test") || arg(0, "build"),
        "bash" | "sh" | "zsh" => args.first().is_some_and(|value| {
            (value.contains('/') || value.contains('\\')) && verification_target(value)
        }),
        _ => {
            (tokens[at].contains('/') || tokens[at].contains('\\'))
                && verification_target(&tokens[at])
        }
    }
}

fn is_verification_command(command: &str) -> bool {
    if command.contains(['\n', '\r', '`'])
        || command.contains("$(")
        || command.contains("<(")
        || command.contains(">(")
    {
        return false;
    }
    let Ok(tokens) = shell_words::split(command) else {
        return false;
    };
    if tokens.is_empty() {
        return false;
    }
    let mut segment = Vec::new();
    for token in tokens {
        if matches!(token.as_str(), ";" | "&&" | "||" | "|" | "&") {
            if segment.is_empty() || !is_verification_segment(&segment) {
                return false;
            }
            segment.clear();
        } else {
            segment.push(token);
        }
    }
    !segment.is_empty() && is_verification_segment(&segment)
}

fn is_verification_request(request: &ToolRequest) -> Option<(String, Option<String>)> {
    let parts = request.tool_name_parts()?;
    let tool = parts.tool_name.to_ascii_lowercase();
    if matches!(
        tool.as_str(),
        "project.test" | "project.exactqa" | "project.lint" | "project.build"
    ) {
        return Some((tool, None));
    }
    if tool != "shell.exec" && tool != "shell" {
        return None;
    }
    let command = shell_command(request)?;
    is_verification_command(command).then(|| {
        (
            tool,
            Some(command.chars().take(MAX_COMMAND_CHARS).collect()),
        )
    })
}

fn is_potential_mutation(request: &ToolRequest) -> bool {
    let Some(parts) = request.tool_name_parts() else {
        return true;
    };
    let tool = parts.tool_name.to_ascii_lowercase();
    if matches!(tool.as_str(), "shell" | "shell.exec") {
        return true;
    }
    [
        "write", "edit", "patch", "delete", "remove", "move", "copy", "rename", "mkdir", "touch",
        "format", "fix", "commit", "checkout", "switch", "merge", "rebase", "reset", "apply",
        "restore", "clean",
    ]
    .iter()
    .any(|marker| tool.split(['.', '_']).any(|part| part == *marker))
}

fn result_evidence(result: &rmcp::model::CallToolResult) -> (bool, Option<i32>, Option<String>) {
    let exit_code = result
        .structured_content
        .as_ref()
        .and_then(|value| value.get("exit_code"))
        .and_then(serde_json::Value::as_i64)
        .and_then(|value| i32::try_from(value).ok());
    let passed = result.is_error != Some(true) && exit_code.is_none_or(|code| code == 0);
    let error = (!passed).then(|| {
        if let Some(code) = exit_code {
            return format!("exit {code}");
        }
        result
            .content
            .iter()
            .filter_map(|content| content.as_text().map(|text| text.text.as_str()))
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(MAX_COMMAND_CHARS)
            .collect::<String>()
    });
    (passed, exit_code, error.filter(|value| !value.is_empty()))
}

fn goal_message_start(conversation: &Conversation, goal: &GoalState) -> usize {
    conversation
        .messages()
        .iter()
        .rposition(|message| {
            if !message.is_user_visible() {
                return false;
            }
            let text = message.as_concat_text();
            let Some(params) = text
                .trim()
                .strip_prefix("/goal")
                .filter(|suffix| suffix.is_empty() || suffix.starts_with(char::is_whitespace))
                .map(str::trim)
            else {
                return false;
            };
            matches!(
                parse_goal_command(params),
                GoalCommand::Start(objective) | GoalCommand::Edit(objective)
                    if objective == goal.objective
            )
        })
        .unwrap_or_else(|| {
            conversation
                .messages()
                .iter()
                .position(|message| message.created >= goal.started_at)
                .unwrap_or(conversation.len())
        })
}

pub fn collect_evidence(
    working_dir: &Path,
    conversation: &Conversation,
    goal: &GoalState,
) -> GoalEvidence {
    let start = goal_message_start(conversation, goal);
    let messages = &conversation.messages()[start..];
    let mut requests: HashMap<String, (String, Option<String>, u64)> = HashMap::new();
    let mut recorded = Vec::new();
    let mut mutation_epoch = 0_u64;
    for message in messages {
        let request_epoch = mutation_epoch;
        let mut message_mutates = false;
        for content in &message.content {
            if let MessageContent::ToolRequest(request) = content {
                if let Some((tool, command)) = is_verification_request(request) {
                    requests.insert(request.id.clone(), (tool, command, request_epoch));
                } else if is_potential_mutation(request) {
                    message_mutates = true;
                }
            }
        }
        if message_mutates {
            mutation_epoch = mutation_epoch.saturating_add(1);
        }
        for content in &message.content {
            let MessageContent::ToolResponse(response) = content else {
                continue;
            };
            let Some((tool, command, request_epoch)) = requests.get(&response.id).cloned() else {
                continue;
            };
            let (passed, exit_code, error) = match response.tool_result.as_ref() {
                Ok(result) => result_evidence(result),
                Err(error) => (false, None, Some(error.to_string())),
            };
            let snapshot =
                response_verification_snapshot(message, &response.id).unwrap_or_default();
            recorded.push((
                request_epoch,
                VerificationEvidence {
                    tool,
                    command,
                    passed,
                    exit_code,
                    error,
                    git_root: snapshot.git_root,
                    worktree_sha256: snapshot.worktree_sha256,
                    recorded_at: message.created,
                },
            ));
        }
    }
    let had_stale_verification = recorded
        .iter()
        .any(|(request_epoch, _)| *request_epoch != mutation_epoch);
    let verification = recorded
        .into_iter()
        .filter_map(|(request_epoch, evidence)| {
            (request_epoch == mutation_epoch).then_some(evidence)
        })
        .collect::<Vec<_>>();
    let (changed_files, diff_sha256) = changed_files_and_hash(working_dir);
    let current_root = git_root(working_dir).map(|root| root.to_string_lossy().to_string());
    let stale_reason = if verification.is_empty() && had_stale_verification {
        Some(
            "verification is stale because a potentially mutating tool was requested after the last check"
                .to_string(),
        )
    } else if current_root.is_some()
        && verification.iter().any(|check| {
            check.passed
                && (check.git_root.as_ref() != current_root.as_ref()
                    || check.worktree_sha256.as_ref() != diff_sha256.as_ref())
        })
    {
        Some(
            "verification is stale because the repository changed after the check returned; rerun verification"
                .to_string(),
        )
    } else {
        None
    };
    GoalEvidence {
        changed_files,
        verification_diff_sha256: stale_reason
            .is_none()
            .then(|| diff_sha256.clone())
            .flatten(),
        diff_sha256,
        verification,
        stale_reason,
        checked_at: Some(Utc::now().timestamp()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::message::Message;
    use rmcp::model::{CallToolRequestParams, CallToolResult, JsonObject};
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn verdict_requires_explicit_protocol_line() {
        assert_eq!(
            parse_verdict("GOAL_STATUS: complete"),
            GoalVerdict::Complete
        );
        assert_eq!(parse_verdict("Goal status: blocked"), GoalVerdict::Blocked);
        assert_eq!(parse_verdict("looks complete"), GoalVerdict::Unspecified);
        for text in [
            "not_GOAL_STATUS: complete",
            "The requested GOAL_STATUS: complete",
            "GOAL_STATUS: complete because tests passed",
            "`GOAL_STATUS: complete`",
        ] {
            assert_eq!(parse_verdict(text), GoalVerdict::Unspecified, "{text:?}");
        }
    }

    #[test]
    fn successful_verification_tool_is_objective_evidence() {
        let dir = tempdir().unwrap();
        let mut goal = GoalState::start("ship it", dir.path());
        goal.started_at = 1;
        let arguments: JsonObject = json!({"command": "cargo test -p goose-cli"})
            .as_object()
            .unwrap()
            .clone();
        let request = Message::assistant().with_tool_request(
            "proof-1",
            Ok(CallToolRequestParams::new("exactcode__shell.exec").with_arguments(arguments)),
        );
        let response =
            Message::user().with_tool_response("proof-1", Ok(CallToolResult::success(vec![])));
        let conversation = Conversation::new_unvalidated(vec![request, response]);

        let evidence = collect_evidence(dir.path(), &conversation, &goal);

        assert!(evidence.objective_ok());
        assert_eq!(evidence.verification[0].tool, "shell.exec");
        assert_eq!(
            evidence.verification[0].command.as_deref(),
            Some("cargo test -p goose-cli")
        );
    }

    #[test]
    fn ordinary_successful_shell_call_is_not_completion_evidence() {
        let dir = tempdir().unwrap();
        let mut goal = GoalState::start("ship it", dir.path());
        goal.started_at = 1;
        let arguments: JsonObject = json!({"command": "ls"}).as_object().unwrap().clone();
        let request = Message::assistant().with_tool_request(
            "not-proof",
            Ok(CallToolRequestParams::new("exactcode__shell.exec").with_arguments(arguments)),
        );
        let response =
            Message::user().with_tool_response("not-proof", Ok(CallToolResult::success(vec![])));
        let conversation = Conversation::new_unvalidated(vec![request, response]);

        assert!(!collect_evidence(dir.path(), &conversation, &goal).objective_ok());
    }

    #[test]
    fn verification_requires_the_executed_program_not_a_substring() {
        let dir = tempdir().unwrap();
        let mut goal = GoalState::start("ship it", dir.path());
        goal.started_at = 1;
        for (index, command) in [
            "echo pytest",
            "printf 'cargo test'",
            "rg 'npm test' README.md",
            "sh -c 'pytest'",
        ]
        .into_iter()
        .enumerate()
        {
            let arguments: JsonObject = json!({"command": command}).as_object().unwrap().clone();
            let id = format!("not-proof-{index}");
            let request = Message::assistant().with_tool_request(
                id.clone(),
                Ok(CallToolRequestParams::new("exactcode__shell.exec").with_arguments(arguments)),
            );
            let response =
                Message::user().with_tool_response(id, Ok(CallToolResult::success(vec![])));
            let conversation = Conversation::new_unvalidated(vec![request, response]);
            assert!(
                collect_evidence(dir.path(), &conversation, &goal)
                    .verification
                    .is_empty(),
                "{command:?} must not count as verification"
            );
        }
    }

    #[test]
    fn verification_accepts_conservative_executable_forms() {
        for command in [
            "cargo test -p goose",
            "env RUST_BACKTRACE=1 cargo clippy --all-targets",
            "timeout 30s python3 -m pytest -q",
            "uv run ruff check .",
            "npm run typecheck",
            "npm run test:unit",
            "./scripts/verify.sh",
            "bash scripts/check.sh",
            "python3 scripts/typecheck.py",
            "task ci-test",
            "mise run verify:release",
            "./gradlew check",
            "mvn verify",
            "dotnet test",
        ] {
            assert!(is_verification_command(command), "{command:?}");
        }
    }

    #[test]
    fn verification_rejects_shell_constructs_that_can_hide_other_programs() {
        let hidden_suffix = format!("cargo test {} ; true", " ".repeat(MAX_COMMAND_CHARS));
        for command in [
            "cargo test\ntrue",
            "cargo test & true",
            "cargo test $(touch source.rs)",
            "cargo test `touch source.rs`",
            "cargo test --help",
            "pytest --collect-only",
            "make test -n",
            "echo ./scripts/verify.sh",
            "bash -c './scripts/verify.sh'",
            "./scripts/verify.sh --dry-run",
            hidden_suffix.as_str(),
        ] {
            assert!(!is_verification_command(command), "{command:?}");
        }
    }

    #[test]
    fn structured_nonzero_exit_is_failed_evidence() {
        let dir = tempdir().unwrap();
        let mut goal = GoalState::start("ship it", dir.path());
        goal.started_at = 1;
        let arguments: JsonObject = json!({"command": "cargo test"})
            .as_object()
            .unwrap()
            .clone();
        let request = Message::assistant().with_tool_request(
            "proof-exit",
            Ok(CallToolRequestParams::new("exactcode__shell.exec").with_arguments(arguments)),
        );
        let mut result = CallToolResult::success(vec![]);
        result.structured_content = Some(json!({"exit_code": 7}));
        let response = Message::user().with_tool_response("proof-exit", Ok(result));
        let conversation = Conversation::new_unvalidated(vec![request, response]);

        let evidence = collect_evidence(dir.path(), &conversation, &goal);

        assert!(!evidence.objective_ok());
        assert_eq!(evidence.verification[0].exit_code, Some(7));
        assert_eq!(evidence.verification[0].error.as_deref(), Some("exit 7"));
    }

    #[test]
    fn mutation_after_verification_makes_evidence_stale() {
        let dir = tempdir().unwrap();
        let mut goal = GoalState::start("ship it", dir.path());
        goal.started_at = 1;
        let proof_args: JsonObject = json!({"command": "cargo test"})
            .as_object()
            .unwrap()
            .clone();
        let edit_args: JsonObject = json!({"path": "src/lib.rs", "content": "changed"})
            .as_object()
            .unwrap()
            .clone();
        let conversation = Conversation::new_unvalidated(vec![
            Message::assistant().with_tool_request(
                "proof",
                Ok(CallToolRequestParams::new("exactcode__shell.exec").with_arguments(proof_args)),
            ),
            Message::user().with_tool_response("proof", Ok(CallToolResult::success(vec![]))),
            Message::assistant().with_tool_request(
                "edit",
                Ok(CallToolRequestParams::new("exactcode__filesystem.write")
                    .with_arguments(edit_args)),
            ),
            Message::user().with_tool_response("edit", Ok(CallToolResult::success(vec![]))),
        ]);

        let evidence = collect_evidence(dir.path(), &conversation, &goal);

        assert!(!evidence.objective_ok());
        assert!(evidence
            .failure_reason()
            .contains("potentially mutating tool was requested after"));
    }

    #[test]
    fn mutation_after_verification_request_but_before_response_is_stale() {
        let dir = tempdir().unwrap();
        let mut goal = GoalState::start("ship it", dir.path());
        goal.started_at = 1;
        let proof_args: JsonObject = json!({"command": "cargo test"})
            .as_object()
            .unwrap()
            .clone();
        let edit_args: JsonObject = json!({"path": "src/lib.rs", "content": "changed"})
            .as_object()
            .unwrap()
            .clone();
        let conversation = Conversation::new_unvalidated(vec![
            Message::assistant().with_tool_request(
                "proof",
                Ok(CallToolRequestParams::new("exactcode__shell.exec").with_arguments(proof_args)),
            ),
            Message::assistant().with_tool_request(
                "edit",
                Ok(CallToolRequestParams::new("exactcode__filesystem.write")
                    .with_arguments(edit_args)),
            ),
            Message::user().with_tool_response("edit", Ok(CallToolResult::success(vec![]))),
            Message::user().with_tool_response("proof", Ok(CallToolResult::success(vec![]))),
        ]);

        let evidence = collect_evidence(dir.path(), &conversation, &goal);

        assert!(!evidence.objective_ok());
        assert!(evidence.verification.is_empty());
        assert!(evidence.failure_reason().contains("potentially mutating"));
    }

    #[test]
    fn failed_verification_prevents_completion_even_after_a_pass() {
        let dir = tempdir().unwrap();
        let mut goal = GoalState::start("ship it", dir.path());
        goal.started_at = 1;
        let arguments: JsonObject = json!({"command": "cargo test"})
            .as_object()
            .unwrap()
            .clone();
        let passed_request = Message::assistant().with_tool_request(
            "proof-pass",
            Ok(CallToolRequestParams::new("exactcode__shell.exec")
                .with_arguments(arguments.clone())),
        );
        let passed_response =
            Message::user().with_tool_response("proof-pass", Ok(CallToolResult::success(vec![])));
        let failed_request = Message::assistant().with_tool_request(
            "proof-fail",
            Ok(CallToolRequestParams::new("exactcode__shell.exec").with_arguments(arguments)),
        );
        let failed_response =
            Message::user().with_tool_response("proof-fail", Ok(CallToolResult::error(vec![])));
        let conversation = Conversation::new_unvalidated(vec![
            passed_request,
            passed_response,
            failed_request,
            failed_response,
        ]);

        assert!(!collect_evidence(dir.path(), &conversation, &goal).objective_ok());
    }

    #[test]
    fn later_success_supersedes_an_earlier_failure_of_the_same_check() {
        let dir = tempdir().unwrap();
        let mut goal = GoalState::start("ship it", dir.path());
        goal.started_at = 1;
        let arguments: JsonObject = json!({"command": "cargo test"})
            .as_object()
            .unwrap()
            .clone();
        let failed_request = Message::assistant().with_tool_request(
            "proof-fail",
            Ok(CallToolRequestParams::new("exactcode__shell.exec")
                .with_arguments(arguments.clone())),
        );
        let failed_response =
            Message::user().with_tool_response("proof-fail", Ok(CallToolResult::error(vec![])));
        let passed_request = Message::assistant().with_tool_request(
            "proof-pass",
            Ok(CallToolRequestParams::new("exactcode__shell.exec").with_arguments(arguments)),
        );
        let passed_response =
            Message::user().with_tool_response("proof-pass", Ok(CallToolResult::success(vec![])));
        let conversation = Conversation::new_unvalidated(vec![
            failed_request,
            failed_response,
            passed_request,
            passed_response,
        ]);

        let evidence = collect_evidence(dir.path(), &conversation, &goal);

        assert!(evidence.objective_ok());
        assert_eq!(evidence.verification.len(), 2, "audit history is retained");
    }

    #[test]
    fn normalized_goal_command_excludes_same_second_prior_evidence() {
        let dir = tempdir().unwrap();
        let goal = GoalState::start("ship it", dir.path());
        let arguments: JsonObject = json!({"command": "cargo test"})
            .as_object()
            .unwrap()
            .clone();
        let mut request = Message::assistant().with_tool_request(
            "old-proof",
            Ok(CallToolRequestParams::new("exactcode__shell.exec").with_arguments(arguments)),
        );
        let mut response =
            Message::user().with_tool_response("old-proof", Ok(CallToolResult::success(vec![])));
        let mut command = Message::user().with_text("/goal    ship it");
        request.created = goal.started_at;
        response.created = goal.started_at;
        command.created = goal.started_at;
        let conversation = Conversation::new_unvalidated(vec![request, response, command]);

        assert!(collect_evidence(dir.path(), &conversation, &goal)
            .verification
            .is_empty());
    }

    #[test]
    fn git_baseline_and_diff_artifact_are_captured() {
        let dir = tempdir().unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "goose@example.test"]);
        git(&["config", "user.name", "Goose Test"]);
        std::fs::write(dir.path().join("proof.txt"), "baseline\n").unwrap();
        git(&["add", "proof.txt"]);
        git(&["commit", "-qm", "baseline"]);
        std::fs::create_dir(dir.path().join("nested")).unwrap();

        let goal = GoalState::start("verify the artifact", &dir.path().join("nested"));
        assert!(goal.baseline.base_sha.is_some());
        assert!(!goal.baseline.dirty);

        std::fs::write(dir.path().join("proof.txt"), "changed\n").unwrap();
        let evidence = collect_evidence(
            &dir.path().join("nested"),
            &Conversation::new_unvalidated(Vec::new()),
            &goal,
        );
        assert_eq!(evidence.changed_files, vec!["proof.txt"]);
        assert_eq!(evidence.diff_sha256.as_deref().map(str::len), Some(64));
    }

    #[test]
    fn verified_goal_reactivates_when_bound_diff_changes() {
        let dir = tempdir().unwrap();
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .status()
                .unwrap()
                .success());
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "goose@example.test"]);
        git(&["config", "user.name", "Goose Test"]);
        std::fs::write(dir.path().join("proof.txt"), "baseline\n").unwrap();
        git(&["add", "proof.txt"]);
        git(&["commit", "-qm", "baseline"]);

        let mut goal = GoalState::start("ship it", dir.path());
        goal.started_at = 1;
        let arguments: JsonObject = json!({"command": "cargo test"})
            .as_object()
            .unwrap()
            .clone();
        let request = Message::assistant().with_tool_request(
            "proof",
            Ok(CallToolRequestParams::new("exactcode__shell.exec").with_arguments(arguments)),
        );
        let mut response =
            Message::user().with_tool_response("proof", Ok(CallToolResult::success(vec![])));
        let MessageContent::ToolRequest(proof_request) = &request.content[0] else {
            unreachable!()
        };
        bind_verification_snapshot(dir.path(), proof_request, &mut response);
        let conversation = Conversation::new_unvalidated(vec![request, response]);
        assert!(goal.evaluate_completion(dir.path(), &conversation));

        std::fs::write(dir.path().join("proof.txt"), "changed after proof\n").unwrap();

        assert!(goal.invalidate_if_worktree_changed(dir.path()));
        assert!(goal.is_active());
        assert!(!goal.evidence.objective_ok());
        assert!(goal.render().contains("verification is stale"));
    }

    #[test]
    fn external_mutation_after_tool_response_rejects_completion() {
        let dir = tempdir().unwrap();
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .status()
                .unwrap()
                .success());
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "goose@example.test"]);
        git(&["config", "user.name", "Goose Test"]);
        std::fs::write(dir.path().join("proof.txt"), "baseline\n").unwrap();
        git(&["add", "proof.txt"]);
        git(&["commit", "-qm", "baseline"]);

        let mut goal = GoalState::start("ship it", dir.path());
        goal.started_at = 1;
        let arguments: JsonObject = json!({"command": "./scripts/verify.sh"})
            .as_object()
            .unwrap()
            .clone();
        let request = Message::assistant().with_tool_request(
            "proof",
            Ok(CallToolRequestParams::new("exactcode__shell.exec").with_arguments(arguments)),
        );
        let mut response =
            Message::user().with_tool_response("proof", Ok(CallToolResult::success(vec![])));
        let MessageContent::ToolRequest(proof_request) = &request.content[0] else {
            unreachable!()
        };
        bind_verification_snapshot(dir.path(), proof_request, &mut response);

        // Simulate an editor, hook, or another agent changing the checkout after
        // the verification process returned but before the model claimed done.
        std::fs::write(dir.path().join("proof.txt"), "changed externally\n").unwrap();
        let conversation = Conversation::new_unvalidated(vec![request, response]);

        assert!(!goal.evaluate_completion(dir.path(), &conversation));
        assert!(goal
            .evidence
            .failure_reason()
            .contains("repository changed after the check returned"));
        assert!(goal.evidence.verification[0].worktree_sha256.is_some());
    }

    #[test]
    fn bound_verification_snapshot_survives_message_serialization() {
        let dir = tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success());
        let arguments: JsonObject = json!({"command": "cargo test"})
            .as_object()
            .unwrap()
            .clone();
        let request = Message::assistant().with_tool_request(
            "proof",
            Ok(CallToolRequestParams::new("exactcode__shell.exec").with_arguments(arguments)),
        );
        let mut response =
            Message::user().with_tool_response("proof", Ok(CallToolResult::success(vec![])));
        let MessageContent::ToolRequest(proof_request) = &request.content[0] else {
            unreachable!()
        };
        bind_verification_snapshot(dir.path(), proof_request, &mut response);
        let response: Message =
            serde_json::from_value(serde_json::to_value(response).unwrap()).unwrap();
        let snapshot = response_verification_snapshot(&response, "proof").unwrap();

        assert_eq!(
            snapshot.git_root,
            git_root(dir.path()).map(|p| p.display().to_string())
        );
        assert!(snapshot.worktree_sha256.is_some());
    }

    #[test]
    fn verified_goal_reactivates_when_head_changes_with_a_clean_tree() {
        let dir = tempdir().unwrap();
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .status()
                .unwrap()
                .success());
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "goose@example.test"]);
        git(&["config", "user.name", "Goose Test"]);
        std::fs::write(dir.path().join("proof.txt"), "baseline\n").unwrap();
        git(&["add", "proof.txt"]);
        git(&["commit", "-qm", "baseline"]);

        let mut goal = GoalState::start("ship it", dir.path());
        goal.status = GoalStatus::Verified;
        goal.evidence.verification_diff_sha256 = changed_files_and_hash(dir.path()).1;
        std::fs::write(dir.path().join("proof.txt"), "next\n").unwrap();
        git(&["add", "proof.txt"]);
        git(&["commit", "-qm", "next"]);

        assert!(goal.invalidate_if_worktree_changed(dir.path()));
    }

    #[test]
    fn active_goal_rejects_completion_evidence_from_a_nested_repository() {
        let dir = tempdir().unwrap();
        let git = |directory: &Path, args: &[&str]| {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(directory)
                .args(args)
                .status()
                .unwrap()
                .success());
        };
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["config", "user.email", "goose@example.test"]);
        git(dir.path(), &["config", "user.name", "Goose Test"]);
        std::fs::write(dir.path().join("outer.txt"), "outer\n").unwrap();
        git(dir.path(), &["add", "outer.txt"]);
        git(dir.path(), &["commit", "-qm", "outer"]);
        let mut goal = GoalState::start("ship the outer repository", dir.path());
        goal.started_at = 1;

        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        git(&nested, &["init", "-q"]);
        git(&nested, &["config", "user.email", "goose@example.test"]);
        git(&nested, &["config", "user.name", "Goose Test"]);
        std::fs::write(nested.join("nested.txt"), "nested\n").unwrap();
        git(&nested, &["add", "nested.txt"]);
        git(&nested, &["commit", "-qm", "nested"]);

        let arguments: JsonObject = json!({"command": "cargo test"})
            .as_object()
            .unwrap()
            .clone();
        let conversation = Conversation::new_unvalidated(vec![
            Message::assistant().with_tool_request(
                "nested-proof",
                Ok(CallToolRequestParams::new("exactcode__shell.exec").with_arguments(arguments)),
            ),
            Message::user().with_tool_response("nested-proof", Ok(CallToolResult::success(vec![]))),
        ]);

        assert!(!goal.evaluate_completion(&nested, &conversation));
        assert!(goal.is_active());
        assert!(goal
            .evidence
            .failure_reason()
            .contains("different repository than the goal baseline"));
    }

    #[test]
    fn verified_goal_reactivates_after_resume_in_a_nested_repository() {
        let dir = tempdir().unwrap();
        let git = |directory: &Path, args: &[&str]| {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(directory)
                .args(args)
                .status()
                .unwrap()
                .success());
        };
        git(dir.path(), &["init", "-q"]);
        let mut goal = GoalState::start("ship the outer repository", dir.path());
        goal.status = GoalStatus::Verified;
        goal.evidence.verification_diff_sha256 = changed_files_and_hash(dir.path()).1;

        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        git(&nested, &["init", "-q"]);

        assert!(goal.invalidate_if_worktree_changed(&nested));
        assert!(goal.is_active());
        assert!(goal
            .evidence
            .failure_reason()
            .contains("moved to a different repository"));
    }

    #[test]
    fn non_git_verified_goal_is_conservatively_reactivated_on_resume() {
        let dir = tempdir().unwrap();
        let mut goal = GoalState::start("ship it", dir.path());
        goal.status = GoalStatus::Verified;

        assert!(goal.invalidate_if_worktree_changed(dir.path()));
        assert!(goal
            .evidence
            .failure_reason()
            .contains("outside a git repository"));
    }

    #[test]
    fn persisted_v1_goal_accepts_missing_additive_fields() {
        let goal: GoalState = serde_json::from_value(json!({
            "objective": "ship it",
            "status": "active"
        }))
        .unwrap();

        assert_eq!(goal.objective, "ship it");
        assert_eq!(goal.baseline, GoalBaseline::default());
        assert_eq!(goal.evidence, GoalEvidence::default());
        assert_eq!(goal.completion_attempts, 0);
    }

    #[test]
    fn goal_command_lifecycle_is_explicit() {
        assert_eq!(parse_goal_command(""), GoalCommand::Inspect);
        assert_eq!(parse_goal_command("pause"), GoalCommand::Pause);
        assert_eq!(parse_goal_command("resume"), GoalCommand::Resume);
        assert_eq!(parse_goal_command("clear"), GoalCommand::Clear);
        assert_eq!(parse_goal_command("off"), GoalCommand::Abandon);
        assert_eq!(
            parse_goal_command("edit ship the release"),
            GoalCommand::Edit("ship the release")
        );
        assert!(!goal_command_starts_turn("resume"));
        assert!(!goal_command_starts_turn("edit ship the release"));
        assert!(!goal_command_starts_turn("pause"));
    }
}
