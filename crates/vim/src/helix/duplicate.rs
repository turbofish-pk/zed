use std::ops::Range;

use editor::{DisplayPoint, MultiBufferOffset, display_map::DisplaySnapshot};
use gpui::Context;
use language::PointUtf16;
use multi_buffer::{MultiBufferPoint, MultiBufferRow};
use text::Bias;
use ui::Window;

use crate::Vim;

#[derive(Copy, Clone)]
enum Direction {
    Above,
    Below,
}

impl Vim {
    /// Creates a duplicate of every selection below it in the first place that has both its start
    /// and end
    pub(super) fn helix_duplicate_selections_below(
        &mut self,
        times: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.duplicate_selections(times, window, cx, Direction::Below);
    }

    /// Creates a duplicate of every selection above it in the first place that has both its start
    /// and end
    pub(super) fn helix_duplicate_selections_above(
        &mut self,
        times: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.duplicate_selections(times, window, cx, Direction::Above);
    }

    fn duplicate_selections(
        &mut self,
        times: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
        direction: Direction,
    ) {
        let times = times.unwrap_or(1);
        self.update_editor(cx, |_, editor, cx| {
            let mut selections = Vec::new();
            let map = editor.display_snapshot(cx);
            let mut original_selections = editor.selections.all_display(&map);
            // The order matters, because it is recorded when the selections are added.
            if matches!(direction, Direction::Above) {
                original_selections.reverse();
            }

            for origin in original_selections {
                let origin = origin.tail()..origin.head();
                selections.push(display_point_range_to_offset_range(&origin, &map));
                let mut last_origin = origin;
                for _ in 1..=times {
                    if let Some(duplicate) =
                        find_next_valid_duplicate_space(last_origin.clone(), &map, direction)
                    {
                        selections.push(display_point_range_to_offset_range(&duplicate, &map));
                        last_origin = duplicate;
                    } else {
                        break;
                    }
                }
            }

            editor.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges(selections);
            });
        });
    }
}

fn find_next_valid_duplicate_space(
    origin: Range<DisplayPoint>,
    map: &DisplaySnapshot,
    direction: Direction,
) -> Option<Range<DisplayPoint>> {
    let buffer = map.buffer_snapshot();
    let start_col_utf16 = buffer
        .point_to_point_utf16(origin.start.to_point(map))
        .column;
    let end_col_utf16 = buffer.point_to_point_utf16(origin.end.to_point(map)).column;

    let mut candidate = origin;
    loop {
        match direction {
            Direction::Below => {
                if candidate.end.row() >= map.max_point().row() {
                    return None;
                }
                *candidate.start.row_mut() += 1;
                *candidate.end.row_mut() += 1;
            }
            Direction::Above => {
                if candidate.start.row() == DisplayPoint::zero().row() {
                    return None;
                }
                *candidate.start.row_mut() = candidate.start.row().0.saturating_sub(1);
                *candidate.end.row_mut() = candidate.end.row().0.saturating_sub(1);
            }
        }

        let start_row = DisplayPoint::new(candidate.start.row(), 0)
            .to_point(map)
            .row;
        let end_row = DisplayPoint::new(candidate.end.row(), 0).to_point(map).row;

        if start_col_utf16 > buffer.line_len_utf16(MultiBufferRow(start_row))
            || end_col_utf16 > buffer.line_len_utf16(MultiBufferRow(end_row))
        {
            continue;
        }

        let start_col = buffer
            .point_utf16_to_point(PointUtf16::new(start_row, start_col_utf16))
            .column;
        let end_col = buffer
            .point_utf16_to_point(PointUtf16::new(end_row, end_col_utf16))
            .column;

        let candidate_start =
            map.point_to_display_point(MultiBufferPoint::new(start_row, start_col), Bias::Left);
        let candidate_end =
            map.point_to_display_point(MultiBufferPoint::new(end_row, end_col), Bias::Right);

        if map.clip_point(candidate_start, Bias::Left) == candidate_start
            && map.clip_point(candidate_end, Bias::Right) == candidate_end
        {
            return Some(candidate_start..candidate_end);
        }
    }
}

fn display_point_range_to_offset_range(
    range: &Range<DisplayPoint>,
    map: &DisplaySnapshot,
) -> Range<MultiBufferOffset> {
    range.start.to_offset(map, Bias::Left)..range.end.to_offset(map, Bias::Right)
}

