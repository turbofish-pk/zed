mod persistence;
pub mod terminal_element;
pub mod terminal_panel;
mod terminal_path_like_target;
pub mod terminal_scrollbar;

use editor::{
    Editor, EditorSettings, actions::SelectAll, blink_manager::BlinkManager,
    ui_scrollbar_settings_from_raw,
};
use gpui::{
    Action, AnyElement, App, ClipboardEntry, Corners, DismissEvent, Entity, EventEmitter,
    ExternalPaths, FocusHandle, Focusable, Font, KeyContext, KeyDownEvent, Keystroke, MouseButton,
    MouseDownEvent, Pixels, Point as GpuiPoint, Rems, Render, ScrollEvent, Styled, Subscription,
    Task, TaskExt, WeakEntity, actions, anchored, deferred, div,
};
use menu;
use persistence::TerminalDb;
use project::{Project, ProjectEntryId, search::SearchQuery};
use schemars::JsonSchema;
use serde::Deserialize;
use settings::{
    SeedQuerySetting, Settings, SettingsStore, TerminalBell, TerminalBlink, WorkingDirectory,
};
use std::{
    any::Any,
    cmp,
    ops::Range as StdRange,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use task::TaskId;
use terminal::{
    Clear, Copy, Event, HoveredWord, MaybeNavigationTarget, Modes, MouseInputMode, Paste,
    PasteText, Point, Range, ScrollLineDown, ScrollLineUp, ScrollPageDown, ScrollPageUp,
    ScrollToBottom, ScrollToTop, Search, ShowCharacterPalette, TaskState, TaskStatus, Terminal,
    TerminalBounds, ToggleViMode,
    terminal_settings::{CursorShape, TerminalSettings},
};
use terminal_element::TerminalElement;
use terminal_panel::TerminalPanel;
use terminal_path_like_target::{hover_path_like_target, open_path_like_target};
use terminal_scrollbar::TerminalScrollHandle;
use ui::{
    ContextMenu, Divider, ScrollAxes, Scrollbars, Tooltip, WithScrollbar,
    prelude::*,
    scrollbars::{self, ScrollbarVisibility},
};
use util::ResultExt;
use workspace::{
    CloseActiveItem, DraggedSelection, DraggedTab, NewCenterTerminal, NewTerminal, Pane,
    ToolbarItemLocation, Workspace, WorkspaceId, delete_unloaded_items,
    item::{
        HighlightedText, Item, ItemEvent, SerializableItem, TabContentParams, TabTooltipContent,
    },
    register_serializable_item,
    searchable::{
        Direction, SearchEvent, SearchOptions, SearchToken, SearchableItem, SearchableItemHandle,
    },
};

struct ImeState {
    marked_text: String,
}

fn viewport_line_for_point(point: Point, display_offset: usize) -> Option<usize> {
    let display_offset = i32::try_from(display_offset).unwrap_or(i32::MAX);
    let line = point.line.saturating_add(display_offset);
    if line < 0 {
        None
    } else {
        usize::try_from(line).ok()
    }
}

const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(500);

/// Event to transmit the scroll from the element to the view
#[derive(Clone, Debug, PartialEq)]
pub struct ScrollTerminal(pub i32);

/// Sends the specified text directly to the terminal.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = terminal)]
pub struct SendText(String);

/// Sends a keystroke sequence to the terminal.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = terminal)]
pub struct SendKeystroke(String);

actions!(
    terminal,
    [
        /// Reruns the last executed task in the terminal.
        RerunTask,
    ]
);

/// Renames the terminal tab.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = terminal)]
pub struct RenameTerminal;

pub fn init(cx: &mut App) {
    terminal_panel::init(cx);

    register_serializable_item::<TerminalView>(cx);

    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(TerminalView::deploy);
    })
    .detach();
}

pub struct BlockProperties {
    pub height: u8,
    pub render: Box<dyn Send + Fn(&mut BlockContext) -> AnyElement>,
}

pub struct BlockContext<'a, 'b> {
    pub window: &'a mut Window,
    pub context: &'b mut App,
    pub dimensions: TerminalBounds,
}

///A terminal view, maintains the PTY's file handles and communicates with the terminal
pub struct TerminalView {
    terminal: Entity<Terminal>,
    workspace: WeakEntity<Workspace>,
    project: WeakEntity<Project>,
    focus_handle: FocusHandle,
    //Currently using iTerm bell, show bell emoji in tab until input is received
    has_bell: bool,
    context_menu: Option<(Entity<ContextMenu>, GpuiPoint<Pixels>, Subscription)>,
    cursor_shape: CursorShape,
    blink_manager: Entity<BlinkManager>,
    mode: TerminalMode,
    /// Corner radii for the terminal's background, for when it's embedded in a
    /// container with rounded corners.
    ///
    /// GPUI can't clip children to rounded corners, so without this the square
    /// background paints over the container's corners. Only the background is
    /// rounded, terminal content isn't clipped.
    background_corner_radii: Option<Corners<Rems>>,
    content_top_padding: Rems,
    read_only: bool,
    // Explicit override for whether workspace-specific context menu actions are shown.
    // When `None`, visibility is derived from `mode` (hidden for embedded terminals).
    show_workspace_actions: Option<bool>,
    blinking_terminal_enabled: bool,
    needs_serialize: bool,
    custom_title: Option<String>,
    hover: Option<HoverTarget>,
    hover_tooltip_update: Task<()>,
    workspace_id: Option<WorkspaceId>,
    show_breadcrumbs: bool,
    block_below_cursor: Option<Rc<BlockProperties>>,
    scroll_top: Pixels,
    scroll_handle: TerminalScrollHandle,
    ime_state: Option<ImeState>,
    self_handle: WeakEntity<Self>,
    rename_editor: Option<Entity<Editor>>,
    rename_editor_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
    _terminal_subscriptions: Vec<Subscription>,
}

#[derive(Default, Clone)]
pub enum TerminalMode {
    #[default]
    Standalone,
    Embedded {
        max_lines_when_unfocused: Option<usize>,
    },
}

#[derive(Clone)]
pub enum ContentMode {
    Scrollable,
    Inline {
        displayed_lines: usize,
        total_lines: usize,
    },
}

impl ContentMode {
    pub fn is_limited(&self) -> bool {
        match self {
            ContentMode::Scrollable => false,
            ContentMode::Inline {
                displayed_lines,
                total_lines,
            } => displayed_lines < total_lines,
        }
    }

    pub fn is_scrollable(&self) -> bool {
        matches!(self, ContentMode::Scrollable)
    }
}

#[derive(Debug)]
#[cfg_attr(test, derive(Clone, Eq, PartialEq))]
struct HoverTarget {
    tooltip: String,
    hovered_word: HoveredWord,
}

impl EventEmitter<Event> for TerminalView {}
impl EventEmitter<ItemEvent> for TerminalView {}
impl EventEmitter<SearchEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TerminalView {
    ///Create a new Terminal in the current working directory or the user's home directory
    pub fn deploy(
        workspace: &mut Workspace,
        action: &NewCenterTerminal,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let working_directory = default_working_directory(workspace, cx);
        TerminalPanel::add_center_terminal(workspace, window, cx, move |project, cx| {
            project.create_local_terminal(cx)
        })
        .detach_and_log_err(cx);
    }

    pub fn new(
        terminal: Entity<Terminal>,
        workspace: WeakEntity<Workspace>,
        workspace_id: Option<WorkspaceId>,
        project: WeakEntity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspace_handle = workspace.clone();
        let terminal_subscriptions =
            subscribe_for_terminal_events(&terminal, workspace, window, cx);

        let focus_handle = cx.focus_handle();
        let focus_in = cx.on_focus_in(&focus_handle, window, |terminal_view, window, cx| {
            terminal_view.focus_in(window, cx);
        });
        let focus_out = cx.on_focus_out(
            &focus_handle,
            window,
            |terminal_view, _event, window, cx| {
                terminal_view.focus_out(window, cx);
            },
        );
        let cursor_shape = TerminalSettings::get_global(cx).cursor_shape;

        let scroll_handle = TerminalScrollHandle::new(terminal.read(cx));

        let blink_manager = cx.new(|cx| {
            BlinkManager::new(
                CURSOR_BLINK_INTERVAL,
                |cx| {
                    !matches!(
                        TerminalSettings::get_global(cx).blinking,
                        TerminalBlink::Off
                    )
                },
                cx,
            )
        });

        let subscriptions = vec![
            focus_in,
            focus_out,
            cx.observe(&blink_manager, |_, _, cx| cx.notify()),
            cx.observe_global::<SettingsStore>(Self::settings_changed),
        ];

        Self {
            terminal,
            workspace: workspace_handle,
            project,
            has_bell: false,
            focus_handle,
            context_menu: None,
            cursor_shape,
            blink_manager,
            blinking_terminal_enabled: false,
            hover: None,
            hover_tooltip_update: Task::ready(()),
            mode: TerminalMode::Standalone,
            background_corner_radii: None,
            content_top_padding: Rems::ZERO,
            read_only: false,
            show_workspace_actions: None,
            workspace_id,
            show_breadcrumbs: TerminalSettings::get_global(cx).toolbar.breadcrumbs,
            block_below_cursor: None,
            scroll_top: Pixels::ZERO,
            scroll_handle,
            needs_serialize: false,
            custom_title: None,
            ime_state: None,
            self_handle: cx.entity().downgrade(),
            rename_editor: None,
            rename_editor_subscription: None,
            _subscriptions: subscriptions,
            _terminal_subscriptions: terminal_subscriptions,
        }
    }

    /// Enable 'embedded' mode where the terminal displays the full content with an optional limit of lines.
    pub fn set_embedded_mode(
        &mut self,
        max_lines_when_unfocused: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.mode = TerminalMode::Embedded {
            max_lines_when_unfocused,
        };
        cx.notify();
    }

    /// Rounds the background without clipping terminal content to the corners.
    pub fn set_background_corner_radii(
        &mut self,
        corner_radii: Option<Corners<Rems>>,
        cx: &mut Context<Self>,
    ) {
        self.background_corner_radii = corner_radii;
        cx.notify();
    }

    pub fn set_content_top_padding(&mut self, padding: Rems, cx: &mut Context<Self>) {
        self.content_top_padding = padding;
        cx.notify();
    }

    fn content_background_corner_radii(
        &self,
        content_mode: &ContentMode,
        cx: &App,
    ) -> Corners<Rems> {
        let mut radii = self.background_corner_radii.unwrap_or_default();
        if content_mode.is_scrollable()
            && TerminalScrollbarSettingsWrapper.visibility(cx) != scrollbars::ShowScrollbar::Never
        {
            radii.top_right = Rems::ZERO;
            radii.bottom_right = Rems::ZERO;
        }
        radii
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Fixes the view's input policy at construction, without restricting producer output
    /// or local navigation.
    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    fn mouse_input_mode(&self) -> MouseInputMode {
        if self.read_only {
            MouseInputMode::LocalSelection
        } else {
            MouseInputMode::ReportToTerminal
        }
    }

    /// Explicitly override whether workspace-specific context menu actions (e.g. creating or
    /// closing terminal tabs, inline assist) are shown.
    ///
    /// This lets hosts that aren't workspace panes (such as the agent panel) hide these
    /// actions without `terminal_view` needing to know about those hosts. When never called,
    /// visibility is derived from the terminal's `mode`.
    pub fn set_show_workspace_actions(&mut self, show: bool, cx: &mut Context<Self>) {
        self.show_workspace_actions = Some(show);
        cx.notify();
    }

    fn shows_workspace_actions(&self) -> bool {
        self.show_workspace_actions
            .unwrap_or_else(|| !matches!(self.mode, TerminalMode::Embedded { .. }))
    }

    const MAX_EMBEDDED_LINES: usize = 1_000;

    /// Returns the current `ContentMode` depending on the set `TerminalMode` and the current number of lines
    ///
    /// Note: Even in embedded mode, the terminal will fallback to scrollable when its content exceeds `MAX_EMBEDDED_LINES`
    pub fn content_mode(&self, window: &Window, cx: &App) -> ContentMode {
        match &self.mode {
            TerminalMode::Standalone => ContentMode::Scrollable,
            TerminalMode::Embedded {
                max_lines_when_unfocused,
            } => {
                let terminal = self.terminal.read(cx);
                let total_lines = terminal.total_lines();

                if total_lines > Self::MAX_EMBEDDED_LINES {
                    ContentMode::Scrollable
                } else {
                    let mut displayed_lines = terminal.used_lines().min(total_lines);

                    if !self.focus_handle.is_focused(window)
                        && let Some(max_lines) = max_lines_when_unfocused
                    {
                        displayed_lines = displayed_lines.min(*max_lines)
                    }

                    ContentMode::Inline {
                        displayed_lines,
                        total_lines,
                    }
                }
            }
        }
    }

    /// Sets the marked (pre-edit) text from the IME.
    pub(crate) fn set_marked_text(&mut self, text: String, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        if text.is_empty() {
            return self.clear_marked_text(cx);
        }
        self.ime_state = Some(ImeState { marked_text: text });
        cx.notify();
    }

    /// Gets the current marked range (UTF-16).
    pub(crate) fn marked_text_range(&self) -> Option<StdRange<usize>> {
        self.ime_state
            .as_ref()
            .map(|state| 0..state.marked_text.encode_utf16().count())
    }

    /// Clears the marked (pre-edit) text state.
    pub(crate) fn clear_marked_text(&mut self, cx: &mut Context<Self>) {
        if self.ime_state.is_some() {
            self.ime_state = None;
            cx.notify();
        }
    }

    /// Commits (sends) the given text to the PTY. Called by InputHandler::replace_text_in_range.
    pub(crate) fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if !self.read_only && !text.is_empty() {
            self.terminal.update(cx, |term, _| {
                term.input(text.to_string().into_bytes());
            });
        }
    }

    pub(crate) fn terminal_bounds(&self, cx: &App) -> TerminalBounds {
        self.terminal.read(cx).last_content().terminal_bounds
    }

    pub fn entity(&self) -> &Entity<Terminal> {
        &self.terminal
    }

    pub fn has_bell(&self) -> bool {
        self.has_bell
    }

    pub fn custom_title(&self) -> Option<&str> {
        self.custom_title.as_deref()
    }

    pub fn set_custom_title(&mut self, label: Option<String>, cx: &mut Context<Self>) {
        let label = label.filter(|l| !l.trim().is_empty());
        if self.custom_title != label {
            self.custom_title = label;
            self.needs_serialize = true;
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        }
    }

    pub(crate) fn mark_needs_serialize(&mut self, cx: &mut Context<Self>) {
        self.needs_serialize = true;
        cx.emit(ItemEvent::UpdateTab);
    }

    pub fn is_renaming(&self) -> bool {
        self.rename_editor.is_some()
    }

    pub fn rename_editor_is_focused(&self, window: &Window, cx: &App) -> bool {
        self.rename_editor
            .as_ref()
            .is_some_and(|editor| editor.focus_handle(cx).is_focused(window))
    }

    fn finish_renaming(&mut self, save: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.rename_editor.take() else {
            return;
        };
        self.rename_editor_subscription = None;
        if save {
            let new_label = editor.read(cx).text(cx).trim().to_string();
            let label = if new_label.is_empty() {
                None
            } else {
                // Only set custom_title if the text differs from the terminal's dynamic title.
                // This prevents subtle layout changes when clicking away without making changes.
                let terminal_title = self.terminal.read(cx).title(true);
                if new_label == terminal_title {
                    None
                } else {
                    Some(new_label)
                }
            };
            self.set_custom_title(label, cx);
        }
        cx.notify();
        self.focus_handle.focus(window, cx);
    }

    pub fn rename_terminal(
        &mut self,
        _: &RenameTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.terminal.read(cx).task().is_some() {
            return;
        }

        let current_label = self
            .custom_title
            .clone()
            .unwrap_or_else(|| self.terminal.read(cx).title(true));

        let rename_editor = cx.new(|cx| Editor::single_line(window, cx));
        let rename_editor_subscription = cx.subscribe_in(&rename_editor, window, {
            let rename_editor = rename_editor.clone();
            move |_this, _, event, window, cx| {
                if let editor::EditorEvent::Blurred = event {
                    // Defer to let focus settle (avoids canceling during double-click).
                    let rename_editor = rename_editor.clone();
                    cx.defer_in(window, move |this, window, cx| {
                        let still_current = this
                            .rename_editor
                            .as_ref()
                            .is_some_and(|current| current == &rename_editor);
                        if still_current && !rename_editor.focus_handle(cx).is_focused(window) {
                            this.finish_renaming(false, window, cx);
                        }
                    });
                }
            }
        });

        self.rename_editor = Some(rename_editor.clone());
        self.rename_editor_subscription = Some(rename_editor_subscription);

        rename_editor.update(cx, |editor, cx| {
            editor.set_text(current_label, window, cx);
            editor.select_all(&SelectAll, window, cx);
            editor.focus_handle(cx).focus(window, cx);
        });
        cx.notify();
    }

    pub fn clear_bell(&mut self, cx: &mut Context<TerminalView>) {
        self.has_bell = false;
        cx.emit(Event::Wakeup);
    }

    pub fn deploy_context_menu(
        &mut self,
        position: GpuiPoint<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let context_menu = ContextMenu::build(window, cx, |menu, _, _| {
            menu.context(self.focus_handle.clone())
                .when(self.shows_workspace_actions(), |menu| {
                    menu.action("New Terminal", Box::new(NewTerminal::default()))
                        .action(
                            "New Center Terminal",
                            Box::new(NewCenterTerminal::default()),
                        )
                        .separator()
                })
                .action("Copy", Box::new(Copy))
                .when(
                    !self.read_only && !matches!(self.mode, TerminalMode::Embedded { .. }),
                    |menu| {
                        menu.action("Paste", Box::new(Paste))
                            .action("Paste Text", Box::new(PasteText))
                    },
                )
                .action("Select All", Box::new(SelectAll))
                .when(
                    !self.read_only && !matches!(self.mode, TerminalMode::Embedded { .. }),
                    |menu| menu.action("Clear", Box::new(Clear)),
                )
                .when(self.shows_workspace_actions(), |menu| {
                    menu.separator().action(
                        "Close Terminal Tab",
                        Box::new(CloseActiveItem {
                            save_intent: None,
                            close_pinned: true,
                        }),
                    )
                })
        });

        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription = cx.subscribe_in(
            &context_menu,
            window,
            |this, _, _: &DismissEvent, window, cx| {
                if this.context_menu.as_ref().is_some_and(|context_menu| {
                    context_menu.0.focus_handle(cx).contains_focused(window, cx)
                }) {
                    cx.focus_self(window);
                }
                this.context_menu.take();
                cx.notify();
            },
        );

        self.context_menu = Some((context_menu, position, subscription));
    }

    fn settings_changed(&mut self, cx: &mut Context<Self>) {
        let settings = TerminalSettings::get_global(cx);
        let breadcrumb_visibility_changed = self.show_breadcrumbs != settings.toolbar.breadcrumbs;
        self.show_breadcrumbs = settings.toolbar.breadcrumbs;

        let should_blink = match settings.blinking {
            TerminalBlink::Off => false,
            TerminalBlink::On => true,
            TerminalBlink::TerminalControlled => self.blinking_terminal_enabled,
        };
        let new_cursor_shape = settings.cursor_shape;
        let old_cursor_shape = self.cursor_shape;
        if old_cursor_shape != new_cursor_shape {
            self.cursor_shape = new_cursor_shape;
            self.terminal.update(cx, |term, _| {
                term.set_cursor_shape(self.cursor_shape);
            });
        }

        self.blink_manager.update(
            cx,
            if should_blink {
                BlinkManager::enable
            } else {
                BlinkManager::disable
            },
        );

        if breadcrumb_visibility_changed {
            cx.emit(ItemEvent::UpdateBreadcrumbs);
        }
        cx.notify();
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only {
            return;
        }
        if self
            .terminal
            .read(cx)
            .last_content
            .mode
            .contains(Modes::ALT_SCREEN)
        {
            self.terminal.update(cx, |term, cx| {
                term.try_keystroke(
                    &Keystroke::parse("ctrl-cmd-space").unwrap(),
                    TerminalSettings::get_global(cx).option_as_meta,
                )
            });
        } else {
            window.show_character_palette();
        }
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |term, _| term.select_all());
        cx.notify();
    }

    fn rerun_task(&mut self, _: &RerunTask, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let task = self
            .terminal
            .read(cx)
            .task()
            .map(|task| terminal_rerun_override(&task.spawned_task.id))
            .unwrap_or_default();
        window.dispatch_action(Box::new(task), cx);
    }

    fn clear(&mut self, _: &Clear, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        self.scroll_top = px(0.);
        self.terminal.update(cx, |term, _| term.clear());
        cx.notify();
    }

    fn max_scroll_top(&self, cx: &App) -> Pixels {
        let terminal = self.terminal.read(cx);

        let Some(block) = self.block_below_cursor.as_ref() else {
            return Pixels::ZERO;
        };

        let line_height = terminal.last_content().terminal_bounds.line_height;
        let viewport_lines = terminal.viewport_lines();
        let cursor_line = viewport_line_for_point(
            terminal.last_content.cursor.point,
            terminal.last_content.display_offset,
        )
        .unwrap_or_default();
        let max_scroll_top_in_lines =
            (block.height as usize).saturating_sub(viewport_lines.saturating_sub(cursor_line + 1));

        max_scroll_top_in_lines as f32 * line_height
    }

    fn scroll_wheel(&mut self, event: &ScrollEvent, cx: &mut Context<Self>) {
        let terminal_content = self.terminal.read(cx).last_content();

        if self.block_below_cursor.is_some() && terminal_content.display_offset == 0 {
            let line_height = terminal_content.terminal_bounds.line_height;
            let y_delta = event.delta.pixel_delta(line_height).y;
            if y_delta < Pixels::ZERO || self.scroll_top > Pixels::ZERO {
                self.scroll_top = cmp::max(
                    Pixels::ZERO,
                    cmp::min(self.scroll_top - y_delta, self.max_scroll_top(cx)),
                );
                cx.notify();
                return;
            }
        }
        self.terminal.update(cx, |term, cx| {
            term.scroll_wheel(
                event,
                TerminalSettings::get_global(cx).scroll_multiplier.max(0.01),
                self.mouse_input_mode(),
            )
        });
    }

    fn is_alt_screen(&self, cx: &App) -> bool {
        self.terminal
            .read(cx)
            .last_content
            .mode
            .contains(Modes::ALT_SCREEN)
    }

    fn scroll_line_up(&mut self, _: &ScrollLineUp, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        let terminal_content = self.terminal.read(cx).last_content();
        if self.block_below_cursor.is_some()
            && terminal_content.display_offset == 0
            && self.scroll_top > Pixels::ZERO
        {
            let line_height = terminal_content.terminal_bounds.line_height;
            self.scroll_top = cmp::max(self.scroll_top - line_height, Pixels::ZERO);
            return;
        }

        self.terminal.update(cx, |term, _| term.scroll_line_up());
        cx.notify();
    }

    fn scroll_line_down(&mut self, _: &ScrollLineDown, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        let terminal_content = self.terminal.read(cx).last_content();
        if self.block_below_cursor.is_some() && terminal_content.display_offset == 0 {
            let max_scroll_top = self.max_scroll_top(cx);
            if self.scroll_top < max_scroll_top {
                let line_height = terminal_content.terminal_bounds.line_height;
                self.scroll_top = cmp::min(self.scroll_top + line_height, max_scroll_top);
            }
            return;
        }

        self.terminal.update(cx, |term, _| term.scroll_line_down());
        cx.notify();
    }

    fn scroll_page_up(&mut self, _: &ScrollPageUp, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        if self.scroll_top == Pixels::ZERO {
            self.terminal.update(cx, |term, _| term.scroll_page_up());
        } else {
            let line_height = self
                .terminal
                .read(cx)
                .last_content
                .terminal_bounds
                .line_height();
            let visible_block_lines = (self.scroll_top / line_height) as usize;
            let viewport_lines = self.terminal.read(cx).viewport_lines();
            let visible_content_lines = viewport_lines - visible_block_lines;

            if visible_block_lines >= viewport_lines {
                self.scroll_top = ((visible_block_lines - viewport_lines) as f32) * line_height;
            } else {
                self.scroll_top = px(0.);
                self.terminal
                    .update(cx, |term, _| term.scroll_up_by(visible_content_lines));
            }
        }
        cx.notify();
    }

    fn scroll_page_down(&mut self, _: &ScrollPageDown, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        self.terminal.update(cx, |term, _| term.scroll_page_down());
        let terminal = self.terminal.read(cx);
        if terminal.last_content().display_offset < terminal.viewport_lines() {
            self.scroll_top = self.max_scroll_top(cx);
        }
        cx.notify();
    }

    fn scroll_to_top(&mut self, _: &ScrollToTop, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        self.terminal.update(cx, |term, _| term.scroll_to_top());
        cx.notify();
    }

    fn scroll_to_bottom(&mut self, _: &ScrollToBottom, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        self.terminal.update(cx, |term, _| term.scroll_to_bottom());
        if self.block_below_cursor.is_some() {
            self.scroll_top = self.max_scroll_top(cx);
        }
        cx.notify();
    }

    pub fn should_show_cursor(&self, focused: bool, cx: &mut Context<Self>) -> bool {
        // Hide cursor when in embedded mode and not focused (read-only output like Agent panel)
        if let TerminalMode::Embedded { .. } = &self.mode {
            if !focused {
                return false;
            }
        }

        // For Standalone mode: always show cursor when not focused or in special modes
        if !focused
            || self
                .terminal
                .read(cx)
                .last_content
                .mode
                .contains(Modes::ALT_SCREEN)
        {
            return true;
        }

        // When focused, check blinking settings and blink manager state
        match TerminalSettings::get_global(cx).blinking {
            TerminalBlink::Off => true,
            TerminalBlink::TerminalControlled => {
                !self.blinking_terminal_enabled || self.blink_manager.read(cx).visible()
            }
            TerminalBlink::On => self.blink_manager.read(cx).visible(),
        }
    }

    pub fn pause_cursor_blinking(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.blink_manager.update(cx, BlinkManager::pause_blinking);
    }

    pub fn terminal(&self) -> &Entity<Terminal> {
        &self.terminal
    }

    pub fn set_block_below_cursor(
        &mut self,
        block: BlockProperties,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.block_below_cursor = Some(Rc::new(block));
        self.scroll_to_bottom(&ScrollToBottom, window, cx);
        cx.notify();
    }

    pub fn clear_block_below_cursor(&mut self, cx: &mut Context<Self>) {
        self.block_below_cursor = None;
        self.scroll_top = Pixels::ZERO;
        cx.notify();
    }

    ///Attempt to paste the clipboard into the terminal
    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |term, _| term.copy(None));
        cx.notify();
    }

    /// Specific handler for the [`editor::actions::Copy`] action in order for
    /// the `Edit > Copy` menu item to not be disabled, as the app expects a
    /// handler for this action in order to enable/disable the menu item.
    fn editor_copy(
        &mut self,
        _: &editor::actions::Copy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.copy(&Copy, window, cx);
    }

    ///Attempt to paste the clipboard into the terminal
    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };

        match clipboard.entries().first() {
            Some(ClipboardEntry::Image(image)) if !image.bytes.is_empty() => {
                self.forward_ctrl_v(cx);
            }
            Some(ClipboardEntry::ExternalPaths(paths)) => {
                self.add_paths_to_terminal(paths.paths(), window, cx);
            }
            _ => {
                if let Some(text) = clipboard.text() {
                    self.terminal
                        .update(cx, |terminal, _cx| terminal.paste(&text));
                }
            }
        }
    }

    /// Specific handler for the [`editor::actions::Paste`] action in order for
    /// the `Edit > Paste` menu item to not be disabled, as the app expects a
    /// handler for this action in order to enable/disable the menu item.
    fn editor_paste(
        &mut self,
        _: &editor::actions::Paste,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.paste(&Paste, window, cx);
    }

    ///Attempt to paste the clipboard text into the terminal
    fn paste_text(&mut self, _: &PasteText, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };

        if let Some(text) = clipboard.text() {
            self.terminal
                .update(cx, |terminal, _cx| terminal.paste(&text));
        }
    }

    /// Emits a raw Ctrl+V so TUI agents can read the OS clipboard directly
    /// and attach images using their native workflows.
    fn forward_ctrl_v(&self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |term, _| {
            term.input(vec![0x16]);
        });
    }

    pub fn add_paths_to_terminal(&self, paths: &[PathBuf], window: &mut Window, cx: &mut App) {
        if self.read_only {
            return;
        }
        let mut text = paths
            .iter()
            .filter_map(|path| Some(format!(" {}", shlex::try_quote(path.to_str()?).ok()?)))
            .collect::<String>();
        text.push(' ');
        window.focus(&self.focus_handle(cx), cx);
        self.terminal.update(cx, |terminal, _| {
            terminal.paste(&text);
        });
    }

    fn send_text(&mut self, text: &SendText, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        self.clear_bell(cx);
        self.blink_manager.update(cx, BlinkManager::pause_blinking);
        self.terminal.update(cx, |term, _| {
            term.input(text.0.to_string().into_bytes());
        });
    }

    fn send_keystroke(&mut self, text: &SendKeystroke, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(keystroke) = Keystroke::parse_with_os(&text.0, cx.platform_info().os).log_err()
        {
            self.clear_bell(cx);
            self.blink_manager.update(cx, BlinkManager::pause_blinking);
            self.process_keystroke(&keystroke, cx);
        }
    }

    fn dispatch_context(&self, cx: &App) -> KeyContext {
        let mut dispatch_context = KeyContext::new_with_defaults(cx.platform_info().os);
        dispatch_context.add("Terminal");

        let mode = self.terminal.read(cx).last_content.mode;
        dispatch_context.set(
            "screen",
            if mode.contains(Modes::ALT_SCREEN) {
                "alt"
            } else {
                "normal"
            },
        );

        if mode.contains(Modes::APP_CURSOR) {
            dispatch_context.add("DECCKM");
        }
        if mode.contains(Modes::APP_KEYPAD) {
            dispatch_context.add("DECPAM");
        } else {
            dispatch_context.add("DECPNM");
        }
        if mode.contains(Modes::SHOW_CURSOR) {
            dispatch_context.add("DECTCEM");
        }
        if mode.contains(Modes::LINE_WRAP) {
            dispatch_context.add("DECAWM");
        }
        if mode.contains(Modes::ORIGIN) {
            dispatch_context.add("DECOM");
        }
        if mode.contains(Modes::INSERT) {
            dispatch_context.add("IRM");
        }
        //LNM is apparently the name for this. https://vt100.net/docs/vt510-rm/LNM.html
        if mode.contains(Modes::LINE_FEED_NEW_LINE) {
            dispatch_context.add("LNM");
        }
        if mode.contains(Modes::FOCUS_IN_OUT) {
            dispatch_context.add("report_focus");
        }
        if mode.contains(Modes::ALTERNATE_SCROLL) {
            dispatch_context.add("alternate_scroll");
        }
        if mode.contains(Modes::BRACKETED_PASTE) {
            dispatch_context.add("bracketed_paste");
        }
        if mode.intersects(Modes::MOUSE_MODE) {
            dispatch_context.add("any_mouse_reporting");
        }
        {
            let mouse_reporting = if mode.contains(Modes::MOUSE_REPORT_CLICK) {
                "click"
            } else if mode.contains(Modes::MOUSE_DRAG) {
                "drag"
            } else if mode.contains(Modes::MOUSE_MOTION) {
                "motion"
            } else {
                "off"
            };
            dispatch_context.set("mouse_reporting", mouse_reporting);
        }
        {
            let format = if mode.contains(Modes::SGR_MOUSE) {
                "sgr"
            } else if mode.contains(Modes::UTF8_MOUSE) {
                "utf8"
            } else {
                "normal"
            };
            dispatch_context.set("mouse_format", format);
        };

        if self.terminal.read(cx).last_content.selection.is_some() {
            dispatch_context.add("selection");
        }

        dispatch_context
    }

    fn set_terminal(
        &mut self,
        terminal: Entity<Terminal>,
        window: &mut Window,
        cx: &mut Context<TerminalView>,
    ) {
        self._terminal_subscriptions =
            subscribe_for_terminal_events(&terminal, self.workspace.clone(), window, cx);
        self.terminal = terminal;
    }

    fn rerun_button(&self, task: &TaskState) -> Option<IconButton> {
        if self.read_only || !task.spawned_task.show_rerun {
            return None;
        }

        let task_id = task.spawned_task.id.clone();
        Some(
            IconButton::new("rerun-icon", IconName::Rerun)
                .icon_size(IconSize::Small)
                .size(ButtonSize::Compact)
                .icon_color(Color::Default)
                .shape(ui::IconButtonShape::Square)
                .tooltip(move |_window, cx| Tooltip::for_action("Rerun task", &RerunTask, cx))
                .on_click(move |_, window, cx| {
                    window.dispatch_action(Box::new(terminal_rerun_override(&task_id)), cx);
                }),
        )
    }
}

fn terminal_rerun_override(task: &TaskId) -> zed_actions::Rerun {
    zed_actions::Rerun {
        task_id: Some(task.0.clone()),
        allow_concurrent_runs: Some(true),
        use_new_terminal: Some(false),
        reevaluate_context: false,
    }
}

fn subscribe_for_terminal_events(
    terminal: &Entity<Terminal>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut Context<TerminalView>,
) -> Vec<Subscription> {
    let terminal_subscription = cx.observe(terminal, |_, _, cx| cx.notify());
    let mut previous_cwd = None;
    let terminal_events_subscription = cx.subscribe_in(
        terminal,
        window,
        move |terminal_view, terminal, event, window, cx| {
            let current_cwd = terminal.read(cx).working_directory();
            if current_cwd != previous_cwd {
                previous_cwd = current_cwd;
                terminal_view.needs_serialize = true;
            }

            match event {
                Event::Wakeup => {
                    cx.notify();
                    window.invalidate_character_coordinates();
                    cx.emit(Event::Wakeup);
                    cx.emit(ItemEvent::UpdateTab);
                    cx.emit(SearchEvent::MatchesInvalidated);
                }

                Event::OutputReplaced => {
                    terminal_view.hover = None;
                    terminal_view.hover_tooltip_update = Task::ready(());
                    terminal_view.scroll_top = Pixels::ZERO;
                    terminal_view.scroll_handle.future_display_offset.set(None);
                    terminal_view.scroll_handle.update(terminal.read(cx));
                    terminal_view.block_below_cursor = None;
                    window.invalidate_character_coordinates();
                    cx.emit(SearchEvent::MatchesInvalidated);
                    cx.emit(SearchEvent::ActiveMatchChanged);
                    cx.emit(ItemEvent::UpdateTab);
                    cx.notify();
                }

                Event::Bell => {
                    terminal_view.has_bell = true;
                    if let TerminalBell::System = TerminalSettings::get_global(cx).bell {
                        window.play_system_bell();
                    }
                    cx.emit(Event::Wakeup);
                }

                Event::BlinkChanged(blinking) => {
                    terminal_view.blinking_terminal_enabled = *blinking;

                    // If in terminal-controlled mode and focused, update blink manager
                    if matches!(
                        TerminalSettings::get_global(cx).blinking,
                        TerminalBlink::TerminalControlled
                    ) && terminal_view.focus_handle.is_focused(window)
                    {
                        terminal_view.blink_manager.update(cx, |manager, cx| {
                            if *blinking {
                                manager.enable(cx);
                            } else {
                                manager.disable(cx);
                            }
                        });
                    }
                }

                Event::TitleChanged => {
                    cx.emit(ItemEvent::UpdateTab);
                }

                Event::NewNavigationTarget(maybe_navigation_target) => {
                    match maybe_navigation_target
                        .as_ref()
                        .zip(terminal.read(cx).last_content.last_hovered_word.as_ref())
                    {
                        Some((MaybeNavigationTarget::Url(url), hovered_word)) => {
                            if Some(hovered_word)
                                != terminal_view
                                    .hover
                                    .as_ref()
                                    .map(|hover| &hover.hovered_word)
                            {
                                terminal_view.hover = Some(HoverTarget {
                                    tooltip: url.clone(),
                                    hovered_word: hovered_word.clone(),
                                });
                                terminal_view.hover_tooltip_update = Task::ready(());
                                cx.notify();
                            }
                        }
                        Some((MaybeNavigationTarget::PathLike(path_like_target), hovered_word)) => {
                            if Some(hovered_word)
                                != terminal_view
                                    .hover
                                    .as_ref()
                                    .map(|hover| &hover.hovered_word)
                            {
                                terminal_view.hover = None;
                                terminal_view.hover_tooltip_update = hover_path_like_target(
                                    &workspace,
                                    hovered_word.clone(),
                                    path_like_target,
                                    cx,
                                );
                                cx.notify();
                            }
                        }
                        None => {
                            terminal_view.hover = None;
                            terminal_view.hover_tooltip_update = Task::ready(());
                            cx.notify();
                        }
                    }
                }

                Event::Open(maybe_navigation_target) => match maybe_navigation_target {
                    MaybeNavigationTarget::Url(url) => cx.open_url(url),
                    MaybeNavigationTarget::PathLike(path_like_target) => open_path_like_target(
                        &workspace,
                        terminal_view,
                        path_like_target,
                        window,
                        cx,
                    ),
                },
                Event::BreadcrumbsChanged => cx.emit(ItemEvent::UpdateBreadcrumbs),
                Event::CloseTerminal => cx.emit(ItemEvent::CloseItem),
                Event::SelectionsChanged => {
                    window.invalidate_character_coordinates();
                    cx.emit(SearchEvent::ActiveMatchChanged)
                }
            }
        },
    );
    vec![terminal_subscription, terminal_events_subscription]
}

fn regex_search_for_query(query: &SearchQuery) -> Option<Search> {
    let str = query.as_str();
    if query.is_regex() {
        if str == "." {
            return None;
        }
        Search::new(str)
    } else {
        Search::new(&regex::escape(str))
    }
}

#[derive(Default)]
struct TerminalScrollbarSettingsWrapper;

impl ScrollbarVisibility for TerminalScrollbarSettingsWrapper {
    fn visibility(&self, cx: &App) -> scrollbars::ShowScrollbar {
        TerminalSettings::get_global(cx)
            .scrollbar
            .show
            .map(ui_scrollbar_settings_from_raw)
            .unwrap_or_else(|| EditorSettings::get_global(cx).scrollbar.show)
    }
}

impl TerminalView {
    /// Attempts to process a keystroke in the terminal. Returns true if handled.
    ///
    /// In vi mode, explicitly triggers a re-render because vi navigation (like j/k)
    /// updates the cursor locally without sending data to the shell, so there's no
    /// shell output to automatically trigger a re-render.
    fn process_keystroke(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) -> bool {
        let handled = self.terminal.update(cx, |term, cx| {
            term.try_keystroke(keystroke, TerminalSettings::get_global(cx).option_as_meta)
        });

        handled
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_bell(cx);
        self.pause_cursor_blinking(window, cx);

        if event.prefer_character_input
            && event.keystroke.key_char.is_some()
        {
            return;
        }

        if self.process_keystroke(&event.keystroke, cx) {
            cx.stop_propagation();
        }
    }

    fn focus_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _| {
            terminal.set_cursor_shape(self.cursor_shape);
            if !self.read_only {
                terminal.focus_in();
            }
        });

        let should_blink = match TerminalSettings::get_global(cx).blinking {
            TerminalBlink::Off => false,
            TerminalBlink::On => true,
            TerminalBlink::TerminalControlled => self.blinking_terminal_enabled,
        };

        if should_blink {
            self.blink_manager.update(cx, BlinkManager::enable);
        }

        window.invalidate_character_coordinates();
        cx.notify();
    }

    fn focus_out(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.blink_manager.update(cx, BlinkManager::disable);
        self.terminal.update(cx, |terminal, _| {
            if !self.read_only {
                terminal.focus_out();
            }
            terminal.set_cursor_shape(CursorShape::Hollow);
        });
        cx.notify();
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // TODO: this should be moved out of render
        self.scroll_handle.update(self.terminal.read(cx));

        if let Some(new_display_offset) = self.scroll_handle.future_display_offset.take() {
            self.terminal.update(cx, |term, _| {
                let delta = new_display_offset as i32 - term.last_content.display_offset as i32;
                match delta.cmp(&0) {
                    cmp::Ordering::Greater => term.scroll_up_by(delta as usize),
                    cmp::Ordering::Less => term.scroll_down_by(-delta as usize),
                    cmp::Ordering::Equal => {}
                }
            });
        }

        let terminal_handle = self.terminal.clone();
        let terminal_view_handle = cx.entity();

        let focused = self.focus_handle.is_focused(window);
        let content_mode = self.content_mode(window, cx);
        let content_corner_radii = self.content_background_corner_radii(&content_mode, cx);

        div()
            .id("terminal-view")
            .size_full()
            .relative()
            .track_focus(&self.focus_handle(cx))
            .key_context(self.dispatch_context(cx))
            .on_action(cx.listener(TerminalView::send_text))
            .on_action(cx.listener(TerminalView::send_keystroke))
            .on_action(cx.listener(TerminalView::copy))
            .on_action(cx.listener(TerminalView::editor_copy))
            .on_action(cx.listener(TerminalView::paste))
            .on_action(cx.listener(TerminalView::editor_paste))
            .on_action(cx.listener(TerminalView::paste_text))
            .on_action(cx.listener(TerminalView::clear))
            .on_action(cx.listener(TerminalView::scroll_line_up))
            .on_action(cx.listener(TerminalView::scroll_line_down))
            .on_action(cx.listener(TerminalView::scroll_page_up))
            .on_action(cx.listener(TerminalView::scroll_page_down))
            .on_action(cx.listener(TerminalView::scroll_to_top))
            .on_action(cx.listener(TerminalView::scroll_to_bottom))
            .on_action(cx.listener(TerminalView::show_character_palette))
            .on_action(cx.listener(TerminalView::select_all))
            .on_action(cx.listener(TerminalView::rerun_task))
            .on_action(cx.listener(TerminalView::rename_terminal))
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    if !this
                        .terminal
                        .read(cx)
                        .mouse_mode(event.modifiers.shift, this.mouse_input_mode())
                    {
                        let had_selection = this.terminal.read(cx).last_content.selection.is_some();
                        if !had_selection {
                            this.terminal.update(cx, |terminal, _| {
                                terminal.select_word_at_event_position(event);
                            });
                        }
                        this.deploy_context_menu(event.position, window, cx);
                        cx.notify();
                    }
                }),
            )
            .child(
                // TODO: Oddly this wrapper div is needed for TerminalElement to not steal events from the context menu
                div()
                    .id("terminal-view-container")
                    .size_full()
                    .bg(cx.theme().colors().editor_background)
                    .when_some(self.background_corner_radii, |this, radii| {
                        this.rounded_tl(radii.top_left)
                            .rounded_tr(radii.top_right)
                            .rounded_bl(radii.bottom_left)
                            .rounded_br(radii.bottom_right)
                    })
                    .child(
                        div()
                            .size_full()
                            .pt(self.content_top_padding)
                            .bg(cx.theme().colors().terminal_background)
                            .rounded_tl(content_corner_radii.top_left)
                            .rounded_tr(content_corner_radii.top_right)
                            .rounded_bl(content_corner_radii.bottom_left)
                            .rounded_br(content_corner_radii.bottom_right)
                            .child(TerminalElement::new(
                                terminal_handle,
                                terminal_view_handle,
                                self.workspace.clone(),
                                self.focus_handle.clone(),
                                focused,
                                self.should_show_cursor(focused, cx),
                                self.block_below_cursor.clone(),
                                self.mode.clone(),
                            )),
                    )
                    .when(content_mode.is_scrollable(), |div| {
                        let colors = cx.theme().colors();
                        div.custom_scrollbars(
                            Scrollbars::for_settings::<TerminalScrollbarSettingsWrapper>()
                                .show_along(ScrollAxes::Vertical)
                                .with_stable_track_along(
                                    ScrollAxes::Vertical,
                                    colors.editor_background,
                                )
                                .tracked_scroll_handle(&self.scroll_handle),
                            window,
                            cx,
                        )
                    }),
            )
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

impl Item for TerminalView {
    type Event = ItemEvent;

    fn tab_tooltip_content(&self, cx: &App) -> Option<TabTooltipContent> {
        Some(TabTooltipContent::Custom(Box::new(Tooltip::element({
            let terminal = self.terminal().read(cx);
            let title = terminal.title(false);
            let pid = terminal.pid_getter()?.fallback_pid();

            move |_, _| {
                v_flex()
                    .gap_1()
                    .child(Label::new(title.clone()))
                    .child(h_flex().flex_grow_1().child(Divider::horizontal()))
                    .child(
                        Label::new(format!("Process ID (PID): {}", pid))
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .into_any_element()
            }
        }))))
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let terminal = self.terminal().read(cx);
        let title = self
            .custom_title
            .as_ref()
            .filter(|title| !title.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| terminal.title(true));

        let (icon, icon_color, rerun_button) = match terminal.task() {
            Some(terminal_task) => match &terminal_task.status {
                TaskStatus::Running => (
                    IconName::PlayFilled,
                    Color::Disabled,
                    self.rerun_button(terminal_task),
                ),
                TaskStatus::Unknown => (
                    IconName::Warning,
                    Color::Warning,
                    self.rerun_button(terminal_task),
                ),
                TaskStatus::Completed { success } => {
                    let rerun_button = self.rerun_button(terminal_task);

                    if *success {
                        (IconName::Check, Color::Success, rerun_button)
                    } else {
                        (IconName::XCircle, Color::Error, rerun_button)
                    }
                }
            },
            None => (IconName::Terminal, Color::Muted, None),
        };

        let self_handle = self.self_handle.clone();
        h_flex()
            .gap_1()
            .group("term-tab-icon")
            .when(!params.selected, |this| {
                this.track_focus(&self.focus_handle)
            })
            .on_action(move |action: &RenameTerminal, window, cx| {
                self_handle
                    .update(cx, |this, cx| this.rename_terminal(action, window, cx))
                    .ok();
            })
            .child(
                h_flex()
                    .group("term-tab-icon")
                    .child(
                        div()
                            .when(rerun_button.is_some(), |this| {
                                this.hover(|style| style.invisible().w_0())
                            })
                            .child(Icon::new(icon).color(icon_color)),
                    )
                    .when_some(rerun_button, |this, rerun_button| {
                        this.child(
                            div()
                                .absolute()
                                .visible_on_hover("term-tab-icon")
                                .child(rerun_button),
                        )
                    }),
            )
            .child(
                div()
                    .relative()
                    .child(
                        Label::new(title)
                            .single_line()
                            .color(params.text_color())
                            .when(self.is_renaming(), |this| this.alpha(0.)),
                    )
                    .when_some(self.rename_editor.clone(), |this, editor| {
                        let self_handle = self.self_handle.clone();
                        let self_handle_cancel = self.self_handle.clone();
                        this.child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .size_full()
                                .child(editor)
                                .on_action(move |_: &menu::Confirm, window, cx| {
                                    self_handle
                                        .update(cx, |this, cx| {
                                            this.finish_renaming(true, window, cx)
                                        })
                                        .ok();
                                })
                                .on_action(move |_: &menu::Cancel, window, cx| {
                                    self_handle_cancel
                                        .update(cx, |this, cx| {
                                            this.finish_renaming(false, window, cx)
                                        })
                                        .ok();
                                }),
                        )
                    }),
            )
            .into_any()
    }

    fn tab_content_text(&self, detail: usize, cx: &App) -> SharedString {
        if let Some(custom_title) = self.custom_title.as_ref().filter(|l| !l.trim().is_empty()) {
            return custom_title.clone().into();
        }
        let terminal = self.terminal().read(cx);
        terminal.title(detail == 0).into()
    }

    fn handle_drop(
        &self,
        active_pane: &Pane,
        dropped: &dyn Any,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        if self.read_only {
            return false;
        }
        let Some(project) = self.project.upgrade() else {
            return false;
        };

        if let Some(paths) = dropped.downcast_ref::<ExternalPaths>() {
            let is_local = project.read(cx).is_local();
            if is_local {
                self.add_paths_to_terminal(paths.paths(), window, cx);
                return true;
            }

            return false;
        } else if let Some(tab) = dropped.downcast_ref::<DraggedTab>() {
            let Some(self_handle) = self.self_handle.upgrade() else {
                return false;
            };

            let Some(workspace) = self.workspace.upgrade() else {
                return false;
            };

            let Some(this_pane) = workspace.read(cx).pane_for(&self_handle) else {
                return false;
            };

            let item = if tab.pane == this_pane {
                active_pane.item_for_index(tab.ix)
            } else {
                tab.pane.read(cx).item_for_index(tab.ix)
            };

            let Some(item) = item else {
                return false;
            };

            if item.downcast::<TerminalView>().is_some() {
                let Some(split_direction) = active_pane.drag_split_direction() else {
                    return false;
                };

                let Some(terminal_panel) = workspace.read(cx).panel::<TerminalPanel>(cx) else {
                    return false;
                };

                if !terminal_panel.read(cx).center.panes().contains(&&this_pane) {
                    return false;
                }

                let source = tab.pane.clone();
                let item_id_to_move = item.item_id();
                let is_zoomed = {
                    let terminal_panel = terminal_panel.read(cx);
                    if terminal_panel.active_pane == this_pane {
                        active_pane.is_zoomed()
                    } else {
                        terminal_panel.active_pane.read(cx).is_zoomed()
                    }
                };

                let workspace = workspace.downgrade();
                let terminal_panel = terminal_panel.downgrade();
                // Defer the split operation to avoid re-entrancy panic.
                // The pane may be the one currently being updated, so we cannot
                // call mark_positions (via split) synchronously.
                window
                    .spawn(cx, async move |cx| {
                        cx.update(|window, cx| {
                            let Ok(new_pane) = terminal_panel.update(cx, |terminal_panel, cx| {
                                let new_pane = terminal_panel::new_terminal_pane(
                                    workspace, project, is_zoomed, window, cx,
                                );
                                terminal_panel.apply_tab_bar_buttons(&new_pane, cx);
                                terminal_panel.center.split(
                                    &this_pane,
                                    &new_pane,
                                    split_direction,
                                    cx,
                                );
                                anyhow::Ok(new_pane)
                            }) else {
                                return;
                            };

                            let Some(new_pane) = new_pane.log_err() else {
                                return;
                            };

                            workspace::move_item(
                                &source,
                                &new_pane,
                                item_id_to_move,
                                new_pane.read(cx).active_item_index(),
                                true,
                                window,
                                cx,
                            );
                        })
                        .ok();
                    })
                    .detach();

                return true;
            } else {
                if let Some(project_path) = item.project_path(cx)
                    && let Some(path) = project.read(cx).absolute_path(&project_path, cx)
                {
                    self.add_paths_to_terminal(&[path], window, cx);
                    return true;
                }
            }

            return false;
        } else if let Some(selection) = dropped.downcast_ref::<DraggedSelection>() {
            let project = project.read(cx);
            let paths = selection
                .items()
                .map(|selected_entry| selected_entry.entry_id)
                .filter_map(|entry_id| project.path_for_entry(entry_id, cx))
                .filter_map(|project_path| project.absolute_path(&project_path, cx))
                .collect::<Vec<_>>();

            if !paths.is_empty() {
                self.add_paths_to_terminal(&paths, window, cx);
            }

            return true;
        } else if let Some(&entry_id) = dropped.downcast_ref::<ProjectEntryId>() {
            let project = project.read(cx);
            if let Some(path) = project
                .path_for_entry(entry_id, cx)
                .and_then(|project_path| project.absolute_path(&project_path, cx))
            {
                self.add_paths_to_terminal(&[path], window, cx);
            }

            return true;
        }

        false
    }

    fn tab_extra_context_menu_actions(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<(SharedString, Box<dyn gpui::Action>)> {
        let terminal = self.terminal.read(cx);
        if terminal.task().is_none() {
            vec![("Rename".into(), Box::new(RenameTerminal))]
        } else {
            Vec::new()
        }
    }

    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        workspace_id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>> {
        let Ok(terminal) = self.project.update(cx, |project, cx| {
            let cwd = project
                .active_project_directory(cx)
                .map(|it| it.to_path_buf());
            project.clone_terminal(self.terminal(), cx, cwd)
        }) else {
            return Task::ready(None);
        };
        cx.spawn_in(window, async move |this, cx| {
            let terminal = terminal.await.log_err()?;
            this.update_in(cx, |this, window, cx| {
                cx.new(|cx| {
                    TerminalView::new(
                        terminal,
                        this.workspace.clone(),
                        workspace_id,
                        this.project.clone(),
                        window,
                        cx,
                    )
                })
            })
            .ok()
        })
    }

    fn is_dirty(&self, cx: &App) -> bool {
        match self.terminal.read(cx).task() {
            Some(task) => task.status == TaskStatus::Running,
            None => self.has_bell(),
        }
    }

    fn has_conflict(&self, _cx: &App) -> bool {
        false
    }

    fn can_save_as(&self, _cx: &App) -> bool {
        false
    }

    fn as_searchable(
        &self,
        handle: &Entity<Self>,
        _: &App,
    ) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(handle.clone()))
    }

    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation {
        if self.show_breadcrumbs && !self.terminal().read(cx).breadcrumb_text.trim().is_empty() {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        Some((
            vec![HighlightedText {
                text: self.terminal().read(cx).breadcrumb_text.clone().into(),
                highlights: vec![],
            }],
            None,
        ))
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.terminal().read(cx).task().is_none() {
            if let Some((new_id, old_id)) = workspace.database_id().zip(self.workspace_id) {
                log::debug!(
                    "Updating workspace id for the terminal, old: {old_id:?}, new: {new_id:?}",
                );
                let db = TerminalDb::global(cx);
                let entity_id = cx.entity_id().as_u64();
                cx.background_spawn(async move {
                    db.update_workspace_id(new_id, old_id, entity_id).await
                })
                .detach();
            }
            self.workspace_id = workspace.database_id();
        }
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl SerializableItem for TerminalView {
    fn serialized_item_kind() -> &'static str {
        "Terminal"
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<workspace::ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        let db = TerminalDb::global(cx);
        delete_unloaded_items(alive_items, workspace_id, "terminals", &db, cx)
    }

    fn serialize(
        &mut self,
        _workspace: &mut Workspace,
        item_id: workspace::ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        let terminal = self.terminal().read(cx);
        if terminal.task().is_some() {
            return None;
        }

        if !self.needs_serialize {
            return None;
        }

        let workspace_id = self.workspace_id?;
        let cwd = terminal.working_directory();
        let custom_title = self.custom_title.clone();
        self.needs_serialize = false;

        let db = TerminalDb::global(cx);
        Some(cx.background_spawn(async move {
            if let Some(cwd) = cwd {
                db.save_working_directory(item_id, workspace_id, cwd)
                    .await?;
            }
            db.save_custom_title(item_id, workspace_id, custom_title)
                .await?;
            Ok(())
        }))
    }

    fn should_serialize(&self, _: &Self::Event) -> bool {
        self.needs_serialize
    }

    fn deserialize(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        window.spawn(cx, async move |cx| {
            let (cwd, custom_title) = cx
                .update(|_window, cx| {
                    let db = TerminalDb::global(cx);
                    let from_db = db
                        .get_working_directory(item_id, workspace_id)
                        .log_err()
                        .flatten();
                    let cwd = if from_db
                        .as_ref()
                        .is_some_and(|from_db| !from_db.as_os_str().is_empty())
                    {
                        from_db
                    } else {
                        workspace
                            .upgrade()
                            .and_then(|workspace| default_working_directory(workspace.read(cx), cx))
                    };
                    let custom_title = db
                        .get_custom_title(item_id, workspace_id)
                        .log_err()
                        .flatten()
                        .filter(|title| !title.trim().is_empty());
                    (cwd, custom_title)
                })
                .ok()
                .unwrap_or((None, None));

            let terminal = project
                .update(cx, |project, cx| project.create_terminal_shell(cwd, cx))
                .await?;
            cx.update(|window, cx| {
                cx.new(|cx| {
                    let mut view = TerminalView::new(
                        terminal,
                        workspace,
                        Some(workspace_id),
                        project.downgrade(),
                        window,
                        cx,
                    );
                    if custom_title.is_some() {
                        view.custom_title = custom_title;
                    }
                    view
                })
            })
        })
    }
}

impl SearchableItem for TerminalView {
    type Match = Range;

    fn supported_options(&self) -> SearchOptions {
        SearchOptions {
            case: false,
            word: false,
            regex: true,
            replacement: false,
            selection: false,
            select_all: false,
            find_in_results: false,
        }
    }

    /// Clear stored matches
    fn clear_matches(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.terminal().update(cx, |term, _| term.matches.clear())
    }

    /// Store matches returned from find_matches somewhere for rendering
    fn update_matches(
        &mut self,
        matches: &[Self::Match],
        _active_match_index: Option<usize>,
        _token: SearchToken,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.terminal()
            .update(cx, |term, _| term.matches = matches.to_vec())
    }

    /// Returns the selection content to pre-load into this search
    fn query_suggestion(
        &mut self,
        _seed_query_override: Option<SeedQuerySetting>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> String {
        self.terminal()
            .read(cx)
            .last_content
            .selection_text
            .clone()
            .unwrap_or_default()
    }

    /// Focus match at given index into the Vec of matches
    fn activate_match(
        &mut self,
        index: usize,
        _: &[Self::Match],
        _token: SearchToken,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.terminal()
            .update(cx, |term, _| term.activate_match(index));
        cx.notify();
    }

    /// Add selections for all matches given.
    fn select_matches(
        &mut self,
        matches: &[Self::Match],
        _token: SearchToken,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.terminal()
            .update(cx, |term, _| term.select_matches(matches));
        cx.notify();
    }

    /// Get all of the matches for this query, should be done on the background
    fn find_matches(
        &mut self,
        query: Arc<SearchQuery>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Vec<Self::Match>> {
        if let Some(s) = regex_search_for_query(&query) {
            self.terminal()
                .update(cx, |term, cx| term.find_matches(s, cx))
        } else {
            Task::ready(vec![])
        }
    }

    /// Reports back to the search toolbar what the active match should be (the selection)
    fn active_match_index(
        &mut self,
        direction: Direction,
        matches: &[Self::Match],
        _token: SearchToken,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        // Selection head might have a value if there's a selection that isn't
        // associated with a match. Therefore, if there are no matches, we should
        // report None, no matter the state of the terminal

        if !matches.is_empty() {
            if let Some(selection_head) = self.terminal().read(cx).selection_head {
                // If selection head is contained in a match. Return that match
                match direction {
                    Direction::Prev => {
                        // If no selection before selection head, return the first match
                        Some(
                            matches
                                .iter()
                                .enumerate()
                                .rev()
                                .find(|(_, search_match)| {
                                    search_match.contains(selection_head)
                                        || search_match.start() < selection_head
                                })
                                .map(|(ix, _)| ix)
                                .unwrap_or(0),
                        )
                    }
                    Direction::Next => {
                        // If no selection after selection head, return the last match
                        Some(
                            matches
                                .iter()
                                .enumerate()
                                .find(|(_, search_match)| {
                                    search_match.contains(selection_head)
                                        || search_match.start() > selection_head
                                })
                                .map(|(ix, _)| ix)
                                .unwrap_or(matches.len().saturating_sub(1)),
                        )
                    }
                }
            } else {
                // Matches found but no active selection, return the first last one (closest to cursor)
                Some(matches.len().saturating_sub(1))
            }
        } else {
            None
        }
    }
    fn replace(
        &mut self,
        _: &Self::Match,
        _: &SearchQuery,
        _token: SearchToken,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) {
        // Replacement is not supported in terminal view, so this is a no-op.
    }
}

/// Gets the working directory for the given workspace, respecting the user's settings.
/// Falls back to home directory when no project directory is available.
///
/// For remote projects, local-only resolution (home dir fallback, shell expansion,
/// local `is_dir` checks) is skipped -- returning `None` lets the remote shell
/// open in the remote user's home directory by default.
pub fn default_working_directory(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    let is_remote = workspace.project().read(cx).is_remote();
    let directory = match &TerminalSettings::get_global(cx).working_directory {
        WorkingDirectory::CurrentFileDirectory => workspace
            .project()
            .read(cx)
            .active_entry_directory(cx)
            .or_else(|| current_project_directory(workspace, cx)),
        WorkingDirectory::CurrentProjectDirectory => current_project_directory(workspace, cx),
        WorkingDirectory::FirstProjectDirectory => first_project_directory(workspace, cx),
        WorkingDirectory::AlwaysHome => None,
        WorkingDirectory::Always { directory } if !is_remote => shellexpand::full(directory)
            .ok()
            .map(|dir| Path::new(&dir.to_string()).to_path_buf())
            .filter(|dir| dir.is_dir()),
        WorkingDirectory::Always { .. } => None,
    };

    if is_remote {
        directory
    } else {
        directory.or_else(dirs::home_dir)
    }
}

fn current_project_directory(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    workspace
        .project()
        .read(cx)
        .active_project_directory(cx)
        .as_deref()
        .map(Path::to_path_buf)
        .or_else(|| first_project_directory(workspace, cx))
}

///Gets the first project's home directory, or the home directory
fn first_project_directory(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    let worktree = workspace.worktrees(cx).next()?.read(cx);
    let worktree_path = worktree.abs_path();
    if worktree.root_entry()?.is_dir() {
        Some(worktree_path.to_path_buf())
    } else {
        // If worktree is a file, return its parent directory
        worktree_path.parent().map(|p| p.to_path_buf())
    }
}
