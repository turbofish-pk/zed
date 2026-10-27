use crate::{
    ActiveDebugLine, Anchor, Autoscroll, BufferSerialization, Capability, Editor, EditorEvent,
    EditorSettings, FormatTarget, MultiBuffer, MultiBufferSnapshot, NavigationData,
    SelectionEffects, ToPoint as _,
    display_map::HighlightKey,
    editor_settings::SeedQuerySetting,
    persistence::{EditorDb, SerializedEditor},
    scroll::{ScrollAnchor, ScrollOffset},
};
use anyhow::{Context as _, Result, anyhow};
use collections::{HashMap, HashSet};
use file_icons::FileIcons;
use fs::MTime;
use futures::channel::oneshot;
use git::status::GitSummary;
use gpui::{
    AnyElement, App, AsyncWindowContext, Context, Entity, EntityId, EventEmitter, Font,
    IntoElement, ParentElement, Pixels, SharedString, Styled, Task, WeakEntity, Window,
};
use language::{
    Bias, Buffer, BufferRow, CharKind, CharScopeContext, HighlightedText, LocalFile, PLAIN_TEXT,
    Point,
    language_settings::{FormatOnSave, LanguageSettings},
};
use lsp::DiagnosticSeverity;
use multi_buffer::{BufferOffset, MultiBufferOffset, MultiBufferRow};
use project::{
    File, Project, ProjectItem as _, ProjectPath,
    git_store::GitStore,
    lsp_store::{FormatTrigger, LanguageServerShowDocumentRequest},
    project_settings::ProjectSettings,
    search::SearchQuery,
};
use rope::TextSummary;
use settings::Settings;
use std::{
    any::{Any, TypeId},
    borrow::Cow,
    cmp::{self, Ordering},
    num::NonZeroU32,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use text::{BufferSnapshot, OffsetRangeExt, ToPoint as _};
use ui::{IconDecorationKind, prelude::*};
use util::{
    ResultExt, TryFutureExt, debug_panic,
    paths::{PathExt, UrlExt as _},
};
use workspace::item::{ItemSettings, SerializableItem, TabContentParams};
use workspace::{
    ItemId, ItemNavHistory, OpenOptions, OpenVisible, ToolbarItemLocation, Workspace, WorkspaceId,
    invalid_item_view::InvalidItemView,
    item::{Item, ItemBufferKind, ItemEvent, ProjectItem, SaveOptions},
    searchable::{
        Direction, FilteredSearchRange, SearchEvent, SearchToken, SearchableItem,
        SearchableItemHandle, SelectSearchOptions,
    },
};
use workspace::{
    Pane, TabBarSettings, WorkspaceSettings, item::ProjectItemKind, searchable::SearchOptions,
};
use zed_actions::preview::{
    markdown::OpenPreview as OpenMarkdownPreview, svg::OpenPreview as OpenSvgPreview,
};

pub const MAX_TAB_TITLE_LEN: usize = 24;

impl Item for Editor {
    type Event = EditorEvent;

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if TypeId::of::<Self>() == type_id {
            Some(self_handle.clone().into())
        } else if TypeId::of::<MultiBuffer>() == type_id {
            Some(self_handle.read(cx).buffer.clone().into())
        } else {
            None
        }
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(data) = data.downcast_ref::<NavigationData>() {
            let newest_selection = self.selections.newest::<Point>(&self.display_snapshot(cx));
            let buffer = self.buffer.read(cx).read(cx);
            let offset = if buffer.can_resolve(&data.cursor_anchor) {
                data.cursor_anchor.to_point(&buffer)
            } else {
                buffer.clip_point(data.cursor_position, Bias::Left)
            };
            let destination_anchor = buffer.anchor_before(offset);

            let mut scroll_anchor = data.scroll_anchor;
            if !buffer.can_resolve(&scroll_anchor.anchor) {
                scroll_anchor.anchor = buffer.anchor_before(
                    buffer.clip_point(Point::new(data.scroll_top_row, 0), Bias::Left),
                );
            }

            drop(buffer);

            let was_folded = |cx: &mut Context<Self>| {
                self.display_snapshot(cx)
                    .folds_in_range(offset..offset)
                    .next()
                    .is_some()
            };
            let buffer_was_folded = |cx: &Context<Self>| {
                destination_anchor
                    .buffer_id()
                    .is_some_and(|buffer_id| self.is_buffer_folded(buffer_id, cx))
            };
            if newest_selection.head() == offset && !was_folded(cx) && !buffer_was_folded(cx) {
                false
            } else {
                self.set_scroll_anchor(scroll_anchor, window, cx);
                self.change_selections(
                    SelectionEffects::default().nav_history(false).unfold(),
                    window,
                    cx,
                    |s| s.select_ranges([offset..offset]),
                );
                true
            }
        } else {
            false
        }
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let multi_buffer = self.buffer().read(cx);
        if let Some(file) = multi_buffer
            .as_singleton()
            .and_then(|buffer| buffer.read(cx).file())
            .and_then(|file| File::from_dyn(Some(file)))
        {
            Some(
                file.worktree
                    .read(cx)
                    .absolutize(&file.path)
                    .compact()
                    .to_string_lossy()
                    .into_owned()
                    .into(),
            )
        } else {
            let title = multi_buffer.title(cx);
            (!title.is_empty()).then(|| title.to_string().into())
        }
    }

    fn tab_content_text(&self, detail: usize, cx: &App) -> SharedString {
        if let Some(path) = path_for_buffer(&self.buffer, detail, true, cx) {
            path.to_string().into()
        } else {
            // Use the same logic as the displayed title for consistency
            self.buffer.read(cx).title(cx).to_string().into()
        }
    }

    fn suggested_filename(&self, cx: &App) -> SharedString {
        let multi_buffer = self.buffer.read(cx);
        let title = multi_buffer.title(cx);
        if let Some(buffer) = multi_buffer.as_singleton() {
            let buffer = buffer.read(cx);
            if buffer.file().is_none()
                && let Some(language) = buffer.language()
                && *language != *PLAIN_TEXT
                && let Some(suffix) = language.path_suffixes().first()
                && !suffix.is_empty()
                && !title.ends_with(&format!(".{suffix}"))
            {
                return format!("{title}.{suffix}").into();
            }
        }

        title.to_string().into()
    }

    fn tab_icon(&self, _: &Window, cx: &App) -> Option<Icon> {
        ItemSettings::get_global(cx)
            .file_icons
            .then(|| {
                path_for_buffer(&self.buffer, 0, true, cx)
                    .and_then(|path| FileIcons::get_icon(Path::new(&*path), cx))
            })
            .flatten()
            .map(Icon::from_path)
    }

    fn tab_content(&self, params: TabContentParams, _: &Window, cx: &App) -> AnyElement {
        let label_color = if ItemSettings::get_global(cx).git_status {
            self.buffer()
                .read(cx)
                .as_singleton()
                .and_then(|buffer| {
                    let buffer = buffer.read(cx);
                    let path = buffer.project_path(cx)?;
                    let buffer_id = buffer.remote_id();
                    let project = self.project()?.read(cx);
                    let entry = project.entry_for_path(&path, cx)?;
                    let status = project
                        .git_store()
                        .read(cx)
                        .display_status_for_buffer_id(buffer_id, cx)?;

                    Some(entry_git_aware_label_color(
                        status.summary(),
                        entry.is_ignored,
                        params.selected,
                    ))
                })
                .unwrap_or_else(|| entry_label_color(params.selected))
        } else {
            entry_label_color(params.selected)
        };

        let description = params.detail.and_then(|detail| {
            let description = self.tab_description(detail, cx)?;
            Some(util::truncate_and_trailoff(
                &description,
                params.max_title_len.unwrap_or(MAX_TAB_TITLE_LEN),
            ))
        });

        // Whether the file was saved in the past but is now deleted.
        let was_deleted: bool = self
            .buffer()
            .read(cx)
            .as_singleton()
            .and_then(|buffer| buffer.read(cx).file())
            .is_some_and(|file| file.disk_state().is_deleted());

        h_flex()
            .gap_1()
            .when(!params.selected, |tab| tab.track_focus(&self.focus_handle))
            .when(params.truncate_title_middle, |this| {
                this.w_full().min_w_0().overflow_hidden()
            })
            .child(
                Label::new(if params.truncate_title_middle {
                    self.title(cx).to_string()
                } else {
                    util::truncate_and_trailoff(
                        &self.title(cx),
                        params.max_title_len.unwrap_or(MAX_TAB_TITLE_LEN),
                    )
                })
                .single_line()
                .color(label_color)
                .when(params.truncate_title_middle, |this| {
                    this.truncate_middle().flex_1()
                })
                .when(params.preview, |this| this.italic())
                .when(was_deleted, |this| this.strikethrough()),
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

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(EntityId, &dyn project::ProjectItem),
    ) {
        self.buffer
            .read(cx)
            .for_each_buffer(&mut |buffer| f(buffer.entity_id(), buffer.read(cx)));
    }

    fn buffer_kind(&self, cx: &App) -> ItemBufferKind {
        match self.buffer.read(cx).is_singleton() {
            true => ItemBufferKind::Singleton,
            false => ItemBufferKind::Multibuffer,
        }
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.active_buffer(cx)?.read(cx).project_path(cx)
    }

    fn can_save_as(&self, cx: &App) -> bool {
        self.buffer.read(cx).is_singleton()
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Editor>>>
    where
        Self: Sized,
    {
        Task::ready(Some(cx.new(|cx| self.clone(window, cx))))
    }

    fn set_nav_history(
        &mut self,
        history: ItemNavHistory,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) {
        self.nav_history = Some(history);
    }

    fn deactivated(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let selection = self.selections.newest_anchor();
        self.push_to_nav_history(selection.head(), None, true, false, cx);
    }

    fn workspace_deactivated(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.hide_hovered_link(cx);
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.buffer().read(cx).read(cx).is_dirty()
    }

    fn capability(&self, cx: &App) -> Capability {
        self.capability(cx)
    }

    // Note: this mirrors the logic in `Editor::toggle_read_only`, but is reachable
    // without relying on focus-based action dispatch.
    fn toggle_read_only(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(buffer) = self.buffer.read(cx).as_singleton() {
            buffer.update(cx, |buffer, cx| {
                buffer.set_capability(
                    match buffer.capability() {
                        Capability::ReadWrite => Capability::Read,
                        Capability::Read => Capability::ReadWrite,
                        Capability::ReadOnly => Capability::ReadOnly,
                    },
                    cx,
                );
            });
        }
        cx.notify();
        window.refresh();
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        self.buffer().read(cx).read(cx).has_deleted_file()
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.buffer().read(cx).read(cx).has_conflict()
    }

    fn can_save(&self, cx: &App) -> bool {
        if self.read_only(cx) {
            return false;
        }
        let buffer = &self.buffer().read(cx);
        if let Some(buffer) = buffer.as_singleton() {
            buffer.read(cx).project_path(cx).is_some()
        } else {
            true
        }
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if self.read_only(cx) {
            return Task::ready(Ok(()));
        }
        let buffers = self.buffer().clone().read(cx).all_buffers();
        let buffers = buffers
            .into_iter()
            .map(|handle| handle.read(cx).base_buffer().unwrap_or(handle.clone()))
            .collect::<HashSet<_>>();

        let buffers_to_save = if self.buffer.read(cx).is_singleton() && !options.autosave {
            buffers
        } else {
            buffers
                .into_iter()
                // Skip untitled buffers: a multi-buffer (e.g. project search results) can
                // excerpt a buffer with no file on disk, which can only be persisted via
                // `save_as`. Trying to save it here errors and aborts the whole save.
                .filter(|buffer| {
                    let buffer = buffer.read(cx);
                    buffer.is_dirty() && !buffer.read_only() && buffer.file().is_some()
                })
                .collect()
        };

        let format_trigger = if options.force_format {
            FormatTrigger::Manual
        } else {
            FormatTrigger::Save
        };

        cx.spawn_in(window, async move |this, cx| {
            if options.format {
                let format_task = this.update_in(cx, |editor, window, cx| {
                    let format_target = compute_format_target(
                        &buffers_to_save,
                        format_trigger,
                        editor.buffer(),
                        project.read(cx).git_store(),
                        cx,
                    );
                    format_target.map(|target| {
                        editor.perform_format(project.clone(), format_trigger, target, window, cx)
                    })
                })?;
                if let Some(format_task) = format_task {
                    format_task.await?;
                }
            }

            if !buffers_to_save.is_empty() {
                project
                    .update(cx, |project, cx| {
                        project.save_buffers(buffers_to_save.clone(), cx)
                    })
                    .await?;
            }

            Ok(())
        })
    }

    fn save_as(
        &mut self,
        project: Entity<Project>,
        path: ProjectPath,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let buffer = self
            .buffer()
            .read(cx)
            .as_singleton()
            .expect("cannot call save_as on an excerpt list");

        project.update(cx, |project, cx| project.save_buffer_as(buffer, path, cx))
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let buffer = self.buffer().clone();
        let buffers = self.buffer.read(cx).all_buffers();
        let reload_buffers =
            project.update(cx, |project, cx| project.reload_buffers(buffers, true, cx));
        cx.spawn_in(window, async move |this, cx| {
            let transaction = reload_buffers.log_err().await;
            this.update(cx, |editor, cx| {
                editor.request_autoscroll(Autoscroll::fit(), cx)
            })?;
            buffer.update(cx, |buffer, cx| {
                if let Some(transaction) = transaction
                    && !buffer.is_singleton()
                {
                    buffer.push_transaction(&transaction.0, cx);
                }
            });
            Ok(())
        })
    }

    fn as_searchable(
        &self,
        handle: &Entity<Self>,
        _: &App,
    ) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(handle.clone()))
    }

    fn pixel_position_of_cursor(&self, _: &App) -> Option<gpui::Point<Pixels>> {
        self.pixel_position_of_newest_cursor
    }

    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation {
        if self.breadcrumbs_visible() && self.buffer().read(cx).is_singleton() {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    // In a non-singleton case, the breadcrumbs are actually shown on sticky file headers of the multibuffer.
    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        if self.buffer.read(cx).is_singleton() {
            let font = theme_settings::ThemeSettings::get_global(cx)
                .buffer_font
                .clone();
            Some((self.breadcrumbs_inner(cx)?, Some(font)))
        } else {
            None
        }
    }

    fn breadcrumb_prefix(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        (!TabBarSettings::get_global(cx).show && ItemSettings::get_global(cx).file_icons)
            .then(|| {
                path_for_buffer(&self.buffer, 0, true, cx)
                    .and_then(|path| FileIcons::get_icon(Path::new(&*path), cx))
            })
            .flatten()
            .map(|icon_path| Icon::from_path(icon_path).into_any_element())
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace = Some((workspace.weak_handle(), workspace.database_id()));
        if let Some(workspace_entity) = &workspace.weak_handle().upgrade() {
            cx.subscribe(
                workspace_entity,
                |editor, _, event: &workspace::Event, cx| {
                    if let workspace::Event::ModalOpened = event {
                        editor.mouse_context_menu.take();
                        editor.hide_blame_popover(true, cx);
                    }
                },
            )
            .detach();
        }

        // Load persisted folds if this editor doesn't already have folds.
        // This handles manually-opened files (not workspace restoration).
        let display_snapshot = self
            .display_map
            .update(cx, |display_map, cx| display_map.snapshot(cx));
        let has_folds = display_snapshot
            .folds_in_range(MultiBufferOffset(0)..display_snapshot.buffer_snapshot().len())
            .next()
            .is_some();

        if !has_folds {
            if let Some(workspace_id) = workspace.database_id()
                && let Some(file_path) = self.buffer().read(cx).as_singleton().and_then(|buffer| {
                    project::File::from_dyn(buffer.read(cx).file()).map(|file| file.abs_path(cx))
                })
            {
                self.load_folds_from_db(workspace_id, file_path, window, cx);
            }
        }
    }

    fn pane_changed(&mut self, new_pane_id: EntityId, cx: &mut Context<Self>) {
        if self
            .highlighted_rows
            .get(&TypeId::of::<ActiveDebugLine>())
            .is_some_and(|lines| !lines.is_empty())
            && let Some(breakpoint_store) = self.breakpoint_store.as_ref()
        {
            breakpoint_store.update(cx, |store, _cx| {
                store.set_active_debug_pane_id(new_pane_id);
            });
        }
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            EditorEvent::Saved | EditorEvent::TitleChanged => {
                f(ItemEvent::UpdateTab);
                f(ItemEvent::UpdateBreadcrumbs);
            }

            EditorEvent::Reparsed(_) => {
                f(ItemEvent::UpdateBreadcrumbs);
            }

            EditorEvent::SelectionsChanged { local } if *local => {
                f(ItemEvent::UpdateBreadcrumbs);
            }

            EditorEvent::BreadcrumbsChanged | EditorEvent::OutlineSymbolsChanged => {
                f(ItemEvent::UpdateBreadcrumbs);
            }

            EditorEvent::DirtyChanged | EditorEvent::CapabilityChanged => {
                f(ItemEvent::UpdateTab);
            }

            EditorEvent::BufferEdited => {
                f(ItemEvent::Edit);
                f(ItemEvent::UpdateBreadcrumbs);
            }

            EditorEvent::BufferRangesUpdated { .. } | EditorEvent::BuffersRemoved { .. } => {
                f(ItemEvent::Edit);
            }

            _ => {}
        }
    }

    fn tab_extra_context_menu_actions(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<(SharedString, Box<dyn gpui::Action>)> {
        let mut actions = Vec::new();

        let is_markdown = self
            .buffer()
            .read(cx)
            .as_singleton()
            .and_then(|buffer| buffer.read(cx).language())
            .is_some_and(|language| language.name().as_ref() == "Markdown");

        let is_svg = self
            .buffer()
            .read(cx)
            .as_singleton()
            .and_then(|buffer| buffer.read(cx).file())
            .is_some_and(|file| {
                std::path::Path::new(file.file_name(cx))
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("svg"))
            });

        if is_markdown {
            actions.push((
                "Open Markdown Preview".into(),
                Box::new(OpenMarkdownPreview) as Box<dyn gpui::Action>,
            ));
        }

        if is_svg {
            actions.push((
                "Open SVG Preview".into(),
                Box::new(OpenSvgPreview) as Box<dyn gpui::Action>,
            ));
        }

        actions
    }

    fn preserve_preview(&self, cx: &App) -> bool {
        self.buffer.read(cx).preserve_preview(cx)
    }
}

impl SerializableItem for Editor {
    fn serialized_item_kind() -> &'static str {
        "Editor"
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        workspace::delete_unloaded_items(
            alive_items,
            workspace_id,
            "editors",
            &EditorDb::global(cx),
            cx,
        )
    }

    fn deserialize(
        project: Entity<Project>,
        _workspace: WeakEntity<Workspace>,
        workspace_id: workspace::WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let serialized_editor = match EditorDb::global(cx)
            .get_serialized_editor(item_id, workspace_id)
            .context("Failed to query editor state")
        {
            Ok(Some(serialized_editor)) => {
                if ProjectSettings::get_global(cx)
                    .session
                    .restore_unsaved_buffers
                {
                    serialized_editor
                } else {
                    SerializedEditor {
                        abs_path: serialized_editor.abs_path,
                        contents: None,
                        language: None,
                        mtime: None,
                    }
                }
            }
            Ok(None) => {
                return Task::ready(Err(anyhow!(
                    "Unable to deserialize editor: No entry in database for item_id: {item_id} and workspace_id {workspace_id:?}"
                )));
            }
            Err(error) => {
                return Task::ready(Err(error));
            }
        };
        log::debug!(
            "Deserialized editor {item_id:?} in workspace {workspace_id:?}, {serialized_editor:?}"
        );

        match serialized_editor {
            SerializedEditor {
                abs_path: None,
                contents: Some(contents),
                language,
                ..
            } => window.spawn(cx, {
                let project = project.clone();
                async move |cx| {
                    let content_language_detection_enabled = language.is_none();
                    let language_registry =
                        project.read_with(cx, |project, _| project.languages().clone());

                    let language = if let Some(language_name) = language {
                        // We don't fail here, because we'd rather not set the language if the name changed
                        // than fail to restore the buffer.
                        language_registry
                            .language_for_name(&language_name)
                            .await
                            .ok()
                    } else {
                        None
                    };

                    // First create the empty buffer
                    let buffer = project
                        .update(cx, |project, cx| project.create_buffer(language, true, cx))
                        .await
                        .context("Failed to create buffer while deserializing editor")?;

                    // Then set the text so that the dirty bit is set correctly
                    buffer.update(cx, |buffer, cx| {
                        if content_language_detection_enabled {
                            buffer.set_content_language_detection_enabled(true);
                        }
                        buffer.set_language_registry(language_registry);
                        buffer.set_text(contents, cx);
                        if let Some(entry) = buffer.peek_undo_stack() {
                            buffer.forget_transaction(entry.transaction_id());
                        }
                    });

                    cx.update(|window, cx| {
                        cx.new(|cx| {
                            let mut editor = Editor::for_buffer(buffer, Some(project), window, cx);

                            editor.read_metadata_from_db(item_id, workspace_id, window, cx);
                            editor
                        })
                    })
                }
            }),
            SerializedEditor {
                abs_path: Some(abs_path),
                contents,
                mtime,
                ..
            } => {
                let opened_buffer = project.update(cx, |project, cx| {
                    let (worktree, path) = project.find_worktree(&abs_path, cx)?;
                    let project_path = ProjectPath {
                        worktree_id: worktree.read(cx).id(),
                        path: path,
                    };
                    Some(project.open_path(project_path, cx))
                });

                match opened_buffer {
                    Some(opened_buffer) => window.spawn(cx, async move |cx| {
                        let (_, buffer) = opened_buffer
                            .await
                            .context("Failed to open path in project")?;

                        if let Some(contents) = contents {
                            buffer.update(cx, |buffer, cx| {
                                restore_serialized_buffer_contents(buffer, contents, mtime, cx);
                            });
                        }

                        cx.update(|window, cx| {
                            cx.new(|cx| {
                                let mut editor =
                                    Editor::for_buffer(buffer, Some(project), window, cx);

                                editor.read_metadata_from_db(item_id, workspace_id, window, cx);
                                editor
                            })
                        })
                    }),
                    None => {
                        // File is not in any worktree (e.g., opened as a standalone file).
                        // Open the buffer directly via the project rather than through
                        // workspace.open_abs_path(), which has the side effect of adding
                        // the item to a pane. The caller (deserialize_to) will add the
                        // returned item to the correct pane.
                        window.spawn(cx, async move |cx| {
                            let buffer = project
                                .update(cx, |project, cx| project.open_local_buffer(&abs_path, cx))
                                .await
                                .with_context(|| {
                                    format!("Failed to open buffer for {abs_path:?}")
                                })?;

                            if let Some(contents) = contents {
                                buffer.update(cx, |buffer, cx| {
                                    restore_serialized_buffer_contents(buffer, contents, mtime, cx);
                                });
                            }

                            cx.update(|window, cx| {
                                cx.new(|cx| {
                                    let mut editor =
                                        Editor::for_buffer(buffer, Some(project), window, cx);
                                    editor.read_metadata_from_db(item_id, workspace_id, window, cx);
                                    editor
                                })
                            })
                        })
                    }
                }
            }
            SerializedEditor {
                abs_path: None,
                contents: None,
                language,
                ..
            } => window.spawn(cx, async move |cx| {
                let buffer = project
                    .update(cx, |project, cx| project.create_buffer(None, true, cx))
                    .await
                    .context("Failed to create buffer")?;
                if language.is_none() {
                    buffer.update(cx, |buffer, _| {
                        buffer.set_content_language_detection_enabled(true);
                    });
                }

                cx.update(|window, cx| {
                    cx.new(|cx| {
                        let mut editor = Editor::for_buffer(buffer, Some(project), window, cx);

                        editor.read_metadata_from_db(item_id, workspace_id, window, cx);
                        editor
                    })
                })
            }),
        }
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let buffer_serialization = self.buffer_serialization?;
        let project = self.project.clone()?;

        let serialize_dirty_buffers = match buffer_serialization {
            // Always serialize dirty buffers, including for worktree-less windows.
            // This enables hot-exit functionality for empty windows and single files.
            BufferSerialization::All => true,
            BufferSerialization::NonDirtyBuffers => false,
        };

        if closing && !serialize_dirty_buffers {
            return None;
        }

        let workspace_id = workspace.database_id()?;

        let buffer = self.buffer().read(cx).as_singleton()?;

        let abs_path = buffer.read(cx).file().and_then(|file| {
            let worktree_id = file.worktree_id(cx);
            project
                .read(cx)
                .worktree_for_id(worktree_id, cx)
                .map(|worktree| worktree.read(cx).absolutize(file.path()))
                .or_else(|| {
                    let full_path = file.full_path(cx);
                    let project_path = project.read(cx).find_project_path(&full_path, cx)?;
                    project.read(cx).absolute_path(&project_path, cx)
                })
        });

        let is_dirty = buffer.read(cx).is_dirty();
        let mtime = buffer.read(cx).saved_mtime();
        let content_language_detection_enabled =
            buffer.read(cx).content_language_detection_enabled();

        let snapshot = buffer.read(cx).snapshot();

        let db = EditorDb::global(cx);
        Some(cx.background_spawn(async move {
            let (contents, language) = if serialize_dirty_buffers && is_dirty {
                let contents = snapshot.text();
                let language = snapshot.language().and_then(|language| {
                    if content_language_detection_enabled && *language == *PLAIN_TEXT {
                        None
                    } else {
                        Some(language.name().to_string())
                    }
                });
                (Some(contents), language)
            } else {
                (None, None)
            };

            let editor = SerializedEditor {
                abs_path,
                contents,
                language,
                mtime,
            };
            log::debug!("Serializing editor {item_id:?} in workspace {workspace_id:?}");
            db.save_serialized_editor(item_id, workspace_id, editor)
                .await
                .context("failed to save serialized editor")
        }))
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        self.should_serialize_buffer()
            && matches!(
                event,
                EditorEvent::Saved
                    | EditorEvent::DirtyChanged
                    | EditorEvent::BufferEdited
                    | EditorEvent::FileHandleChanged
            )
    }
}

#[derive(Debug, Default)]
struct EditorRestorationData {
    entries: HashMap<PathBuf, RestorationData>,
}

#[derive(Default, Debug)]
pub struct RestorationData {
    pub scroll_position: (BufferRow, gpui::Point<ScrollOffset>),
    pub folds: Vec<Range<Point>>,
    pub selections: Vec<Range<Point>>,
}

impl ProjectItem for Editor {
    type Item = Buffer;

    fn project_item_kind() -> Option<ProjectItemKind> {
        Some(ProjectItemKind("Editor"))
    }

    fn for_project_item(
        project: Entity<Project>,
        pane: Option<&Pane>,
        buffer: Entity<Buffer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut editor = Self::for_buffer(buffer.clone(), Some(project), window, cx);
        let multibuffer_snapshot = editor.buffer().read(cx).snapshot(cx);

        if let Some(buffer_snapshot) = editor.buffer().read(cx).snapshot(cx).as_singleton()
            && WorkspaceSettings::get(None, cx).restore_on_file_reopen
            && let Some(restoration_data) = Self::project_item_kind()
                .and_then(|kind| pane.as_ref()?.project_item_restoration_data.get(&kind))
                .and_then(|data| data.downcast_ref::<EditorRestorationData>())
                .and_then(|data| {
                    let file = project::File::from_dyn(buffer.read(cx).file())?;
                    data.entries.get(&file.abs_path(cx))
                })
        {
            if !restoration_data.folds.is_empty() {
                editor.fold_ranges(
                    clip_ranges(&restoration_data.folds, buffer_snapshot),
                    false,
                    window,
                    cx,
                );
            }
            if !restoration_data.selections.is_empty() {
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
                    s.select_ranges(clip_ranges(&restoration_data.selections, buffer_snapshot));
                });
            }
            let (top_row, offset) = restoration_data.scroll_position;
            let anchor = multibuffer_snapshot.anchor_before(Point::new(top_row, 0));
            editor.set_scroll_anchor(ScrollAnchor { anchor, offset }, window, cx);
        }

        editor
    }

    fn for_broken_project_item(
        abs_path: &Path,
        is_local: bool,
        e: &anyhow::Error,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InvalidItemView> {
        Some(InvalidItemView::new(abs_path, is_local, e, window, cx))
    }
}

fn clip_ranges<'a>(
    original: impl IntoIterator<Item = &'a Range<Point>> + 'a,
    snapshot: &'a BufferSnapshot,
) -> Vec<Range<Point>> {
    original
        .into_iter()
        .map(|range| {
            snapshot.clip_point(range.start, Bias::Left)
                ..snapshot.clip_point(range.end, Bias::Right)
        })
        .collect()
}

impl EventEmitter<SearchEvent> for Editor {}

impl Editor {
    /// Panes keep increasing `detail` until tabs for files with the same name look different.
    pub fn tab_description(&self, detail: usize, cx: &App) -> Option<SharedString> {
        let path = path_for_buffer(&self.buffer, detail, false, cx)?;
        let description = path.trim();
        if description.is_empty() {
            None
        } else {
            Some(description.to_string().into())
        }
    }

    pub fn update_restoration_data(
        &self,
        cx: &mut Context<Self>,
        write: impl for<'a> FnOnce(&'a mut RestorationData) + 'static,
    ) {
        if self.mode.is_minimap() || !WorkspaceSettings::get(None, cx).restore_on_file_reopen {
            return;
        }

        let editor = cx.entity();
        cx.defer(move |cx| {
            editor.update(cx, |editor, cx| {
                let kind = Editor::project_item_kind()?;
                let pane = editor.workspace()?.read(cx).pane_for(&cx.entity())?;
                let buffer = editor.buffer().read(cx).as_singleton()?;
                let file_abs_path = project::File::from_dyn(buffer.read(cx).file())?.abs_path(cx);
                pane.update(cx, |pane, _| {
                    let data = pane
                        .project_item_restoration_data
                        .entry(kind)
                        .or_insert_with(|| Box::new(EditorRestorationData::default()) as Box<_>);
                    let data = match data.downcast_mut::<EditorRestorationData>() {
                        Some(data) => data,
                        None => {
                            *data = Box::new(EditorRestorationData::default());
                            data.downcast_mut::<EditorRestorationData>()
                                .expect("just written the type downcasted to")
                        }
                    };

                    let data = data.entries.entry(file_abs_path).or_default();
                    write(data);
                    Some(())
                })
            });
        });
    }
}

// Replace-all commonly expands several hits against the same line.
#[derive(Default)]
struct SearchHitContext {
    row: Option<u32>,
    text: String,
}

impl SearchHitContext {
    fn for_hit(
        &mut self,
        snapshot: &MultiBufferSnapshot,
        hit: &Range<Anchor>,
    ) -> (&str, Range<usize>) {
        let start = hit.start.to_point(snapshot);
        let end = hit.end.to_point(snapshot);
        let range = if start.row == end.row {
            if self.row != Some(start.row) {
                self.text.clear();
                self.text.extend(snapshot.text_for_range(
                    Point::new(start.row, 0)
                        ..Point::new(start.row, snapshot.line_len(MultiBufferRow(start.row))),
                ));
                self.row = Some(start.row);
            }
            start.column as usize..end.column as usize
        } else {
            self.row = None;
            self.text.clear();
            self.text.extend(snapshot.text_for_range(start..end));
            0..self.text.len()
        };
        (&self.text, range)
    }
}

impl SearchableItem for Editor {
    type Match = Range<Anchor>;

    fn get_matches(&self, _window: &mut Window, _: &mut App) -> (Vec<Range<Anchor>>, SearchToken) {
        (
            self.background_highlights
                .get(&HighlightKey::BufferSearchHighlights)
                .map_or(Vec::new(), |(_color, ranges)| {
                    ranges.iter().cloned().collect()
                }),
            SearchToken::default(),
        )
    }

    fn clear_matches(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self
            .clear_background_highlights(HighlightKey::BufferSearchHighlights, cx)
            .is_some()
        {
            cx.emit(SearchEvent::MatchesInvalidated);
        }
    }

    fn update_matches(
        &mut self,
        matches: &[Range<Anchor>],
        active_match_index: Option<usize>,
        _token: SearchToken,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing_range = self
            .background_highlights
            .get(&HighlightKey::BufferSearchHighlights)
            .map(|(_, range)| range.as_ref());
        let updated = existing_range != Some(matches);
        self.highlight_background(
            HighlightKey::BufferSearchHighlights,
            matches,
            move |index, theme| {
                if active_match_index == Some(*index) {
                    theme.colors().search_active_match_background
                } else {
                    theme.colors().search_match_background
                }
            },
            cx,
        );
        if updated {
            cx.emit(SearchEvent::MatchesInvalidated);
        }
    }

    fn has_filtered_search_ranges(&mut self) -> bool {
        self.has_background_highlights(HighlightKey::SearchWithinRange)
    }

    fn toggle_filtered_search_ranges(
        &mut self,
        enabled: Option<FilteredSearchRange>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_filtered_search_ranges() {
            self.previous_search_ranges = self
                .clear_background_highlights(HighlightKey::SearchWithinRange, cx)
                .map(|(_, ranges)| ranges)
        }

        if let Some(range) = enabled {
            let ranges = self.selections.disjoint_anchor_ranges().collect::<Vec<_>>();

            if ranges.iter().any(|s| s.start != s.end) {
                self.set_search_within_ranges(&ranges, cx);
            } else if let Some(previous_search_ranges) = self.previous_search_ranges.take()
                && range != FilteredSearchRange::Selection
            {
                self.set_search_within_ranges(&previous_search_ranges, cx);
            }
        }
    }

    fn supported_options(&self) -> SearchOptions {
        if self.in_project_search {
            SearchOptions {
                case: true,
                word: true,
                regex: true,
                replacement: false,
                selection: false,
                select_all: true,
                find_in_results: true,
            }
        } else {
            SearchOptions {
                case: true,
                word: true,
                regex: true,
                replacement: true,
                selection: true,
                select_all: true,
                find_in_results: false,
            }
        }
    }

    fn query_suggestion(
        &mut self,
        seed_query_override: Option<SeedQuerySetting>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> String {
        let setting = seed_query_override
            .unwrap_or_else(|| EditorSettings::get_global(cx).seed_search_query_from_cursor);
        let snapshot = self.snapshot(window, cx);
        let selection = self.selections.newest_adjusted(&snapshot.display_snapshot);
        let buffer_snapshot = snapshot.buffer_snapshot();

        match setting {
            SeedQuerySetting::Never => String::new(),
            SeedQuerySetting::Selection | SeedQuerySetting::Always if !selection.is_empty() => {
                buffer_snapshot
                    .text_for_range(selection.start..selection.end)
                    .collect()
            }
            SeedQuerySetting::Selection => String::new(),
            SeedQuerySetting::Always => {
                let (range, kind) = buffer_snapshot
                    .surrounding_word(selection.start, Some(CharScopeContext::Completion));
                if kind == Some(CharKind::Word) {
                    let text: String = buffer_snapshot.text_for_range(range).collect();
                    if !text.trim().is_empty() {
                        return text;
                    }
                }
                String::new()
            }
        }
    }

    fn activate_match(
        &mut self,
        index: usize,
        matches: &[Range<Anchor>],
        _token: SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.unfold_ranges(&[matches[index].clone()], false, true, cx);
        let range = self.range_for_match(&matches[index]);
        let autoscroll = if EditorSettings::get_global(cx).search.center_on_match {
            Autoscroll::center()
        } else {
            Autoscroll::fit()
        };
        self.change_selections(
            SelectionEffects::scroll(autoscroll).from_search(true),
            window,
            cx,
            |s| {
                s.select_ranges([range]);
            },
        )
    }

    fn select_matches(
        &mut self,
        matches: &[Self::Match],
        _token: SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.unfold_ranges(matches, false, false, cx);
        self.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
            s.select_ranges(matches.iter().cloned())
        });
    }
    fn replace(
        &mut self,
        identifier: &Self::Match,
        query: &SearchQuery,
        _token: SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let replacement = if query.replacement_requires_context() {
            let snapshot = self.buffer.read(cx).snapshot(cx);
            let mut context = SearchHitContext::default();
            let (line, hit) = context.for_hit(&snapshot, identifier);
            query
                .replacement_for(line, hit)
                .map(|replacement| Arc::<str>::from(&*replacement))
        } else {
            query.replacement().map(Arc::<str>::from)
        };

        if let Some(replacement) = replacement {
            self.transact(window, cx, |this, _, cx| {
                this.edit([(identifier.clone(), replacement)], cx);
            });
        }
    }
    fn replace_all(
        &mut self,
        matches: &mut dyn Iterator<Item = &Self::Match>,
        query: &SearchQuery,
        _token: SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let mut edits = vec![];

        // A regex might have replacement variables so we cannot apply
        // the same replacement to all matches
        if query.replacement_requires_context() {
            let mut context = SearchHitContext::default();
            edits = matches
                .filter_map(|m| {
                    let (line, hit) = context.for_hit(&snapshot, m);
                    query
                        .replacement_for(line, hit)
                        .map(|replacement| (m.clone(), Arc::from(&*replacement)))
                })
                .collect();
        } else if let Some(replacement) = query.replacement().map(Arc::<str>::from) {
            edits = matches.map(|m| (m.clone(), replacement.clone())).collect();
        }

        if !edits.is_empty() {
            self.transact(window, cx, |this, _, cx| {
                this.edit(edits, cx);
            });
        }
    }

    /// Takes the current cursor position and finds the next match in the
    /// provided `direction`, the provide `count` number of times, wrapping
    /// around if necessary.
    fn match_index_for_direction(
        &mut self,
        matches: &[Range<Anchor>],
        current_index: usize,
        direction: Direction,
        count: usize,
        _token: SearchToken,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        if count == 0 {
            return current_index;
        }

        let cursor = if self.selections.disjoint_anchors_arc().len() == 1 {
            self.selections.newest_anchor().head()
        } else {
            matches[current_index].start
        };

        let buffer = self.buffer().read(cx).snapshot(cx);
        let new_idx = match direction {
            Direction::Next => matches
                .iter()
                .position(|m| m.start.cmp(&cursor, &buffer).is_gt())
                .unwrap_or(0),
            Direction::Prev => matches
                .iter()
                .rposition(|m| m.end.cmp(&cursor, &buffer).is_lt())
                .unwrap_or(matches.len() - 1),
        } as isize;

        // We'll use `count - 1` because the first jump to the next or previous
        // match already happens in the scenario above, when we find the next or
        // previous match starting from the cursor position.
        let count = count.saturating_sub(1);
        let count = match direction {
            Direction::Prev => -(count as isize),
            Direction::Next => count as isize,
        };

        let new_idx = (new_idx + count) % matches.len() as isize;
        let new_idx = if new_idx.is_negative() {
            // We need a `matches.len() - 1` here in case `next_idx` has now been
            // set to `0`, otherwise we'd end up returning `matches.len()`, which
            // would be out of bounds.
            new_idx + (matches.len() - 1) as isize
        } else {
            new_idx
        };
        assert!(new_idx < matches.len() as isize);
        new_idx as usize
    }

    fn find_matches(
        &mut self,
        query: Arc<project::search::SearchQuery>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Vec<Range<Anchor>>> {
        let buffer = self.buffer().read(cx).snapshot(cx);
        let search_within_ranges = self
            .background_highlights
            .get(&HighlightKey::SearchWithinRange)
            .map_or(vec![], |(_color, ranges)| {
                ranges.iter().cloned().collect::<Vec<_>>()
            });

        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let mut ranges = Vec::new();

            let search_within_ranges = if search_within_ranges.is_empty() {
                vec![buffer.anchor_before(MultiBufferOffset(0))..buffer.anchor_after(buffer.len())]
            } else {
                search_within_ranges
            };
            let num_cpus = executor.num_cpus();
            for range in search_within_ranges {
                for (search_buffer, search_range, deleted_hunk_anchor) in
                    buffer.range_to_buffer_ranges_with_deleted_hunks(range)
                {
                    let query = query.clone();

                    let mut results = Vec::new();
                    executor
                        .scoped(|scope| {
                            for search_range in chunk_search_range(
                                search_buffer.text.clone(),
                                &query,
                                num_cpus as u32,
                                search_range,
                            ) {
                                let query = query.clone();
                                let buffer = buffer.clone();

                                let (tx, rx) = oneshot::channel();
                                results.push(rx);
                                scope.spawn(async move {
                                    let chunk_result = query
                                        .search(
                                            search_buffer,
                                            Some(search_range.start..search_range.end),
                                        )
                                        .await
                                        .into_iter()
                                        .filter_map(|match_range| {
                                            if let Some(deleted_hunk_anchor) = deleted_hunk_anchor {
                                                let start = search_buffer.anchor_after(
                                                    search_range.start + match_range.start,
                                                );
                                                let end = search_buffer.anchor_before(
                                                    search_range.start + match_range.end,
                                                );
                                                Some(
                                                    deleted_hunk_anchor.with_diff_base_anchor(start)
                                                        ..deleted_hunk_anchor
                                                            .with_diff_base_anchor(end),
                                                )
                                            } else {
                                                let start = search_buffer.anchor_after(
                                                    search_range.start + match_range.start,
                                                );
                                                let end = search_buffer.anchor_before(
                                                    search_range.start + match_range.end,
                                                );
                                                buffer.anchor_range_in_buffer(start..end)
                                            }
                                        })
                                        .collect::<Vec<_>>();
                                    _ = tx.send(chunk_result);
                                });
                            }
                        })
                        .await;

                    for rx in results {
                        if let Ok(results) = rx.await {
                            ranges.extend(results);
                        }
                    }
                }
            }

            ranges
        })
    }

    fn active_match_index(
        &mut self,
        direction: Direction,
        matches: &[Range<Anchor>],
        _token: SearchToken,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        active_match_index(
            direction,
            matches,
            &self.selections.newest_anchor().head(),
            &self.buffer().read(cx).snapshot(cx),
        )
    }

    fn search_bar_visibility_changed(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) {
        self.expect_bounds_change = self.last_bounds;
    }

    fn set_select_search_options(
        &mut self,
        select_search_options: Option<SelectSearchOptions>,
        _cx: &mut Context<Self>,
    ) {
        if self.select_next_options == select_search_options {
            return;
        }

        self.select_next_options = select_search_options;
        self.select_next_state = None;
        self.select_prev_state = None;
    }
}

pub fn active_match_index(
    direction: Direction,
    ranges: &[Range<Anchor>],
    cursor: &Anchor,
    buffer: &MultiBufferSnapshot,
) -> Option<usize> {
    if ranges.is_empty() {
        None
    } else {
        let r = ranges.binary_search_by(|probe| {
            if probe.end.cmp(cursor, buffer).is_lt() {
                Ordering::Less
            } else if probe.start.cmp(cursor, buffer).is_gt() {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        });
        match direction {
            Direction::Prev => match r {
                Ok(i) => Some(i),
                Err(i) => Some(i.saturating_sub(1)),
            },
            Direction::Next => match r {
                Ok(i) | Err(i) => Some(cmp::min(i, ranges.len() - 1)),
            },
        }
    }
}

/// Opens a path-like target (e.g. `items.rs:100:5`) in the workspace, moving the cursor
/// to the one-based row/column if present. Returns whether the target was opened.
pub async fn open_resolved_target(
    workspace: &WeakEntity<Workspace>,
    open_target: &workspace::path_link::OpenTarget,
    cx: &mut AsyncWindowContext,
) -> Result<bool> {
    let path_to_open = open_target.path();
    let mut opened_items = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_paths(
                vec![path_to_open.path.clone()],
                workspace::OpenOptions {
                    visible: Some(workspace::OpenVisible::OnlyDirectories),
                    ..Default::default()
                },
                None,
                window,
                cx,
            )
        })
        .context("workspace update")?
        .await;
    if opened_items.len() != 1 {
        debug_panic!(
            "Received {} items for one path {path_to_open:?}",
            opened_items.len(),
        );
    }
    let Some(opened_item) = opened_items.pop() else {
        return Ok(false);
    };

    if open_target.is_file() {
        let Some(opened_item) = opened_item else {
            return Ok(false);
        };
        let opened_item =
            opened_item.with_context(|| format!("opening {:?}", path_to_open.path))?;
        if let Some(row) = path_to_open.row
            && let Some(editor) = opened_item.downcast::<Editor>()
        {
            let column = path_to_open.column.unwrap_or(0);
            editor
                .downgrade()
                .update_in(cx, |editor, window, cx| {
                    if let Some(buffer) = editor.buffer().read(cx).as_singleton() {
                        let point = buffer.read(cx).snapshot().point_from_external_input(
                            row.saturating_sub(1),
                            column.saturating_sub(1),
                        );
                        editor.go_to_singleton_buffer_point(point, window, cx);
                    }
                })
                .log_err();
        }
        Ok(true)
    } else if open_target.is_dir() {
        workspace.update(cx, |workspace, cx| {
            workspace.project().update(cx, |_, cx| {
                cx.emit(project::Event::ActivateProjectPanel);
            })
        })?;
        Ok(true)
    } else {
        Ok(false)
    }
}

pub fn entry_label_color(selected: bool) -> Color {
    if selected {
        Color::Default
    } else {
        Color::Muted
    }
}

pub fn entry_diagnostic_aware_icon_name_and_color(
    diagnostic_severity: Option<DiagnosticSeverity>,
) -> Option<(IconName, Color)> {
    match diagnostic_severity {
        Some(DiagnosticSeverity::ERROR) => Some((IconName::Close, Color::Error)),
        Some(DiagnosticSeverity::WARNING) => Some((IconName::Triangle, Color::Warning)),
        _ => None,
    }
}

pub fn entry_diagnostic_aware_icon_decoration_and_color(
    diagnostic_severity: Option<DiagnosticSeverity>,
) -> Option<(IconDecorationKind, Color)> {
    match diagnostic_severity {
        Some(DiagnosticSeverity::ERROR) => Some((IconDecorationKind::X, Color::Error)),
        Some(DiagnosticSeverity::WARNING) => Some((IconDecorationKind::Triangle, Color::Warning)),
        _ => None,
    }
}

pub fn entry_git_aware_label_color(git_status: GitSummary, ignored: bool, selected: bool) -> Color {
    let tracked = git_status.index + git_status.worktree;
    if git_status.conflict > 0 {
        Color::Conflict
    } else if tracked.deleted > 0 {
        Color::Deleted
    } else if tracked.modified > 0 {
        Color::Modified
    } else if tracked.added > 0 || git_status.untracked > 0 {
        Color::Created
    } else if ignored {
        Color::Ignored
    } else {
        entry_label_color(selected)
    }
}

fn path_for_buffer<'a>(
    buffer: &Entity<MultiBuffer>,
    height: usize,
    include_filename: bool,
    cx: &'a App,
) -> Option<Cow<'a, str>> {
    let file = buffer.read(cx).as_singleton()?.read(cx).file()?;
    path_for_file(file, height, include_filename, cx)
}

fn path_for_file<'a>(
    file: &'a Arc<dyn language::File>,
    mut height: usize,
    include_filename: bool,
    cx: &'a App,
) -> Option<Cow<'a, str>> {
    if project::File::from_dyn(Some(file)).is_none() {
        return None;
    }

    let file = file.as_ref();
    // Ensure we always render at least the filename.
    height += 1;

    let mut prefix = file.path().as_ref();
    while height > 0 {
        if let Some(parent) = prefix.parent() {
            prefix = parent;
            height -= 1;
        } else {
            break;
        }
    }

    // The full_path method allocates, so avoid calling it if height is zero.
    if height > 0 {
        let mut full_path = file.full_path(cx);
        if !include_filename {
            if !full_path.pop() {
                return None;
            }
        }
        Some(full_path.to_string_lossy().into_owned().into())
    } else {
        let mut path = file.path().strip_prefix(prefix).ok()?;
        if !include_filename {
            path = path.parent()?;
        }
        Some(path.display(file.path_style(cx)))
    }
}

/// Restores serialized buffer contents by overwriting the buffer with saved text.
/// This is somewhat wasteful since we load the whole buffer from disk then overwrite it,
/// but keeps implementation simple as we don't need to persist all metadata from loading
/// (git diff base, etc.).
fn restore_serialized_buffer_contents(
    buffer: &mut Buffer,
    contents: String,
    mtime: Option<MTime>,
    cx: &mut Context<Buffer>,
) {
    // If we did restore an mtime, store it on the buffer so that
    // the next edit will mark the buffer as dirty/conflicted.
    if mtime.is_some() {
        buffer.did_reload(buffer.version(), buffer.line_ending(), mtime, cx);
    }
    buffer.set_text(contents, cx);
    if let Some(entry) = buffer.peek_undo_stack() {
        buffer.forget_transaction(entry.transaction_id());
    }
}

fn chunk_search_range(
    buffer: BufferSnapshot,
    query: &SearchQuery,
    num_cpus: u32,
    initial_range: Range<BufferOffset>,
) -> Box<dyn Iterator<Item = Range<usize>> + 'static> {
    let range = initial_range.to_offset(&buffer);
    if range.is_empty() {
        return Box::new(std::iter::empty());
    }

    let summary: TextSummary = buffer.text_summary_for_range(initial_range);
    let num_chunks = if !query.is_regex() && !query.as_str().contains('\n') {
        NonZeroU32::new(summary.lines.row.saturating_add(1).min(num_cpus.max(1)))
    } else {
        NonZeroU32::new(1)
    };

    let Some(num_chunks) = num_chunks else {
        return Box::new(std::iter::empty());
    };

    let mut chunk_start = range.start;
    let rope = buffer.as_rope().clone();
    let range_end = range.end;
    let average_chunk_length = summary.len.div_ceil(num_chunks.get() as usize);
    Box::new(std::iter::from_fn(move || {
        if chunk_start >= range_end {
            return None;
        }
        let candidate_position = chunk_start + average_chunk_length;
        let adjusted = rope.ceil_char_boundary(candidate_position);
        let mut as_point = rope.offset_to_point(adjusted);
        as_point.row += 1;
        as_point.column = 0;
        let end_offset = buffer.point_to_offset(as_point).min(range_end);
        let ret = chunk_start..end_offset;
        chunk_start = end_offset;
        Some(ret)
    }))
}

/// Decides what to format based on the `format_on_save` settings of the saved buffers.
///
/// In the modifications modes, only lines with unstaged changes are formatted.
/// When no git diff is available for a buffer, `modifications` skips formatting while `modifications_if_available`
/// falls back to formatting entire buffers.
/// When a diff is available but empty, nothing is formatted in either mode.
fn compute_format_target(
    buffers: &HashSet<Entity<Buffer>>,
    trigger: FormatTrigger,
    multi_buffer: &Entity<MultiBuffer>,
    git_store: &Entity<GitStore>,
    cx: &App,
) -> Option<FormatTarget> {
    if trigger == FormatTrigger::Manual {
        return Some(FormatTarget::Buffers(buffers.clone()));
    }

    let multi_buffer_snapshot = multi_buffer.read(cx).snapshot(cx);
    let git_store = git_store.read(cx);

    let mut fall_back_to_full_format = false;
    let mut modified_ranges: Vec<Range<Point>> = Vec::new();

    for buffer_entity in buffers.iter() {
        let buffer = buffer_entity.read(cx);
        let settings = LanguageSettings::for_buffer(buffer, cx);
        match settings.format_on_save {
            FormatOnSave::On | FormatOnSave::Off => {
                return Some(FormatTarget::Buffers(buffers.clone()));
            }
            FormatOnSave::Modifications | FormatOnSave::ModificationsIfAvailable => {}
        }

        let Some(diff_snapshot) = git_store
            .get_unstaged_diff(buffer.remote_id(), cx)
            .map(|diff| diff.read(cx).snapshot(cx))
        else {
            if settings.format_on_save == FormatOnSave::ModificationsIfAvailable {
                fall_back_to_full_format = true;
            }
            continue;
        };

        let anchor_ranges = compute_modified_ranges(&buffer.snapshot(), &diff_snapshot);
        let flat_anchors = anchor_ranges
            .iter()
            .flat_map(|range| [range.start, range.end])
            .collect::<Vec<_>>();
        let multi_buffer_anchors =
            multi_buffer_snapshot.text_anchors_to_visible_anchors(flat_anchors);
        for pair in multi_buffer_anchors.chunks_exact(2) {
            let (Some(start), Some(end)) = (&pair[0], &pair[1]) else {
                continue;
            };
            modified_ranges
                .push(start.to_point(&multi_buffer_snapshot)..end.to_point(&multi_buffer_snapshot));
        }
    }

    if fall_back_to_full_format {
        Some(FormatTarget::Buffers(buffers.clone()))
    } else if modified_ranges.is_empty() {
        None
    } else {
        Some(FormatTarget::Ranges(modified_ranges))
    }
}

/// Computes the buffer ranges that have unstaged changes, expanded to full lines and
/// with adjacent hunks merged, for use with format-on-save. An empty result means the
/// buffer has no formatable modifications.
fn compute_modified_ranges(
    buffer_snapshot: &language::BufferSnapshot,
    diff_snapshot: &buffer_diff::BufferDiffSnapshot,
) -> Vec<Range<text::Anchor>> {
    let mut merged: Vec<Range<text::Anchor>> = Vec::new();
    for hunk in diff_snapshot.hunks(buffer_snapshot) {
        let range = hunk.buffer_range;
        if range.start.cmp(&range.end, buffer_snapshot).is_eq() {
            // Deletion-only hunks produce no buffer content to format.
            continue;
        }
        let start_point = range.start.to_point(buffer_snapshot);
        let end_point = range.end.to_point(buffer_snapshot);
        let start_row = start_point.row;
        let end_row = if end_point.column == 0 && end_point.row > start_point.row {
            end_point.row - 1
        } else {
            end_point.row
        };
        let line_start = text::Point::new(start_row, 0);
        let line_end = text::Point::new(end_row, buffer_snapshot.line_len(end_row));
        let expanded =
            buffer_snapshot.anchor_before(line_start)..buffer_snapshot.anchor_after(line_end);

        if let Some(last) = merged.last_mut() {
            let last_end_point = last.end.to_point(buffer_snapshot);
            if start_row <= last_end_point.row + 1 {
                if expanded.end.to_point(buffer_snapshot) > last_end_point {
                    last.end = expanded.end;
                }
                continue;
            }
        }
        merged.push(expanded);
    }
    merged
}

pub(crate) fn handle_lsp_show_document(
    workspace: &mut Workspace,
    request: &LanguageServerShowDocumentRequest,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<()> {
    let request = request.clone();
    if request.external {
        cx.open_url(request.uri.as_str());
        request.respond(true);
        return Task::ready(());
    }
    let Ok(abs_path) = request.uri.to_file_path_ext(workspace.path_style(cx)) else {
        log::error!(
            "language server requested to show document with unsupported uri {}",
            request.uri.as_str()
        );
        request.respond(false);
        return Task::ready(());
    };
    let open_task = workspace.open_abs_path(
        abs_path,
        OpenOptions {
            visible: Some(OpenVisible::None),
            focus: Some(request.take_focus),
            ..OpenOptions::default()
        },
        window,
        cx,
    );
    cx.spawn_in(window, async move |_, cx| {
        let success = match open_task.await {
            Ok(item) => match item.downcast::<Editor>().zip(request.selection) {
                Some((editor, selection)) => editor
                    .update_in(cx, |editor, window, cx| {
                        let snapshot = editor.buffer().read(cx).snapshot(cx);
                        let range = language::range_from_lsp(selection);
                        let start = snapshot.point_utf16_to_offset(
                            snapshot.clip_point_utf16(range.start, Bias::Left),
                        );
                        let end = snapshot.point_utf16_to_offset(
                            snapshot.clip_point_utf16(range.end, Bias::Left),
                        );
                        editor.change_selections(
                            SelectionEffects::scroll(Autoscroll::center()),
                            window,
                            cx,
                            |selections| selections.select_ranges([start..end]),
                        );
                    })
                    .is_ok(),
                None => true,
            },
            Err(error) => {
                log::error!("failed to show document for a language server: {error:#}");
                false
            }
        };
        request.respond(success);
    })
}