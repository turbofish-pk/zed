use crate::{
    Anchor, AnchorRangeExt, DisplayPoint, DisplayRow, Editor, EditorSettings, EditorSnapshot,
    GlobalDiagnosticRenderer, HighlightKey, Hover,
    display_map::{ToDisplayPoint, is_invisible},
    editor_settings::EditorSettingsScrollbarProxy,
    hover_links::{InlayHighlight, RangeInEditor},
    movement::TextLayoutDetails,
    scroll::ScrollAmount,
};
use anyhow::Context as _;
use gpui::{
    AnyElement, App, AsyncWindowContext, Bounds, Context, Entity, Focusable as _, FontWeight, Hsla,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels, ScrollHandle, Size,
    StatefulInteractiveElement, StyleRefinement, Styled, Subscription, Task, TaskExt,
    TextStyleRefinement, Window, canvas, div, px,
};
use language::{CharKind, DiagnosticEntry, Language, LanguageRegistry};
use lsp::DiagnosticSeverity;
use markdown::{CopyButtonVisibility, Markdown, MarkdownElement, MarkdownStyle};
use multi_buffer::{MultiBufferOffset, MultiBufferSnapshot, ToOffset, ToPoint};
use project::{HoverBlock, HoverBlockKind, InlayHintLabelPart};
use settings::Settings;
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
};
use std::{ops::Range, sync::Arc, time::Duration};
use std::{path::PathBuf, rc::Rc};
use theme_settings::ThemeSettings;
use ui::{CopyButton, Scrollbars, WithScrollbar, prelude::*, theme_is_transparent};
use url::Url;
use util::TryFutureExt;
use workspace::{OpenOptions, OpenVisible, Workspace};

use crate::debugger_hover::{
    DEBUGGER_HOVER_MIN_WIDTH, DebuggerHoverView, build_debugger_hover_view,
};

pub const MIN_POPOVER_CHARACTER_WIDTH: f32 = 20.;
pub const MIN_POPOVER_LINE_HEIGHT: f32 = 4.;
pub const POPOVER_RIGHT_OFFSET: Pixels = px(8.0);
pub const HOVER_POPOVER_GAP: Pixels = px(10.);
const MAX_HOVER_BYTES: usize = 100_000;
const DEBUGGER_HOVER_HEADER_HEIGHT: Pixels = px(24.);

/// Bindable action which uses the most recent selection head to trigger a hover
pub fn hover(editor: &mut Editor, _: &Hover, window: &mut Window, cx: &mut Context<Editor>) {
    let head = editor.selections.newest_anchor().head();
    show_hover(editor, head, true, window, cx);
}

/// The internal hover action dispatches between `show_hover` or `hide_hover`
/// depending on whether a point to hover over is provided.
pub fn hover_at(
    editor: &mut Editor,
    anchor: Option<Anchor>,
    mouse_position: Option<gpui::Point<Pixels>>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    editor.hover_state.hint_hover_task = None;
    if EditorSettings::get_global(cx).hover_popover_enabled {
        if editor.hover_state.debugger_menu_open(cx) {
            editor.hover_state.hiding_delay_task = None;
            return;
        }
        if show_keyboard_hover(editor, window, cx) {
            return;
        }

        if let Some(anchor) = anchor {
            editor.hover_state.hiding_delay_task = None;
            editor.hover_state.closest_mouse_distance = None;
            show_hover(editor, anchor, false, window, cx);
        } else if !editor.hover_state.visible() {
            editor.hover_state.info_task = None;
        } else {
            let settings = EditorSettings::get_global(cx);
            if !settings.hover_popover_sticky {
                hide_hover(editor, cx);
                return;
            }

            let mut getting_closer = false;
            if let Some(mouse_position) = mouse_position {
                getting_closer = editor.hover_state.is_mouse_getting_closer(mouse_position);
            }

            // If we are moving away and a timer is already running, just let it count down.
            if !getting_closer && editor.hover_state.hiding_delay_task.is_some() {
                return;
            }

            // If we are moving closer, or if no timer is running at all, start/restart the timer.
            let delay = Duration::from_millis(settings.hover_popover_hiding_delay.0);
            let task = cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                this.update(cx, |editor, cx| {
                    if !editor.hover_state.debugger_menu_open(cx) {
                        hide_popovers(editor, cx);
                    }
                })
                .ok();
            });
            editor.hover_state.hiding_delay_task = Some(task);
        }
    }
}

pub fn show_keyboard_hover(
    editor: &mut Editor,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> bool {
    if let Some(anchor) = editor.hover_state.info_popovers.iter().find_map(|p| {
        if *p.keyboard_grace.borrow() {
            p.anchor
        } else {
            None
        }
    }) {
        show_hover(editor, anchor, false, window, cx);
        return true;
    }

    if let Some(anchor) = editor
        .hover_state
        .diagnostic_popover
        .as_ref()
        .and_then(|d| {
            if *d.keyboard_grace.borrow() {
                Some(d.anchor)
            } else {
                None
            }
        })
    {
        show_hover(editor, anchor, false, window, cx);
        return true;
    }

    false
}

pub struct InlayHover {
    pub(crate) range: InlayHighlight,
    pub tooltip: HoverBlock,
}

pub fn find_hovered_hint_part(
    label_parts: Vec<InlayHintLabelPart>,
    offset_in_label: usize,
) -> Option<(InlayHintLabelPart, Range<usize>)> {
    let mut part_start = 0;
    for part in label_parts {
        let part_end = part_start + part.value.len();
        if (part_start..part_end).contains(&offset_in_label) {
            return Some((part, part_start..part_end));
        }
        part_start = part_end;
    }
    None
}

pub fn hover_at_inlay(
    editor: &mut Editor,
    inlay_hover: InlayHover,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Option<Task<()>> {
    if !EditorSettings::get_global(cx).hover_popover_enabled || editor.pending_rename.is_some() {
        return None;
    }
    let project = editor.project.clone()?;

    let symbol_range = RangeInEditor::Inlay(inlay_hover.range);
    if editor
        .hover_state
        .info_popovers
        .iter()
        .any(|popover| popover.symbol_range == symbol_range)
    {
        editor.hover_state.hiding_delay_task = None;
        return Some(Task::ready(()));
    }

    let hover_popover_delay = EditorSettings::get_global(cx).hover_popover_delay.0;

    editor.hover_state.info_task = None;
    editor.hover_state.hiding_delay_task = None;
    editor.hover_state.closest_mouse_distance = None;

    Some(cx.spawn_in(window, async move |editor, cx| {
        cx.background_executor()
            .timer(Duration::from_millis(hover_popover_delay))
            .await;
        let language_registry = project.read_with(cx, |project, _| project.languages().clone());
        let parsed_content =
            parse_blocks(&[inlay_hover.tooltip], Some(&language_registry), None, cx);

        editor
            .update(cx, |editor, cx| {
                let subscription = parsed_content
                    .as_ref()
                    .map(|parsed_content| cx.observe(parsed_content, |_, _, cx| cx.notify()));
                editor.hover_state.diagnostic_popover = None;
                editor.hover_state.stable_debugger_hover_origin = None;
                editor.hover_state.info_popovers = vec![InfoPopover {
                    symbol_range,
                    fallback_state: None,
                    parsed_content,
                    debugger_hover: None,
                    scroll_handle: ScrollHandle::new(),
                    keyboard_grace: Rc::new(RefCell::new(false)),
                    anchor: None,
                    last_bounds: Rc::new(Cell::new(None)),
                    _subscriptions: subscription.into_iter().collect(),
                }];
                cx.notify();
            })
            .ok();
    }))
}

/// Hides the type information popup.
/// Triggered by the `Hover` action when the cursor is not over a symbol or when the
/// selections changed.
pub fn hide_hover(editor: &mut Editor, cx: &mut Context<Editor>) -> bool {
    editor.hover_state.hint_hover_task = None;
    hide_popovers(editor, cx)
}

/// Queries the LSP and shows type info and documentation
/// about the symbol the mouse is currently hovering over.
/// Triggered by the `Hover` action when the cursor may be over a symbol.
fn show_hover(
    editor: &mut Editor,
    anchor: Anchor,
    ignore_timeout: bool,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Option<()> {
    editor.hover_state.hint_hover_task = None;
    if editor.pending_rename.is_some() {
        return None;
    }

    let snapshot = editor.snapshot(window, cx);

    let (buffer_position, _) = editor
        .buffer
        .read(cx)
        .snapshot(cx)
        .anchor_to_buffer_anchor(anchor)?;
    let buffer = editor.buffer.read(cx).buffer(buffer_position.buffer_id)?;

    let language_registry = editor
        .project()
        .map(|project| project.read(cx).languages().clone());
    let debugger_project = editor.project().map(|project| project.downgrade());
    let provider = editor.semantics_provider.clone()?;

    editor.hover_state.hiding_delay_task = None;
    editor.hover_state.closest_mouse_distance = None;

    if !ignore_timeout {
        if same_info_hover(editor, &snapshot, anchor)
            || same_diagnostic_hover(editor, &snapshot, anchor)
            || editor.hover_state.diagnostic_popover.is_some()
        {
            // Hover triggered from same location as last time. Don't show again.
            return None;
        } else {
            hide_hover(editor, cx);
        }
    }

    let hover_popover_delay = EditorSettings::get_global(cx).hover_popover_delay.0;
    let all_diagnostics_active = editor.all_diagnostics_active();
    let active_group_id = editor.active_diagnostic_group_id();

    let renderer = GlobalDiagnosticRenderer::global(cx);
    let task = cx.spawn_in(window, async move |this, cx| {
        async move {
            // If we need to delay, delay a set amount initially before making the lsp request
            let delay = if ignore_timeout {
                None
            } else {
                let lsp_request_early = hover_popover_delay / 2;
                cx.background_executor()
                    .timer(Duration::from_millis(
                        hover_popover_delay - lsp_request_early,
                    ))
                    .await;

                // Construct delay task to wait for later
                let total_delay = Some(
                    cx.background_executor()
                        .timer(Duration::from_millis(lsp_request_early)),
                );
                total_delay
            };

            let mut hover_request =
                cx.update(|_, cx| provider.hover(&buffer, buffer_position, cx))?;
            let debugger_request =
                cx.update(|_, cx| provider.debugger_hover(&buffer, buffer_position, cx))?;

            if let Some(delay) = delay {
                delay.await;
            }
            let offset = anchor.to_offset(&snapshot.buffer_snapshot());
            let local_diagnostic = if all_diagnostics_active {
                None
            } else {
                snapshot
                    .buffer_snapshot()
                    .diagnostics_with_buffer_ids_in_range::<MultiBufferOffset>(offset..offset)
                    .filter(|(_, diagnostic)| {
                        Some(diagnostic.diagnostic.group_id) != active_group_id
                    })
                    // Find the entry with the most specific range
                    .min_by_key(|(_, entry)| entry.range.end - entry.range.start)
            };

            let diagnostic_popover = if let Some((buffer_id, local_diagnostic)) = local_diagnostic {
                let group = snapshot
                    .buffer_snapshot()
                    .diagnostic_group(buffer_id, local_diagnostic.diagnostic.group_id)
                    .collect::<Vec<_>>();
                let point_range = local_diagnostic
                    .range
                    .start
                    .to_point(&snapshot.buffer_snapshot())
                    ..local_diagnostic
                        .range
                        .end
                        .to_point(&snapshot.buffer_snapshot());
                let markdown = cx.update(|_, cx| {
                    renderer
                        .as_ref()
                        .and_then(|renderer| {
                            renderer.render_hover(
                                group,
                                point_range,
                                buffer_id,
                                language_registry.clone(),
                                cx,
                            )
                        })
                        .context("no rendered diagnostic")
                })??;

                let (background_color, border_color) = cx.update(|_, cx| {
                    let status_colors = cx.theme().status();
                    match local_diagnostic.diagnostic.severity {
                        DiagnosticSeverity::ERROR => {
                            (status_colors.error_background, status_colors.error_border)
                        }
                        DiagnosticSeverity::WARNING => (
                            status_colors.warning_background,
                            status_colors.warning_border,
                        ),
                        DiagnosticSeverity::INFORMATION => {
                            (status_colors.info_background, status_colors.info_border)
                        }
                        DiagnosticSeverity::HINT => {
                            (status_colors.hint_background, status_colors.hint_border)
                        }
                        _ => (
                            status_colors.ignored_background,
                            status_colors.ignored_border,
                        ),
                    }
                })?;

                let subscription =
                    this.update(cx, |_, cx| cx.observe(&markdown, |_, _, cx| cx.notify()))?;

                let local_diagnostic = DiagnosticEntry::new(
                    snapshot
                        .buffer_snapshot()
                        .anchor_before(local_diagnostic.range.start)
                        ..snapshot
                            .buffer_snapshot()
                            .anchor_after(local_diagnostic.range.end),
                    local_diagnostic.diagnostic.to_owned(),
                );

                let scroll_handle = ScrollHandle::new();

                Some(DiagnosticPopover {
                    local_diagnostic,
                    markdown,
                    border_color,
                    scroll_handle,
                    background_color,
                    keyboard_grace: Rc::new(RefCell::new(ignore_timeout)),
                    anchor,
                    last_bounds: Rc::new(Cell::new(None)),
                    _subscription: subscription,
                })
            } else {
                None
            };

            this.update(cx, |this, _| {
                this.hover_state.diagnostic_popover = diagnostic_popover;
            })?;

            let invisible_char = if let Some(invisible) = snapshot
                .buffer_snapshot()
                .chars_at(anchor)
                .next()
                .filter(|&c| is_invisible(c))
            {
                let after = snapshot.buffer_snapshot().anchor_after(
                    anchor.to_offset(&snapshot.buffer_snapshot()) + invisible.len_utf8(),
                );
                Some((invisible, anchor..after))
            } else if let Some(invisible) = snapshot
                .buffer_snapshot()
                .reversed_chars_at(anchor)
                .next()
                .filter(|&c| is_invisible(c))
            {
                let before = snapshot.buffer_snapshot().anchor_before(
                    anchor.to_offset(&snapshot.buffer_snapshot()) - invisible.len_utf8(),
                );

                Some((invisible, before..anchor))
            } else {
                None
            };

            let debugger_response = match debugger_request {
                Some(request) => request.await,
                None => None,
            };
            let show_debugger_hover_only = debugger_response.is_some();
            let hovers_response = if show_debugger_hover_only {
                Vec::new()
            } else {
                match hover_request.take() {
                    Some(request) => request.await.unwrap_or_default(),
                    None => Vec::new(),
                }
            };
            let snapshot = this.update_in(cx, |this, window, cx| this.snapshot(window, cx))?;
            let mut hover_highlights = Vec::with_capacity(hovers_response.len());
            let mut info_popovers = Vec::with_capacity(
                hovers_response.len() + if invisible_char.is_some() { 1 } else { 0 },
            );

            if !show_debugger_hover_only && let Some((invisible, range)) = invisible_char {
                let blocks = vec![HoverBlock {
                    text: format!("Unicode character U+{:02X}", invisible as u32),
                    kind: HoverBlockKind::PlainText,
                }];
                let parsed_content = parse_blocks(&blocks, language_registry.as_ref(), None, cx);
                let scroll_handle = ScrollHandle::new();
                let subscriptions = this
                    .update(cx, |_, cx| {
                        parsed_content
                            .as_ref()
                            .map(|parsed_content| {
                                vec![cx.observe(parsed_content, |_, _, cx| cx.notify())]
                            })
                            .unwrap_or_default()
                    })
                    .ok()
                    .unwrap_or_default();
                info_popovers.push(InfoPopover {
                    symbol_range: RangeInEditor::Text(range),
                    fallback_state: None,
                    parsed_content,
                    debugger_hover: None,
                    scroll_handle,
                    keyboard_grace: Rc::new(RefCell::new(ignore_timeout)),
                    anchor: Some(anchor),
                    last_bounds: Rc::new(Cell::new(None)),
                    _subscriptions: subscriptions,
                })
            }

            let doc_link_task = if show_debugger_hover_only {
                None
            } else {
                this.update(cx, |editor, cx| {
                    editor.document_links_at(buffer.clone(), buffer_position, cx)
                })
                .ok()
                .flatten()
            };
            let doc_link_tooltips = match doc_link_task {
                Some(task) => task
                    .await
                    .into_iter()
                    .filter_map(|(_, link)| {
                        let multi_buffer_range = snapshot
                            .buffer_snapshot()
                            .buffer_anchor_range_to_anchor_range(link.range.clone())?;
                        let tooltip = link.tooltip?;
                        Some((multi_buffer_range, tooltip))
                    })
                    .collect::<Vec<_>>(),
                None => Vec::new(),
            };

            let buffer_snapshot = snapshot.buffer_snapshot();
            let mut debugger_view = None;
            if let Some(debugger_response) = debugger_response {
                let range = snapshot
                    .buffer_snapshot()
                    .buffer_anchor_range_to_anchor_range(debugger_response.range)
                    .unwrap_or_else(|| anchor..anchor);
                let debugger_hover = build_debugger_hover_view(
                    Some(debugger_response.data),
                    debugger_project.clone(),
                    cx,
                );
                if let Some(view) = debugger_hover {
                    debugger_view = Some(view.entity_id());
                    hover_highlights.push(range.clone());
                    let subscription =
                        this.update(cx, |_, cx| cx.observe(&view, |_, _, cx| cx.notify()))?;
                    info_popovers.push(InfoPopover {
                        symbol_range: RangeInEditor::Text(range),
                        fallback_state: None,
                        parsed_content: None,
                        debugger_hover: Some(view),
                        scroll_handle: ScrollHandle::new(),
                        keyboard_grace: Rc::new(RefCell::new(ignore_timeout)),
                        anchor: Some(anchor),
                        last_bounds: Rc::new(Cell::new(None)),
                        _subscriptions: vec![subscription],
                    });
                }
            }

            for hover_result in hovers_response {
                let project::Hover {
                    contents: blocks,
                    range: hover_range,
                    language,
                } = hover_result;

                // Create symbol range of anchors for highlighting and filtering of future requests.
                let (range, fallback_state) = hover_range
                    .and_then(|range| buffer_snapshot.buffer_anchor_range_to_anchor_range(range))
                    .filter(|range| !range.to_offset(buffer_snapshot).is_empty())
                    .map(|range| (range, None))
                    .unwrap_or_else(|| {
                        let range = fallback_hover_range(buffer_snapshot, anchor);
                        let fallback_state = FallbackHoverState::new(buffer_snapshot, &range);
                        (range, Some(fallback_state))
                    });

                let parsed_content =
                    parse_blocks(&blocks, language_registry.as_ref(), language, cx);
                if parsed_content.is_none() {
                    continue;
                }
                let scroll_handle = ScrollHandle::new();
                hover_highlights.push(range.clone());
                let subscriptions = this
                    .update(cx, |_, cx| {
                        let mut subscriptions = Vec::new();
                        if let Some(parsed_content) = parsed_content.as_ref() {
                            subscriptions.push(cx.observe(parsed_content, |_, _, cx| cx.notify()));
                        }
                        subscriptions
                    })
                    .ok()
                    .unwrap_or_default();
                info_popovers.push(InfoPopover {
                    symbol_range: RangeInEditor::Text(range),
                    fallback_state,
                    parsed_content,
                    debugger_hover: None,
                    scroll_handle,
                    keyboard_grace: Rc::new(RefCell::new(ignore_timeout)),
                    anchor: Some(anchor),
                    last_bounds: Rc::new(Cell::new(None)),
                    _subscriptions: subscriptions,
                });
            }

            if !show_debugger_hover_only {
                for (multi_buffer_range, tooltip) in doc_link_tooltips {
                    let blocks = vec![HoverBlock {
                        text: tooltip.to_string(),
                        kind: HoverBlockKind::Markdown,
                    }];
                    let parsed_content =
                        parse_blocks(&blocks, language_registry.as_ref(), None, cx);
                    let scroll_handle = ScrollHandle::new();
                    let subscription = this
                        .update(cx, |_, cx| {
                            parsed_content.as_ref().map(|parsed_content| {
                                cx.observe(parsed_content, |_, _, cx| cx.notify())
                            })
                        })
                        .ok()
                        .flatten();
                    info_popovers.push(InfoPopover {
                        symbol_range: RangeInEditor::Text(multi_buffer_range),
                        fallback_state: None,
                        parsed_content,
                        debugger_hover: None,
                        scroll_handle,
                        keyboard_grace: Rc::new(RefCell::new(ignore_timeout)),
                        anchor: Some(anchor),
                        last_bounds: Rc::new(Cell::new(None)),
                        _subscriptions: subscription.into_iter().collect(),
                    });
                }
            }

            this.update_in(cx, |editor, window, cx| {
                if hover_highlights.is_empty() {
                    editor.clear_background_highlights(HighlightKey::HoverState, cx);
                } else {
                    // Highlight the selected symbol using a background highlight
                    editor.highlight_background(
                        HighlightKey::HoverState,
                        &hover_highlights,
                        |_, theme| theme.colors().element_hover, // todo update theme
                        cx,
                    );
                }

                if show_debugger_hover_only {
                    editor.hover_state.diagnostic_popover = None;
                }
                editor.hover_state.stable_debugger_hover_origin = None;
                editor.hover_state.info_popovers = info_popovers;
                cx.notify();
                window.refresh();
            })?;

            if let (Some(view_id), Some(request)) = (debugger_view, hover_request) {
                let lsp_hovers = request.await.unwrap_or_default();
                let language = lsp_hovers.iter().find_map(|hover| hover.language.clone());
                let blocks = lsp_hovers
                    .into_iter()
                    .flat_map(|hover| hover.contents)
                    .collect::<Vec<_>>();
                if let Some(markdown) =
                    parse_blocks(&blocks, language_registry.as_ref(), language, cx)
                {
                    this.update_in(cx, |editor, window, cx| {
                        if let Some(popover) =
                            editor.hover_state.info_popovers.iter().find(|popover| {
                                popover
                                    .debugger_hover
                                    .as_ref()
                                    .is_some_and(|view| view.entity_id() == view_id)
                            })
                        {
                            let symbol_range = popover.symbol_range.clone();
                            let keyboard_grace = popover.keyboard_grace.clone();
                            let anchor = popover.anchor;
                            let subscription = cx.observe(&markdown, |_, _, cx| cx.notify());
                            editor.hover_state.info_popovers.push(InfoPopover {
                                symbol_range,
                                parsed_content: Some(markdown),
                                debugger_hover: None,
                                scroll_handle: ScrollHandle::new(),
                                keyboard_grace,
                                anchor,
                                last_bounds: Rc::new(Cell::new(None)),
                                fallback_state: None,
                                _subscriptions: vec![subscription],
                            });
                            cx.notify();
                            window.refresh();
                        }
                    })?;
                }
            }

            anyhow::Ok(())
        }
        .log_err()
        .await
    });

    editor.hover_state.info_task = Some(task);
    None
}

/// The range to use for a hover result whose language server reported no usable range.
fn fallback_hover_range(buffer_snapshot: &MultiBufferSnapshot, anchor: Anchor) -> Range<Anchor> {
    let offset = anchor.to_offset(buffer_snapshot);
    let syntax_offset = if offset == buffer_snapshot.len()
        || buffer_snapshot
            .chars_at(offset)
            .next()
            .is_some_and(char::is_whitespace)
    {
        match buffer_snapshot.reversed_chars_at(offset).next() {
            Some(character) if !character.is_whitespace() => offset - character.len_utf8(),
            _ if offset == buffer_snapshot.len() => return anchor..anchor,
            _ => offset,
        }
    } else {
        offset
    };
    if buffer_snapshot
        .chars_at(syntax_offset)
        .next()
        .is_some_and(|character| !character.is_whitespace())
        && let Some((_, range)) = buffer_snapshot.syntax_ancestor(syntax_offset..syntax_offset)
        && !range.is_empty()
        && (syntax_offset == offset || range.end == offset)
    {
        return buffer_snapshot.anchor_after(range.start)..buffer_snapshot.anchor_before(range.end);
    }

    let classifier = buffer_snapshot.char_classifier_at(offset);
    if buffer_snapshot
        .chars_at(offset)
        .next()
        .is_some_and(|character| classifier.kind(character) == CharKind::Punctuation)
    {
        return anchor..anchor;
    }

    let (range, kind) = buffer_snapshot.surrounding_word(offset, None);
    if kind != Some(CharKind::Word) {
        return anchor..anchor;
    }
    buffer_snapshot.anchor_after(range.start)..buffer_snapshot.anchor_before(range.end)
}

fn same_info_hover(editor: &mut Editor, snapshot: &EditorSnapshot, anchor: Anchor) -> bool {
    editor.hover_state.info_popovers.iter_mut().any(
        |InfoPopover {
             symbol_range,
             fallback_state,
             ..
         }| {
            symbol_range
                .as_text_range()
                .map(|range| {
                    let buffer_snapshot = snapshot.buffer_snapshot();
                    let hover_range = range.to_offset(buffer_snapshot);
                    let offset = anchor.to_offset(buffer_snapshot);
                    if let Some(fallback_state) = fallback_state {
                        if fallback_state.edit_count != buffer_snapshot.edit_count() {
                            return false;
                        }
                        if offset == hover_range.end {
                            if fallback_state.non_text_state_update_count
                                != buffer_snapshot.non_text_state_update_count()
                            {
                                *fallback_state = FallbackHoverState::new(buffer_snapshot, &range);
                            }
                            return fallback_state.includes_end;
                        }
                    }
                    // LSP returns a hover result for the end index of ranges that should be hovered, so we need to
                    // use an inclusive range here to check if we should dismiss the popover
                    (hover_range.start..=hover_range.end).contains(&offset)
                })
                .unwrap_or(false)
        },
    )
}

fn same_diagnostic_hover(editor: &Editor, snapshot: &EditorSnapshot, anchor: Anchor) -> bool {
    editor
        .hover_state
        .diagnostic_popover
        .as_ref()
        .map(|diagnostic| {
            let hover_range = diagnostic
                .local_diagnostic
                .range
                .to_offset(&snapshot.buffer_snapshot());
            let offset = anchor.to_offset(&snapshot.buffer_snapshot());

            // Here we do basically the same as in `same_info_hover`, see comment there for an explanation
            (hover_range.start..=hover_range.end).contains(&offset)
        })
        .unwrap_or(false)
}

fn parse_blocks(
    blocks: &[HoverBlock],
    language_registry: Option<&Arc<LanguageRegistry>>,
    language: Option<Arc<Language>>,
    cx: &mut AsyncWindowContext,
) -> Option<Entity<Markdown>> {
    let combined_text = combine_hover_blocks(blocks);

    if combined_text.trim().is_empty() {
        return None;
    }

    cx.new_window_entity(|_window, cx| {
        Markdown::new(
            combined_text.into(),
            language_registry.cloned(),
            language.map(|language| language.name()),
            cx,
        )
    })
    .ok()
}

fn combine_hover_blocks(blocks: &[HoverBlock]) -> String {
    let mut combined = String::new();
    let mut budget = MAX_HOVER_BYTES;
    let mut dropped_blocks = false;
    let last_block_index = blocks
        .iter()
        .rposition(|block| !block.text.trim().is_empty());
    for (index, block) in blocks.iter().enumerate() {
        let text = block.text.trim();
        if text.is_empty() {
            continue;
        }
        let separator = if combined.is_empty() { "" } else { "\n\n" };
        let is_last_block = Some(index) == last_block_index;
        let reserved_for_dropped_marker = if is_last_block { 0 } else { "\n\n…".len() };
        let mut truncated_inline = false;
        let piece = match &block.kind {
            project::HoverBlockKind::PlainText | project::HoverBlockKind::Markdown => {
                let Some(block_budget) =
                    budget.checked_sub(separator.len() + reserved_for_dropped_marker)
                else {
                    dropped_blocks = true;
                    break;
                };
                match fit_in_budget(text.len(), block_budget) {
                    BudgetFit::Fits => Cow::Borrowed(text),
                    BudgetFit::TooSmall => {
                        dropped_blocks = true;
                        break;
                    }
                    BudgetFit::NeedsTruncation => {
                        truncated_inline = true;
                        Cow::Owned(truncate_hover_markdown(text, block_budget))
                    }
                }
            }
            project::HoverBlockKind::Code { language } => {
                let language = language.replace(['`', '\r', '\n'], "");
                let fence = wrapping_code_fence(text);
                let overhead = separator.len()
                    + reserved_for_dropped_marker
                    + fence.len() * 2
                    + language.len()
                    + "\n\n".len();
                let Some(block_budget) = budget.checked_sub(overhead) else {
                    dropped_blocks = true;
                    break;
                };
                let text = match fit_in_budget(text.len(), block_budget) {
                    BudgetFit::Fits => Cow::Borrowed(text),
                    BudgetFit::TooSmall => {
                        dropped_blocks = true;
                        break;
                    }
                    BudgetFit::NeedsTruncation => {
                        truncated_inline = true;
                        Cow::Owned(format!(
                            "{}…",
                            truncated_to_byte_budget(text, block_budget.saturating_sub("…".len()))
                        ))
                    }
                };
                Cow::Owned(format!("{fence}{language}\n{text}\n{fence}"))
            }
        };
        budget = budget.saturating_sub(separator.len() + piece.len());
        combined.push_str(separator);
        combined.push_str(&piece);
        if truncated_inline {
            dropped_blocks = !is_last_block;
            break;
        }
    }
    if dropped_blocks {
        if !combined.is_empty() {
            combined.push_str("\n\n");
        }
        combined.push('…');
    }
    combined
}

enum BudgetFit {
    Fits,
    NeedsTruncation,
    TooSmall,
}

fn fit_in_budget(len: usize, budget: usize) -> BudgetFit {
    if len <= budget {
        BudgetFit::Fits
    } else if budget <= "…".len() {
        BudgetFit::TooSmall
    } else {
        BudgetFit::NeedsTruncation
    }
}

fn truncated_to_byte_budget(text: &str, budget: usize) -> &str {
    let mut end = budget.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn wrapping_code_fence(text: &str) -> String {
    let mut longest_run = 0;
    let mut current_run = 0;
    for byte in text.bytes() {
        if byte == b'`' {
            current_run += 1;
            longest_run = longest_run.max(current_run);
        } else {
            current_run = 0;
        }
    }
    "`".repeat((longest_run + 1).max(3))
}

fn truncate_hover_markdown(text: &str, max_len: usize) -> String {
    let mut cut = max_len.saturating_sub("…".len()).min(text.len());
    loop {
        let mut truncated = truncated_to_byte_budget(text, cut);
        if let Some(&split_byte) = text.as_bytes().get(truncated.len())
            && (split_byte == b'`' || split_byte == b'~')
        {
            let mut end = truncated.len();
            while end > 0 && text.as_bytes()[end - 1] == split_byte {
                end -= 1;
            }
            truncated = &text[..end];
        }
        if let Some(&next_byte) = text.as_bytes().get(truncated.len())
            && next_byte != b'\n'
            && !truncated.ends_with('\n')
            && ends_with_code_fence_line(truncated)
        {
            let line_start = truncated.rfind('\n').map_or(0, |index| index + 1);
            truncated = &text[..line_start];
        }
        let ellipsis = if ends_with_code_fence_line(truncated) {
            "\n…"
        } else {
            "…"
        };
        let closing_fence = unclosed_code_fence(truncated);
        let total_len = truncated.len()
            + ellipsis.len()
            + closing_fence
                .as_ref()
                .map_or(0, |fence| "\n".len() + fence.len());
        if total_len <= max_len || truncated.is_empty() {
            return match closing_fence {
                Some(fence) => format!("{truncated}{ellipsis}\n{fence}"),
                None => format!("{truncated}{ellipsis}"),
            };
        }
        cut = truncated.len().saturating_sub(total_len - max_len);
    }
}

fn ends_with_code_fence_line(text: &str) -> bool {
    text.lines()
        .next_back()
        .is_some_and(|line| parse_code_fence(line).is_some())
}

struct CodeFence {
    fence_char: char,
    len: usize,
    has_info: bool,
}

fn parse_code_fence(line: &str) -> Option<CodeFence> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let first_char @ ('`' | '~') = trimmed.chars().next()? else {
        return None;
    };
    let len = trimmed.chars().take_while(|&c| c == first_char).count();
    if len < 3 {
        return None;
    }
    let info = trimmed[len..].trim();
    if first_char == '`' && info.contains('`') {
        return None;
    }
    Some(CodeFence {
        fence_char: first_char,
        len,
        has_info: !info.is_empty(),
    })
}

fn unclosed_code_fence(markdown: &str) -> Option<String> {
    let mut open_fence: Option<CodeFence> = None;
    for line in markdown.lines() {
        let Some(fence) = parse_code_fence(line) else {
            continue;
        };
        match &open_fence {
            None => open_fence = Some(fence),
            Some(open) => {
                if fence.fence_char == open.fence_char && fence.len >= open.len && !fence.has_info {
                    open_fence = None;
                }
            }
        }
    }
    open_fence.map(|fence| fence.fence_char.to_string().repeat(fence.len))
}

pub fn hover_markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
    let settings = ThemeSettings::get_global(cx);
    let ui_font_family = settings.ui_font.family.clone();
    let ui_font_features = settings.ui_font.features.clone();
    let ui_font_fallbacks = settings.ui_font.fallbacks.clone();
    let buffer_font_family = settings.buffer_font.family.clone();
    let buffer_font_features = settings.buffer_font.features.clone();
    let buffer_font_fallbacks = settings.buffer_font.fallbacks.clone();
    let buffer_font_weight = settings.buffer_font.weight;

    let mut base_text_style = window.text_style();
    base_text_style.refine(&TextStyleRefinement {
        font_family: Some(ui_font_family),
        font_features: Some(ui_font_features),
        font_fallbacks: ui_font_fallbacks,
        color: Some(cx.theme().colors().editor_foreground),
        ..Default::default()
    });
    MarkdownStyle {
        base_text_style,
        code_block: StyleRefinement::default()
            .my(rems(1.))
            .font_buffer(cx)
            .font_features(buffer_font_features.clone())
            .font_weight(buffer_font_weight),
        inline_code: TextStyleRefinement {
            background_color: Some(cx.theme().colors().background),
            font_family: Some(buffer_font_family),
            font_features: Some(buffer_font_features),
            font_fallbacks: buffer_font_fallbacks,
            font_weight: Some(buffer_font_weight),
            ..Default::default()
        },
        rule_color: cx.theme().colors().border,
        block_quote_border_color: Color::Muted.color(cx),
        block_quote: TextStyleRefinement {
            color: Some(Color::Muted.color(cx)),
            ..Default::default()
        },
        link: TextStyleRefinement {
            color: Some(cx.theme().colors().editor_foreground),
            underline: Some(gpui::UnderlineStyle {
                thickness: px(1.),
                color: Some(cx.theme().colors().editor_foreground),
                wavy: false,
            }),
            ..Default::default()
        },
        syntax: cx.theme().syntax().clone(),
        selection_background_color: cx.theme().colors().element_selection_background,
        heading: StyleRefinement::default()
            .font_weight(FontWeight::BOLD)
            .text_base()
            .mt(rems(1.))
            .mb_0(),
        table_columns_min_size: true,
        ..Default::default()
    }
}

pub fn diagnostics_markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
    let settings = ThemeSettings::get_global(cx);
    let ui_font_family = settings.ui_font.family.clone();
    let ui_font_fallbacks = settings.ui_font.fallbacks.clone();
    let ui_font_features = settings.ui_font.features.clone();
    let buffer_font_family = settings.buffer_font.family.clone();
    let buffer_font_features = settings.buffer_font.features.clone();
    let buffer_font_fallbacks = settings.buffer_font.fallbacks.clone();

    let mut base_text_style = window.text_style();
    base_text_style.refine(&TextStyleRefinement {
        font_family: Some(ui_font_family),
        font_features: Some(ui_font_features),
        font_fallbacks: ui_font_fallbacks,
        color: Some(cx.theme().colors().editor_foreground),
        ..Default::default()
    });
    MarkdownStyle {
        base_text_style,
        code_block: StyleRefinement::default().my(rems(1.)).font_buffer(cx),
        inline_code: TextStyleRefinement {
            background_color: Some(cx.theme().colors().editor_background.opacity(0.5)),
            font_family: Some(buffer_font_family),
            font_features: Some(buffer_font_features),
            font_fallbacks: buffer_font_fallbacks,
            ..Default::default()
        },
        rule_color: cx.theme().colors().border,
        block_quote_border_color: Color::Muted.color(cx),
        block_quote: TextStyleRefinement {
            color: Some(Color::Muted.color(cx)),
            ..Default::default()
        },
        link: TextStyleRefinement {
            color: Some(cx.theme().colors().editor_foreground),
            underline: Some(gpui::UnderlineStyle {
                thickness: px(1.),
                color: Some(cx.theme().colors().editor_foreground),
                wavy: false,
            }),
            ..Default::default()
        },
        syntax: cx.theme().syntax().clone(),
        selection_background_color: cx.theme().colors().element_selection_background,
        height_is_multiple_of_line_height: true,
        heading: StyleRefinement::default()
            .font_weight(FontWeight::BOLD)
            .text_base()
            .mb_0(),
        table_columns_min_size: true,
        ..Default::default()
    }
}

fn parse_file_link(link: &str) -> Option<(PathBuf, Option<String>)> {
    let uri = Url::parse(link).ok().filter(|uri| uri.scheme() == "file")?;
    let fragment = uri.fragment().map(ToOwned::to_owned);
    let path = uri.to_file_path().unwrap_or_else(|_| {
        let encoded = uri.path();

        urlencoding::decode(encoded)
            .map(Cow::into_owned)
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(encoded))
    });

    Some((path, fragment))
}

pub fn open_markdown_url(
    workspace: Option<Entity<Workspace>>,
    link: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some((path, fragment)) = parse_file_link(&link)
        && let Some(workspace) = workspace
    {
        workspace.update(cx, |workspace, cx| {
            let task = workspace.open_abs_path(
                path,
                OpenOptions {
                    visible: Some(OpenVisible::None),
                    ..Default::default()
                },
                window,
                cx,
            );

            cx.spawn_in(window, async move |_, cx| {
                let item = task.await?;
                // Ruby LSP uses URLs with #L1,1-4,4
                // we'll just take the first number and assume it's a line number
                let Some(fragment) = fragment else {
                    return anyhow::Ok(());
                };
                let mut accum = 0u32;
                for c in fragment.chars() {
                    if c >= '0' && c <= '9' && accum < u32::MAX / 2 {
                        accum *= 10;
                        accum += c as u32 - '0' as u32;
                    } else if accum > 0 {
                        break;
                    }
                }
                if accum == 0 {
                    return Ok(());
                }
                let Some(editor) = cx.update(|_, cx| item.act_as::<Editor>(cx))? else {
                    return Ok(());
                };
                editor.update_in(cx, |editor, window, cx| {
                    editor.change_selections(Default::default(), window, cx, |selections| {
                        selections.select_ranges([
                            text::Point::new(accum - 1, 0)..text::Point::new(accum - 1, 0)
                        ]);
                    });
                })
            })
            .detach_and_log_err(cx);
        });
        return;
    }

    if let Some(workspace) = workspace {
        workspace.update(cx, |workspace, cx| {
            workspace.open_url_or_file(&link, None, window, cx);
        });
    } else {
        cx.open_url(&link);
    }
}

#[derive(Default)]
pub struct HoverState {
    pub info_popovers: Vec<InfoPopover>,
    pub diagnostic_popover: Option<DiagnosticPopover>,
    pub info_task: Option<Task<Option<()>>>,
    pub closest_mouse_distance: Option<Pixels>,
    pub hiding_delay_task: Option<Task<()>>,
    pub stable_debugger_hover_origin: Option<gpui::Point<Pixels>>,
    pub(crate) hint_hover_task: Option<Task<()>>,
}

impl HoverState {
    fn debugger_menu_open(&self, cx: &App) -> bool {
        self.info_popovers.iter().any(|popover| {
            popover
                .debugger_hover
                .as_ref()
                .is_some_and(|view| view.read(cx).has_menu())
        })
    }
    pub fn visible(&self) -> bool {
        !self.info_popovers.is_empty() || self.diagnostic_popover.is_some()
    }

    pub fn is_single_debugger_hover(&self) -> bool {
        let [info_popover] = self.info_popovers.as_slice() else {
            return false;
        };

        self.diagnostic_popover.is_none() && info_popover.debugger_hover.is_some()
    }

    pub fn stable_debugger_hover_origin(&self) -> Option<gpui::Point<Pixels>> {
        self.is_single_debugger_hover()
            .then_some(self.stable_debugger_hover_origin)
            .flatten()
    }

    fn keyboard_debugger_hover(&self) -> Option<Entity<DebuggerHoverView>> {
        self.info_popovers.iter().find_map(|info_popover| {
            (*info_popover.keyboard_grace.borrow())
                .then(|| info_popover.debugger_hover.clone())
                .flatten()
        })
    }

    pub fn keyboard_debugger_hover_active(&self) -> bool {
        self.keyboard_debugger_hover().is_some()
    }

    pub fn is_mouse_getting_closer(&mut self, mouse_position: gpui::Point<Pixels>) -> bool {
        if !self.visible() {
            return false;
        }

        let mut popover_bounds = Vec::new();
        for info_popover in &self.info_popovers {
            if let Some(bounds) = info_popover.last_bounds.get() {
                popover_bounds.push(bounds);
            }
        }
        if let Some(diagnostic_popover) = &self.diagnostic_popover {
            if let Some(bounds) = diagnostic_popover.last_bounds.get() {
                popover_bounds.push(bounds);
            }
        }

        if popover_bounds.is_empty() {
            return false;
        }

        let distance = popover_bounds
            .iter()
            .map(|bounds| self.distance_from_point_to_bounds(mouse_position, *bounds))
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(px(f32::MAX));

        if let Some(closest_distance) = self.closest_mouse_distance {
            if distance > closest_distance + px(4.0) {
                return false;
            }
        }

        self.closest_mouse_distance =
            Some(distance.min(self.closest_mouse_distance.unwrap_or(distance)));
        true
    }

    fn distance_from_point_to_bounds(
        &self,
        point: gpui::Point<Pixels>,
        bounds: Bounds<Pixels>,
    ) -> Pixels {
        let center_x = bounds.origin.x + bounds.size.width / 2.;
        let center_y = bounds.origin.y + bounds.size.height / 2.;
        let dx: f32 = ((point.x - center_x).abs() - bounds.size.width / 2.)
            .max(px(0.0))
            .into();
        let dy: f32 = ((point.y - center_y).abs() - bounds.size.height / 2.)
            .max(px(0.0))
            .into();
        px((dx.powi(2) + dy.powi(2)).sqrt())
    }

    pub(crate) fn render(
        &mut self,
        snapshot: &EditorSnapshot,
        visible_rows: Range<DisplayRow>,
        max_size: Size<Pixels>,
        text_layout_details: &TextLayoutDetails,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Option<(DisplayPoint, Vec<AnyElement>)> {
        if !self.visible() {
            return None;
        }
        // If there is a diagnostic, position the popovers based on that.
        // Otherwise use the start of the hover range
        let anchor = self
            .diagnostic_popover
            .as_ref()
            .map(|diagnostic_popover| &diagnostic_popover.local_diagnostic.range.start)
            .or_else(|| {
                self.info_popovers.iter().find_map(|info_popover| {
                    match &info_popover.symbol_range {
                        RangeInEditor::Text(range) => Some(&range.start),
                        RangeInEditor::Inlay(_) => None,
                    }
                })
            })
            .or_else(|| {
                self.info_popovers.iter().find_map(|info_popover| {
                    match &info_popover.symbol_range {
                        RangeInEditor::Text(_) => None,
                        RangeInEditor::Inlay(range) => Some(&range.inlay_position),
                    }
                })
            })?;
        let mut point = anchor.to_display_point(&snapshot.display_snapshot);
        // Clamp the point within the visible rows in case the popup source spans multiple lines
        if visible_rows.end <= point.row() {
            point = crate::movement::up_by_rows(
                &snapshot.display_snapshot,
                point,
                1 + (point.row() - visible_rows.end).0,
                text::SelectionGoal::None,
                true,
                text_layout_details,
            )
            .0;
        } else if point.row() < visible_rows.start {
            point = crate::movement::down_by_rows(
                &snapshot.display_snapshot,
                point,
                (visible_rows.start - point.row()).0,
                text::SelectionGoal::None,
                true,
                text_layout_details,
            )
            .0;
        }

        if !visible_rows.contains(&point.row()) {
            log::error!("Hover popover point out of bounds after moving");
            return None;
        }

        let mut elements = Vec::new();

        if let Some(diagnostic_popover) = self.diagnostic_popover.as_ref() {
            elements.push(diagnostic_popover.render(max_size, window, cx));
        }
        for info_popover in &mut self.info_popovers {
            elements.push(info_popover.render(max_size, window, cx));
        }

        Some((point, elements))
    }

    pub fn focused(&self, window: &mut Window, cx: &mut Context<Editor>) -> bool {
        let mut hover_popover_is_focused = false;
        for info_popover in &self.info_popovers {
            if let Some(view) = &info_popover.debugger_hover
                && let Some(menu) = view.read(cx).context_menu()
                && menu.focus_handle(cx).contains_focused(window, cx)
            {
                hover_popover_is_focused = true;
            }
            if let Some(markdown_view) = &info_popover.parsed_content
                && markdown_view.focus_handle(cx).is_focused(window)
            {
                hover_popover_is_focused = true;
            }
        }
        if let Some(diagnostic_popover) = &self.diagnostic_popover
            && diagnostic_popover
                .markdown
                .focus_handle(cx)
                .is_focused(window)
        {
            hover_popover_is_focused = true;
        }
        hover_popover_is_focused
    }
}

impl Editor {
    fn ensure_debugger_hover_selection_visible(&mut self, cx: &mut Context<Self>) {
        let Some((debugger_hover, scroll_handle)) =
            self.hover_state
                .info_popovers
                .iter()
                .find_map(|info_popover| {
                    if !*info_popover.keyboard_grace.borrow() {
                        return None;
                    }

                    info_popover
                        .debugger_hover
                        .clone()
                        .map(|debugger_hover| (debugger_hover, info_popover.scroll_handle.clone()))
                })
        else {
            return;
        };

        let Some(selected_bounds) = debugger_hover.read(cx).selected_row_bounds() else {
            return;
        };
        let viewport_bounds = scroll_handle.bounds();
        let mut scroll_offset = scroll_handle.offset();
        let initial_offset = scroll_offset;

        if selected_bounds.top() < viewport_bounds.top() {
            scroll_offset.y += viewport_bounds.top() - selected_bounds.top();
        } else if selected_bounds.bottom() > viewport_bounds.bottom() {
            scroll_offset.y += viewport_bounds.bottom() - selected_bounds.bottom();
        }

        if scroll_offset != initial_offset {
            scroll_handle.set_offset(scroll_offset);
            cx.notify();
        }
    }

    pub fn debugger_hover_expand_selected(
        &mut self,
        _: &crate::actions::DebuggerHoverExpandSelected,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(debugger_hover) = self.hover_state.keyboard_debugger_hover() {
            debugger_hover.update(cx, |debugger_hover, cx| debugger_hover.expand_selected(cx));
        }
    }

    pub fn debugger_hover_collapse_selected(
        &mut self,
        _: &crate::actions::DebuggerHoverCollapseSelected,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(debugger_hover) = self.hover_state.keyboard_debugger_hover() {
            debugger_hover.update(cx, |debugger_hover, cx| {
                debugger_hover.collapse_selected(cx)
            });
        }
    }

    pub fn debugger_hover_select_next(
        &mut self,
        _: &crate::actions::DebuggerHoverSelectNext,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(debugger_hover) = self.hover_state.keyboard_debugger_hover() {
            debugger_hover.update(cx, |debugger_hover, cx| debugger_hover.select_next(cx));
            self.ensure_debugger_hover_selection_visible(cx);
        }
    }

    pub fn debugger_hover_select_previous(
        &mut self,
        _: &crate::actions::DebuggerHoverSelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(debugger_hover) = self.hover_state.keyboard_debugger_hover() {
            debugger_hover.update(cx, |debugger_hover, cx| debugger_hover.select_previous(cx));
            self.ensure_debugger_hover_selection_visible(cx);
        }
    }
}

pub struct InfoPopover {
    pub symbol_range: RangeInEditor,
    pub parsed_content: Option<Entity<Markdown>>,
    debugger_hover: Option<Entity<DebuggerHoverView>>,
    pub scroll_handle: ScrollHandle,
    pub keyboard_grace: Rc<RefCell<bool>>,
    pub anchor: Option<Anchor>,
    pub last_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    fallback_state: Option<FallbackHoverState>,
    _subscriptions: Vec<Subscription>,
}

struct FallbackHoverState {
    edit_count: usize,
    non_text_state_update_count: usize,
    includes_end: bool,
}

impl FallbackHoverState {
    fn new(buffer_snapshot: &MultiBufferSnapshot, range: &Range<Anchor>) -> Self {
        let offsets = range.to_offset(buffer_snapshot);
        let includes_end = offsets.end == buffer_snapshot.len()
            || fallback_hover_range(buffer_snapshot, range.end).to_offset(buffer_snapshot)
                == offsets;
        Self {
            edit_count: buffer_snapshot.edit_count(),
            non_text_state_update_count: buffer_snapshot.non_text_state_update_count(),
            includes_end,
        }
    }
}

impl InfoPopover {
    pub(crate) fn render(
        &mut self,
        max_size: Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> AnyElement {
        let keyboard_grace = Rc::clone(&self.keyboard_grace);
        let this = cx.entity().downgrade();
        let this2 = this.clone();
        let bounds_cell = self.last_bounds.clone();
        let parsed_content = self.parsed_content.clone();
        let debugger_hover = self.debugger_hover.clone();
        let debugger_hover_only = debugger_hover.is_some();
        let debugger_content_width = debugger_hover
            .as_ref()
            .and_then(|view| view.read(cx).content_width(window, cx));
        let scrollbars = if let Some(view_id) = debugger_hover.as_ref().map(Entity::entity_id) {
            Scrollbars::always_visible(ui::ScrollAxes::Both)
                .id(("debugger-hover-scrollbar", view_id))
                .with_stable_track_along(
                    ui::ScrollAxes::Vertical,
                    cx.theme().colors().elevated_surface_background,
                )
                .tracked_scroll_handle(&self.scroll_handle)
        } else {
            Scrollbars::for_settings::<EditorSettingsScrollbarProxy>()
                .tracked_scroll_handle(&self.scroll_handle)
        };
        div()
            .id("info_popover")
            .occlude()
            .elevation_2(cx)
            .when(debugger_hover_only, |this| this.flex().flex_col())
            .child(
                canvas(
                    {
                        move |bounds, _window, _cx| {
                            bounds_cell.set(Some(bounds));
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            // Prevent a mouse down/move on the popover from being propagated to the editor,
            // because that would dismiss the popover.
            .on_mouse_move({
                move |_, _, cx: &mut App| {
                    this.update(cx, |editor, _| {
                        editor.hover_state.closest_mouse_distance = Some(px(0.0));
                        editor.hover_state.hiding_delay_task = None;
                        editor.hover_state.hint_hover_task = None;
                    })
                    .ok();
                    cx.stop_propagation()
                }
            })
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                let mut keyboard_grace = keyboard_grace.borrow_mut();
                *keyboard_grace = false;
                cx.stop_propagation();
            })
            .when_some(debugger_hover.as_ref(), |this, view| {
                this.child(view.update(cx, |view, cx| view.render_header(cx).into_any_element()))
            })
            .when(
                parsed_content.is_some() || debugger_hover.is_some(),
                |this| {
                    this.child(
                        div()
                            .id("info-content-container")
                            .overflow_y_scroll()
                            .when(debugger_hover_only, |this| this.overflow_x_scroll())
                            .max_w(max_size.width)
                            .max_h(if debugger_hover_only {
                                (max_size.height - DEBUGGER_HOVER_HEADER_HEIGHT).max(Pixels::ZERO)
                            } else {
                                max_size.height
                            })
                            .when(debugger_hover_only, |this| {
                                this.min_w(DEBUGGER_HOVER_MIN_WIDTH.min(max_size.width))
                            })
                            .track_scroll(&self.scroll_handle)
                            .child(
                                v_flex()
                                    .when(!debugger_hover_only, |this| this.w_full())
                                    .when_some(debugger_content_width, |this, width| this.w(width))
                                    .flex_none()
                                    .when(debugger_hover_only, |this| this.gap_0().p_1())
                                    .when(!debugger_hover_only, |this| this.gap_2().p_2())
                                    .when_some(debugger_hover, |this, debugger_hover| {
                                        this.child(debugger_hover)
                                    })
                                    .when_some(parsed_content, |this, markdown| {
                                        this.child(
                                            div().child(
                                                MarkdownElement::new(
                                                    markdown,
                                                    hover_markdown_style(window, cx),
                                                )
                                                .scroll_handle(self.scroll_handle.clone())
                                                .code_block_renderer(
                                                    markdown::CodeBlockRenderer::Default {
                                                        copy_button_visibility:
                                                            CopyButtonVisibility::Hidden,
                                                        wrap_button_visibility:
                                                            markdown::WrapButtonVisibility::Hidden,
                                                        border: false,
                                                    },
                                                )
                                                .on_url_click(move |link, window, cx| {
                                                    open_markdown_url(
                                                        this2
                                                            .read_with(cx, |editor, _| {
                                                                editor.workspace()
                                                            })
                                                            .ok()
                                                            .flatten(),
                                                        link,
                                                        window,
                                                        cx,
                                                    )
                                                }),
                                            ),
                                        )
                                    }),
                            )
                            .map(|content| {
                                div()
                                    .relative()
                                    .child(content)
                                    .custom_scrollbars(scrollbars, window, cx)
                            }),
                    )
                },
            )
            .into_any_element()
    }

    pub fn scroll(&self, amount: ScrollAmount, window: &mut Window, cx: &mut Context<Editor>) {
        let mut current = self.scroll_handle.offset();
        current.y -= amount.pixels(
            window.line_height(),
            self.scroll_handle.bounds().size.height - px(16.),
        ) / 2.0;
        cx.notify();
        self.scroll_handle.set_offset(current);
    }
}

pub struct DiagnosticPopover {
    pub(crate) local_diagnostic: DiagnosticEntry<Anchor>,
    markdown: Entity<Markdown>,
    border_color: Hsla,
    background_color: Hsla,
    pub keyboard_grace: Rc<RefCell<bool>>,
    pub anchor: Anchor,
    pub last_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    _subscription: Subscription,
    pub scroll_handle: ScrollHandle,
}

impl DiagnosticPopover {
    pub fn render(
        &self,
        max_size: Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> AnyElement {
        let keyboard_grace = Rc::clone(&self.keyboard_grace);
        let this = cx.entity().downgrade();
        let bounds_cell = self.last_bounds.clone();
        div()
            .id("diagnostic")
            .occlude()
            .elevation_2_borderless(cx)
            .child(
                canvas(
                    {
                        move |bounds, _window, _cx| {
                            bounds_cell.set(Some(bounds));
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            // Don't draw the background color if the theme
            // allows transparent surfaces.
            .when(theme_is_transparent(cx), |this| {
                this.bg(gpui::transparent_black())
            })
            // Prevent a mouse move on the popover from being propagated to the editor,
            // because that would dismiss the popover.
            .on_mouse_move({
                let this = this.clone();
                move |_, _, cx: &mut App| {
                    this.update(cx, |editor, _| {
                        editor.hover_state.closest_mouse_distance = Some(px(0.0));
                        editor.hover_state.hiding_delay_task = None;
                        editor.hover_state.hint_hover_task = None;
                    })
                    .ok();
                    cx.stop_propagation()
                }
            })
            // Prevent a mouse down on the popover from being propagated to the editor,
            // because that would move the cursor.
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                let mut keyboard_grace = keyboard_grace.borrow_mut();
                *keyboard_grace = false;
                cx.stop_propagation();
            })
            .child(
                div()
                    .relative()
                    .py_1()
                    .pl_2()
                    .pr_8()
                    .bg(self.background_color)
                    .border_1()
                    .border_color(self.border_color)
                    .rounded_lg()
                    .child(
                        div()
                            .id("diagnostic-content-container")
                            .max_w(max_size.width)
                            .max_h(max_size.height)
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll_handle)
                            .child(
                                MarkdownElement::new(
                                    self.markdown.clone(),
                                    diagnostics_markdown_style(window, cx),
                                )
                                .code_block_renderer(markdown::CodeBlockRenderer::Default {
                                    copy_button_visibility: CopyButtonVisibility::Hidden,
                                    wrap_button_visibility: markdown::WrapButtonVisibility::Hidden,
                                    border: false,
                                })
                                .on_url_click(
                                    move |link, window, cx| {
                                        if let Some(renderer) = GlobalDiagnosticRenderer::global(cx)
                                        {
                                            this.update(cx, |this, cx| {
                                                renderer.as_ref().open_link(this, link, window, cx);
                                            })
                                            .ok();
                                        }
                                    },
                                ),
                            ),
                    )
                    .child(div().absolute().top_1().right_1().child({
                        let message = self
                            .local_diagnostic
                            .diagnostic
                            .message
                            .as_shared_string()
                            .clone();
                        CopyButton::new("copy-diagnostic", message).tooltip_label("Copy Diagnostic")
                    }))
                    .custom_scrollbars(
                        Scrollbars::for_settings::<EditorSettingsScrollbarProxy>()
                            .tracked_scroll_handle(&self.scroll_handle),
                        window,
                        cx,
                    ),
            )
            .into_any_element()
    }
}

fn hide_popovers(editor: &mut Editor, cx: &mut Context<Editor>) -> bool {
    let info_popovers = editor.hover_state.info_popovers.drain(..);
    let diagnostics_popover = editor.hover_state.diagnostic_popover.take();
    let did_hide = info_popovers.count() > 0 || diagnostics_popover.is_some();

    editor.hover_state.info_task = None;
    editor.hover_state.hiding_delay_task = None;
    editor.hover_state.closest_mouse_distance = None;
    editor.hover_state.stable_debugger_hover_origin = None;

    editor.clear_background_highlights(HighlightKey::HoverState, cx);

    if did_hide {
        cx.notify();
    }

    did_hide
}

