//! Incremental index of provider-native conversations for the sidebar.
//!
//! The Resume palette lists one provider at a time and re-reads its history on
//! every open. The sidebar instead shows every provider together and refreshes
//! often, so this index keeps one summary per native session file keyed by the
//! file's modification time and length: a refresh is a directory walk plus a
//! `stat` per file, and only files that changed are parsed again. The cache is
//! persisted beside the task database so a daemon restart does not re-read
//! hundreds of megabytes of transcripts.
//!
//! Every source here is a plain file read. Providers that answer only through a
//! running agent process (Amp, Cursor, OpenCode, DeepSeek, Fx) stay in the
//! Resume palette.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read as _};
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{ProviderKind, ProviderResumeCursor, ProviderSessionSummary};
use waku_protocol::native_session::NativeSessionSummary;

const CACHE_FILE: &str = "native-index.json";
const CACHE_VERSION: u32 = 1;
/// Codex puts its session metadata and first prompt at the front of a rollout.
const CODEX_HEAD_BYTES: u64 = 2 * 1024 * 1024;

/// Providers the index can list from files alone.
pub const INDEXED_PROVIDERS: [ProviderKind; 6] = [
    ProviderKind::Claude,
    ProviderKind::Codex,
    ProviderKind::Pi,
    ProviderKind::OhMyPi,
    ProviderKind::Grok,
    ProviderKind::Kimi,
];

#[derive(Clone, Deserialize, Serialize)]
struct CachedFile {
    provider: ProviderKind,
    modified: u64,
    len: u64,
    /// `None` records a file that is not a listable conversation, so it is not
    /// parsed again until it changes.
    summary: Option<NativeSessionSummary>,
}

#[derive(Default, Deserialize, Serialize)]
struct CacheFile {
    version: u32,
    files: Vec<(PathBuf, CachedFile)>,
}

pub struct NativeIndex {
    cache_path: Option<PathBuf>,
    files: Mutex<HashMap<PathBuf, CachedFile>>,
}

impl NativeIndex {
    pub fn new(state_directory: Option<&Path>) -> Self {
        let cache_path = state_directory.map(|directory| directory.join(CACHE_FILE));
        let files = cache_path
            .as_deref()
            .and_then(|path| fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<CacheFile>(&bytes).ok())
            .filter(|cache| cache.version == CACHE_VERSION)
            .map(|cache| cache.files.into_iter().collect())
            .unwrap_or_default();
        Self {
            cache_path,
            files: Mutex::new(files),
        }
    }

    /// Newest-first conversations for each requested provider, at most `limit`
    /// per provider.
    pub fn list(&self, providers: &[ProviderKind], limit: usize) -> Vec<NativeSessionSummary> {
        // One scan at a time: concurrent refreshes would parse the same
        // changed files twice and race on the cache file.
        let mut files = self.files.lock();
        let mut changed = false;
        let mut sessions = Vec::new();
        for &provider in providers {
            let mut listed = match provider {
                ProviderKind::Claude => scan(
                    &mut files,
                    &mut changed,
                    provider,
                    claude_files(),
                    claude_summary,
                ),
                ProviderKind::Codex => {
                    let titles = codex_thread_names();
                    let mut listed = scan(
                        &mut files,
                        &mut changed,
                        provider,
                        codex_files(),
                        codex_summary,
                    );
                    for session in &mut listed {
                        if let Some(title) = titles.get(session.summary.cursor.native_id()) {
                            session.summary.title = title.clone();
                        }
                    }
                    listed
                }
                ProviderKind::Pi | ProviderKind::OhMyPi => scan(
                    &mut files,
                    &mut changed,
                    provider,
                    pi_files(provider),
                    |path| crate::pi_session::summary_for_file(provider, path).map(native),
                ),
                ProviderKind::Grok => crate::grok_session::list_provider_sessions(limit)
                    .unwrap_or_default()
                    .into_iter()
                    .map(native)
                    .collect(),
                ProviderKind::Kimi => crate::kimi_session::list_provider_sessions(limit)
                    .unwrap_or_default()
                    .into_iter()
                    .map(native)
                    .collect(),
                _ => Vec::new(),
            };
            let mut seen = HashSet::new();
            listed.retain(|session| seen.insert(session.summary.cursor.native_id().to_owned()));
            listed.sort_by(|a, b| b.summary.updated_at.cmp(&a.summary.updated_at));
            listed.truncate(limit);
            sessions.extend(listed);
        }
        if changed {
            self.persist(&files);
        }
        sessions
    }

    fn persist(&self, files: &HashMap<PathBuf, CachedFile>) {
        let Some(path) = &self.cache_path else {
            return;
        };
        let cache = CacheFile {
            version: CACHE_VERSION,
            files: files
                .iter()
                .map(|(path, file)| (path.clone(), file.clone()))
                .collect(),
        };
        let Ok(bytes) = serde_json::to_vec(&cache) else {
            return;
        };
        let temporary = path.with_extension("json.tmp");
        if fs::write(&temporary, bytes).is_ok() {
            let _ = fs::rename(&temporary, path);
        }
    }
}

/// Refresh the cache for one provider's files and return their summaries.
fn scan(
    files: &mut HashMap<PathBuf, CachedFile>,
    changed: &mut bool,
    provider: ProviderKind,
    paths: Vec<PathBuf>,
    summarize: impl Fn(&Path) -> Option<NativeSessionSummary>,
) -> Vec<NativeSessionSummary> {
    let mut seen = HashSet::with_capacity(paths.len());
    let mut sessions = Vec::new();
    for path in paths {
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        let modified = modified_seconds(&metadata);
        let len = metadata.len();
        seen.insert(path.clone());
        let fresh = files
            .get(&path)
            .is_some_and(|file| file.modified == modified && file.len == len);
        if !fresh {
            let summary = summarize(&path);
            files.insert(
                path.clone(),
                CachedFile {
                    provider,
                    modified,
                    len,
                    summary,
                },
            );
            *changed = true;
        }
        if let Some(summary) = files.get(&path).and_then(|file| file.summary.clone()) {
            sessions.push(summary);
        }
    }
    let before = files.len();
    files.retain(|path, file| file.provider != provider || seen.contains(path));
    *changed |= files.len() != before;
    sessions
}

fn modified_seconds(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|time| time.as_secs())
        .unwrap_or_default()
}

fn native(summary: ProviderSessionSummary) -> NativeSessionSummary {
    let (project_path, branch) = worktree_origin(&summary.cwd).unzip();
    NativeSessionSummary {
        summary,
        project_path,
        branch: branch.flatten(),
    }
}

/// Folders macOS guards with a privacy prompt. Probing a historical cwd inside
/// one would ask the user for access just because the sidebar refreshed.
fn is_privacy_protected(path: &Path) -> bool {
    let Some(home) = dirs::home_dir() else {
        return true;
    };
    ["Desktop", "Documents", "Downloads", "Library"]
        .iter()
        .any(|folder| path.starts_with(home.join(folder)))
}

/// When `cwd` is a linked Git worktree, the main checkout it belongs to and its
/// checked-out branch.
fn worktree_origin(cwd: &Path) -> Option<(PathBuf, Option<String>)> {
    if !cwd.is_absolute() || is_privacy_protected(cwd) {
        return None;
    }
    let dot_git = cwd.join(".git");
    if !fs::symlink_metadata(&dot_git).ok()?.is_file() {
        return None;
    }
    let contents = fs::read_to_string(&dot_git).ok()?;
    let gitdir = contents
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)?;
    let gitdir = if Path::new(gitdir).is_absolute() {
        PathBuf::from(gitdir)
    } else {
        cwd.join(gitdir)
    };
    let worktrees = gitdir.parent()?;
    let main_git = worktrees.parent()?;
    if worktrees.file_name()? != "worktrees" || main_git.file_name()? != ".git" {
        return None;
    }
    let main = main_git.parent()?.to_path_buf();
    let branch = fs::read_to_string(gitdir.join("HEAD"))
        .ok()
        .and_then(|head| {
            head.trim()
                .strip_prefix("ref: refs/heads/")
                .map(str::to_owned)
        });
    Some((main, branch))
}

// ── Claude Code ─────────────────────────────────────────────────────────────

/// Every top-level conversation, including desktop and SDK sessions that the
/// terminal's `history.jsonl` does not index. Waku's own tasks are removed by
/// the client, which knows their native cursors.
fn claude_files() -> Vec<PathBuf> {
    crate::claude_session::projects_directory()
        .ok()
        .and_then(|directory| crate::claude_session::provider_session_files(&directory).ok())
        .map(|files| files.into_iter().map(|(_, path)| path).collect())
        .unwrap_or_default()
}

fn claude_summary(path: &Path) -> Option<NativeSessionSummary> {
    crate::claude_session::session_summary_from_path(path)
        .ok()
        .map(native)
}

// ── Codex ───────────────────────────────────────────────────────────────────

fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
}

/// Rollouts live under `sessions/YYYY/MM/DD/`.
fn codex_files() -> Vec<PathBuf> {
    let Some(root) = codex_home().map(|home| home.join("sessions")) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    let mut directories = vec![(root, 0)];
    while let Some((directory, depth)) = directories.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() && depth < 3 {
                directories.push((path, depth + 1));
            } else if file_type.is_file()
                && path.extension().and_then(|value| value.to_str()) == Some("jsonl")
                && path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|name| name.starts_with("rollout-"))
            {
                files.push(path);
            }
        }
    }
    files
}

/// User-visible thread names; the last record for a thread wins.
fn codex_thread_names() -> HashMap<String, String> {
    let Some(path) = codex_home().map(|home| home.join("session_index.jsonl")) else {
        return HashMap::new();
    };
    let Ok(file) = fs::File::open(path) else {
        return HashMap::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .filter_map(|value| {
            let id = value.get("id")?.as_str()?.to_owned();
            let name = value.get("thread_name")?.as_str()?.trim();
            (!name.is_empty()).then(|| (id, name.to_owned()))
        })
        .collect()
}

fn codex_title_from_prompt(prompt: &str) -> Option<String> {
    let mut title = prompt
        .split_whitespace()
        .take(7)
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        return None;
    }
    if title.chars().count() > 54 {
        title = format!("{}…", title.chars().take(53).collect::<String>());
    }
    Some(title)
}

/// The person's words in one rollout line: a `user_message` event, or, in
/// rollouts that predate those events, a user message item that is not one of
/// the context blocks Codex injects (AGENTS.md, environment, plugins, images).
fn codex_prompt(line: &str) -> Option<String> {
    // Cheap reject before parsing: most lines are model output.
    if !line.contains("\"user_message\"") && !line.contains("\"role\":\"user\"") {
        return None;
    }
    let value = serde_json::from_str::<Value>(line).ok()?;
    let payload = value.get("payload")?;
    let text = match (value.get("type")?.as_str()?, payload.get("type")?.as_str()?) {
        ("event_msg", "user_message") => payload.get("message")?.as_str()?.to_owned(),
        ("response_item", "message") if payload.get("role")?.as_str()? == "user" => payload
            .get("content")?
            .as_array()?
            .iter()
            .filter_map(|content| content.get("text")?.as_str())
            .map(str::trim)
            .find(|text| {
                !text.is_empty()
                    && !text.starts_with('<')
                    && !text.starts_with("# AGENTS.md instructions")
                    && !text.starts_with("# Files mentioned by the user")
            })?
            .to_owned(),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}

fn codex_summary(path: &Path) -> Option<NativeSessionSummary> {
    let file = fs::File::open(path).ok()?;
    let modified = file
        .metadata()
        .ok()
        .map(|metadata| modified_seconds(&metadata))
        .unwrap_or_default();
    let mut lines = BufReader::new(file.take(CODEX_HEAD_BYTES)).lines();
    let meta = serde_json::from_str::<Value>(&lines.next()?.ok()?).ok()?;
    if meta.get("type")?.as_str()? != "session_meta" {
        return None;
    }
    let payload = meta.get("payload")?;
    // Terminal and desktop threads only: `exec` runs and sub-agent threads
    // are not conversations a person resumes.
    match payload.get("source") {
        Some(Value::String(source)) if matches!(source.as_str(), "cli" | "vscode") => {}
        None => {}
        _ => return None,
    }
    let thread_id = payload
        .get("id")
        .or_else(|| payload.get("session_id"))?
        .as_str()?
        .to_owned();
    let cwd = PathBuf::from(payload.get("cwd")?.as_str()?);
    let created_at = payload
        .get("timestamp")
        .or_else(|| meta.get("timestamp"))
        .and_then(Value::as_str)
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .and_then(|value| u64::try_from(value.timestamp()).ok())
        .unwrap_or(modified);
    let prompt = lines
        .map_while(Result::ok)
        .find_map(|line| codex_prompt(&line))?;
    let title = codex_title_from_prompt(&prompt)?;
    Some(native(ProviderSessionSummary {
        cursor: ProviderResumeCursor::Codex { thread_id },
        title,
        cwd,
        created_at,
        updated_at: modified.max(created_at),
    }))
}

// ── Pi and Oh My Pi ─────────────────────────────────────────────────────────

fn pi_files(provider: ProviderKind) -> Vec<PathBuf> {
    crate::pi_session::session_roots(provider)
        .unwrap_or_default()
        .iter()
        .flat_map(|root| crate::pi_session::session_files(root))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("waku-native-index-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn codex_rollout_summary_uses_meta_and_first_prompt() {
        let root = temp_root();
        let path = root.join("rollout-a.jsonl");
        write(
            &path,
            concat!(
                r#"{"timestamp":"2026-02-03T01:47:54.215Z","type":"session_meta","payload":{"id":"t1","timestamp":"2026-02-03T01:47:54.204Z","cwd":"/tmp/project","source":"vscode"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<env>"}]}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"fix the login bug"}}"#,
                "\n"
            ),
        );
        let summary = codex_summary(&path).unwrap();
        assert_eq!(summary.summary.cursor.native_id(), "t1");
        assert_eq!(summary.summary.title, "fix the login bug");
        assert_eq!(summary.summary.cwd, PathBuf::from("/tmp/project"));
        assert_eq!(summary.summary.created_at, 1_770_083_274);
    }

    #[test]
    fn codex_exec_and_subagent_threads_are_skipped() {
        let root = temp_root();
        for (name, source) in [("exec", r#""exec""#), ("sub", r#"{"subagent":{}}"#)] {
            let path = root.join(format!("rollout-{name}.jsonl"));
            write(
                &path,
                &format!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{name}\",\"cwd\":\"/tmp\",\"source\":{source}}}}}\n\
                     {{\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"hi\"}}}}\n"
                ),
            );
            assert!(codex_summary(&path).is_none(), "{name}");
        }
    }

    #[test]
    fn linked_worktree_resolves_to_its_main_checkout() {
        let root = temp_root().canonicalize().unwrap();
        let main = root.join("repo");
        let gitdir = main.join(".git/worktrees/feature");
        write(&gitdir.join("HEAD"), "ref: refs/heads/feature/login\n");
        let worktree = root.join("wt");
        write(
            &worktree.join(".git"),
            &format!("gitdir: {}\n", gitdir.display()),
        );
        let (project, branch) = worktree_origin(&worktree).unwrap();
        assert_eq!(project, main);
        assert_eq!(branch.as_deref(), Some("feature/login"));
        assert!(worktree_origin(&main).is_none());
    }

    #[test]
    fn unchanged_files_are_not_parsed_again() {
        let root = temp_root();
        let path = root.join("a.jsonl");
        write(&path, "x");
        let calls = std::cell::Cell::new(0);
        let summarize = |_: &Path| {
            calls.set(calls.get() + 1);
            None
        };
        let mut files = HashMap::new();
        let mut changed = false;
        scan(
            &mut files,
            &mut changed,
            ProviderKind::Codex,
            vec![path.clone()],
            summarize,
        );
        scan(
            &mut files,
            &mut changed,
            ProviderKind::Codex,
            vec![path.clone()],
            summarize,
        );
        assert_eq!(calls.get(), 1);
        scan(
            &mut files,
            &mut changed,
            ProviderKind::Codex,
            Vec::new(),
            summarize,
        );
        assert!(files.is_empty());
    }
}
