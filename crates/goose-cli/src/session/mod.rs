mod builder;
mod checkpoints;
mod completion;
pub mod editor;
mod elicitation;
mod images;
mod input;
mod live_input;
mod output;
mod paste;
pub mod streaming_buffer;
mod task_execution_display;
mod thinking;

use crate::session::task_execution_display::{
    format_task_execution_notification, TASK_EXECUTION_NOTIFICATION_TYPE,
};
use goose::conversation::{fix_conversation, merge_consecutive_messages_for_request, Conversation};
use std::env;
use std::io::Write;
use std::str::FromStr;
use tokio::signal::ctrl_c;
use tokio_util::task::AbortOnDropHandle;

pub use builder::{build_session, SessionBuilderConfig};
use console::Color;
use goose::agents::platform_extensions::developer::shell::{
    parse_shell_output_notification, ShellOutputNotificationParams, ShellOutputStream,
};
use goose::agents::AgentEvent;
use goose::agents::SUBAGENT_TOOL_REQUEST_TYPE;
use goose::permission::permission_confirmation::PrincipalType;
use goose::permission::Permission;
use goose::permission::PermissionConfirmation;
use goose::providers::base::Provider;
use goose::providers::base::ProviderUsage;
use goose::utils::safe_truncate;

use anyhow::{Context, Result};
use completion::GooseCompleter;
use goose::agents::extension::{Envs, ExtensionConfig, PLATFORM_EXTENSIONS};
use goose::agents::types::RetryConfig;
use goose::agents::{Agent, SessionConfig, COMPACT_TRIGGERS};
use goose::config::extensions::name_to_key;
use goose::config::{Config, GooseMode};
use input::InputResult;
use rmcp::model::ServerNotification;
use rmcp::model::{CallToolRequestParams, ElicitationAction, JsonObject, PromptMessage};
use rmcp::model::{ErrorCode, ErrorData};
use strum::VariantNames;

use goose::config::paths::Paths;
use goose::config::providers;
use goose::conversation::message::{ActionRequiredData, Message, MessageContent};
use goose::providers::inventory::ProviderInventoryService;
use goose::session::{Session, SessionManager, SessionType};
use goose_providers::thinking::ThinkingEffort;
use rustyline::EditMode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio;
use tokio_util::sync::CancellationToken;
use tracing::warn;

const GOOSE_PLANNER_CONTEXT_LIMIT: &str = "GOOSE_PLANNER_CONTEXT_LIMIT";
const SHELL_STATUS_FALLBACK_WIDTH: usize = 120;
const SHELL_STATUS_MAX_LINES: usize = 3;
const SHELL_STATUS_RESERVED_WIDTH: usize = 2;
const REVIEW_DIFF_LIMIT: usize = 180_000;
const GOVERNED_SESSION_ENV: &str = "EXACTCODE_GOVERNED_SESSION";
const GOVERNED_WORKSPACE_ENV: &str = "EXACTCODE_WORKSPACE_ROOT";
#[cfg(not(target_os = "windows"))]
const NULL_DEVICE: &str = "/dev/null";
#[cfg(target_os = "windows")]
const NULL_DEVICE: &str = "NUL";

const THINKING_EFFORTS: [ThinkingEffort; 6] = [
    ThinkingEffort::Off,
    ThinkingEffort::Low,
    ThinkingEffort::Medium,
    ThinkingEffort::High,
    ThinkingEffort::XHigh,
    ThinkingEffort::Max,
];

fn next_thinking_effort(current: ThinkingEffort) -> ThinkingEffort {
    let index = THINKING_EFFORTS
        .iter()
        .position(|effort| *effort == current)
        .unwrap_or(0);
    THINKING_EFFORTS[(index + 1) % THINKING_EFFORTS.len()]
}

fn permission_mode(policy: &str) -> Option<GooseMode> {
    match policy {
        "ask" => Some(GooseMode::Approve),
        "accept-edit" => Some(GooseMode::SmartApprove),
        "no-perms" => Some(GooseMode::Auto),
        "read-only" => Some(GooseMode::Chat),
        _ => None,
    }
}

fn permission_policy_name(mode: GooseMode) -> &'static str {
    match mode {
        GooseMode::Approve => "ask",
        GooseMode::SmartApprove => "accept-edit",
        GooseMode::Auto => "no-perms",
        GooseMode::Chat => "read-only",
    }
}

fn governed_agent_requires_confirmation(
    governed: bool,
    mode: GooseMode,
) -> std::result::Result<bool, &'static str> {
    if !governed || mode == GooseMode::Auto {
        return Ok(false);
    }
    if mode == GooseMode::Chat {
        return Err("Subagent delegation is unavailable while this governed session is read-only.");
    }
    Ok(true)
}

fn preserve_stream_draft(prefill: &mut Option<String>, draft: String) {
    if !draft.is_empty() {
        *prefill = Some(draft);
    }
}

fn governed_provider_allowed(governed: bool, provider: Option<&str>) -> bool {
    !governed || provider.is_none_or(|name| name == "openai")
}

fn code_rewind_block_reason(
    governed: bool,
    capability_mode: Option<&str>,
    approval_mode: GooseMode,
) -> Option<&'static str> {
    if governed && matches!(capability_mode, Some("read_only" | "read-only")) {
        return Some(
            "Code rewind is unavailable under ExactCode's read-only capability policy. Conversation rewind and fork remain available.",
        );
    }
    if approval_mode == GooseMode::Chat {
        return Some(
            "Code rewind is unavailable while this session's approval policy is read-only.",
        );
    }
    None
}

fn governed_no_prompts_allowed(governed: bool, capability_mode: Option<&str>) -> bool {
    !governed || capability_mode == Some("read_only")
}

fn slash_tool_matches(registered_name: &str, tool_name: &str) -> bool {
    registered_name == tool_name || registered_name.ends_with(&format!("__{tool_name}"))
}

fn valid_process_id(process_id: &str) -> bool {
    !process_id.is_empty()
        && process_id.split_whitespace().count() == 1
        && process_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character))
}

fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return etcetera::home_dir().unwrap_or_else(|_| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = etcetera::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

fn resolve_working_directory(
    requested: Option<&str>,
    current: &std::path::Path,
    previous: Option<&std::path::Path>,
) -> Result<PathBuf> {
    let requested = requested.map(str::trim).filter(|value| !value.is_empty());
    let candidate = match requested {
        None => etcetera::home_dir().context("Could not determine the home directory")?,
        Some("-") => previous
            .map(PathBuf::from)
            .context("No previous working directory in this session")?,
        Some(value) => {
            let path = expand_home(value);
            if path.is_absolute() {
                path
            } else {
                current.join(path)
            }
        }
    };
    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("Directory does not exist: {}", candidate.display()))?;
    if !canonical.is_dir() {
        anyhow::bail!("Not a directory: {}", canonical.display());
    }
    Ok(canonical)
}

fn governed_workspace_root() -> Result<Option<PathBuf>> {
    if std::env::var(GOVERNED_SESSION_ENV).as_deref() != Ok("1") {
        return Ok(None);
    }
    let configured = std::env::var(GOVERNED_WORKSPACE_ENV)
        .with_context(|| format!("{GOVERNED_WORKSPACE_ENV} is required in governed mode"))?;
    let root = PathBuf::from(configured)
        .canonicalize()
        .context("The governed workspace root does not exist")?;
    anyhow::ensure!(
        root.is_dir(),
        "The governed workspace root is not a directory"
    );
    Ok(Some(root))
}

fn enforce_governed_workspace(path: &std::path::Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("Directory does not exist: {}", path.display()))?;
    if let Some(root) = governed_workspace_root()? {
        anyhow::ensure!(
            canonical.starts_with(&root),
            "Governed sessions cannot leave the authorized workspace: {}",
            root.display()
        );
    }
    Ok(canonical)
}

fn governed_builtin_is_blocked(names: &str) -> bool {
    std::env::var(GOVERNED_SESSION_ENV).as_deref() == Ok("1")
        && names
            .split(',')
            .map(str::trim)
            .any(|name| name.eq_ignore_ascii_case("developer"))
}

fn resolve_session_selector<'a>(sessions: &'a [Session], selector: &str) -> Result<&'a Session> {
    let selector = selector.trim();
    anyhow::ensure!(!selector.is_empty(), "Session name or ID is required");

    if let Some(session) = sessions.iter().find(|session| session.id == selector) {
        return Ok(session);
    }

    let name_matches: Vec<&Session> = sessions
        .iter()
        .filter(|session| session.name == selector)
        .collect();
    match name_matches.as_slice() {
        [session] => return Ok(*session),
        [] => {}
        _ => anyhow::bail!(
            "More than one saved session is named '{selector}'; use a session ID instead"
        ),
    }

    let prefix_matches: Vec<&Session> = sessions
        .iter()
        .filter(|session| session.id.starts_with(selector))
        .collect();
    match prefix_matches.as_slice() {
        [session] => Ok(*session),
        [] => anyhow::bail!("No saved session matches '{selector}'"),
        _ => anyhow::bail!("Session ID prefix '{selector}' is ambiguous"),
    }
}

fn run_git(repo: &std::path::Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .with_context(|| format!("Failed to run git {}", args.join(" ")))
}

fn successful_git(repo: &std::path::Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = run_git(repo, args)?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

fn collect_worktree_diff(working_dir: &std::path::Path) -> Result<String> {
    let root_output = successful_git(working_dir, &["rev-parse", "--show-toplevel"])
        .context("The active working directory is not inside a Git repository")?;
    let root = PathBuf::from(String::from_utf8_lossy(&root_output).trim());

    let has_head = run_git(&root, &["rev-parse", "--verify", "HEAD"])
        .map(|output| output.status.success())
        .unwrap_or(false);
    let mut diff = if has_head {
        successful_git(&root, &["diff", "--no-ext-diff", "--binary", "HEAD", "--"])?
    } else {
        let mut combined = successful_git(
            &root,
            &["diff", "--no-ext-diff", "--binary", "--cached", "--"],
        )?;
        combined.extend(successful_git(
            &root,
            &["diff", "--no-ext-diff", "--binary", "--"],
        )?);
        combined
    };

    let untracked = successful_git(&root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    for raw_path in untracked
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = String::from_utf8_lossy(raw_path);
        let output = run_git(
            &root,
            &["diff", "--no-index", "--binary", "--", NULL_DEVICE, &path],
        )?;
        anyhow::ensure!(
            matches!(output.status.code(), Some(0 | 1)),
            "git diff for untracked file '{}' failed: {}",
            path,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        diff.extend(output.stdout);
    }

    Ok(String::from_utf8_lossy(&diff).into_owned())
}

fn bounded_review_diff(diff: &str) -> (String, bool) {
    if diff.len() <= REVIEW_DIFF_LIMIT {
        return (diff.to_string(), false);
    }
    let mut end = REVIEW_DIFF_LIMIT;
    while !diff.is_char_boundary(end) {
        end -= 1;
    }
    (diff.get(..end).unwrap_or_default().to_string(), true)
}

fn short_session_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn planner_provider_messages(plan_messages: &Conversation) -> Conversation {
    // The planner prompt has no turn-context instructions; drop the blocks.
    let projected_messages: Vec<Message> = plan_messages
        .agent_visible_messages()
        .into_iter()
        .filter(|message| !message.is_turn_context())
        .collect();
    let fixed = fix_conversation(Conversation::new_unvalidated(projected_messages)).0;
    Conversation::new_unvalidated(merge_consecutive_messages_for_request(
        fixed.messages().clone(),
    ))
}

#[derive(Serialize, Deserialize, Debug)]
struct JsonOutput {
    messages: Vec<Message>,
    metadata: JsonMetadata,
}

#[derive(Serialize, Deserialize, Debug)]
struct JsonMetadata {
    total_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_read_input_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_write_input_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_usd: Option<f64>,
    status: String,
}

#[derive(Serialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamEvent {
    Message {
        message: Message,
    },
    Notification {
        extension_id: String,
        #[serde(flatten)]
        data: NotificationData,
    },
    Error {
        error: String,
    },
    Complete {
        total_tokens: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        input_tokens: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        output_tokens: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_read_input_tokens: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_write_input_tokens: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
    },
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "snake_case")]
enum NotificationData {
    Log {
        message: String,
    },
    Progress {
        progress: f64,
        total: Option<f64>,
        message: Option<String>,
    },
}

pub enum RunMode {
    Normal,
    Plan,
}

struct HistoryManager {
    history_file: PathBuf,
    old_history_file: PathBuf,
}

impl HistoryManager {
    fn new() -> Self {
        Self {
            history_file: Paths::state_dir().join("history.txt"),
            old_history_file: Paths::config_dir().join("history.txt"),
        }
    }

    fn load(
        &self,
        editor: &mut rustyline::Editor<GooseCompleter, rustyline::history::DefaultHistory>,
    ) {
        if let Some(parent) = self.history_file.parent() {
            if !parent.exists() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    eprintln!("Warning: Failed to create history directory: {}", e);
                }
            }
        }

        let history_files = [&self.history_file, &self.old_history_file];
        if let Some(file) = history_files.iter().find(|f| f.exists()) {
            if let Err(err) = editor.load_history(file) {
                eprintln!("Warning: Failed to load command history: {}", err);
            }
        }
    }

    fn save(
        &self,
        editor: &mut rustyline::Editor<GooseCompleter, rustyline::history::DefaultHistory>,
    ) {
        if let Err(err) = editor.save_history(&self.history_file) {
            eprintln!("Warning: Failed to save command history: {}", err);
        } else if self.old_history_file.exists() {
            if let Err(err) = std::fs::remove_file(&self.old_history_file) {
                eprintln!("Warning: Failed to remove old history file: {}", err);
            }
        }
    }
}

pub struct CliSession {
    agent: Agent,
    messages: Conversation,
    session_id: String,
    completion_cache: Arc<std::sync::RwLock<CompletionCache>>,
    debug: bool,
    run_mode: RunMode,
    scheduled_job_id: Option<String>,
    max_turns: Option<u32>,
    edit_mode: Option<EditMode>,
    retry_config: Option<RetryConfig>,
    output_format: String,
    stats: bool,
    previous_working_dir: Option<PathBuf>,
    queued_followups: VecDeque<String>,
    stream_input_prefill: Option<String>,
    pending_images: Vec<images::PendingImage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintStatus {
    Default,
    Interrupted,
    MaybeExit,
}

// Cache structure for completion data
pub struct CompletionCache {
    pub prompts: HashMap<String, Vec<String>>,
    pub prompt_info: HashMap<String, output::PromptInfo>,
    pub provider_names: Vec<String>,
    pub provider_models: HashMap<String, Vec<String>>,
    pub session_selectors: Vec<String>,
    pub current_session_provider: String,
    pub current_thinking_effort: ThinkingEffort,
    pub last_updated: Instant,
    pub hint_status: HintStatus,
}

impl CompletionCache {
    fn new() -> Self {
        Self {
            prompts: HashMap::new(),
            prompt_info: HashMap::new(),
            provider_names: Vec::new(),
            provider_models: HashMap::new(),
            session_selectors: Vec::new(),
            current_session_provider: String::new(),
            current_thinking_effort: ThinkingEffort::Off,
            last_updated: Instant::now(),
            hint_status: HintStatus::Default,
        }
    }
}

pub enum PlannerResponseType {
    Plan,
    ClarifyingQuestions,
}

/// Decide if the planner's response is a plan or a clarifying question
///
/// This function is called after the planner has generated a response
/// to the user's message. The response is either a plan or a clarifying
/// question.
pub async fn classify_planner_response(
    session_id: &str,
    message_text: String,
    provider: Arc<dyn Provider>,
    model_config: goose_providers::model::ModelConfig,
) -> Result<PlannerResponseType> {
    let prompt = format!(
        "The text below is the output from an AI model which can either provide a plan or list of clarifying questions. Based on the text below, decide if the output is a \"plan\" or \"clarifying questions\".\n---\n{message_text}"
    );

    let message = Message::user().with_text(&prompt);
    let (result, _usage) = goose::session_context::with_session_id(
        Some(session_id.to_string()),
        provider.complete(
            &model_config,
            "Reply only with the classification label: \"plan\" or \"clarifying questions\"",
            &[message],
            &[],
        ),
    )
    .await?;

    let predicted = result.as_concat_text();
    if predicted.to_lowercase().contains("plan") {
        Ok(PlannerResponseType::Plan)
    } else {
        Ok(PlannerResponseType::ClarifyingQuestions)
    }
}

fn planner_classification_text(response: &Message) -> Result<String> {
    let text = response.agent_visible_content().as_concat_text();
    anyhow::ensure!(
        !text.trim().is_empty(),
        "Planner returned no agent-visible text to classify"
    );
    Ok(text)
}

impl CliSession {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        agent: Agent,
        session_id: String,
        debug: bool,
        scheduled_job_id: Option<String>,
        max_turns: Option<u32>,
        edit_mode: Option<EditMode>,
        retry_config: Option<RetryConfig>,
        output_format: String,
        stats: bool,
    ) -> Self {
        let messages = agent
            .config
            .session_manager
            .get_session(&session_id, true)
            .await
            .map(|session| session.conversation.unwrap_or_default())
            .unwrap();

        CliSession {
            agent,
            messages,
            session_id,
            completion_cache: Arc::new(std::sync::RwLock::new(CompletionCache::new())),
            debug,
            run_mode: RunMode::Normal,
            scheduled_job_id,
            max_turns,
            edit_mode,
            retry_config,
            output_format,
            stats,
            previous_working_dir: None,
            queued_followups: VecDeque::new(),
            stream_input_prefill: None,
            pending_images: Vec::new(),
        }
    }

    pub fn session_id(&self) -> &String {
        &self.session_id
    }

    /// Parse a stdio extension command string into an ExtensionConfig
    /// Format: "ENV1=val1 ENV2=val2 command args..."
    pub fn parse_stdio_extension(extension_command: &str) -> Result<ExtensionConfig> {
        let mut parts = goose::utils::split_command_args(extension_command)?;
        let mut envs = HashMap::new();

        while let Some(part) = parts.first() {
            if !part.contains('=') {
                break;
            }
            let env_part = parts.remove(0);
            let (key, value) = env_part.split_once('=').unwrap();
            envs.insert(key.to_string(), value.to_string());
        }

        if parts.is_empty() {
            return Err(anyhow::anyhow!("No command provided in extension string"));
        }

        let cmd = parts.remove(0);
        let name = std::path::Path::new(&cmd)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("unnamed")
            .to_string();

        Ok(ExtensionConfig::Stdio {
            name,
            cmd,
            args: parts,
            envs: Envs::new(envs),
            env_keys: Vec::new(),
            description: goose::config::DEFAULT_EXTENSION_DESCRIPTION.to_string(),
            timeout: Some(goose::config::DEFAULT_EXTENSION_TIMEOUT),
            cwd: None,
            bundled: None,
            available_tools: Vec::new(),
        })
    }

    pub fn parse_streamable_http_extension(extension_url: &str, timeout: u64) -> ExtensionConfig {
        let name = url::Url::parse(extension_url)
            .ok()
            .map(|u| {
                let mut s = String::new();
                if let Some(host) = u.host_str() {
                    s.push_str(host);
                }
                if let Some(port) = u.port() {
                    s.push('_');
                    s.push_str(&port.to_string());
                }
                let path = u.path().trim_matches('/');
                if !path.is_empty() {
                    s.push('_');
                    s.push_str(path);
                }
                name_to_key(&s)
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unnamed".to_string());

        ExtensionConfig::StreamableHttp {
            name,
            uri: extension_url.to_string(),
            envs: Envs::new(HashMap::new()),
            env_keys: Vec::new(),
            headers: HashMap::new(),
            description: goose::config::DEFAULT_EXTENSION_DESCRIPTION.to_string(),
            timeout: Some(timeout),
            socket: None,
            bundled: None,
            available_tools: Vec::new(),
        }
    }

    /// Parse builtin extension names (comma-separated) into ExtensionConfigs
    pub fn parse_builtin_extensions(builtin_name: &str) -> Vec<ExtensionConfig> {
        builtin_name
            .split(',')
            .map(|name| {
                let extension_name = name.trim();
                if PLATFORM_EXTENSIONS.contains_key(extension_name) {
                    ExtensionConfig::Platform {
                        name: extension_name.to_string(),
                        description: extension_name.to_string(),
                        display_name: None,
                        bundled: None,
                        available_tools: Vec::new(),
                    }
                } else {
                    ExtensionConfig::Builtin {
                        name: extension_name.to_string(),
                        display_name: None,
                        timeout: None,
                        bundled: None,
                        description: extension_name.to_string(),
                        available_tools: Vec::new(),
                    }
                }
            })
            .collect()
    }

    async fn add_and_persist_extensions(&mut self, configs: Vec<ExtensionConfig>) -> Result<()> {
        for config in configs {
            self.agent
                .add_extension(config, &self.session_id)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to start extension: {}", e))?;
        }

        self.invalidate_completion_cache().await;

        Ok(())
    }

    pub async fn add_extension(&mut self, extension_command: String) -> Result<()> {
        let config = Self::parse_stdio_extension(&extension_command)?;
        self.add_and_persist_extensions(vec![config]).await
    }

    pub async fn add_streamable_http_extension(&mut self, extension_url: String) -> Result<()> {
        let config = Self::parse_streamable_http_extension(
            &extension_url,
            goose::config::DEFAULT_EXTENSION_TIMEOUT,
        );
        self.add_and_persist_extensions(vec![config]).await
    }

    pub async fn add_builtin(&mut self, builtin_name: String) -> Result<()> {
        let configs = Self::parse_builtin_extensions(&builtin_name);
        self.add_and_persist_extensions(configs).await
    }

    pub async fn list_prompts(
        &mut self,
        extension: Option<String>,
    ) -> Result<HashMap<String, Vec<String>>> {
        let prompts = self.agent.list_extension_prompts(&self.session_id).await;

        // Early validation if filtering by extension
        if let Some(filter) = &extension {
            if !prompts.contains_key(filter) {
                return Err(anyhow::anyhow!("Extension '{}' not found", filter));
            }
        }

        // Convert prompts into filtered map of extension names to prompt names
        Ok(prompts
            .into_iter()
            .filter(|(ext, _)| extension.as_ref().is_none_or(|f| f == ext))
            .map(|(extension, prompt_list)| {
                let names = prompt_list.into_iter().map(|p| p.name).collect();
                (extension, names)
            })
            .collect())
    }

    pub async fn get_prompt_info(&mut self, name: &str) -> Result<Option<output::PromptInfo>> {
        let prompts = self.agent.list_extension_prompts(&self.session_id).await;

        // Find which extension has this prompt
        for (extension, prompt_list) in prompts {
            if let Some(prompt) = prompt_list.iter().find(|p| p.name == name) {
                return Ok(Some(output::PromptInfo {
                    name: prompt.name.clone(),
                    description: prompt.description.clone(),
                    arguments: prompt.arguments.clone(),
                    extension: Some(extension),
                }));
            }
        }

        Ok(None)
    }

    pub async fn get_prompt(&mut self, name: &str, arguments: Value) -> Result<Vec<PromptMessage>> {
        Ok(self
            .agent
            .get_prompt(&self.session_id, name, arguments)
            .await?
            .messages)
    }

    /// Process a single message and get the response
    pub(crate) async fn process_message(
        &mut self,
        message: Message,
        cancel_token: CancellationToken,
        interactive: bool,
    ) -> Result<()> {
        let cancel_token = cancel_token.clone();
        if interactive {
            let prompt = message.as_concat_text();
            if let Err(error) = self.capture_turn_checkpoint(&prompt).await {
                warn!(error = %error, "failed to capture automatic turn checkpoint");
            }
        }
        self.push_message(message);
        self.process_agent_response(interactive, cancel_token)
            .await?;
        Ok(())
    }

    /// Start an interactive session, optionally with an initial message
    pub async fn interactive(&mut self, prompt: Option<String>) -> Result<()> {
        let result = self.run_interactive(prompt).await;

        self.agent
            .emit_hook(goose::hooks::HookEvent::SessionEnd, &self.session_id)
            .await;

        if result.is_ok() {
            println!(
                "\n  {} {}",
                console::style("●").red(),
                console::style(format!("session closed · {}", &self.session_id)).dim()
            );
        }

        result
    }

    async fn run_interactive(&mut self, prompt: Option<String>) -> Result<()> {
        if let Some(prompt) = prompt {
            let msg = Message::user().with_text(&prompt);
            self.process_message(msg, CancellationToken::default(), true)
                .await?;
        }

        self.update_completion_cache().await?;

        let mut editor = self.create_editor()?;
        let history_manager = HistoryManager::new();
        history_manager.load(&mut editor);
        let mut pending_input: Option<String> = None;

        loop {
            let conversation_strings: Vec<String> = self
                .messages
                .user_visible_messages()
                .iter()
                .map(|msg| {
                    let role = match msg.role {
                        rmcp::model::Role::User => "User",
                        rmcp::model::Role::Assistant => "Assistant",
                    };
                    format!("## {}: {}", role, msg.as_concat_text())
                })
                .collect();
            if let Some(followup) = self.queued_followups.pop_front() {
                output::session_message("Running queued follow-up");
                editor.add_history_entry(&followup)?;
                let input = input::parse_submitted_input(&followup);
                if matches!(input, InputResult::Exit) {
                    break;
                }
                self.handle_input(input, &history_manager, &mut editor, &conversation_strings)
                    .await?;
                continue;
            }
            self.display_context_usage().await?;

            output::run_status_hook("waiting");
            if pending_input.is_none() {
                pending_input = self.stream_input_prefill.take();
            }
            let input = input::get_input(
                &mut editor,
                Some(&conversation_strings),
                pending_input.as_deref(),
            )?;
            pending_input = None;
            if matches!(input, InputResult::Exit) {
                break;
            }
            if let InputResult::CycleThinking(buffer) = input {
                self.handle_cycle_thinking().await?;
                pending_input = Some(buffer);
                continue;
            }
            self.handle_input(input, &history_manager, &mut editor, &conversation_strings)
                .await?;
        }

        Ok(())
    }

    fn create_editor(
        &self,
    ) -> Result<rustyline::Editor<GooseCompleter, rustyline::history::DefaultHistory>> {
        let builder =
            rustyline::Config::builder().completion_type(rustyline::CompletionType::Circular);
        let builder = match self.edit_mode {
            Some(mode) => builder.edit_mode(mode),
            None => builder.edit_mode(EditMode::Emacs),
        };
        let config = builder.build();
        let mut editor =
            rustyline::Editor::<GooseCompleter, rustyline::history::DefaultHistory>::with_config(
                config,
            )?;
        let completer = GooseCompleter::new(self.completion_cache.clone());
        editor.set_helper(Some(completer));
        Ok(editor)
    }

    async fn handle_input(
        &mut self,
        input: InputResult,
        history: &HistoryManager,
        editor: &mut rustyline::Editor<GooseCompleter, rustyline::history::DefaultHistory>,
        conversation_messages: &[String],
    ) -> Result<()> {
        match input {
            InputResult::Message(content) => {
                self.handle_message_input(&content, history, editor).await?;
            }
            InputResult::Exit => unreachable!("Exit is handled in the main loop"),
            InputResult::AddExtension(cmd) => {
                history.save(editor);
                if governed_workspace_root()?.is_some() {
                    output::render_error(
                        "Dynamic stdio extensions are disabled in this ExactCode-governed session.",
                    );
                } else {
                    match self.add_extension(cmd.clone()).await {
                        Ok(_) => output::render_extension_success(&cmd),
                        Err(e) => output::render_extension_error(&cmd, &e.to_string()),
                    }
                }
            }
            InputResult::AddBuiltin(names) => {
                history.save(editor);
                if governed_builtin_is_blocked(&names) {
                    output::render_error(
                        "The raw developer extension is disabled in this ExactCode-governed session.",
                    );
                } else {
                    match self.add_builtin(names.clone()).await {
                        Ok(_) => output::render_builtin_success(&names),
                        Err(e) => output::render_builtin_error(&names, &e.to_string()),
                    }
                }
            }
            InputResult::ToggleTheme => {
                history.save(editor);
                self.handle_toggle_theme();
            }
            InputResult::ToggleFullToolOutput => {
                history.save(editor);
                self.handle_toggle_full_tool_output();
            }
            InputResult::SelectTheme(theme_name) => {
                history.save(editor);
                self.handle_select_theme(&theme_name);
            }
            InputResult::Retry => {}
            InputResult::ListPrompts(extension) => {
                history.save(editor);
                match self.list_prompts(extension).await {
                    Ok(prompts) => output::render_prompts(&prompts),
                    Err(e) => output::render_error(&e.to_string()),
                }
            }
            InputResult::GooseMode(mode) => {
                history.save(editor);
                self.handle_goose_mode(&mode).await?;
            }
            InputResult::Permissions(policy) => {
                history.save(editor);
                self.handle_permissions(policy.as_deref()).await?;
            }
            InputResult::Model(options) => {
                history.save(editor);
                self.handle_model(options).await?;
            }
            InputResult::Thinking(effort) => {
                history.save(editor);
                self.handle_thinking(effort.as_deref()).await?;
            }
            InputResult::AttachImages(paths) => {
                history.save(editor);
                self.handle_attach_images(&paths).await?;
            }
            InputResult::Images(action) => {
                history.save(editor);
                self.handle_images(action.as_deref());
            }
            InputResult::CycleThinking(_) => {
                unreachable!("Shift+Tab is handled before normal input dispatch")
            }
            InputResult::ChangeDirectory(directory) => {
                history.save(editor);
                self.handle_change_directory(directory.as_deref()).await?;
            }
            InputResult::PrintWorkingDirectory => {
                self.handle_print_working_directory().await?;
            }
            InputResult::NewSession(name) => {
                history.save(editor);
                self.handle_new_session(name.as_deref()).await?;
            }
            InputResult::ResumeSession(selector) => {
                history.save(editor);
                self.handle_resume_session(selector.as_deref()).await?;
            }
            InputResult::ForkSession(name) => {
                history.save(editor);
                self.handle_fork_session(name.as_deref()).await?;
            }
            InputResult::RenameSession(name) => {
                history.save(editor);
                self.handle_rename_session(&name).await?;
            }
            InputResult::ListSessions => {
                self.handle_list_sessions().await?;
            }
            InputResult::Diff => {
                self.handle_diff().await?;
            }
            InputResult::Review(instructions) => {
                history.save(editor);
                self.handle_review(instructions.as_deref(), history, editor)
                    .await?;
            }
            InputResult::Rewind(options) => {
                history.save(editor);
                self.handle_rewind(options).await?;
            }
            InputResult::Queue(message) => {
                history.save(editor);
                self.handle_queue(message.as_deref()).await;
            }
            InputResult::ProcessList => {
                history.save(editor);
                self.handle_process_list().await?;
            }
            InputResult::StopProcess(process) => {
                history.save(editor);
                self.handle_stop_process(&process).await?;
            }
            InputResult::Subagents => {
                history.save(editor);
                self.handle_subagents().await?;
            }
            InputResult::Agent(instructions) => {
                history.save(editor);
                self.handle_agent(instructions.as_deref()).await?;
            }
            InputResult::Plan(options) => {
                self.handle_plan_mode(options).await?;
            }
            InputResult::EndPlan => {
                self.run_mode = RunMode::Normal;
                output::render_exit_plan_mode();
            }
            InputResult::Clear => {
                history.save(editor);
                self.handle_clear().await?;
            }
            InputResult::PromptCommand(opts) => {
                history.save(editor);
                self.handle_prompt_command(opts).await?;
            }
            InputResult::Recipe(filepath_opt) => {
                history.save(editor);
                self.handle_recipe(filepath_opt).await;
            }
            InputResult::Compact => {
                history.save(editor);
                self.handle_compact().await?;
            }
            InputResult::Edit(prefill) => {
                history.save(editor);
                match crate::session::editor::resolve_editor_command() {
                    Some(editor_cmd) => {
                        let messages: Vec<&str> =
                            conversation_messages.iter().map(|s| s.as_str()).collect();
                        match crate::session::editor::get_editor_input(
                            &editor_cmd,
                            &messages,
                            prefill.as_deref(),
                        ) {
                            Ok((message, true)) => {
                                editor.add_history_entry(message.as_str())?;
                                history.save(editor);
                                self.handle_message_input(&message, history, editor).await?;
                            }
                            Ok((_, false)) => {}
                            Err(e) => {
                                output::render_error(&format!("Failed to open editor: {}", e));
                            }
                        }
                    }
                    None => {
                        output::render_error(
                            "No editor found. Set one with:\n  \
                                 goose configure set goose_prompt_editor \"vim\"\n  \
                                 or set $VISUAL or $EDITOR in your shell.",
                        );
                    }
                }
            }
            InputResult::LoadSkills(names) => {
                history.save(editor);
                self.handle_load_skills(&names).await?;
            }
            InputResult::ListSkills => {
                history.save(editor);
                self.handle_list_skills().await?;
            }
        }
        Ok(())
    }

    async fn handle_message_input(
        &mut self,
        content: &str,
        history: &HistoryManager,
        editor: &mut rustyline::Editor<GooseCompleter, rustyline::history::DefaultHistory>,
    ) -> Result<()> {
        if let Err(error) = self.capture_turn_checkpoint(content).await {
            output::render_error(&format!(
                "Turn checkpoint could not be saved; continuing without it: {error:#}"
            ));
        }
        let message = images::message_with_images(content, &mut self.pending_images);
        match self.run_mode {
            RunMode::Normal => {
                history.save(editor);
                self.push_message(message);

                let _provider = self.agent.provider().await?;

                println!();
                output::run_status_hook("thinking");
                output::show_thinking();
                let start_time = Instant::now();
                self.process_agent_response(true, CancellationToken::default())
                    .await?;
                output::hide_thinking();

                let elapsed = start_time.elapsed();
                let elapsed_str = format_elapsed_time(elapsed);
                println!("{}", console::style(format!("  ⏱ {}", elapsed_str)).dim());
            }
            RunMode::Plan => {
                let mut plan_messages = self.messages.clone();
                plan_messages.push(message);
                let (reasoner, reasoner_model_config) = get_reasoner().await?;
                self.plan_with_reasoner_model(plan_messages, reasoner, reasoner_model_config)
                    .await?;
            }
        }
        Ok(())
    }

    async fn handle_attach_images(&mut self, paths: &[String]) -> Result<()> {
        let working_dir = self.current_session_working_directory().await?;
        let governed_root = governed_workspace_root()?;
        match images::load_images(
            paths,
            &working_dir,
            governed_root.as_deref(),
            &self.pending_images,
        ) {
            Ok(loaded) => {
                let added = loaded.len();
                self.pending_images.extend(loaded);
                let bytes: usize = self.pending_images.iter().map(|image| image.byte_len).sum();
                output::session_message(&format!(
                    "Attached {added} image(s) for the next message · {}/{} · {} total",
                    self.pending_images.len(),
                    images::MAX_IMAGE_ATTACHMENTS,
                    images::format_bytes(bytes)
                ));
                for image in self.pending_images.iter().rev().take(added).rev() {
                    println!(
                        "  {} · {} · {}",
                        image.path.display(),
                        image.mime_type,
                        images::format_bytes(image.byte_len)
                    );
                }
            }
            Err(error) => output::render_error(&error.to_string()),
        }
        Ok(())
    }

    fn handle_images(&mut self, action: Option<&str>) {
        match action.map(str::trim).filter(|value| !value.is_empty()) {
            Some("clear") => {
                let removed = self.pending_images.len();
                self.pending_images.clear();
                output::session_message(&format!("Cleared {removed} pending image(s)"));
            }
            Some(_) => output::render_error("Usage: /images [clear]"),
            None if self.pending_images.is_empty() => {
                output::session_message("No images are attached to the next message");
            }
            None => {
                let bytes: usize = self.pending_images.iter().map(|image| image.byte_len).sum();
                output::session_message(&format!(
                    "{} image(s) attached for the next message · {} total",
                    self.pending_images.len(),
                    images::format_bytes(bytes)
                ));
                for image in &self.pending_images {
                    println!(
                        "  {} · {} · {}",
                        image.path.display(),
                        image.mime_type,
                        images::format_bytes(image.byte_len)
                    );
                }
            }
        }
    }

    fn handle_toggle_theme(&self) {
        let current = output::get_theme();
        let new_theme = match current {
            output::Theme::Ansi => {
                println!("Switching to Light theme");
                output::Theme::Light
            }
            output::Theme::Light => {
                println!("Switching to Dark theme");
                output::Theme::Dark
            }
            output::Theme::Dark => {
                println!("Switching to Ansi theme");
                output::Theme::Ansi
            }
        };
        output::set_theme(new_theme);
    }

    fn handle_select_theme(&self, theme_name: &str) {
        let new_theme = match theme_name {
            "light" => {
                println!("Switching to Light theme");
                output::Theme::Light
            }
            "dark" => {
                println!("Switching to Dark theme");
                output::Theme::Dark
            }
            "ansi" => {
                println!("Switching to Ansi theme");
                output::Theme::Ansi
            }
            _ => output::Theme::Dark,
        };
        output::set_theme(new_theme);
    }

    fn handle_toggle_full_tool_output(&self) {
        let enabled = output::toggle_full_tool_output();
        if enabled {
            println!(
                "{}",
                console::style(
                    "✓ Full tool output enabled - tool parameters will no longer be truncated"
                )
                .green()
            );
        } else {
            println!(
                "{}",
                console::style(
                    "✓ Full tool output disabled - tool parameters will be truncated to fit terminal width"
                )
                .dim()
            );
        }
    }

    async fn handle_goose_mode(&self, mode: &str) -> Result<()> {
        let config = Config::global();
        let mode = match GooseMode::from_str(&mode.to_lowercase()) {
            Ok(mode) => mode,
            Err(_) => {
                output::render_error(&format!(
                    "Invalid mode '{mode}'. Mode must be one of: {}",
                    GooseMode::VARIANTS.join(", ")
                ));
                return Ok(());
            }
        };
        if governed_workspace_root()?.is_some() && mode == GooseMode::Auto {
            output::render_error(
                "Auto mode is unavailable in an ExactCode-governed session. Use /permissions to change approvals without leaving the workspace capability boundary.",
            );
            return Ok(());
        }
        self.agent.update_goose_mode(mode, &self.session_id).await?;
        config.set_goose_mode(mode)?;
        output::goose_mode_message(&format!("Goose mode set to '{mode}'"));
        Ok(())
    }

    async fn handle_permissions(&self, requested: Option<&str>) -> Result<()> {
        let current = self
            .agent
            .config
            .session_manager
            .get_session(&self.session_id, false)
            .await?
            .goose_mode;
        let requested = requested.map(str::trim).filter(|value| !value.is_empty());
        let policy = if let Some(policy) = requested {
            policy.to_ascii_lowercase()
        } else if !std::io::stdin().is_terminal() {
            output::session_message(&format!(
                "Current session approval policy: {}. Use /permissions ask|accept-edit|no-perms|read-only.",
                permission_policy_name(current)
            ));
            return Ok(());
        } else {
            let mut items = vec![
                (
                    "ask".to_string(),
                    "Ask".to_string(),
                    "confirm every tool call".to_string(),
                ),
                (
                    "accept-edit".to_string(),
                    "Accept edits".to_string(),
                    "ask only for sensitive calls".to_string(),
                ),
            ];
            if governed_no_prompts_allowed(
                governed_workspace_root()?.is_some(),
                std::env::var("EXACTCODE_CAPABILITY_MODE").ok().as_deref(),
            ) {
                items.push((
                    "no-perms".to_string(),
                    "No prompts".to_string(),
                    "let the governed host policy decide".to_string(),
                ));
            }
            items.push((
                "read-only".to_string(),
                "Read only".to_string(),
                "disable all tool calls in Goose".to_string(),
            ));
            match cliclack::select("Approval policy for this session:")
                .items(&items)
                .initial_value(permission_policy_name(current).to_string())
                .interact()
            {
                Ok(policy) => policy,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        };

        let Some(mode) = permission_mode(&policy) else {
            output::render_error(
                "Unknown approval policy. Use ask, accept-edit, no-perms, or read-only.",
            );
            return Ok(());
        };
        let governed = governed_workspace_root()?.is_some();
        let capability_mode = std::env::var("EXACTCODE_CAPABILITY_MODE").ok();
        if policy == "no-perms"
            && !governed_no_prompts_allowed(governed, capability_mode.as_deref())
        {
            output::render_error(
                "No-prompts mode is unavailable because this ExactCode-governed session has a writable host bridge. Persist `ecode permissions no-perms` and restart `ecode` to relaunch under ExactCode's read-only capability, or use ask/accept-edit in this session.",
            );
            return Ok(());
        }
        self.agent.update_goose_mode(mode, &self.session_id).await?;
        let boundary = if governed {
            " ExactCode's workspace capability and allow/deny policy remain the hard ceiling."
        } else {
            ""
        };
        output::session_message(&format!(
            "Session approval policy set to '{}'.{boundary}",
            permission_policy_name(mode)
        ));
        Ok(())
    }

    async fn effective_thinking_effort(&self) -> Result<ThinkingEffort> {
        let model_config = self
            .agent
            .model_config_for_session(&self.session_id)
            .await?;
        Ok(model_config
            .thinking_effort()
            .or_else(|| Config::global().get_goose_thinking_effort())
            .unwrap_or(ThinkingEffort::Off))
    }

    async fn handle_thinking(&mut self, requested: Option<&str>) -> Result<()> {
        let current = self.effective_thinking_effort().await?;
        let Some(requested) = requested.map(str::trim).filter(|value| !value.is_empty()) else {
            output::session_message(&format!(
                "Current session reasoning effort: '{current}'\n\
                 Tip: use '/think off|low|medium|high|xhigh|max' or press Shift+Tab to cycle."
            ));
            return Ok(());
        };

        if requested.split_whitespace().count() != 1 {
            output::render_error(
                "Expected one reasoning effort: off, low, medium, high, xhigh, or max.",
            );
            return Ok(());
        }
        let effort = match ThinkingEffort::from_str(requested) {
            Ok(effort) => effort,
            Err(_) => {
                output::render_error(&format!(
                    "Invalid reasoning effort '{requested}'. Use off, low, medium, high, xhigh, or max."
                ));
                return Ok(());
            }
        };
        if effort == current {
            output::session_message(&format!("Reasoning effort already set to '{effort}'"));
            return Ok(());
        }

        self.agent
            .update_thinking_effort(&self.session_id, effort)
            .await?;
        self.completion_cache
            .write()
            .unwrap()
            .current_thinking_effort = effort;
        output::session_message(&format!(
            "Session reasoning effort changed from '{current}' to '{effort}'"
        ));
        Ok(())
    }

    async fn handle_cycle_thinking(&mut self) -> Result<()> {
        let current = self.effective_thinking_effort().await?;
        let next = next_thinking_effort(current);
        self.handle_thinking(Some(&next.to_string())).await
    }

    async fn current_session_working_directory(&self) -> Result<PathBuf> {
        Ok(self
            .agent
            .config
            .session_manager
            .get_session(&self.session_id, false)
            .await?
            .working_dir)
    }

    async fn handle_print_working_directory(&self) -> Result<()> {
        let working_dir = self.current_session_working_directory().await?;
        output::session_message(&format!("Working directory: {}", working_dir.display()));
        Ok(())
    }

    async fn handle_change_directory(&mut self, requested: Option<&str>) -> Result<()> {
        if requested.is_some_and(|value| value.trim().is_empty()) {
            output::render_error(
                "Expected one directory path. Quote paths containing spaces, for example: /cd '../other project'",
            );
            return Ok(());
        }

        let current = self.current_session_working_directory().await?;
        let target = match resolve_working_directory(
            requested,
            &current,
            self.previous_working_dir.as_deref(),
        ) {
            Ok(target) => target,
            Err(error) => {
                output::render_error(&error.to_string());
                return Ok(());
            }
        };
        let target = match enforce_governed_workspace(&target) {
            Ok(target) => target,
            Err(error) => {
                output::render_error(&error.to_string());
                return Ok(());
            }
        };
        if target == current {
            output::session_message(&format!(
                "Working directory already set to {}",
                target.display()
            ));
            return Ok(());
        }

        self.agent
            .config
            .session_manager
            .update(&self.session_id)
            .working_dir(target.clone())
            .apply()
            .await?;
        let session = self
            .agent
            .config
            .session_manager
            .get_session(&self.session_id, false)
            .await?;
        self.agent.restore_provider_from_session(&session).await?;
        self.agent
            .extension_manager
            .update_working_dir(&target)
            .await;
        if let Err(error) = std::env::set_current_dir(&target) {
            self.agent
                .config
                .session_manager
                .update(&self.session_id)
                .working_dir(current.clone())
                .apply()
                .await?;
            return Err(error).with_context(|| {
                format!("Failed to change working directory to {}", target.display())
            });
        }

        self.previous_working_dir = Some(current);
        output::session_message(&format!(
            "Working directory changed to {}",
            target.display()
        ));
        Ok(())
    }

    async fn user_sessions(&self) -> Result<Vec<Session>> {
        let mut sessions = self
            .agent
            .config
            .session_manager
            .list_sessions_by_types(&[SessionType::User])
            .await?;
        if let Some(root) = governed_workspace_root()? {
            sessions.retain(|session| {
                session
                    .working_dir
                    .canonicalize()
                    .is_ok_and(|path| path.starts_with(&root))
            });
        }
        Ok(sessions)
    }

    async fn activate_session(&mut self, target_id: &str) -> Result<()> {
        if target_id == self.session_id {
            output::session_message("That session is already active");
            return Ok(());
        }

        let manager = &self.agent.config.session_manager;
        let target = manager.get_session(target_id, true).await?;
        let governed = governed_workspace_root()?.is_some();
        if !governed_provider_allowed(governed, target.provider_name.as_deref()) {
            anyhow::bail!(
                "Cannot resume '{}': ExactCode-governed sessions require the 'openai' shim provider, but this session saved provider '{}'.",
                target.name,
                target.provider_name.as_deref().unwrap_or_default(),
            );
        }
        if target.goose_mode == GooseMode::Auto
            && !governed_no_prompts_allowed(
                governed,
                std::env::var("EXACTCODE_CAPABILITY_MODE").ok().as_deref(),
            )
        {
            anyhow::bail!(
                "Cannot resume '{}': it saved Auto/no-prompts mode, but this ExactCode-governed host bridge is writable. Resume after launching ExactCode read-only, or use a session saved with ask/accept-edit.",
                target.name,
            );
        }
        let target_working_dir = enforce_governed_workspace(&target.working_dir)
            .with_context(|| format!("Cannot resume '{}'", target.name))?;

        let old_session_id = self.session_id.clone();
        let old_working_dir = self.current_session_working_directory().await?;
        let process_working_dir = std::env::current_dir()?;
        std::env::set_current_dir(&target_working_dir).with_context(|| {
            format!(
                "Failed to switch to session working directory {}",
                target_working_dir.display()
            )
        })?;
        if let Err(error) = self.agent.restore_provider_from_session(&target).await {
            let _ = std::env::set_current_dir(process_working_dir);
            return Err(error).context("Failed to restore the session provider");
        }

        self.agent
            .extension_manager
            .update_working_dir(&target_working_dir)
            .await;
        self.agent
            .emit_hook(goose::hooks::HookEvent::SessionEnd, &old_session_id)
            .await;

        self.session_id = target.id.clone();
        self.messages = target.conversation.unwrap_or_default();
        self.pending_images.clear();
        self.run_mode = RunMode::Normal;
        self.scheduled_job_id = None;
        self.previous_working_dir = Some(old_working_dir);
        self.update_completion_cache().await?;

        output::session_message(&format!(
            "Resumed '{}' · {} · {}",
            target.name,
            target.id,
            target_working_dir.display()
        ));
        self.render_message_history();
        Ok(())
    }

    async fn handle_new_session(&mut self, requested_name: Option<&str>) -> Result<()> {
        let manager = &self.agent.config.session_manager;
        let current = manager.get_session(&self.session_id, false).await?;
        let name = requested_name
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or("CLI Session")
            .to_string();
        let created = manager
            .create_session(
                current.working_dir.clone(),
                name.clone(),
                SessionType::User,
                current.goose_mode,
            )
            .await?;

        self.agent.persist_extension_state(&created.id).await?;
        let mut update = manager.update(&created.id).goose_mode(current.goose_mode);
        if requested_name.is_some() {
            update = update.user_provided_name(name);
        }
        if let Some(provider_name) = current.provider_name {
            update = update.provider_name(provider_name);
        }
        if let Some(model_config) = current.model_config {
            update = update.model_config(model_config);
        }
        if let Some(project_id) = current.project_id {
            update = update.project_id(Some(project_id));
        }
        update.apply().await?;

        self.activate_session(&created.id).await
    }

    async fn handle_resume_session(&mut self, requested: Option<&str>) -> Result<()> {
        let sessions = self.user_sessions().await?;
        if sessions.is_empty() {
            output::render_error("No saved sessions found");
            return Ok(());
        }

        let target_id = if let Some(selector) = requested {
            match resolve_session_selector(&sessions, selector) {
                Ok(session) => session.id.clone(),
                Err(error) => {
                    output::render_error(&error.to_string());
                    return Ok(());
                }
            }
        } else {
            let items: Vec<(String, String, String)> = sessions
                .iter()
                .map(|session| {
                    let current = if session.id == self.session_id {
                        " · current"
                    } else {
                        ""
                    };
                    (
                        session.id.clone(),
                        session.name.clone(),
                        format!(
                            "{} · {}{}",
                            short_session_id(&session.id),
                            session.updated_at.format("%Y-%m-%d %H:%M"),
                            current
                        ),
                    )
                })
                .collect();
            match cliclack::select("Resume a saved session:")
                .items(&items)
                .interact()
            {
                Ok(id) => id,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        };

        self.activate_session(&target_id).await
    }

    async fn handle_fork_session(&mut self, requested_name: Option<&str>) -> Result<()> {
        let manager = &self.agent.config.session_manager;
        let current = manager.get_session(&self.session_id, false).await?;
        let name = requested_name
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{} (fork)", current.name));
        let fork = manager.copy_session(&self.session_id, name.clone()).await?;
        manager
            .update(&fork.id)
            .user_provided_name(name)
            .parent_session_id(Some(self.session_id.clone()))
            .apply()
            .await?;
        self.activate_session(&fork.id).await
    }

    async fn handle_rename_session(&self, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            output::render_error("Usage: /rename <name>");
            return Ok(());
        }
        self.agent
            .config
            .session_manager
            .update(&self.session_id)
            .user_provided_name(name)
            .apply()
            .await?;
        output::session_message(&format!("Renamed current session to '{name}'"));
        Ok(())
    }

    async fn handle_list_sessions(&self) -> Result<()> {
        let sessions = self.user_sessions().await?;
        output::render_sessions(&sessions, &self.session_id);
        Ok(())
    }

    async fn handle_diff(&self) -> Result<()> {
        let working_dir = self.current_session_working_directory().await?;
        match collect_worktree_diff(&working_dir) {
            Ok(diff) if diff.is_empty() => output::session_message("Working tree is clean"),
            Ok(diff) => output::render_worktree_diff(&diff),
            Err(error) => output::render_error(&error.to_string()),
        }
        Ok(())
    }

    async fn handle_review(
        &mut self,
        instructions: Option<&str>,
        history: &HistoryManager,
        editor: &mut rustyline::Editor<GooseCompleter, rustyline::history::DefaultHistory>,
    ) -> Result<()> {
        let working_dir = self.current_session_working_directory().await?;
        let diff = match collect_worktree_diff(&working_dir) {
            Ok(diff) if diff.is_empty() => {
                output::session_message("Working tree is clean; there is nothing to review");
                return Ok(());
            }
            Ok(diff) => diff,
            Err(error) => {
                output::render_error(&error.to_string());
                return Ok(());
            }
        };
        let (diff, truncated) = bounded_review_diff(&diff);
        let extra = instructions
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| format!("\nAdditional instructions: {value}\n"))
            .unwrap_or_default();
        let truncation = if truncated {
            "\nThe embedded diff was truncated. Inspect the repository directly before concluding the review.\n"
        } else {
            ""
        };
        let prompt = format!(
            "Review the current working-tree changes for correctness defects, regressions, security risks, and missing tests. Report findings first, ordered by severity, with precise file and line references. If there are no findings, say so explicitly.{extra}{truncation}\n```diff\n{diff}\n```"
        );
        self.handle_message_input(&prompt, history, editor).await
    }

    async fn capture_turn_checkpoint(&self, prompt: &str) -> Result<checkpoints::TurnCheckpoint> {
        let working_dir = self.current_session_working_directory().await?;
        let authorized_root = governed_workspace_root()?;
        checkpoints::CheckpointJournal::new(&self.session_id)?.capture(
            &working_dir,
            authorized_root.as_deref(),
            &self.messages,
            prompt,
        )
    }

    async fn handle_rewind(&mut self, options: input::RewindCommandOptions) -> Result<()> {
        let journal = checkpoints::CheckpointJournal::new(&self.session_id)?;
        let Some(selector) = options
            .selector
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            let checkpoints = journal.list()?;
            if checkpoints.is_empty() {
                output::session_message(
                    "No turn checkpoints yet. One is saved automatically before each submitted prompt.",
                );
                return Ok(());
            }
            output::session_message("Automatic turn checkpoints (newest first):");
            for checkpoint in checkpoints.iter().take(20) {
                let code = if checkpoint.code.is_some() {
                    "code+conversation"
                } else {
                    "conversation only"
                };
                let prompt = safe_truncate(checkpoint.prompt.trim(), 72);
                println!(
                    "  {} · {} · {} · {}",
                    checkpoint.id,
                    checkpoint.created_at.format("%Y-%m-%d %H:%M:%S UTC"),
                    code,
                    if prompt.is_empty() {
                        "(no prompt)"
                    } else {
                        &prompt
                    }
                );
                if let Some(reason) = &checkpoint.code_unavailable_reason {
                    println!("    code unavailable: {}", safe_truncate(reason, 120));
                }
            }
            if checkpoints.len() > 20 {
                output::session_message(&format!(
                    "Showing 20 of {} checkpoints",
                    checkpoints.len()
                ));
            }
            output::session_message(
                "Restore with /rewind <id> conversation|code|both, or branch from the earlier conversation with /rewind <id> fork.",
            );
            return Ok(());
        };

        let action = options
            .action
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("conversation")
            .to_ascii_lowercase();
        if !matches!(action.as_str(), "conversation" | "code" | "both" | "fork") {
            output::render_error("Usage: /rewind <checkpoint-id> [conversation|code|both|fork]");
            return Ok(());
        }
        let checkpoint = match journal.get(selector) {
            Ok(checkpoint) => checkpoint,
            Err(error) => {
                output::render_error(&error.to_string());
                return Ok(());
            }
        };

        let checkpoint_working_dir = match enforce_governed_workspace(&checkpoint.working_dir) {
            Ok(path) => path,
            Err(error) => {
                output::render_error(&format!(
                    "Checkpoint working directory is no longer available: {error:#}"
                ));
                return Ok(());
            }
        };

        if action == "fork" {
            let manager = &self.agent.config.session_manager;
            let current = manager.get_session(&self.session_id, false).await?;
            let parent_id = self.session_id.clone();
            let name = format!("{} (rewind {})", current.name, checkpoint.id);
            let fork = manager.copy_session(&parent_id, name.clone()).await?;
            manager
                .replace_conversation(&fork.id, &checkpoint.conversation)
                .await?;
            manager
                .update(&fork.id)
                .user_provided_name(name)
                .parent_session_id(Some(parent_id))
                .working_dir(checkpoint_working_dir)
                .apply()
                .await?;
            self.activate_session(&fork.id).await?;
            if !checkpoint.prompt.trim().is_empty() {
                self.stream_input_prefill = Some(checkpoint.prompt.clone());
            }
            output::session_message(&format!(
                "Forked from checkpoint {}. Its original prompt is ready to edit; code was not changed.",
                checkpoint.id
            ));
            return Ok(());
        }

        let restores_code = matches!(action.as_str(), "code" | "both");
        let restores_conversation = matches!(action.as_str(), "conversation" | "both");
        if restores_code {
            let session = self
                .agent
                .config
                .session_manager
                .get_session(&self.session_id, false)
                .await?;
            let governed = std::env::var(GOVERNED_SESSION_ENV).as_deref() == Ok("1");
            let capability = std::env::var("EXACTCODE_CAPABILITY_MODE").ok();
            if let Some(reason) =
                code_rewind_block_reason(governed, capability.as_deref(), session.goose_mode)
            {
                output::render_error(reason);
                return Ok(());
            }
            if checkpoint.code.is_none() {
                output::render_error(
                    checkpoint
                        .code_unavailable_reason
                        .as_deref()
                        .unwrap_or("This checkpoint has no code snapshot"),
                );
                return Ok(());
            }
        }

        if !std::io::stdin().is_terminal() {
            output::render_error(
                "Rewind requires an interactive confirmation because it discards newer state.",
            );
            return Ok(());
        }
        let confirmed = match cliclack::confirm(format!(
            "Restore {action} state from checkpoint {}? A safety checkpoint will be saved first.",
            checkpoint.id
        ))
        .initial_value(false)
        .interact()
        {
            Ok(confirmed) => confirmed,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => false,
            Err(error) => return Err(error.into()),
        };
        if !confirmed {
            output::session_message("Nothing was rewound");
            return Ok(());
        }

        // This captures both layers before the first destructive operation, so
        // a failed or regretted rewind is itself recoverable.
        let safety = self.capture_turn_checkpoint("").await?;
        let authorized_root = governed_workspace_root()?;
        if let Some(code) = checkpoint.code.as_ref().filter(|_| restores_code) {
            if let Err(error) =
                checkpoints::restore_git(code, &checkpoint_working_dir, authorized_root.as_deref())
            {
                output::render_error(&format!(
                    "Code rewind failed before conversation history changed: {error:#}. Safety checkpoint: {}",
                    safety.id
                ));
                return Ok(());
            }
        }
        if restores_conversation {
            let current_working_dir = self.current_session_working_directory().await?;
            if current_working_dir != checkpoint_working_dir {
                let Some(path) = checkpoint_working_dir.to_str() else {
                    output::render_error(
                        "Checkpoint working directory cannot be represented in this terminal.",
                    );
                    return Ok(());
                };
                self.handle_change_directory(Some(path)).await?;
            }
            self.agent
                .config
                .session_manager
                .replace_conversation(&self.session_id, &checkpoint.conversation)
                .await?;
            self.messages = checkpoint.conversation.clone();
            self.pending_images.clear();
            self.queued_followups.clear();
            self.agent.discard_pending_steers(&self.session_id).await;
            if !checkpoint.prompt.trim().is_empty() {
                self.stream_input_prefill = Some(checkpoint.prompt.clone());
            }
        }
        output::session_message(&format!(
            "Restored {action} state from {}. Safety checkpoint: {}{}",
            checkpoint.id,
            safety.id,
            if restores_conversation && !checkpoint.prompt.trim().is_empty() {
                ". The original prompt is ready to edit"
            } else {
                ""
            }
        ));
        Ok(())
    }

    async fn handle_queue(&self, requested: Option<&str>) {
        let requested = requested.map(str::trim).filter(|value| !value.is_empty());
        match requested {
            Some(value) if value.eq_ignore_ascii_case("clear") => {
                self.agent.discard_pending_steers(&self.session_id).await;
                output::session_message("Queued guidance cleared");
            }
            Some(value) => {
                self.agent
                    .steer(&self.session_id, Message::user().with_text(value))
                    .await;
                let count = self.agent.pending_steer_count(&self.session_id).await;
                output::session_message(&format!(
                    "Queued guidance ({count} pending). It will be applied at the next safe model/tool boundary after you send the next prompt."
                ));
            }
            None => {
                let count = self.agent.pending_steer_count(&self.session_id).await;
                output::session_message(&format!(
                    "{count} queued follow-up(s). Use '/queue <message>' or '/queue clear'.\n\
                     While a response streams, type guidance and press Enter to steer the active turn, or Tab to queue the next turn."
                ));
            }
        }
    }

    async fn call_slash_tool(&self, tool_suffix: &str, arguments: JsonObject) -> Result<()> {
        let tool_name = self
            .agent
            .list_tools(&self.session_id, None)
            .await
            .into_iter()
            .find(|tool| slash_tool_matches(tool.name.as_ref(), tool_suffix))
            .map(|tool| tool.name.into_owned());
        let Some(tool_name) = tool_name else {
            output::render_error(&format!(
                "'{tool_suffix}' is unavailable in this session. Start ExactCode with its governed host bridge enabled."
            ));
            return Ok(());
        };
        let session = self
            .agent
            .config
            .session_manager
            .get_session(&self.session_id, false)
            .await?;
        let request = CallToolRequestParams::new(tool_name.clone()).with_arguments(arguments);
        let request_id = format!(
            "slash-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let (_, dispatched) = self
            .agent
            .dispatch_tool_call(request, request_id, None, &session)
            .await;
        let result = dispatched
            .map_err(|error| anyhow::anyhow!(error.to_string()))?
            .result
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let text = result
            .content
            .iter()
            .filter_map(|content| content.as_text().map(|text| text.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        let text = if text.is_empty() {
            result
                .structured_content
                .as_ref()
                .map(serde_json::to_string_pretty)
                .transpose()?
                .unwrap_or_else(|| "Tool completed without output".to_string())
        } else {
            text
        };
        if result.is_error.unwrap_or(false) {
            output::render_error(&text);
        } else {
            output::session_message(&text);
        }
        Ok(())
    }

    async fn handle_process_list(&self) -> Result<()> {
        self.call_slash_tool("process.list", JsonObject::new())
            .await
    }

    async fn handle_stop_process(&self, process: &str) -> Result<()> {
        let process = process.trim();
        if !valid_process_id(process) {
            output::render_error("Process ID may contain only letters, numbers, '-' and '_'.");
            return Ok(());
        }
        let confirmed = match cliclack::confirm(format!("Stop background process '{process}'?"))
            .initial_value(false)
            .interact()
        {
            Ok(confirmed) => confirmed,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => false,
            Err(error) => return Err(error.into()),
        };
        if !confirmed {
            output::session_message("Process was not stopped");
            return Ok(());
        }
        self.call_slash_tool(
            "process.kill",
            serde_json::json!({"process_id": process})
                .as_object()
                .expect("literal object")
                .clone(),
        )
        .await
    }

    async fn handle_subagents(&self) -> Result<()> {
        self.call_slash_tool("summon__load", JsonObject::new())
            .await
    }

    async fn handle_agent(&self, instructions: Option<&str>) -> Result<()> {
        let Some(instructions) = instructions
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            output::session_message(
                "Usage: /agent <instructions>\nUse /agent stop <task-id> to cancel a delegated task, or /subagents to inspect tasks.",
            );
            return Ok(());
        };
        if let Some(task_id) = instructions.strip_prefix("stop ").map(str::trim) {
            if task_id.is_empty() || task_id.split_whitespace().count() != 1 {
                output::render_error("Usage: /agent stop <task-id>");
                return Ok(());
            }
            return self
                .call_slash_tool(
                    "summon__cancel",
                    serde_json::json!({"source": task_id})
                        .as_object()
                        .expect("literal object")
                        .clone(),
                )
                .await;
        }

        let session = self
            .agent
            .config
            .session_manager
            .get_session(&self.session_id, false)
            .await?;
        let governed = governed_workspace_root()?.is_some();
        let requires_confirmation =
            match governed_agent_requires_confirmation(governed, session.goose_mode) {
                Ok(requires_confirmation) => requires_confirmation,
                Err(error) => {
                    output::render_error(error);
                    return Ok(());
                }
            };
        if requires_confirmation {
            output::session_message(&format!(
                "Current session policy: '{}'. A delegated child runs without further approval prompts after this one-time task approval. ExactCode's capability and allow/deny boundary remain the hard ceiling.",
                permission_policy_name(session.goose_mode),
            ));
            let summary = safe_truncate(instructions, 160);
            let confirmed = match cliclack::confirm(format!(
                "Delegate this task with unattended authority: '{summary}'?"
            ))
            .initial_value(false)
            .interact()
            {
                Ok(confirmed) => confirmed,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => false,
                Err(error) => return Err(error.into()),
            };
            if !confirmed {
                output::session_message("Subagent was not started");
                return Ok(());
            }
        }
        if let Err(error) = self
            .capture_turn_checkpoint(&format!("/agent {instructions}"))
            .await
        {
            output::render_error(&format!(
                "Delegation checkpoint could not be saved; continuing without it: {error:#}"
            ));
        }
        self.call_slash_tool(
            "summon__delegate",
            serde_json::json!({"instructions": instructions, "async": true})
                .as_object()
                .expect("literal object")
                .clone(),
        )
        .await
    }

    async fn handle_model(&mut self, mut options: input::ModelCommandOptions) -> Result<()> {
        let provider = self.agent.provider().await?;
        let current_provider_name = provider.get_name().to_string();
        let current_model_config = self
            .agent
            .model_config_for_session(&self.session_id)
            .await?;
        let current_model_name = current_model_config.model_name.clone();
        let picker_requested = options.provider.is_none() && options.model.is_none();

        if picker_requested {
            let mut models = match provider.fetch_supported_models().await {
                Ok(models) if !models.is_empty() => {
                    self.completion_cache
                        .write()
                        .unwrap()
                        .provider_models
                        .insert(current_provider_name.clone(), models.clone());
                    models
                }
                _ => self
                    .completion_cache
                    .read()
                    .unwrap()
                    .provider_models
                    .get(&current_provider_name)
                    .cloned()
                    .unwrap_or_default(),
            };
            if !models.contains(&current_model_name) {
                models.insert(0, current_model_name.clone());
            }
            models.sort();
            models.dedup();
            let items: Vec<(String, String, String)> = models
                .into_iter()
                .map(|model| {
                    let description = if model == current_model_name {
                        "current".to_string()
                    } else {
                        String::new()
                    };
                    (model.clone(), model, description)
                })
                .collect();
            let selection = cliclack::select(format!(
                "Select a model for provider '{current_provider_name}':"
            ))
            .items(&items)
            .initial_value(current_model_name.clone())
            .interact();
            match selection {
                Ok(model) => options.model = Some(model),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }

        let requested_provider = options
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let target_provider_name = requested_provider.unwrap_or(&current_provider_name);

        if options.provider.is_some() && requested_provider.is_none() {
            output::render_error("Provider name is required after '--provider'.");
            return Ok(());
        }

        if !governed_provider_allowed(
            governed_workspace_root()?.is_some(),
            Some(target_provider_name),
        ) {
            output::render_error(
                "ExactCode-governed sessions use only the 'openai' provider pointed at the signed local gateway shim. Switch model IDs freely, but direct provider switching is disabled.",
            );
            return Ok(());
        }

        let target_entry = match goose::providers::get_from_registry(target_provider_name).await {
            Ok(entry) => entry,
            Err(_) => {
                output::render_error(&format!(
                    "Unknown provider '{}'. Use tab-completion to see available providers.",
                    target_provider_name
                ));
                return Ok(());
            }
        };

        if target_provider_name.ends_with("-acp") {
            output::render_error(
                "Session model switching is not supported for ACP providers in the CLI.",
            );
            return Ok(());
        }

        if provider.manages_own_context() {
            output::render_error(&format!(
                "Session model or provider switching is not supported for provider '{}' because it manages its own conversation context.",
                current_provider_name
            ));
            return Ok(());
        }

        if options
            .model
            .as_deref()
            .is_some_and(|model| model.split_whitespace().count() > 1)
        {
            output::render_error("Unexpected arguments after model name.");
            return Ok(());
        }

        let target_model_name = match options.model.as_deref().map(str::trim) {
            Some(m) if !m.is_empty() => m.to_string(),
            _ => {
                if target_provider_name == current_provider_name {
                    current_model_name.clone()
                } else {
                    let known: Vec<&str> = target_entry
                        .metadata()
                        .known_models
                        .iter()
                        .map(|m| m.name.as_str())
                        .collect();
                    if known.contains(&current_model_name.as_str()) {
                        current_model_name.clone()
                    } else {
                        target_entry.metadata().default_model.clone()
                    }
                }
            }
        };

        let mut new_model_config = build_switched_model_config(
            target_provider_name,
            &target_model_name,
            &current_model_config,
        )?;

        let configured_effort = Config::global().get_goose_thinking_effort();
        if picker_requested {
            new_model_config = preserve_picker_thinking_effort(
                new_model_config,
                &current_model_config,
                configured_effort,
            );
        }
        let new_effort = new_model_config.thinking_effort().or(configured_effort);
        let current_effort = current_model_config.thinking_effort().or(configured_effort);
        let provider_unchanged = target_provider_name == current_provider_name;
        if provider_unchanged
            && new_model_config.model_name == current_model_config.model_name
            && new_effort == current_effort
        {
            output::goose_mode_message(&format!(
                "Session already using model '{}' for provider '{}'",
                current_model_name, current_provider_name
            ));
            return Ok(());
        }

        if let Some(model_info) = target_entry
            .metadata()
            .known_models
            .iter()
            .find(|m| m.name == target_model_name)
        {
            if model_info.context_limit < current_model_config.context_limit.unwrap_or(0) {
                eprintln!(
                    "{}",
                    console::style(format!(
                        "Warning: '{}' has a smaller context window ({} tokens) than the current session ({} tokens). \
                        You may need to use /compact.",
                        target_model_name,
                        model_info.context_limit,
                        current_model_config.context_limit.unwrap_or(0)
                    ))
                    .yellow()
                );
            }
        }

        let extensions = self.agent.get_extension_configs().await;
        let new_provider = match goose::providers::create(target_provider_name, extensions).await {
            Ok(p) => p,
            Err(e) => {
                output::render_error(&format!(
                    "Cannot switch to provider '{}': {}\n\
                         Set credentials via `goose configure` or the appropriate environment variable.\n\
                         Session continues with current provider '{}'.",
                    target_provider_name, e, current_provider_name
                ));
                return Ok(());
            }
        };

        if new_provider.manages_own_context() {
            output::render_error(&format!(
                "Session provider switching is not supported for '{}' because it manages its own conversation context.",
                target_provider_name
            ));
            return Ok(());
        }

        self.agent
            .update_provider(new_provider, new_model_config, &self.session_id)
            .await?;

        let mode = self.agent.goose_mode().await;
        self.agent.update_goose_mode(mode, &self.session_id).await?;

        self.update_completion_cache().await?;

        if provider_unchanged {
            output::goose_mode_message(&format!(
                "Session model switched from '{}' to '{}' for provider '{}'",
                current_model_name, target_model_name, current_provider_name
            ));
        } else {
            output::goose_mode_message(&format!(
                "Session switched from provider '{}' / model '{}' to provider '{}' / model '{}'",
                current_provider_name, current_model_name, target_provider_name, target_model_name
            ));
        }
        Ok(())
    }

    async fn handle_plan_mode(&mut self, options: input::PlanCommandOptions) -> Result<()> {
        self.run_mode = RunMode::Plan;
        output::render_enter_plan_mode();

        if options.message_text.is_empty() {
            return Ok(());
        }

        let mut plan_messages = self.messages.clone();
        plan_messages.push(images::message_with_images(
            &options.message_text,
            &mut self.pending_images,
        ));

        let (reasoner, reasoner_model_config) = get_reasoner().await?;
        self.plan_with_reasoner_model(plan_messages, reasoner, reasoner_model_config)
            .await
    }

    async fn handle_clear(&mut self) -> Result<()> {
        if let Err(e) = self
            .agent
            .config
            .session_manager
            .replace_conversation(&self.session_id, &Conversation::default())
            .await
        {
            output::render_error(&format!("Failed to clear session: {}", e));
            return Ok(());
        }

        if let Err(e) = self
            .agent
            .config
            .session_manager
            .update(&self.session_id)
            .usage(goose_providers::conversation::token_usage::Usage::new(
                Some(0),
                Some(0),
                Some(0),
            ))
            .apply()
            .await
        {
            output::render_error(&format!("Failed to reset token counts: {}", e));
            return Ok(());
        }

        self.messages.clear();
        self.pending_images.clear();
        tracing::info!("Chat context cleared by user.");
        output::render_message(
            &Message::assistant().with_text("Chat context cleared.\n"),
            self.debug,
        );
        Ok(())
    }

    async fn handle_recipe(&mut self, filepath_opt: Option<String>) {
        println!("{}", console::style("Generating Recipe").green());

        output::show_thinking();
        let recipe = self
            .agent
            .create_recipe(&self.session_id, self.messages.clone())
            .await;
        output::hide_thinking();

        match recipe {
            Ok(recipe) => {
                let filepath_str = filepath_opt.as_deref().unwrap_or("recipe.yaml");
                match self.save_recipe(&recipe, filepath_str) {
                    Ok(path) => println!(
                        "{}",
                        console::style(format!("Saved recipe to {}", path.display())).green()
                    ),
                    Err(e) => println!("{}", console::style(e).red()),
                }
            }
            Err(e) => {
                println!(
                    "{}: {:?}",
                    console::style("Failed to generate recipe").red(),
                    e
                );
            }
        }
    }

    async fn handle_load_skills(&mut self, names: &[String]) -> Result<()> {
        // NOTE: We don't validate the skill names here because the load_skill tool will
        // handle that and provide feedback to the user if any skill names are invalid.
        let message = format!(
            "Use the load_skill tool to load the following skills: {}.",
            names
                .iter()
                .map(|n| format!("\"{}\"", n))
                .collect::<Vec<_>>()
                .join(", ")
        );
        self.push_message(Message::user().with_text(&message));
        output::show_thinking();
        let result = self
            .process_agent_response(true, CancellationToken::default())
            .await;
        output::hide_thinking();
        result?;

        Ok(())
    }

    async fn handle_list_skills(&mut self) -> Result<()> {
        use comfy_table::{presets, Cell, ContentArrangement, Table};
        use goose::custom_requests::SourceType;
        use goose::skills::list_installed_skills;
        let cwd = std::env::current_dir().unwrap_or_default();
        let skills = list_installed_skills(Some(&cwd));

        if skills.is_empty() {
            println!("{}", console::style("No skills available.").yellow());
            return Ok(());
        }

        let mut table = Table::new();
        table.set_content_arrangement(ContentArrangement::Dynamic);
        table.load_preset(presets::ASCII_FULL);
        table.set_header(vec!["Skill", "Location", "Description"]);

        let mut sorted_skills = skills;
        sorted_skills.sort_by(|a, b| a.name.cmp(&b.name));

        for skill in &sorted_skills {
            let location = if skill.source_type == SourceType::BuiltinSkill {
                "built-in"
            } else if skill.global {
                "global"
            } else {
                "project"
            };
            table.add_row(vec![
                Cell::new(&skill.name),
                Cell::new(location),
                Cell::new(&skill.description),
            ]);
        }

        println!("{table}");
        Ok(())
    }

    async fn handle_compact(&mut self) -> Result<()> {
        let prompt = "Are you sure you want to compact this conversation? This will condense the message history.";
        let should_summarize = match cliclack::confirm(prompt).initial_value(true).interact() {
            Ok(choice) => choice,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::Interrupted {
                    false
                } else {
                    return Err(e.into());
                }
            }
        };

        if should_summarize {
            self.push_message(Message::user().with_text(COMPACT_TRIGGERS[0]));
            output::show_thinking();
            self.process_agent_response(true, CancellationToken::default())
                .await?;
            output::hide_thinking();
        } else {
            println!("{}", console::style("Compaction cancelled.").yellow());
        }
        Ok(())
    }

    async fn plan_with_reasoner_model(
        &mut self,
        plan_messages: Conversation,
        reasoner: Arc<dyn Provider>,
        model_config: goose_providers::model::ModelConfig,
    ) -> Result<(), anyhow::Error> {
        let plan_prompt = self.agent.get_plan_prompt(&self.session_id).await?;
        let provider_messages = planner_provider_messages(&plan_messages);
        output::show_thinking();
        let (plan_response, _usage) = goose::session_context::with_session_id(
            Some(self.session_id.clone()),
            reasoner.complete(
                &model_config,
                &plan_prompt,
                provider_messages.messages(),
                &[],
            ),
        )
        .await?;
        let classifier_text = planner_classification_text(&plan_response);
        let plan_response = plan_response.user_visible_content();
        output::render_message(&plan_response, self.debug);
        output::hide_thinking();
        let classifier_text = classifier_text?;
        anyhow::ensure!(
            !plan_response.content.is_empty(),
            "Planner returned no user-visible content"
        );
        let planner_response_type = classify_planner_response(
            &self.session_id,
            classifier_text,
            self.agent.provider().await?,
            self.agent
                .model_config_for_session(&self.session_id)
                .await?,
        )
        .await?;

        match planner_response_type {
            PlannerResponseType::Plan => {
                println!();
                let should_act = match cliclack::confirm(
                    "Do you want to clear message history & act on this plan?",
                )
                .initial_value(true)
                .interact()
                {
                    Ok(choice) => choice,
                    Err(e) => {
                        if e.kind() == std::io::ErrorKind::Interrupted {
                            false // If interrupted, set should_act to false
                        } else {
                            return Err(e.into());
                        }
                    }
                };
                if should_act {
                    output::render_act_on_plan();
                    self.run_mode = RunMode::Normal;
                    // set goose mode: auto if that isn't already the case
                    let config = Config::global();
                    let curr_goose_mode = config.get_goose_mode().unwrap_or_default();
                    if curr_goose_mode != GooseMode::Auto {
                        config.set_goose_mode(GooseMode::Auto).unwrap();
                    }

                    // clear the messages before acting on the plan
                    self.messages.clear();
                    // add the plan response as a user message
                    let plan_message = Message::user().with_text(plan_response.as_concat_text());
                    self.push_message(plan_message);
                    // act on the plan
                    output::show_thinking();
                    self.process_agent_response(true, CancellationToken::default())
                        .await?;
                    output::hide_thinking();

                    // Reset run & goose mode
                    if curr_goose_mode != GooseMode::Auto {
                        config.set_goose_mode(curr_goose_mode)?;
                    }
                } else {
                    // add the plan response (assistant message) & carry the conversation forward
                    // in the next round, the user might wanna slightly modify the plan
                    self.push_message(plan_response);
                }
            }
            PlannerResponseType::ClarifyingQuestions => {
                // add the plan response (assistant message) & carry the conversation forward
                // in the next round, the user will answer the clarifying questions
                self.push_message(plan_response);
            }
        }

        Ok(())
    }

    /// Process a single message and exit
    pub async fn headless(&mut self, prompt: String) -> Result<()> {
        let message = Message::user().with_text(&prompt);
        let result = self
            .process_message(message, CancellationToken::default(), false)
            .await;
        self.agent
            .emit_hook(goose::hooks::HookEvent::SessionEnd, &self.session_id)
            .await;
        result?;
        Ok(())
    }

    async fn process_agent_response(
        &mut self,
        interactive: bool,
        cancel_token: CancellationToken,
    ) -> Result<()> {
        let is_json_mode = self.output_format == "json";
        let is_stream_json_mode = self.output_format == "stream-json";

        let session_config = SessionConfig {
            id: self.session_id.clone(),
            schedule_id: self.scheduled_job_id.clone(),
            max_turns: self.max_turns,
            retry_config: self.retry_config.clone(),
        };
        let user_message = self
            .messages
            .last()
            .ok_or_else(|| anyhow::anyhow!("No user message"))?;

        let cancel_token_interrupt = cancel_token.clone();
        let handle = tokio::spawn(async move {
            if ctrl_c().await.is_ok() {
                cancel_token_interrupt.cancel();
            }
        });
        let _drop_handle = AbortOnDropHandle::new(handle);

        let mut stream = self
            .agent
            .reply(
                user_message.clone(),
                session_config.clone(),
                Some(cancel_token.clone()),
            )
            .await?;

        let mut progress_bars = output::McpSpinners::new();
        let cancel_token_clone = cancel_token.clone();
        let mut markdown_buffer = streaming_buffer::MarkdownBuffer::new();
        let mut prompted_credits_urls: HashSet<String> = HashSet::new();
        let mut thinking_header_shown = false;
        let run_started = Instant::now();
        let mut first_token_at: Option<Instant> = None;
        let mut last_usage: Option<ProviderUsage> = None;
        let live_input_enabled = interactive && !is_json_mode && !is_stream_json_mode;
        let mut live_input = if live_input_enabled {
            live_input::LiveInput::start(self.stream_input_prefill.take().unwrap_or_default())?
        } else {
            None
        };

        use futures::StreamExt;
        loop {
            tokio::select! {
                // If response completion and a terminal key arrive together,
                // completion wins. This prevents an Enter intended for the
                // just-finished turn from remaining in Agent's steer queue and
                // silently changing the next turn.
                biased;
                result = stream.next() => {
                    if let Some(input) = live_input.as_mut() {
                        input.clear_line()?;
                    }
                    match result {
                        Some(Ok(AgentEvent::Message(message))) => {
                            if first_token_at.is_none() && message_has_text(&message) {
                                first_token_at = Some(Instant::now());
                            }
                            if let Some((id, security_prompt)) = find_tool_confirmation(&message) {
                                let buffered_input = live_input
                                    .take()
                                    .map(live_input::LiveInput::stop)
                                    .transpose()?
                                    .unwrap_or_default();
                                let permission = if interactive {
                                    prompt_tool_confirmation(&security_prompt)?
                                } else {
                                    // Non-interactive/headless mode: refuse to run in
                                    // Approve/SmartApprove modes since auto-allowing would
                                    // bypass the safety contract those modes are meant to enforce.
                                    let config = Config::global();
                                    let goose_mode = config.get_goose_mode().unwrap_or(GooseMode::Auto);
                                    if goose_mode == GooseMode::Approve || goose_mode == GooseMode::SmartApprove {
                                        cancel_token_clone.cancel();
                                        drop(stream);
                                        return Err(anyhow::anyhow!(
                                            "Tool approval required in non-interactive mode with GooseMode::{goose_mode}. \
                                             This is an invalid configuration — Approve/SmartApprove modes require an \
                                             interactive terminal. Use GooseMode::Auto for headless sessions."
                                        ));
                                    }
                                    tracing::warn!(
                                        "Tool confirmation required in non-interactive mode, auto-allowing"
                                    );
                                    Permission::AllowOnce
                                };

                                if permission == Permission::Cancel {
                                    output::render_text("Tool call cancelled. Returning to chat...", Some(Color::Yellow), true);
                                    self.agent.handle_confirmation(id.clone(), PermissionConfirmation {
                                        principal_type: PrincipalType::Tool,
                                        permission: Permission::DenyOnce,
                                    }).await;
                                    let mut response_message = Message::user();
                                    response_message.content.push(MessageContent::tool_response(
                                        id,
                                        Err(ErrorData {
                                            code: ErrorCode::INVALID_REQUEST,
                                            message: std::borrow::Cow::from("Tool call cancelled by user"),
                                            data: None,
                                        }),
                                    ));
                                    self.messages.push(response_message);
                                    cancel_token_clone.cancel();
                                    drop(stream);
                                    preserve_stream_draft(
                                        &mut self.stream_input_prefill,
                                        buffered_input,
                                    );
                                    break;
                                }
                                self.agent.handle_confirmation(id, PermissionConfirmation {
                                    principal_type: PrincipalType::Tool,
                                    permission,
                                }).await;
                                if live_input_enabled {
                                    live_input = live_input::LiveInput::start(buffered_input)?;
                                }
                            } else if let Some((elicitation_id, elicitation_message, schema)) = find_elicitation_request(&message) {
                                if !interactive {
                                    // Non-interactive/headless mode: cannot collect user input
                                    tracing::warn!(
                                        "Elicitation requested in non-interactive mode, cancelling"
                                    );
                                    cancel_token_clone.cancel();
                                    drop(stream);
                                    return Err(anyhow::anyhow!(
                                        "Elicitation requested but no interactive terminal is available to collect user input"
                                    ));
                                }

                                output::hide_thinking();
                                let _ = progress_bars.hide();
                                let buffered_input = live_input
                                    .take()
                                    .map(live_input::LiveInput::stop)
                                    .transpose()?
                                    .unwrap_or_default();

                                match elicitation::collect_elicitation_input(&elicitation_message, &schema) {
                                    Ok(input) => {
                                        match &input.action {
                                            ElicitationAction::Decline => {
                                                output::render_text("Information request declined.", Some(Color::Yellow), true);
                                            }
                                            ElicitationAction::Cancel => {
                                                output::render_text("Information request cancelled.", Some(Color::Yellow), true);
                                            }
                                            ElicitationAction::Accept => {}
                                            _ => {}
                                        }

                                        let should_cancel = input.action == ElicitationAction::Cancel;
                                        let action = input.action;
                                        let user_data_value = serde_json::to_value(input.user_data)
                                            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                                        let response_message = Message::user()
                                            .with_content(MessageContent::action_required_elicitation_response(
                                                elicitation_id,
                                                user_data_value,
                                                action,
                                            ))
                                            .with_visibility(false, true);
                                        self.messages.push(response_message.clone());
                                        // Elicitation responses return an empty stream - the response
                                        // unblocks the waiting tool call via ActionRequiredManager
                                        let _ = self.agent.reply(response_message, session_config.clone(), Some(cancel_token.clone())).await?;
                                        if should_cancel {
                                            cancel_token_clone.cancel();
                                            drop(stream);
                                            preserve_stream_draft(
                                                &mut self.stream_input_prefill,
                                                buffered_input,
                                            );
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        output::render_error(&format!("Failed to collect input: {}", e));
                                        cancel_token_clone.cancel();
                                        drop(stream);
                                        preserve_stream_draft(
                                            &mut self.stream_input_prefill,
                                            buffered_input,
                                        );
                                        break;
                                    }
                                }
                                if live_input_enabled {
                                    live_input = live_input::LiveInput::start(buffered_input)?;
                                }
                            } else {
                                log_tool_metrics(&message, &self.messages);
                                self.messages.push(message.clone());

                                if interactive { output::hide_thinking() };
                                let _ = progress_bars.hide();

                                if is_stream_json_mode {
                                    emit_stream_event(&StreamEvent::Message { message: message.clone() });
                                } else if !is_json_mode {
                                    output::render_message_streaming(&message, &mut markdown_buffer, &mut thinking_header_shown, self.debug);
                                    maybe_open_credits_top_up_url(
                                        &message,
                                        interactive,
                                        &mut prompted_credits_urls,
                                    );
                                }
                            }
                        }
                        Some(Ok(AgentEvent::Usage(usage))) => {
                            last_usage = Some(usage);
                        }
                        Some(Ok(AgentEvent::MessageUsage { .. })) => {}
                        Some(Ok(AgentEvent::McpNotification((extension_id, notification)))) => {
                            handle_mcp_notification(
                                &extension_id,
                                &notification,
                                &mut progress_bars,
                                is_stream_json_mode,
                                interactive,
                                is_json_mode,
                                self.debug,
                            );
                        }
                        Some(Ok(AgentEvent::HistoryReplaced(updated_conversation))) => {
                            self.messages = updated_conversation;
                        }
                        Some(Err(e)) => {
                            handle_agent_error(&e, is_stream_json_mode);
                            cancel_token_clone.cancel();
                            drop(stream);
                            if let Err(e) = self.handle_interrupted_messages(false).await {
                                eprintln!("Error handling interruption: {}", e);
                            } else if !is_stream_json_mode {
                                output::render_error(
                                    "The error above was an exception we were not able to handle.\n\
                                    These errors are often related to connection or authentication\n\
                                    We've removed the conversation up to the most recent user message\n\
                                    - depending on the error you may be able to continue",
                                );
                            }
                            break;
                        }
                        None => break,
                    }
                    if let Some(input) = live_input.as_mut() {
                        input.redraw()?;
                    }
                }
                action = async {
                    live_input
                        .as_mut()
                        .expect("live input select branch is guarded")
                        .next_action()
                        .await
                }, if live_input.is_some() => {
                    match action? {
                        Some(live_input::LiveInputAction::Steer(message)) => {
                            self.agent
                                .steer(&self.session_id, Message::user().with_text(&message))
                                .await;
                            output::run_status_hook("steered");
                        }
                        Some(live_input::LiveInputAction::Queue(message)) => {
                            self.queued_followups.push_back(message);
                            output::run_status_hook("queued follow-up");
                        }
                        Some(live_input::LiveInputAction::Cancel) => {
                            cancel_token_clone.cancel();
                        }
                        None => {
                            if let Some(input) = live_input.take() {
                                preserve_stream_draft(
                                    &mut self.stream_input_prefill,
                                    input.stop()?,
                                );
                            }
                        }
                    }
                }
                _ = cancel_token_clone.cancelled() => {
                    drop(stream);
                    if let Err(e) = self.handle_interrupted_messages(true).await {
                        eprintln!("Error handling interruption: {}", e);
                    }
                    break;
                }
            }
        }

        if let Some(input) = live_input.take() {
            preserve_stream_draft(&mut self.stream_input_prefill, input.stop()?);
        }

        if !is_json_mode && !is_stream_json_mode {
            output::flush_markdown_buffer_current_theme(&mut markdown_buffer);
        }

        if is_json_mode {
            let metadata = match self
                .agent
                .config
                .session_manager
                .get_session_usage_totals(&self.session_id)
                .await
            {
                Ok(totals) => JsonMetadata {
                    total_tokens: totals.accumulated_usage.total_tokens,
                    input_tokens: totals.accumulated_usage.input_tokens,
                    output_tokens: totals.accumulated_usage.output_tokens,
                    cache_read_input_tokens: totals.accumulated_usage.cache_read_input_tokens,
                    cache_write_input_tokens: totals.accumulated_usage.cache_write_input_tokens,
                    cost_usd: totals.accumulated_cost,
                    status: "completed".to_string(),
                },
                Err(_) => JsonMetadata {
                    total_tokens: None,
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_input_tokens: None,
                    cache_write_input_tokens: None,
                    cost_usd: None,
                    status: "completed".to_string(),
                },
            };
            let json_output = JsonOutput {
                messages: self.messages.user_visible_messages(),
                metadata,
            };
            println!("{}", serde_json::to_string_pretty(&json_output)?);
        } else if is_stream_json_mode {
            let totals = self
                .agent
                .config
                .session_manager
                .get_session_usage_totals(&self.session_id)
                .await
                .ok();
            let (
                total_tokens,
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_write_input_tokens,
                cost_usd,
            ) = match totals {
                Some(totals) => (
                    totals.accumulated_usage.total_tokens,
                    totals.accumulated_usage.input_tokens,
                    totals.accumulated_usage.output_tokens,
                    totals.accumulated_usage.cache_read_input_tokens,
                    totals.accumulated_usage.cache_write_input_tokens,
                    totals.accumulated_cost,
                ),
                None => (None, None, None, None, None, None),
            };
            emit_stream_event(&StreamEvent::Complete {
                total_tokens,
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_write_input_tokens,
                cost_usd,
            });
        } else {
            println!();
            if self.stats {
                print_run_stats(run_started, first_token_at, last_usage.as_ref());
            }
        }

        Ok(())
    }

    async fn handle_interrupted_messages(&mut self, interrupt: bool) -> Result<()> {
        if interrupt {
            let mut cache = self.completion_cache.write().unwrap();
            cache.hint_status = HintStatus::Interrupted;
        }

        let tool_requests = self
            .messages
            .last()
            .filter(|msg| msg.role == rmcp::model::Role::Assistant)
            .map_or(Vec::new(), |msg| {
                msg.content
                    .iter()
                    .filter_map(|content| {
                        if let MessageContent::ToolRequest(req) = content {
                            Some((req.id.clone(), req.tool_call.clone()))
                        } else {
                            None
                        }
                    })
                    .collect()
            });

        let interrupt_prompt = "Yes — what would you like me to do?";

        if !tool_requests.is_empty() {
            let mut response_message = Message::user();

            let notification = if interrupt {
                "Interrupted by the user to make a correction".to_string()
            } else {
                "An uncaught error happened during tool use".to_string()
            };
            for (req_id, _) in &tool_requests {
                response_message.content.push(MessageContent::tool_response(
                    req_id.clone(),
                    Err(ErrorData {
                        code: ErrorCode::INTERNAL_ERROR,
                        message: std::borrow::Cow::from(notification.clone()),
                        data: None,
                    }),
                ));
            }
            self.push_message(response_message);
            self.push_message(Message::assistant().with_text(interrupt_prompt));
            output::render_message(
                &Message::assistant().with_text(interrupt_prompt),
                self.debug,
            );
        } else {
            while self.messages.last().is_some_and(Message::is_turn_context) {
                self.messages.pop();
            }
            if let Some(last_msg) = self.messages.last() {
                if last_msg.role == rmcp::model::Role::User {
                    match last_msg.content.first() {
                        Some(MessageContent::ToolResponse(_)) => {
                            self.push_message(Message::assistant().with_text(interrupt_prompt));
                            output::render_message(
                                &Message::assistant().with_text(interrupt_prompt),
                                self.debug,
                            );
                        }
                        Some(_) => {
                            self.messages.pop();
                            let assistant_msg = Message::assistant().with_text(interrupt_prompt);
                            self.push_message(assistant_msg.clone());
                            output::render_message(&assistant_msg, self.debug);
                        }
                        None => {
                            // Empty message content — nothing to do, just continue gracefully
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn update_completion_cache(&mut self) -> Result<()> {
        let prompts = self.agent.list_extension_prompts(&self.session_id).await;
        let all_providers = goose::providers::providers().await;
        let sessions = self.user_sessions().await.unwrap_or_default();
        let session_provider = self.agent.provider().await?.get_name().to_string();
        let session_thinking_effort = self.effective_thinking_effort().await?;

        let provider_ids: Vec<String> = all_providers.iter().map(|(m, _)| m.name.clone()).collect();
        let inventory_models: HashMap<String, Vec<String>> = {
            let storage = SessionManager::instance().storage().clone();
            let inventory = ProviderInventoryService::new(storage);
            inventory
                .entries(&provider_ids)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|entry| {
                    let model_ids: Vec<String> =
                        entry.models.iter().map(|m| m.id.clone()).collect();
                    (entry.provider_id, model_ids)
                })
                .collect()
        };

        let config = Config::global();
        let configured_models: HashMap<String, String> = all_providers
            .iter()
            .filter_map(|(m, _)| {
                providers::get_provider_entry(config, &m.name)
                    .map(|entry| (m.name.clone(), entry.model))
                    .filter(|(_, model)| !model.is_empty())
            })
            .collect();

        let mut cache = self.completion_cache.write().unwrap();
        cache.prompts.clear();
        cache.prompt_info.clear();

        for (extension, prompt_list) in prompts {
            let names: Vec<String> = prompt_list.iter().map(|p| p.name.clone()).collect();
            cache.prompts.insert(extension.clone(), names);

            for prompt in prompt_list {
                cache.prompt_info.insert(
                    prompt.name.clone(),
                    output::PromptInfo {
                        name: prompt.name.clone(),
                        description: prompt.description.clone(),
                        arguments: prompt.arguments.clone(),
                        extension: Some(extension.clone()),
                    },
                );
            }
        }

        cache.provider_names = all_providers.iter().map(|(m, _)| m.name.clone()).collect();
        cache.current_session_provider = session_provider;
        cache.current_thinking_effort = session_thinking_effort;
        cache.session_selectors = sessions
            .into_iter()
            .flat_map(|session| [session.name, session.id])
            .collect();
        cache.session_selectors.sort();
        cache.session_selectors.dedup();
        cache.provider_models.clear();
        for (metadata, _) in &all_providers {
            let mut models: Vec<String> = metadata
                .known_models
                .iter()
                .map(|m| m.name.clone())
                .collect();

            if let Some(inv_models) = inventory_models.get(&metadata.name) {
                for model_id in inv_models {
                    if !models.contains(model_id) {
                        models.push(model_id.clone());
                    }
                }
            }

            if let Some(model) = configured_models.get(&metadata.name) {
                if !models.contains(model) {
                    models.push(model.clone());
                }
            }

            cache.provider_models.insert(metadata.name.clone(), models);
        }

        cache.last_updated = Instant::now();
        Ok(())
    }

    /// Invalidate the completion cache
    /// This should be called when extensions are added or removed
    async fn invalidate_completion_cache(&self) {
        let mut cache = self.completion_cache.write().unwrap();
        cache.prompts.clear();
        cache.prompt_info.clear();
        cache.last_updated = Instant::now();
    }

    pub fn message_history(&self) -> Conversation {
        self.messages.clone()
    }

    /// Render all past messages from the session history
    pub fn render_message_history(&self) {
        let messages = self.messages.user_visible_messages();
        if messages.is_empty() {
            return;
        }

        println!(
            "\n  {} {}",
            console::style("↻").cyan(),
            console::style(format!("{} messages restored", messages.len())).dim()
        );

        // Render each message
        for message in &messages {
            output::render_message(message, self.debug);
        }

        println!();
    }

    pub async fn get_session(&self) -> Result<goose::session::Session> {
        self.agent
            .config
            .session_manager
            .get_session(&self.session_id, false)
            .await
    }

    pub async fn get_total_token_usage(&self) -> Result<Option<i32>> {
        let metadata = self.get_session().await?;
        Ok(metadata.accumulated_usage.total_tokens)
    }

    /// Display enhanced context usage with session totals
    pub async fn display_context_usage(&self) -> Result<()> {
        let provider = self.agent.provider().await?;
        let model_config = self
            .agent
            .model_config_for_session(&self.session_id)
            .await?;
        let context_limit = provider
            .get_context_limit(&model_config)
            .await
            .unwrap_or_else(|_| model_config.context_limit());

        let config = Config::global();
        let show_cost = config
            .get_param::<bool>("GOOSE_CLI_SHOW_COST")
            .unwrap_or(false);

        let provider_name = config
            .get_goose_provider()
            .unwrap_or_else(|_| "unknown".to_string());

        match self.get_session().await {
            Ok(metadata) => {
                let total_tokens = metadata.usage.total_tokens.unwrap_or(0) as usize;

                output::display_context_usage(total_tokens, context_limit);

                if show_cost {
                    output::display_cost_usage(
                        &provider_name,
                        &model_config.model_name,
                        &metadata.usage,
                    );
                }
            }
            Err(_) => {
                output::display_context_usage(0, context_limit);
            }
        }

        Ok(())
    }

    /// Handle prompt command execution
    async fn handle_prompt_command(&mut self, opts: input::PromptCommandOptions) -> Result<()> {
        // name is required
        if opts.name.is_empty() {
            output::render_error("Prompt name argument is required");
            return Ok(());
        }

        if opts.info {
            match self.get_prompt_info(&opts.name).await? {
                Some(info) => output::render_prompt_info(&info),
                None => output::render_error(&format!("Prompt '{}' not found", opts.name)),
            }
        } else {
            // Convert the arguments HashMap to a Value
            let arguments = serde_json::to_value(opts.arguments)
                .map_err(|e| anyhow::anyhow!("Failed to serialize arguments: {}", e))?;

            match self.get_prompt(&opts.name, arguments).await {
                Ok(messages) => {
                    let start_len = self.messages.len();
                    let mut valid = true;
                    let num_messages = messages.len();
                    for (i, prompt_message) in messages.into_iter().enumerate() {
                        let msg = Message::from(prompt_message);
                        // ensure we get a User - Assistant - User type pattern
                        let expected_role = if i % 2 == 0 {
                            rmcp::model::Role::User
                        } else {
                            rmcp::model::Role::Assistant
                        };

                        if msg.role != expected_role {
                            output::render_error(&format!(
                                "Expected {:?} message at position {}, but found {:?}",
                                expected_role, i, msg.role
                            ));
                            valid = false;
                            // get rid of everything we added to messages
                            self.messages.truncate(start_len);
                            break;
                        }

                        if msg.role == rmcp::model::Role::User {
                            output::render_message(&msg, self.debug);
                        }
                        self.push_message(msg);
                    }

                    if valid {
                        if num_messages > 1 {
                            for i in 0..(num_messages - 1) {
                                let msg = &self.messages.messages()[start_len + i];
                                self.agent
                                    .config
                                    .session_manager
                                    .add_message(&self.session_id, msg)
                                    .await?;
                            }
                        }

                        output::show_thinking();
                        self.process_agent_response(true, CancellationToken::default())
                            .await?;
                        output::hide_thinking();
                    }
                }
                Err(e) => output::render_error(&e.to_string()),
            }
        }

        Ok(())
    }

    /// Save a recipe to a file
    ///
    /// # Arguments
    /// * `recipe` - The recipe to save
    /// * `filepath_str` - The path to save the recipe to
    ///
    /// # Returns
    /// * `Result<PathBuf, String>` - The path the recipe was saved to or an error message
    fn save_recipe(
        &self,
        recipe: &goose::recipe::Recipe,
        filepath_str: &str,
    ) -> anyhow::Result<PathBuf> {
        let path_buf = PathBuf::from(filepath_str);
        let mut path = path_buf.clone();

        // Update the final path if it's relative
        if path_buf.is_relative() {
            // If the path is relative, resolve it relative to the current working directory
            let cwd = std::env::current_dir().context("Failed to get current directory")?;
            path = cwd.join(&path_buf);
        }

        // Check if parent directory exists
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                return Err(anyhow::anyhow!(
                    "Directory '{}' does not exist",
                    parent.display()
                ));
            }
        }

        // Try creating the file
        let file = std::fs::File::create(path.as_path())
            .context(format!("Failed to create file '{}'", path.display()))?;

        // Write YAML
        serde_yaml::to_writer(file, recipe).context("Failed to save recipe")?;

        Ok(path)
    }

    fn push_message(&mut self, message: Message) {
        self.messages.push(message);
    }
}

fn message_has_text(message: &Message) -> bool {
    message.content.iter().any(
        |content| matches!(content, MessageContent::Text(text) if !text.text.trim().is_empty()),
    )
}

fn print_run_stats(
    run_started: Instant,
    first_token_at: Option<Instant>,
    usage: Option<&ProviderUsage>,
) {
    let elapsed = run_started.elapsed();
    let stats = usage.and_then(|usage| usage.stats.as_ref());
    let generation_elapsed = stats
        .and_then(|stats| stats.elapsed_ms)
        .map(Duration::from_millis);
    let output_tokens = usage
        .and_then(|usage| usage.usage.output_tokens)
        .and_then(|tokens| usize::try_from(tokens).ok())
        .or_else(|| stats.and_then(|stats| stats.output_tokens));
    let tokens_per_second = output_tokens.map(|tokens| {
        let rate_elapsed = generation_elapsed.unwrap_or(elapsed);
        if rate_elapsed.as_secs_f64() > 0.0 {
            tokens as f64 / rate_elapsed.as_secs_f64()
        } else {
            0.0
        }
    });
    let model_load_ms = stats.and_then(|stats| stats.model_load_ms);
    let generation_time_to_first_token_ms = stats.and_then(|stats| stats.time_to_first_token_ms);

    eprintln!("\nStats:");
    if let Some(ms) = model_load_ms {
        eprintln!("  Model load: {:.2}s", ms as f64 / 1000.0);
    }
    if model_load_ms.is_some() {
        match generation_time_to_first_token_ms {
            Some(ms) => eprintln!(
                "  Generation time to first token: {:.2}s",
                ms as f64 / 1000.0
            ),
            None => eprintln!("  Generation time to first token: unavailable"),
        }
        match first_token_at {
            Some(first) => eprintln!(
                "  End-to-end time to first token: {:.2}s",
                first.duration_since(run_started).as_secs_f64()
            ),
            None => eprintln!("  End-to-end time to first token: unavailable"),
        }
    } else if let Some(ms) = generation_time_to_first_token_ms {
        eprintln!("  Time to first token: {:.2}s", ms as f64 / 1000.0);
    } else {
        match first_token_at {
            Some(first) => eprintln!(
                "  Time to first token: {:.2}s",
                first.duration_since(run_started).as_secs_f64()
            ),
            None => eprintln!("  Time to first token: unavailable"),
        }
    }
    match tokens_per_second {
        Some(rate) => eprintln!("  Tokens/sec: {:.2}", rate),
        None => eprintln!("  Tokens/sec: unavailable"),
    }
    if let Some(tokens) = output_tokens {
        eprintln!("  Output tokens: {tokens}");
    }

    if let Some(draft) = stats.and_then(|stats| stats.draft.as_ref()) {
        eprintln!("  Draft accept rate: {:.1}%", draft.accept_rate * 100.0);
        eprintln!(
            "  Draft tokens: {} accepted: {} target verified: {} rounds: {}",
            draft.draft_tokens, draft.accepted_tokens, draft.target_tokens, draft.rounds
        );
        if let Some(model) = &draft.model {
            eprintln!("  Draft model: {model}");
        }
    }
}

fn maybe_open_credits_top_up_url(
    message: &Message,
    interactive: bool,
    prompted_credits_urls: &mut HashSet<String>,
) {
    if !interactive || !std::io::stdout().is_terminal() {
        return;
    }

    let Some(url) = output::get_credits_top_up_url(message) else {
        return;
    };

    if !prompted_credits_urls.insert(url.clone()) {
        return;
    }

    let should_open = cliclack::confirm("Open the top-up URL in your browser?")
        .initial_value(false)
        .interact()
        .unwrap_or(false);

    if should_open && webbrowser::open(&url).is_err() {
        output::render_text(
            "Could not open browser automatically. Visit the URL above.",
            Some(Color::Yellow),
            true,
        );
    }
}

fn emit_stream_event(event: &StreamEvent) {
    if let Ok(json) = serde_json::to_string(event) {
        println!("{}", json);
    }
}

/// Prompt user for tool call confirmation, returns the Permission selected
fn prompt_tool_confirmation(security_prompt: &Option<String>) -> Result<Permission> {
    output::hide_thinking();

    let prompt = if let Some(security_message) = security_prompt {
        println!("\n{}", security_message);
        "Do you allow this tool call?".to_string()
    } else {
        "Goose would like to call the above tool, do you allow?".to_string()
    };

    let permission_result = if security_prompt.is_none() {
        cliclack::select(prompt)
            .item(Permission::AllowOnce, "Allow", "Allow the tool call once")
            .item(
                Permission::AlwaysAllow,
                "Always Allow",
                "Always allow the tool call",
            )
            .item(Permission::DenyOnce, "Deny", "Deny the tool call")
            .item(
                Permission::Cancel,
                "Cancel",
                "Cancel the AI response and tool call",
            )
            .interact()
    } else {
        cliclack::select(prompt)
            .item(Permission::AllowOnce, "Allow", "Allow the tool call once")
            .item(Permission::DenyOnce, "Deny", "Deny the tool call")
            .item(
                Permission::Cancel,
                "Cancel",
                "Cancel the AI response and tool call",
            )
            .interact()
    };

    match permission_result {
        Ok(p) => Ok(p),
        Err(e) => {
            if e.kind() == std::io::ErrorKind::Interrupted {
                Ok(Permission::Cancel)
            } else {
                Err(e.into())
            }
        }
    }
}

/// Extract tool confirmation request from a message
fn find_tool_confirmation(message: &Message) -> Option<(String, Option<String>)> {
    message.content.iter().find_map(|content| {
        if let MessageContent::ActionRequired(action) = content {
            if let ActionRequiredData::ToolConfirmation { id, prompt, .. } = &action.data {
                return Some((id.clone(), prompt.clone()));
            }
        }
        None
    })
}

/// Extract elicitation request from a message
fn find_elicitation_request(message: &Message) -> Option<(String, String, Value)> {
    message.content.iter().find_map(|content| {
        if let MessageContent::ActionRequired(action) = content {
            if let ActionRequiredData::Elicitation {
                id,
                message,
                requested_schema,
            } = &action.data
            {
                return Some((id.clone(), message.clone(), requested_schema.clone()));
            }
        }
        None
    })
}

/// Handle MCP notification event (logging or progress)
#[expect(deprecated)]
fn handle_mcp_notification(
    extension_id: &str,
    notification: &ServerNotification,
    progress_bars: &mut output::McpSpinners,
    is_stream_json_mode: bool,
    interactive: bool,
    is_json_mode: bool,
    debug: bool,
) {
    match notification {
        ServerNotification::LoggingMessageNotification(log_notif) => {
            if let Some(obj) = log_notif.params.data.as_object() {
                if obj.get("type").and_then(|v| v.as_str()) == Some(SUBAGENT_TOOL_REQUEST_TYPE) {
                    if let (Some(subagent_id), Some(tool_call)) = (
                        obj.get("subagent_id").and_then(|v| v.as_str()),
                        obj.get("tool_call").and_then(|v| v.as_object()),
                    ) {
                        let tool_name = tool_call
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        let arguments = tool_call
                            .get("arguments")
                            .and_then(|v| v.as_object())
                            .cloned();

                        if interactive {
                            let _ = progress_bars.hide();
                        }
                        if is_stream_json_mode {
                            emit_stream_event(&StreamEvent::Notification {
                                extension_id: extension_id.to_string(),
                                data: NotificationData::Log {
                                    message: output::format_subagent_tool_call_message(
                                        subagent_id,
                                        tool_name,
                                    ),
                                },
                            });
                            return;
                        }
                        if !is_json_mode {
                            output::render_subagent_tool_call(
                                subagent_id,
                                tool_name,
                                arguments.as_ref(),
                                debug,
                            );
                            return;
                        }
                    }
                }
            }

            let (formatted, subagent_id, notif_type) =
                format_logging_notification(&log_notif.params.data, debug);

            if is_stream_json_mode {
                emit_stream_event(&StreamEvent::Notification {
                    extension_id: extension_id.to_string(),
                    data: NotificationData::Log {
                        message: formatted.clone(),
                    },
                });
            } else {
                display_log_notification(
                    &formatted,
                    subagent_id.as_deref(),
                    notif_type.as_deref(),
                    progress_bars,
                    interactive,
                    is_json_mode,
                );
            }
        }
        ServerNotification::ProgressNotification(prog_notif) => {
            if is_stream_json_mode {
                emit_stream_event(&StreamEvent::Notification {
                    extension_id: extension_id.to_string(),
                    data: NotificationData::Progress {
                        progress: prog_notif.params.progress,
                        total: prog_notif.params.total,
                        message: prog_notif.params.message.clone(),
                    },
                });
            } else {
                progress_bars.update(
                    &prog_notif.params.progress_token.0.to_string(),
                    prog_notif.params.progress,
                    prog_notif.params.total,
                    prog_notif.params.message.as_deref(),
                );
            }
        }
        ServerNotification::CustomNotification(notification) => {
            if let Some(params) = parse_shell_output_notification(notification) {
                if is_stream_json_mode
                    || is_json_mode
                    || !interactive
                    || !std::io::stdout().is_terminal()
                {
                    return;
                }
                display_shell_output_notification(params, progress_bars);
            }
        }
        _ => (),
    }
}

fn display_shell_output_notification(
    params: ShellOutputNotificationParams,
    progress_bars: &mut output::McpSpinners,
) {
    if params.truncated {
        return;
    }

    let max_width = console::Term::stdout()
        .size_checked()
        .map(|(_, width)| usize::from(width).saturating_sub(SHELL_STATUS_RESERVED_WIDTH))
        .unwrap_or(SHELL_STATUS_FALLBACK_WIDTH);
    let lines = latest_shell_output_lines(&params, max_width)
        .into_iter()
        .map(|(stream, line)| match stream {
            ShellOutputStream::Stdout => console::style(line).dim().to_string(),
            ShellOutputStream::Stderr => console::style(line).yellow().dim().to_string(),
        })
        .collect::<Vec<_>>();
    if !lines.is_empty() {
        progress_bars.log_shell_output(lines, SHELL_STATUS_MAX_LINES);
    }
}

fn latest_shell_output_lines(
    params: &ShellOutputNotificationParams,
    max_width: usize,
) -> Vec<(ShellOutputStream, String)> {
    let mut lines = params
        .chunks
        .iter()
        .rev()
        .flat_map(|chunk| {
            chunk
                .output
                .lines()
                .rev()
                .map(move |line| (chunk.stream, line))
        })
        .take(SHELL_STATUS_MAX_LINES)
        .map(|(stream, line)| {
            let line = output::sanitize_terminal_line(line);
            (stream, safe_truncate(&line, max_width))
        })
        .collect::<Vec<_>>();
    lines.reverse();
    lines
}

/// Format a logging notification from MCP, returns (formatted_message, subagent_id, notification_type)
fn format_logging_notification(
    data: &Value,
    debug: bool,
) -> (String, Option<String>, Option<String>) {
    match data {
        Value::String(s) => (s.clone(), None, None),
        Value::Object(o) => {
            if let Some(Value::String(msg)) = o.get("message") {
                let subagent_id = o.get("subagent_id").and_then(|v| v.as_str());
                let notification_type = o.get("type").and_then(|v| v.as_str());

                let formatted = match notification_type {
                    Some("subagent_created") | Some("completed") | Some("terminated") => {
                        format!("🤖 {}", msg)
                    }
                    Some("tool_usage") | Some("tool_completed") | Some("tool_error") => {
                        format!("🔧 {}", msg)
                    }
                    Some("message_processing") | Some("turn_progress") => {
                        format!("💭 {}", msg)
                    }
                    Some("response_generated") => {
                        let config = Config::global();
                        let min_priority = config
                            .get_param::<f32>("GOOSE_CLI_MIN_PRIORITY")
                            .ok()
                            .unwrap_or(output::DEFAULT_MIN_PRIORITY);

                        if min_priority > 0.1 && !debug {
                            if let Some(response_content) = msg.strip_prefix("Responded: ") {
                                format!("🤖 Responded: {}", safe_truncate(response_content, 100))
                            } else {
                                format!("🤖 {}", msg)
                            }
                        } else {
                            format!("🤖 {}", msg)
                        }
                    }
                    _ => msg.to_string(),
                };
                (
                    formatted,
                    subagent_id.map(str::to_string),
                    notification_type.map(str::to_string),
                )
            } else if let Some(Value::String(output)) = o.get("output") {
                let notification_type = o.get("type").and_then(|v| v.as_str()).map(str::to_string);
                (output.to_owned(), None, notification_type)
            } else if let Some(result) = format_task_execution_notification(data) {
                result
            } else {
                (data.to_string(), None, None)
            }
        }
        v => (v.to_string(), None, None),
    }
}

/// Display a logging notification based on its type and context
fn display_log_notification(
    formatted_message: &str,
    subagent_id: Option<&str>,
    notification_type: Option<&str>,
    progress_bars: &mut output::McpSpinners,
    interactive: bool,
    is_json_mode: bool,
) {
    if subagent_id.is_some() {
        if interactive {
            let _ = progress_bars.hide();
            if !is_json_mode {
                println!("{}", console::style(formatted_message).green().dim());
            }
        } else if !is_json_mode {
            progress_bars.log(formatted_message);
        }
    } else if let Some(ntype) = notification_type {
        if ntype == TASK_EXECUTION_NOTIFICATION_TYPE {
            if interactive {
                let _ = progress_bars.hide();
            }
            if !is_json_mode {
                for line in formatted_message.lines() {
                    println!("    {}", console::style(line).dim());
                }
                std::io::stdout().flush().unwrap();
            }
        } else if ntype == "shell_output" {
            let config = Config::global();
            let min_priority = config
                .get_param::<f32>("GOOSE_CLI_MIN_PRIORITY")
                .ok()
                .unwrap_or(output::DEFAULT_MIN_PRIORITY);

            if min_priority < 0.1 {
                if interactive {
                    let _ = progress_bars.hide();
                }
                if !is_json_mode {
                    println!("    {}", console::style(formatted_message).dim());
                }
            }
        }
    } else if output::is_showing_thinking() {
        output::set_thinking_message(&formatted_message.to_string());
    } else {
        progress_bars.log(formatted_message);
    }
}

/// Log tool request/response metrics
fn log_tool_metrics(message: &Message, messages: &Conversation) {
    for content in &message.content {
        if let MessageContent::ToolRequest(tool_request) = content {
            if let Ok(tool_call) = &tool_request.tool_call {
                tracing::info!(
                    monotonic_counter.goose.tool_calls = 1,
                    tool_name = %tool_call.name,
                    "Tool call started"
                );
            }
        }
        if let MessageContent::ToolResponse(tool_response) = content {
            let tool_name = messages
                .iter()
                .rev()
                .find_map(|msg| {
                    msg.content.iter().find_map(|c| {
                        if let MessageContent::ToolRequest(req) = c {
                            if req.id == tool_response.id {
                                req.tool_call.as_ref().ok().map(|tc| tc.name.clone())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                })
                .unwrap_or_else(|| "unknown".to_string().into());

            let result_status = if tool_response.tool_result.is_ok() {
                "success"
            } else {
                "error"
            };
            tracing::info!(
                monotonic_counter.goose.tool_completions = 1,
                tool_name = %tool_name,
                result = %result_status,
                "Tool call completed"
            );
        }
    }
}

/// Handle and display an agent error
fn handle_agent_error(e: &anyhow::Error, is_stream_json_mode: bool) {
    let error_msg = e.to_string();

    if is_stream_json_mode {
        emit_stream_event(&StreamEvent::Error {
            error: error_msg.clone(),
        });
    }

    if e.downcast_ref::<goose_providers::errors::ProviderError>()
        .map(|provider_error| {
            matches!(
                provider_error,
                goose_providers::errors::ProviderError::ContextLengthExceeded(_)
            )
        })
        .unwrap_or(false)
    {
        if !is_stream_json_mode {
            output::render_text(
                "Compaction requested. Should have happened in the agent!",
                Some(Color::Yellow),
                true,
            );
        }
        warn!("Compaction requested. Should have happened in the agent!");
    }

    if !is_stream_json_mode {
        eprintln!("Error: {}", error_msg);
    }
}

async fn get_reasoner(
) -> Result<(Arc<dyn Provider>, goose_providers::model::ModelConfig), anyhow::Error> {
    use goose::providers::create;

    let config = Config::global();

    // Try planner-specific provider first, fall back to default provider
    let provider = if let Ok(provider) = config.get_param::<String>("GOOSE_PLANNER_PROVIDER") {
        provider
    } else {
        println!("WARNING: GOOSE_PLANNER_PROVIDER not found. Using default provider...");
        config
            .get_goose_provider()
            .expect("No provider configured. Run 'goose configure' first")
    };

    // Try planner-specific model first, fall back to default model
    let model = if let Ok(model) = config.get_param::<String>("GOOSE_PLANNER_MODEL") {
        model
    } else {
        println!("WARNING: GOOSE_PLANNER_MODEL not found. Using default model...");
        config
            .get_goose_model()
            .expect("No model configured. Run 'goose configure' first")
    };

    let planner_context_limit = match env::var(GOOSE_PLANNER_CONTEXT_LIMIT)
        .ok()
        .map(|v| v.parse::<usize>())
    {
        Some(Ok(n)) if n >= 4096 => Some(n),
        Some(Ok(_)) => anyhow::bail!("{} must be at least 4096", GOOSE_PLANNER_CONTEXT_LIMIT),
        Some(Err(e)) => anyhow::bail!("{}: {}", GOOSE_PLANNER_CONTEXT_LIMIT, e),
        None => None,
    };

    let model_config =
        goose::model_config::model_config_from_user_config(&provider, model.as_str())?
            .with_context_limit(planner_context_limit);
    let extensions = goose::config::extensions::get_enabled_extensions_with_config(config);
    let reasoner = create(&provider, extensions).await?;

    Ok((reasoner, model_config))
}

/// Format elapsed time duration
/// Shows seconds if less than 60, otherwise shows minutes:seconds
fn format_elapsed_time(duration: std::time::Duration) -> String {
    let total_secs = duration.as_secs();
    if total_secs < 60 {
        format!("{:.2}s", duration.as_secs_f64())
    } else {
        let minutes = total_secs / 60;
        let seconds = total_secs % 60;
        format!("{}m {:02}s", minutes, seconds)
    }
}

fn build_switched_model_config(
    provider_name: &str,
    model_name: &str,
    current_model_config: &goose_providers::model::ModelConfig,
) -> Result<goose_providers::model::ModelConfig> {
    goose::model_config::model_config_from_user_config(provider_name, model_name)
        .map(|config| {
            config
                .with_temperature(current_model_config.temperature)
                .with_toolshim(current_model_config.toolshim)
                .with_toolshim_model(current_model_config.toolshim_model.clone())
        })
        .map_err(|e| anyhow::anyhow!("Failed to create model configuration: {e}"))
}

fn preserve_picker_thinking_effort(
    target: goose_providers::model::ModelConfig,
    current: &goose_providers::model::ModelConfig,
    configured: Option<ThinkingEffort>,
) -> goose_providers::model::ModelConfig {
    match current.thinking_effort().or(configured) {
        Some(effort) => target.with_thinking_effort(effort),
        None => target,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactcode_permission_names_map_to_goose_modes() {
        assert_eq!(permission_mode("ask"), Some(GooseMode::Approve));
        assert_eq!(
            permission_mode("accept-edit"),
            Some(GooseMode::SmartApprove)
        );
        assert_eq!(permission_mode("no-perms"), Some(GooseMode::Auto));
        assert_eq!(permission_mode("read-only"), Some(GooseMode::Chat));
        assert_eq!(permission_mode("anything-else"), None);
        for mode in [
            GooseMode::Approve,
            GooseMode::SmartApprove,
            GooseMode::Auto,
            GooseMode::Chat,
        ] {
            assert_eq!(permission_mode(permission_policy_name(mode)), Some(mode));
        }
    }

    #[test]
    fn governed_direct_agent_requires_task_consent_except_in_no_prompts_mode() {
        assert_eq!(
            governed_agent_requires_confirmation(true, GooseMode::Approve),
            Ok(true)
        );
        assert_eq!(
            governed_agent_requires_confirmation(true, GooseMode::SmartApprove),
            Ok(true)
        );
        assert_eq!(
            governed_agent_requires_confirmation(true, GooseMode::Auto),
            Ok(false)
        );
        assert!(governed_agent_requires_confirmation(true, GooseMode::Chat)
            .unwrap_err()
            .contains("read-only"));
    }

    #[test]
    fn standalone_direct_agent_keeps_existing_prompt_behavior() {
        for mode in [
            GooseMode::Approve,
            GooseMode::SmartApprove,
            GooseMode::Auto,
            GooseMode::Chat,
        ] {
            assert_eq!(governed_agent_requires_confirmation(false, mode), Ok(false));
        }
    }

    #[test]
    fn streaming_draft_preservation_keeps_nonempty_input_only() {
        let mut prefill = Some("existing".to_string());
        preserve_stream_draft(&mut prefill, String::new());
        assert_eq!(prefill.as_deref(), Some("existing"));

        preserve_stream_draft(&mut prefill, "unfinished guidance".to_string());
        assert_eq!(prefill.as_deref(), Some("unfinished guidance"));
    }

    #[test]
    fn governed_sessions_only_accept_the_gateway_shim_provider() {
        assert!(governed_provider_allowed(true, Some("openai")));
        assert!(governed_provider_allowed(true, None));
        assert!(!governed_provider_allowed(true, Some("anthropic")));
        assert!(!governed_provider_allowed(true, Some("openrouter")));
        assert!(governed_provider_allowed(false, Some("anthropic")));
    }

    #[test]
    fn code_rewind_respects_capability_and_approval_read_only_boundaries() {
        assert!(
            code_rewind_block_reason(true, Some("read_only"), GooseMode::Auto)
                .unwrap()
                .contains("capability")
        );
        assert!(
            code_rewind_block_reason(true, Some("read-only"), GooseMode::Auto)
                .unwrap()
                .contains("capability")
        );
        assert!(code_rewind_block_reason(false, None, GooseMode::Chat)
            .unwrap()
            .contains("approval"));
        assert_eq!(
            code_rewind_block_reason(true, Some("workspace_write"), GooseMode::Approve),
            None
        );
        assert_eq!(
            code_rewind_block_reason(true, Some("full_control"), GooseMode::Auto),
            None
        );
    }

    #[test]
    fn governed_no_prompts_requires_an_independently_read_only_bridge() {
        assert!(!governed_no_prompts_allowed(true, Some("workspace_write")));
        assert!(!governed_no_prompts_allowed(true, Some("full_control")));
        assert!(!governed_no_prompts_allowed(true, None));
        assert!(governed_no_prompts_allowed(true, Some("read_only")));
        assert!(governed_no_prompts_allowed(false, Some("workspace_write")));
    }

    #[test]
    fn deterministic_slash_tools_match_prefixed_registration_names() {
        assert!(slash_tool_matches(
            "exactcode_host__process.list",
            "process.list"
        ));
        assert!(slash_tool_matches("summon__delegate", "delegate"));
        assert!(!slash_tool_matches(
            "other__process.list.extra",
            "process.list"
        ));
        assert!(valid_process_id("proc-123_abc"));
        assert!(!valid_process_id(""));
        assert!(!valid_process_id("123;kill"));
        assert!(!valid_process_id("two ids"));
    }
    use goose::agents::extension::Envs;
    use goose::config::ExtensionConfig;
    use std::collections::HashMap;
    use std::time::Duration;
    use test_case::test_case;

    #[test]
    fn thinking_effort_cycle_wraps_after_max() {
        assert_eq!(
            next_thinking_effort(ThinkingEffort::Off),
            ThinkingEffort::Low
        );
        assert_eq!(
            next_thinking_effort(ThinkingEffort::Low),
            ThinkingEffort::Medium
        );
        assert_eq!(
            next_thinking_effort(ThinkingEffort::Medium),
            ThinkingEffort::High
        );
        assert_eq!(
            next_thinking_effort(ThinkingEffort::High),
            ThinkingEffort::XHigh
        );
        assert_eq!(
            next_thinking_effort(ThinkingEffort::XHigh),
            ThinkingEffort::Max
        );
        assert_eq!(
            next_thinking_effort(ThinkingEffort::Max),
            ThinkingEffort::Off
        );
    }

    #[test]
    fn working_directory_resolution_supports_relative_paths_and_dash() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("current");
        let sibling = temp.path().join("sibling");
        std::fs::create_dir_all(&current).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();

        assert_eq!(
            resolve_working_directory(Some("../sibling"), &current, None).unwrap(),
            sibling.canonicalize().unwrap()
        );
        assert_eq!(
            resolve_working_directory(Some("-"), &sibling, Some(&current)).unwrap(),
            current.canonicalize().unwrap()
        );
        assert!(resolve_working_directory(Some("missing"), &current, None).is_err());
    }

    #[test]
    fn governed_workspace_rejects_escape_and_developer_reenable() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let inside = root.join("nested");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let root_string = root.to_string_lossy().to_string();
        let _guard = env_lock::lock_env([
            (GOVERNED_SESSION_ENV, Some("1")),
            (GOVERNED_WORKSPACE_ENV, Some(root_string.as_str())),
        ]);

        assert_eq!(
            enforce_governed_workspace(&inside).unwrap(),
            inside.canonicalize().unwrap()
        );
        assert!(enforce_governed_workspace(&outside).is_err());
        assert!(governed_builtin_is_blocked("developer"));
        assert!(governed_builtin_is_blocked("todo, Developer"));
        assert!(!governed_builtin_is_blocked("todo"));
    }

    #[test]
    fn ordinary_goose_session_keeps_normal_extension_and_directory_behavior() {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let _guard = env_lock::lock_env([
            (GOVERNED_SESSION_ENV, None::<&str>),
            (GOVERNED_WORKSPACE_ENV, None::<&str>),
        ]);

        assert_eq!(
            enforce_governed_workspace(&outside).unwrap(),
            outside.canonicalize().unwrap()
        );
        assert!(!governed_builtin_is_blocked("developer"));
    }

    #[tokio::test]
    async fn session_selector_accepts_name_exact_id_and_unique_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let manager = SessionManager::new(temp.path().to_path_buf());
        let first = manager
            .create_session(
                temp.path().to_path_buf(),
                "first task".to_string(),
                SessionType::User,
                GooseMode::Auto,
            )
            .await
            .unwrap();
        let second = manager
            .create_session(
                temp.path().to_path_buf(),
                "second task".to_string(),
                SessionType::User,
                GooseMode::Auto,
            )
            .await
            .unwrap();
        let sessions = vec![first.clone(), second.clone()];

        assert_eq!(
            resolve_session_selector(&sessions, "first task")
                .unwrap()
                .id,
            first.id
        );
        assert_eq!(
            resolve_session_selector(&sessions, &second.id).unwrap().id,
            second.id
        );
        let unique_prefix = (1..=first.id.chars().count())
            .map(|end| first.id.chars().take(end).collect::<String>())
            .find(|prefix| {
                sessions
                    .iter()
                    .filter(|session| session.id.starts_with(prefix))
                    .count()
                    == 1
            })
            .unwrap();
        assert_eq!(
            resolve_session_selector(&sessions, &unique_prefix)
                .unwrap()
                .id,
            first.id
        );
        assert!(resolve_session_selector(&sessions, "missing").is_err());

        let mut ambiguous_a = first;
        let mut ambiguous_b = second;
        ambiguous_a.id = "shared-a".to_string();
        ambiguous_b.id = "shared-b".to_string();
        assert!(resolve_session_selector(&[ambiguous_a, ambiguous_b], "shared").is_err());

        let mut duplicate_a = sessions[0].clone();
        let mut duplicate_b = sessions[1].clone();
        duplicate_a.name = "duplicate".to_string();
        duplicate_b.name = "duplicate".to_string();
        assert!(resolve_session_selector(&[duplicate_a, duplicate_b], "duplicate").is_err());
    }

    #[test]
    fn worktree_diff_includes_tracked_staged_and_untracked_changes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "goose@example.invalid"]);
        git(&["config", "user.name", "Goose Test"]);
        std::fs::write(root.join("tracked.txt"), "before\n").unwrap();
        git(&["add", "tracked.txt"]);
        git(&["commit", "-qm", "initial"]);

        std::fs::write(root.join("tracked.txt"), "after\n").unwrap();
        git(&["add", "tracked.txt"]);
        std::fs::write(root.join("tracked.txt"), "after again\n").unwrap();
        std::fs::write(root.join("untracked.txt"), "brand new\n").unwrap();

        let diff = collect_worktree_diff(root).unwrap();
        assert!(diff.contains("-before"));
        assert!(diff.contains("+after again"));
        assert!(diff.contains("untracked.txt"));
        assert!(diff.contains("+brand new"));
    }

    #[test]
    fn review_diff_is_bounded_on_a_character_boundary() {
        let input = "é".repeat(REVIEW_DIFF_LIMIT);
        let (bounded, truncated) = bounded_review_diff(&input);
        assert!(truncated);
        assert!(bounded.len() <= REVIEW_DIFF_LIMIT);
        assert!(bounded.len() > REVIEW_DIFF_LIMIT - "é".len());
        assert!(bounded.is_char_boundary(bounded.len()));
    }

    #[test]
    fn planner_classification_excludes_user_only_content() {
        use rmcp::model::{Annotations, Role, TextContent};

        let user_only = TextContent::new("user-only plan")
            .with_annotations(Annotations::default().with_audience(vec![Role::User]));
        let assistant_only = TextContent::new("agent classification text")
            .with_annotations(Annotations::default().with_audience(vec![Role::Assistant]));
        let mixed = Message::assistant()
            .with_content(MessageContent::Text(user_only.clone()))
            .with_content(MessageContent::Text(assistant_only));

        assert_eq!(
            planner_classification_text(&mixed).unwrap(),
            "agent classification text"
        );
        assert!(planner_classification_text(
            &Message::assistant().with_content(MessageContent::Text(user_only))
        )
        .is_err());
    }

    #[test]
    fn planner_history_is_fixed_after_audience_projection() {
        use rmcp::model::{Annotations, Role, TextContent};

        let hidden_separator = MessageContent::Text(
            TextContent::new("hidden separator")
                .with_annotations(Annotations::default().with_audience(vec![Role::User])),
        );
        let history = Conversation::new_unvalidated([
            Message::user().with_text("first request"),
            Message::assistant().with_content(hidden_separator),
            Message::user().with_text("second request"),
        ]);

        let provider_messages = planner_provider_messages(&history).agent_visible_messages();

        assert_eq!(provider_messages.len(), 1);
        assert_eq!(provider_messages[0].role, Role::User);
        assert_eq!(
            provider_messages[0].as_concat_text(),
            "first request\nsecond request"
        );
        assert!(!provider_messages[0]
            .as_concat_text()
            .contains("hidden separator"));
    }

    #[test]
    fn planner_history_excludes_turn_context_events() {
        use goose::conversation::message::MessageMetadata;

        let history = Conversation::new_unvalidated([
            Message::user().with_text("plan the refactor"),
            Message::user()
                .with_text("<turn-context>cwd /repo, todo: ship v2</turn-context>")
                .with_metadata(MessageMetadata::agent_only().with_turn_context()),
            Message::assistant().with_text("on it"),
        ]);

        let provider_text = planner_provider_messages(&history)
            .agent_visible_messages()
            .iter()
            .map(|message| message.as_concat_text())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(provider_text.contains("plan the refactor"));
        assert!(
            !provider_text.contains("turn-context"),
            "the planner prompt has no turn-context instructions, so blocks must not reach it"
        );
    }

    #[test]
    fn test_format_elapsed_time_under_60_seconds() {
        // Test sub-second duration
        let duration = Duration::from_millis(500);
        assert_eq!(format_elapsed_time(duration), "0.50s");

        // Test exactly 1 second
        let duration = Duration::from_secs(1);
        assert_eq!(format_elapsed_time(duration), "1.00s");

        // Test 45.75 seconds
        let duration = Duration::from_millis(45750);
        assert_eq!(format_elapsed_time(duration), "45.75s");

        // Test 59.99 seconds
        let duration = Duration::from_millis(59990);
        assert_eq!(format_elapsed_time(duration), "59.99s");
    }

    #[test]
    fn test_format_elapsed_time_minutes() {
        // Test exactly 60 seconds (1 minute)
        let duration = Duration::from_secs(60);
        assert_eq!(format_elapsed_time(duration), "1m 00s");

        // Test 61 seconds (1 minute 1 second)
        let duration = Duration::from_secs(61);
        assert_eq!(format_elapsed_time(duration), "1m 01s");

        // Test 90 seconds (1 minute 30 seconds)
        let duration = Duration::from_secs(90);
        assert_eq!(format_elapsed_time(duration), "1m 30s");

        // Test 119 seconds (1 minute 59 seconds)
        let duration = Duration::from_secs(119);
        assert_eq!(format_elapsed_time(duration), "1m 59s");

        // Test 120 seconds (2 minutes)
        let duration = Duration::from_secs(120);
        assert_eq!(format_elapsed_time(duration), "2m 00s");

        // Test 605 seconds (10 minutes 5 seconds)
        let duration = Duration::from_secs(605);
        assert_eq!(format_elapsed_time(duration), "10m 05s");

        // Test 3661 seconds (61 minutes 1 second)
        let duration = Duration::from_secs(3661);
        assert_eq!(format_elapsed_time(duration), "61m 01s");
    }

    #[test]
    fn test_format_elapsed_time_edge_cases() {
        // Test zero duration
        let duration = Duration::from_secs(0);
        assert_eq!(format_elapsed_time(duration), "0.00s");

        // Test very small duration (1 millisecond)
        let duration = Duration::from_millis(1);
        assert_eq!(format_elapsed_time(duration), "0.00s");

        // Test fractional seconds are truncated for minute display
        // 60.5 seconds should still show as 1m 00s (not 1m 00.5s)
        let duration = Duration::from_millis(60500);
        assert_eq!(format_elapsed_time(duration), "1m 00s");
    }

    #[test_case(
        "/usr/bin/my-server",
        ExtensionConfig::Stdio {
            name: "my-server".into(),
            cmd: "/usr/bin/my-server".into(),
            args: vec![],
            envs: Envs::default(),
            env_keys: vec![],
            description: goose::config::DEFAULT_EXTENSION_DESCRIPTION.to_string(),
            timeout: Some(goose::config::DEFAULT_EXTENSION_TIMEOUT),
            cwd: None,
            bundled: None,
            available_tools: vec![],
        }
        ; "name_from_cmd_basename"
    )]
    #[test_case(
        "MY_SECRET=s3cret npx -y @modelcontextprotocol/server-everything",
        ExtensionConfig::Stdio {
            name: "npx".into(),
            cmd: "npx".into(),
            args: vec!["-y".into(), "@modelcontextprotocol/server-everything".into()],
            envs: Envs::new([("MY_SECRET".into(), "s3cret".into())].into()),
            env_keys: vec![],
            description: goose::config::DEFAULT_EXTENSION_DESCRIPTION.to_string(),
            timeout: Some(goose::config::DEFAULT_EXTENSION_TIMEOUT),
            cwd: None,
            bundled: None,
            available_tools: vec![],
        }
        ; "env_prefix_name_from_cmd"
    )]
    #[test_case(
        r#""/Applications/IntelliJ IDEA.app/Contents/jbr/Contents/Home/bin/java" -classpath "/path/with spaces/lib.jar" Main"#,
        ExtensionConfig::Stdio {
            name: "java".into(),
            cmd: "/Applications/IntelliJ IDEA.app/Contents/jbr/Contents/Home/bin/java".into(),
            args: vec!["-classpath".into(), "/path/with spaces/lib.jar".into(), "Main".into()],
            envs: Envs::default(),
            env_keys: vec![],
            description: goose::config::DEFAULT_EXTENSION_DESCRIPTION.to_string(),
            timeout: Some(goose::config::DEFAULT_EXTENSION_TIMEOUT),
            cwd: None,
            bundled: None,
            available_tools: vec![],
        }
        ; "quoted_path_with_spaces"
    )]
    fn test_parse_stdio_extension(input: &str, expected: ExtensionConfig) {
        assert_eq!(CliSession::parse_stdio_extension(input).unwrap(), expected);
    }

    #[test]
    fn test_parse_stdio_extension_no_command() {
        assert!(CliSession::parse_stdio_extension("").is_err());
    }

    #[test]
    fn test_build_switched_model_config_rebuilds_target_model_settings() {
        let _guard = env_lock::lock_env([
            ("GOOSE_MAX_TOKENS", None::<&str>),
            ("GOOSE_TEMPERATURE", None::<&str>),
            ("GOOSE_CONTEXT_LIMIT", None::<&str>),
            ("GOOSE_TOOLSHIM", None::<&str>),
            ("GOOSE_TOOLSHIM_OLLAMA_MODEL", None::<&str>),
        ]);

        let current_model_config = goose_providers::model::ModelConfig {
            model_name: "gpt-4o".to_string(),
            context_limit: Some(128_000),
            temperature: Some(0.25),
            max_tokens: Some(16_384),
            toolshim: true,
            toolshim_model: Some("qwen2.5-coder".to_string()),
            request_params: Some(HashMap::from([(
                "anthropic_beta".to_string(),
                serde_json::json!(["output-128k-2025-02-19"]),
            )])),
            reasoning: Some(false),
            request_headers: None,
        };

        let switched =
            build_switched_model_config("openai", "gpt-5.4", &current_model_config).unwrap();
        let expected = goose_providers::model::ModelConfig::new("gpt-5.4")
            .with_canonical_limits("openai")
            .with_temperature(Some(0.25))
            .with_toolshim(true)
            .with_toolshim_model(Some("qwen2.5-coder".to_string()));

        assert_eq!(switched.model_name, expected.model_name);
        assert_eq!(switched.context_limit, expected.context_limit);
        assert_eq!(switched.max_tokens, expected.max_tokens);
        assert_eq!(switched.request_params, expected.request_params);
        assert_eq!(switched.reasoning, expected.reasoning);
        assert_eq!(switched.temperature, Some(0.25));
        assert!(switched.toolshim);
        assert_eq!(switched.toolshim_model.as_deref(), Some("qwen2.5-coder"));
    }

    #[test]
    fn test_build_switched_model_config_detects_effort_suffix_change() {
        let _guard = env_lock::lock_env([
            ("GOOSE_MAX_TOKENS", None::<&str>),
            ("GOOSE_TEMPERATURE", None::<&str>),
            ("GOOSE_CONTEXT_LIMIT", None::<&str>),
            ("GOOSE_TOOLSHIM", None::<&str>),
            ("GOOSE_TOOLSHIM_OLLAMA_MODEL", None::<&str>),
            ("GOOSE_THINKING_EFFORT", None::<&str>),
        ]);

        let current = goose_providers::model::ModelConfig::new("gpt-5.4-high")
            .with_canonical_limits("openai");
        assert_eq!(current.model_name, "gpt-5.4");
        assert_eq!(
            current.thinking_effort(),
            Some(goose_providers::thinking::ThinkingEffort::High)
        );

        let switched = build_switched_model_config("openai", "gpt-5.4", &current).unwrap();

        assert_eq!(switched.model_name, current.model_name);
        assert_ne!(switched.thinking_effort(), current.thinking_effort());
    }

    #[test]
    fn model_picker_preserves_session_thinking_effort() {
        let current = goose_providers::model::ModelConfig::new("gpt-5.4")
            .with_thinking_effort(ThinkingEffort::Medium);
        let target = goose_providers::model::ModelConfig::new("gpt-5.6");

        let switched = preserve_picker_thinking_effort(target, &current, None);

        assert_eq!(switched.thinking_effort(), Some(ThinkingEffort::Medium));
    }

    #[test]
    fn test_split_command_args_windows_paths() {
        assert_eq!(
            goose::utils::split_command_args(r"C:\tools\mcp.exe --arg value").unwrap(),
            vec![r"C:\tools\mcp.exe", "--arg", "value"]
        );
        assert_eq!(
            goose::utils::split_command_args(r#""C:\Program Files\server\mcp.exe" --arg"#).unwrap(),
            vec![r"C:\Program Files\server\mcp.exe", "--arg"]
        );
    }

    #[test]
    fn test_split_command_args_unmatched_quote() {
        assert!(goose::utils::split_command_args(r#""unmatched"#).is_err());
    }

    #[test_case(
        "https://mcp.kiwi.com", 300,
        ExtensionConfig::StreamableHttp {
            name: "mcp_kiwi_com".into(),
            uri: "https://mcp.kiwi.com".into(),
            envs: Envs::default(),
            env_keys: vec![],
            headers: HashMap::new(),
            description: goose::config::DEFAULT_EXTENSION_DESCRIPTION.to_string(),
            timeout: Some(300),
            socket: None,
            bundled: None,
            available_tools: vec![],
        }
        ; "name_from_host"
    )]
    #[test_case(
        "http://localhost:8080/api", 300,
        ExtensionConfig::StreamableHttp {
            name: "localhost_8080_api".into(),
            uri: "http://localhost:8080/api".into(),
            envs: Envs::default(),
            env_keys: vec![],
            headers: HashMap::new(),
            description: goose::config::DEFAULT_EXTENSION_DESCRIPTION.to_string(),
            timeout: Some(300),
            socket: None,
            bundled: None,
            available_tools: vec![],
        }
        ; "port_and_path"
    )]
    #[test_case(
        "http://localhost:9090/other", 300,
        ExtensionConfig::StreamableHttp {
            name: "localhost_9090_other".into(),
            uri: "http://localhost:9090/other".into(),
            envs: Envs::default(),
            env_keys: vec![],
            headers: HashMap::new(),
            description: goose::config::DEFAULT_EXTENSION_DESCRIPTION.to_string(),
            timeout: Some(300),
            socket: None,
            bundled: None,
            available_tools: vec![],
        }
        ; "different_port_and_path"
    )]
    fn test_parse_streamable_http_extension(url: &str, timeout: u64, expected: ExtensionConfig) {
        assert_eq!(
            CliSession::parse_streamable_http_extension(url, timeout),
            expected
        );
    }
}
