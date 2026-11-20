//! `/goal`: a goal Koda keeps working on, turn after turn, until the model
//! finds it achieved, as Codex's `/goal`.
//!
//! - `/goal` shows the session's goal.
//! - `/goal <objective>` sets it, replacing any goal the session has.
//! - `/goal pause`, `/goal resume` and `/goal clear` stop it, go on with it,
//!   and remove it.
//! - `/goal budget <tokens>` limits the tokens it may use; `/goal budget
//!   off` removes the limit.
//!
//! It runs during a turn too. An interrupt (Esc) stops the turn, not the
//! goal: Koda waits for the next message, or `/goal resume`, before it goes
//! on. `/goal pause` is how the goal is stopped. What changes is said
//! as the host reports it (`session/goal/updated`), whoever changed it: the
//! user, the model, or Koda as it stops a goal that can't go on.

use koda_client::RequestHandle;
use koda_client::events::Outgoing;
use koda_client::protocol::ServerNotification;
use koda_client::protocol::goal::{
    Goal, GoalStatus, GoalVerdict, SessionGoalClearParams, SessionGoalGetParams,
    SessionGoalSetParams,
};
use koda_client::protocol::methods;

use super::TuiContext;
use crate::scroll_buffer::ScrollBuffer;
use crate::tui_output;

/// Run `/goal` with what follows it.
pub(crate) async fn command(
    requests: &RequestHandle,
    session_id: &str,
    arg: &str,
    buffer: &mut ScrollBuffer,
) {
    let arg = arg.trim();
    let set =
        |status: Option<GoalStatus>, token_budget: Option<Option<i64>>| SessionGoalSetParams {
            session_id: session_id.to_string(),
            objective: None,
            status,
            token_budget,
        };
    let result = match arg
        .split_once(' ')
        .map_or((arg, ""), |(a, b)| (a, b.trim()))
    {
        ("", _) => {
            show(requests, session_id, buffer).await;
            return;
        }
        ("pause", "") => send(requests, set(Some(GoalStatus::Paused), None)).await,
        ("resume", "") => send(requests, set(Some(GoalStatus::Active), None)).await,
        ("clear", "") => requests
            .request::<methods::SessionGoalClear>(SessionGoalClearParams {
                session_id: session_id.to_string(),
            })
            .await
            .map(|cleared| {
                if !cleared.cleared {
                    tui_output::dim_msg(buffer, "This session has no goal.".into());
                }
            })
            .map_err(|e| e.to_string()),
        ("budget", "off") => send(requests, set(None, Some(None))).await,
        ("budget", tokens) => match parse_tokens(tokens) {
            Some(tokens) => send(requests, set(None, Some(Some(tokens)))).await,
            None => Err("Give the budget in tokens, such as 200000 or 200k, or `off`.".into()),
        },
        _ => replace(requests, session_id, arg).await,
    };
    if let Err(why) = result {
        tui_output::err_msg(buffer, format!("Goal: {why}"));
    }
}

async fn send(requests: &RequestHandle, params: SessionGoalSetParams) -> Result<(), String> {
    requests
        .request::<methods::SessionGoalSet>(params)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Set a new goal, clearing the one the session has first, so the new one
/// starts with nothing used.
async fn replace(
    requests: &RequestHandle,
    session_id: &str,
    objective: &str,
) -> Result<(), String> {
    requests
        .request::<methods::SessionGoalClear>(SessionGoalClearParams {
            session_id: session_id.to_string(),
        })
        .await
        .map_err(|e| e.to_string())?;
    send(
        requests,
        SessionGoalSetParams {
            session_id: session_id.to_string(),
            objective: Some(objective.to_string()),
            status: Some(GoalStatus::Active),
            token_budget: None,
        },
    )
    .await
}

async fn show(requests: &RequestHandle, session_id: &str, buffer: &mut ScrollBuffer) {
    let params = SessionGoalGetParams {
        session_id: session_id.to_string(),
    };
    match requests.request::<methods::SessionGoalGet>(params).await {
        Ok(answer) => match answer.goal {
            Some(goal) => {
                tui_output::ok_msg(
                    buffer,
                    format!("Goal ({}): {}", status_name(goal.status), goal.objective),
                );
                tui_output::dim_msg(buffer, format!("  {}", usage(&goal)));
            }
            None => tui_output::dim_msg(
                buffer,
                "No goal. `/goal <objective>` sets one; Koda then keeps working on it until \
                 it's done."
                    .into(),
            ),
        },
        Err(e) => tui_output::err_msg(buffer, format!("Couldn't read the goal: {e}")),
    }
}

/// `200000`, `200k` or `1.5m` tokens.
fn parse_tokens(text: &str) -> Option<i64> {
    let text = text.trim().to_ascii_lowercase().replace([',', '_'], "");
    let (number, scale) = match text.chars().last()? {
        'k' => (&text[..text.len() - 1], 1_000.0),
        'm' => (&text[..text.len() - 1], 1_000_000.0),
        _ => (text.as_str(), 1.0),
    };
    let tokens = (number.parse::<f64>().ok()? * scale).round();
    (tokens >= 1.0 && tokens < i64::MAX as f64).then_some(tokens as i64)
}

fn status_name(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::Blocked => "blocked",
        GoalStatus::BudgetLimited => "budget used up",
        GoalStatus::Complete => "complete",
    }
}

/// `12.3K tokens of 200K, 4m 10s`.
fn usage(goal: &Goal) -> String {
    let tokens = match goal.token_budget {
        Some(budget) => format!("{} of {} tokens", tokens(goal.tokens_used), tokens(budget)),
        None => format!("{} tokens", tokens(goal.tokens_used)),
    };
    format!("{tokens}, {}", duration(goal.time_used_seconds))
}

fn tokens(count: i64) -> String {
    match count {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1_000_000.0),
        n if n >= 1_000 => format!("{:.1}K", n as f64 / 1_000.0),
        n => n.to_string(),
    }
}

fn duration(seconds: i64) -> String {
    match seconds {
        s if s >= 3_600 => format!("{}h {}m", s / 3_600, s % 3_600 / 60),
        s if s >= 60 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

/// `Background command exited (code 0): cargo test`.
fn exit_line(exit: &koda_client::protocol::background::BackgroundTerminalExit) -> String {
    let how = exit.exit_code.map_or_else(
        || "stopped by a signal".to_string(),
        |code| format!("code {code}"),
    );
    format!("Background command exited ({how}): {}", exit.command)
}

/// What the goal evaluator's verdict says (`goal_judge = "evaluator"`).
fn verdict(verdict: GoalVerdict, reason: Option<&str>) -> String {
    let what = match verdict {
        GoalVerdict::Met => "Goal check: met",
        GoalVerdict::NotYet => "Goal check: not met yet",
        GoalVerdict::Impossible => "Goal check: impossible",
    };
    match reason {
        Some(reason) => format!("{what}. {reason}"),
        None => format!("{what}."),
    }
}

/// What a change to the goal says, given the goal as last reported.
fn change(before: Option<&Goal>, now: &Goal) -> Option<String> {
    let same = before.filter(|before| {
        before.session_id == now.session_id && before.created_at == now.created_at
    });
    let Some(before) = same else {
        // A new goal has used nothing; one this TUI hasn't seen yet, as in
        // a resumed session, is shown as it stands.
        let fresh = now.tokens_used == 0 && now.time_used_seconds == 0;
        return Some(match now.status {
            GoalStatus::Active if fresh => format!("Goal set: {}", now.objective),
            status => format!("Goal ({}): {}", status_name(status), now.objective),
        });
    };
    if before.status == now.status {
        return (before.objective != now.objective)
            .then(|| format!("Goal changed: {}", now.objective))
            .or_else(|| {
                (before.token_budget != now.token_budget).then(|| format!("Goal: {}", usage(now)))
            });
    }
    Some(match now.status {
        GoalStatus::Active => "Goal resumed.".to_string(),
        GoalStatus::Paused => "Goal paused. `/goal resume` goes on with it.".to_string(),
        GoalStatus::Blocked => {
            "Goal blocked: no progress could be made. `/goal resume` tries again.".to_string()
        }
        GoalStatus::BudgetLimited => format!(
            "Goal stopped: its token budget is used up ({}). `/goal budget <tokens>` raises it.",
            usage(now)
        ),
        GoalStatus::Complete => format!("Goal complete ({}).", usage(now)),
    })
}

impl TuiContext {
    /// Say what changed when the host reports the session's goal changed,
    /// or that background commands exited (the model is being told).
    pub(crate) fn note_session(&mut self, message: &Outgoing) {
        let Outgoing::Notification(notification) = message else {
            return;
        };
        match notification {
            ServerNotification::SessionGoalUpdated(updated)
                if updated.session_id == self.session.id =>
            {
                let line = change(self.goal.as_ref(), &updated.goal);
                self.goal = Some(updated.goal.clone());
                if let Some(line) = line {
                    tui_output::ok_msg(&mut self.scroll_buffer, line);
                }
            }
            ServerNotification::SessionGoalJudged(judged)
                if judged.session_id == self.session.id =>
            {
                let line = verdict(judged.verdict, judged.reason.as_deref());
                match judged.verdict {
                    GoalVerdict::NotYet => tui_output::dim_msg(&mut self.scroll_buffer, line),
                    _ => tui_output::ok_msg(&mut self.scroll_buffer, line),
                }
            }
            ServerNotification::SessionBackgroundTerminalsExited(exited)
                if exited.session_id == self.session.id =>
            {
                for exit in &exited.data {
                    tui_output::dim_msg(&mut self.scroll_buffer, exit_line(exit));
                }
            }
            ServerNotification::SessionGoalHeld(held) if held.session_id == self.session.id => {
                tui_output::warn_msg(
                    &mut self.scroll_buffer,
                    format!(
                        "Goal on hold: {} turns in a row called no tool. It goes on after your \
                         next message, or /goal resume.",
                        held.turns
                    ),
                );
            }
            ServerNotification::SessionGoalCleared(cleared)
                if cleared.session_id == self.session.id =>
            {
                self.goal = None;
                tui_output::dim_msg(&mut self.scroll_buffer, "Goal cleared.".into());
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal(status: GoalStatus, objective: &str) -> Goal {
        Goal {
            session_id: "s".into(),
            objective: objective.into(),
            status,
            token_budget: Some(200_000),
            tokens_used: 12_345,
            time_used_seconds: 250,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn a_verdict_says_what_the_check_found() {
        assert_eq!(
            verdict(GoalVerdict::NotYet, Some("Lint still fails.")),
            "Goal check: not met yet. Lint still fails."
        );
        assert_eq!(verdict(GoalVerdict::Met, None), "Goal check: met.");
    }

    #[test]
    fn budgets_read_as_people_write_them() {
        assert_eq!(parse_tokens("200000"), Some(200_000));
        assert_eq!(parse_tokens("200k"), Some(200_000));
        assert_eq!(parse_tokens("1.5M"), Some(1_500_000));
        assert_eq!(parse_tokens("1,000"), Some(1_000));
        assert_eq!(parse_tokens("0"), None);
        assert_eq!(parse_tokens("lots"), None);
    }

    #[test]
    fn a_change_says_what_changed() {
        let active = goal(GoalStatus::Active, "ship");
        let new = Goal {
            tokens_used: 0,
            time_used_seconds: 0,
            ..active.clone()
        };
        assert_eq!(change(None, &new).unwrap(), "Goal set: ship");
        assert_eq!(
            change(None, &active).unwrap(),
            "Goal (active): ship",
            "resumed"
        );
        assert_eq!(change(Some(&active), &active), None, "only usage moved");
        let paused = goal(GoalStatus::Paused, "ship");
        assert!(
            change(Some(&active), &paused)
                .unwrap()
                .starts_with("Goal paused.")
        );
        let done = goal(GoalStatus::Complete, "ship");
        assert_eq!(
            change(Some(&active), &done).unwrap(),
            "Goal complete (12.3K of 200.0K tokens, 4m 10s)."
        );
        let edited = goal(GoalStatus::Active, "ship twice");
        assert_eq!(
            change(Some(&active), &edited).unwrap(),
            "Goal changed: ship twice"
        );
        let replaced = Goal {
            created_at: 2,
            ..goal(GoalStatus::Active, "other")
        };
        assert_eq!(
            change(Some(&active), &replaced).unwrap(),
            "Goal (active): other"
        );
    }
}
