//! Sidebar preferences for the native history catalog: which agents' history
//! is listed, pinned projects, and project display names.
//!
//! These live in their own small file beside the task database rather than in
//! Waku's persisted app state, so this feature adds no fields to upstream
//! structures. Projects are keyed by folder path, which is stable for both Waku
//! projects and folders that only native history knows about.

use serde::{Deserialize, Serialize};
use waku_protocol::native_session::NativeSessionSummary;

use super::native_catalog::NativeSessionEntry;
use super::sidebar::{CancelSessionRename, SESSION_RENAME_PARENT_CONTEXT};
use super::*;

const PREFS_FILE: &str = "native-catalog.json";

/// Providers listed from their own files by the daemon's native index.
pub(super) const INDEXED_PROVIDERS: [ProviderKind; 6] = [
    ProviderKind::Claude,
    ProviderKind::Codex,
    ProviderKind::Pi,
    ProviderKind::OhMyPi,
    ProviderKind::Grok,
    ProviderKind::Kimi,
];

/// Providers that can list history only through a running agent process or
/// server. Off by default; when enabled they refresh far less often.
pub(super) const PROCESS_PROVIDERS: [ProviderKind; 6] = [
    ProviderKind::Amp,
    ProviderKind::Cursor,
    ProviderKind::OpenCode,
    ProviderKind::OpenCode2,
    ProviderKind::DeepSeek,
    ProviderKind::Fx,
];

#[derive(Default, Deserialize, Serialize)]
pub(super) struct NativePrefs {
    /// Providers whose history is listed; `None` means the indexed defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    providers: Option<Vec<ProviderKind>>,
    #[serde(default)]
    pinned_projects: Vec<PathBuf>,
    #[serde(default)]
    project_names: HashMap<PathBuf, String>,
    /// Optional parts of each sidebar row.
    #[serde(default)]
    pub(super) show: super::sidebar_compact::RowDisplay,
}

impl NativePrefs {
    fn path() -> PathBuf {
        crate::persistence::StateStore::default_path().with_file_name(PREFS_FILE)
    }

    pub(super) fn load() -> Self {
        fs_read_json(&Self::path()).unwrap_or_default()
    }

    pub(super) fn save(&self) {
        if let Ok(bytes) = serde_json::to_vec_pretty(self) {
            let path = Self::path();
            let temporary = path.with_extension("json.tmp");
            if std::fs::write(&temporary, bytes).is_ok() {
                let _ = std::fs::rename(&temporary, path);
            }
        }
    }

    pub(super) fn is_enabled(&self, provider: ProviderKind) -> bool {
        match &self.providers {
            Some(providers) => providers.contains(&provider),
            None => INDEXED_PROVIDERS.contains(&provider),
        }
    }

    fn enabled_count(&self) -> usize {
        INDEXED_PROVIDERS
            .iter()
            .chain(&PROCESS_PROVIDERS)
            .filter(|provider| self.is_enabled(**provider))
            .count()
    }

    fn toggle(&mut self, provider: ProviderKind) {
        let mut providers = self
            .providers
            .clone()
            .unwrap_or_else(|| INDEXED_PROVIDERS.to_vec());
        if let Some(index) = providers.iter().position(|value| *value == provider) {
            providers.remove(index);
        } else {
            providers.push(provider);
        }
        self.providers = Some(providers);
    }
}

fn fs_read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// What the command palette needs to offer a native conversation.
pub(super) struct NativePaletteEntry {
    pub(super) id: Uuid,
    pub(super) title: String,
    pub(super) provider: ProviderKind,
    pub(super) project: String,
    pub(super) path: String,
    pub(super) timestamp: u64,
}

impl Waku {
    /// The folder a sidebar project group stands for.
    fn group_project_path(&self, project_id: Uuid) -> Option<PathBuf> {
        self.state
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.path.clone())
            .or_else(|| self.native_group_path(project_id))
    }

    /// A user-chosen sidebar name for a project group.
    pub(super) fn custom_project_label(&self, project_id: Uuid) -> Option<String> {
        let path = self.group_project_path(project_id)?;
        self.native_catalog.prefs.project_names.get(&path).cloned()
    }

    /// Group ids of pinned projects, for ordering the project view.
    pub(super) fn pinned_project_groups(&self) -> HashSet<SidebarGroup> {
        let pinned = &self.native_catalog.prefs.pinned_projects;
        if pinned.is_empty() {
            return HashSet::new();
        }
        self.state
            .projects
            .iter()
            .filter(|project| pinned.contains(&project.path))
            .map(|project| SidebarGroup::Project(project.id))
            .chain(
                pinned.iter().map(|path| {
                    SidebarGroup::Project(super::native_catalog::native_project_id(path))
                }),
            )
            .collect()
    }

    fn toggle_project_pin(&mut self, project_id: Uuid, cx: &mut Context<Self>) {
        let Some(path) = self.group_project_path(project_id) else {
            return;
        };
        let pinned = &mut self.native_catalog.prefs.pinned_projects;
        if let Some(index) = pinned.iter().position(|value| *value == path) {
            pinned.remove(index);
        } else {
            pinned.push(path);
        }
        self.native_catalog.prefs.save();
        self.native_catalog.touch();
        cx.notify();
    }

    fn toggle_native_provider(&mut self, provider: ProviderKind, cx: &mut Context<Self>) {
        self.native_catalog.prefs.toggle(provider);
        self.native_catalog.prefs.save();
        self.native_catalog.touch();
        if self.native_catalog.prefs.is_enabled(provider) {
            self.refresh_native_catalog_now(cx);
        }
        cx.notify();
    }

    pub(super) fn is_renaming_project_group(&self, group: SidebarGroup) -> bool {
        matches!(group, SidebarGroup::Project(id) if self.native_catalog.renaming_project == Some(id))
    }

    fn begin_project_rename(
        &mut self,
        project_id: Uuid,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session_rename = None;
        self.native_catalog.renaming_project = Some(project_id);
        self.session_rename_input.update(cx, |input, cx| {
            input.set_content(label, cx);
            input.select_all_text(cx);
        });
        let focus = self.session_rename_input.read(cx).focus();
        window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        cx.notify();
    }

    /// Commit an in-progress project rename. Returns whether one was active, so
    /// the shared rename field's submit handler can stop there.
    pub(super) fn commit_project_rename(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(project_id) = self.native_catalog.renaming_project.take() else {
            return false;
        };
        let name = self
            .session_rename_input
            .read(cx)
            .content()
            .trim()
            .to_owned();
        if let Some(path) = self.group_project_path(project_id) {
            let default_name = Project::from_path(path.clone()).name;
            let names = &mut self.native_catalog.prefs.project_names;
            if name.is_empty() || name == default_name {
                names.remove(&path);
            } else {
                names.insert(path, name);
            }
            self.native_catalog.prefs.save();
            self.native_catalog.touch();
        }
        cx.notify();
        true
    }

    fn cancel_project_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.native_catalog.renaming_project.take().is_none() {
            return;
        }
        let focus = self.composer_focus(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn reset_project_name(&mut self, project_id: Uuid, cx: &mut Context<Self>) {
        if let Some(path) = self.group_project_path(project_id)
            && self
                .native_catalog
                .prefs
                .project_names
                .remove(&path)
                .is_some()
        {
            self.native_catalog.prefs.save();
            self.native_catalog.touch();
            cx.notify();
        }
    }

    /// The group header's label, or the inline rename field while renaming.
    pub(super) fn render_project_header_label(
        &self,
        group: SidebarGroup,
        label: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.is_renaming_project_group(group) {
            return div().min_w_0().truncate().child(label).into_any_element();
        }
        let theme = Theme::current(cx);
        div()
            .id("project-rename-field")
            .key_context(SESSION_RENAME_PARENT_CONTEXT)
            .on_action(cx.listener(|this, _: &CancelSessionRename, window, cx| {
                this.cancel_project_rename(window, cx);
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.commit_project_rename(cx);
            }))
            .h(px(20.0))
            .flex_1()
            .min_w_0()
            .px(px(4.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(theme.accent)
            .bg(theme.inset)
            .flex()
            .items_center()
            .text_color(theme.text)
            .child(self.session_rename_input.clone())
            .into_any_element()
    }

    /// Wrap a project header in its context menu: pin and rename.
    pub(super) fn project_header_menu(
        &self,
        group: SidebarGroup,
        header: Stateful<Div>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let SidebarGroup::Project(project_id) = group else {
            return header.into_any_element();
        };
        let Some(path) = self.group_project_path(project_id) else {
            return header.into_any_element();
        };
        let pinned = self.native_catalog.prefs.pinned_projects.contains(&path);
        let renamed = self.native_catalog.prefs.project_names.contains_key(&path);
        let label = self
            .custom_project_label(project_id)
            .unwrap_or_else(|| Project::from_path(path.clone()).name);
        let menu = self.menu_handle(format!("project-header-{project_id}"), cx);
        let waku = cx.entity().downgrade();
        context_menu(
            div().w_full().child(header),
            SharedString::from(format!("project-header-menu-{project_id}")),
            &menu,
            move |_| {
                let pin_waku = waku.clone();
                let rename_waku = waku.clone();
                let reset_waku = waku.clone();
                let label = label.clone();
                let mut items = vec![
                    MenuItem::new(
                        if pinned {
                            tr!("sidebar.unpin_project")
                        } else {
                            tr!("sidebar.pin_project")
                        },
                        move |_, cx| {
                            let _ = pin_waku.update(cx, |waku, cx| {
                                waku.toggle_project_pin(project_id, cx);
                            });
                        },
                    ),
                    MenuItem::new(tr!("common.rename"), move |window, cx| {
                        let label = label.clone();
                        let _ = rename_waku.update(cx, |waku, cx| {
                            waku.begin_project_rename(project_id, label, window, cx);
                        });
                    }),
                ];
                if renamed {
                    items.push(MenuItem::new(
                        tr!("sidebar.reset_project_name"),
                        move |_, cx| {
                            let _ = reset_waku.update(cx, |waku, cx| {
                                waku.reset_project_name(project_id, cx);
                            });
                        },
                    ));
                }
                items
            },
        )
    }

    /// The sidebar options submenu choosing which agents' history is listed.
    pub(super) fn native_history_menu(
        &self,
        cx: &mut Context<Self>,
    ) -> impl Fn() -> MenuItem + 'static {
        let weak = cx.entity().downgrade();
        let enabled = INDEXED_PROVIDERS
            .iter()
            .chain(&PROCESS_PROVIDERS)
            .map(|provider| (*provider, self.native_catalog.prefs.is_enabled(*provider)))
            .collect::<Vec<_>>();
        let count = self.native_catalog.prefs.enabled_count();
        move || {
            let weak = weak.clone();
            let enabled = enabled.clone();
            MenuItem::submenu_with_value(
                tr!("sidebar.agent_history"),
                count.to_string(),
                move |_| {
                    let mut items =
                        vec![MenuItem::Header(tr!("sidebar.agent_history_files").into())];
                    for (index, (provider, on)) in enabled.iter().copied().enumerate() {
                        if index == INDEXED_PROVIDERS.len() {
                            items.push(MenuItem::Separator);
                            items.push(MenuItem::Header(
                                tr!("sidebar.agent_history_processes").into(),
                            ));
                        }
                        let weak = weak.clone();
                        items.push(
                            MenuItem::new(provider.display_name(), move |_, cx| {
                                let _ = weak.update(cx, |waku, cx| {
                                    waku.toggle_native_provider(provider, cx);
                                });
                            })
                            .selected(on),
                        );
                    }
                    items
                },
            )
        }
    }

    /// Native conversations not yet imported, for the command palette.
    pub(super) fn native_palette_entries(&self) -> Vec<NativePaletteEntry> {
        let imported = self.imported_native_ids();
        self.native_catalog
            .entries()
            .filter(|entry| !imported.contains(&entry.native_key()))
            .map(|entry| {
                let path = entry.group_path().to_path_buf();
                let project = self
                    .state
                    .projects
                    .iter()
                    .find(|project| project.path == path)
                    .and_then(|project| self.custom_project_label(project.id))
                    .or_else(|| self.native_catalog.prefs.project_names.get(&path).cloned())
                    .unwrap_or_else(|| Project::from_path(path.clone()).name);
                NativePaletteEntry {
                    id: entry.id,
                    title: entry.summary.title.clone(),
                    provider: entry.summary.provider(),
                    project,
                    path: entry.summary.cwd.to_string_lossy().into_owned(),
                    timestamp: entry.timestamp(),
                }
            })
            .collect()
    }
}

/// Wrap a process-listed provider summary the way the native index would.
pub(super) fn process_summary(summary: ProviderSessionSummary) -> NativeSessionSummary {
    NativeSessionSummary {
        summary,
        project_path: None,
        branch: None,
    }
}

impl NativeSessionEntry {
    pub(super) fn native_key(&self) -> (ProviderKind, &str) {
        (self.summary.provider(), self.summary.cursor.native_id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_agents_are_listed_until_the_user_chooses() {
        let mut prefs = NativePrefs::default();
        assert!(prefs.is_enabled(ProviderKind::Claude));
        assert!(!prefs.is_enabled(ProviderKind::Amp));
        prefs.toggle(ProviderKind::Amp);
        prefs.toggle(ProviderKind::Grok);
        assert!(prefs.is_enabled(ProviderKind::Amp));
        assert!(!prefs.is_enabled(ProviderKind::Grok));
        assert!(prefs.is_enabled(ProviderKind::Codex));
        assert_eq!(prefs.enabled_count(), INDEXED_PROVIDERS.len());
    }

    #[test]
    fn prefs_round_trip_with_paths_as_keys() {
        let mut prefs = NativePrefs::default();
        prefs.pinned_projects.push(PathBuf::from("/tmp/a"));
        prefs
            .project_names
            .insert(PathBuf::from("/tmp/a"), "Alpha".into());
        let json = serde_json::to_string(&prefs).unwrap();
        let back: NativePrefs = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pinned_projects, prefs.pinned_projects);
        assert_eq!(back.project_names, prefs.project_names);
        assert!(back.providers.is_none());
    }
}
