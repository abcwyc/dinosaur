# Fork notes: native agent history in the sidebar

This fork lists every coding agent's own history in the sidebar, grouped by
project and ordered by recency, with single-line rows. The feature lives in new
files; upstream files only carry thin hooks. This page lists every hook so an
upstream merge can be checked point by point.

## New files (no merge conflicts expected)

| File | Role |
| --- | --- |
| `crates/waku-protocol/src/native_session.rs` | Wire types: `NativeSessionSummary`, `PullRequestTarget`/`Info`/`State` |
| `crates/waku-core/src/native_index.rs` | Daemon: incremental index of provider history files (mtime/size cache in `native-index.json`), Codex rollout parser, Git worktree resolution |
| `crates/waku-core/src/pull_requests.rs` | Daemon: `gh pr list --head <branch>` lookup, 5-minute cache |
| `src/app/native_catalog.rs` | Client: polls the index, merges native rows into sidebar groups, imports on click, re-syncs stale imports |
| `src/app/native_projects.rs` | Client: preferences file `native-catalog.json` (enabled agents, pins, project names, row display), header context menu, "Agent history" menu, palette entries |
| `src/app/sidebar_compact.rs` | Client: single-line row layout, "Show" menu, pull request refresh, `render_compact_session_item` |
| `assets/icons/git-pull-request.svg` | Icon |

## Hooks in upstream files

### `src/app/sidebar.rs` (most likely to conflict)

| Hook | Purpose |
| --- | --- |
| `pub(super)` on `SESSION_RENAME_PARENT_CONTEXT`, `SIDEBAR_SESSION_ROW_GAP`, `SIDEBAR_GROUP_GUIDE_X`, `SIDEBAR_GROUP_CHILD_PADDING`, `sidebar_session_timestamp`, `persisted_sidebar_branch_label`, `localized_session_title`, `sidebar_session_selected`, `begin_session_rename`, `cancel_session_rename` | Visibility only |
| `SIDEBAR_SESSION_CARD_HEIGHT = sidebar_compact::COMPACT_ROW_HEIGHT` | Single-line row height |
| `SidebarRow::Native(Uuid)` + its arms in `sidebar_row_height`, `sidebar_row`, the header's `has_expanded_children` | Native row type |
| `render_sidebar_header_actions`: `native_history_menu` and `row_display_menu` appended to the options menu | "Agent history" and "Show" menus |
| `ensure_sidebar_branch_labels`: guard is "branch or PR shown" instead of "project grouping"; `refresh_pull_requests` after labels land | Branch labels feed rows in both groupings |
| `sidebar_rows_cached`: mixes `native_catalog.fingerprint(..)` | Rebuild rows when the catalog changes |
| `sidebar_rows`: `merge_native_date_groups`, `merge_native_project_groups`, `tag_native_sidebar_rows` | Merge native rows, pins, ordering |
| `render_sidebar_group_header`: `custom_project_label` first, `native_project_label` fallback; `render_project_header_label`; click/key skip while renaming; `project_header_menu` wraps the header | Project rename/pin |
| `open_new_task_for_sidebar_group`: `adopt_native_project` | New task in a folder only native history knows |
| `commit_session_rename`: `commit_project_rename` first | Shared rename field |
| `render_sidebar_session_item`: 3-line hook to `render_compact_session_item` | **Upstream's body below it is kept verbatim but no longer runs.** If upstream changes task-row behavior (menu items, status, keyboard), port it to `sidebar_compact.rs::render_compact_session_item`. |
| Test `selected_session_uses_nearest_bottom_edge_for_an_unmeasured_lower_row` | Expected offsets for the 31 px row |

### Other client files

| File | Hook |
| --- | --- |
| `src/app.rs` | `mod native_catalog; mod native_projects; mod sidebar_compact;`, fields `native_catalog`, `native_row_focuses`, and a 30 s refresh loop beside the idle-session sweep |
| `src/app/command_palette.rs` | `PaletteAction::OpenNativeSession`, native entries chained into `command_palette_task_candidates`, `pub(super) fn import_provider_session` |
| `src/assets.rs` | `"git-pull-request"` icon |
| `crates/waku-client/src/persistence.rs` | Default `SidebarGrouping::Project` (enum default and `PersistedState::empty`, plus its test); `native_sessions` and `pull_requests` store requests |
| `locales/*.yml` | `sidebar.agent_history*`, `sidebar.pin/unpin/reset_project_name`, `sidebar.show*`, `sidebar.pr_*`; zh-CN `sidebar.*_ago` shortened to fit one line |

### Daemon and protocol

| File | Hook |
| --- | --- |
| `crates/waku-protocol/src/lib.rs` | `pub mod native_session;` |
| `crates/waku-protocol/src/protocol.rs` | `Command::ListNativeSessions`, `Command::LookupPullRequests`, `ResponsePayload::NativeSessions`, `ResponsePayload::PullRequests` |
| `crates/waku-core/src/lib.rs` | `pub mod native_index; pub mod pull_requests;` |
| `crates/waku-core/src/daemon.rs` | Fields `native_index`, `pull_requests`; two command arms; both commands listed in `handle_driver_command`'s wrong-path arm |
| `crates/waku-core/src/claude_session.rs` | `pub(crate)` on `session_summary_from_path`, `projects_directory`, `provider_session_files` |
| `crates/waku-core/src/pi_session.rs` | `pub(crate)` on `session_roots`, `session_files`; new `summary_for_file` |

## Upstream APIs this feature depends on

A change to any of these breaks the build, which is the easy case:
`import_provider_session`, `provider_session_history` / `LoadProviderSession`,
`ListProviderSessions` (process-backed agents), `grok_session` and
`kimi_session::list_provider_sessions`, `ProviderResumeCursor::native_id`,
`AgentSession` (`provider_cursor`, `transcript_blocks`, `workspace`,
`detail_loaded`), `SessionWorkspace::Worktree`, `sidebar_branch_labels`,
`session_rename_input`, `MenuItem`, `context_menu`, `ui::provider_icon`,
`command_env::{find_executable, command, output}`.

## Formats read directly (silent breakage)

These are parsed without going through a provider API, so a format change shows
up as missing rows, not a build error. `native_index` tests cover the shapes.

- Codex: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (first line
  `session_meta` with `id`, `cwd`, `source`; first `user_message` event or user
  message item) and `~/.codex/session_index.jsonl` (`thread_name`).
- Git worktrees: `<cwd>/.git` file → `gitdir: <main>/.git/worktrees/<name>`.
- `gh pr list --json number,state,isDraft,url`.

## After merging upstream

```sh
bun run protocol:generate
cargo test -p waku -p waku-core -p waku-protocol -p waku-client
```

Then open the app and check that Claude and Codex history still appears, a
native row imports on click, and the Show menu toggles branch, PR, and harness.
If the Metal toolchain is missing locally, add
`--features gpui_platform/runtime_shaders` to `cargo build`/`cargo test` for
the `waku` crate.
