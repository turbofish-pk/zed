use std::{
    ops::Range,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};

use editor::{
    Editor, RowHighlightOptions, SelectionEffects,
    scroll::{Autoscroll, ScrollOffset},
};
use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    Action, AnyView, App, AsyncWindowContext, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, HighlightStyle, ParentElement, Point, Render, Styled, StyledText,
    Subscription, Task, TextRun, WeakEntity, Window,
};
use language::{
    Buffer, CodeLabel, File as _, Language, Location, Rope, ToOffset, ToPoint, lsp_to_symbol_kind,
};
use picker::{Picker, PickerDelegate};
use project::{CallHierarchyItem, LspStoreEvent, Project};
use settings::Settings;
use theme_settings::ThemeSettings;
use ui::{
    KeyBinding, ListItem, ListItemSpacing, prelude::*, tooltip_container, utils::buffer_text_style,
};
use util::{ResultExt, paths::PathExt};
use workspace::{DismissDecision, ModalView, Workspace};
pub use zed_actions::{ShowIncomingCalls, ShowOutgoingCalls, ToggleDirection};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CallHierarchyMode {
    #[default]
    Incoming,
    Outgoing,
}

impl CallHierarchyMode {
    fn opposite(self) -> Self {
        match self {
            CallHierarchyMode::Incoming => CallHierarchyMode::Outgoing,
            CallHierarchyMode::Outgoing => CallHierarchyMode::Incoming,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Call {
    pub item: CallHierarchyItem,
    pub target: Location,
    pub site_count: usize,
    pub label: Option<Arc<CodeLabel>>,
    pub display: CallDisplay,
}

#[derive(Debug, Clone, Default)]
pub struct CallDisplay {
    pub name: SharedString,
    pub detail: Option<SharedString>,
    pub path: Option<SharedString>,
    pub full_signature: SharedString,
    pub label_text: Option<SharedString>,
    pub needs_tooltip: bool,
}

const BASE_MODAL_WIDTH: Rems = Rems(42.0);

#[derive(Debug, Clone, Copy, PartialEq, settings::RegisterSetting)]
pub struct CallHierarchySettings {
    pub modal_max_width: settings::ModalWidthContent,
}

impl Settings for CallHierarchySettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let call_hierarchy = content.call_hierarchy.as_ref().unwrap();
        Self {
            modal_max_width: call_hierarchy.modal_max_width.unwrap(),
        }
    }
}

pub async fn fetch_calls(
    item: &CallHierarchyItem,
    project: &Entity<Project>,
    mode: CallHierarchyMode,
    cx: &mut AsyncWindowContext,
) -> Vec<Call> {
    let raw_calls = match mode {
        CallHierarchyMode::Incoming => {
            let task = project.update(cx, |project, cx| project.incoming_calls(item.clone(), cx));
            task.await
                .log_err()
                .flatten()
                .unwrap_or_default()
                .into_iter()
                .map(|call| (call.from, call.from_ranges))
                .collect::<Vec<_>>()
        }
        CallHierarchyMode::Outgoing => {
            let task = project.update(cx, |project, cx| project.outgoing_calls(item.clone(), cx));
            task.await
                .log_err()
                .flatten()
                .unwrap_or_default()
                .into_iter()
                .map(|call| (call.to, call.from_ranges))
                .collect::<Vec<_>>()
        }
    };

    let mut calls = cx
        .update(|_, cx| {
            raw_calls
                .into_iter()
                .map(|(item, mut sites)| {
                    sites.sort_by_key(|site| {
                        let buffer = site.buffer.read(cx);
                        site.range.start.to_offset(buffer)
                    });
                    let site_count = sites.len().max(1);
                    let target = sites
                        .into_iter()
                        .next()
                        .unwrap_or_else(|| item_location(&item));
                    Call {
                        item,
                        target,
                        site_count,
                        label: None,
                        display: CallDisplay::default(),
                    }
                })
                .collect::<Vec<_>>()
        })
        .ok()
        .unwrap_or_default();

    attach_labels(&mut calls, project, cx).await;
    cx.update(|_, cx| {
        for call in calls.iter_mut() {
            call.display = compute_call_display(call, cx);
        }
        calls.sort_by_cached_key(|call| {
            let offset = call
                .target
                .range
                .start
                .to_offset(call.target.buffer.read(cx));
            (call.display.path.clone(), offset)
        });
    })
    .ok();
    calls
}

pub fn init(cx: &mut App) {
    cx.observe_new(CallHierarchyView::register).detach();
}

pub struct CallHierarchyView {
    picker: Entity<Picker<CallHierarchyDelegate>>,
    mode: CallHierarchyMode,
    _subscriptions: [Subscription; 2],
}

impl CallHierarchyView {
    fn register(editor: &mut Editor, _: Option<&mut Window>, cx: &mut Context<Editor>) {
        if editor.mode().is_full() {
            let handle = cx.entity().downgrade();
            editor
                .register_action({
                    let handle = handle.clone();
                    move |_: &ShowIncomingCalls, window, cx| {
                        if let Some(editor) = handle.upgrade() {
                            toggle_call_hierarchy(editor, CallHierarchyMode::Incoming, window, cx);
                        }
                    }
                })
                .detach();
            editor
                .register_action(move |_: &ShowOutgoingCalls, window, cx| {
                    if let Some(editor) = handle.upgrade() {
                        toggle_call_hierarchy(editor, CallHierarchyMode::Outgoing, window, cx);
                    }
                })
                .detach();
        }
    }

    fn new(
        editor: Entity<Editor>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        mode: CallHierarchyMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> CallHierarchyView {
        let prev_scroll_position = editor.update(cx, |editor, cx| Some(editor.scroll_position(cx)));
        let project_subscription =
            cx.subscribe_in(&project, window, |view, _, event, window, cx| match event {
                project::Event::LanguageServerBufferRegistered { buffer_id, .. } => {
                    view.retry_empty_hierarchy(
                        |delegate, cx| {
                            delegate
                                .queried_buffer
                                .as_ref()
                                .is_some_and(|buffer| buffer.read(cx).remote_id() == *buffer_id)
                        },
                        window,
                        cx,
                    );
                }
                _ => {}
            });
        let lsp_store_subscription = cx.subscribe_in(
            &project.read(cx).lsp_store(),
            window,
            |view, _, event, window, cx| match event {
                LspStoreEvent::LanguageServerUpdate {
                    message: proto::update_language_server::Variant::WorkEnd(_),
                    ..
                } => {
                    view.retry_empty_hierarchy(
                        |delegate, _| delegate.queried_buffer.is_some(),
                        window,
                        cx,
                    );
                }
                _ => {}
            },
        );
        let delegate = CallHierarchyDelegate::new(
            cx.entity().downgrade(),
            editor,
            project,
            workspace,
            mode,
            prev_scroll_position,
            cx.focus_handle(),
        );
        let modal_width = CallHierarchySettings::get_global(cx)
            .modal_max_width
            .to_pixels(
                BASE_MODAL_WIDTH.to_pixels(window.rem_size()),
                window.viewport_size().width,
            );
        let picker = cx.new(|cx| {
            Picker::uniform_list(delegate, window, cx)
                .initial_width(Rems::from_pixels(modal_width, window))
                .show_scrollbar(true)
                .max_height(Rems::from_pixels(
                    window.viewport_size().height * 0.75,
                    window,
                ))
        });
        let picker_focus_handle = picker.focus_handle(cx);
        picker.update(cx, |picker, cx| {
            picker.delegate.modal_width = modal_width;
            picker.delegate.focus_handle = picker_focus_handle;
            picker.delegate.fetch_root(window, cx);
        });

        CallHierarchyView {
            picker,
            mode,
            _subscriptions: [project_subscription, lsp_store_subscription],
        }
    }

    fn retry_empty_hierarchy(
        &mut self,
        should_retry: impl FnOnce(&CallHierarchyDelegate, &App) -> bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.picker.update(cx, |picker, cx| {
            if picker.delegate.root_item.is_none() && should_retry(&picker.delegate, cx) {
                picker.delegate.fetch_root(window, cx);
            }
        });
    }

    fn set_mode(&mut self, mode: CallHierarchyMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode == mode {
            cx.emit(DismissEvent);
            return;
        }
        self.mode = mode;
        self.picker.update(cx, |picker, cx| {
            picker.delegate.mode = mode;
            if let Some(root_item) = picker.delegate.root_item.clone() {
                picker.delegate.load_calls(root_item, None, window, cx);
            }
            picker.set_query("", window, cx);
            picker.refresh_placeholder(window, cx);
        });
        cx.notify();
    }
}

impl Focusable for CallHierarchyView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for CallHierarchyView {}

impl ModalView for CallHierarchyView {
    fn on_before_dismiss(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> DismissDecision {
        self.picker.update(cx, |picker, cx| {
            picker.delegate.restore_editor(window, cx);
        });
        DismissDecision::Dismiss(true)
    }
}

impl Render for CallHierarchyView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("CallHierarchyPicker")
            .on_action(cx.listener(|view, _: &ShowIncomingCalls, window, cx| {
                view.set_mode(CallHierarchyMode::Incoming, window, cx);
            }))
            .on_action(cx.listener(|view, _: &ShowOutgoingCalls, window, cx| {
                view.set_mode(CallHierarchyMode::Outgoing, window, cx);
            }))
            .on_action(cx.listener(|view, _: &ToggleDirection, window, cx| {
                view.set_mode(view.mode.opposite(), window, cx);
            }))
            .child(self.picker.clone())
    }
}

pub struct CallHierarchyDelegate {
    view: WeakEntity<CallHierarchyView>,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    editor: Entity<Editor>,
    mode: CallHierarchyMode,
    state: FetchState,
    fetch_task: Option<Task<()>>,
    root_item: Option<CallHierarchyItem>,
    root_stack: Vec<CallHierarchyItem>,
    prev_scroll_position: Option<Point<ScrollOffset>>,
    focus_handle: FocusHandle,
    queried_buffer: Option<Entity<Buffer>>,
    modal_width: Pixels,
    calls: Vec<Call>,
    candidates: Arc<Vec<StringMatchCandidate>>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl CallHierarchyDelegate {
    fn new(
        view: WeakEntity<CallHierarchyView>,
        editor: Entity<Editor>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        mode: CallHierarchyMode,
        prev_scroll_position: Option<Point<ScrollOffset>>,
        focus_handle: FocusHandle,
    ) -> Self {
        Self {
            view,
            workspace,
            project,
            editor,
            mode,
            state: FetchState::Loading,
            fetch_task: None,
            root_item: None,
            root_stack: Vec::new(),
            prev_scroll_position,
            focus_handle,
            queried_buffer: None,
            modal_width: Pixels::ZERO,
            calls: Vec::new(),
            candidates: Arc::new(Vec::new()),
            matches: Vec::new(),
            selected_index: 0,
        }
    }

    fn fetch_root(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let editor_anchor = self.editor.update(cx, |editor, cx| {
            let selection = editor.selections.newest_anchor().head();
            editor
                .buffer()
                .read(cx)
                .text_anchor_for_position(selection, cx)
        });
        let Some((buffer, position)) = editor_anchor else {
            self.state = FetchState::NoSymbol;
            cx.notify();
            return;
        };
        self.queried_buffer = Some(buffer.clone());
        self.state = FetchState::Loading;
        cx.notify();

        let prepare_task = self.project.update(cx, |project, cx| {
            project.prepare_call_hierarchy(&buffer, position, cx)
        });

        self.fetch_task = Some(cx.spawn_in(window, async move |picker, mut cx| {
            let root_item = match prepare_task.await {
                Ok(items) => items.unwrap_or_default().into_iter().next(),
                Err(error) => {
                    log::error!("failed to prepare call hierarchy: {error:#}");
                    picker
                        .update_in(cx, |picker, window, cx| {
                            picker
                                .delegate
                                .workspace
                                .update(cx, |workspace, cx| workspace.show_error(error, cx))
                                .ok();
                            picker.delegate.dismissed(window, cx);
                        })
                        .ok();
                    return;
                }
            };
            let Some(root_item) = root_item else {
                picker
                    .update_in(cx, |picker, _, cx| {
                        picker.delegate.state = FetchState::NoSymbol;
                        cx.notify();
                    })
                    .ok();
                return;
            };
            picker
                .update_in(cx, |picker, window, cx| {
                    picker.delegate.root_item = Some(root_item.clone());
                    picker.refresh_placeholder(window, cx);
                })
                .ok();
            load_and_apply_calls(picker, root_item, None, &mut cx).await;
        }));
    }

    fn load_calls(
        &mut self,
        root_item: CallHierarchyItem,
        select_item: Option<CallHierarchyItem>,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.state = FetchState::Loading;
        self.calls.clear();
        self.matches.clear();
        self.selected_index = 0;
        cx.notify();

        self.fetch_task = Some(cx.spawn_in(window, async move |picker, mut cx| {
            load_and_apply_calls(picker, root_item, select_item, &mut cx).await;
        }));
    }

    fn selected_call(&self) -> Option<&Call> {
        let mat = self.matches.get(self.selected_index)?;
        self.calls.get(mat.candidate_id)
    }

    fn expand_selected(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) -> bool {
        let Some(new_root) = self.selected_call().map(|call| call.item.clone()) else {
            return false;
        };
        if let Some(previous_root) = self.root_item.replace(new_root.clone()) {
            self.root_stack.push(previous_root);
        }
        self.load_calls(new_root, None, window, cx);
        true
    }

    fn collapse_to_parent(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) -> bool {
        let Some(parent) = self.root_stack.pop() else {
            return false;
        };
        let child = self.root_item.replace(parent.clone());
        self.load_calls(parent, child, window, cx);
        true
    }

    fn preview_selected(&self, cx: &mut Context<Picker<Self>>) {
        let call = self.selected_call();
        self.editor.update(cx, |editor, cx| {
            editor.clear_row_highlights::<CallHierarchyPreview>();
            let Some(call) = call else {
                return;
            };
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let Some(buffer_snapshot) = snapshot.as_singleton() else {
                return;
            };
            if buffer_snapshot.remote_id() != call.target.buffer.read(cx).remote_id() {
                return;
            }
            let Some(start) = snapshot.anchor_in_buffer(call.target.range.start) else {
                return;
            };
            let Some(end) = snapshot.anchor_in_buffer(call.target.range.end) else {
                return;
            };
            editor.highlight_rows::<CallHierarchyPreview>(
                start..end,
                |cx| cx.theme().colors().editor_highlighted_line_background,
                RowHighlightOptions {
                    autoscroll: true,
                    ..RowHighlightOptions::default()
                },
                cx,
            );
            editor.request_autoscroll(Autoscroll::center(), cx);
        });
    }

    fn restore_editor(&self, window: &mut Window, cx: &mut App) {
        let scroll_position = self.prev_scroll_position;
        self.editor.update(cx, |editor, cx| {
            editor.clear_row_highlights::<CallHierarchyPreview>();
            if let Some(scroll_position) = scroll_position {
                editor.set_scroll_position(scroll_position, window, cx);
            }
        });
    }
}

impl PickerDelegate for CallHierarchyDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "call hierarchy"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        match (&self.root_item, self.mode) {
            (Some(root), CallHierarchyMode::Incoming) => {
                Arc::from(format!("Search calls to `{}`...", root.name))
            }
            (Some(root), CallHierarchyMode::Outgoing) => {
                Arc::from(format!("Search calls from `{}`...", root.name))
            }
            (None, CallHierarchyMode::Incoming) => Arc::from("Search incoming calls..."),
            (None, CallHierarchyMode::Outgoing) => Arc::from("Search outgoing calls..."),
        }
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(SharedString::new_static(match self.state {
            FetchState::Loading => "Fetching call hierarchy…",
            FetchState::NoSymbol => "No callable symbol under the cursor",
            FetchState::Loaded => {
                if self.calls.is_empty() {
                    match self.mode {
                        CallHierarchyMode::Incoming => "No incoming calls found",
                        CallHierarchyMode::Outgoing => "No outgoing calls found",
                    }
                } else {
                    "No matches"
                }
            }
        }))
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
        self.preview_selected(cx);
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        if query.is_empty() {
            self.matches = identity_matches(self.calls.len());
            self.selected_index = 0;
            return Task::ready(());
        }

        let candidates = self.candidates.clone();
        let executor = cx.background_executor().clone();
        cx.spawn(async move |picker, cx| {
            let matches = fuzzy::match_strings(
                candidates.as_slice(),
                &query,
                true,
                true,
                100,
                &AtomicBool::new(false),
                executor,
            )
            .await;

            picker
                .update(cx, |picker, cx| {
                    picker.delegate.matches = matches;
                    picker.delegate.selected_index = 0;
                    cx.notify();
                })
                .ok();
        })
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(call) = self.selected_call() else {
            return;
        };

        let buffer = call.target.buffer.clone();
        let target = call.target.range.start;
        self.prev_scroll_position = None;

        self.workspace
            .update(cx, |workspace, cx| {
                let position = target.to_point(&buffer.read(cx).snapshot());
                let pane = if secondary {
                    workspace.adjacent_pane(window, cx)
                } else {
                    workspace.active_pane().clone()
                };

                let editor = workspace.open_project_item::<Editor>(
                    Some(pane),
                    buffer,
                    true,
                    true,
                    true,
                    true,
                    window,
                    cx,
                );

                editor.update(cx, |editor, cx| {
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::center()),
                        window,
                        cx,
                        |s| s.select_ranges([position..position]),
                    );
                });
            })
            .ok();

        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.restore_editor(window, cx);
        self.view.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn select_child(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<String> {
        self.expand_selected(window, cx).then(String::new)
    }

    fn select_parent(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<String> {
        self.collapse_to_parent(window, cx).then(String::new)
    }

    fn render_footer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<gpui::AnyElement> {
        if self.calls.is_empty() && self.root_stack.is_empty() {
            return None;
        }
        let focus_handle = self.focus_handle.clone();
        let expand_label = match self.mode {
            CallHierarchyMode::Incoming => "Show Callers",
            CallHierarchyMode::Outgoing => "Show Callees",
        };
        Some(
            h_flex()
                .w_full()
                .p_1p5()
                .gap_0p5()
                .justify_end()
                .flex_wrap()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .when(!self.root_stack.is_empty(), |this| {
                    this.child(
                        Button::new("collapse-call", "Back")
                            .key_binding(
                                KeyBinding::for_action_in(&menu::SelectParent, &focus_handle, cx)
                                    .map(|key_binding| key_binding.size(rems_from_px(12_f32))),
                            )
                            .on_click(|_, window, cx| {
                                window.dispatch_action(menu::SelectParent.boxed_clone(), cx);
                            }),
                    )
                })
                .when(!self.calls.is_empty(), |this| {
                    this.child(
                        Button::new("expand-call", expand_label)
                            .key_binding(
                                KeyBinding::for_action_in(&menu::SelectChild, &focus_handle, cx)
                                    .map(|key_binding| key_binding.size(rems_from_px(12_f32))),
                            )
                            .on_click(|_, window, cx| {
                                window.dispatch_action(menu::SelectChild.boxed_clone(), cx);
                            }),
                    )
                })
                .child(
                    Button::new("toggle-direction", "Switch Direction")
                        .key_binding(
                            KeyBinding::for_action_in(&ToggleDirection, &focus_handle, cx)
                                .map(|key_binding| key_binding.size(rems_from_px(12_f32))),
                        )
                        .on_click(|_, window, cx| {
                            window.dispatch_action(ToggleDirection.boxed_clone(), cx);
                        }),
                )
                .into_any_element(),
        )
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let mat = self.matches.get(ix)?;
        let call = self.calls.get(mat.candidate_id)?;

        let (name_styled, detail_styled) = render_item(call, mat.ranges(), cx);
        let tooltip_label = call.label.clone().zip(call.display.label_text.clone());
        let tooltip_max_width = self.modal_width;
        let full_signature = call.display.full_signature.clone();
        let full_path = call.display.path.clone();

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .text_ui(cx)
                        .child(
                            h_flex()
                                .overflow_x_hidden()
                                .text_size(ThemeSettings::get_global(cx).buffer_font_size(cx))
                                .child(name_styled)
                                .children(detail_styled),
                        )
                        .children(call.display.path.clone().map(|path| {
                            Label::new(path)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate()
                        })),
                )
                .when(call.display.needs_tooltip, |this| {
                    this.tooltip(move |_window, cx| {
                        AnyView::from(cx.new(|_| CallSignatureTooltip {
                            label: tooltip_label.clone(),
                            full_signature: full_signature.clone(),
                            path: full_path.clone(),
                            max_width: tooltip_max_width,
                        }))
                    })
                }),
        )
    }
}

struct CallSignatureTooltip {
    label: Option<(Arc<CodeLabel>, SharedString)>,
    full_signature: SharedString,
    path: Option<SharedString>,
    max_width: Pixels,
}

impl Render for CallSignatureTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut signature_style = buffer_text_style(cx);
        signature_style.line_height =
            relative(ThemeSettings::get_global(cx).buffer_line_height.value());
        let signature = match &self.label {
            Some((label, label_text)) => StyledText::new(label_text.clone())
                .with_default_highlights(
                    &signature_style,
                    cx.theme().syntax().resolve_runs(&label.runs),
                )
                .into_any_element(),
            None => div().child(self.full_signature.clone()).into_any_element(),
        };
        let buffer_font_size = ThemeSettings::get_global(cx).buffer_font_size(cx);
        let max_width = self.max_width;
        let path = self.path.clone();
        tooltip_container(cx, move |el, _| {
            el.max_w(max_width)
                .gap_0p5()
                .child(div().text_size(buffer_font_size).child(signature))
                .children(
                    path.map(|path| Label::new(path).size(LabelSize::Small).color(Color::Muted)),
                )
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetchState {
    Loading,
    NoSymbol,
    Loaded,
}

struct CallHierarchyPreview;

fn toggle_call_hierarchy(
    editor: Entity<Editor>,
    mode: CallHierarchyMode,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = editor.read(cx).workspace() else {
        log::error!("call hierarchy: editor has no workspace");
        return;
    };

    let project = workspace.read(cx).project().clone();
    let workspace_weak = workspace.downgrade();

    workspace.update(cx, |workspace, cx| {
        workspace.toggle_modal(window, cx, |window, cx| {
            CallHierarchyView::new(editor, project, workspace_weak, mode, window, cx)
        });
    });
}

fn item_location(item: &CallHierarchyItem) -> Location {
    Location {
        buffer: item.buffer.clone(),
        range: item.selection_range.clone(),
    }
}

async fn attach_labels(calls: &mut [Call], project: &Entity<Project>, cx: &mut AsyncWindowContext) {
    let mut fallback_groups = Vec::<(Arc<Language>, Vec<usize>)>::new();
    for (index, call) in calls.iter_mut().enumerate() {
        let Some(language) = call
            .item
            .buffer
            .read_with(cx, |buffer, _| buffer.language().cloned())
        else {
            continue;
        };
        if let Some(label) = signature_label(&call.item, &language) {
            call.label = Some(Arc::new(label));
            continue;
        }
        match fallback_groups
            .iter_mut()
            .find(|(group_language, _)| group_language.name() == language.name())
        {
            Some((_, indices)) => indices.push(index),
            None => fallback_groups.push((language, vec![index])),
        }
    }
    if fallback_groups.is_empty() {
        return;
    }
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    for (language, indices) in fallback_groups {
        let Some(lsp_adapter) = language_registry
            .lsp_adapters(&language.name())
            .first()
            .cloned()
        else {
            continue;
        };
        let symbols = indices
            .iter()
            .map(|&index| language::Symbol {
                name: calls[index].item.name.clone(),
                kind: lsp_to_symbol_kind(calls[index].item.kind),
                container_name: None,
            })
            .collect::<Vec<_>>();
        let Some(labels) = lsp_adapter
            .labels_for_symbols(&symbols, &language)
            .await
            .log_err()
        else {
            continue;
        };
        for (index, label) in indices.into_iter().zip(labels) {
            calls[index].label = label.map(Arc::new);
        }
    }
}

async fn load_and_apply_calls(
    picker: WeakEntity<Picker<CallHierarchyDelegate>>,
    root_item: CallHierarchyItem,
    select_item: Option<CallHierarchyItem>,
    cx: &mut AsyncWindowContext,
) {
    let Ok((project, mode)) = picker.read_with(cx, |picker, _| {
        (picker.delegate.project.clone(), picker.delegate.mode)
    }) else {
        return;
    };
    let mut calls = fetch_calls(&root_item, &project, mode, cx).await;
    picker
        .update_in(cx, |picker, window, cx| {
            let row_insets = (DynamicSpacing::Base04.rems(cx).to_pixels(window.rem_size())
                + DynamicSpacing::Base06.rems(cx).to_pixels(window.rem_size()))
                * 2.;
            let available_width = picker.delegate.modal_width - row_insets;
            let settings = ThemeSettings::get_global(cx);
            let buffer_font_size = settings.buffer_font_size(cx);
            let path_font_size = TextSize::Small.rems(cx).to_pixels(window.rem_size());
            for call in calls.iter_mut() {
                let signature = call
                    .display
                    .label_text
                    .clone()
                    .unwrap_or_else(|| call.display.full_signature.clone());
                let signature_width = shaped_width(
                    signature,
                    settings.buffer_font.clone(),
                    buffer_font_size,
                    window,
                );
                let path_width = call
                    .display
                    .path
                    .clone()
                    .map(|path| {
                        shaped_width(path, settings.ui_font.clone(), path_font_size, window)
                    })
                    .unwrap_or_default();
                call.display.needs_tooltip = signature_width.max(path_width) > available_width;
            }
            picker.delegate.state = FetchState::Loaded;
            picker.delegate.calls = calls;
            picker.delegate.candidates = Arc::new(
                picker
                    .delegate
                    .calls
                    .iter()
                    .enumerate()
                    .map(|(id, call)| StringMatchCandidate::new(id, &call.item.name))
                    .collect(),
            );
            picker.delegate.matches = identity_matches(picker.delegate.calls.len());
            picker.delegate.selected_index = 0;
            let restored_index = select_item.and_then(|target| {
                picker.delegate.calls.iter().position(|call| {
                    call.item.buffer == target.buffer
                        && call.item.selection_range == target.selection_range
                        && call.item.name == target.name
                })
            });
            if let Some(restored_index) = restored_index {
                picker.set_selected_index(restored_index, None, true, window, cx);
            }
            picker.refresh_placeholder(window, cx);
            cx.notify();
        })
        .ok();
}

fn call_display_path(buffer: &Buffer, cx: &App) -> Option<PathBuf> {
    let file = buffer.file()?;
    Some(
        if let Some(project_file) = project::File::from_dyn(Some(file))
            && project_file.worktree.read(cx).is_visible()
        {
            project_file.full_path(cx)
        } else if let Some(local_file) = file.as_local() {
            local_file.abs_path(cx).compact()
        } else {
            file.full_path(cx)
        },
    )
}

fn signature_label(item: &CallHierarchyItem, language: &Arc<Language>) -> Option<CodeLabel> {
    let detail = item
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|detail| !detail.is_empty())?;
    let signature = collapse_whitespace(detail);
    let name_start = find_symbol_name(&signature, &item.name)?;
    let filter_range = name_start..name_start + item.name.len();
    let source = Rope::from_iter([signature.as_str(), " {}"]);
    let runs = language
        .highlight_text(&source, 0..signature.len())
        .collect();
    Some(CodeLabel::new(signature, filter_range, runs))
}

fn collapse_whitespace(text: &str) -> String {
    fn is_word(ch: char) -> bool {
        ch.is_alphanumeric() || ch == '_'
    }

    let mut result = String::new();
    let mut previous_indent = 0;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let indent = line.chars().take_while(|ch| ch.is_whitespace()).count();
        let fragment = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
        if result.is_empty() {
            result = fragment;
        } else {
            let opens_block = indent > previous_indent
                && result.chars().last().is_some_and(|last| !is_word(last));
            let closes_block = indent < previous_indent
                && fragment.chars().next().is_some_and(|first| !is_word(first));
            if closes_block && result.ends_with(',') {
                result.pop();
            }
            if !opens_block && !closes_block {
                result.push(' ');
            }
            result.push_str(&fragment);
        }
        previous_indent = indent;
    }
    if result.ends_with(',') {
        result.pop();
    }
    result
}

fn find_symbol_name(text: &str, symbol_name: &str) -> Option<usize> {
    if symbol_name.is_empty() {
        return None;
    }
    text.match_indices(symbol_name)
        .map(|(start, _)| start)
        .find(|&start| {
            let boundary_before = text[..start]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_');
            let boundary_after = text[start + symbol_name.len()..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_');
            boundary_before && boundary_after
        })
}

fn identity_matches(count: usize) -> Vec<StringMatch> {
    (0..count)
        .map(|index| StringMatch {
            candidate_id: index,
            score: 0.0,
            positions: Vec::new(),
            string: String::new(),
        })
        .collect()
}

/// Extracts display information from a `Call` for rendering in UI.
fn compute_call_display(call: &Call, cx: &App) -> CallDisplay {
    let buffer = call.target.buffer.read(cx);
    let line_number = call.target.range.start.to_point(&buffer.snapshot()).row + 1;
    let path = call_display_path(buffer, cx).map(|path| {
        let path = path.to_string_lossy();
        SharedString::from(if call.site_count > 1 {
            format!("{path}:{line_number} ({} calls)", call.site_count)
        } else {
            format!("{path}:{line_number}")
        })
    });

    let detail = extract_call_detail(call).map(SharedString::from);
    let full_signature = SharedString::from(match &detail {
        Some(detail) => format!("{}{detail}", call.item.name),
        None => call.item.name.clone(),
    });
    let label_text = call
        .label
        .as_ref()
        .map(|label| SharedString::from(label.text.clone()));

    CallDisplay {
        name: SharedString::from(call.item.name.clone()),
        detail,
        path,
        full_signature,
        label_text,
        needs_tooltip: false,
    }
}

fn extract_call_detail(call: &Call) -> Option<String> {
    let detail = if let Some(detail) = call.label.as_deref().and_then(detail_from_label_suffix) {
        Some(detail)
    } else {
        call.item
            .detail
            .as_deref()
            .map(str::trim)
            .filter(|detail| !detail.is_empty())
            .and_then(|detail| {
                trim_detail_after_symbol_name(detail, &call.item.name)
                    .or_else(|| Some(detail.to_owned()))
            })
    };
    detail.map(|detail| collapse_whitespace(&detail))
}

fn detail_from_label_suffix(label: &CodeLabel) -> Option<String> {
    label
        .text
        .get(label.filter_range.end..)
        .map(str::trim)
        .filter(|suffix| !suffix.is_empty())
        .map(ToOwned::to_owned)
}

fn trim_detail_after_symbol_name(detail: &str, symbol_name: &str) -> Option<String> {
    let name_start = find_symbol_name(detail, symbol_name)?;
    let suffix = detail.get(name_start + symbol_name.len()..)?.trim();
    (!suffix.is_empty()).then(|| suffix.to_owned())
}

fn shaped_width(
    text: SharedString,
    font: gpui::Font,
    font_size: Pixels,
    window: &Window,
) -> Pixels {
    let run = TextRun {
        len: text.len(),
        font,
        color: gpui::Hsla::default(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(text, font_size, &[run], None)
        .width
}

fn render_item(
    call_item: &Call,
    match_ranges: impl IntoIterator<Item = Range<usize>>,
    cx: &App,
) -> (StyledText, Option<StyledText>) {
    let mut base_text_style = buffer_text_style(cx);
    base_text_style.text_overflow = Some(gpui::TextOverflow::Truncate(SharedString::from("…")));

    let highlight_style = HighlightStyle {
        background_color: Some(cx.theme().colors().text_accent.alpha(0.3)),
        ..HighlightStyle::default()
    };

    let label_carries_detail = call_item
        .label
        .as_ref()
        .is_some_and(|label| detail_from_label_suffix(label).is_some());
    let detail_styled = if label_carries_detail {
        None
    } else {
        call_item.display.detail.clone().map(|detail| {
            let mut detail_style = base_text_style.clone();
            detail_style.color = cx.theme().colors().text_muted;
            StyledText::new(detail).with_default_highlights(&detail_style, std::iter::empty())
        })
    };

    let name_styled = if let Some((label, label_text)) = call_item
        .label
        .as_ref()
        .zip(call_item.display.label_text.clone())
    {
        let syntax_runs = cx.theme().syntax().resolve_runs(&label.runs);
        let custom_highlights = match_ranges.into_iter().map(|range| {
            let start = label.filter_range.start + range.start;
            let end = label.filter_range.start + range.end;
            (start..end, highlight_style)
        });

        StyledText::new(label_text).with_default_highlights(
            &base_text_style,
            gpui::combine_highlights(custom_highlights, syntax_runs),
        )
    } else {
        StyledText::new(call_item.display.name.clone()).with_default_highlights(
            &base_text_style,
            match_ranges.into_iter().map(|r| (r, highlight_style)),
        )
    };

    (name_styled, detail_styled)
}

