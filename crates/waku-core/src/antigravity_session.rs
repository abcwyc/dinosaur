//! Antigravity CLI (`agy`) model discovery and conversation catalog.
//!
//! agy keeps one summary row per conversation in
//! `~/.gemini/antigravity-cli/conversation_summaries.db` (SQLite): title,
//! workspace URIs, and timestamps. Transcripts live in per-conversation
//! databases whose steps are protobuf blobs with no published schema, so an
//! import carries the title only; agy itself still holds the full context and
//! resumes it with `--conversation <id>`.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::model::{
    ProviderKind, ProviderModel, ProviderResumeCursor, ProviderSessionHistory,
    ProviderSessionSummary,
};

fn cli_home() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".gemini/antigravity-cli"))
}

/// `agy models` prints one `id<TAB>name` line per model after a status line.
pub(crate) fn discover_models(binary: &Path) -> Vec<ProviderModel> {
    let mut command = crate::command_env::command(binary);
    let command = command.arg("models");
    let Ok(output) = crate::command_env::output(command) else {
        return Vec::new();
    };
    parse_models(&String::from_utf8_lossy(&output.stdout))
}

fn parse_models(output: &str) -> Vec<ProviderModel> {
    let mut models = output
        .lines()
        .filter_map(|line| {
            let (id, name) = line.split_once('\t')?;
            let id = id.trim();
            (!id.is_empty() && !id.contains(char::is_whitespace))
                .then(|| ProviderModel::new(id, name.trim()))
        })
        .collect::<Vec<_>>();
    if let Some(first) = models.first_mut() {
        *first = first.clone().default();
    }
    models
}

/// The project folder of a conversation: its first `file://` workspace that
/// is not a scratch directory some wrapper created for the run.
fn workspace_cwd(workspace_uris: &str) -> Option<PathBuf> {
    let uris = serde_json::from_str::<Vec<String>>(workspace_uris).ok()?;
    uris.iter()
        .filter_map(|uri| uri.strip_prefix("file://"))
        .map(percent_decode)
        .map(PathBuf::from)
        .find(|path| {
            path.is_absolute()
                && !path.starts_with("/var/folders")
                && !path.starts_with("/private/var/folders")
                && !path.starts_with("/tmp")
                && !path.starts_with(std::env::temp_dir())
        })
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16)
        {
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn seconds(timestamp: &str) -> u64 {
    chrono::DateTime::parse_from_str(timestamp, "%Y-%m-%d %H:%M:%S%.f%:z")
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(timestamp))
        .ok()
        .and_then(|time| u64::try_from(time.timestamp()).ok())
        .unwrap_or_default()
}

pub fn list_provider_sessions(limit: usize) -> anyhow::Result<Vec<ProviderSessionSummary>> {
    let Some(path) = cli_home().map(|home| home.join("conversation_summaries.db")) else {
        return Ok(Vec::new());
    };
    if limit == 0 || !path.is_file() {
        return Ok(Vec::new());
    }
    list_provider_sessions_in(&path, limit)
}

fn list_provider_sessions_in(
    path: &Path,
    limit: usize,
) -> anyhow::Result<Vec<ProviderSessionSummary>> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    // Sub-agent and battle-mode children are not conversations a person
    // resumes on their own.
    let mut statement = connection.prepare(
        "SELECT conversation_id, title, preview, workspace_uris,
                last_modified_time, last_user_input_time
           FROM conversation_summaries
          WHERE nesting_depth = 0 AND parent_conversation_id = ''
          ORDER BY last_modified_time DESC
          LIMIT ?1",
    )?;
    let rows = statement.query_map([limit as i64], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    Ok(rows
        .filter_map(Result::ok)
        .filter_map(|(id, title, preview, uris, modified, input)| {
            let cwd = workspace_cwd(&uris)?;
            let title = [title, preview]
                .into_iter()
                .map(|value| value.trim().to_owned())
                .find(|value| !value.is_empty())?;
            let updated_at = seconds(&modified);
            // agy records no creation time; the last input is the closest.
            let created_at = match seconds(&input) {
                0 => updated_at,
                input => input.min(updated_at),
            };
            Some(ProviderSessionSummary {
                cursor: ProviderResumeCursor::from_session_id(ProviderKind::Antigravity, id),
                title,
                cwd,
                created_at,
                updated_at,
            })
        })
        .collect())
}

/// agy's transcripts are protobuf without a published schema; the import
/// opens with an empty transcript and resumes agy's own context.
pub fn provider_session_history(_conversation_id: &str) -> ProviderSessionHistory {
    ProviderSessionHistory::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_parse_tab_separated_listing() {
        let models = parse_models(
            "Fetching available models...\ngemini-3.1-pro-high\tGemini 3.1 Pro (High)\nclaude-sonnet-4-6\tClaude Sonnet 4.6 (Thinking)\n",
        );
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gemini-3.1-pro-high");
        assert_eq!(models[1].name, "Claude Sonnet 4.6 (Thinking)");
    }

    #[test]
    fn workspace_skips_scratch_folders_and_decodes() {
        let cwd = workspace_cwd(
            r#"["file:///var/folders/yn/T/codexhost-agy","file:///Users/me/My%20App"]"#,
        );
        assert_eq!(cwd, Some(PathBuf::from("/Users/me/My App")));
        assert_eq!(workspace_cwd(r#"["file:///tmp/x"]"#), None);
    }

    #[test]
    fn summary_timestamps_parse() {
        assert_eq!(seconds("2026-09-21 03:41:03.678104+00:00"), 1_789_962_063);
    }

    #[test]
    fn catalog_lists_top_level_conversations_newest_first() {
        let root = std::env::temp_dir().join(format!("waku-agy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("conversation_summaries.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE conversation_summaries (conversation_id text, title text, preview text,
                   workspace_uris text, last_modified_time text, last_user_input_time text,
                   nesting_depth integer, parent_conversation_id text);
                 INSERT INTO conversation_summaries VALUES
                   ('old','Old','', '[\"file:///Users/me/a\"]','2026-09-01 00:00:00+00:00','2026-09-01 00:00:00+00:00',0,''),
                   ('new','New','', '[\"file:///Users/me/b\"]','2026-09-02 00:00:00+00:00','2026-09-02 00:00:00+00:00',0,''),
                   ('child','Child','', '[\"file:///Users/me/b\"]','2026-09-03 00:00:00+00:00','2026-09-03 00:00:00+00:00',1,'new');",
            )
            .unwrap();
        let sessions = list_provider_sessions_in(&path, 10).unwrap();
        let ids = sessions
            .iter()
            .map(|session| session.cursor.native_id())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["new", "old"]);
        assert_eq!(sessions[0].cwd, PathBuf::from("/Users/me/b"));
    }
}
