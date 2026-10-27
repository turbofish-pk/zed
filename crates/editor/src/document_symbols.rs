use std::ops::Range;

use collections::HashMap;
use futures::FutureExt;
use futures::future::join_all;
use gpui::{App, Context, HighlightStyle, Task};
use itertools::Itertools as _;
use language::language_settings::LanguageSettings;
use language::{Buffer, LanguageAwareStyling, OutlineItem, highlight_ranges_from_text};
use multi_buffer::{
    Anchor, AnchorRangeExt as _, MultiBufferOffset, MultiBufferRow, MultiBufferSnapshot,
    ToOffset as _,
};
use text::BufferId;
use theme::{ActiveTheme as _, SyntaxTheme};
use unicode_segmentation::UnicodeSegmentation as _;
use util::maybe;

use crate::display_map::DisplaySnapshot;
use crate::{Editor, LSP_REQUEST_DEBOUNCE_TIMEOUT};

impl Editor {
    /// Returns all document outline items for a buffer, using LSP or
    /// tree-sitter based on the `document_symbols` setting.
    /// External consumers (outline modal, outline panel, breadcrumbs) should use this.
    pub fn buffer_outline_items(
        &self,
        buffer_id: BufferId,
        cx: &mut Context<Self>,
    ) -> Task<Vec<OutlineItem<text::Anchor>>> {
        let Some(buffer) = self.buffer.read(cx).buffer(buffer_id) else {
            return Task::ready(Vec::new());
        };

        if lsp_symbols_enabled(buffer.read(cx), cx) {
            let refresh_task = self.refresh_document_symbols_task.clone();
            cx.spawn(async move |editor, cx| {
                refresh_task.await;
                editor
                    .read_with(cx, |editor, _| {
                        editor
                            .lsp_document_symbols
                            .get(&buffer_id)
                            .cloned()
                            .unwrap_or_default()
                    })
                    .ok()
                    .unwrap_or_default()
            })
        } else {
            let buffer_snapshot = buffer.read(cx).snapshot();
            let syntax = cx.theme().syntax().clone();
            cx.background_executor()
                .spawn(async move { buffer_snapshot.outline(Some(&syntax)).items })
        }
    }

    /// Whether the buffer at `cursor` has LSP document symbols enabled.
    pub(super) fn uses_lsp_document_symbols(
        &self,
        cursor: Anchor,
        multi_buffer_snapshot: &MultiBufferSnapshot,
        cx: &Context<Self>,
    ) -> bool {
        let Some((anchor, _)) = multi_buffer_snapshot.anchor_to_buffer_anchor(cursor) else {
            return false;
        };
        let Some(buffer) = self.buffer.read(cx).buffer(anchor.buffer_id) else {
            return false;
        };
        lsp_symbols_enabled(buffer.read(cx), cx)
    }

    /// Filters editor-local LSP document symbols to the ancestor chain
    /// containing `cursor`. Never triggers an LSP request.
    pub(super) fn lsp_symbols_at_cursor(
        &self,
        cursor: Anchor,
        multi_buffer_snapshot: &MultiBufferSnapshot,
        _cx: &Context<Self>,
    ) -> Option<(BufferId, Vec<OutlineItem<Anchor>>)> {
        let (cursor_text_anchor, buffer) = multi_buffer_snapshot.anchor_to_buffer_anchor(cursor)?;
        let all_items = self
            .lsp_document_symbols
            .get(&cursor_text_anchor.buffer_id)?;
        if all_items.is_empty() {
            return None;
        }

        let mut symbols = all_items
            .iter()
            .filter(|item| {
                item.range.start.cmp(&cursor_text_anchor, buffer).is_le()
                    && item.range.end.cmp(&cursor_text_anchor, buffer).is_ge()
            })
            .filter_map(|item| {
                let range_start = multi_buffer_snapshot.anchor_in_buffer(item.range.start)?;
                let range_end = multi_buffer_snapshot.anchor_in_buffer(item.range.end)?;
                let source_range_for_text_start =
                    multi_buffer_snapshot.anchor_in_buffer(item.source_range_for_text.start)?;
                let source_range_for_text_end =
                    multi_buffer_snapshot.anchor_in_buffer(item.source_range_for_text.end)?;
                Some(OutlineItem {
                    depth: item.depth,
                    range: range_start..range_end,
                    selection_range: multi_buffer_snapshot
                        .anchor_in_buffer(item.selection_range.start)?
                        ..multi_buffer_snapshot.anchor_in_buffer(item.selection_range.end)?,
                    source_range_for_text: source_range_for_text_start..source_range_for_text_end,
                    text: item.text.clone(),
                    highlight_ranges: item.highlight_ranges.clone(),
                    name_ranges: item.name_ranges.clone(),
                    body_range: item.body_range.as_ref().and_then(|r| {
                        Some(
                            multi_buffer_snapshot.anchor_in_buffer(r.start)?
                                ..multi_buffer_snapshot.anchor_in_buffer(r.end)?,
                        )
                    }),
                    annotation_range: item.annotation_range.as_ref().and_then(|r| {
                        Some(
                            multi_buffer_snapshot.anchor_in_buffer(r.start)?
                                ..multi_buffer_snapshot.anchor_in_buffer(r.end)?,
                        )
                    }),
                })
            })
            .collect::<Vec<_>>();

        let mut prev_depth = None;
        symbols.retain(|item| {
            let retain = prev_depth.is_none_or(|prev_depth| item.depth > prev_depth);
            prev_depth = Some(item.depth);
            retain
        });

        Some((buffer.remote_id(), symbols))
    }

    /// Fetches document symbols from the LSP for buffers that have the setting
    /// enabled. Called from `update_lsp_data` on edits, server events, etc.
    /// When the fetch completes, stores results in `self.lsp_document_symbols`
    /// and triggers `refresh_outline_symbols_at_cursor` so breadcrumbs pick up the new data.
    pub(super) fn refresh_document_symbols(
        &mut self,
        for_buffer: Option<BufferId>,
        cx: &mut Context<Self>,
    ) {
        if !self.lsp_data_enabled() {
            return;
        }
        let Some(project) = self.project.as_ref().map(|p| p.downgrade()) else {
            return;
        };

        let buffers_to_query = self
            .visible_buffers(cx)
            .into_iter()
            .filter(|buffer| self.is_lsp_relevant(buffer.read(cx).file(), cx))
            .filter_map(|buffer| {
                let id = buffer.read(cx).remote_id();
                if for_buffer.is_none_or(|target| target == id)
                    && lsp_symbols_enabled(buffer.read(cx), cx)
                {
                    Some(buffer)
                } else {
                    None
                }
            })
            .unique_by(|buffer| buffer.read(cx).remote_id())
            .collect::<Vec<_>>();

        let mut symbols_altered = false;
        let multi_buffer = self.buffer().clone();
        self.lsp_document_symbols.retain(|buffer_id, _| {
            let Some(buffer) = multi_buffer.read(cx).buffer(*buffer_id) else {
                symbols_altered = true;
                return false;
            };
            let retain = lsp_symbols_enabled(buffer.read(cx), cx);
            symbols_altered |= !retain;
            retain
        });
        if symbols_altered {
            self.refresh_outline_symbols_at_cursor(cx);
        }

        if buffers_to_query.is_empty() {
            return;
        }

        self.refresh_document_symbols_task = cx
            .spawn(async move |editor, cx| {
                cx.background_executor()
                    .timer(LSP_REQUEST_DEBOUNCE_TIMEOUT)
                    .await;

                let Some(tasks) = project
                    .update(cx, |project, cx| {
                        project.lsp_store().update(cx, |lsp_store, cx| {
                            buffers_to_query
                                .into_iter()
                                .map(|buffer| {
                                    let buffer_id = buffer.read(cx).remote_id();
                                    let task = lsp_store.fetch_document_symbols(&buffer, cx);
                                    async move { (buffer_id, task.await) }
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .ok()
                else {
                    return;
                };

                let results = join_all(tasks).await.into_iter().collect::<HashMap<_, _>>();
                editor
                    .update(cx, |editor, cx| {
                        let syntax = cx.theme().syntax().clone();
                        let display_snapshot =
                            editor.display_map.update(cx, |map, cx| map.snapshot(cx));
                        let mut highlighted_results = results;
                        for (buffer_id, items) in highlighted_results.iter_mut() {
                            let language = editor
                                .buffer
                                .read(cx)
                                .buffer(*buffer_id)
                                .and_then(|buffer| buffer.read(cx).language().cloned());
                            for item in items {
                                if let Some(highlights) =
                                    highlights_from_buffer(&display_snapshot, &item, &syntax)
                                {
                                    item.highlight_ranges = highlights;
                                } else if let Some(language) = &language {
                                    item.highlight_ranges =
                                        highlight_ranges_from_text(&item.text, language, &syntax);
                                }
                            }
                        }
                        editor.lsp_document_symbols.extend(highlighted_results);
                        editor.refresh_outline_symbols_at_cursor(cx);
                    })
                    .ok();
            })
            .shared();
    }
}

fn lsp_symbols_enabled(buffer: &Buffer, cx: &App) -> bool {
    LanguageSettings::for_buffer(buffer, cx)
        .document_symbols
        .lsp_enabled()
}

/// Finds where the symbol name appears in the buffer and returns combined
/// (tree-sitter + semantic token) highlights for those positions.
///
/// First tries to find the name verbatim near the selection range so that
/// complex names (`impl Trait for Type`) get full highlighting. Falls back
/// to word-by-word matching for cases like `impl<T> Trait<T> for Type`
/// where the LSP name doesn't appear verbatim in the buffer.
fn highlights_from_buffer(
    display_snapshot: &DisplaySnapshot,
    item: &OutlineItem<text::Anchor>,
    syntax_theme: &SyntaxTheme,
) -> Option<Vec<(Range<usize>, HighlightStyle)>> {
    let outline_text = &item.text;
    if outline_text.is_empty() {
        return None;
    }

    let multi_buffer_snapshot = display_snapshot.buffer();
    let multi_buffer_source_range_anchors =
        multi_buffer_snapshot.text_anchors_to_visible_anchors([
            item.source_range_for_text.start,
            item.source_range_for_text.end,
        ]);
    let anchor_range = maybe!({
        Some(
            (*multi_buffer_source_range_anchors.get(0)?)?
                ..(*multi_buffer_source_range_anchors.get(1)?)?,
        )
    });
    let buffer_snapshot =
        multi_buffer_snapshot.buffer_for_id(item.source_range_for_text.start.buffer_id)?;
    let can_reuse_buffer_highlight_offsets = buffer_snapshot
        .text_for_range(item.source_range_for_text.clone())
        .flat_map(str::bytes)
        .eq(outline_text.bytes());
    if can_reuse_buffer_highlight_offsets {
        let visible_range = anchor_range
            .as_ref()
            .and_then(|_| {
                multi_buffer_snapshot
                    .buffer_anchor_range_to_anchor_range(item.source_range_for_text.clone())
            })
            .map(|range| range.to_offset(multi_buffer_snapshot))
            .filter(|range| range.end.0.checked_sub(range.start.0) == Some(outline_text.len()));
        let highlights = if let Some(range) = visible_range {
            display_snapshot.combined_highlights(range, syntax_theme)
        } else {
            let mut offset = 0;
            buffer_snapshot
                .chunks(
                    item.source_range_for_text.clone(),
                    LanguageAwareStyling {
                        tree_sitter: true,
                        diagnostics: false,
                    },
                )
                .filter_map(|chunk| {
                    let start = offset;
                    offset += chunk.text.len();
                    chunk
                        .syntax_highlight_id
                        .and_then(|id| syntax_theme.get(id))
                        .map(|style| (start..offset, *style))
                })
                .collect::<Vec<_>>()
        };
        return (!highlights.is_empty()).then_some(highlights);
    }

    let selection_point_range = anchor_range?.to_point(multi_buffer_snapshot);
    let mut search_start = selection_point_range.start;
    search_start.column = 0;
    let search_start_offset = search_start.to_offset(&multi_buffer_snapshot);
    let mut search_end = selection_point_range.end;
    search_end.column = multi_buffer_snapshot.line_len(MultiBufferRow(search_end.row));

    let search_text = multi_buffer_snapshot
        .text_for_range(search_start..search_end)
        .collect::<String>();

    let mut outline_text_highlights = Vec::new();
    match search_text.find(outline_text.as_str()) {
        Some(start_index) => {
            let multibuffer_start = search_start_offset + MultiBufferOffset(start_index);
            let multibuffer_end = multibuffer_start + MultiBufferOffset(outline_text.len());
            outline_text_highlights.extend(
                display_snapshot
                    .combined_highlights(multibuffer_start..multibuffer_end, syntax_theme),
            );
        }
        None => {
            for (outline_text_word_start, outline_word) in outline_text.split_word_bound_indices() {
                if let Some(start_index) = search_text.find(outline_word) {
                    let multibuffer_start = search_start_offset + MultiBufferOffset(start_index);
                    let multibuffer_end = multibuffer_start + MultiBufferOffset(outline_word.len());
                    outline_text_highlights.extend(
                        display_snapshot
                            .combined_highlights(multibuffer_start..multibuffer_end, syntax_theme)
                            .into_iter()
                            .map(|(range_in_word, style)| {
                                (
                                    outline_text_word_start + range_in_word.start
                                        ..outline_text_word_start + range_in_word.end,
                                    style,
                                )
                            }),
                    );
                }
            }
        }
    }

    if outline_text_highlights.is_empty() {
        None
    } else {
        Some(outline_text_highlights)
    }
}

