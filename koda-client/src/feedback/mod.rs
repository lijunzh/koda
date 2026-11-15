//! `feedback/save`, as Codex's: a ZIP on the host with a report and, with
//! the user's consent, what a maintainer needs to debug the session.
//!
//! ```text
//! report.json                     category, note, session, versions
//! attachments/0000/koda-logs.log  the sessions' log lines (consent)
//! attachments/0001/session-….json each session's stored rows (consent)
//! attachments/…/diagnostics.txt   versions, sandbox, environment (consent)
//! manifest.json                   what each attachment is
//! ```
//!
//! The sessions are the one reported and its sub-agents', as Codex takes a
//! thread and its sub-agent threads. A session's stored rows stand in for
//! the rollout file Codex attaches: the data, not a rendering of it. The
//! ZIP is written as Codex writes it: to a new absolute path, through a
//! temp file, never replacing a file, within 200 MiB and 128 attachments.

mod env_filter;

use crate::session::{internal, invalid};
use koda_protocol::feedback::{FeedbackSaveParams, FeedbackSaveResponse};
use koda_protocol::rpc::ErrorObject;
use koda_state::db::Database;
use koda_state::session_store::SessionStore;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The categories a client offers, in Codex's names. Codex's fifth,
/// `safety_check`, is for OpenAI's own safety checks, which Koda has none
/// of; any other name is taken as given, as Codex takes it.
pub const CLASSIFICATIONS: [&str; 4] = ["bug", "bad_result", "good_result", "other"];

/// The most a ZIP holds, uncompressed, as Codex's: 200 MiB.
const MAX_BYTES: usize = 200 * 1024 * 1024;
/// The most attachments a ZIP holds, as Codex's.
const MAX_ATTACHMENTS: usize = 128;

/// One file in `attachments/`.
struct Attachment {
    filename: String,
    content_type: &'static str,
    bytes: Vec<u8>,
}

/// Answer `feedback/save` from the sessions in `db`.
pub(crate) async fn save(
    db: &Database,
    params: FeedbackSaveParams,
) -> Result<FeedbackSaveResponse, ErrorObject> {
    if params.classification.trim().is_empty() {
        return Err(invalid("classification is empty"));
    }
    let path = PathBuf::from(&params.save_path);
    if !path.is_absolute() {
        return Err(invalid("savePath must be absolute"));
    }
    if path.exists() {
        return Err(invalid(format!("{} already exists", path.display())));
    }

    let sessions = match &params.session_id {
        Some(id) => {
            let found = db
                .get_session(id)
                .await
                .map_err(|e| internal(format!("couldn't read the session: {e:#}")))?;
            if found.is_none() {
                return Err(invalid(format!("no session {id:?}")));
            }
            with_sub_agents(db, id)
                .await
                .map_err(|e| internal(format!("couldn't list its sub-agents: {e:#}")))?
        }
        None => Vec::new(),
    };

    let report = report(&params, &sessions);
    let mut attachments = Vec::new();
    if params.include_logs {
        let lines = koda_core::log_db::session_lines(&sessions)
            .await
            .map_err(|e| internal(format!("couldn't read the log: {e:#}")))?;
        let mut log = String::new();
        for line in &lines {
            log.push_str(&line.render());
            log.push('\n');
        }
        attachments.push(Attachment {
            filename: "koda-logs.log".into(),
            content_type: "text/plain",
            bytes: log.into_bytes(),
        });
        for id in &sessions {
            let stored = db
                .export_session(id)
                .await
                .map_err(|e| internal(format!("couldn't read session {id}: {e:#}")))?;
            attachments.push(Attachment {
                filename: format!("session-{id}.json"),
                content_type: "application/json",
                bytes: serde_json::to_vec_pretty(&stored).unwrap_or_default(),
            });
        }
        attachments.push(Attachment {
            filename: "diagnostics.txt".into(),
            content_type: "text/plain",
            bytes: diagnostics().into_bytes(),
        });
    }

    let target = path.clone();
    tokio::task::spawn_blocking(move || write_zip(&target, &report, &attachments))
        .await
        .map_err(|e| internal(format!("the ZIP writer stopped: {e}")))?
        .map_err(|e| internal(format!("couldn't write {}: {e:#}", path.display())))?;
    tracing::info!(
        path = %path.display(),
        classification = %params.classification,
        include_logs = params.include_logs,
        sessions = sessions.len(),
        "feedback saved"
    );
    Ok(FeedbackSaveResponse {
        saved_path: path.display().to_string(),
        session_id: params.session_id,
    })
}

/// `id` and every session it ran as a sub-agent, depth first, oldest
/// first.
async fn with_sub_agents(db: &Database, id: &str) -> anyhow::Result<Vec<String>> {
    let mut seen = HashSet::new();
    let mut order = Vec::new();
    let mut stack = vec![id.to_string()];
    while let Some(next) = stack.pop() {
        if !seen.insert(next.clone()) {
            continue;
        }
        let children = db.child_session_ids(&next).await?;
        order.push(next);
        stack.extend(children.into_iter().rev());
    }
    Ok(order)
}

/// `report.json`: Codex's report fields (title, level, category, note,
/// session, version) as plain JSON; Codex's is a Sentry event because it
/// uploads to Sentry, and Koda uploads nothing.
fn report(params: &FeedbackSaveParams, sessions: &[String]) -> Vec<u8> {
    let name = match params.classification.as_str() {
        "bug" => "Bug",
        "bad_result" => "Bad result",
        "good_result" => "Good result",
        "other" => "Other",
        other => other,
    };
    let title = match &params.session_id {
        Some(id) => format!("[{name}]: Koda session {id}"),
        None => format!("[{name}]: Koda"),
    };
    let level = match params.classification.as_str() {
        "bug" | "bad_result" => "error",
        _ => "info",
    };
    let created_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    let report = serde_json::json!({
        "title": title,
        "level": level,
        "classification": params.classification,
        "reason": params.reason,
        "sessionId": params.session_id,
        "subAgentSessionIds": sessions.iter().skip(1).collect::<Vec<_>>(),
        "includeLogs": params.include_logs,
        "kodaVersion": env!("CARGO_PKG_VERSION"),
        "gitSha": option_env!("KODA_GIT_SHA"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "createdAtMs": created_at_ms,
    });
    serde_json::to_vec_pretty(&report).unwrap_or_default()
}

/// `diagnostics.txt`: what a maintainer asks first. Keys appear by length
/// only, through the environment allowlist.
fn diagnostics() -> String {
    let sandbox = koda_core::sandbox::dependency_report();
    let log = match koda_core::log_db::installed() {
        Some((layer, _)) if layer.has_write_failure() => {
            "open; some lines failed to store, so the log is incomplete"
        }
        Some(_) => "open",
        None => "not open in this process",
    };
    let mut text = format!(
        "koda {}{}\nos: {} {}\nsandbox: {}, {}{}\nlog database: {log}\n\nEnvironment (allowlisted; keys shown by length only):\n",
        env!("CARGO_PKG_VERSION"),
        option_env!("KODA_GIT_SHA")
            .map(|sha| format!(" ({sha})"))
            .unwrap_or_default(),
        std::env::consts::OS,
        std::env::consts::ARCH,
        sandbox.backend,
        if sandbox.available {
            "available"
        } else {
            "unavailable"
        },
        sandbox
            .reason
            .map(|reason| format!(" ({reason})"))
            .unwrap_or_default(),
    );
    text.push_str(&env_filter::render(&env_filter::filter(std::env::vars())));
    text
}

/// Write the ZIP to `path` as Codex writes it: a temp file beside it,
/// published only when complete and never over an existing file, within
/// [`MAX_BYTES`] uncompressed and [`MAX_ATTACHMENTS`].
fn write_zip(path: &Path, report: &[u8], attachments: &[Attachment]) -> anyhow::Result<()> {
    use anyhow::Context;
    anyhow::ensure!(
        attachments.len() <= MAX_ATTACHMENTS,
        "{} attachments, over the limit of {MAX_ATTACHMENTS}",
        attachments.len()
    );
    let parent = path.parent().context("the path has no folder")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("couldn't write in {}", parent.display()))?;
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(true);
    let mut zip = zip::ZipWriter::new(temp.as_file_mut());
    let mut left = MAX_BYTES;
    let mut add = |zip: &mut zip::ZipWriter<&mut std::fs::File>,
                   name: &str,
                   bytes: &[u8]|
     -> anyhow::Result<()> {
        left = left
            .checked_sub(bytes.len())
            .context("the report is over the 200 MiB limit")?;
        zip.start_file(name, options)?;
        zip.write_all(bytes)?;
        Ok(())
    };
    add(&mut zip, "report.json", report)?;
    let mut manifest = Vec::new();
    for (index, attachment) in attachments.iter().enumerate() {
        let entry = format!("attachments/{index:04}/{}", attachment.filename);
        add(&mut zip, &entry, &attachment.bytes)?;
        manifest.push(serde_json::json!({
            "entry": entry,
            "filename": attachment.filename,
            "contentType": attachment.content_type,
        }));
    }
    let manifest = serde_json::to_vec_pretty(&manifest)?;
    add(&mut zip, "manifest.json", &manifest)?;
    zip.finish()?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    {
        // It can quote prompts, commands and output: owner-only.
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn entries(path: &Path) -> Vec<(String, String)> {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
        (0..zip.len())
            .map(|i| {
                let mut file = zip.by_index(i).unwrap();
                let mut text = String::new();
                file.read_to_string(&mut text).unwrap();
                (file.name().unwrap().to_string(), text)
            })
            .collect()
    }

    fn params(dir: &Path, session: Option<&str>, include_logs: bool) -> FeedbackSaveParams {
        FeedbackSaveParams {
            classification: "bug".into(),
            reason: Some("it hung".into()),
            session_id: session.map(str::to_string),
            include_logs,
            save_path: dir.join("report.zip").display().to_string(),
        }
    }

    /// With consent: the report, the logs, each session's rows (its
    /// sub-agent's too, compacted messages included) and diagnostics, with
    /// a manifest naming them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_report_with_logs_holds_the_sessions_and_their_sub_agents() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::init(dir.path()).await.unwrap();
        let parent = db.create_session("default", dir.path()).await.unwrap();
        let child = db.create_session("explore", dir.path()).await.unwrap();
        db.set_metadata(
            &child,
            koda_state::session_store::PARENT_SESSION_KEY,
            &parent,
        )
        .await
        .unwrap();
        db.insert_message(
            &parent,
            &koda_state::session_store::Role::User,
            Some("fix it"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        db.compact_session(&parent, "summary", 0).await.unwrap();

        let saved = save(&db, params(dir.path(), Some(&parent), true))
            .await
            .unwrap();
        assert_eq!(saved.session_id.as_deref(), Some(parent.as_str()));
        let files = entries(Path::new(&saved.saved_path));
        let names: Vec<_> = files.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "report.json",
                "attachments/0000/koda-logs.log",
                &format!("attachments/0001/session-{parent}.json"),
                &format!("attachments/0002/session-{child}.json"),
                "attachments/0003/diagnostics.txt",
                "manifest.json",
            ]
        );
        let report: serde_json::Value = serde_json::from_str(&files[0].1).unwrap();
        assert_eq!(report["classification"], "bug");
        assert_eq!(report["level"], "error");
        assert_eq!(report["reason"], "it hung");
        assert_eq!(report["subAgentSessionIds"][0], child.as_str());
        assert_eq!(report["title"], format!("[Bug]: Koda session {parent}"));

        let stored: serde_json::Value = serde_json::from_str(&files[2].1).unwrap();
        assert_eq!(stored["sessions"][0]["id"], parent.as_str());
        let messages = stored["messages"].as_array().unwrap();
        assert!(
            messages
                .iter()
                .any(|m| m["content"] == "fix it" && !m["compacted_at"].is_null()),
            "the compacted prompt is kept, marked: {messages:#?}"
        );
        assert!(files[4].1.contains("sandbox:"), "{}", files[4].1);
        let manifest: serde_json::Value = serde_json::from_str(&files[5].1).unwrap();
        assert_eq!(manifest[3]["filename"], "diagnostics.txt");
    }

    /// Without consent, only the report.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn without_consent_only_the_report_is_saved() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::init(dir.path()).await.unwrap();
        let session = db.create_session("default", dir.path()).await.unwrap();
        let saved = save(&db, params(dir.path(), Some(&session), false))
            .await
            .unwrap();
        let names: Vec<_> = entries(Path::new(&saved.saved_path))
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["report.json", "manifest.json"]);
    }

    /// A relative path, an existing file and an unknown session are
    /// refused, and nothing is replaced.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bad_paths_and_sessions_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::init(dir.path()).await.unwrap();
        let mut relative = params(dir.path(), None, false);
        relative.save_path = "report.zip".into();
        assert!(save(&db, relative).await.is_err());

        let existing = params(dir.path(), None, false);
        std::fs::write(&existing.save_path, "keep").unwrap();
        assert!(save(&db, existing.clone()).await.is_err());
        assert_eq!(
            std::fs::read_to_string(&existing.save_path).unwrap(),
            "keep"
        );

        let mut unknown = params(dir.path(), Some("no-such-session"), false);
        unknown.save_path = dir.path().join("other.zip").display().to_string();
        assert!(save(&db, unknown).await.is_err());
    }
}
