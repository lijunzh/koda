//! `/review`: a code review of the current changes, as Codex's `/review`
//! runs one.
//!
//! The review runs in a session of its own, so the work's context stays
//! clean: Codex's rubric is its `developer_instructions`, and the request
//! is Codex's, for the target named:
//!
//! - `/review`: the uncommitted changes (staged, unstaged, untracked);
//! - `/review <branch>`: the changes against that branch, from the merge
//!   base;
//! - `/review <commit>`: the changes that commit made;
//! - `/review <anything else>`: those words, as the reviewer's instructions.
//!
//! It runs on `review_model` when the config file names one, else on the
//! session's own model. The TUI shows the review as it runs, then goes back
//! to the main session, deletes the review's, and lists the findings. The
//! main session's next prompt carries them, in Codex's words, so "fix the
//! P1" works.
//!
//! # Provenance
//!
//! `prompts/review_rubric.md` is Codex's `prompts/templates/review/rubric.md`
//! verbatim, and the request and results texts are Codex's
//! (`prompts/src/review_request.rs`, `prompts/templates/review/*.xml`), at
//! openai/codex `05ae1b0fdd5ab412d24248acc76c5d9f6f275440`, under the
//! Apache License 2.0 (see `NOTICE`).

use std::path::Path;

use koda_client::events::Outgoing;
use koda_client::protocol::methods;
use koda_client::protocol::turn::TurnStatus;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use super::side::SideState;
use super::{CommandOutcome, TuiContext, TuiSession};
use crate::tui_output;

/// Codex's review rubric, verbatim.
pub(crate) const REVIEW_RUBRIC: &str = include_str!("prompts/review_rubric.md");

const UNCOMMITTED_PROMPT: &str = "Review the current code changes (staged, unstaged, and \
untracked files) and provide prioritized findings.";

/// What `/review` reviews.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReviewTarget {
    /// The working tree's changes.
    Uncommitted,
    /// The changes against `branch`, from their merge base.
    BaseBranch {
        branch: String,
        merge_base: Option<String>,
    },
    /// What one commit changed.
    Commit { sha: String, title: Option<String> },
    /// The reviewer's instructions, as typed.
    Custom(String),
}

impl ReviewTarget {
    /// The request the reviewer gets, in Codex's words.
    pub(crate) fn prompt(&self) -> String {
        match self {
            Self::Uncommitted => UNCOMMITTED_PROMPT.to_string(),
            Self::BaseBranch {
                branch,
                merge_base: Some(sha),
            } => format!(
                "Review the code changes against the base branch '{branch}'. The merge base \
                 commit for this comparison is {sha}. Run `git diff {sha}` to inspect the \
                 changes relative to {branch}. Provide prioritized, actionable findings."
            ),
            Self::BaseBranch {
                branch,
                merge_base: None,
            } => format!(
                "Review the code changes against the base branch '{branch}'. Start by finding \
                 the merge diff between the current branch and {branch}'s upstream e.g. (`git \
                 merge-base HEAD \"$(git rev-parse --abbrev-ref \"{branch}@{{upstream}}\")\"`), \
                 then run `git diff` against that SHA to see what changes we would merge into \
                 the {branch} branch. Provide prioritized, actionable findings."
            ),
            Self::Commit {
                sha,
                title: Some(title),
            } => format!(
                "Review the code changes introduced by commit {sha} (\"{title}\"). Provide \
                 prioritized, actionable findings."
            ),
            Self::Commit { sha, title: None } => format!(
                "Review the code changes introduced by commit {sha}. Provide prioritized, \
                 actionable findings."
            ),
            Self::Custom(instructions) => instructions.clone(),
        }
    }

    /// What the review is of, for the banner and the results, as Codex's
    /// TUI names it.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Uncommitted => "current changes".to_string(),
            Self::BaseBranch { branch, .. } => format!("changes against '{branch}'"),
            Self::Commit { sha, title } => {
                let short = &sha[..7.min(sha.len())];
                match title {
                    Some(title) => format!("commit {short}: {title}"),
                    None => format!("commit {short}"),
                }
            }
            Self::Custom(instructions) => instructions.trim().to_string(),
        }
    }
}

/// Run git in `dir` for one line of output.
fn git_line(dir: &Path, args: &[&str]) -> Option<String> {
    let output = koda_git::command(dir, koda_git::Operation::Metadata)
        .ok()?
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// What `/review <arg>` names in the repository at `dir`: a branch first
/// (a branch is a commit too), then a commit, else instructions.
pub(crate) fn resolve_target(dir: &Path, arg: &str) -> ReviewTarget {
    let arg = arg.trim();
    if arg.is_empty() {
        return ReviewTarget::Uncommitted;
    }
    let one_word = !arg.contains(char::is_whitespace);
    let is_branch = one_word
        && ["refs/heads/", "refs/remotes/"].iter().any(|prefix| {
            git_line(
                dir,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{prefix}{arg}"),
                ],
            )
            .is_some()
        });
    if is_branch {
        return ReviewTarget::BaseBranch {
            branch: arg.to_string(),
            merge_base: git_line(dir, &["merge-base", "HEAD", arg]),
        };
    }
    let commit = one_word
        .then(|| {
            git_line(
                dir,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{arg}^{{commit}}"),
                ],
            )
        })
        .flatten();
    match commit {
        Some(sha) => ReviewTarget::Commit {
            title: git_line(dir, &["log", "-1", "--format=%s", &sha]),
            sha,
        },
        None => ReviewTarget::Custom(arg.to_string()),
    }
}

/// The review's output, as Codex's rubric asks for it.
#[derive(Debug, serde::Deserialize)]
struct ReviewOutput {
    #[serde(default)]
    findings: Vec<Finding>,
    overall_correctness: Option<String>,
    overall_explanation: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct Finding {
    title: String,
    #[serde(default)]
    body: String,
    priority: Option<u8>,
    code_location: Option<CodeLocation>,
}

#[derive(Debug, serde::Deserialize)]
struct CodeLocation {
    absolute_file_path: String,
    line_range: Option<LineRange>,
}

#[derive(Debug, serde::Deserialize)]
struct LineRange {
    start: u64,
    end: u64,
}

/// The JSON object in a reply, fenced or not.
fn parse_output(reply: &str) -> Option<ReviewOutput> {
    let start = reply.find('{')?;
    let end = reply.rfind('}')?;
    serde_json::from_str(reply.get(start..=end)?).ok()
}

/// The findings as lines for the scroll buffer: the verdict, then each
/// finding with its place, paths relative to `root`. `None` when the reply
/// isn't the JSON the rubric asks for.
fn findings_lines(reply: &str, root: &Path) -> Option<Vec<Line<'static>>> {
    let output = parse_output(reply)?;
    let mut lines = Vec::new();
    if output.findings.is_empty() {
        lines.push(Line::from("  No findings."));
    }
    for finding in &output.findings {
        let title = match finding.priority {
            // The rubric puts the tag in the title; add it when it doesn't.
            Some(p) if !finding.title.starts_with('[') => format!("[P{p}] {}", finding.title),
            _ => finding.title.clone(),
        };
        let place = finding.code_location.as_ref().map(|location| {
            let path = Path::new(&location.absolute_file_path);
            let path = path
                .strip_prefix(root)
                .unwrap_or(path)
                .display()
                .to_string();
            match &location.line_range {
                Some(range) if range.end > range.start => {
                    format!("{path}:{}-{}", range.start, range.end)
                }
                Some(range) => format!("{path}:{}", range.start),
                None => path,
            }
        });
        let mut spans = vec![Span::styled(
            format!("  \u{2022} {title}"),
            Style::default().fg(Color::Yellow),
        )];
        if let Some(place) = place {
            spans.push(Span::styled(format!("  {place}"), tui_output::DIM));
        }
        lines.push(Line::from(spans));
        for body in finding.body.lines().filter(|l| !l.trim().is_empty()) {
            lines.push(Line::styled(format!("    {body}"), tui_output::DIM));
        }
    }
    if let Some(verdict) = &output.overall_correctness {
        let explanation = output.overall_explanation.as_deref().unwrap_or_default();
        lines.push(Line::from(format!("  Verdict: {verdict}. {explanation}")));
    }
    Some(lines)
}

/// What the main session's next prompt carries after a review, in Codex's
/// words (`exit_success.xml`).
fn results_for_the_model(reply: &str) -> String {
    format!(
        "<user_action>\n  <context>User initiated a review task. Here's the full review output \
         from reviewer model. User may select one or more comments to resolve.</context>\n  \
         <action>review</action>\n  <results>\n  {reply}\n  </results>\n</user_action>"
    )
}

impl TuiContext {
    /// `/review [target]`: open a review session and run the review there.
    pub(crate) async fn enter_review(&mut self, arg: &str) -> CommandOutcome {
        if self.side.is_some() {
            tui_output::warn_msg(
                &mut self.scroll_buffer,
                "Finish the side conversation first: Ctrl+C on an empty prompt returns."
                    .to_string(),
            );
            return CommandOutcome::Handled;
        }
        let target = resolve_target(&self.project_root, arg);
        // `review_model`, else the session's own model.
        let review_model = self
            .client
            .request::<methods::ConfigRead>(Default::default())
            .await
            .ok()
            .and_then(|read| read.config.review_model);
        let model = review_model.unwrap_or_else(|| {
            self.settings
                .alias
                .clone()
                .unwrap_or_else(|| self.settings.model.clone())
        });
        let config = std::collections::BTreeMap::from([(
            "developer_instructions".to_string(),
            serde_json::Value::String(REVIEW_RUBRIC.to_string()),
        )]);
        let started = self
            .client
            .request::<methods::SessionStart>(koda_client::protocol::session::SessionStartParams {
                model_provider: Some(self.settings.model_provider.clone()),
                base_url: Some(self.settings.base_url.clone()),
                model: Some(model),
                cwd: Some(self.client.project_root().display().to_string()),
                config: Some(config),
                // Its findings go back to the main session; it isn't kept.
                ephemeral: Some(true),
                ..Default::default()
            })
            .await;
        let started = match started {
            Ok(started) => started.session,
            Err(e) => {
                tui_output::err_msg(
                    &mut self.scroll_buffer,
                    format!("Couldn't start the review: {e}"),
                );
                return CommandOutcome::Handled;
            }
        };
        let label = target.label();
        let main = std::mem::replace(
            &mut self.session,
            TuiSession {
                id: started.id.clone(),
                instruction_sources: started.instruction_sources.clone().unwrap_or_default(),
            },
        );
        self.host_session = started.id.clone();
        self.side = Some(SideState::new(
            main,
            self.settings.clone(),
            None,
            Some(label.clone()),
        ));
        self.scroll_buffer.push_lines([
            Line::default(),
            Line::styled(
                format!("  \u{2500}\u{2500}\u{2500} review: {label} \u{2500}\u{2500}\u{2500}"),
                tui_output::DIM,
            ),
            Line::default(),
        ]);
        if let Err(e) = self.sync_settings(true).await {
            tui_output::warn_msg(&mut self.scroll_buffer, format!("{e:#}"));
        }
        self.prepare_inference_start(&target.prompt()).await
    }

    /// After a turn: if it was a review's, go back to the main session and
    /// list the findings, which the next prompt there carries. Returns the
    /// turn Koda started there meanwhile, on its goal say, for the caller to
    /// follow, with what it asked.
    pub(crate) async fn finish_review(&mut self) -> Option<(String, Vec<Outgoing>)> {
        let label = self.side.as_ref().and_then(|side| side.review.clone())?;
        let (status, reply) = self
            .last_turn
            .take()
            .unwrap_or((TurnStatus::Interrupted, None));
        // Started at the prompt, but Koda may have started a turn on the
        // main session's goal since.
        let running = self.leave_side().await;
        let reply = reply.filter(|_| status == TurnStatus::Completed);
        let Some(reply) = reply else {
            tui_output::warn_msg(
                &mut self.scroll_buffer,
                "The review didn't finish. Run /review again and let it complete.".to_string(),
            );
            return running;
        };
        self.scroll_buffer.push_lines([
            Line::default(),
            Line::styled(
                format!("  Review of {label}"),
                Style::default().fg(Color::Cyan),
            ),
        ]);
        match findings_lines(&reply, &self.project_root) {
            Some(lines) => self.scroll_buffer.push_lines(lines),
            None => self.scroll_buffer.push_lines(
                reply
                    .lines()
                    .map(|l| Line::from(format!("  {l}")))
                    .collect::<Vec<_>>(),
            ),
        }
        tui_output::dim_msg(
            &mut self.scroll_buffer,
            "Your next message here comes with the review, so you can ask to fix findings."
                .to_string(),
        );
        self.pending_review = Some(results_for_the_model(&reply));
        running
    }

    /// The main session's next prompt, with the review's results ahead of it.
    pub(crate) fn with_review_results(&mut self, message: String) -> String {
        if self.side.is_some() {
            return message;
        }
        match self.pending_review.take() {
            Some(results) => format!("{results}\n\n{message}"),
            None => message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(status.status.success(), "git {args:?}: {status:?}");
    }

    #[test]
    fn a_target_is_a_branch_then_a_commit_then_instructions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.txt"), "a").unwrap();
        git(root, &["add", "a.txt"]);
        git(root, &["commit", "-q", "-m", "first commit"]);
        git(root, &["checkout", "-q", "-b", "topic"]);
        std::fs::write(root.join("a.txt"), "b").unwrap();
        git(root, &["commit", "-q", "-am", "second commit"]);

        assert_eq!(resolve_target(root, "  "), ReviewTarget::Uncommitted);
        let ReviewTarget::BaseBranch { branch, merge_base } = resolve_target(root, "main") else {
            panic!("main is a branch");
        };
        assert_eq!(branch, "main");
        let base = merge_base.expect("a merge base");
        assert!(
            ReviewTarget::BaseBranch {
                branch,
                merge_base: Some(base.clone())
            }
            .prompt()
            .contains(&format!("git diff {base}"))
        );
        let ReviewTarget::Commit { sha, title } = resolve_target(root, "HEAD") else {
            panic!("HEAD is a commit");
        };
        assert_eq!(title.as_deref(), Some("second commit"));
        let commit = ReviewTarget::Commit { sha, title };
        assert!(commit.label().starts_with("commit "), "{}", commit.label());
        assert!(commit.prompt().contains("(\"second commit\")"));
        assert_eq!(
            resolve_target(root, "look at error handling"),
            ReviewTarget::Custom("look at error handling".into())
        );
    }

    #[test]
    fn findings_read_from_the_rubrics_json() {
        let reply = r#"```json
{"findings": [{"title": "[P1] Off by one in the loop", "body": "The loop skips the last item.",
  "confidence_score": 0.9, "priority": 1,
  "code_location": {"absolute_file_path": "/repo/src/lib.rs", "line_range": {"start": 10, "end": 12}}}],
 "overall_correctness": "patch is incorrect", "overall_explanation": "It drops data.",
 "overall_confidence_score": 0.8}
```"#;
        let lines = findings_lines(reply, Path::new("/repo")).unwrap();
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert!(text[0].contains("[P1] Off by one in the loop"), "{text:?}");
        assert!(text[0].contains("src/lib.rs:10-12"), "{text:?}");
        assert!(text[1].contains("skips the last item"), "{text:?}");
        assert!(
            text.last().unwrap().contains("patch is incorrect"),
            "{text:?}"
        );
        assert!(findings_lines("Looks fine to me.", Path::new("/")).is_none());
        assert!(results_for_the_model("R").contains("<results>\n  R\n"));
        assert!(REVIEW_RUBRIC.starts_with("# Review guidelines:"));
    }
}
