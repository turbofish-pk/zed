pub mod cursor_position;

use cursor_position::UserCaretPosition;
use editor::{
    Anchor, Editor, RowHighlightOptions, SelectionEffects, ToPoint,
    actions::Tab,
    scroll::{Autoscroll, ScrollOffset},
};
use gpui::{
    App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString, Styled,
    Subscription, div, prelude::*,
};
use language::Buffer;
use text::{Bias, Point};
use theme::ActiveTheme;
use ui::prelude::*;
use util::paths::FILE_ROW_COLUMN_DELIMITER;
use workspace::{DismissDecision, ModalView};

pub fn init(cx: &mut App) {
    cx.observe_new(GoToLine::register).detach();
}

pub struct GoToLine {
    line_editor: Entity<Editor>,
    active_editor: Entity<Editor>,
    active_buffer: Entity<Buffer>,
    current_text: SharedString,
    prev_scroll_position: Option<gpui::Point<ScrollOffset>>,
    current_line: u32,
    _subscriptions: Vec<Subscription>,
}

impl ModalView for GoToLine {
    fn on_before_dismiss(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> DismissDecision {
        self.prev_scroll_position.take();
        DismissDecision::Dismiss(true)
    }
}

impl Focusable for GoToLine {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.line_editor.focus_handle(cx)
    }
}
impl EventEmitter<DismissEvent> for GoToLine {}

enum GoToLineRowHighlights {}

impl GoToLine {
    fn register(editor: &mut Editor, _window: Option<&mut Window>, cx: &mut Context<Editor>) {
        let handle = cx.entity().downgrade();
        editor
            .register_action(move |_: &editor::actions::ToggleGoToLine, window, cx| {
                let Some(editor_handle) = handle.upgrade() else {
                    return;
                };
                let Some(workspace) = editor_handle.read(cx).workspace() else {
                    return;
                };
                let editor = editor_handle.read(cx);
                let Some(buffer) = editor.active_buffer(cx) else {
                    return;
                };
                workspace.update(cx, |workspace, cx| {
                    workspace.toggle_modal(window, cx, move |window, cx| {
                        GoToLine::new(editor_handle, buffer, window, cx)
                    });
                })
            })
            .detach();
    }

    pub fn new(
        active_editor: Entity<Editor>,
        active_buffer: Entity<Buffer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (user_caret, last_line, scroll_position) = active_editor.update(cx, |editor, cx| {
            let user_caret = UserCaretPosition::at_selection_end(
                &editor
                    .selections
                    .last::<Point>(&editor.display_snapshot(cx)),
                &editor.buffer().read(cx).snapshot(cx),
            );

            let snapshot = active_buffer.read(cx).snapshot();
            let last_line = editor
                .buffer()
                .read(cx)
                .snapshot(cx)
                .excerpts_for_buffer(snapshot.remote_id())
                .map(move |range| text::ToPoint::to_point(&range.context.end, &snapshot).row)
                .max()
                .unwrap_or(0);

            (user_caret, last_line, editor.scroll_position(cx))
        });

        let line = user_caret.line.get();
        let column = user_caret.character.get();

        let line_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            let editor_handle = cx.entity().downgrade();
            editor
                .register_action::<Tab>({
                    move |_, window, cx| {
                        let Some(editor) = editor_handle.upgrade() else {
                            return;
                        };
                        editor.update(cx, |editor, cx| {
                            if let Some(placeholder_text) = editor.placeholder_text(cx)
                                && editor.text(cx).is_empty()
                            {
                                editor.set_text(placeholder_text, window, cx);
                            }
                        });
                    }
                })
                .detach();
            editor.set_placeholder_text(
                &format!("{line}{FILE_ROW_COLUMN_DELIMITER}{column}"),
                window,
                cx,
            );
            editor
        });
        let line_editor_change = cx.subscribe_in(&line_editor, window, Self::on_line_editor_event);

        let current_text = format!(
            "Current Line: {} of {} (column {})",
            line,
            last_line + 1,
            column
        );

        Self {
            line_editor,
            active_editor,
            active_buffer,
            current_text: current_text.into(),
            prev_scroll_position: Some(scroll_position),
            current_line: line,
            _subscriptions: vec![line_editor_change, cx.on_release_in(window, Self::release)],
        }
    }

    fn release(&mut self, window: &mut Window, cx: &mut App) {
        let scroll_position = self.prev_scroll_position.take();
        self.active_editor.update(cx, |editor, cx| {
            editor.clear_row_highlights::<GoToLineRowHighlights>();
            if let Some(scroll_position) = scroll_position {
                editor.set_scroll_position(scroll_position, window, cx);
            }
            cx.notify();
        })
    }

    fn on_line_editor_event(
        &mut self,
        _: &Entity<Editor>,
        event: &editor::EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            editor::EditorEvent::Blurred if window.is_window_active() => {
                self.prev_scroll_position.take();
                cx.emit(DismissEvent)
            }
            editor::EditorEvent::BufferEdited => self.highlight_current_line(cx),
            _ => {}
        }
    }

    fn highlight_current_line(&mut self, cx: &mut Context<Self>) {
        self.active_editor.update(cx, |editor, cx| {
            editor.clear_row_highlights::<GoToLineRowHighlights>();
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let Some(start) = self.anchor_from_query(editor, cx) else {
                return;
            };
            let mut start_point = start.to_point(&snapshot);
            start_point.column = 0;
            // Force non-empty range to ensure the line is highlighted.
            let mut end_point = snapshot.clip_point(start_point + Point::new(0, 1), Bias::Left);
            if start_point == end_point {
                end_point = snapshot.clip_point(start_point + Point::new(1, 0), Bias::Left);
            }

            let end = snapshot.anchor_after(end_point);
            editor.highlight_rows::<GoToLineRowHighlights>(
                start..end,
                |cx| cx.theme().colors().editor_highlighted_line_background,
                RowHighlightOptions {
                    autoscroll: true,
                    ..Default::default()
                },
                cx,
            );
            editor.request_autoscroll(Autoscroll::center(), cx);
        });
        cx.notify();
    }

    fn anchor_from_query(&self, editor: &Editor, cx: &Context<Editor>) -> Option<Anchor> {
        let (query_row, query_char) = if let Some(offset) = self.relative_line_from_query(cx) {
            let target = if offset >= 0 {
                self.current_line.saturating_add(offset as u32)
            } else {
                self.current_line.saturating_sub(offset.unsigned_abs())
            };
            (target, None)
        } else {
            self.line_and_char_from_query(cx)?
        };

        let row = query_row.saturating_sub(1);
        let character = query_char.unwrap_or(0).saturating_sub(1);
        let target_point = {
            let buffer_snapshot = self.active_buffer.read(cx).snapshot();
            let row = row.min(buffer_snapshot.max_point().row);
            buffer_snapshot.point_from_external_input(row, character)
        };

        editor
            .buffer()
            .read(cx)
            .buffer_point_to_anchor(&self.active_buffer, target_point, cx)
    }

    fn relative_line_from_query(&self, cx: &App) -> Option<i32> {
        let input = self.line_editor.read(cx).text(cx);
        let trimmed = input.trim();

        let mut last_direction_char: Option<char> = None;
        let mut number_start_index = 0;

        for (i, c) in trimmed.char_indices() {
            match c {
                '+' | 'f' | 'F' | '-' | 'b' | 'B' => {
                    last_direction_char = Some(c);
                    number_start_index = i + c.len_utf8();
                }
                _ => break,
            }
        }

        let direction = last_direction_char?;

        let number_part = &trimmed[number_start_index..];
        let line_part = number_part
            .split(FILE_ROW_COLUMN_DELIMITER)
            .next()
            .unwrap_or(number_part)
            .trim();

        let value = line_part.parse::<u32>().ok()?;

        match direction {
            '+' | 'f' | 'F' => Some(value as i32),
            '-' | 'b' | 'B' => Some(-(value as i32)),
            _ => None,
        }
    }

    fn line_and_char_from_query(&self, cx: &App) -> Option<(u32, Option<u32>)> {
        let input = self.line_editor.read(cx).text(cx);
        let mut components = input
            .splitn(2, FILE_ROW_COLUMN_DELIMITER)
            .map(str::trim)
            .fuse();
        let row = components.next().and_then(|row| row.parse::<u32>().ok())?;
        let column = components.next().and_then(|col| col.parse::<u32>().ok());
        Some((row, column))
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.active_editor.update(cx, |editor, cx| {
            let Some(start) = self.anchor_from_query(editor, cx) else {
                return;
            };
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::center()).unfold(),
                window,
                cx,
                |s| s.select_anchor_ranges([start..start]),
            );
            editor.focus_handle(cx).focus(window, cx);
            cx.notify()
        });
        self.prev_scroll_position.take();

        cx.emit(DismissEvent);
    }
}

impl Render for GoToLine {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let help_text = if let Some(offset) = self.relative_line_from_query(cx) {
            let target_line = if offset >= 0 {
                self.current_line.saturating_add(offset as u32)
            } else {
                self.current_line.saturating_sub(offset.unsigned_abs())
            };
            format!("Go to line {target_line} ({offset:+} from current)").into()
        } else {
            match self.line_and_char_from_query(cx) {
                Some((line, Some(character))) => {
                    format!("Go to line {line}, character {character}").into()
                }
                Some((line, None)) => format!("Go to line {line}").into(),
                None => self.current_text.clone(),
            }
        };

        v_flex()
            .w(rems(24.))
            .elevation_2(cx)
            .key_context("GoToLine")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .child(
                div()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .px_2()
                    .py_1()
                    .child(self.line_editor.clone()),
            )
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .child(Label::new(help_text).color(Color::Muted)),
            )
    }
}

