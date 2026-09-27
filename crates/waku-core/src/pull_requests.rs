//! Pull request lookup for sidebar rows, through the GitHub CLI.
//!
//! Each branch costs one `gh pr list` network round trip, so results are
//! cached for a few minutes and lookups run a few at a time on the request's
//! own daemon thread.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::Value;
use waku_protocol::native_session::{PullRequestInfo, PullRequestState, PullRequestTarget};

const CACHE_TTL: Duration = Duration::from_secs(300);
const CONCURRENCY: usize = 4;
/// Default branches: their "pull request" would be an unrelated fork's.
const SKIPPED_BRANCHES: [&str; 4] = ["main", "master", "HEAD", "trunk"];

#[derive(Default)]
pub struct PullRequestCache {
    entries: Mutex<HashMap<PullRequestTarget, (Instant, Option<PullRequestInfo>)>>,
}

impl PullRequestCache {
    pub fn lookup(&self, targets: Vec<PullRequestTarget>) -> Vec<PullRequestInfo> {
        let Some(gh) = crate::command_env::find_executable("gh") else {
            return Vec::new();
        };
        let mut found = Vec::new();
        let mut missing = Vec::new();
        {
            let entries = self.entries.lock();
            for target in targets {
                if SKIPPED_BRANCHES.contains(&target.branch.as_str())
                    || crate::native_index::is_privacy_protected(&target.cwd)
                {
                    continue;
                }
                match entries.get(&target) {
                    Some((at, info)) if at.elapsed() < CACHE_TTL => found.extend(info.clone()),
                    _ => missing.push(target),
                }
            }
        }
        for chunk in missing.chunks(CONCURRENCY) {
            let results = std::thread::scope(|scope| {
                chunk
                    .iter()
                    .map(|target| scope.spawn(|| (target.clone(), query(&gh, target))))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .filter_map(|handle| handle.join().ok())
                    .collect::<Vec<_>>()
            });
            let mut entries = self.entries.lock();
            for (target, result) in results {
                // A failed query (offline, no GitHub remote) is cached as
                // "none" too, so it is not retried on every refresh.
                let info = result.ok().flatten();
                found.extend(info.clone());
                entries.insert(target, (Instant::now(), info));
            }
        }
        found
    }
}

fn query(gh: &Path, target: &PullRequestTarget) -> anyhow::Result<Option<PullRequestInfo>> {
    let mut command = crate::command_env::command(gh);
    command
        .args([
            "pr",
            "list",
            "--head",
            &target.branch,
            "--state",
            "all",
            "--limit",
            "1",
            "--json",
            "number,state,isDraft,url",
        ])
        .current_dir(&target.cwd)
        .stdin(std::process::Stdio::null());
    let output = crate::command_env::output(&mut command)?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = serde_json::from_slice::<Value>(&output.stdout)?;
    Ok(parse(&value, target))
}

fn parse(value: &Value, target: &PullRequestTarget) -> Option<PullRequestInfo> {
    let pull_request = value.as_array()?.first()?;
    let state = match pull_request.get("state")?.as_str()? {
        "OPEN" if pull_request.get("isDraft").and_then(Value::as_bool) == Some(true) => {
            PullRequestState::Draft
        }
        "OPEN" => PullRequestState::Open,
        "MERGED" => PullRequestState::Merged,
        _ => PullRequestState::Closed,
    };
    Some(PullRequestInfo {
        target: target.clone(),
        number: pull_request.get("number")?.as_u64()?,
        state,
        url: pull_request.get("url")?.as_str()?.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gh_pr_list_output_maps_to_states() {
        let target = PullRequestTarget {
            cwd: "/tmp/repo".into(),
            branch: "feature".into(),
        };
        let parsed = |json: &str| parse(&serde_json::from_str(json).unwrap(), &target);
        let open = parsed(r#"[{"number":7,"state":"OPEN","isDraft":false,"url":"u"}]"#).unwrap();
        assert_eq!((open.number, open.state), (7, PullRequestState::Open));
        let draft = parsed(r#"[{"number":8,"state":"OPEN","isDraft":true,"url":"u"}]"#).unwrap();
        assert_eq!(draft.state, PullRequestState::Draft);
        let merged =
            parsed(r#"[{"number":9,"state":"MERGED","isDraft":false,"url":"u"}]"#).unwrap();
        assert_eq!(merged.state, PullRequestState::Merged);
        assert!(parsed("[]").is_none());
    }
}
