//! Single-line sidebar rows.
//!
//! One line carries a status mark, the agent (harness) badge,
//! the title, the branch, the branch's pull request, and recency. Which of the
//! optional parts show is a sidebar preference ("Show" in the options menu).
//! Dinosaur tasks and not-yet-imported native conversations share this layout.
//!
//! Pull requests come from the daemon's GitHub CLI lookup, refreshed in the
//! background with the native catalog; rendering only reads the stored map.

use serde::{Deserialize, Serialize};
use waku_protocol::native_session::{PullRequestInfo, PullRequestState, PullRequestTarget};

use super::sidebar::{
    CancelSessionRename, SESSION_RENAME_PARENT_CONTEXT, SIDEBAR_GROUP_CHILD_PADDING,
    SIDEBAR_GROUP_GUIDE_X, SIDEBAR_SESSION_ROW_GAP, localized_session_title,
    persisted_sidebar_branch_label, session_time_label, sidebar_session_selected,
};
use super::*;

/// Optional parts of a sidebar row.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub(super) struct RowDisplay {
    pub(super) branch: bool,
    pub(super) pull_request: bool,
    pub(super) harness: bool,
}

impl Default for RowDisplay {
    fn default() -> Self {
        Self {
            branch: true,
            // Off until asked for: it calls GitHub for every shown branch.
            pull_request: false,
            harness: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RowPart {
    Branch,
    PullRequest,
    Harness,
}

impl RowPart {
    const ALL: [Self; 3] = [Self::Branch, Self::PullRequest, Self::Harness];

    fn label(self) -> String {
        match self {
            Self::Branch => tr!("sidebar.show_branch"),
            Self::PullRequest => tr!("sidebar.show_pull_request"),
            Self::Harness => tr!("sidebar.show_harness"),
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Branch => "icons/git-branch.svg",
            Self::PullRequest => "icons/git-pull-request.svg",
            Self::Harness => "icons/bot.svg",
        }
    }
}

impl RowDisplay {
    fn get(self, part: RowPart) -> bool {
        match part {
            RowPart::Branch => self.branch,
            RowPart::PullRequest => self.pull_request,
            RowPart::Harness => self.harness,
        }
    }

    fn toggle(&mut self, part: RowPart) {
        let value = match part {
            RowPart::Branch => &mut self.branch,
            RowPart::PullRequest => &mut self.pull_request,
            RowPart::Harness => &mut self.harness,
        };
        *value = !*value;
    }
}

/// The leading mark: activity for Dinosaur tasks, a hollow ring for native
/// conversations that have not been imported yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RowStatus {
    Session(SessionStatus),
    Native { importing: bool },
}

pub(super) struct CompactRow {
    pub(super) status: RowStatus,
    pub(super) provider: ProviderKind,
    pub(super) title: AnyElement,
    pub(super) branch: Option<SharedString>,
    /// Where the branch is checked out, for its pull request.
    pub(super) branch_cwd: Option<PathBuf>,
    pub(super) time: Option<String>,
    pub(super) time_emphasis: bool,
}

pub(super) const COMPACT_ROW_HEIGHT: f32 = 30.0;
const BRANCH_MAX_WIDTH: f32 = 96.0;

fn pull_request_color(state: PullRequestState, theme: &Theme) -> Hsla {
    match state {
        PullRequestState::Open => theme.success,
        PullRequestState::Draft => theme.text_tertiary,
        PullRequestState::Merged => theme.accent,
        PullRequestState::Closed => theme.danger,
    }
}

fn pull_request_state_label(state: PullRequestState) -> String {
    match state {
        PullRequestState::Open => tr!("sidebar.pr_open"),
        PullRequestState::Draft => tr!("sidebar.pr_draft"),
        PullRequestState::Merged => tr!("sidebar.pr_merged"),
        PullRequestState::Closed => tr!("sidebar.pr_closed"),
    }
}

impl Waku {
    pub(super) fn row_display(&self) -> RowDisplay {
        self.native_catalog.prefs.show
    }

    fn toggle_row_part(&mut self, part: RowPart, cx: &mut Context<Self>) {
        self.native_catalog.prefs.show.toggle(part);
        self.native_catalog.prefs.save();
        if part == RowPart::Branch {
            // Branch labels for local tasks are only scanned while shown.
            self.sidebar_branch_scan_fingerprint.set(None);
        }
        if part == RowPart::PullRequest && self.native_catalog.prefs.show.pull_request {
            self.refresh_pull_requests(cx);
        }
        cx.notify();
    }

    /// The "Show" section of the sidebar options menu.
    pub(super) fn row_display_menu(
        &self,
        cx: &mut Context<Self>,
    ) -> impl Fn() -> Vec<MenuItem> + 'static {
        let weak = cx.entity().downgrade();
        let display = self.row_display();
        move || {
            let mut items = vec![
                MenuItem::Separator,
                MenuItem::Header(tr!("sidebar.show").into()),
            ];
            for part in RowPart::ALL {
                let weak = weak.clone();
                items.push(
                    MenuItem::new(part.label(), move |_, cx| {
                        let _ = weak.update(cx, |waku, cx| waku.toggle_row_part(part, cx));
                    })
                    .icon(part.icon())
                    .selected(display.get(part)),
                );
            }
            items
        }
    }

    /// Every branch a row could show, with where it is checked out.
    fn pull_request_targets(&self) -> Vec<PullRequestTarget> {
        let labels = self.sidebar_branch_labels.borrow();
        let mut targets = HashSet::new();
        for session in self.state.sessions.iter().filter(|s| s.has_started()) {
            let target = match &session.workspace {
                SessionWorkspace::Worktree { path, branch } => Some((path.clone(), branch.clone())),
                SessionWorkspace::Local => self
                    .state
                    .projects
                    .iter()
                    .find(|project| project.id == session.project_id)
                    .and_then(|project| {
                        labels
                            .get(&project.path)
                            .map(|branch| (project.path.clone(), branch.to_string()))
                    }),
                SessionWorkspace::NewWorktree { .. } => None,
            };
            if let Some((cwd, branch)) = target {
                targets.insert(PullRequestTarget { cwd, branch });
            }
        }
        targets.extend(self.native_pull_request_targets());
        targets.into_iter().collect()
    }

    /// Look up pull requests for every shown branch in the background.
    pub(super) fn refresh_pull_requests(&mut self, cx: &mut Context<Self>) {
        if !self.row_display().pull_request {
            return;
        }
        let targets = self.pull_request_targets();
        if targets.is_empty() {
            return;
        }
        let Some(generation) = self.native_catalog.pull_requests_pending.begin() else {
            return;
        };
        let fetch = self.store.pull_requests(targets);
        cx.spawn(async move |waku, cx| {
            let result = cx.background_executor().spawn(async move { fetch() }).await;
            let _ = waku.update(cx, |waku, cx| {
                if !waku.native_catalog.pull_requests_pending.finish(generation) {
                    return;
                }
                if let Ok(pull_requests) = result {
                    waku.native_catalog.pull_requests = pull_requests
                        .into_iter()
                        .map(|info| (info.target.clone(), info))
                        .collect();
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// A stable focus handle per pull request, so keyboard focus survives the
    /// virtualized list re-rendering the row.
    fn pull_request_focus(&self, url: &str, cx: &mut Context<Self>) -> FocusHandle {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        ("pull-request", url).hash(&mut hasher);
        let key = Uuid::from_u64_pair(hasher.finish(), 0x7072);
        self.native_row_focuses
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| cx.focus_handle())
            .clone()
    }

    fn pull_request_for(&self, cwd: &Path, branch: &str) -> Option<&PullRequestInfo> {
        self.native_catalog.pull_requests.get(&PullRequestTarget {
            cwd: cwd.to_path_buf(),
            branch: branch.to_owned(),
        })
    }

    /// The horizontal content of one sidebar row. Callers own the interactive
    /// container (focus, selection, click, context menu).
    pub(super) fn render_compact_row(&self, row: CompactRow, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let display = self.row_display();

        let status = match row.status {
            RowStatus::Session(SessionStatus::Connecting | SessionStatus::Working)
            | RowStatus::Native { importing: true } => {
                motion::spin_slow(icon("icons/loader-circle.svg", 11.0, theme.accent))
                    .into_any_element()
            }
            RowStatus::Session(status @ SessionStatus::Waiting) => {
                icon("icons/alert.svg", 11.0, status_color(&theme, status)).into_any_element()
            }
            RowStatus::Session(status @ SessionStatus::Failed) => {
                icon("icons/x.svg", 11.0, status_color(&theme, status)).into_any_element()
            }
            RowStatus::Session(status @ SessionStatus::Background) => {
                icon("icons/hourglass.svg", 11.0, status_color(&theme, status)).into_any_element()
            }
            RowStatus::Session(SessionStatus::Idle) => div()
                .size(px(6.0))
                .rounded_full()
                .bg(theme.text_ghost)
                .into_any_element(),
            RowStatus::Native { importing: false } => div()
                .size(px(6.0))
                .rounded_full()
                .border_1()
                .border_color(theme.text_ghost)
                .into_any_element(),
        };

        let harness = display.harness.then(|| {
            div()
                .size(px(16.0))
                .flex_none()
                .rounded(px(4.0))
                .bg(theme.overlay_strong)
                .flex()
                .items_center()
                .justify_center()
                .child(icon(
                    crate::ui::provider_icon(row.provider),
                    11.0,
                    theme.text_secondary,
                ))
        });

        let branch = display
            .branch
            .then_some(row.branch.clone())
            .flatten()
            .map(|branch| {
                div()
                    .flex_none()
                    .max_w(px(BRANCH_MAX_WIDTH))
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(3.0))
                    .child(icon("icons/git-branch.svg", 11.0, theme.text_tertiary))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(sp(11.5))
                            .text_color(theme.text_tertiary)
                            .child(branch),
                    )
            });

        let pull_request = display
            .pull_request
            .then(|| {
                let branch = row.branch.as_ref()?;
                let cwd = row.branch_cwd.as_ref()?;
                self.pull_request_for(cwd, branch).cloned()
            })
            .flatten()
            .map(|info| {
                let color = pull_request_color(info.state, &theme);
                let url = info.url.clone();
                let key_url = info.url.clone();
                let focus = self.pull_request_focus(&info.url, cx);
                div()
                    .id(SharedString::from(format!(
                        "pr-{}-{}",
                        info.number, info.target.branch
                    )))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .px(px(3.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .track_focus(&focus)
                    .tab_index(0)
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .hover(|style| style.bg(theme.overlay))
                    .on_key_down(move |event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            cx.stop_propagation();
                            cx.open_url(&key_url);
                        }
                    })
                    .tooltip(Tooltip::text(format!(
                        "#{} · {}",
                        info.number,
                        pull_request_state_label(info.state)
                    )))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        cx.open_url(&url);
                    })
                    .child(icon("icons/git-pull-request.svg", 11.0, color))
                    .child(
                        div()
                            .text_size(sp(11.5))
                            .text_color(color)
                            .child(SharedString::from(format!("#{}", info.number))),
                    )
            });

        div()
            .w_full()
            .min_w_0()
            .h(px(18.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .w(px(11.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(status),
            )
            .when_some(harness, |element, badge| element.child(badge))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .child(row.title),
            )
            .when_some(branch, |element, branch| element.child(branch))
            .when_some(pull_request, |element, pull_request| {
                element.child(pull_request)
            })
            .when_some(row.time, |element, time| {
                element.child(
                    div()
                        .flex_none()
                        .text_size(sp(12.0))
                        .text_color(if row.time_emphasis {
                            theme.text_tertiary
                        } else {
                            theme.text_ghost
                        })
                        .child(SharedString::from(time)),
                )
            })
    }
}

impl Waku {
    /// The single-line form of a Dinosaur task row. Hooked in at the top of
    /// `sidebar::render_sidebar_session_item`, whose body is upstream's
    /// two-line row and is kept verbatim so upstream edits merge cleanly;
    /// behavior changes there must be ported here (see docs/fork-notes.md).
    pub(super) fn render_compact_session_item(
        &self,
        session_id: Uuid,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::current(cx);
        let Some(session) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            return Some(div().into_any_element());
        };
        let selected = sidebar_session_selected(
            self.state.selected_session,
            self.pending_session_activation
                .map(|pending| pending.session_id),
            session_id,
        );
        let project = self
            .state
            .projects
            .iter()
            .find(|project| project.id == session.project_id);
        let grouped_by_project = self.state.sidebar_grouping == SidebarGrouping::Project;
        let left_padding = if grouped_by_project {
            SIDEBAR_GROUP_CHILD_PADDING
        } else {
            8.0
        };
        let branch = persisted_sidebar_branch_label(&session.workspace)
            .map(|branch| SharedString::from(branch.to_owned()))
            .or_else(|| {
                if !matches!(&session.workspace, SessionWorkspace::Local) {
                    return None;
                }
                project.and_then(|project| {
                    self.sidebar_branch_labels
                        .borrow()
                        .get(&project.path)
                        .cloned()
                })
            });
        let branch_cwd = session
            .workspace
            .path()
            .map(Path::to_path_buf)
            .or_else(|| project.map(|project| project.path.clone()));
        let rename_input =
            (self.session_rename == Some(session_id)).then(|| self.session_rename_input.clone());
        let renaming = rename_input.is_some();
        let title = if let Some(rename_input) = rename_input {
            div()
                .id(SharedString::from(format!(
                    "session-rename-field-{session_id}"
                )))
                .key_context(SESSION_RENAME_PARENT_CONTEXT)
                .on_action(cx.listener(|this, _: &CancelSessionRename, window, cx| {
                    this.cancel_session_rename(window, cx);
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
                .text_size(sp(13.5))
                .text_color(theme.text)
                .child(rename_input)
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_w_0()
                .whitespace_normal()
                .line_clamp(1)
                .text_overflow(gpui::TextOverflow::Truncate("...".into()))
                .text_size(sp(13.0))
                .text_color(theme.text)
                .child(SharedString::from(localized_session_title(session)))
                .into_any_element()
        };
        let waku = cx.entity().downgrade();
        let menu = self.menu_handle(format!("session-{session_id}"), cx);
        let row_focus = menu.trigger_focus_handle().clone();
        let keyboard_menu = menu.clone();
        let compact = self.render_compact_row(
            CompactRow {
                status: RowStatus::Session(session.status),
                provider: session.provider,
                title,
                branch,
                branch_cwd,
                time: session_time_label(session, unix_time()),
                time_emphasis: session.is_busy(),
            },
            cx,
        );
        let row = div()
            .id(SharedString::from(format!("session-{}", session.id)))
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .pl(px(left_padding))
            .pr(px(8.0))
            .py(px(5.0))
            .rounded(px(7.0))
            .cursor_default()
            .when(selected, |element| {
                element.bg(theme.sidebar_item_background)
            })
            .hover(|element| element.bg(theme.sidebar_item_background))
            .active(|element| element.bg(theme.sidebar_item_background))
            .child(compact)
            .when(!renaming, |element| {
                element
                    .track_focus(&row_focus)
                    .tab_index(0)
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        let key = event.keystroke.key.as_str();
                        if matches!(key, "enter" | "space") {
                            this.select_session(session_id, cx);
                            cx.stop_propagation();
                        } else if key == "f10" && event.keystroke.modifiers.shift {
                            keyboard_menu.open_context_menu(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_session(session_id, cx);
                    }))
            });
        let row = if renaming {
            div()
                .w_full()
                .child(row)
                .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                    if this.session_rename == Some(session_id) {
                        this.commit_session_rename(cx);
                    }
                }))
                .into_any_element()
        } else {
            context_menu(
                div().w_full().child(row),
                SharedString::from(format!("session-menu-{session_id}")),
                &menu,
                move |_| {
                    let rename_waku = waku.clone();
                    let remove_waku = waku.clone();
                    vec![
                        MenuItem::new(tr!("common.rename"), move |window, cx| {
                            let _ = rename_waku.update(cx, |waku, cx| {
                                waku.begin_session_rename(session_id, window, cx);
                            });
                        }),
                        MenuItem::Separator,
                        MenuItem::new(tr!("common.remove"), move |_, cx| {
                            let _ = remove_waku
                                .update(cx, |waku, cx| waku.remove_session(session_id, cx));
                        }),
                    ]
                },
            )
        };

        Some(
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
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_display_defaults_and_survives_missing_fields() {
        let display: RowDisplay = serde_json::from_str(r#"{"branch":false}"#).unwrap();
        assert!(!display.branch);
        assert!(!display.pull_request && display.harness);
        let mut display = RowDisplay::default();
        display.toggle(RowPart::Harness);
        assert!(!display.get(RowPart::Harness));
    }
}
