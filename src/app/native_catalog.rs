//! Provider-native conversations shown in the sidebar before they are imported.
//!
//! Every supported agent CLI keeps its own history on disk. The daemon keeps an
//! incremental index of those files (`waku_core::native_index`); this module
//! polls it in the background and merges the results into the sidebar, so
//! history started in a terminal or another app appears under its project next
//! to Waku's own tasks, ordered by recency. A row stays a lightweight "native"
//! entry until it is opened, which imports it through the same path the Resume
//! palette uses.
//!
//! The sidebar only reaches this module through a handful of hooks: merging
//! native ids into its already-built groups, retagging those rows, labelling a
//! group whose folder has no Waku project yet, and rendering a native row.

use std::hash::{DefaultHasher, Hash, Hasher};

use waku_protocol::native_session::NativeSessionSummary;

use super::native_projects::{INDEXED_PROVIDERS, NativePrefs, PROCESS_PROVIDERS, process_summary};
use super::sidebar::{
    SIDEBAR_GROUP_CHILD_PADDING, SIDEBAR_GROUP_GUIDE_X, SIDEBAR_SESSION_ROW_GAP, format_time_ago,
};
use super::sidebar_compact::{CompactRow, RowStatus};
use super::*;

const NATIVE_CATALOG_LIMIT: usize = 250;
/// Clock and write-order slack before a native file counts as newer than the
/// imported copy.
const NATIVE_SYNC_SLACK_SECONDS: u64 = 30;
/// A warm index refresh is a directory walk and a `stat` per file.
pub(super) const NATIVE_CATALOG_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
/// A request older than this is presumed lost; a new one may start and the
/// old one's late answer is ignored.
const REQUEST_STALE_AFTER: Duration = Duration::from_secs(120);

/// One background request slot: at most one in flight, with a generation so a
/// superseded request's answer is dropped.
#[derive(Default)]
pub(super) struct InFlight {
    generation: u64,
    since: Option<Instant>,
}

impl InFlight {
    /// Start a request unless a recent one is still pending.
    pub(super) fn begin(&mut self) -> Option<u64> {
        if self
            .since
            .is_some_and(|since| since.elapsed() < REQUEST_STALE_AFTER)
        {
            return None;
        }
        self.generation = self.generation.wrapping_add(1);
        self.since = Some(Instant::now());
        Some(self.generation)
    }

    /// Settle a request; false when a newer one superseded it.
    pub(super) fn finish(&mut self, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        self.since = None;
        true
    }
}
/// Listing a process-backed provider starts its agent or server, so it is
/// refreshed rarely.
const PROCESS_PROVIDER_REFRESH_INTERVAL: Duration = Duration::from_secs(600);

pub(super) struct NativeSessionEntry {
    pub(super) id: Uuid,
    pub(super) summary: ProviderSessionSummary,
    /// Main checkout when `summary.cwd` is a linked Git worktree.
    project_path: Option<PathBuf>,
    branch: Option<String>,
}

impl NativeSessionEntry {
    fn new(native: NativeSessionSummary) -> Self {
        Self {
            id: native_row_id(&native.summary.cursor),
            summary: native.summary,
            project_path: native.project_path,
            branch: native.branch,
        }
    }

    pub(super) fn timestamp(&self) -> u64 {
        if self.summary.updated_at == 0 {
            self.summary.created_at
        } else {
            self.summary.updated_at
        }
    }

    /// The folder this conversation is grouped under in the sidebar.
    pub(super) fn group_path(&self) -> &Path {
        self.project_path.as_deref().unwrap_or(&self.summary.cwd)
    }
}

#[derive(Default)]
pub(super) struct NativeCatalog {
    entries: Vec<NativeSessionEntry>,
    /// Bumped whenever the visible entry set changes; mixed into the sidebar
    /// row fingerprint.
    generation: u64,
    /// The native index listing in flight.
    pending: InFlight,
    /// Native row currently being imported.
    importing: Option<Uuid>,
    /// Imported tasks whose native history is being re-read.
    syncing: HashSet<Uuid>,
    /// Native timestamp each imported task was last re-read at, so a
    /// conversation whose history comes back empty is not fetched again until
    /// it changes.
    synced: HashMap<Uuid, u64>,
    /// Listings from process-backed providers, refreshed on their own cadence.
    process_entries: HashMap<ProviderKind, Vec<NativeSessionEntry>>,
    process_pending: HashSet<ProviderKind>,
    process_refreshed: HashMap<ProviderKind, Instant>,
    pub(super) prefs: NativePrefs,
    /// Project group whose header shows the inline rename field.
    pub(super) renaming_project: Option<Uuid>,
    /// Folder behind each native-only group id, rebuilt when entries change so
    /// header rendering never hashes the whole catalog.
    group_paths: HashMap<Uuid, PathBuf>,
    /// Pull requests by branch checkout, from the daemon's GitHub CLI lookup.
    pub(super) pull_requests: HashMap<
        waku_protocol::native_session::PullRequestTarget,
        waku_protocol::native_session::PullRequestInfo,
    >,
    pub(super) pull_requests_pending: InFlight,
}

impl NativeCatalog {
    pub(super) fn load() -> Self {
        Self {
            prefs: NativePrefs::load(),
            ..Self::default()
        }
    }

    /// Entries from every enabled provider.
    pub(super) fn entries(&self) -> impl Iterator<Item = &NativeSessionEntry> {
        self.entries
            .iter()
            .chain(self.process_entries.values().flatten())
            .filter(|entry| self.prefs.is_enabled(entry.summary.provider()))
    }

    /// Invalidate the sidebar rows after a preference or entry change.
    pub(super) fn touch(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.group_paths = self
            .entries
            .iter()
            .chain(self.process_entries.values().flatten())
            .map(|entry| {
                let path = entry.group_path();
                (native_project_id(path), path.to_path_buf())
            })
            .collect();
    }

    fn entry(&self, id: Uuid) -> Option<&NativeSessionEntry> {
        self.entries().find(|entry| entry.id == id)
    }

    fn replace(&mut self, sessions: Vec<NativeSessionSummary>) {
        let entries = sessions
            .into_iter()
            .map(NativeSessionEntry::new)
            .collect::<Vec<_>>();
        let unchanged = self.entries.len() == entries.len()
            && self.entries.iter().zip(&entries).all(|(a, b)| {
                a.id == b.id
                    && a.summary.updated_at == b.summary.updated_at
                    && a.summary.title == b.summary.title
                    && a.project_path == b.project_path
            });
        if !unchanged {
            self.entries = entries;
            self.touch();
        }
    }

    fn remove(&mut self, id: Uuid) {
        self.entries.retain(|entry| entry.id != id);
        for entries in self.process_entries.values_mut() {
            entries.retain(|entry| entry.id != id);
        }
        self.touch();
    }

    /// Order-independent digest of what the sidebar reads from the catalog,
    /// including which entries sit inside the project-recency window.
    pub(super) fn fingerprint(&self, recent_cutoff: u64) -> u64 {
        let recent = self
            .entries()
            .filter(|entry| entry.timestamp() >= recent_cutoff)
            .count() as u64;
        mix(
            mix(self.generation, recent),
            self.importing.map_or(0, |id| id.as_u128() as u64),
        )
    }
}

fn stable_hash(seed: u64, value: impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    seed.hash(&mut hasher);
    value.hash(&mut hasher);
    hasher.finish()
}

fn stable_id(value: impl Hash + Copy) -> Uuid {
    let high = stable_hash(0x6e61_7469_7665_0001, value);
    let low = stable_hash(0x6e61_7469_7665_0002, value);
    Uuid::from_u64_pair(high, low)
}

/// Runtime-only sidebar identity for a provider-native conversation.
fn native_row_id(cursor: &ProviderResumeCursor) -> Uuid {
    stable_id((cursor.provider().id(), cursor.native_id()))
}

/// Runtime-only group identity for a folder that has no Waku project yet.
pub(super) fn native_project_id(cwd: &Path) -> Uuid {
    stable_id(("project", cwd))
}

/// Insert `native` ids into `ids`, keeping the list ordered by timestamp in the
/// sidebar's direction. `ids` is already ordered; ties keep Waku rows first.
fn merge_by_timestamp(
    ids: &mut Vec<Uuid>,
    native: &[(Uuid, u64)],
    timestamps: &HashMap<Uuid, u64>,
    ordering: SidebarOrdering,
) {
    if native.is_empty() {
        return;
    }
    let before = |a: u64, b: u64| match ordering {
        SidebarOrdering::Newest => a > b,
        SidebarOrdering::Oldest => a < b,
    };
    let existing = std::mem::take(ids);
    let mut native = native.to_vec();
    native.sort_by(|a, b| match ordering {
        SidebarOrdering::Newest => b.1.cmp(&a.1),
        SidebarOrdering::Oldest => a.1.cmp(&b.1),
    });
    let mut native = native.into_iter().peekable();
    for id in existing {
        let timestamp = timestamps.get(&id).copied().unwrap_or_default();
        while let Some((native_id, _)) = native.next_if(|(_, t)| before(*t, timestamp)) {
            ids.push(native_id);
        }
        ids.push(id);
    }
    ids.extend(native.map(|(id, _)| id));
}

/// Merge native items into project groups, add groups for folders only native
/// history knows, and order groups: pinned first, then by their leading entry
/// in the sidebar's direction; the projectless group stays last.
fn arrange_project_groups(
    groups: &mut Vec<(SidebarGroup, Vec<Uuid>)>,
    items: Vec<(Uuid, SidebarGroup, u64)>,
    pinned: &HashSet<SidebarGroup>,
    timestamps: &mut HashMap<Uuid, u64>,
    ordering: SidebarOrdering,
) {
    let mut native: HashMap<SidebarGroup, Vec<(Uuid, u64)>> = HashMap::new();
    for (id, group, timestamp) in items {
        timestamps.insert(id, timestamp);
        native.entry(group).or_default().push((id, timestamp));
    }
    for (group, ids) in groups.iter_mut() {
        if let Some(native) = native.remove(group) {
            merge_by_timestamp(ids, &native, timestamps, ordering);
        }
    }
    for (group, native) in native {
        let mut ids = Vec::new();
        merge_by_timestamp(&mut ids, &native, timestamps, ordering);
        groups.push((group, ids));
    }
    let head = |ids: &Vec<Uuid>| {
        ids.first()
            .and_then(|id| timestamps.get(id))
            .copied()
            .unwrap_or_default()
    };
    groups.sort_by(|(a_group, a_ids), (b_group, b_ids)| {
        let a_last = *a_group == SidebarGroup::Projectless;
        let b_last = *b_group == SidebarGroup::Projectless;
        a_last
            .cmp(&b_last)
            .then_with(|| pinned.contains(b_group).cmp(&pinned.contains(a_group)))
            .then_with(|| match ordering {
                SidebarOrdering::Newest => head(b_ids).cmp(&head(a_ids)),
                SidebarOrdering::Oldest => head(a_ids).cmp(&head(b_ids)),
            })
    });
}

/// An imported task whose conversation continued in its own CLI since the
/// import: loaded, idle, never continued in Waku (it has no Waku transcript
/// blocks, only imported messages), older than the native conversation, and not
/// already re-read at this native timestamp.
fn import_is_stale(
    session: &AgentSession,
    native_timestamp: u64,
    in_use: bool,
    synced_at: Option<u64>,
) -> bool {
    session.detail_loaded
        && session.transcript_blocks.is_empty()
        && !session.is_busy()
        && !in_use
        && native_timestamp > session.updated_at.saturating_add(NATIVE_SYNC_SLACK_SECONDS)
        && synced_at.is_none_or(|synced| native_timestamp > synced)
}

impl Waku {
    /// Refresh the catalog from the daemon's native index in the background,
    /// plus any enabled process-backed provider whose listing is due.
    pub(super) fn refresh_native_catalog(&mut self, cx: &mut Context<Self>) {
        self.refresh_native_index(cx);
        self.refresh_pull_requests(cx);
        let due = PROCESS_PROVIDERS
            .into_iter()
            .filter(|provider| self.native_catalog.prefs.is_enabled(*provider))
            .filter(|provider| {
                self.native_catalog
                    .process_refreshed
                    .get(provider)
                    .is_none_or(|at| at.elapsed() >= PROCESS_PROVIDER_REFRESH_INTERVAL)
            })
            .collect::<Vec<_>>();
        for provider in due {
            self.refresh_process_provider(provider, cx);
        }
    }

    /// Refresh now, including process-backed providers that are not yet due.
    pub(super) fn refresh_native_catalog_now(&mut self, cx: &mut Context<Self>) {
        self.native_catalog.process_refreshed.clear();
        self.refresh_native_catalog(cx);
    }

    fn refresh_native_index(&mut self, cx: &mut Context<Self>) {
        let providers = INDEXED_PROVIDERS
            .into_iter()
            .filter(|provider| self.native_catalog.prefs.is_enabled(*provider))
            .collect::<Vec<_>>();
        if providers.is_empty() {
            return;
        }
        let Some(generation) = self.native_catalog.pending.begin() else {
            return;
        };
        let fetch = self.store.native_sessions(providers, NATIVE_CATALOG_LIMIT);
        cx.spawn(async move |waku, cx| {
            let result = cx.background_executor().spawn(async move { fetch() }).await;
            let _ = waku.update(cx, |waku, cx| {
                if !waku.native_catalog.pending.finish(generation) {
                    return;
                }
                // A failed listing keeps the last good one: the daemon may be
                // restarting.
                if let Ok(sessions) = result {
                    waku.native_catalog.replace(sessions);
                    waku.sync_imported_sessions(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn refresh_process_provider(&mut self, provider: ProviderKind, cx: &mut Context<Self>) {
        if !self.native_catalog.process_pending.insert(provider) {
            return;
        }
        self.native_catalog
            .process_refreshed
            .insert(provider, Instant::now());
        let fetch = self.store.provider_sessions(provider, NATIVE_CATALOG_LIMIT);
        cx.spawn(async move |waku, cx| {
            let result = cx.background_executor().spawn(async move { fetch() }).await;
            let _ = waku.update(cx, |waku, cx| {
                waku.native_catalog.process_pending.remove(&provider);
                if let Ok(summaries) = result {
                    let entries = summaries
                        .into_iter()
                        .map(|summary| NativeSessionEntry::new(process_summary(summary)))
                        .collect();
                    waku.native_catalog
                        .process_entries
                        .insert(provider, entries);
                    waku.native_catalog.touch();
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn stale_import(&self, session: &AgentSession, entry: &NativeSessionEntry) -> bool {
        import_is_stale(
            session,
            entry.timestamp(),
            self.runtimes.contains_key(&session.id)
                || self.native_catalog.syncing.contains(&session.id),
            self.native_catalog.synced.get(&session.id).copied(),
        )
    }

    /// Re-import native history for imported tasks that went stale.
    fn sync_imported_sessions(&mut self, cx: &mut Context<Self>) {
        let entries = self
            .native_catalog
            .entries()
            .map(|entry| {
                (
                    (entry.summary.provider(), entry.summary.cursor.native_id()),
                    entry,
                )
            })
            .collect::<HashMap<_, _>>();
        let stale = self
            .state
            .sessions
            .iter()
            .filter_map(|session| {
                let cursor = session.provider_cursor.as_ref()?;
                let entry = entries.get(&(cursor.provider(), cursor.native_id()))?;
                self.stale_import(session, entry)
                    .then(|| (session.id, entry.summary.clone()))
            })
            .collect::<Vec<_>>();
        for (session_id, summary) in stale {
            self.native_catalog.syncing.insert(session_id);
            let fetch = self
                .store
                .provider_session_history(summary.cursor.clone(), summary.cwd.clone());
            cx.spawn(async move |waku, cx| {
                let result = cx.background_executor().spawn(async move { fetch() }).await;
                let _ = waku.update(cx, |waku, cx| {
                    waku.native_catalog.syncing.remove(&session_id);
                    waku.native_catalog
                        .synced
                        .insert(session_id, summary.updated_at);
                    let Ok(history) = result else {
                        return;
                    };
                    waku.apply_native_history(session_id, &summary, history, cx);
                });
            })
            .detach();
        }
    }

    fn apply_native_history(
        &mut self,
        session_id: Uuid,
        summary: &ProviderSessionSummary,
        history: ProviderSessionHistory,
        cx: &mut Context<Self>,
    ) {
        let busy = self.runtimes.contains_key(&session_id);
        let Some(session) = self
            .state
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
        else {
            return;
        };
        // Re-check: the task may have been continued in Waku meanwhile.
        if busy || session.is_busy() || !session.transcript_blocks.is_empty() {
            return;
        }
        if history.messages.is_empty() && history.turns.is_empty() {
            return;
        }
        session.messages = history.messages;
        session.turns = history.turns;
        session.auto_title = Some(summary.title.clone());
        session.updated_at = session.updated_at.max(summary.updated_at);
        session.last_reply_at = Some(summary.updated_at);
        self.state.mark_session_dirty(session_id);
        self.save();
        if self.state.selected_session == Some(session_id) {
            self.reset_transcript_rows(self.transcript_row_count());
        }
        cx.notify();
    }

    /// Native conversations already imported as Waku tasks.
    pub(super) fn imported_native_ids(&self) -> HashSet<(ProviderKind, &str)> {
        self.state
            .sessions
            .iter()
            .filter_map(|session| session.provider_cursor.as_ref())
            .map(|cursor| (cursor.provider(), cursor.native_id()))
            .collect()
    }

    /// Native entries not yet imported, as `(row id, sidebar group, timestamp)`.
    fn native_sidebar_items(
        &self,
        projectless_project_ids: &HashSet<Uuid>,
    ) -> Vec<(Uuid, SidebarGroup, u64)> {
        let imported = self.imported_native_ids();
        let projects = self
            .state
            .projects
            .iter()
            .map(|project| (project.path.as_path(), project.id))
            .collect::<HashMap<_, _>>();
        self.native_catalog
            .entries()
            .filter(|entry| !imported.contains(&entry.native_key()))
            .map(|entry| {
                let cwd = entry.group_path();
                let group = match projects.get(cwd) {
                    Some(id) if projectless_project_ids.contains(id) => SidebarGroup::Projectless,
                    Some(id) => SidebarGroup::Project(*id),
                    None if cwd.as_os_str().is_empty() => SidebarGroup::Projectless,
                    None => SidebarGroup::Project(native_project_id(cwd)),
                };
                (entry.id, group, entry.timestamp())
            })
            .collect()
    }

    /// Merge native entries into the date-bucketed sidebar view.
    pub(super) fn merge_native_date_groups(
        &self,
        groups: &mut [Vec<Uuid>],
        bucket: impl Fn(u64) -> usize,
    ) {
        let timestamps = self
            .state
            .sessions
            .iter()
            .map(|session| {
                (
                    session.id,
                    super::sidebar::sidebar_session_timestamp(session),
                )
            })
            .collect::<HashMap<_, _>>();
        let timestamps = &timestamps;
        let mut native: Vec<Vec<(Uuid, u64)>> = vec![Vec::new(); groups.len()];
        for (id, _, timestamp) in self.native_sidebar_items(&HashSet::new()) {
            native[bucket(timestamp)].push((id, timestamp));
        }
        for (ids, native) in groups.iter_mut().zip(&native) {
            merge_by_timestamp(ids, native, timestamps, self.state.sidebar_ordering);
        }
    }

    /// Merge native entries into the project-grouped sidebar view, creating
    /// groups for folders Waku has no project for, and reorder groups by their
    /// most relevant entry. The projectless group stays last.
    pub(super) fn merge_native_project_groups(
        &self,
        groups: &mut Vec<(SidebarGroup, Vec<Uuid>)>,
        timestamps: &mut HashMap<Uuid, u64>,
        projectless_project_ids: &HashSet<Uuid>,
    ) {
        let items = self.native_sidebar_items(projectless_project_ids);
        let pinned = self.pinned_project_groups();
        if items.is_empty() && pinned.is_empty() {
            return;
        }
        arrange_project_groups(
            groups,
            items,
            &pinned,
            timestamps,
            self.state.sidebar_ordering,
        );
    }

    /// Turn session rows that point at native entries into native rows.
    pub(super) fn tag_native_sidebar_rows(&self, rows: &mut [SidebarRow]) {
        let native = self
            .native_catalog
            .entries()
            .map(|entry| entry.id)
            .collect::<HashSet<_>>();
        if native.is_empty() {
            return;
        }
        for row in rows {
            if let SidebarRow::Session(id) = *row
                && native.contains(&id)
            {
                *row = SidebarRow::Native(id);
            }
        }
    }

    /// Sidebar label for a group whose folder has no Waku project yet.
    pub(super) fn native_project_label(&self, project_id: Uuid) -> Option<String> {
        self.native_group_path(project_id)
            .map(|cwd| Project::from_path(cwd).name)
    }

    /// Worktree branches of native conversations, for pull request lookup.
    pub(super) fn native_pull_request_targets(
        &self,
    ) -> Vec<waku_protocol::native_session::PullRequestTarget> {
        self.native_catalog
            .entries()
            .filter_map(|entry| {
                Some(waku_protocol::native_session::PullRequestTarget {
                    cwd: entry.summary.cwd.clone(),
                    branch: entry.branch.clone()?,
                })
            })
            .collect()
    }

    /// The folder behind a native-only project group.
    pub(super) fn native_group_path(&self, project_id: Uuid) -> Option<PathBuf> {
        self.native_catalog.group_paths.get(&project_id).cloned()
    }

    /// Resolve a sidebar group id to a real Waku project, adopting the folder
    /// of a native-only group as a project first.
    pub(super) fn adopt_native_project(&mut self, project_id: Uuid) -> Uuid {
        if self
            .state
            .projects
            .iter()
            .any(|project| project.id == project_id)
        {
            return project_id;
        }
        let Some(cwd) = self
            .native_catalog
            .entries()
            .map(|entry| entry.group_path().to_path_buf())
            .find(|cwd| native_project_id(cwd) == project_id)
        else {
            return project_id;
        };
        let project = Project::from_path(cwd);
        let id = project.id;
        self.state.projects.push(project);
        self.analytics.track(crate::analytics::Event::ProjectAdded);
        id
    }

    /// Import a native conversation and open it as a Waku task.
    pub(super) fn open_native_session(
        &mut self,
        id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.native_catalog.importing.is_some() {
            return;
        }
        let Some((summary, worktree)) = self.native_catalog.entry(id).map(|entry| {
            let worktree =
                entry.project_path.clone().zip(entry.branch.clone()).map(
                    |(project_path, branch)| (project_path, entry.summary.cwd.clone(), branch),
                );
            (entry.summary.clone(), worktree)
        }) else {
            return;
        };
        self.native_catalog.importing = Some(id);
        let fetch = self
            .store
            .provider_session_history(summary.cursor.clone(), summary.cwd.clone());
        let window_handle = window.window_handle();
        cx.notify();
        cx.spawn(async move |waku, cx| {
            let result = cx.background_executor().spawn(async move { fetch() }).await;
            let _ = window_handle.update(cx, |_, window, cx| {
                let _ = waku.update(cx, |waku, cx| {
                    waku.native_catalog.importing = None;
                    match result {
                        Ok(history) => {
                            waku.native_catalog.remove(id);
                            waku.import_native_session(summary, worktree, history, window, cx);
                        }
                        Err(error) => {
                            waku.show_toast(tr!(
                                "command_palette.resume_failed",
                                error = error.to_string()
                            ));
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// Import through the Resume palette's path. A conversation that ran in a
    /// linked worktree becomes a worktree task of the main checkout's project,
    /// the same shape Waku gives its own worktree tasks, while the driver keeps
    /// resuming in the worktree directory it actually ran in.
    fn import_native_session(
        &mut self,
        mut summary: ProviderSessionSummary,
        worktree: Option<(PathBuf, PathBuf, String)>,
        history: ProviderSessionHistory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cursor = summary.cursor.clone();
        if let Some((project_path, _, _)) = &worktree {
            summary.cwd = project_path.clone();
        }
        self.import_provider_session(summary, history, window, cx);
        let Some((_, path, branch)) = worktree else {
            return;
        };
        if let Some(session) = self.state.sessions.iter_mut().find(|session| {
            session.provider_cursor.as_ref().is_some_and(|existing| {
                existing.provider() == cursor.provider()
                    && existing.native_id() == cursor.native_id()
            })
        }) && session.workspace.is_local()
        {
            session.workspace = SessionWorkspace::Worktree { path, branch };
            let session_id = session.id;
            self.state.mark_session_dirty(session_id);
            self.save();
        }
    }

    pub(super) fn render_native_session_item(
        &self,
        id: Uuid,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let Some(entry) = self.native_catalog.entry(id) else {
            return div().into_any_element();
        };
        let provider = entry.summary.provider();
        let importing = self.native_catalog.importing == Some(id);
        let grouped_by_project = self.state.sidebar_grouping == SidebarGrouping::Project;
        let left_padding = if grouped_by_project {
            SIDEBAR_GROUP_CHILD_PADDING
        } else {
            8.0
        };
        let time_label = format_time_ago(unix_time().saturating_sub(entry.timestamp()));
        let focus = self
            .native_row_focuses
            .borrow_mut()
            .entry(id)
            .or_insert_with(|| cx.focus_handle())
            .clone();
        let title = div()
            .flex_1()
            .min_w_0()
            .truncate()
            .text_size(sp(13.0))
            .text_color(theme.text_secondary)
            .child(SharedString::from(entry.summary.title.clone()))
            .into_any_element();
        let compact = self.render_compact_row(
            CompactRow {
                status: RowStatus::Native { importing },
                provider,
                title,
                branch: entry.branch.clone().map(SharedString::from),
                branch_cwd: Some(entry.summary.cwd.clone()),
                time: Some(time_label),
                time_emphasis: false,
            },
            cx,
        );

        let row = div()
            .id(SharedString::from(format!("native-session-{id}")))
            .track_focus(&focus)
            .tab_index(0)
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .pl(px(left_padding))
            .pr(px(8.0))
            .py(px(5.0))
            .rounded(px(7.0))
            .cursor_default()
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .hover(|element| element.bg(theme.sidebar_item_background))
            .active(|element| element.bg(theme.sidebar_item_background))
            .tooltip(Tooltip::text(format!(
                "{} · {}",
                provider.display_name(),
                entry.summary.cwd.display()
            )))
            .child(compact)
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.open_native_session(id, window, cx);
                    cx.stop_propagation();
                }
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_native_session(id, window, cx);
            }));

        div()
            .relative()
            .w_full()
            .pb(px(SIDEBAR_SESSION_ROW_GAP))
            .child(row)
            .when(grouped_by_project, |element| {
                element.child(
                    div()
                        .absolute()
                        .left(px(SIDEBAR_GROUP_GUIDE_X))
                        .top_0()
                        .bottom_0()
                        .w(px(1.0))
                        .bg(theme.border),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_rows_merge_into_recency_order() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let n1 = Uuid::from_u128(10);
        let n2 = Uuid::from_u128(11);
        let timestamps = HashMap::from([(a, 300), (b, 100)]);
        let mut ids = vec![a, b];
        merge_by_timestamp(
            &mut ids,
            &[(n1, 50), (n2, 200)],
            &timestamps,
            SidebarOrdering::Newest,
        );
        assert_eq!(ids, vec![a, n2, b, n1]);

        let mut ids = vec![b, a];
        merge_by_timestamp(
            &mut ids,
            &[(n2, 200), (n1, 50)],
            &timestamps,
            SidebarOrdering::Oldest,
        );
        assert_eq!(ids, vec![n1, b, n2, a]);
    }

    #[test]
    fn native_only_folders_join_groups_by_recency_and_pins_lead() {
        let waku_project = SidebarGroup::Project(Uuid::from_u128(1));
        let native_project = SidebarGroup::Project(Uuid::from_u128(2));
        let pinned_project = SidebarGroup::Project(Uuid::from_u128(3));
        let session = Uuid::from_u128(10);
        let mut timestamps = HashMap::from([(session, 200)]);
        let mut groups = vec![
            (SidebarGroup::Projectless, vec![]),
            (waku_project, vec![session]),
        ];
        let items = vec![
            (Uuid::from_u128(20), native_project, 300),
            (Uuid::from_u128(21), waku_project, 250),
            (Uuid::from_u128(22), pinned_project, 50),
            (Uuid::from_u128(23), SidebarGroup::Projectless, 999),
        ];
        arrange_project_groups(
            &mut groups,
            items,
            &HashSet::from([pinned_project]),
            &mut timestamps,
            SidebarOrdering::Newest,
        );
        let order = groups.iter().map(|(group, _)| *group).collect::<Vec<_>>();
        assert_eq!(
            order,
            vec![
                pinned_project,
                native_project,
                waku_project,
                SidebarGroup::Projectless
            ]
        );
        assert_eq!(groups[2].1, vec![Uuid::from_u128(21), session]);
        assert_eq!(timestamps[&Uuid::from_u128(20)], 300);
    }

    #[test]
    fn stale_imports_are_reread_once_per_native_change() {
        let mut session = AgentSession::new(Uuid::from_u128(1), ProviderKind::Antigravity);
        session.detail_loaded = true;
        session.updated_at = 1_000;
        assert!(import_is_stale(&session, 2_000, false, None));
        assert!(
            !import_is_stale(&session, 1_010, false, None),
            "within slack"
        );
        assert!(
            !import_is_stale(&session, 2_000, true, None),
            "running or syncing"
        );
        // An empty re-read records the timestamp, so it is not fetched again.
        assert!(!import_is_stale(&session, 2_000, false, Some(2_000)));
        assert!(import_is_stale(&session, 2_100, false, Some(2_000)));
    }

    #[test]
    fn native_ids_are_stable_per_provider_session() {
        let first = native_project_id(Path::new("/tmp/a"));
        assert_eq!(first, native_project_id(Path::new("/tmp/a")));
        assert_ne!(first, native_project_id(Path::new("/tmp/b")));
    }
}
