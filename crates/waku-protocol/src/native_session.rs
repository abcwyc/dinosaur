//! Provider-native conversations listed for the sidebar before import.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::model::ProviderSessionSummary;

/// One provider-native conversation from the daemon's incremental native
/// index. `summary.cwd` is where the conversation actually ran and is what a
/// resumed driver must use; `project_path` is set when that directory is a Git
/// worktree of another checkout, so the sidebar can group it with its
/// repository and an import can become a worktree task of that project.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct NativeSessionSummary {
    pub summary: ProviderSessionSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

impl NativeSessionSummary {
    /// The folder the sidebar groups this conversation under.
    pub fn group_path(&self) -> &std::path::Path {
        self.project_path.as_deref().unwrap_or(&self.summary.cwd)
    }
}

/// A branch checked out in `cwd` whose pull request the sidebar would like to
/// show.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestTarget {
    pub cwd: PathBuf,
    pub branch: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum PullRequestState {
    Open,
    Draft,
    Merged,
    Closed,
}

/// The most recent pull request whose head is `target.branch`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestInfo {
    pub target: PullRequestTarget,
    pub number: u64,
    pub state: PullRequestState,
    pub url: String,
}
