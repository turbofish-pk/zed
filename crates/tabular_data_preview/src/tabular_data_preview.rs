use editor::{Editor, EditorEvent};
use gpui::{Action, AppContext, Entity, EventEmitter, FocusHandle, Focusable, Task, actions};

use schemars::JsonSchema;
use serde::Deserialize;
use ui::{SharedString, prelude::*};
use workspace::{Item, Pane, Workspace};

use crate::parser::EditorState;

pub use crate::table_view::{CellSelection, PerformanceMetrics, TableView};

mod parser;
mod renderer;
mod settings;
mod table_data_engine;
mod table_view;
pub mod types;

actions!(tabular_data, [OpenPreview, OpenPreviewToTheSide]);

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NavigationDirection {
    #[default]
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Deserialize, PartialEq, JsonSchema, Action)]
#[action(namespace = tabular_data)]
pub struct MoveFocusedCell {
    pub direction: NavigationDirection,
}

/// Editor-backed adapter: watches an [`Editor`], parses its buffer into a [`crate::types::TableLikeContent`],
/// and feeds the result to an embedded [`TableView`] that owns all grid rendering.
pub struct TabularDataPreviewPane {
    /// The reusable tabular viewer this adapter drives.
    pub(crate) table: Entity<TableView>,
    active_editor_state: EditorState,
    pub(crate) parsing_task: Option<Task<anyhow::Result<()>>>,
    pub(crate) is_parsing: bool,
    pub(crate) parse_error: Option<SharedString>,
    /// Time when the last parsing operation ended, used for smart debouncing
    pub(crate) last_parse_end_time: Option<std::time::Instant>,
    /// Forwards the table's notifications so observers of this pane (e.g. `cx.condition` in
    /// tests, or anything watching for the mapping/list-state to settle) see them too.
    _table_subscription: gpui::Subscription,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        TabularDataPreviewPane::register(workspace);
    })
    .detach()
}

impl TabularDataPreviewPane {
    pub fn register(workspace: &mut Workspace) {
        workspace.register_action_renderer(|div, _, _, cx| {
            div.on_action(cx.listener(|workspace, _: &OpenPreview, window, cx| {
                if let Some(editor) =
                    Self::resolve_active_item_as_tabular_data_editor(workspace, cx)
                {
                    let pane = workspace.active_pane().clone();
                    Self::open_preview_in_pane(editor, pane, window, cx);
                }
            }))
            .on_action(cx.listener(
                |workspace, _: &OpenPreviewToTheSide, window, cx| {
                    if let Some(editor) =
                        Self::resolve_active_item_as_tabular_data_editor(workspace, cx)
                    {
                        let pane = workspace.active_pane().clone();
                        Self::open_preview_to_the_side_of_pane(workspace, editor, pane, window, cx);
                    }
                },
            ))
        });
    }

    pub fn open_preview_in_pane(
        editor: Entity<Editor>,
        pane: Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        Self::activate_or_add_preview(editor, pane, true, window, cx);
    }

    pub fn open_preview_to_the_side_of_pane(
        workspace: &mut Workspace,
        editor: Entity<Editor>,
        origin_pane: Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let target_pane = workspace.adjacent_pane_of(&origin_pane, window, cx);
        Self::activate_or_add_preview(editor.clone(), target_pane, false, window, cx);
        editor.focus_handle(cx).focus(window, cx);
    }

    fn activate_or_add_preview(
        editor: Entity<Editor>,
        pane: Entity<Pane>,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let existing_view_idx = Self::find_existing_preview_item_idx(pane.read(cx), &editor, cx);
        if let Some(existing_view_idx) = existing_view_idx {
            pane.update(cx, |pane, cx| {
                pane.activate_item(existing_view_idx, focus, focus, window, cx);
            });
        } else {
            let preview_pane = Self::new(&editor, window, cx);
            pane.update(cx, |pane, cx| {
                pane.add_item(Box::new(preview_pane), focus, focus, None, window, cx);
            });
        }
        cx.notify();
    }

    fn find_existing_preview_item_idx(
        pane: &Pane,
        editor: &Entity<Editor>,
        cx: &App,
    ) -> Option<usize> {
        pane.items_of_type::<TabularDataPreviewPane>()
            .find(|view| &view.read(cx).active_editor_state.editor == editor)
            .and_then(|view| pane.index_for_item(&view))
    }

    fn new(editor: &Entity<Editor>, window: &Window, cx: &mut Context<Workspace>) -> Entity<Self> {
        cx.new(|cx| {
            let subscription = cx.subscribe(
                editor,
                |this: &mut TabularDataPreviewPane, _editor, event: &EditorEvent, cx| {
                    match event {
                        EditorEvent::Edited { .. } | EditorEvent::DirtyChanged => {
                            this.parse_from_active_editor(true, cx);
                        }
                        _ => {}
                    };
                },
            );

            let table = cx.new(|cx| TableView::new(window, cx));
            let table_subscription = cx.observe(&table, |_, _, cx| cx.notify());

            let mut view = TabularDataPreviewPane {
                active_editor_state: EditorState {
                    editor: editor.clone(),
                    _subscription: subscription,
                },
                table,
                parsing_task: None,
                is_parsing: false,
                parse_error: None,
                last_parse_end_time: None,
                _table_subscription: table_subscription,
            };

            view.parse_from_active_editor(false, cx);
            view
        })
    }

    pub(crate) fn editor_state(&self) -> &EditorState {
        &self.active_editor_state
    }

    pub fn resolve_active_item_as_tabular_data_editor(
        workspace: &Workspace,
        cx: &mut Context<Workspace>,
    ) -> Option<Entity<Editor>> {
        let editor = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx))?;
        Self::is_tabular_data_file(&editor, cx).then_some(editor)
    }

    pub fn is_tabular_data_file(editor: &Entity<Editor>, cx: &App) -> bool {
        parser::TabularFormat::from_editor(editor, cx).is_some()
    }
}

impl Focusable for TabularDataPreviewPane {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.table.read(cx).focus_handle(cx)
    }
}

impl Render for TabularDataPreviewPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(match &self.parse_error {
            Some(error) => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .text_color(cx.theme().status().error)
                .child(error.clone())
                .into_any_element(),
            None => self.table.clone().into_any_element(),
        })
    }
}

impl EventEmitter<()> for TabularDataPreviewPane {}

impl Item for TabularDataPreviewPane {
    type Event = ();

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Table))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.editor_state()
            .editor
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()
            .and_then(|b| {
                let file = b.read(cx).file()?;
                let local_file = file.as_local()?;
                local_file
                    .abs_path(cx)
                    .file_name()
                    .map(|name| format!("Preview {}", name.to_string_lossy()).into())
            })
            .unwrap_or_else(|| SharedString::from("Tabular Data Preview"))
    }
}

