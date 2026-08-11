//! Decides whether a completed response should finish, continue, or retry the turn.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::time::Duration;

use crate::agents::goal::{self, GoalState, GoalVerdict};
use crate::agents::retry::{
    execute_on_failure_command_with_timeout, execute_success_checks_with_timeout,
};
use crate::agents::state_machine::operation::{
    applied, ends_turn, messages_since_kickoff, not_applicable, yielded_with, Emitter, Operation,
    OperationResult, SlashCommand, StateEffect,
};
use crate::agents::types::RetryConfig;
use crate::conversation::message::{Message, MessageErrorKind, SystemNotificationType};
use crate::conversation::Conversation;
use crate::session::Session;
use tokio::sync::Mutex;

pub(super) const NUDGED: &str = "nudged";
pub(super) const ATTEMPTS: &str = "attempts";

fn retry_error(error: &str) -> Message {
    Message::assistant().with_error(
        MessageErrorKind::Other,
        format!("Retry logic encountered an error: {error}"),
    )
}

pub struct RetryOperation<'a> {
    goal: &'a Mutex<Option<String>>,
    grind: &'a Mutex<Option<String>>,
    retry_timeout: Duration,
    on_failure_timeout: Duration,
}

impl<'a> RetryOperation<'a> {
    pub fn new(
        goal: &'a Mutex<Option<String>>,
        grind: &'a Mutex<Option<String>>,
        retry_timeout: Duration,
        on_failure_timeout: Duration,
    ) -> Self {
        Self {
            goal,
            grind,
            retry_timeout,
            on_failure_timeout,
        }
    }

    fn retry_config(session: &Session) -> Option<&RetryConfig> {
        session
            .recipe
            .as_ref()
            .and_then(|recipe| recipe.retry.as_ref())
    }

    fn reset_conversation(conversation: &Conversation) -> Result<Conversation> {
        let messages = messages_since_kickoff(conversation)?;
        let kickoff = conversation.len() - messages.len();
        Ok(Conversation::new_unvalidated(
            conversation.messages()[..=kickoff].to_vec(),
        ))
    }

    fn goal_was_nudged(&self, messages: &[Message]) -> bool {
        messages
            .iter()
            .any(|message| self.message_meta(message, NUDGED).is_some())
    }

    /// Attempts are counted on the kickoff message because a retry replaces the
    /// conversation with everything up to and including it — the only message
    /// that outlives the attempt it belongs to.
    fn attempts(&self, messages: &[Message]) -> u32 {
        messages
            .first()
            .and_then(|message| self.message_meta(message, ATTEMPTS))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32
    }

    async fn run_goal_command(
        &self,
        command: &SlashCommand<'_>,
        session: &Session,
        conversation: &Conversation,
        emit: &Emitter,
    ) -> Result<OperationResult> {
        let params = command.params_str;
        let action = goal::parse_goal_command(params);
        let mut extension_data = session.extension_data.clone();
        let mut extension_changed = false;
        let mut started_goal = None;

        let current = GoalState::from_session(session);
        let response = match action {
            goal::GoalCommand::Inspect => match current {
                Some(mut state) => {
                    if state.invalidate_if_worktree_changed(&session.working_dir) {
                        state.write_to(&mut extension_data)?;
                        extension_changed = true;
                        *self.goal.lock().await = Some(state.objective.clone());
                    }
                    Message::assistant().with_text(state.render())
                }
                None => Message::assistant().with_text(
                    "No goal set. Use `/goal <description>` to set one; `/goal edit|pause|resume|clear` manages it.",
                ),
            },
            goal::GoalCommand::Start(objective) => {
                let state = GoalState::start(objective, &session.working_dir);
                state.write_to(&mut extension_data)?;
                extension_changed = true;
                *self.goal.lock().await = Some(state.objective.clone());
                let response = Message::assistant().with_text(format!(
                    "Verified goal started. Completion now requires a model completion verdict and fresh successful test, lint, typecheck, or build evidence.\n\n{}",
                    state.render()
                ));
                started_goal = Some(state);
                response
            }
            goal::GoalCommand::Edit(objective) => match current {
                Some(mut state) => {
                    state.edit(objective, &session.working_dir);
                    state.write_to(&mut extension_data)?;
                    extension_changed = true;
                    *self.goal.lock().await = Some(state.objective.clone());
                    let response = Message::assistant().with_text(format!(
                        "Goal objective edited; prior evidence was reset.\n\n{}",
                        state.render()
                    ));
                    response
                }
                None => Message::assistant()
                    .with_text("No goal set. Use `/goal <description>` to start one."),
            },
            goal::GoalCommand::Pause => match current {
                Some(mut state) => {
                    if state.pause() {
                        state.write_to(&mut extension_data)?;
                        extension_changed = true;
                        *self.goal.lock().await = None;
                        Message::assistant()
                            .with_text(format!("Goal paused.\n\n{}", state.render()))
                    } else {
                        Message::assistant().with_text("Only an active goal can be paused.")
                    }
                }
                None => Message::assistant().with_text("No goal set."),
            },
            goal::GoalCommand::Resume => match current {
                Some(mut state) => {
                    if state.resume() {
                        state.write_to(&mut extension_data)?;
                        extension_changed = true;
                        *self.goal.lock().await = Some(state.objective.clone());
                        let response = Message::assistant()
                            .with_text(format!("Goal resumed.\n\n{}", state.render()));
                        response
                    } else {
                        Message::assistant().with_text("Only a paused goal can be resumed.")
                    }
                }
                None => Message::assistant().with_text("No goal set."),
            },
            goal::GoalCommand::Abandon => {
                if let Some(mut state) = current {
                    state.abandon();
                    state.write_to(&mut extension_data)?;
                    extension_changed = true;
                }
                *self.goal.lock().await = None;
                Message::assistant().with_text(
                    "Goal abandoned and retained in the session evidence. The agent will finish normally.",
                )
            }
            goal::GoalCommand::Clear => {
                extension_data.extension_states.remove("exactcode_goal.v1");
                extension_changed = true;
                *self.goal.lock().await = None;
                Message::assistant().with_text(
                    "Goal and its retained evidence cleared from this session.",
                )
            }
            goal::GoalCommand::Invalid(message) => Message::assistant().with_text(message),
        };

        let command_message = messages_since_kickoff(conversation)?
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("slash command conversation has no kickoff message"))?;
        let message_id = command_message
            .id
            .clone()
            .ok_or_else(|| anyhow!("Persisted slash command message has no id"))?;
        emit.message(command_message.with_visibility(true, false))
            .await;
        let response = emit.message(response.with_visibility(true, false)).await;

        let mut effects = vec![
            StateEffect::SetMessageVisibility {
                message_id,
                user_visible: true,
                agent_visible: false,
            },
            response.into(),
        ];
        if extension_changed {
            effects.push(StateEffect::SetExtensionData(extension_data));
        }
        if let Some(goal) = started_goal {
            effects.push(
                Message::user()
                    .with_text(goal::kickoff_prompt(&goal))
                    .with_visibility(false, true)
                    .into(),
            );
            applied(effects)
        } else {
            yielded_with(effects)
        }
    }
}

#[async_trait]
impl Operation for RetryOperation<'_> {
    fn name(&self) -> &'static str {
        "retry"
    }

    async fn run_command(
        &self,
        command: &SlashCommand<'_>,
        session: &Session,
        conversation: &Conversation,
        emit: &Emitter,
    ) -> Result<OperationResult> {
        if command.command == "goal" {
            return self
                .run_goal_command(command, session, conversation, emit)
                .await;
        }

        let target = match command.command {
            "grind" => &self.grind,
            _ => return not_applicable(),
        };
        let label = "grind goal";
        let params = command.params_str;
        let starts_turn = !params.is_empty() && !matches!(params, "off" | "clear" | "none");

        let response = if params.is_empty() {
            match target.lock().await.clone() {
                Some(value) => Message::assistant().with_text(format!("Current {label}: {value}")),
                None => Message::assistant().with_text(format!(
                    "No {label} set. Use `/{command_name} <description>` to set one.",
                    command_name = command.command
                )),
            }
        } else if !starts_turn {
            *target.lock().await = None;
            Message::assistant().with_text("Grind cleared. The agent will finish normally.")
        } else {
            *target.lock().await = Some(params.to_string());
            Message::assistant().with_text(format!(
                "Grind goal set. The agent will keep working until max_turns is reached:\n\n> {params}"
            ))
        };

        let command_message = messages_since_kickoff(conversation)?
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("slash command conversation has no kickoff message"))?;
        let message_id = command_message
            .id
            .clone()
            .ok_or_else(|| anyhow!("Persisted slash command message has no id"))?;
        let command_message = command_message.with_visibility(true, false);
        let response = response.with_visibility(true, false);
        emit.message(command_message).await;
        let response = emit.message(response).await;

        let mut effects = vec![
            StateEffect::SetMessageVisibility {
                message_id,
                user_visible: true,
                agent_visible: false,
            },
            response.into(),
        ];
        if starts_turn {
            effects.push(
                Message::user()
                    .with_text(format!(
                        "Start working toward this goal now:\n\n**Goal:** {params}"
                    ))
                    .with_visibility(false, true)
                    .into(),
            );
        } else {
            return yielded_with(effects);
        }
        applied(effects)
    }

    async fn run(
        &self,
        session: &Session,
        conversation: &Conversation,
        emit: &Emitter,
    ) -> Result<OperationResult> {
        let messages = messages_since_kickoff(conversation)?;
        if !ends_turn(messages) {
            return not_applicable();
        }

        if let Some(mut state) = GoalState::from_session(session).filter(GoalState::is_active) {
            let assistant_text = messages
                .iter()
                .rev()
                .find(|message| message.role == rmcp::model::Role::Assistant)
                .map(Message::as_concat_text)
                .unwrap_or_default();
            match goal::parse_verdict(&assistant_text) {
                GoalVerdict::Complete => {
                    let mut extension_data = session.extension_data.clone();
                    if state.evaluate_completion(&session.working_dir, conversation) {
                        state.write_to(&mut extension_data)?;
                        *self.goal.lock().await = None;
                        emit.message(Message::assistant().with_system_notification(
                            SystemNotificationType::InlineMessage,
                            "Goal VERIFIED: completion claim and objective evidence both passed.",
                        ))
                        .await;
                        return applied([StateEffect::SetExtensionData(extension_data)]);
                    }
                    state.write_to(&mut extension_data)?;
                    let mut message = Message::user()
                        .with_text(goal::verification_nudge(&state))
                        .with_visibility(false, true);
                    self.set_message_meta(&mut message, NUDGED, serde_json::json!(true));
                    emit.message(Message::assistant().with_system_notification(
                        SystemNotificationType::InlineMessage,
                        format!(
                            "Goal evidence rejected: {}",
                            state.evidence.failure_reason()
                        ),
                    ))
                    .await;
                    return applied([
                        StateEffect::SetExtensionData(extension_data),
                        message.into(),
                    ]);
                }
                GoalVerdict::Blocked => {
                    state.block("the agent reported GOAL_STATUS: blocked");
                    let mut extension_data = session.extension_data.clone();
                    state.write_to(&mut extension_data)?;
                    *self.goal.lock().await = None;
                    emit.message(Message::assistant().with_system_notification(
                        SystemNotificationType::InlineMessage,
                        "Goal BLOCKED and retained with its evidence.",
                    ))
                    .await;
                    return applied([StateEffect::SetExtensionData(extension_data)]);
                }
                GoalVerdict::Continue => {
                    let mut message = Message::user()
                        .with_text(goal::verification_nudge(&state))
                        .with_visibility(false, true);
                    self.set_message_meta(&mut message, NUDGED, serde_json::json!(true));
                    emit.message(Message::assistant().with_system_notification(
                        SystemNotificationType::InlineMessage,
                        format!("Goal ACTIVE: {}", state.objective),
                    ))
                    .await;
                    return applied([message.into()]);
                }
                GoalVerdict::Unspecified if !self.goal_was_nudged(messages) => {
                    let mut message = Message::user()
                        .with_text(goal::verification_nudge(&state))
                        .with_visibility(false, true);
                    self.set_message_meta(&mut message, NUDGED, serde_json::json!(true));
                    emit.message(Message::assistant().with_system_notification(
                        SystemNotificationType::InlineMessage,
                        format!("Goal ACTIVE: {}", state.objective),
                    ))
                    .await;
                    return applied([message.into()]);
                }
                GoalVerdict::Unspecified => {}
            }
        } else if !self.goal_was_nudged(messages) {
            // Compatibility for callers that still set the transient Agent goal directly.
            if let Some(goal) = self.goal.lock().await.clone() {
                let mut message = Message::user()
                    .with_text(format!(
                        "Before finishing, check whether this goal is fully met:\n\n**Goal:** {goal}"
                    ))
                    .with_visibility(false, true);
                self.set_message_meta(&mut message, NUDGED, serde_json::json!(true));
                return applied([message.into()]);
            }
        }

        if let Some(grind) = self.grind.lock().await.clone() {
            let nudge = format!(
                "Keep working. The grind goal is not yet complete:\n\n\
                 **Goal:** {grind}\n\n\
                 Continue until it is fully done."
            );
            let message = Message::user()
                .with_text(&nudge)
                .with_visibility(false, true);
            emit.message(Message::assistant().with_system_notification(
                SystemNotificationType::InlineMessage,
                format!("Grind: {grind}"),
            ))
            .await;
            return applied([message.into()]);
        }

        if GoalState::from_session(session).is_none() {
            *self.goal.lock().await = None;
        }
        *self.grind.lock().await = None;

        let Some(retry_config) = Self::retry_config(session) else {
            return not_applicable();
        };

        let retry_timeout = retry_config
            .timeout_seconds
            .map(Duration::from_secs)
            .unwrap_or(self.retry_timeout);
        let success =
            execute_success_checks_with_timeout(&retry_config.checks, retry_timeout).await;
        let success = match success {
            Ok(success) => success,
            Err(error) => {
                let message = emit.message(retry_error(&error.to_string())).await;
                return applied([message.into()]);
            }
        };
        if success {
            return not_applicable();
        }

        let attempts = self.attempts(messages);
        if attempts >= retry_config.max_retries {
            let message = Message::assistant().with_error(
                MessageErrorKind::Other,
                format!(
                    "Maximum retry attempts ({}) exceeded. Unable to complete the task successfully.",
                    retry_config.max_retries
                ),
            );
            #[cfg(feature = "telemetry")]
            crate::posthog::emit_error(
                "retry_max_exceeded",
                &format!("Max retries ({}) exceeded", retry_config.max_retries),
            );
            let message = emit.message(message).await;
            return applied([message.into()]);
        }

        if let Some(command) = &retry_config.on_failure {
            let timeout = retry_config
                .on_failure_timeout_seconds
                .map(Duration::from_secs)
                .unwrap_or(self.on_failure_timeout);
            if let Err(error) = execute_on_failure_command_with_timeout(command, timeout).await {
                let message = emit.message(retry_error(&error.to_string())).await;
                return applied([message.into()]);
            }
        }

        let mut reset = Self::reset_conversation(conversation)?;
        if let Some(kickoff) = reset.messages_mut().last_mut() {
            self.set_message_meta(kickoff, ATTEMPTS, serde_json::json!(attempts + 1));
        }
        applied([reset.into()])
    }
}
