use crate::{
    branch_diff::BranchDiff,
    diff_multibuffer::DiffMultibuffer,
    git_panel::{GitPanel, GitPanelAddon, GitStatusEntry},
    staged_diff::StagedDiff,
    unstaged_diff::UnstagedDiff,
};
use anyhow::{Context as _, Result};
use buffer_diff::DiffHunkSecondaryStatus;
use editor::{
    DefaultDiffHunkRenderer, Editor, EditorEvent, SplittableEditor,
    actions::{GoToHunk, GoToPreviousHunk},
};
use git::{Commit, StageAll, StageAndNext, ToggleStaged, UnstageAll, UnstageAndNext};
use gpui::{
    Action, App, AppContext as _, Entity, EventEmitter, FocusHandle, Focusable, Render,
    Subscription, Task, WeakEntity, actions,
};
use language::Capability;
use multi_buffer::MultiBuffer;
use project::{
    Project, ProjectPath,
    git_store::{
        Repository,
        diff_buffer_list::{self, DiffBase},
    },
};
use schemars::JsonSchema;
use serde::Deserialize;
use settings::GitDiffBaseSetting;
use std::any::{Any, TypeId};
use std::sync::Arc;
use ui::{DiffStat, Divider, Tooltip, prelude::*};
use workspace::{
    ItemNavHistory, SerializableItem, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView,
    Workspace,
    item::{Item, ItemEvent, ItemHandle, SaveOptions},
    searchable::SearchableItemHandle,
};
use zed_actions::git as git_actions;

actions!(
    git,
    [
        /// Shows the diff against the configured diff base: working changes
        /// relative to HEAD, or all branch changes relative to the default
        /// branch, following the `git.diff_base` setting.
        Diff,
        /// Shows working changes relative to HEAD.
        DiffHead,
        /// Toggles the git diff base between HEAD and the default branch.
        ToggleDiffBase,
        /// Adds files to the git staging area.
        Add,
        LeaderAndFollower,
        /// Compare with a specific branch
        CompareWithBranch,
    ]
);

/// Shows the diff between the working directory and your default
/// branch (typically main or master).
#[derive(PartialEq, Clone, Deserialize, Default, JsonSchema, Action)]
#[action(namespace = git, name = "DiffBranch", deprecated_aliases = ["git::BranchDiff"])]
pub(crate) struct DeployBranchDiff;

pub struct ProjectDiff {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    diff: Entity<DiffMultibuffer>,
    _diff_event_subscription: Subscription,
    _diff_observation: Subscription,
}

impl ProjectDiff {
    pub(crate) fn register(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
        workspace.register_action(Self::deploy);
        workspace.register_action(|workspace, _: &DiffHead, window, cx| {
            Self::deploy_at(workspace, None, window, cx);
        });
        workspace.register_action(|workspace, _: &ToggleDiffBase, _window, cx| {
            settings::update_settings_file(
                workspace.app_state().fs.clone(),
                cx,
                move |settings, _| {
                    let git = settings.git.get_or_insert_default();
                    git.diff_base = Some(match git.diff_base.unwrap_or_default() {
                        GitDiffBaseSetting::Head => GitDiffBaseSetting::DefaultBranch,
                        GitDiffBaseSetting::DefaultBranch => GitDiffBaseSetting::Head,
                    });
                },
            );
        });
        workspace.register_action(
            |workspace, _: &git_actions::ViewUncommittedChanges, window, cx| {
                Self::deploy_at(workspace, None, window, cx);
            },
        );
        workspace.register_action(
            |workspace, _: &git_actions::ViewUnstagedChanges, window, cx| {
                UnstagedDiff::deploy_at(workspace, None, window, cx);
            },
        );
        workspace.register_action(
            |workspace, _: &git_actions::ViewStagedChanges, window, cx| {
                StagedDiff::deploy_at(workspace, None, window, cx);
            },
        );
        workspace.register_action(|workspace, _: &Add, window, cx| {
            Self::deploy_at(workspace, None, window, cx);
        });
        workspace::register_serializable_item::<ProjectDiff>(cx);
    }

    fn deploy(
        workspace: &mut Workspace,
        _: &Diff,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project = workspace.project().clone();
        if project.read(cx).git_store().read(cx).diff_base() == GitDiffBaseSetting::DefaultBranch {
            BranchDiff::deploy_branch_diff(workspace, window, cx);
            return;
        }
        Self::deploy_at(workspace, None, window, cx)
    }

    pub fn deploy_at(
        workspace: &mut Workspace,
        entry: Option<GitStatusEntry>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let intended_repo = workspace.project().read(cx).active_repository(cx);

        let existing = workspace.items_of_type::<Self>(cx).next();
        let project_diff = if let Some(existing) = existing {
            existing.update(cx, |project_diff, cx| {
                project_diff.move_to_beginning(window, cx);
            });

            workspace.activate_item(&existing, true, true, window, cx);
            existing
        } else {
            let workspace_handle = cx.entity();
            let project_diff =
                cx.new(|cx| Self::new(workspace.project().clone(), workspace_handle, window, cx));
            workspace.add_item_to_active_pane(
                Box::new(project_diff.clone()),
                None,
                true,
                window,
                cx,
            );
            project_diff
        };

        if let Some(intended) = &intended_repo {
            let needs_switch = project_diff
                .read(cx)
                .repo(cx)
                .map_or(true, |current| current.read(cx).id != intended.read(cx).id);
            if needs_switch {
                project_diff.update(cx, |project_diff, cx| {
                    project_diff.set_repo(Some(intended.clone()), cx);
                });
            }
        }

        if let Some(entry) = entry {
            project_diff.update(cx, |project_diff, cx| {
                project_diff.move_to_entry(entry, window, cx);
            })
        }
    }

    pub fn deploy_at_project_path(
        workspace: &mut Workspace,
        project_path: ProjectPath,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let existing = workspace.items_of_type::<Self>(cx).next();
        let project_diff = if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            existing
        } else {
            let workspace_handle = cx.entity();
            let project_diff =
                cx.new(|cx| Self::new(workspace.project().clone(), workspace_handle, window, cx));
            workspace.add_item_to_active_pane(
                Box::new(project_diff.clone()),
                None,
                true,
                window,
                cx,
            );
            project_diff
        };
        project_diff.update(cx, |project_diff, cx| {
            project_diff.move_to_project_path(&project_path, window, cx);
        });
    }

    pub fn autoscroll(&self, cx: &mut Context<Self>) {
        self.diff.update(cx, |diff, cx| diff.autoscroll(cx));
    }

    fn new(
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let git_store = project.read(cx).git_store().clone();
        let branch_diff =
            cx.new(|cx| diff_buffer_list::DiffBufferList::new(DiffBase::Head, git_store, None, cx));
        Self::new_impl(branch_diff, project, workspace, window, cx)
    }

    fn new_impl(
        branch_diff: Entity<diff_buffer_list::DiffBufferList>,
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspace_handle = workspace.downgrade();
        let diff = cx.new(|cx| {
            DiffMultibuffer::new(
                branch_diff,
                Capability::ReadWrite,
                "No uncommitted changes",
                move |editor, cx| {
                    editor.set_diff_hunk_renderer(Some(Arc::new(DefaultDiffHunkRenderer)), cx);
                    editor.rhs_editor().update(cx, |rhs_editor, _cx| {
                        rhs_editor.set_read_only(false);
                        rhs_editor.register_addon(GitPanelAddon {
                            workspace: workspace_handle,
                        });
                    });
                },
                project.clone(),
                workspace.clone(),
                window,
                cx,
            )
        });
        Self::from_diff(diff, project, workspace, cx)
    }

    fn from_diff(
        diff: Entity<DiffMultibuffer>,
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let diff_event_subscription = cx.subscribe(&diff, |_, _, event: &EditorEvent, cx| {
            if event == &(EditorEvent::SelectionsChanged { local: true }) {
                cx.emit(event.clone())
            }
        });
        let diff_observation = cx.observe(&diff, |_, _, cx| cx.notify());

        Self {
            project,
            workspace: workspace.downgrade(),
            diff,
            _diff_event_subscription: diff_event_subscription,
            _diff_observation: diff_observation,
        }
    }

    pub fn diff_base<'a>(&'a self, cx: &'a App) -> &'a DiffBase {
        self.diff.read(cx).diff_base(cx)
    }

    pub(crate) fn repo(&self, cx: &App) -> Option<Entity<Repository>> {
        self.diff.read(cx).repo(cx)
    }

    pub(crate) fn set_repo(&mut self, repo: Option<Entity<Repository>>, cx: &mut Context<Self>) {
        self.diff
            .update(cx, |diff, cx| diff.set_repo(repo.clone(), cx));
    }

    pub fn move_to_entry(
        &mut self,
        entry: GitStatusEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff
            .update(cx, |diff, cx| diff.move_to_entry(entry, window, cx));
    }

    pub fn move_to_project_path(
        &mut self,
        project_path: &ProjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff.update(cx, |diff, cx| {
            diff.move_to_project_path(project_path, window, cx)
        });
    }

    fn move_to_beginning(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.diff
            .update(cx, |diff, cx| diff.move_to_beginning(window, cx));
    }

    pub fn calculate_changed_lines(&self, cx: &App) -> (u32, u32) {
        self.diff.read(cx).calculate_changed_lines(cx)
    }

    /// Returns the splittable editor of the currently-shown diff view.
    pub fn editor(&self, cx: &App) -> Entity<SplittableEditor> {
        self.diff.read(cx).editor().clone()
    }

    /// Returns the multibuffer of the currently-shown diff view.
    pub fn multibuffer(&self, cx: &App) -> Entity<MultiBuffer> {
        self.diff.read(cx).multibuffer().clone()
    }

    fn button_states(&self, cx: &App) -> ButtonStates {
        let diff = self.diff.read(cx);
        let editor = diff.editor().read(cx).rhs_editor().clone();
        let editor = editor.read(cx);
        let snapshot = diff.multibuffer().read(cx).snapshot(cx);
        let prev_next = snapshot.diff_hunks().next().is_some();
        let (selection, ranges) = diff.selected_ranges(cx);
        let mut has_staged_hunks = false;
        let mut has_unstaged_hunks = false;
        for hunk in editor.diff_hunks_in_ranges(&ranges, &snapshot) {
            match hunk.status.secondary {
                DiffHunkSecondaryStatus::HasSecondaryHunk
                | DiffHunkSecondaryStatus::SecondaryHunkAdditionPending => {
                    has_unstaged_hunks = true;
                }
                DiffHunkSecondaryStatus::OverlapsWithSecondaryHunk => {
                    has_staged_hunks = true;
                    has_unstaged_hunks = true;
                }
                DiffHunkSecondaryStatus::NoSecondaryHunk
                | DiffHunkSecondaryStatus::SecondaryHunkRemovalPending => {
                    has_staged_hunks = true;
                }
            }
        }
        let mut stage_all = false;
        let mut unstage_all = false;
        self.workspace
            .read_with(cx, |workspace, cx| {
                if let Some(git_panel) = workspace.panel::<GitPanel>(cx) {
                    let git_panel = git_panel.read(cx);
                    stage_all = git_panel.can_stage_all();
                    unstage_all = git_panel.can_unstage_all();
                }
            })
            .ok();

        ButtonStates {
            stage: has_unstaged_hunks,
            unstage: has_staged_hunks,
            prev_next,
            selection,
            stage_all,
            unstage_all,
        }
    }

    #[cfg(feature = "test-support")]
    pub fn excerpt_paths(&self, cx: &App) -> Vec<std::sync::Arc<util::rel_path::RelPath>> {
        self.diff.read(cx).excerpt_paths(cx)
    }

    #[cfg(feature = "test-support")]
    pub fn excerpt_file_paths(&self, cx: &App) -> Vec<String> {
        self.diff.read(cx).excerpt_file_paths(cx)
    }
}

struct ButtonStates {
    stage: bool,
    unstage: bool,
    prev_next: bool,
    selection: bool,
    stage_all: bool,
    unstage_all: bool,
}

impl EventEmitter<EditorEvent> for ProjectDiff {}

impl Focusable for ProjectDiff {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.diff.read(cx).focus_handle(cx)
    }
}

impl Item for ProjectDiff {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitBranch).color(Color::Muted))
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.diff
            .update(cx, |diff, cx| diff.deactivated(window, cx));
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.diff
            .update(cx, |diff, cx| diff.navigate(data, window, cx))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(self.tab_content_text(0, cx))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Uncommitted Changes".into()
    }

    fn as_searchable(&self, _: &Entity<Self>, cx: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.diff.read(cx).editor().clone()))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.diff.read(cx).for_each_project_item(cx, f)
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.diff.read(cx).active_project_path(cx)
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff
            .update(cx, |diff, cx| diff.set_nav_history(nav_history, cx));
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        let Some(workspace) = self.workspace.upgrade() else {
            return Task::ready(None);
        };
        Task::ready(Some(cx.new(|cx| {
            ProjectDiff::new(self.project.clone(), workspace, window, cx)
        })))
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.diff.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.diff.read(cx).has_conflict(cx)
    }

    fn can_save(&self, _: &App) -> bool {
        true
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.diff
            .update(cx, |diff, cx| diff.save(options, project, window, cx))
    }

    fn save_as(
        &mut self,
        _: Entity<Project>,
        _: ProjectPath,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> Task<Result<()>> {
        unreachable!()
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.diff
            .update(cx, |diff, cx| diff.reload(project, window, cx))
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(
                self.diff
                    .read(cx)
                    .editor()
                    .read(cx)
                    .rhs_editor()
                    .clone()
                    .into(),
            )
        } else if type_id == TypeId::of::<SplittableEditor>() {
            Some(self.diff.read(cx).editor().clone().into())
        } else if type_id == TypeId::of::<diff_buffer_list::DiffBufferList>() {
            Some(self.diff.read(cx).branch_diff().clone().into())
        } else {
            None
        }
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff.update(cx, |diff, cx| {
            diff.added_to_workspace(workspace, window, cx)
        });
    }
}

impl Render for ProjectDiff {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.diff.clone())
    }
}

impl SerializableItem for ProjectDiff {
    fn serialized_item_kind() -> &'static str {
        "ProjectDiff"
    }

    fn cleanup(
        _: workspace::WorkspaceId,
        _: Vec<workspace::ItemId>,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn deserialize(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        _workspace_id: workspace::WorkspaceId,
        _item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        window.spawn(cx, async move |cx| {
            cx.update(|window, cx| {
                let git_store = project.read(cx).git_store().clone();
                let branch_diff = cx.new(|cx| {
                    diff_buffer_list::DiffBufferList::new(DiffBase::Head, git_store, None, cx)
                });
                let workspace = workspace.upgrade().context("workspace gone")?;
                anyhow::Ok(
                    cx.new(|cx| ProjectDiff::new_impl(branch_diff, project, workspace, window, cx)),
                )
            })?
        })
    }

    fn serialize(
        &mut self,
        _: &mut Workspace,
        _: workspace::ItemId,
        _: bool,
        _: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        Some(Task::ready(Ok(())))
    }

    fn should_serialize(&self, _: &Self::Event) -> bool {
        false
    }
}

pub(crate) mod persistence {

    use anyhow::Context as _;
    use db::{
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use project::git_store::diff_buffer_list::DiffBase;
    use workspace::{ItemId, WorkspaceDb, WorkspaceId};

    pub struct ProjectDiffDb(ThreadSafeConnection);

    impl Domain for ProjectDiffDb {
        const NAME: &str = stringify!(ProjectDiffDb);

        // Legacy databases stored branch diffs under the "ProjectDiff" item
        // kind, disambiguated by the `diff_base` column. Step 1 rewrites those
        // item kinds so that each diff view owns its serialized kind.
        const MIGRATIONS: &[&str] = &[
            sql!(
                CREATE TABLE project_diffs(
                    workspace_id INTEGER,
                    item_id INTEGER UNIQUE,

                    diff_base TEXT,

                    PRIMARY KEY(workspace_id, item_id),
                    FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
                ) STRICT;
            ),
            r#"
                UPDATE items SET kind = 'BranchDiff'
                WHERE kind = 'ProjectDiff' AND EXISTS (
                    SELECT 1 FROM project_diffs
                    WHERE project_diffs.item_id = items.item_id
                    AND project_diffs.workspace_id = items.workspace_id
                    AND project_diffs.diff_base LIKE '{"Merge"%'
                );
            "#,
        ];
    }

    db::static_connection!(ProjectDiffDb, [WorkspaceDb]);

    impl ProjectDiffDb {
        pub async fn save_project_diff_base(
            &self,
            item_id: ItemId,
            workspace_id: WorkspaceId,
            diff_base: DiffBase,
        ) -> anyhow::Result<()> {
            self.write(move |connection| {
                let sql_stmt = sql!(
                    INSERT OR REPLACE INTO project_diffs(item_id, workspace_id, diff_base) VALUES (?, ?, ?)
                );
                let diff_base_str = serde_json::to_string(&diff_base)?;
                let mut query = connection.exec_bound::<(ItemId, WorkspaceId, String)>(sql_stmt)?;
                query((item_id, workspace_id, diff_base_str)).context(format!(
                    "exec_bound failed to execute or parse for: {}",
                    sql_stmt
                ))
            })
            .await
        }

        pub fn get_project_diff_base(
            &self,
            item_id: ItemId,
            workspace_id: WorkspaceId,
        ) -> anyhow::Result<DiffBase> {
            let sql_stmt =
                sql!(SELECT diff_base FROM project_diffs WHERE item_id =  ?AND workspace_id =  ?);
            let diff_base_str = self.select_row_bound::<(ItemId, WorkspaceId), String>(sql_stmt)?(
                (item_id, workspace_id),
            )
            .context(::std::format!(
                "Error in get_diff_base, select_row_bound failed to execute or parse for: {}",
                sql_stmt
            ))?;
            let Some(diff_base_str) = diff_base_str else {
                return Ok(DiffBase::Head);
            };
            serde_json::from_str(&diff_base_str).context("deserializing diff base")
        }
    }
}

pub struct ProjectDiffToolbar {
    project_diff: Option<WeakEntity<ProjectDiff>>,
    workspace: WeakEntity<Workspace>,
}

impl ProjectDiffToolbar {
    pub fn new(workspace: &Workspace, _: &mut Context<Self>) -> Self {
        Self {
            project_diff: None,
            workspace: workspace.weak_handle(),
        }
    }

    fn project_diff(&self, _: &App) -> Option<Entity<ProjectDiff>> {
        self.project_diff.as_ref()?.upgrade()
    }

    fn dispatch_action(&self, action: &dyn Action, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(project_diff) = self.project_diff(cx) {
            project_diff.focus_handle(cx).focus(window, cx);
        }
        let action = action.boxed_clone();
        cx.defer(move |cx| {
            cx.dispatch_action(action.as_ref());
        })
    }

    fn stage_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                if let Some(panel) = workspace.panel::<GitPanel>(cx) {
                    panel.update(cx, |panel, cx| {
                        panel.stage_all(&Default::default(), window, cx);
                    });
                }
            })
            .ok();
    }

    fn unstage_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                let Some(panel) = workspace.panel::<GitPanel>(cx) else {
                    return;
                };
                panel.update(cx, |panel, cx| {
                    panel.unstage_all(&Default::default(), window, cx);
                });
            })
            .ok();
    }
}

impl EventEmitter<ToolbarItemEvent> for ProjectDiffToolbar {}

impl ToolbarItemView for ProjectDiffToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.project_diff = active_pane_item
            .and_then(|item| item.act_as::<ProjectDiff>(cx))
            .map(|entity| entity.downgrade());
        if self.project_diff.is_some() {
            ToolbarItemLocation::PrimaryRight
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn pane_focus_update(
        &mut self,
        _pane_focused: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }
}

impl Render for ProjectDiffToolbar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(project_diff) = self.project_diff(cx) else {
            return div();
        };
        let focus_handle = project_diff.focus_handle(cx);
        let button_states = project_diff.read(cx).button_states(cx);

        let (additions, deletions) = project_diff.read(cx).calculate_changed_lines(cx);
        let is_multibuffer_empty = project_diff.read(cx).multibuffer(cx).read(cx).is_empty();

        let stage_all_button_width = rems(5.);

        h_flex()
            .my_neg_1()
            .py_1()
            .gap_1p5()
            .flex_wrap()
            .justify_between()
            .when(!is_multibuffer_empty, |this| {
                this.child(DiffStat::new(
                    "project-diff-stat",
                    additions as usize,
                    deletions as usize,
                ))
                .child(Divider::vertical().ml_1())
            })
            // n.b. the only reason these arrows are here is because we don't
            // support "undo" for staging so we need a way to go back.
            .child(
                h_group_sm()
                    .child(
                        IconButton::new("up", IconName::ArrowUp)
                            .icon_size(IconSize::Small)
                            .disabled(!button_states.prev_next)
                            .tooltip(Tooltip::for_action_title_in(
                                "Go to Previous Hunk",
                                &GoToPreviousHunk,
                                &focus_handle,
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dispatch_action(&GoToPreviousHunk, window, cx)
                            })),
                    )
                    .child(
                        IconButton::new("down", IconName::ArrowDown)
                            .icon_size(IconSize::Small)
                            .disabled(!button_states.prev_next)
                            .tooltip(Tooltip::for_action_title_in(
                                "Go to Next Hunk",
                                &GoToHunk,
                                &focus_handle,
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dispatch_action(&GoToHunk, window, cx)
                            })),
                    ),
            )
            .child(Divider::vertical())
            .child(
                h_group_sm()
                    .when(button_states.selection, |this| {
                        this.child(
                            Button::new("stage", "Toggle Staged")
                                .tooltip(Tooltip::for_action_title_in(
                                    "Toggle Staged",
                                    &ToggleStaged,
                                    &focus_handle,
                                ))
                                .disabled(!button_states.stage && !button_states.unstage)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.dispatch_action(&ToggleStaged, window, cx)
                                })),
                        )
                    })
                    .when(!button_states.selection, |this| {
                        this.child(
                            Button::new("stage", "Stage")
                                .disabled(!button_states.stage)
                                .tooltip(Tooltip::for_action_title_in(
                                    "Stage and Go to Next Hunk",
                                    &StageAndNext,
                                    &focus_handle,
                                ))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.dispatch_action(&StageAndNext, window, cx)
                                })),
                        )
                        .child(
                            Button::new("unstage", "Unstage")
                                .disabled(!button_states.unstage)
                                .tooltip(Tooltip::for_action_title_in(
                                    "Unstage and Go to Next Hunk",
                                    &UnstageAndNext,
                                    &focus_handle,
                                ))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.dispatch_action(&UnstageAndNext, window, cx)
                                })),
                        )
                    }),
            )
            .child(Divider::vertical())
            .when(
                button_states.unstage_all && !button_states.stage_all,
                |this| {
                    this.child(
                        Button::new("unstage-all", "Unstage All")
                            .width(stage_all_button_width)
                            .tooltip(Tooltip::for_action_title_in(
                                "Unstage All Changes",
                                &UnstageAll,
                                &focus_handle,
                            ))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.unstage_all(window, cx)),
                            ),
                    )
                },
            )
            .when(
                !button_states.unstage_all || button_states.stage_all,
                |this| {
                    this.child(
                        Button::new("stage-all", "Stage All")
                            .width(stage_all_button_width)
                            .disabled(!button_states.stage_all)
                            .tooltip(Tooltip::for_action_title_in(
                                "Stage All Changes",
                                &StageAll,
                                &focus_handle,
                            ))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.stage_all(window, cx)),
                            ),
                    )
                },
            )
            .child(Divider::vertical())
            .child(
                Button::new("commit", "Commit")
                    .tooltip(Tooltip::for_action_title_in(
                        "Commit",
                        &Commit,
                        &focus_handle,
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.dispatch_action(&Commit, window, cx);
                    })),
            )
    }
}

