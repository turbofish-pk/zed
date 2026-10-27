use std::any::TypeId;
use std::borrow::Cow;
use std::cmp::min;
use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use editor::items::open_resolved_target;
use editor::scroll::Autoscroll;
use editor::{
    Editor, EditorEvent, EditorSettingsScrollbarProxy, MAX_TAB_TITLE_LEN, MultiBufferOffset,
    SelectionEffects,
};
use gpui::{
    Action, AnyElement, App, ClipboardItem, Context, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, Global, ImageSource, InteractiveElement, IntoElement, IsZero, Pixels, Render,
    Resource, RetainAllImageCache, ScrollHandle, SharedString, SharedUri, Subscription, Task,
    WeakEntity, Window, point, px,
};
use language::{Buffer, LanguageRegistry};
use markdown::{
    CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownFont,
    MarkdownOptions, MarkdownStyle,
};
use project::search::SearchQuery;
use project::{Project, ProjectPath, image_store};
use settings::{SeedQuerySetting, Settings, update_settings_file};
use theme::{SystemAppearance, Theme, ThemeRegistry};
use theme_settings::ThemeSettings;
use ui::utils::WithRemSize;
use ui::{
    ContextMenu, LinkPreview, ScrollAxes, Scrollbars, WithScrollbar, prelude::*, right_click_menu,
};
use util::{
    ResultExt,
    markdown::{source_position_from_fragment, split_local_url_fragment},
    paths::{PathStyle, PathWithPosition},
    rel_path::RelPath,
};
use workspace::item::{
    Item, ItemBufferKind, ItemHandle, SaveOptions, SerializableItem, TabContentParams,
};
use workspace::notifications::{NotifyResultExt, NotifyTaskExt};
use workspace::path_link::{PathMatching, resolve_open_target};
use workspace::searchable::{
    Direction, SearchEvent, SearchOptions, SearchToken, SearchableItem, SearchableItemHandle,
};
use workspace::{
    ItemId, ItemNavHistory, Pane, SaveIntent, Workspace, WorkspaceId, delete_unloaded_items,
};
use zed_actions::{DecreaseBufferFontSize, IncreaseBufferFontSize, ResetBufferFontSize};

use crate::markdown_preview_settings::MarkdownPreviewSettings;
use crate::{
    CloseAndReturnToEditor, OpenFollowingPreview, OpenPreview, OpenPreviewToTheSide, ScrollDown,
    ScrollDownByItem,
};
use crate::{ScrollPageDown, ScrollPageUp, ScrollToBottom, ScrollToTop, ScrollUp, ScrollUpByItem};

const REPARSE_DEBOUNCE: Duration = Duration::from_millis(200);

fn reset_persisted_font_size(settings: &mut settings::SettingsContent) {
    if let Some(markdown_preview) = settings.markdown_preview.as_mut() {
        markdown_preview.font_size = None;
    }
}

pub struct MarkdownPreviewView {
    workspace: WeakEntity<Workspace>,
    self_handle: WeakEntity<Self>,
    active_editor: Option<EditorState>,
    focus_handle: FocusHandle,
    markdown: Entity<Markdown>,
    _markdown_subscription: Subscription,
    active_source_index: Option<usize>,
    scroll_handle: ScrollHandle,
    image_cache: Entity<RetainAllImageCache>,
    base_directory: Option<PathBuf>,
    pending_update_task: Option<Task<Result<()>>>,
    hovered_url: Option<SharedString>,
    mode: MarkdownPreviewMode,
    /// Search results depend on the parsed markdown, which lags behind the source while a
    /// background parse is in flight. Tracked so matches can be invalidated once it lands.
    markdown_parse_pending: bool,
    nav_history: Option<ItemNavHistory>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MarkdownPreviewMode {
    /// The preview will always show the contents of the provided editor.
    Default,
    /// The preview will "follow" the currently active editor.
    Follow,
}

impl MarkdownPreviewMode {
    fn to_db(self) -> i64 {
        match self {
            Self::Default => 0,
            Self::Follow => 1,
        }
    }

    fn from_db(value: i64) -> Self {
        match value {
            1 => Self::Follow,
            _ => Self::Default,
        }
    }
}

struct EditorState {
    editor: Entity<Editor>,
    _subscription: Subscription,
}

#[derive(Default)]
struct SuppressedAutoPreviews(HashSet<EntityId>);

impl Global for SuppressedAutoPreviews {}

#[derive(Clone, Copy, Debug)]
pub enum MarkdownPreviewEvent {
    SourceEditorChanged,
    SourceFileHandleChanged,
}

enum PreviewLinkTarget {
    Heading(SharedString),
    Position { row: u32, column: u32 },
}

impl MarkdownPreviewView {
    pub fn register(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        cx.default_global::<SuppressedAutoPreviews>();
        let workspace_entity = cx.entity();
        cx.subscribe_in(
            &workspace_entity,
            window,
            |workspace, _, event: &workspace::Event, window, cx| {
                let workspace::Event::ItemAdded { item } = event else {
                    return;
                };
                if cx
                    .global_mut::<SuppressedAutoPreviews>()
                    .0
                    .contains(&item.item_id())
                {
                    return;
                }
                if !MarkdownPreviewSettings::get_global(cx).open_markdown_files_in_preview
                    || workspace.is_restoring()
                {
                    return;
                }
                let Some(editor) = item.downcast::<Editor>() else {
                    return;
                };
                if !Self::is_markdown_editor(workspace, &editor, cx) {
                    return;
                }
                let Some(pane) = workspace.pane_for_item_id(item.item_id()) else {
                    return;
                };
                let replaces_editor = !item.is_dirty(cx);
                let replaces_temporary_editor =
                    replaces_editor && pane.read(cx).is_active_preview_item(item.item_id());
                let activate = workspace.active_pane() == &pane
                    && pane
                        .read(cx)
                        .active_item()
                        .is_some_and(|active_item| active_item.item_id() == item.item_id());
                let focus = editor.focus_handle(cx).contains_focused(window, cx);
                let preview = Self::activate_or_add_preview(
                    workspace,
                    editor,
                    pane.clone(),
                    activate,
                    focus,
                    window,
                    cx,
                );
                if !replaces_editor {
                    return;
                }
                pane.update(cx, |pane, cx| {
                    if replaces_temporary_editor {
                        pane.replace_preview_item_id(preview.entity_id(), window, cx);
                    } else {
                        pane.remove_item(item.item_id(), false, false, window, cx);
                    }
                });
            },
        )
        .detach();

        workspace.register_action(move |workspace, _: &OpenPreview, window, cx| {
            if let Some(item) = workspace.item_for_action(window, cx)
                && let Some(editor) = item.act_as::<Editor>(cx)
                && Self::is_markdown_file(&editor, cx)
                && let Some(pane) = workspace.pane_for_item_id(item.item_id())
            {
                Self::open_preview_in_pane(workspace, editor, pane, window, cx);
            }
        });

        workspace.register_action(move |workspace, _: &OpenPreviewToTheSide, window, cx| {
            if let Some(editor) = Self::resolve_active_item_as_markdown_editor(workspace, cx) {
                let pane = workspace.active_pane().clone();
                Self::open_preview_to_the_side_of_pane(workspace, editor, pane, window, cx);
            }
        });

        workspace.register_action(move |workspace, _: &OpenFollowingPreview, window, cx| {
            if let Some(editor) = Self::resolve_active_item_as_markdown_editor(workspace, cx) {
                // Check if there's already a following preview
                let existing_follow_view_idx = {
                    let active_pane = workspace.active_pane().read(cx);
                    active_pane
                        .items_of_type::<MarkdownPreviewView>()
                        .find(|view| view.read(cx).mode == MarkdownPreviewMode::Follow)
                        .and_then(|view| active_pane.index_for_item(&view))
                };

                if let Some(existing_follow_view_idx) = existing_follow_view_idx {
                    workspace.active_pane().update(cx, |pane, cx| {
                        pane.activate_item(existing_follow_view_idx, true, true, window, cx);
                    });
                } else {
                    let view = Self::create_following_markdown_view(workspace, editor, window, cx);
                    workspace.active_pane().update(cx, |pane, cx| {
                        pane.add_item(Box::new(view.clone()), true, true, None, window, cx)
                    });
                }
                cx.notify();
            }
        });
    }

    pub fn open_preview_in_pane(
        workspace: &mut Workspace,
        editor: Entity<Editor>,
        pane: Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        Self::activate_or_add_preview(workspace, editor, pane, true, true, window, cx);
    }

    pub fn open_preview_to_the_side_of_pane(
        workspace: &mut Workspace,
        editor: Entity<Editor>,
        origin_pane: Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let target_pane = workspace.adjacent_pane_of(&origin_pane, window, cx);
        Self::activate_or_add_preview(
            workspace,
            editor.clone(),
            target_pane,
            false,
            false,
            window,
            cx,
        );
        editor.focus_handle(cx).focus(window, cx);
    }

    fn activate_or_add_preview(
        workspace: &mut Workspace,
        editor: Entity<Editor>,
        pane: Entity<Pane>,
        activate: bool,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<MarkdownPreviewView> {
        let existing_view =
            Self::find_existing_independent_preview_item_idx(pane.read(cx), &editor, cx);
        let view = if let Some((existing_view, existing_view_idx)) = existing_view {
            pane.update(cx, |pane, cx| {
                pane.activate_item(existing_view_idx, activate, focus, window, cx);
            });
            existing_view
        } else {
            let view = Self::create_markdown_view(workspace, editor, window, cx);
            pane.update(cx, |pane, cx| {
                pane.add_item(Box::new(view.clone()), activate, focus, None, window, cx)
            });
            view
        };
        cx.notify();
        view
    }

    fn find_existing_independent_preview_item_idx(
        pane: &Pane,
        editor: &Entity<Editor>,
        cx: &App,
    ) -> Option<(Entity<MarkdownPreviewView>, usize)> {
        let target_buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
        let view = pane.items_of_type::<MarkdownPreviewView>().find(|view| {
            // Only look for independent (Default mode) previews, not Follow previews.
            // Match by buffer entity rather than editor entity so the lookup survives
            // workspace restoration, where the preview's bound editor may differ from
            // the editor the user is currently invoking the action on even though both
            // wrap the same source buffer.
            view.read(cx).mode == MarkdownPreviewMode::Default
                && view.read(cx).is_previewing(&target_buffer, cx)
        })?;
        let index = pane.index_for_item(&view)?;
        Some((view, index))
    }

    fn is_previewing(&self, buffer: &Entity<Buffer>, cx: &App) -> bool {
        self.active_editor.as_ref().is_some_and(|state| {
            state
                .editor
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                .as_ref()
                == Some(buffer)
        })
    }

    pub fn resolve_active_item_as_markdown_editor(
        workspace: &Workspace,
        cx: &mut Context<Workspace>,
    ) -> Option<Entity<Editor>> {
        if let Some(editor) = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx))
            && Self::is_markdown_file(&editor, cx)
        {
            return Some(editor);
        }
        None
    }

    pub fn create_markdown_view(
        workspace: &mut Workspace,
        editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<MarkdownPreviewView> {
        let language_registry = workspace.project().read(cx).languages().clone();
        let workspace_handle = workspace.weak_handle();
        MarkdownPreviewView::new(
            MarkdownPreviewMode::Default,
            editor,
            workspace_handle,
            language_registry,
            window,
            cx,
        )
    }

    fn create_following_markdown_view(
        workspace: &mut Workspace,
        editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<MarkdownPreviewView> {
        let language_registry = workspace.project().read(cx).languages().clone();
        let workspace_handle = workspace.weak_handle();
        MarkdownPreviewView::new(
            MarkdownPreviewMode::Follow,
            editor,
            workspace_handle,
            language_registry,
            window,
            cx,
        )
    }

    pub fn new(
        mode: MarkdownPreviewMode,
        active_editor: Entity<Editor>,
        workspace: WeakEntity<Workspace>,
        language_registry: Arc<LanguageRegistry>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let markdown = cx.new(|cx| {
                Markdown::new_with_options(
                    SharedString::default(),
                    Some(language_registry),
                    None,
                    MarkdownOptions {
                        parse_html: true,
                        render_mermaid_diagrams: true,
                        parse_heading_slugs: true,
                        render_metadata_blocks: true,
                        ..Default::default()
                    },
                    cx,
                )
            });
            let mut this = Self {
                active_editor: None,
                focus_handle: cx.focus_handle(),
                workspace: workspace.clone(),
                self_handle: cx.weak_entity(),
                _markdown_subscription: cx.observe(
                    &markdown,
                    |this: &mut Self, markdown: Entity<Markdown>, cx| {
                        this.sync_active_root_block(cx);
                        let is_parsing = markdown.read(cx).is_parsing();
                        if this.markdown_parse_pending && !is_parsing {
                            cx.emit(SearchEvent::MatchesInvalidated);
                        }
                        this.markdown_parse_pending = is_parsing;
                    },
                ),
                markdown,
                active_source_index: None,
                scroll_handle: ScrollHandle::new(),
                image_cache: RetainAllImageCache::new(cx),
                base_directory: None,
                pending_update_task: None,
                hovered_url: None,
                mode,
                markdown_parse_pending: false,
                nav_history: None,
            };

            this.set_editor(active_editor, window, cx);

            match mode {
                MarkdownPreviewMode::Follow => {
                    if let Some(workspace) = &workspace.upgrade() {
                        cx.observe_in(workspace, window, |this, workspace, window, cx| {
                            let item = workspace.read(cx).active_item(cx);
                            this.workspace_updated(item, window, cx);
                        })
                        .detach();
                    } else {
                        log::error!("Failed to listen to workspace updates");
                    }
                }
                MarkdownPreviewMode::Default => {
                    // After workspace restoration the bound editor may be an orphan that
                    // wraps the right buffer but isn't the canonical Editor instance in
                    // any pane. Re-binding to the workspace's editor for our buffer is
                    // what restores cursor-driven scroll sync — `SelectionsChanged` only
                    // fires from the editor the user actually interacts with.
                    //
                    // Subscribing to `workspace::Event` (rather than `observe`) keeps the
                    // rebind check off the cursor-move hot path; `observe` would fire on
                    // every workspace `cx.notify`.
                    if let Some(workspace) = &workspace.upgrade() {
                        cx.subscribe_in(workspace, window, Self::on_workspace_event)
                            .detach();
                    }
                }
            }

            this
        })
    }

    fn workspace_updated(
        &mut self,
        active_item: Option<Box<dyn ItemHandle>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(item) = active_item
            && item.item_id() != cx.entity_id()
            && let Some(editor) = item.act_as::<Editor>(cx)
            && Self::is_markdown_file(&editor, cx)
        {
            self.set_editor(editor, window, cx);
        }
    }

    pub fn is_markdown_path(path: impl AsRef<Path>, languages: &Arc<LanguageRegistry>) -> bool {
        languages
            .language_for_file_path(path.as_ref())
            .is_some_and(|language_id| {
                languages
                    .available_language_for_name("Markdown")
                    .is_some_and(|markdown| markdown.id() == language_id)
            })
    }

    pub fn open_for_project_path(
        project_path: ProjectPath,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let open_buffer = workspace
            .project()
            .update(cx, |project, cx| project.open_buffer(project_path, cx));

        cx.spawn_in(window, async move |workspace, mut cx| {
            let Some(buffer) = open_buffer
                .await
                .notify_workspace_async_err(workspace.clone(), &mut cx)
            else {
                return;
            };
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let project = workspace.project().clone();
                    let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project), window, cx));
                    let preview = Self::create_markdown_view(workspace, editor, window, cx);
                    workspace.active_pane().update(cx, |pane, cx| {
                        pane.add_item(Box::new(preview), true, true, None, window, cx);
                    });
                })
                .ok();
        })
        .detach();
    }

    pub fn is_markdown_file(editor: &Entity<Editor>, cx: &App) -> bool {
        let buffer = editor.read(cx).buffer().read(cx);
        if let Some(buffer) = buffer.as_singleton()
            && let Some(language) = buffer.read(cx).language()
        {
            return language.name() == "Markdown";
        }
        false
    }

    fn is_markdown_editor(workspace: &Workspace, editor: &Entity<Editor>, cx: &App) -> bool {
        let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() else {
            return false;
        };
        let buffer = buffer.read(cx);
        if let Some(language) = buffer.language() {
            return language.name() == "Markdown";
        }
        let Some(file) = buffer.file() else {
            return false;
        };
        let languages = workspace.project().read(cx).languages().clone();
        let Some(markdown) = languages.available_language_for_name("Markdown") else {
            return false;
        };
        languages.language_for_file(file, None, cx) == Some(markdown.id())
    }

    fn set_editor(&mut self, editor: Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(active) = &self.active_editor
            && active.editor == editor
        {
            return;
        }

        let had_active_editor = self.active_editor.is_some();
        let subscription = cx.subscribe_in(
            &editor,
            window,
            |this, editor, event: &EditorEvent, window, cx| {
                match event {
                    EditorEvent::Edited { .. }
                    | EditorEvent::BufferEdited { .. }
                    | EditorEvent::DirtyChanged
                    | EditorEvent::BuffersEdited { .. } => {
                        this.update_markdown_from_active_editor(true, false, window, cx);
                    }
                    EditorEvent::FileHandleChanged => {
                        this.base_directory =
                            Self::get_folder_for_active_editor(editor.read(cx), cx);
                        this.update_markdown_from_active_editor(false, false, window, cx);
                        cx.emit(MarkdownPreviewEvent::SourceFileHandleChanged);
                    }
                    EditorEvent::SelectionsChanged { .. } => {
                        let (selection_start, editor_is_focused) =
                            editor.update(cx, |editor, cx| {
                                let index = Self::selected_source_index(editor, cx);
                                let focused = editor.focus_handle(cx).is_focused(window);
                                (index, focused)
                            });
                        if let Some(selection_start) = selection_start {
                            let reveal = editor_is_focused || this.focus_handle.is_focused(window);
                            this.sync_preview_to_source_index(selection_start, reveal, cx);
                            cx.notify();
                        }
                    }
                    _ => {}
                };
            },
        );

        self.base_directory = Self::get_folder_for_active_editor(editor.read(cx), cx);
        self.hovered_url = None;
        self.active_editor = Some(EditorState {
            editor,
            _subscription: subscription,
        });
        self.update_markdown_from_active_editor(false, true, window, cx);
        if had_active_editor {
            cx.emit(MarkdownPreviewEvent::SourceEditorChanged);
        }
    }

    fn on_workspace_event(
        &mut self,
        workspace: &Entity<Workspace>,
        event: &workspace::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(
            event,
            workspace::Event::ItemAdded { .. } | workspace::Event::ItemRemoved { .. }
        ) {
            return;
        }
        let candidate = self.find_canonical_editor(workspace.read(cx), cx);
        if let Some(editor) = candidate
            && self
                .active_editor
                .as_ref()
                .is_none_or(|s| s.editor != editor)
        {
            self.set_editor(editor, window, cx);
        }
    }

    fn find_canonical_editor(&self, workspace: &Workspace, cx: &App) -> Option<Entity<Editor>> {
        let current = self.active_editor.as_ref()?.editor.clone();
        let our_buffer = current.read(cx).buffer().read(cx).as_singleton()?;
        let mut fallback = None;
        for editor in workspace.items_of_type::<Editor>(cx) {
            if editor.read(cx).buffer().read(cx).as_singleton().as_ref() != Some(&our_buffer) {
                continue;
            }
            if editor == current {
                return Some(current);
            }
            fallback.get_or_insert(editor);
        }
        fallback
    }

    fn update_markdown_from_active_editor(
        &mut self,
        wait_for_debounce: bool,
        should_reveal: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = &self.active_editor {
            // if there is already a task to update the ui and the current task is also debounced (not high priority), do nothing
            if wait_for_debounce && self.pending_update_task.is_some() {
                return;
            }
            self.pending_update_task = Some(self.schedule_markdown_update(
                wait_for_debounce,
                should_reveal,
                state.editor.clone(),
                window,
                cx,
            ));
        }
    }

    fn schedule_markdown_update(
        &mut self,
        wait_for_debounce: bool,
        should_reveal_selection: bool,
        editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        cx.spawn_in(window, async move |view, cx| {
            if wait_for_debounce {
                // Wait for the user to stop typing
                cx.background_executor().timer(REPARSE_DEBOUNCE).await;
            }

            let update = view.update(cx, |view, cx| {
                let is_active_editor = view
                    .active_editor
                    .as_ref()
                    .is_some_and(|active_editor| active_editor.editor == editor);
                if !is_active_editor {
                    return None;
                }

                editor.update(cx, |editor, cx| {
                    let contents = editor
                        .buffer()
                        .read(cx)
                        .as_singleton()?
                        .read(cx)
                        .as_rope()
                        .to_string()
                        .into();
                    let selection_start = Self::selected_source_index(editor, cx)?;
                    Some((contents, selection_start))
                })
            })?;

            view.update(cx, move |view, cx| {
                if let Some((contents, selection_start)) = update {
                    view.hovered_url = None;
                    view.markdown.update(cx, |markdown, cx| {
                        markdown.reset(contents, cx);
                    });
                    view.markdown_parse_pending = view.markdown.read(cx).is_parsing();
                    view.sync_preview_to_source_index(selection_start, should_reveal_selection, cx);
                    cx.emit(SearchEvent::MatchesInvalidated);
                }
                view.pending_update_task = None;
                cx.notify();
            })
        })
    }

    fn selected_source_index(editor: &Editor, cx: &mut App) -> Option<usize> {
        let display_snapshot = editor.display_snapshot(cx);
        let source_offset = editor
            .selections
            .last::<MultiBufferOffset>(&display_snapshot)
            .range()
            .start;
        let buffer = editor.buffer().read(cx).as_singleton()?;
        let buffer_id = buffer.read(cx).remote_id();
        let (buffer_snapshot, buffer_offset) = display_snapshot
            .buffer_snapshot()
            .point_to_buffer_offset(source_offset)?;

        if buffer_snapshot.remote_id() == buffer_id {
            Some(buffer_offset.0)
        } else {
            None
        }
    }

    fn sync_preview_to_source_index(
        &mut self,
        source_index: usize,
        reveal: bool,
        cx: &mut Context<Self>,
    ) {
        self.active_source_index = Some(source_index);
        self.sync_active_root_block(cx);
        self.markdown.update(cx, |markdown, cx| {
            if reveal {
                markdown.request_autoscroll_to_source_index(source_index, cx);
            }
        });
    }

    fn sync_active_root_block(&mut self, cx: &mut Context<Self>) {
        self.markdown.update(cx, |markdown, cx| {
            markdown.set_active_root_for_source_index(self.active_source_index, cx);
        });
    }

    fn navigate_to_link_target(
        &mut self,
        target: Option<PreviewLinkTarget>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            None => {}
            Some(PreviewLinkTarget::Heading(slug)) => {
                self.markdown.update(cx, |markdown, cx| {
                    markdown.scroll_to_heading_when_parsed(slug, cx);
                });
            }
            Some(PreviewLinkTarget::Position { row, column }) => {
                let Some(editor) = self
                    .active_editor
                    .as_ref()
                    .map(|state| state.editor.clone())
                else {
                    return;
                };
                let Some(source_index) =
                    editor
                        .read(cx)
                        .buffer()
                        .read(cx)
                        .as_singleton()
                        .map(|buffer| {
                            let snapshot = buffer.read(cx).snapshot();
                            let point = snapshot.point_from_external_input(row, column);
                            snapshot.point_to_offset(point)
                        })
                else {
                    return;
                };
                Self::change_selection_to_source_index(&editor, source_index, false, window, cx);
                self.sync_preview_to_source_index(source_index, true, cx);
            }
        }
    }

    fn change_selection_to_source_index(
        editor: &Entity<Editor>,
        source_index: usize,
        move_focus: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        editor.update(cx, |editor, cx| {
            let selection = MultiBufferOffset(source_index)..MultiBufferOffset(source_index);
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::center()),
                window,
                cx,
                |selections| selections.select_ranges(vec![selection]),
            );
            if move_focus {
                window.focus(&editor.focus_handle(cx), cx);
            }
        });
    }

    /// The absolute path of the file that is currently being previewed.
    fn get_folder_for_active_editor(editor: &Editor, cx: &App) -> Option<PathBuf> {
        let file = editor.file_at(MultiBufferOffset(0), cx)?;
        let project_path = ProjectPath::from_file(file.as_ref(), cx);
        let absolute_path = editor
            .project()
            .and_then(|project| project.read(cx).absolute_path(&project_path, cx))
            .or_else(|| file.as_local().map(|file| file.abs_path(cx)))?;
        absolute_path.parent().map(Path::to_path_buf)
    }

    fn project_path_for_active_editor(editor: &Editor, cx: &App) -> Option<ProjectPath> {
        let file = editor.file_at(MultiBufferOffset(0), cx)?;
        Some(ProjectPath::from_file(file.as_ref(), cx))
    }

    fn line_scroll_amount(&self, cx: &App) -> Pixels {
        let settings = ThemeSettings::get_global(cx);
        settings.markdown_preview_font_size(cx) * settings.buffer_line_height.value()
    }

    fn increase_font_size(
        &mut self,
        action: &IncreaseBufferFontSize,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.adjust_font_size(action.persist, px(1.0), cx);
    }

    fn decrease_font_size(
        &mut self,
        action: &DecreaseBufferFontSize,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.adjust_font_size(action.persist, px(-1.0), cx);
    }

    fn adjust_font_size(&mut self, persist: bool, delta: Pixels, cx: &mut Context<Self>) {
        if persist {
            let Ok(fs) = self
                .workspace
                .read_with(cx, |workspace, _| workspace.app_state().fs.clone())
            else {
                return;
            };
            update_settings_file(fs, cx, move |settings, cx| {
                let size = ThemeSettings::get_global(cx).markdown_preview_font_size(cx) + delta;
                settings.markdown_preview.get_or_insert_default().font_size =
                    Some(f32::from(theme_settings::clamp_font_size(size)).into());
            });
        } else {
            theme_settings::adjust_markdown_preview_font_size(cx, |size| size + delta);
        }
    }

    fn reset_font_size(
        &mut self,
        action: &ResetBufferFontSize,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if action.persist {
            let Ok(fs) = self
                .workspace
                .read_with(cx, |workspace, _| workspace.app_state().fs.clone())
            else {
                return;
            };
            update_settings_file(fs, cx, move |settings, _| {
                reset_persisted_font_size(settings);
            });
        } else {
            theme_settings::reset_markdown_preview_font_size(cx);
        }
    }

    fn scroll_by_amount(&self, distance: Pixels) {
        let offset = self.scroll_handle.offset();
        self.scroll_handle
            .set_offset(point(offset.x, offset.y - distance));
    }

    fn scroll_page_up(&mut self, _: &ScrollPageUp, _window: &mut Window, cx: &mut Context<Self>) {
        let viewport_height = self.scroll_handle.bounds().size.height;
        if viewport_height.is_zero() {
            return;
        }

        self.scroll_by_amount(-viewport_height);
        cx.notify();
    }

    fn scroll_page_down(
        &mut self,
        _: &ScrollPageDown,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport_height = self.scroll_handle.bounds().size.height;
        if viewport_height.is_zero() {
            return;
        }

        self.scroll_by_amount(viewport_height);
        cx.notify();
    }

    fn scroll_up(&mut self, _: &ScrollUp, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(bounds) = self
            .scroll_handle
            .bounds_for_item(self.scroll_handle.top_item())
        {
            let item_height = bounds.size.height;
            // Scroll no more than the rough equivalent of a large headline
            let max_height = window.rem_size() * 2;
            let scroll_height = min(item_height, max_height);
            self.scroll_by_amount(-scroll_height);
        } else {
            let scroll_height = self.line_scroll_amount(cx);
            if !scroll_height.is_zero() {
                self.scroll_by_amount(-scroll_height);
            }
        }
        cx.notify();
    }

    fn scroll_down(&mut self, _: &ScrollDown, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(bounds) = self
            .scroll_handle
            .bounds_for_item(self.scroll_handle.top_item())
        {
            let item_height = bounds.size.height;
            // Scroll no more than the rough equivalent of a large headline
            let max_height = window.rem_size() * 2;
            let scroll_height = min(item_height, max_height);
            self.scroll_by_amount(scroll_height);
        } else {
            let scroll_height = self.line_scroll_amount(cx);
            if !scroll_height.is_zero() {
                self.scroll_by_amount(scroll_height);
            }
        }
        cx.notify();
    }

    fn scroll_up_by_item(
        &mut self,
        _: &ScrollUpByItem,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(bounds) = self
            .scroll_handle
            .bounds_for_item(self.scroll_handle.top_item())
        {
            self.scroll_by_amount(-bounds.size.height);
        }
        cx.notify();
    }

    fn scroll_down_by_item(
        &mut self,
        _: &ScrollDownByItem,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(bounds) = self
            .scroll_handle
            .bounds_for_item(self.scroll_handle.top_item())
        {
            self.scroll_by_amount(bounds.size.height);
        }
        cx.notify();
    }

    fn scroll_to_top(&mut self, _: &ScrollToTop, _window: &mut Window, cx: &mut Context<Self>) {
        self.scroll_handle.scroll_to_item(0);
        cx.notify();
    }

    fn scroll_to_bottom(
        &mut self,
        _: &ScrollToBottom,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.scroll_handle.scroll_to_bottom();
        cx.notify();
    }

    fn close_and_return_to_editor(
        &mut self,
        _: &CloseAndReturnToEditor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self
            .active_editor
            .as_ref()
            .map(|state| state.editor.clone())
        else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let preview_id = cx.entity_id();

        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                let Some(pane) = workspace.pane_for_item_id(preview_id) else {
                    return;
                };
                if !workspace.activate_item(&editor, true, true, window, cx) {
                    let editor_entity_id = editor.entity_id();
                    // Re-adding the source emits `ItemAdded`; suppress its automatic preview for the editor's lifetime.
                    cx.global_mut::<SuppressedAutoPreviews>()
                        .0
                        .insert(editor_entity_id);

                    App::observe_release(cx, &editor, move |_, cx| {
                        cx.global_mut::<SuppressedAutoPreviews>()
                            .0
                            .remove(&editor_entity_id);
                    })
                    .detach();

                    workspace.add_item(
                        pane.clone(),
                        Box::new(editor),
                        None,
                        true,
                        true,
                        window,
                        cx,
                    );
                }

                pane.update(cx, |pane, cx| {
                    pane.close_item_by_id(preview_id, SaveIntent::Skip, window, cx)
                })
                .detach_and_log_err(cx);
            });
        });
    }

    /// Returns the theme chosen in `markdown_preview.theme`, or `None` if the
    /// user hasn't set one or it can't be resolved.
    fn resolve_preview_theme(&self, cx: &App) -> Option<Arc<Theme>> {
        let theme_settings = ThemeSettings::get_global(cx);
        let theme_selection = theme_settings.markdown_preview_theme.as_ref()?;
        let theme_name = theme_selection.name(SystemAppearance::global(cx).0);
        ThemeRegistry::global(cx).get(&theme_name.0).ok()
    }

    fn render_markdown_element(
        &self,
        preview_theme: &Option<Arc<Theme>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> MarkdownElement {
        let active_editor = self
            .active_editor
            .as_ref()
            .map(|state| state.editor.clone());
        let source_project_path = active_editor
            .as_ref()
            .and_then(|editor| Self::project_path_for_active_editor(editor.read(cx), cx));

        let mut project = None;
        let mut workspace_directory = None;
        if let Some(workspace_entity) = self.workspace.upgrade() {
            let project_entity = workspace_entity.read(cx).project().clone();
            if let Some(tree) = project_entity.read(cx).worktrees(cx).next() {
                workspace_directory = Some(tree.read(cx).abs_path().to_path_buf());
            }
            project = Some(project_entity);
        }

        let markdown_style = if let Some(theme) = preview_theme {
            MarkdownStyle::themed_with_overrides(
                MarkdownFont::Preview,
                theme.colors(),
                theme.syntax(),
                window,
                cx,
            )
        } else {
            MarkdownStyle::themed(MarkdownFont::Preview, window, cx)
        };

        let mut markdown_element = MarkdownElement::new(self.markdown.clone(), markdown_style)
            .input_focus_handle(self.focus_handle.clone())
            .code_block_renderer(CodeBlockRenderer::Default {
                copy_button_visibility: CopyButtonVisibility::VisibleOnHover,
                wrap_button_visibility: markdown::WrapButtonVisibility::Hidden,
                border: false,
            })
            .scroll_handle(self.scroll_handle.clone())
            .show_root_block_markers()
            .image_resolver({
                let base_directory = self.base_directory.clone();
                move |dest_url, cx| {
                    resolve_preview_image(
                        dest_url,
                        base_directory.as_deref(),
                        workspace_directory.as_deref(),
                        project.as_ref(),
                        source_project_path.as_ref(),
                        PathStyle::local(),
                        cx,
                    )
                }
            })
            .on_url_click({
                let view_handle = cx.entity().downgrade();
                let workspace = self.workspace.clone();
                let base_directory = self.base_directory.clone();
                move |url, window, cx| {
                    handle_url_click(
                        url,
                        &view_handle,
                        base_directory.clone(),
                        &workspace,
                        window,
                        cx,
                    );
                }
            })
            .on_url_hover({
                let view_handle = cx.entity().downgrade();
                move |hovered_url, _window, cx| {
                    view_handle
                        .update(cx, |view, cx| {
                            if view.hovered_url != hovered_url {
                                view.hovered_url = hovered_url;
                                cx.notify();
                            }
                        })
                        .log_err();
                }
            });

        if let Some(active_editor) = active_editor {
            let editor_for_checkbox = active_editor.clone();
            let view_handle = cx.entity().downgrade();
            markdown_element = markdown_element
                .on_source_click(move |source_index, click_count, window, cx| {
                    if click_count == 1 {
                        Self::change_selection_to_source_index(
                            &active_editor,
                            source_index,
                            false,
                            window,
                            cx,
                        );
                    }
                    false
                })
                .on_checkbox_toggle(move |source_range, new_checked, window, cx| {
                    Self::apply_checkbox_toggle_to_editor(
                        &editor_for_checkbox,
                        source_range,
                        new_checked,
                        cx,
                    );
                    Self::refresh_preview(view_handle.clone(), window, cx);
                });
        }

        markdown_element
    }

    fn apply_checkbox_toggle_to_editor(
        editor: &Entity<Editor>,
        source_range: std::ops::Range<usize>,
        new_checked: bool,
        cx: &mut App,
    ) {
        let task_marker = if new_checked { "[x]" } else { "[ ]" };
        let expected_existing_marker = if new_checked { "[ ]" } else { "[x]" };

        editor.update(cx, |editor, cx| {
            let existing_marker: String = editor
                .buffer()
                .read(cx)
                .snapshot(cx)
                .text_for_range(
                    MultiBufferOffset(source_range.start)..MultiBufferOffset(source_range.end),
                )
                .collect();

            debug_assert_eq!(existing_marker, expected_existing_marker);

            editor.edit(
                [(
                    MultiBufferOffset(source_range.start)..MultiBufferOffset(source_range.end),
                    task_marker,
                )],
                cx,
            );
        });
    }

    fn refresh_preview(view_handle: WeakEntity<Self>, window: &mut Window, cx: &mut App) {
        if let Some(view) = view_handle.upgrade() {
            let preview_is_focused = view.read(cx).focus_handle.contains_focused(window, cx);
            if !preview_is_focused {
                return;
            }

            cx.update_entity(&view, |this, cx| {
                this.update_markdown_from_active_editor(false, false, window, cx);
            });
        }
    }
}

fn handle_url_click(
    url: SharedString,
    view: &WeakEntity<MarkdownPreviewView>,
    base_directory: Option<PathBuf>,
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let (path_part, fragment) = split_local_url_fragment(url.as_ref());

    if path_part.is_empty() {
        if let Some(fragment) = fragment {
            let view = view.clone();
            let slug = SharedString::from(fragment.to_string());
            window.defer(cx, move |window, cx| {
                if let Some(view) = view.upgrade() {
                    let markdown = view.read(cx).markdown.clone();
                    let active_editor = view
                        .read(cx)
                        .active_editor
                        .as_ref()
                        .map(|state| state.editor.clone());

                    let source_index =
                        markdown.update(cx, |markdown, cx| markdown.scroll_to_heading(&slug, cx));

                    if let Some(source_index) = source_index {
                        if let Some(editor) = active_editor {
                            MarkdownPreviewView::change_selection_to_source_index(
                                &editor,
                                source_index,
                                true,
                                window,
                                cx,
                            );
                        }
                    }
                }
            });
        }
    } else {
        open_preview_url(
            SharedString::from(path_part.to_string()),
            fragment.map(|fragment| SharedString::from(fragment.to_string())),
            base_directory,
            Some(view.clone()),
            workspace,
            window,
            cx,
        );
    }
}

fn open_preview_url(
    url: SharedString,
    fragment: Option<SharedString>,
    base_directory: Option<PathBuf>,
    source_view: Option<WeakEntity<MarkdownPreviewView>>,
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let decoded_path = urlencoding::decode(&url).unwrap_or_else(|_| Cow::Borrowed(&url));

    // Only `://` URLs skip local handling (a scheme check would misclassify `main.rs:2`); ambiguous links like `tel:123` are tried as files first, then fall back to the URL handler.
    if !decoded_path.contains("://") {
        let parsed_path = PathWithPosition::parse_str(&decoded_path);
        let fragment_position = fragment
            .as_deref()
            .and_then(|fragment| fragment.strip_prefix('L'))
            .and_then(source_position_from_fragment);

        if !window.modifiers().alt
            && workspace
                .read_with(cx, |workspace, cx| {
                    MarkdownPreviewView::is_markdown_path(
                        &parsed_path.path,
                        workspace.project().read(cx).languages(),
                    )
                })
                .is_ok_and(|is_markdown| is_markdown)
        {
            let link_target = if let Some(row) = parsed_path.row {
                Some(PreviewLinkTarget::Position {
                    row: row.saturating_sub(1),
                    column: parsed_path.column.unwrap_or(1).saturating_sub(1),
                })
            } else if let Some((row, column)) = fragment_position {
                Some(PreviewLinkTarget::Position { row, column })
            } else {
                fragment.map(PreviewLinkTarget::Heading)
            };
            open_markdown_link_in_preview(
                parsed_path.path.to_string_lossy().into_owned(),
                link_target,
                decoded_path.into_owned(),
                base_directory,
                source_view,
                workspace,
                window,
                cx,
            );
            return;
        }

        let path_with_position = if parsed_path.row.is_some() {
            Some(decoded_path.to_string())
        } else {
            fragment_position.map(|(row, column)| {
                if column == 0 {
                    format!("{decoded_path}:{}", row + 1)
                } else {
                    format!("{decoded_path}:{}:{}", row + 1, column + 1)
                }
            })
        };

        if let Some(path_with_position) = path_with_position {
            open_preview_path_with_position(
                path_with_position,
                decoded_path.into_owned(),
                base_directory,
                workspace,
                window,
                cx,
            );
            return;
        }
    }

    if let Some(workspace) = workspace.upgrade() {
        workspace.update(cx, |workspace, cx| {
            workspace.open_url_or_file(&decoded_path, base_directory.as_deref(), window, cx);
        });
    } else {
        cx.open_url(url.as_ref());
    }
}

fn find_preview_for_buffer(
    workspace: &Workspace,
    buffer: &Entity<Buffer>,
    cx: &App,
) -> Option<Entity<MarkdownPreviewView>> {
    workspace
        .items_of_type::<MarkdownPreviewView>(cx)
        .find(|view| {
            view.read(cx).mode == MarkdownPreviewMode::Default
                && view.read(cx).is_previewing(buffer, cx)
        })
}

fn open_markdown_link_in_preview(
    path: String,
    link_target: Option<PreviewLinkTarget>,
    fallback_url: String,
    base_directory: Option<PathBuf>,
    source_view: Option<WeakEntity<MarkdownPreviewView>>,
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let open_target = resolve_open_target(
        workspace,
        PathMatching::Exact,
        &path,
        base_directory.as_deref(),
        cx,
    );
    let workspace = workspace.clone();
    let workspace_for_error = workspace.clone();
    let task = window.spawn(cx, async move |cx| {
        let target_path = open_target
            .await
            .filter(|open_target| open_target.is_file())
            .map(|open_target| open_target.path().path.clone());
        let Some(abs_path) = target_path else {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.open_url_or_file(&fallback_url, base_directory.as_deref(), window, cx);
            })?;
            return anyhow::Ok(());
        };

        let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
        let (worktree, relative_path) = project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(abs_path, false, cx)
            })
            .await
            .context("resolving Markdown preview link worktree")?;
        let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
        let buffer = project
            .update(cx, |project, cx| {
                project.open_buffer(
                    ProjectPath {
                        worktree_id,
                        path: relative_path,
                    },
                    cx,
                )
            })
            .await
            .context("opening Markdown preview link buffer")?;

        workspace.update_in(cx, |workspace, window, cx| {
            if let Some(source_view) = source_view.as_ref().and_then(|view| view.upgrade())
                && source_view.read(cx).is_previewing(&buffer, cx)
            {
                source_view.update(cx, |view, cx| {
                    view.navigate_to_link_target(link_target, window, cx);
                });
                return;
            }

            if let Some(existing_view) = find_preview_for_buffer(workspace, &buffer, cx) {
                workspace.activate_item(&existing_view, true, true, window, cx);
                existing_view.update(cx, |view, cx| {
                    view.navigate_to_link_target(link_target, window, cx);
                });
                return;
            }

            let editor = cx.new(|cx| {
                Editor::for_buffer(buffer, Some(workspace.project().clone()), window, cx)
            });
            let preview = MarkdownPreviewView::create_markdown_view(workspace, editor, window, cx);
            let pane = source_view
                .as_ref()
                .and_then(|view| view.upgrade())
                .and_then(|view| workspace.pane_for(&view))
                .unwrap_or_else(|| workspace.active_pane().clone());
            pane.update(cx, |pane, cx| {
                pane.add_item(Box::new(preview.clone()), true, true, None, window, cx);
            });
            preview.update(cx, |view, cx| {
                view.navigate_to_link_target(link_target, window, cx);
            });
        })?;
        anyhow::Ok(())
    });
    task.detach_and_notify_err(workspace_for_error, window, cx);
}

fn open_preview_path_with_position(
    path_with_position: String,
    fallback_path: String,
    base_directory: Option<PathBuf>,
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let open_target = resolve_open_target(
        workspace,
        PathMatching::Exact,
        &path_with_position,
        base_directory.as_deref(),
        cx,
    );
    let workspace = workspace.clone();
    let workspace_for_error = workspace.clone();
    let task = window.spawn(cx, async move |cx| {
        let opened = if let Some(open_target) = open_target.await {
            open_resolved_target(&workspace, &open_target, cx)
                .await
                .context("opening Markdown preview link")?
        } else {
            false
        };

        if !opened {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.open_url_or_file(&fallback_path, base_directory.as_deref(), window, cx);
            })?;
        }

        anyhow::Ok(())
    });
    task.detach_and_notify_err(workspace_for_error, window, cx);
}

fn resolve_preview_image(
    dest_url: &str,
    base_directory: Option<&Path>,
    workspace_directory: Option<&Path>,
    project: Option<&Entity<Project>>,
    source_project_path: Option<&ProjectPath>,
    client_path_style: PathStyle,
    cx: &App,
) -> Option<ImageSource> {
    if dest_url.starts_with("data:") {
        return None;
    }

    if dest_url.starts_with("http://") || dest_url.starts_with("https://") {
        return Some(ImageSource::Resource(Resource::Uri(SharedUri::from(
            dest_url.to_string(),
        ))));
    }

    let decoded = urlencoding::decode(dest_url)
        .map(|decoded| decoded.into_owned())
        .unwrap_or_else(|_| dest_url.to_string());
    let workspace_relative_path = ['/', '\\']
        .iter()
        .find_map(|prefix| decoded.strip_prefix(*prefix));

    if let (Some(project), Some(source_project_path)) = (project, source_project_path) {
        let project_path = resolve_project_path_for_preview_image(
            &decoded,
            workspace_relative_path,
            source_project_path,
            project,
            cx,
        );
        if let Some(project_path) = project_path {
            let is_remote = project
                .read(cx)
                .worktree_for_id(project_path.worktree_id, cx)
                .is_some_and(|worktree| !worktree.read(cx).is_local());
            if is_remote {
                return Some(image_store::project_image_source(
                    project.downgrade(),
                    project_path,
                ));
            }

            let path = project.read(cx).absolute_path(&project_path, cx)?;
            return path
                .exists()
                .then(|| ImageSource::Resource(Resource::Path(Arc::from(path.as_path()))));
        } else if project.read(cx).is_remote() {
            return None;
        }
    }

    let path = if let (Some(stripped), Some(root)) = (workspace_relative_path, workspace_directory)
    {
        client_path_style.join_path(root, stripped).ok()?
    } else if client_path_style.has_root(Path::new(&decoded)) {
        PathBuf::from(decoded)
    } else {
        client_path_style.join_path(base_directory?, decoded).ok()?
    };

    path.exists()
        .then(|| ImageSource::Resource(Resource::Path(Arc::from(path.as_path()))))
}

fn resolve_project_path_for_preview_image(
    decoded_path: &str,
    workspace_relative_path: Option<&str>,
    source_project_path: &ProjectPath,
    project: &Entity<Project>,
    cx: &App,
) -> Option<ProjectPath> {
    let worktree = project
        .read(cx)
        .worktree_for_id(source_project_path.worktree_id, cx)?;
    let path_style = worktree.read(cx).path_style();

    if workspace_relative_path.is_none() && path_style.is_absolute(Path::new(decoded_path)) {
        return project
            .read(cx)
            .project_path_for_absolute_path(Path::new(decoded_path), cx);
    }

    let source_directory = source_project_path.path.parent()?;
    let path = if let Some(workspace_relative_path) = workspace_relative_path {
        RelPath::new(Path::new(workspace_relative_path), path_style)
            .ok()?
            .into_owned()
    } else {
        let joined_path = path_style
            .join_path(source_directory.as_std_path(), decoded_path)
            .ok()?;
        RelPath::new(&joined_path, path_style).ok()?.into_owned()
    };

    Some(ProjectPath {
        worktree_id: source_project_path.worktree_id,
        path: path.into(),
    })
}

impl Focusable for MarkdownPreviewView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<MarkdownPreviewEvent> for MarkdownPreviewView {}
impl EventEmitter<SearchEvent> for MarkdownPreviewView {}

impl Item for MarkdownPreviewView {
    type Event = MarkdownPreviewEvent;

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            self.active_editor
                .as_ref()
                .map(|state| state.editor.clone().into())
        } else {
            None
        }
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::FileDoc))
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let preview = self.self_handle.clone();
        let editor = self
            .active_editor
            .as_ref()
            .map(|editor_state| editor_state.editor.read(cx));
        let max_title_len = params.max_title_len.unwrap_or(MAX_TAB_TITLE_LEN);

        // Like editor tabs, the title is only the file name, since `tab_content_text` can be a
        // full path for files outside the project. The directories needed to tell apart files
        // with the same name are shown in the description.
        let title = match editor {
            Some(editor) if params.truncate_title_middle => format!("Preview {}", editor.title(cx)),
            Some(editor) => format!(
                "Preview {}",
                util::truncate_and_trailoff(&editor.title(cx), max_title_len)
            ),
            None => "Markdown Preview".to_string(),
        };
        let description = editor
            .zip(params.detail)
            .and_then(|(editor, detail)| editor.tab_description(detail, cx))
            .map(|description| util::truncate_and_trailoff(&description, max_title_len));

        h_flex()
            .gap_1()
            .when(!params.selected, |tab| tab.track_focus(&self.focus_handle))
            .when(params.truncate_title_middle, |this| {
                this.w_full().min_w_0().overflow_hidden()
            })
            .on_action(move |action: &CloseAndReturnToEditor, window, cx| {
                preview
                    .update(cx, |preview, cx| {
                        preview.close_and_return_to_editor(action, window, cx);
                    })
                    .ok();
            })
            .child(
                Label::new(title)
                    .single_line()
                    .color(params.text_color())
                    .when(params.truncate_title_middle, |this| {
                        this.truncate_middle().flex_1()
                    }),
            )
            .when_some(description, |this, description| {
                this.child(
                    Label::new(description)
                        .single_line()
                        .size(LabelSize::XSmall)
                        .when(params.truncate_title_middle, |this| {
                            this.truncate_start().flex_shrink()
                        })
                        .color(Color::Muted),
                )
            })
            .into_any_element()
    }

    fn tab_content_text(&self, detail: usize, cx: &App) -> SharedString {
        self.active_editor
            .as_ref()
            .map(|editor_state| {
                let title = editor_state.editor.read(cx).tab_content_text(detail, cx);
                format!("Preview {title}").into()
            })
            .unwrap_or_else(|| SharedString::from("Markdown Preview"))
    }

    // Save As writes the source file, so suggest its name rather than the tab text, which can be a
    // full path for files outside the project.
    fn suggested_filename(&self, cx: &App) -> SharedString {
        self.active_editor
            .as_ref()
            .map(|editor_state| editor_state.editor.read(cx).suggested_filename(cx))
            .unwrap_or_else(|| SharedString::from("Markdown Preview"))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        self.active_editor
            .as_ref()?
            .editor
            .read(cx)
            .tab_tooltip_text(cx)
    }

    fn tab_extra_context_menu_actions(
        &self,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Vec<(SharedString, Box<dyn Action>)> {
        vec![("Show Source".into(), Box::new(CloseAndReturnToEditor))]
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mode != MarkdownPreviewMode::Default {
            return;
        }
        if let Some(editor) = self.find_canonical_editor(workspace, cx)
            && self
                .active_editor
                .as_ref()
                .is_none_or(|s| s.editor != editor)
        {
            self.set_editor(editor, window, cx);
        }
    }

    fn can_save(&self, cx: &App) -> bool {
        self.active_editor
            .as_ref()
            .is_some_and(|editor_state| editor_state.editor.read(cx).can_save(cx))
    }

    fn can_save_as(&self, cx: &App) -> bool {
        self.active_editor
            .as_ref()
            .is_some_and(|editor_state| editor_state.editor.read(cx).can_save_as(cx))
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.active_editor
            .as_ref()
            .map(|editor_state| {
                editor_state
                    .editor
                    .update(cx, |editor, cx| editor.save(options, project, window, cx))
            })
            .unwrap_or_else(|| Task::ready(Ok(())))
    }

    fn save_as(
        &mut self,
        project: Entity<Project>,
        path: project::ProjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.active_editor
            .as_ref()
            .map(|editor_state| {
                editor_state
                    .editor
                    .update(cx, |editor, cx| editor.save_as(project, path, window, cx))
            })
            .unwrap_or_else(|| Task::ready(Ok(())))
    }

    fn reload(
        &mut self,
        _project: Entity<Project>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        // The preview is not the owner of the source editor's buffer, so force-closing it should not discard editor changes.
        Task::ready(Ok(()))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(workspace::item::ItemEvent)) {
        match event {
            MarkdownPreviewEvent::SourceEditorChanged
            | MarkdownPreviewEvent::SourceFileHandleChanged => {
                f(workspace::item::ItemEvent::UpdateTab);
                f(workspace::item::ItemEvent::UpdateBreadcrumbs);
            }
        }
    }

    fn buffer_kind(&self, _cx: &App) -> ItemBufferKind {
        ItemBufferKind::Singleton
    }

    fn as_searchable(
        &self,
        handle: &Entity<Self>,
        _: &App,
    ) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(handle.clone()))
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        let Some(active_editor) = self.active_editor.as_ref() else {
            return Task::ready(None);
        };

        let Some(project) = active_editor.editor.read(cx).project() else {
            return Task::ready(None);
        };

        let language_registry = project.read(cx).languages().clone();

        Task::ready(Some(MarkdownPreviewView::new(
            MarkdownPreviewMode::Default,
            active_editor.editor.clone(),
            self.workspace.clone(),
            language_registry,
            window,
            cx,
        )))
    }

    fn set_nav_history(
        &mut self,
        history: ItemNavHistory,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) {
        self.nav_history = Some(history);
    }

    fn deactivated(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(nav_history) = self.nav_history.as_mut() {
            nav_history.push::<()>(None, None, cx);
        }
    }
}

impl Render for MarkdownPreviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let preview_theme = self.resolve_preview_theme(cx);
        let bg_color = preview_theme
            .as_ref()
            .map(|theme| theme.colors().editor_background)
            .unwrap_or_else(|| cx.theme().colors().editor_background);
        let preview_font_size = ThemeSettings::get_global(cx).markdown_preview_font_size(cx);
        let hovered_url = self.hovered_url.clone();
        div()
            .image_cache(self.image_cache.clone())
            .id("MarkdownPreview")
            .key_context("MarkdownPreview")
            .track_focus(&self.focus_handle(cx))
            .on_hover(cx.listener(|view, hovered, _window, cx| {
                if !hovered && view.hovered_url.take().is_some() {
                    cx.notify();
                }
            }))
            .on_action(cx.listener(MarkdownPreviewView::scroll_page_up))
            .on_action(cx.listener(MarkdownPreviewView::scroll_page_down))
            .on_action(cx.listener(MarkdownPreviewView::scroll_up))
            .on_action(cx.listener(MarkdownPreviewView::scroll_down))
            .on_action(cx.listener(MarkdownPreviewView::scroll_up_by_item))
            .on_action(cx.listener(MarkdownPreviewView::scroll_down_by_item))
            .on_action(cx.listener(MarkdownPreviewView::scroll_to_top))
            .on_action(cx.listener(MarkdownPreviewView::scroll_to_bottom))
            .on_action(cx.listener(MarkdownPreviewView::close_and_return_to_editor))
            .on_action(cx.listener(MarkdownPreviewView::increase_font_size))
            .on_action(cx.listener(MarkdownPreviewView::decrease_font_size))
            .on_action(cx.listener(MarkdownPreviewView::reset_font_size))
            .w_full()
            .flex_1()
            .min_h_0()
            .relative()
            .bg(bg_color)
            .child(
                WithRemSize::new(preview_font_size).size_full().child(
                    div()
                        .id("markdown-preview-scroll-container")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll_handle)
                        .restrict_scroll_to_axis()
                        .p_4()
                        .child({
                            let markdown_element =
                                self.render_markdown_element(&preview_theme, window, cx);
                            let markdown = self.markdown.clone();
                            let max_width = MarkdownPreviewSettings::get_global(cx).max_width;
                            let content = right_click_menu("markdown-preview-context-menu")
                                .trigger(move |_, _, _| markdown_element)
                                .maybe_menu(move |window, cx| {
                                    let focus = window.focused(cx);
                                    let markdown = markdown.read(cx);
                                    let context_menu_link = markdown.context_menu_link().cloned();
                                    let selected_text =
                                        markdown.context_menu_selected_text().cloned();
                                    let selected_markdown =
                                        markdown.context_menu_selected_markdown().cloned();
                                    if context_menu_link.is_none()
                                        && selected_text.is_none()
                                        && selected_markdown.is_none()
                                    {
                                        return None;
                                    }
                                    Some(ContextMenu::build(window, cx, move |menu, _, _cx| {
                                        menu.when_some(focus, |menu, focus| menu.context(focus))
                                            .when_some(selected_text, |menu, text| {
                                                menu.entry(
                                                    "Copy",
                                                    Some(Box::new(markdown::Copy)),
                                                    move |_, cx| {
                                                        cx.write_to_clipboard(
                                                            ClipboardItem::new_string(
                                                                text.to_string(),
                                                            ),
                                                        );
                                                    },
                                                )
                                            })
                                            .when_some(selected_markdown, |menu, text| {
                                                menu.entry(
                                                    "Copy as Markdown",
                                                    Some(Box::new(markdown::CopyAsMarkdown)),
                                                    move |_, cx| {
                                                        cx.write_to_clipboard(
                                                            ClipboardItem::new_string(
                                                                text.to_string(),
                                                            ),
                                                        );
                                                    },
                                                )
                                            })
                                            .when_some(context_menu_link, |menu, url| {
                                                menu.entry("Copy Link", None, move |_, cx| {
                                                    cx.write_to_clipboard(
                                                        ClipboardItem::new_string(url.to_string()),
                                                    );
                                                })
                                            })
                                    }))
                                });
                            div()
                                .w_full()
                                .when_some(max_width, |this, max_width| {
                                    this.max_w(max_width).mx_auto()
                                })
                                .child(content)
                        }),
                ),
            )
            .custom_scrollbars(
                Scrollbars::for_settings::<EditorSettingsScrollbarProxy>()
                    .show_along(ScrollAxes::Vertical)
                    .tracked_scroll_handle(&self.scroll_handle),
                window,
                cx,
            )
            .when_some(hovered_url, |this, hovered_url| {
                this.child(
                    div()
                        .absolute()
                        .bottom_2()
                        .left_0()
                        .max_w_full()
                        .overflow_hidden()
                        .child(LinkPreview::new(hovered_url.as_ref(), cx)),
                )
            })
    }
}

impl SearchableItem for MarkdownPreviewView {
    type Match = Range<usize>;

    fn supported_options(&self) -> SearchOptions {
        SearchOptions {
            case: true,
            word: true,
            regex: true,
            replacement: false,
            selection: false,
            select_all: false,
            find_in_results: false,
        }
    }

    fn get_matches(&self, _window: &mut Window, cx: &mut App) -> (Vec<Self::Match>, SearchToken) {
        (
            self.markdown.read(cx).search_highlights().to_vec(),
            SearchToken::default(),
        )
    }

    fn clear_matches(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let had_highlights = !self.markdown.read(cx).search_highlights().is_empty();
        self.markdown.update(cx, |markdown, cx| {
            markdown.clear_search_highlights(cx);
        });
        if had_highlights {
            cx.emit(SearchEvent::MatchesInvalidated);
        }
    }

    fn update_matches(
        &mut self,
        matches: &[Self::Match],
        active_match_index: Option<usize>,
        _token: SearchToken,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        debug_assert!(
            matches
                .windows(2)
                .all(|ranges| (ranges[0].start, ranges[0].end) <= (ranges[1].start, ranges[1].end))
        );
        let old_highlights = self.markdown.read(cx).search_highlights();
        let changed = old_highlights != matches;
        self.markdown.update(cx, |markdown, cx| {
            markdown.set_search_highlights(matches.to_vec(), active_match_index, cx);
        });
        if changed {
            cx.emit(SearchEvent::MatchesInvalidated);
        }
    }

    fn query_suggestion(
        &mut self,
        _seed_query_override: Option<SeedQuerySetting>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> String {
        self.markdown
            .read(cx)
            .selected_source()
            .unwrap_or_default()
            .to_string()
    }

    fn activate_match(
        &mut self,
        index: usize,
        matches: &[Self::Match],
        _token: SearchToken,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(match_range) = matches.get(index) {
            let start = match_range.start;
            self.markdown.update(cx, |markdown, cx| {
                markdown.set_active_search_highlight(Some(index), cx);
                markdown.request_autoscroll_to_source_index(start, cx);
            });
        }
    }

    fn select_matches(
        &mut self,
        _matches: &[Self::Match],
        _token: SearchToken,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn replace(
        &mut self,
        _: &Self::Match,
        _: &SearchQuery,
        _token: SearchToken,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn find_matches(
        &mut self,
        query: Arc<SearchQuery>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Vec<Self::Match>> {
        let markdown = self.markdown.read(cx);
        let source = markdown.source().to_string();
        let non_rendered_ranges = markdown.non_rendered_source_ranges();
        cx.background_spawn(async move {
            filter_non_rendered_matches(query.search_str(&source), &non_rendered_ranges)
        })
    }

    fn active_match_index(
        &mut self,
        direction: Direction,
        matches: &[Self::Match],
        _token: SearchToken,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        if matches.is_empty() {
            return None;
        }

        let markdown = self.markdown.read(cx);
        let current_source_index = markdown
            .active_search_highlight()
            .and_then(|i| markdown.search_highlights().get(i))
            .map(|m| m.start)
            .or(self.active_source_index)
            .unwrap_or(0);

        match direction {
            Direction::Next => matches
                .iter()
                .position(|m| m.start >= current_source_index)
                .or(Some(0)),
            Direction::Prev => matches
                .iter()
                .rposition(|m| m.start <= current_source_index)
                .or(Some(matches.len().saturating_sub(1))),
        }
    }
}

/// `non_rendered_ranges` must be sorted by start and disjoint, as returned by
/// [`Markdown::non_rendered_source_ranges`]. Matches may arrive in any order.
fn filter_non_rendered_matches(
    matches: Vec<Range<usize>>,
    non_rendered_ranges: &[Range<usize>],
) -> Vec<Range<usize>> {
    matches
        .into_iter()
        .filter(|match_range| {
            let candidate =
                non_rendered_ranges.partition_point(|range| range.end <= match_range.start);

            !non_rendered_ranges
                .get(candidate)
                .is_some_and(|range| range.start < match_range.end)
        })
        .collect()
}

impl SerializableItem for MarkdownPreviewView {
    fn serialized_item_kind() -> &'static str {
        "MarkdownPreviewView"
    }

    fn deserialize(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let db = persistence::MarkdownPreviewDb::global(cx);
        window.spawn(cx, async move |cx| {
            let (abs_path, mode_value) = db
                .get_preview(item_id, workspace_id)?
                .context("No markdown preview entry found")?;
            let mode = MarkdownPreviewMode::from_db(mode_value);

            let (worktree, relative_path) = project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(abs_path.clone(), false, cx)
                })
                .await
                .context("Path not found")?;
            let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());

            let project_path = ProjectPath {
                worktree_id,
                path: relative_path,
            };

            let buffer = project
                .update(cx, |project, cx| project.open_buffer(project_path, cx))
                .await?;

            cx.update(|window, cx| {
                let language_registry = project.read(cx).languages().clone();
                let editor =
                    cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
                MarkdownPreviewView::new(mode, editor, workspace, language_registry, window, cx)
            })
        })
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let db = persistence::MarkdownPreviewDb::global(cx);
        delete_unloaded_items(alive_items, workspace_id, "markdown_previews", &db, cx)
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let editor = self.active_editor.as_ref()?.editor.clone();
        let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
        let file = buffer.read(cx).file()?;
        let worktree_id = file.worktree_id(cx);
        let abs_path = workspace
            .project()
            .read(cx)
            .worktree_for_id(worktree_id, cx)?
            .read(cx)
            .absolutize(file.path());
        let mode = self.mode.to_db();
        let db = persistence::MarkdownPreviewDb::global(cx);
        Some(cx.background_spawn(async move {
            db.save_preview(item_id, workspace_id, abs_path, mode).await
        }))
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        matches!(
            event,
            MarkdownPreviewEvent::SourceEditorChanged
                | MarkdownPreviewEvent::SourceFileHandleChanged
        )
    }
}

mod persistence {
    use std::path::PathBuf;

    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use workspace::{ItemId, WorkspaceDb, WorkspaceId};

    pub struct MarkdownPreviewDb(ThreadSafeConnection);

    impl Domain for MarkdownPreviewDb {
        const NAME: &str = stringify!(MarkdownPreviewDb);

        const MIGRATIONS: &[&str] = &[sql!(
            CREATE TABLE markdown_previews (
                workspace_id INTEGER,
                item_id INTEGER,
                abs_path BLOB,
                mode INTEGER NOT NULL DEFAULT 0,

                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;
        )];
    }

    db::static_connection!(MarkdownPreviewDb, [WorkspaceDb]);

    impl MarkdownPreviewDb {
        query! {
            pub async fn save_preview(
                item_id: ItemId,
                workspace_id: WorkspaceId,
                abs_path: PathBuf,
                mode: i64
            ) -> Result<()> {
                INSERT OR REPLACE INTO markdown_previews(item_id, workspace_id, abs_path, mode)
                VALUES (?, ?, ?, ?)
            }
        }

        query! {
            pub fn get_preview(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<(PathBuf, i64)>> {
                SELECT abs_path, mode
                FROM markdown_previews
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}

