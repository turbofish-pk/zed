use std::ops::Range;
use std::{cmp, sync::Arc};

use editor::scroll::ScrollOffset;
use editor::{Anchor, AnchorRangeExt, Editor, scroll::Autoscroll};
use editor::{MultiBufferOffset, RowHighlightOptions, SelectionEffects};
use fuzzy_nucleo::StringMatch;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, HighlightStyle,
    ParentElement, Point, Rems, Render, Styled, StyledText, Task, WeakEntity, Window, div, rems,
};
use language::{OffsetRangeExt, Outline, OutlineItem, OutlineSearchEntry};
use picker::{MatchLocation, Picker, PickerDelegate, PreviewUpdate};
use theme::ActiveTheme;
use ui::{ListItem, ListItemSpacing, prelude::*, utils::buffer_text_style};
use workspace::{DismissDecision, ModalView};

pub fn init(cx: &mut App) {
    cx.observe_new(OutlineView::register).detach();
    zed_actions::outline::TOGGLE_OUTLINE
        .set(|view, window, cx| {
            let Ok(editor) = view.downcast::<Editor>() else {
                return;
            };

            toggle(editor, &Default::default(), window, cx);
        })
        .ok();
}

pub fn toggle(
    editor: Entity<Editor>,
    _: &zed_actions::outline::ToggleOutline,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = editor.read(cx).workspace() else {
        return;
    };
    if workspace.read(cx).active_modal::<OutlineView>(cx).is_some() {
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                OutlineView::new(Outline::new(Vec::new()), editor.clone(), window, cx)
            });
        });
        return;
    }

    let Some(task) = outline_for_editor(&editor, cx) else {
        return;
    };
    let editor = editor.clone();
    window
        .spawn(cx, async move |cx| {
            let items = task.await;
            if items.is_empty() {
                return;
            }
            cx.update(|window, cx| {
                let outline = Outline::new(items);
                workspace.update(cx, |workspace, cx| {
                    workspace.toggle_modal(window, cx, |window, cx| {
                        OutlineView::new(outline, editor, window, cx)
                    });
                });
            })
            .ok();
        })
        .detach();
}

fn outline_for_editor(
    editor: &Entity<Editor>,
    cx: &mut App,
) -> Option<Task<Vec<OutlineItem<Anchor>>>> {
    let multibuffer = editor.read(cx).buffer().read(cx).snapshot(cx);
    let buffer_snapshot = multibuffer.as_singleton()?;
    let buffer_id = buffer_snapshot.remote_id();
    let task = editor.update(cx, |editor, cx| editor.buffer_outline_items(buffer_id, cx));

    Some(cx.background_executor().spawn(async move {
        task.await
            .into_iter()
            .filter_map(|item| {
                Some(OutlineItem {
                    depth: item.depth,
                    range: multibuffer.anchor_in_buffer(item.range.start)?
                        ..multibuffer.anchor_in_buffer(item.range.end)?,
                    selection_range: multibuffer.anchor_in_buffer(item.selection_range.start)?
                        ..multibuffer.anchor_in_buffer(item.selection_range.end)?,
                    source_range_for_text: multibuffer
                        .anchor_in_buffer(item.source_range_for_text.start)?
                        ..multibuffer.anchor_in_buffer(item.source_range_for_text.end)?,
                    text: item.text,
                    highlight_ranges: item.highlight_ranges,
                    name_ranges: item.name_ranges,
                    body_range: item.body_range.and_then(|r| {
                        Some(
                            multibuffer.anchor_in_buffer(r.start)?
                                ..multibuffer.anchor_in_buffer(r.end)?,
                        )
                    }),
                    annotation_range: item.annotation_range.and_then(|r| {
                        Some(
                            multibuffer.anchor_in_buffer(r.start)?
                                ..multibuffer.anchor_in_buffer(r.end)?,
                        )
                    }),
                })
            })
            .collect()
    }))
}

pub struct OutlineView {
    picker: Entity<Picker<OutlineViewDelegate>>,
}

impl Focusable for OutlineView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for OutlineView {}
impl ModalView for OutlineView {
    fn on_before_dismiss(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> DismissDecision {
        self.picker.update(cx, |picker, cx| {
            picker.delegate.restore_active_editor(window, cx)
        });
        DismissDecision::Dismiss(true)
    }
}

impl Render for OutlineView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .on_action(cx.listener(
                |_this: &mut OutlineView,
                 _: &zed_actions::outline::ToggleOutline,
                 _window: &mut Window,
                 cx: &mut Context<OutlineView>| {
                    // When outline::Toggle is triggered while the outline is open, dismiss it
                    cx.emit(DismissEvent);
                },
            ))
            .child(self.picker.clone())
    }
}

impl OutlineView {
    fn register(editor: &mut Editor, _: Option<&mut Window>, cx: &mut Context<Editor>) {
        if editor.mode().is_full() {
            let handle = cx.entity().downgrade();
            editor
                .register_action(move |action, window, cx| {
                    if let Some(editor) = handle.upgrade() {
                        toggle(editor, action, window, cx);
                    }
                })
                .detach();
        }
    }

    fn new(
        outline: Outline<Anchor>,
        editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> OutlineView {
        let project = editor.read(cx).project().cloned();
        let delegate = OutlineViewDelegate::new(cx.entity().downgrade(), outline, editor, cx);
        let picker = cx.new(|cx| {
            let picker = if let Some(project) = project {
                let preview = picker_preview::editor_preview(project, window, cx);
                Picker::uniform_list_with_preview(delegate, preview, window, cx)
            } else {
                Picker::uniform_list(delegate, window, cx)
            };
            picker
                .max_height(Rems::from_pixels(
                    window.viewport_size().height * 0.75,
                    window,
                ))
                .show_scrollbar(true)
        });
        OutlineView { picker }
    }
}

struct OutlineViewDelegate {
    outline_view: WeakEntity<OutlineView>,
    active_editor: Entity<Editor>,
    outline: Arc<Outline<Anchor>>,
    selected_match_index: usize,
    prev_scroll_position: Option<Point<ScrollOffset>>,
    matches: Vec<OutlineSearchEntry>,
}

enum OutlineRowHighlights {}

impl OutlineViewDelegate {
    fn new(
        outline_view: WeakEntity<OutlineView>,
        outline: Outline<Anchor>,
        editor: Entity<Editor>,

        cx: &mut Context<OutlineView>,
    ) -> Self {
        Self {
            outline_view,
            matches: Default::default(),
            selected_match_index: 0,
            prev_scroll_position: Some(editor.update(cx, |editor, cx| editor.scroll_position(cx))),
            active_editor: editor,
            outline: Arc::new(outline),
        }
    }

    fn restore_active_editor(&mut self, window: &mut Window, cx: &mut App) {
        self.active_editor.update(cx, |editor, cx| {
            editor.clear_row_highlights::<OutlineRowHighlights>();
            if let Some(scroll_position) = self.prev_scroll_position {
                editor.set_scroll_position(scroll_position, window, cx);
            }
        })
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        navigate: bool,

        cx: &mut Context<Picker<OutlineViewDelegate>>,
    ) {
        let Some(selected_match) = self.matches.get(ix) else {
            self.selected_match_index = self.matches.len();
            return;
        };

        self.selected_match_index = ix;

        if navigate {
            let outline_item = &self.outline.items[selected_match.candidate_id()];

            self.active_editor.update(cx, |active_editor, cx| {
                active_editor.clear_row_highlights::<OutlineRowHighlights>();
                active_editor.highlight_rows::<OutlineRowHighlights>(
                    outline_item.range.start..outline_item.range.end,
                    |cx| cx.theme().colors().editor_highlighted_line_background,
                    RowHighlightOptions {
                        autoscroll: true,
                        ..Default::default()
                    },
                    cx,
                );
                active_editor.request_autoscroll(Autoscroll::center(), cx);
            });
        }
    }
}

impl PickerDelegate for OutlineViewDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "outline view"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search buffer symbols...".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_match_index
    }

    fn can_select(&self, ix: usize, _window: &mut Window, _cx: &mut Context<Picker<Self>>) -> bool {
        ix < self.matches.len()
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _: &mut Window,
        cx: &mut Context<Picker<OutlineViewDelegate>>,
    ) {
        self.set_selected_index(ix, true, cx);
    }

    fn try_get_preview_data_for_match(&self, cx: &App) -> Option<PreviewUpdate> {
        let selected_match = self.matches.get(self.selected_match_index)?;
        let outline_item = self.outline.items.get(selected_match.candidate_id())?;
        let multi_buffer = self.active_editor.read(cx).buffer().clone();
        let (buffer, start) = multi_buffer
            .read(cx)
            .text_anchor_for_position(outline_item.selection_range.start, cx)?;
        let (end_buffer, end) = multi_buffer
            .read(cx)
            .text_anchor_for_position(outline_item.selection_range.end, cx)?;
        if buffer != end_buffer {
            return None;
        }

        let range = (start..end).to_offset(&buffer.read(cx).text_snapshot());
        Some(PreviewUpdate::from_buffer(
            buffer,
            MatchLocation {
                anchor_range: start..end,
                range,
            },
        ))
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<OutlineViewDelegate>>,
    ) -> Task<()> {
        let is_query_empty = query.is_empty();
        if is_query_empty {
            self.restore_active_editor(window, cx);
        }

        let outline = self.outline.clone();
        cx.spawn_in(window, async move |this, cx| {
            let matches = if is_query_empty {
                outline
                    .items
                    .iter()
                    .enumerate()
                    .map(|(index, _)| {
                        OutlineSearchEntry::Match(StringMatch {
                            candidate_id: index,
                            score: Default::default(),
                            positions: Default::default(),
                            string: Default::default(),
                        })
                    })
                    .collect()
            } else {
                outline
                    .search(&query, cx.background_executor().clone())
                    .await
            };

            let _ = this.update(cx, |this, cx| {
                this.delegate.matches = matches;
                let selected_index = if is_query_empty {
                    let (buffer, cursor_offset) =
                        this.delegate.active_editor.update(cx, |editor, cx| {
                            let snapshot = editor.display_snapshot(cx);
                            let cursor_offset = editor
                                .selections
                                .newest::<MultiBufferOffset>(&snapshot)
                                .head();
                            (snapshot.buffer().clone(), cursor_offset)
                        });
                    this.delegate
                        .matches
                        .iter()
                        .enumerate()
                        .filter_map(|(ix, entry)| {
                            let item = &this.delegate.outline.items[entry.candidate_id()];
                            let range = item.range.to_offset(&buffer);
                            range.contains(&cursor_offset).then_some((ix, item.depth))
                        })
                        .max_by_key(|(ix, depth)| (*depth, cmp::Reverse(*ix)))
                        .map(|(ix, _)| ix)
                        .unwrap_or(0)
                } else {
                    this.delegate
                        .matches
                        .iter()
                        .enumerate()
                        .filter_map(|(ix, entry)| {
                            entry
                                .as_match()
                                .filter(|m| !m.positions.is_empty())
                                .map(|m| (ix, m.score))
                        })
                        .max_by(|(ix_a, a), (ix_b, b)| a.total_cmp(b).then(ix_b.cmp(ix_a)))
                        .map(|(ix, _)| ix)
                        .unwrap_or(0)
                };

                this.delegate
                    .set_selected_index(selected_index, !is_query_empty, cx);
            });
        })
    }

    fn confirm(
        &mut self,
        _: bool,
        window: &mut Window,
        cx: &mut Context<Picker<OutlineViewDelegate>>,
    ) {
        self.prev_scroll_position.take();
        self.set_selected_index(self.selected_match_index, true, cx);

        self.active_editor.update(cx, |active_editor, cx| {
            let highlight = active_editor
                .highlighted_rows::<OutlineRowHighlights>(cx)
                .next();
            if let Some((rows, _)) = highlight {
                active_editor.change_selections(
                    SelectionEffects::scroll(Autoscroll::center()).unfold(),
                    window,
                    cx,
                    |s| s.select_ranges([rows.start..rows.start]),
                );
                active_editor.clear_row_highlights::<OutlineRowHighlights>();
                window.focus(&active_editor.focus_handle(cx), cx);
            }
        });

        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, window: &mut Window, cx: &mut Context<Picker<OutlineViewDelegate>>) {
        self.outline_view
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .ok();
        self.restore_active_editor(window, cx);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let entry = self.matches.get(ix)?;
        let outline_item = self.outline.items.get(entry.candidate_id())?;
        let ranges = entry.as_match().into_iter().flat_map(|m| m.ranges());

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    div()
                        .text_ui(cx)
                        .pl(rems(outline_item.depth as f32))
                        .child(render_item(outline_item, ranges, cx)),
                ),
        )
    }
}

pub fn render_item<T, M: IntoIterator<Item = Range<usize>>>(
    outline_item: &OutlineItem<T>,
    match_ranges: M,
    cx: &App,
) -> impl IntoElement + use<T, M> {
    let highlight_style = HighlightStyle {
        background_color: Some(cx.theme().colors().text_accent.alpha(0.3)),
        ..Default::default()
    };
    let custom_highlights = match_ranges
        .into_iter()
        .map(|range| (range, highlight_style));

    let text_style = buffer_text_style(cx);
    let buffer_font_size = text_style.font_size;
    let highlights = gpui::combine_highlights(
        custom_highlights,
        outline_item.highlight_ranges.iter().cloned(),
    );

    div().text_size(buffer_font_size).child(
        StyledText::new(outline_item.text.clone()).with_default_highlights(&text_style, highlights),
    )
}

