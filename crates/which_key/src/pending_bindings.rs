use std::rc::Rc;

use gpui::{
    App, AvailableSpace, KeybindingKeystroke, Pixels, RenderOnce, ScrollHandle, Window, size,
};
use ui::{
    Divider, DividerColor, GradientFade, KeyBinding, LabelSize, WithScrollbar, prelude::*,
    text_for_keybinding_keystrokes,
};

use crate::PendingBinding;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingBindingRow {
    pub(crate) keystrokes: Rc<[KeybindingKeystroke]>,
    pub(crate) action_name: SharedString,
    pub(crate) is_group: bool,
}

pub(crate) fn prepare_pending_bindings(
    bindings: Vec<PendingBinding>,
    cx: &App,
) -> Vec<PendingBindingRow> {
    let mut rows = group_bindings(bindings)
        .into_iter()
        .map(|row| {
            let formatted_text = text_for_keybinding_keystrokes(&row.keystrokes, cx);
            (row, formatted_text)
        })
        .collect::<Vec<_>>();

    rows.sort_by(|(row_a, text_a), (row_b, text_b)| {
        row_a
            .is_group
            .cmp(&row_b.is_group)
            .then_with(|| row_a.keystrokes.len().cmp(&row_b.keystrokes.len()))
            .then_with(|| text_a.len().cmp(&text_b.len()))
            .then_with(|| text_a.cmp(text_b))
    });

    rows.into_iter().map(|(row, _)| row).collect()
}

fn group_bindings(bindings: Vec<PendingBinding>) -> Vec<PendingBindingRow> {
    let mut groups: Vec<Vec<PendingBinding>> = Vec::new();
    for binding in bindings {
        let first_keystroke = binding.remaining_keystrokes.first();
        let group = groups.iter_mut().find(|group| {
            group
                .first()
                .is_some_and(|existing| existing.remaining_keystrokes.first() == first_keystroke)
        });
        let Some(group) = group else {
            groups.push(vec![binding]);
            continue;
        };
        // Candidates come in precedence order, so the first binding for a sequence is the one
        // dispatch tries first. Dispatch only moves on to the next binding when nothing on the
        // focus path handles the first action. Like GPUI's shortcut display, the popup treats the
        // later bindings as shadowed and shows only the first. Bindings for the same sequence
        // share a first keystroke, so checking the group is enough.
        if group
            .iter()
            .any(|existing| existing.remaining_keystrokes == binding.remaining_keystrokes)
        {
            continue;
        }
        group.push(binding);
    }

    let mut result = Vec::new();
    for bindings in groups {
        // A group row would hide what the next keystroke runs, so a binding that completes on it
        // gets its own row and only the longer bindings collapse. Sorting moves the group row
        // down with the other groups, away from that row.
        let longer_count = bindings
            .iter()
            .filter(|binding| binding.remaining_keystrokes.len() > 1)
            .count();

        if longer_count > 1
            && let Some(first_keystroke) = bindings
                .first()
                .and_then(|binding| binding.remaining_keystrokes.first())
                .cloned()
        {
            result.extend(
                bindings
                    .into_iter()
                    .filter(|binding| binding.remaining_keystrokes.len() <= 1)
                    .map(binding_row),
            );
            result.push(PendingBindingRow {
                keystrokes: Rc::from([first_keystroke]),
                action_name: format!("+{longer_count} keybinds").into(),
                is_group: true,
            });
        } else {
            result.extend(bindings.into_iter().map(binding_row));
        }
    }

    result
}

fn binding_row(binding: PendingBinding) -> PendingBindingRow {
    PendingBindingRow {
        keystrokes: binding.remaining_keystrokes.into(),
        action_name: binding.action_name,
        is_group: false,
    }
}

const MAX_KEY_COLUMN_WIDTH: Rems = rems(12.5);
const MIN_KEY_COLUMN_WIDTH: Rems = rems(3.);
const HEADER_LABEL: &str = "is waiting for more keys…";

#[derive(IntoElement)]
pub(crate) struct PendingBindings {
    id: &'static str,
    pending_keystrokes: Rc<[KeybindingKeystroke]>,
    bindings: Rc<[PendingBindingRow]>,
    scroll_handle: ScrollHandle,
    max_width: Pixels,
    max_content_height: Pixels,
}

impl PendingBindings {
    pub(crate) fn new(
        id: &'static str,
        pending_keystrokes: Rc<[KeybindingKeystroke]>,
        bindings: Rc<[PendingBindingRow]>,
        scroll_handle: ScrollHandle,
        max_width: Pixels,
        max_content_height: Pixels,
    ) -> Self {
        Self {
            id,
            pending_keystrokes,
            bindings,
            scroll_handle,
            max_width,
            max_content_height,
        }
    }

    fn keybinding(keystrokes: Rc<[KeybindingKeystroke]>, cx: &App) -> KeyBinding {
        KeyBinding::from_keystrokes(keystrokes, KeyBinding::is_vim_mode(cx)).color(Color::Accent)
    }

    fn keybinding_width(
        keystrokes: Rc<[KeybindingKeystroke]>,
        window: &mut Window,
        cx: &mut App,
    ) -> Pixels {
        Self::keybinding(keystrokes, cx)
            .into_any_element()
            .layout_as_root(
                size(AvailableSpace::MaxContent, AvailableSpace::MaxContent),
                window,
                cx,
            )
            .width
    }

    fn label_width(text: SharedString, window: &mut Window, cx: &mut App) -> Pixels {
        Label::new(text)
            .size(LabelSize::Small)
            .single_line()
            .into_any_element()
            .layout_as_root(
                size(AvailableSpace::MaxContent, AvailableSpace::MaxContent),
                window,
                cx,
            )
            .width
    }

    fn key_cell(
        keystrokes: Rc<[KeybindingKeystroke]>,
        keybinding_width: Pixels,
        key_column_width: Pixels,
        cx: &App,
    ) -> AnyElement {
        let keybinding = Self::keybinding(keystrokes, cx);
        if keybinding_width <= key_column_width {
            return keybinding.into_any_element();
        }
        let background = cx.theme().colors().elevated_surface_background;
        h_flex()
            .relative()
            .w(key_column_width)
            .overflow_hidden()
            .child(keybinding)
            .child(GradientFade::new(background, background, background))
            .into_any_element()
    }
}

impl RenderOnce for PendingBindings {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let has_bindings = !self.bindings.is_empty();
        let rem_size = window.rem_size();
        // Measure the actual key components so the fixed header and scrolling rows
        // share a column width. Labels get their full width first and the key column takes
        // what's left of the panel, so a long sequence fades out instead of cropping labels.
        let pending_keystrokes_width =
            Self::keybinding_width(self.pending_keystrokes.clone(), window, cx);
        let binding_widths = self
            .bindings
            .iter()
            .map(|binding| Self::keybinding_width(binding.keystrokes.clone(), window, cx))
            .collect::<Vec<_>>();
        let widest_label = self
            .bindings
            .iter()
            .map(|binding| binding.action_name.clone())
            .chain([SharedString::from(HEADER_LABEL)])
            .map(|text| Self::label_width(text, window, cx))
            .fold(px(0.), Pixels::max);
        // `px_2` on both sides, the `gap_2` between columns, and the panel's 1px borders.
        let horizontal_chrome = rems(1.5).to_pixels(rem_size) + px(2.);
        let key_column_limit = (self.max_width - horizontal_chrome - widest_label)
            .max(MIN_KEY_COLUMN_WIDTH.to_pixels(rem_size))
            .min(MAX_KEY_COLUMN_WIDTH.to_pixels(rem_size));
        let key_column_width = binding_widths
            .iter()
            .copied()
            .fold(pending_keystrokes_width, Pixels::max)
            .min(key_column_limit);
        let content = h_flex()
            .items_start()
            .gap_2()
            .px_2()
            .py_1()
            .child(
                v_flex()
                    .w(key_column_width)
                    .gap_1()
                    .items_end()
                    .flex_shrink_0()
                    .children(self.bindings.iter().zip(&binding_widths).map(
                        |(binding, keybinding_width)| {
                            h_flex().h_6().flex_none().child(Self::key_cell(
                                binding.keystrokes.clone(),
                                *keybinding_width,
                                key_column_width,
                                cx,
                            ))
                        },
                    )),
            )
            .child(
                v_flex()
                    .gap_1()
                    .flex_1()
                    .min_w_0()
                    .children(self.bindings.iter().map(|binding| {
                        h_flex().h_6().flex_none().w_full().min_w_0().child(
                            Label::new(binding.action_name.clone())
                                .size(LabelSize::Small)
                                .color(if binding.is_group {
                                    Color::Success
                                } else {
                                    Color::Default
                                })
                                .single_line()
                                .truncate(),
                        )
                    })),
            );

        v_flex()
            // Title section
            .child(
                h_flex()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .child(
                        h_flex()
                            .w(key_column_width)
                            .flex_shrink_0()
                            .justify_end()
                            .child(Self::key_cell(
                                self.pending_keystrokes,
                                pending_keystrokes_width,
                                key_column_width,
                                cx,
                            )),
                    )
                    .child(
                        Label::new(HEADER_LABEL)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .single_line()
                            .truncate(),
                    ),
            )
            .when(has_bindings, |element| {
                element.child(Divider::horizontal().color(DividerColor::BorderFaded))
            })
            .when(has_bindings, |element| {
                element.child(
                    div()
                        .max_h(self.max_content_height)
                        .child(
                            div()
                                .id(self.id)
                                .overflow_y_scroll()
                                .track_scroll(&self.scroll_handle)
                                .max_h(self.max_content_height)
                                .child(content),
                        )
                        .vertical_scrollbar_for(&self.scroll_handle, window, cx),
                )
            })
    }
}

