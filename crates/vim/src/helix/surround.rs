use collections::HashMap;
use editor::display_map::DisplaySnapshot;
use editor::{Bias, DisplayPoint, MultiBufferOffset, movement};
use gpui::{Context, Window};
use multi_buffer::{Anchor, MultiBufferSnapshot, ToOffset};
use text::Selection;

use crate::Vim;
use crate::object::{DelimiterRange, innermost_surrounding_pair, surrounding_markers};
use crate::surrounds::{bracket_pair_for_str_helix, surround_pair_for_char_helix};

fn surrounding_markers_containing_cursor(
    display_map: &DisplaySnapshot,
    cursor: DisplayPoint,
    open_marker: char,
    close_marker: char,
) -> Option<std::ops::Range<DisplayPoint>> {
    let range = surrounding_markers(display_map, cursor, true, true, open_marker, close_marker)?;
    let cursor_offset = cursor.to_offset(display_map, Bias::Left);
    let start_offset = range.start.to_offset(display_map, Bias::Left);
    let end_offset = range.end.to_offset(display_map, Bias::Right);

    if cursor_offset >= start_offset && cursor_offset <= end_offset {
        Some(range)
    } else {
        None
    }
}

/// The delimiter ranges of the pair surrounding the cursor: a literal search
/// for an explicit pair character, or the tree-sitter based closest pair for
/// 'm', matching `mim`/`mam`.
fn surrounding_pair_ranges(
    display_map: &DisplaySnapshot,
    cursor: DisplayPoint,
    target_char: char,
) -> Option<DelimiterRange> {
    match surround_pair_for_char_helix(target_char) {
        Some(pair) => {
            let range =
                surrounding_markers_containing_cursor(display_map, cursor, pair.open, pair.close)?;
            let open_start = range.start.to_offset(display_map, Bias::Left);
            let open_end = open_start + pair.open.len_utf8();
            let close_end = range.end.to_offset(display_map, Bias::Left);
            let close_start = close_end - pair.close.len_utf8();
            Some(DelimiterRange {
                open: open_start..open_end,
                close: close_start..close_end,
            })
        }
        None => {
            let cursor_range = cursor..movement::right(display_map, cursor);
            innermost_surrounding_pair(display_map, cursor_range)
        }
    }
}

fn selection_cursor(map: &DisplaySnapshot, selection: &Selection<DisplayPoint>) -> DisplayPoint {
    if selection.reversed || selection.is_empty() {
        selection.head()
    } else {
        editor::movement::left(map, selection.head())
    }
}

/// Anchor a pre-edit selection so it survives the surround edits with its
/// direction intact, the way Helix maps selections through a transaction.
fn preserved_selection_anchors(
    display_map: &DisplaySnapshot,
    selection: &Selection<DisplayPoint>,
) -> std::ops::Range<Anchor> {
    let range = selection.range();
    let snapshot = display_map.buffer_snapshot();
    let start = snapshot.anchor_before(range.start.to_offset(display_map, Bias::Left));
    let end = snapshot.anchor_before(range.end.to_offset(display_map, Bias::Left));
    if selection.reversed {
        end..start
    } else {
        start..end
    }
}

type SurroundEdits = Vec<(std::ops::Range<MultiBufferOffset>, String)>;

/// One end of a selection that `ms` surrounds, resolved after the edit.
struct SurroundedEdge {
    /// Anchors around the pre-edit offset: everything inserted there ends up
    /// between them.
    before: Anchor,
    after: Anchor,
    /// The length of the text requested at this offset that belongs before
    /// the edge.
    skip: usize,
}

impl SurroundedEdge {
    fn new(snapshot: &MultiBufferSnapshot, offset: MultiBufferOffset, skip: usize) -> Self {
        Self {
            before: snapshot.anchor_before(offset),
            after: snapshot.anchor_after(offset),
            skip,
        }
    }

    fn resolve(&self, snapshot: &MultiBufferSnapshot) -> MultiBufferOffset {
        let before = self.before.to_offset(snapshot);
        let after = self.after.to_offset(snapshot);
        // The multi buffer does not always grow by the requested amount: the
        // editor can be read-only, or an excerpt can end at the insertion
        // point. Only move over text that was actually inserted.
        before + self.skip.min(after - before)
    }
}

enum SurroundSelections {
    Anchors(Vec<std::ops::Range<Anchor>>),
    Surrounded(Vec<std::ops::Range<SurroundedEdge>>),
}

fn apply_helix_surround_edits<F>(
    vim: &mut Vim,
    window: &mut Window,
    cx: &mut Context<Vim>,
    mut build: F,
) where
    F: FnMut(&DisplaySnapshot, Vec<Selection<DisplayPoint>>) -> (SurroundEdits, SurroundSelections),
{
    vim.update_editor(cx, |_, editor, cx| {
        editor.transact(window, cx, |editor, window, cx| {
            editor.set_clip_at_line_ends(false, cx);

            let display_map = editor.display_snapshot(cx);
            let selections = editor.selections.all_display(&display_map);
            let (mut edits, new_selections) = build(&display_map, selections);

            // The sort is stable, so insertions at the same offset keep the
            // order they were pushed in.
            edits.sort_by_key(|edit| edit.0.start);
            editor.edit(edits, cx);

            let snapshot = editor.buffer().read(cx).snapshot(cx);
            editor.change_selections(Default::default(), window, cx, |s| match new_selections {
                SurroundSelections::Anchors(anchors) => s.select_anchor_ranges(anchors),
                SurroundSelections::Surrounded(edges) => s.select_ranges(
                    edges
                        .iter()
                        .map(|edge| edge.start.resolve(&snapshot)..edge.end.resolve(&snapshot)),
                ),
            });
            editor.set_clip_at_line_ends(true, cx);
        });
    });
}

impl Vim {
    /// ms - Add surrounding characters around selection.
    pub fn helix_surround_add(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.stop_recording(cx);

        let pair = bracket_pair_for_str_helix(text);

        apply_helix_surround_edits(self, window, cx, |display_map, selections| {
            let snapshot = display_map.buffer_snapshot();
            let mut edits = Vec::new();
            let mut ranges = Vec::new();
            // The length of the text requested so far at each insertion
            // point. Keyed by the position in the source buffer: offsets on
            // both sides of an excerpt boundary can map to the same source
            // position, and the multi buffer coalesces their insertions.
            let mut requested_at = HashMap::default();
            let insertion_point = |offset| {
                snapshot
                    .point_to_buffer_offset(offset)
                    .map(|(buffer, offset)| (buffer.remote_id(), offset))
            };

            // Like Helix, the new selection covers the surrounded text
            // including the added delimiters, so that surrounds compose and
            // `i`/`a` land outside the pair. When two selections touch, the
            // closing delimiter of the first and the opening delimiter of the
            // second are inserted at the same offset, so no anchor can end up
            // between them. Instead, each edge records how much of the text
            // requested at its offset was pushed before it.
            for selection in selections {
                let range = selection.range();
                let start = range.start.to_offset(display_map, Bias::Right);
                let end = range.end.to_offset(display_map, Bias::Left);

                let requested = requested_at.entry(insertion_point(start)).or_insert(0);
                let start_edge = SurroundedEdge::new(snapshot, start, *requested);
                *requested += pair.start.len();
                edits.push((start..start, pair.start.clone()));

                let requested = requested_at.entry(insertion_point(end)).or_insert(0);
                *requested += pair.end.len();
                let end_edge = SurroundedEdge::new(snapshot, end, *requested);
                edits.push((end..end, pair.end.clone()));

                if selection.reversed {
                    ranges.push(end_edge..start_edge);
                } else {
                    ranges.push(start_edge..end_edge);
                }
            }

            (edits, SurroundSelections::Surrounded(ranges))
        });
    }

    /// mr - Replace innermost surrounding pair containing the cursor.
    pub fn helix_surround_replace(
        &mut self,
        old_char: char,
        new_char: char,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stop_recording(cx);

        let new_char_str = new_char.to_string();
        let new_pair = bracket_pair_for_str_helix(&new_char_str);

        apply_helix_surround_edits(self, window, cx, |display_map, selections| {
            let mut edits: Vec<(std::ops::Range<MultiBufferOffset>, String)> = Vec::new();
            let mut anchors = Vec::new();

            for selection in selections {
                let cursor = selection_cursor(display_map, &selection);
                anchors.push(preserved_selection_anchors(display_map, &selection));

                if let Some(pair) = surrounding_pair_ranges(display_map, cursor, old_char) {
                    edits.push((pair.close, new_pair.end.clone()));
                    edits.push((pair.open, new_pair.start.clone()));
                }
            }

            (edits, SurroundSelections::Anchors(anchors))
        });
    }

    /// md - Delete innermost surrounding pair containing the cursor.
    pub fn helix_surround_delete(
        &mut self,
        target_char: char,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stop_recording(cx);

        apply_helix_surround_edits(self, window, cx, |display_map, selections| {
            let mut edits: Vec<(std::ops::Range<MultiBufferOffset>, String)> = Vec::new();
            let mut anchors = Vec::new();

            for selection in selections {
                let cursor = selection_cursor(display_map, &selection);
                anchors.push(preserved_selection_anchors(display_map, &selection));

                if let Some(pair) = surrounding_pair_ranges(display_map, cursor, target_char) {
                    edits.push((pair.close, String::new()));
                    edits.push((pair.open, String::new()));
                }
            }

            (edits, SurroundSelections::Anchors(anchors))
        });
    }
}

