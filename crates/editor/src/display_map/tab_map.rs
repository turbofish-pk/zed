use super::{
  Highlights,
  fold_map::{self, Chunk, FoldChunks, FoldEdit, FoldOffset, FoldPoint, FoldSnapshot},
};

use language::{LanguageAwareStyling, Point};
use multi_buffer::{MultiBufferRow, MultiBufferSnapshot};
use std::{cmp, num::NonZeroU32, ops::Range};
use sum_tree::Bias;

const MAX_EXPANSION_COLUMN: u32 = 256;
const MAX_ROW_SCAN_CHUNKS: usize = 8;

// Handles a tab width <= 128
const SPACES: &[u8; rope::Chunk::MASK_BITS] = &[b' '; _];
const MAX_TABS: NonZeroU32 = NonZeroU32::new(SPACES.len() as u32).unwrap();

/// Keeps track of hard tabs in a text buffer.
///
/// See the [`display_map` module documentation](crate::display_map) for more information.
pub struct TabMap(TabSnapshot);

impl TabMap {
  #[ztracing::instrument(skip_all)]
  pub fn new(fold_snapshot: FoldSnapshot, tab_size: NonZeroU32) -> (Self, TabSnapshot) {
    let snapshot = TabSnapshot {
      fold_snapshot,
      tab_size: tab_size.min(MAX_TABS),
      max_expansion_column: MAX_EXPANSION_COLUMN,
      version: 0,
    };
    (Self(snapshot.clone()), snapshot)
  }

  #[ztracing::instrument(skip_all)]
  pub fn sync(
    &mut self,
    fold_snapshot: FoldSnapshot,
    mut fold_edits: Vec<FoldEdit>,
    tab_size: NonZeroU32,
  ) -> (TabSnapshot, Vec<TabEdit>) {
    let tab_size = tab_size.min(MAX_TABS);

    if self.0.tab_size != tab_size {
      let old_max_point = self.0.max_point();
      self.0.version += 1;
      self.0.fold_snapshot = fold_snapshot;
      self.0.tab_size = tab_size;
      return (
        self.0.clone(),
        vec![TabEdit {
          old: TabPoint::zero()..old_max_point,
          new: TabPoint::zero()..self.0.max_point(),
        }],
      );
    }

    let old_snapshot = &mut self.0;
    let mut new_version = old_snapshot.version;
    if old_snapshot.fold_snapshot.version != fold_snapshot.version {
      new_version += 1;
    }

    if fold_edits.is_empty() {
      old_snapshot.version = new_version;
      old_snapshot.fold_snapshot = fold_snapshot;
      old_snapshot.tab_size = tab_size;
      return (old_snapshot.clone(), vec![]);
    }

    // Expand each edit to include the next tab on the same line as the edit,
    // and any subsequent tabs on that line that moved across the tab expansion
    // boundary.
    //
    // This is necessary because a tab's display width depends on its column
    // position: it expands to fill up to the next tab stop. When an edit
    // shifts text on a line, any tab character after the edit may now render
    // at a different width even though the tab byte itself wasn't touched.
    // Additionally, tabs beyond `max_expansion_column` are rendered as a
    // single space instead of expanding to the next tab stop. An edit that
    // shifts a tab across that boundary changes its display width, so the
    // edit must cover it. We scan forward from the edit end to the end of
    // the line, extending the edit to include the first subsequent tab (whose
    // rendered width may have changed) and the last tab that crossed the
    // expansion boundary (transitioning between expanded and non-expanded).
    let old_fold_len = old_snapshot.fold_snapshot.len();
    let mut old_chunks: Option<FoldChunks> = None;
    for fold_edit in &mut fold_edits {
      let old_end = fold_edit.old.end.to_point(&old_snapshot.fold_snapshot);
      let new_end = fold_edit.new.end.to_point(&fold_snapshot);

      let chunks = match &mut old_chunks {
        Some(chunks) => {
          chunks.seek(fold_edit.old.end..old_fold_len);
          chunks
        }
        None => old_chunks.insert(old_snapshot.fold_snapshot.chunks(
          fold_edit.old.end..old_fold_len,
          LanguageAwareStyling {
            tree_sitter: false,
            diagnostics: false,
          },
          Highlights::default(),
        )),
      };

      let mut offset_from_edit = 0;
      let mut first_tab_offset = None;
      let mut last_tab_with_changed_expansion_offset = None;
      'outer: for chunk in chunks {
        let row_end = chunk.newlines.trailing_zeros();
        let mut remaining_tabs = chunk.tabs & !u128::MAX.unbounded_shl(row_end);
        while remaining_tabs != 0 {
          let ix = remaining_tabs.trailing_zeros();
          let offset_from_edit = offset_from_edit + ix;
          if first_tab_offset.is_none() {
            first_tab_offset = Some(offset_from_edit);
          }

          let old_column = old_end.column() + offset_from_edit;
          let new_column = new_end.column() + offset_from_edit;
          let was_expanded = old_column < old_snapshot.max_expansion_column;
          let is_expanded = new_column < old_snapshot.max_expansion_column;
          if was_expanded != is_expanded {
            last_tab_with_changed_expansion_offset = Some(offset_from_edit);
          } else if !was_expanded && !is_expanded {
            break 'outer;
          }

          remaining_tabs &= remaining_tabs - 1;
        }

        if chunk.newlines != 0 {
          break;
        }

        offset_from_edit += chunk.text.len() as u32;
        if old_end.column() + offset_from_edit >= old_snapshot.max_expansion_column
          && new_end.column() + offset_from_edit >= old_snapshot.max_expansion_column
        {
          break;
        }
      }

      if let Some(offset) = last_tab_with_changed_expansion_offset.or(first_tab_offset) {
        fold_edit.old.end.0 += offset as usize + 1;
        fold_edit.new.end.0 += offset as usize + 1;
      }
    }
    drop(old_chunks);

    let new_snapshot = TabSnapshot {
      fold_snapshot,
      tab_size,
      max_expansion_column: old_snapshot.max_expansion_column,
      version: new_version,
    };

    let _old_alloc_ptr = fold_edits.as_ptr();
    // Combine any edits that overlap due to the expansion.
    let mut fold_edits = fold_edits.into_iter();
    let mut first_edit = fold_edits.next().unwrap();
    // This code relies on reusing allocations from the Vec<_> - at the time of writing .flatten() prevents them.
    #[allow(clippy::filter_map_identity)]
    let mut v: Vec<_> = fold_edits
      .scan(&mut first_edit, |state, edit| {
        if state.old.end >= edit.old.start {
          state.old.end = edit.old.end;
          state.new.end = edit.new.end;
          Some(None) // Skip this edit, it's merged
        } else {
          let new_state = edit;
          let result = Some(Some(state.clone())); // Yield the previous edit
          **state = new_state;
          result
        }
      })
      .filter_map(|x| x)
      .collect();
    v.push(first_edit);
    debug_assert_eq!(v.as_ptr(), _old_alloc_ptr, "Fold edits were reallocated");
    let tab_edits = {
      let mut old_cursor = old_snapshot.tab_point_cursor();
      let mut new_cursor = new_snapshot.tab_point_cursor();
      v.into_iter()
        .map(|fold_edit| {
          let old_start = fold_edit.old.start.to_point(&old_snapshot.fold_snapshot);
          let old_end = fold_edit.old.end.to_point(&old_snapshot.fold_snapshot);
          let new_start = fold_edit.new.start.to_point(&new_snapshot.fold_snapshot);
          let new_end = fold_edit.new.end.to_point(&new_snapshot.fold_snapshot);
          TabEdit {
            old: old_cursor.map(old_start)..old_cursor.map(old_end),
            new: new_cursor.map(new_start)..new_cursor.map(new_end),
          }
        })
        .collect()
    };
    *old_snapshot = new_snapshot;
    (old_snapshot.clone(), tab_edits)
  }
}

#[derive(Clone)]
pub struct TabSnapshot {
  pub fold_snapshot: FoldSnapshot,
  pub tab_size: NonZeroU32,
  /// The maximum column up to which a tab can expand.
  /// Any tab after this column will not expand.
  pub max_expansion_column: u32,
  pub version: usize,
}

impl std::ops::Deref for TabSnapshot {
  type Target = FoldSnapshot;

  fn deref(&self) -> &Self::Target {
    &self.fold_snapshot
  }
}

impl TabSnapshot {
  #[inline]
  pub fn buffer_snapshot(&self) -> &MultiBufferSnapshot {
    &self.fold_snapshot.inlay_snapshot.buffer
  }

  #[ztracing::instrument(skip_all)]
  pub fn line_len(&self, row: u32) -> u32 {
    let max_point = self.max_point();
    if row < max_point.row() {
      self
        .fold_point_to_tab_point(FoldPoint::new(row, self.fold_snapshot.line_len(row)))
        .0
        .column
    } else {
      max_point.column()
    }
  }

  #[ztracing::instrument(skip_all)]
  pub fn text_summary(&self) -> TextSummary {
    self.text_summary_for_range(TabPoint::zero()..self.max_point())
  }

  #[ztracing::instrument(skip_all, fields(rows))]
  pub fn text_summary_for_range(&self, range: Range<TabPoint>) -> TextSummary {
    let input_start = self.tab_point_to_fold_point(range.start, Bias::Left).0;
    let input_end = self.tab_point_to_fold_point(range.end, Bias::Right).0;
    let input_summary = self
      .fold_snapshot
      .text_summary_for_range(input_start..input_end);

    let line_end = if range.start.row() == range.end.row() {
      range.end
    } else {
      self.max_point()
    };
    let first_line_chars = self
      .chunks(
        range.start..line_end,
        LanguageAwareStyling {
          tree_sitter: false,
          diagnostics: false,
        },
        Highlights::default(),
      )
      .flat_map(|chunk| chunk.text.chars())
      .take_while(|&c| c != '\n')
      .count() as u32;

    let last_line_chars = if range.start.row() == range.end.row() {
      first_line_chars
    } else {
      self
        .chunks(
          TabPoint::new(range.end.row(), 0)..range.end,
          LanguageAwareStyling {
            tree_sitter: false,
            diagnostics: false,
          },
          Highlights::default(),
        )
        .flat_map(|chunk| chunk.text.chars())
        .count() as u32
    };

    TextSummary {
      lines: range.end.0 - range.start.0,
      first_line_chars,
      last_line_chars,
      longest_row: input_summary.longest_row,
      longest_row_chars: input_summary.longest_row_chars,
    }
  }

  #[ztracing::instrument(skip_all)]
  pub(crate) fn chunks<'a>(
    &'a self,
    range: Range<TabPoint>,
    language_aware: LanguageAwareStyling,
    highlights: Highlights<'a>,
  ) -> TabChunks<'a> {
    let (input_start, expanded_char_column, to_next_stop) =
      self.tab_point_to_fold_point(range.start, Bias::Left);
    let input_column = input_start.column();
    let input_start = input_start.to_offset(&self.fold_snapshot);
    let input_end = self
      .tab_point_to_fold_point(range.end, Bias::Right)
      .0
      .to_offset(&self.fold_snapshot);
    let to_next_stop = if range.start.0 + Point::new(0, to_next_stop) > range.end.0 {
      range.end.column() - range.start.column()
    } else {
      to_next_stop
    };

    TabChunks {
      snapshot: self,
      fold_chunks: self
        .fold_snapshot
        .chunks(input_start..input_end, language_aware, highlights),
      input_column,
      column: expanded_char_column,
      max_expansion_column: self.max_expansion_column,
      output_position: range.start.0,
      max_output_position: range.end.0,
      tab_size: self.tab_size,
      chunk: Chunk {
        text: unsafe { std::str::from_utf8_unchecked(&SPACES[..to_next_stop as usize]) },
        is_tab: true,
        chars: u128::MAX.unbounded_shr(u128::BITS - to_next_stop),
        ..Default::default()
      },
      inside_leading_tab: to_next_stop > 0,
    }
  }

  #[ztracing::instrument(skip_all)]
  pub fn rows(&self, row: u32) -> fold_map::FoldRows<'_> {
    self.fold_snapshot.row_infos(row)
  }

  #[ztracing::instrument(skip_all)]
  pub fn max_point(&self) -> TabPoint {
    self.fold_point_to_tab_point(self.fold_snapshot.max_point())
  }

  #[ztracing::instrument(skip_all)]
  pub fn clip_point(&self, point: TabPoint, bias: Bias) -> TabPoint {
    self.fold_point_to_tab_point(
      self
        .fold_snapshot
        .clip_point(self.tab_point_to_fold_point(point, bias).0, bias),
    )
  }

  #[ztracing::instrument(skip_all)]
  pub fn fold_point_to_tab_point(&self, input: FoldPoint) -> TabPoint {
    let chunks = self.fold_snapshot.chunks_at(FoldPoint::new(input.row(), 0));
    let mut tab_cursor = TabStopCursor::new(chunks);
    let expanded = self.expand_tabs(&mut tab_cursor, &mut 0, &mut 0, input.column());
    TabPoint::new(input.row(), expanded)
  }

  #[ztracing::instrument(skip_all)]
  pub fn tab_point_cursor(&self) -> TabPointCursor<'_> {
    TabPointCursor {
      snapshot: self,
      tab_stop_cursor: None,
      row: 0,
      tab_count: 0,
      expanded_tab_len: 0,
      needs_seek: false,
      max_scan_chunks: MAX_ROW_SCAN_CHUNKS,
    }
  }

  #[ztracing::instrument(skip_all)]
  pub fn tab_point_to_fold_point(&self, output: TabPoint, bias: Bias) -> (FoldPoint, u32, u32) {
    let chunks = self
      .fold_snapshot
      .chunks_at(FoldPoint::new(output.row(), 0));

    let tab_cursor = TabStopCursor::new(chunks);
    let expanded = output.column();
    let (collapsed, expanded_char_column, to_next_stop) =
      self.collapse_tabs(tab_cursor, expanded, bias);

    (
      FoldPoint::new(output.row(), collapsed),
      expanded_char_column,
      to_next_stop,
    )
  }

  pub fn buffer_row_to_tab_row(&self, buffer_row: MultiBufferRow) -> u32 {
    self
      .point_to_tab_point(Point::new(buffer_row.0, 0), Bias::Left)
      .row()
  }

  #[ztracing::instrument(skip_all)]
  pub fn point_to_tab_point(&self, point: Point, bias: Bias) -> TabPoint {
    let inlay_point = self.fold_snapshot.inlay_snapshot.to_inlay_point(point);
    let fold_point = self.fold_snapshot.to_fold_point(inlay_point, bias);
    self.fold_point_to_tab_point(fold_point)
  }

  #[ztracing::instrument(skip_all)]
  pub fn tab_point_to_point(&self, point: TabPoint, bias: Bias) -> Point {
    let fold_point = self.tab_point_to_fold_point(point, bias).0;
    let inlay_point = fold_point.to_inlay_point(&self.fold_snapshot);
    self
      .fold_snapshot
      .inlay_snapshot
      .to_buffer_point(inlay_point)
  }

  /// Expands the tabs between the cursor's position and `column`. `tab_count`
  /// and `expanded_tab_len` carry the tabs already passed on this row, so the
  /// same cursor can be advanced to a later column of the row.
  #[ztracing::instrument(skip_all)]
  fn expand_tabs(
    &self,
    cursor: &mut TabStopCursor<'_>,
    tab_count: &mut u32,
    expanded_tab_len: &mut u32,
    column: u32,
  ) -> u32 {
    // we only ever act on a single row at a time
    // so the main difference is that other layers build a transform sumtree, and can then just run through that
    // we cant quite do this here, as we need to work with the previous layer chunk to understand the tabs of the corresponding row
    // we can still do forward searches for this though, we search for a row, then traverse the column up to where we need to be
    let tab_size = self.tab_size.get();

    let end_column = column.min(self.max_expansion_column);
    let mut seek_target = end_column - cursor.byte_offset;

    while let Some(tab_stop) = cursor.seek_forward(seek_target) {
      let expanded_chars_old = tab_stop.char_offset + *expanded_tab_len - *tab_count;
      let tab_len = tab_size - ((expanded_chars_old - 1) % tab_size);
      *tab_count += 1;
      *expanded_tab_len += tab_len;

      seek_target = end_column - cursor.byte_offset;
    }

    let left_over_char_bytes = if !cursor.is_char_boundary() {
      cursor.bytes_until_next_char().unwrap_or(0) as u32
    } else {
      0
    };

    let collapsed_bytes = cursor.byte_offset() + left_over_char_bytes;
    let expanded_bytes =
      cursor.byte_offset() + *expanded_tab_len - *tab_count + left_over_char_bytes;

    expanded_bytes + column.saturating_sub(collapsed_bytes)
  }

  #[ztracing::instrument(skip_all)]
  fn collapse_tabs<'a>(
    &self,
    mut cursor: TabStopCursor<'a>,
    column: u32,
    bias: Bias,
  ) -> (u32, u32, u32) {
    let tab_size = self.tab_size.get();
    let mut collapsed_column = column;
    let mut seek_target = column.min(self.max_expansion_column);
    let mut tab_count = 0;
    let mut expanded_tab_len = 0;

    while let Some(tab_stop) = cursor.seek_forward(seek_target) {
      // Calculate how much we want to expand this tab stop (into spaces)
      let expanded_chars_old = tab_stop.char_offset + expanded_tab_len - tab_count;
      let tab_len = tab_size - ((expanded_chars_old - 1) % tab_size);
      // Increment tab count
      tab_count += 1;
      // The count of how many spaces we've added to this line in place of tab bytes
      expanded_tab_len += tab_len;

      // The count of bytes at this point in the iteration while considering tab_count and previous expansions
      let expanded_bytes = tab_stop.byte_offset + expanded_tab_len - tab_count;

      // Did we expand past the search target?
      if expanded_bytes > column {
        let mut expanded_chars = tab_stop.char_offset + expanded_tab_len - tab_count;
        // We expanded past the search target, so need to account for the offshoot
        expanded_chars -= expanded_bytes - column;
        return match bias {
          Bias::Left => (
            cursor.byte_offset() - 1,
            expanded_chars,
            expanded_bytes - column,
          ),
          Bias::Right => (cursor.byte_offset(), expanded_chars, 0),
        };
      } else {
        // otherwise we only want to move the cursor collapse column forward
        collapsed_column = collapsed_column - tab_len + 1;
        seek_target = (collapsed_column - cursor.byte_offset)
          .min(self.max_expansion_column - cursor.byte_offset);
      }
    }

    let collapsed_bytes = cursor.byte_offset();
    let expanded_bytes = cursor.byte_offset() + expanded_tab_len - tab_count;
    let expanded_chars = cursor.char_offset() + expanded_tab_len - tab_count;
    (
      collapsed_bytes + column.saturating_sub(expanded_bytes),
      expanded_chars,
      0,
    )
  }
}

/// Maps `FoldPoint`s to `TabPoint`s, reusing the scan over the fold chunks
/// across calls. It is most efficient when points are supplied in
/// non-decreasing order; a point before the previous one is still mapped
/// correctly, at the cost of an extra seek.
pub struct TabPointCursor<'a> {
  snapshot: &'a TabSnapshot,
  tab_stop_cursor: Option<TabStopCursor<'a>>,
  row: u32,
  tab_count: u32,
  expanded_tab_len: u32,
  needs_seek: bool,
  max_scan_chunks: usize,
}

impl TabPointCursor<'_> {
  /// Resets the cursor to the start so it can seek backward again.
  pub fn reset(&mut self) {
    self.needs_seek = true;
  }

  #[ztracing::instrument(skip_all)]
  pub fn map(&mut self, point: FoldPoint) -> TabPoint {
    let snapshot = self.snapshot;
    let row = point.row();
    let end_column = point.column().min(snapshot.max_expansion_column);

    let cursor = match &mut self.tab_stop_cursor {
      Some(cursor) => {
        let can_continue = !self.needs_seek && !cursor.crossed_newline;
        if !(can_continue && row == self.row && end_column >= cursor.byte_offset) {
          // Scanning over the rows in between is cheaper than seeking
          // the fold chunks, as long as the target row is nearby.
          let advanced = can_continue
            && row > self.row
            && cursor.advance_rows(row - self.row, self.max_scan_chunks);
          if !advanced {
            let row_start = FoldPoint::new(row, 0).to_offset(&snapshot.fold_snapshot);
            cursor.seek_to(row_start..snapshot.fold_snapshot.len());
          }
          self.tab_count = 0;
          self.expanded_tab_len = 0;
          self.needs_seek = false;
        }
        cursor
      }
      None => {
        self.tab_count = 0;
        self.expanded_tab_len = 0;
        self.needs_seek = false;
        self.tab_stop_cursor.insert(TabStopCursor::new(
          snapshot.fold_snapshot.chunks_at(FoldPoint::new(row, 0)),
        ))
      }
    };
    self.row = row;

    let expanded = snapshot.expand_tabs(
      cursor,
      &mut self.tab_count,
      &mut self.expanded_tab_len,
      point.column(),
    );
    TabPoint::new(row, expanded)
  }
}

#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq)]
pub struct TabPoint(pub Point);

impl TabPoint {
  pub fn new(row: u32, column: u32) -> Self {
    Self(Point::new(row, column))
  }

  pub fn zero() -> Self {
    Self::new(0, 0)
  }

  pub fn row(self) -> u32 {
    self.0.row
  }

  pub fn column(self) -> u32 {
    self.0.column
  }
}

impl From<Point> for TabPoint {
  fn from(point: Point) -> Self {
    Self(point)
  }
}

pub type TabEdit = text::Edit<TabPoint>;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TextSummary {
  pub lines: Point,
  pub first_line_chars: u32,
  pub last_line_chars: u32,
  pub longest_row: u32,
  pub longest_row_chars: u32,
}

impl<'a> From<&'a str> for TextSummary {
  #[ztracing::instrument(skip_all)]
  fn from(text: &'a str) -> Self {
    let sum = text::TextSummary::from(text);

    TextSummary {
      lines: sum.lines,
      first_line_chars: sum.first_line_chars,
      last_line_chars: sum.last_line_chars,
      longest_row: sum.longest_row,
      longest_row_chars: sum.longest_row_chars,
    }
  }
}

impl<'a> std::ops::AddAssign<&'a Self> for TextSummary {
  #[ztracing::instrument(skip_all)]
  fn add_assign(&mut self, other: &'a Self) {
    let joined_chars = self.last_line_chars + other.first_line_chars;
    if joined_chars > self.longest_row_chars {
      self.longest_row = self.lines.row;
      self.longest_row_chars = joined_chars;
    }
    if other.longest_row_chars > self.longest_row_chars {
      self.longest_row = self.lines.row + other.longest_row;
      self.longest_row_chars = other.longest_row_chars;
    }

    if self.lines.row == 0 {
      self.first_line_chars += other.first_line_chars;
    }

    if other.lines.row == 0 {
      self.last_line_chars += other.first_line_chars;
    } else {
      self.last_line_chars = other.last_line_chars;
    }

    self.lines += &other.lines;
  }
}

pub struct TabChunks<'a> {
  snapshot: &'a TabSnapshot,
  max_expansion_column: u32,
  max_output_position: Point,
  tab_size: NonZeroU32,
  // region: iteration state
  fold_chunks: FoldChunks<'a>,
  chunk: Chunk<'a>,
  column: u32,
  output_position: Point,
  input_column: u32,
  inside_leading_tab: bool,
  // endregion: iteration state
}

impl TabChunks<'_> {
  #[ztracing::instrument(skip_all)]
  pub(crate) fn seek(&mut self, range: Range<TabPoint>) {
    let (input_start, expanded_char_column, to_next_stop) = self
      .snapshot
      .tab_point_to_fold_point(range.start, Bias::Left);
    let input_column = input_start.column();
    let input_start = input_start.to_offset(&self.snapshot.fold_snapshot);
    let input_end = self
      .snapshot
      .tab_point_to_fold_point(range.end, Bias::Right)
      .0
      .to_offset(&self.snapshot.fold_snapshot);
    let to_next_stop = if range.start.0 + Point::new(0, to_next_stop) > range.end.0 {
      range.end.column() - range.start.column()
    } else {
      to_next_stop
    };

    self.fold_chunks.seek(input_start..input_end);
    self.input_column = input_column;
    self.column = expanded_char_column;
    self.output_position = range.start.0;
    self.max_output_position = range.end.0;
    self.chunk = Chunk {
      text: unsafe { std::str::from_utf8_unchecked(&SPACES[..to_next_stop as usize]) },
      is_tab: true,
      chars: u128::MAX.unbounded_shr(u128::BITS - to_next_stop),
      ..Default::default()
    };
    self.inside_leading_tab = to_next_stop > 0;
  }
}

impl<'a> Iterator for TabChunks<'a> {
  type Item = Chunk<'a>;

  #[ztracing::instrument(skip_all)]
  fn next(&mut self) -> Option<Self::Item> {
    while self.chunk.text.is_empty() {
      let chunk = self.fold_chunks.next()?;
      self.chunk = chunk;
      if self.inside_leading_tab {
        self.chunk.text = &self.chunk.text[1..];
        self.chunk.tabs >>= 1;
        self.chunk.chars >>= 1;
        self.chunk.newlines >>= 1;
        self.inside_leading_tab = false;
        self.input_column += 1;
      }
    }

    if self.chunk.tabs & 1 != 0 {
      self.chunk.text = &self.chunk.text[1..];
      self.chunk.tabs >>= 1;
      self.chunk.chars >>= 1;
      self.chunk.newlines >>= 1;

      let tab_size = if self.input_column < self.max_expansion_column {
        self.tab_size.get()
      } else {
        1
      };
      let mut len = tab_size - self.column % tab_size;
      let next_output_position = cmp::min(
        self.output_position + Point::new(0, len),
        self.max_output_position,
      );
      len = next_output_position.column - self.output_position.column;
      self.column += len;
      self.input_column += 1;
      self.output_position = next_output_position;

      return Some(Chunk {
        text: unsafe { std::str::from_utf8_unchecked(&SPACES[..len as usize]) },
        is_tab: true,
        chars: u128::MAX.unbounded_shr(u128::BITS - len),
        tabs: 0,
        newlines: 0,
        ..self.chunk.clone()
      });
    }

    // Fast path: no tabs in the remaining chunk, return it directly
    if self.chunk.tabs == 0 {
      let chunk = self.chunk.clone();
      self.chunk.text = "";
      self.chunk.tabs = 0;
      self.chunk.chars = 0;
      self.chunk.newlines = 0;
      let chunk_len = chunk.text.len() as u32;

      let newline_count = chunk.newlines.count_ones();
      if newline_count > 0 {
        let last_newline_bit = 128 - chunk.newlines.leading_zeros();
        let chars_after_last_newline = chunk.chars.unbounded_shr(last_newline_bit).count_ones();
        let bytes_after_last_newline = chunk_len - last_newline_bit;

        self.column = chars_after_last_newline;
        self.input_column = bytes_after_last_newline;
        self.output_position = Point::new(
          self.output_position.row + newline_count,
          bytes_after_last_newline,
        );
      } else {
        let char_count = chunk.chars.count_ones();
        self.column += char_count;
        if !self.inside_leading_tab {
          self.input_column += chunk_len;
        }
        self.output_position.column += chunk_len;
      }

      return Some(chunk);
    }

    // Split at the next tab position
    let prefix_len = self.chunk.tabs.trailing_zeros() as usize;
    let (prefix, suffix) = self.chunk.text.split_at(prefix_len);

    let mask = 1u128.unbounded_shl(prefix_len as u32).wrapping_sub(1);
    let prefix_chars = self.chunk.chars & mask;
    let prefix_newlines = self.chunk.newlines & mask;

    self.chunk.text = suffix;
    self.chunk.tabs = self.chunk.tabs.unbounded_shr(prefix_len as u32);
    self.chunk.chars = self.chunk.chars.unbounded_shr(prefix_len as u32);
    self.chunk.newlines = self.chunk.newlines.unbounded_shr(prefix_len as u32);

    let newline_count = prefix_newlines.count_ones();
    if newline_count > 0 {
      let last_newline_bit = 128 - prefix_newlines.leading_zeros();
      let chars_after_last_newline = prefix_chars.unbounded_shr(last_newline_bit).count_ones();
      let bytes_after_last_newline = prefix_len as u32 - last_newline_bit;

      self.column = chars_after_last_newline;
      self.input_column = bytes_after_last_newline;
      self.output_position = Point::new(
        self.output_position.row + newline_count,
        bytes_after_last_newline,
      );
    } else {
      let char_count = prefix_chars.count_ones();
      self.column += char_count;
      if !self.inside_leading_tab {
        self.input_column += prefix_len as u32;
      }
      self.output_position.column += prefix_len as u32;
    }

    Some(Chunk {
      text: prefix,
      chars: prefix_chars,
      tabs: 0,
      newlines: prefix_newlines,
      ..self.chunk.clone()
    })
  }
}

struct TabStopCursor<'a> {
  chunks: FoldChunks<'a>,
  byte_offset: u32,
  char_offset: u32,
  /// Chunk
  /// last tab position iterated through
  current_chunk: Option<(TabStopChunk<'a>, u32)>,
  /// Whether a seek moved past the end of the row it started on, which only
  /// happens for columns beyond the end of the row.
  crossed_newline: bool,
}

struct TabStopChunk<'a> {
  chars: u128,
  text: &'a str,
  tabs: u128,
  newlines: u128,
}

impl<'a> TabStopChunk<'a> {
  fn new(chunk: Chunk<'a>) -> Self {
    Self {
      chars: chunk.chars,
      text: chunk.text,
      tabs: chunk.tabs,
      newlines: chunk.newlines,
    }
  }

  fn has_newline_in_byte_range(&self, range: Range<u32>) -> bool {
    let mask = u128::MAX.unbounded_shl(range.start) & !u128::MAX.unbounded_shl(range.end);
    self.newlines & mask != 0
  }
}

impl<'a> TabStopCursor<'a> {
  fn new(chunks: FoldChunks<'a>) -> Self {
    Self {
      chunks,
      byte_offset: 0,
      char_offset: 0,
      current_chunk: None,
      crossed_newline: false,
    }
  }

  fn seek_to(&mut self, range: Range<FoldOffset>) {
    self.chunks.seek(range);
    self.byte_offset = 0;
    self.char_offset = 0;
    self.current_chunk = None;
    self.crossed_newline = false;
  }

  /// Moves to the start of the row that is `row_delta` rows below the current
  /// one, pulling at most `max_chunks` new chunks. Returns false if that row
  /// was not reached, in which case the cursor must be repositioned with
  /// `seek_to`.
  fn advance_rows(&mut self, row_delta: u32, max_chunks: usize) -> bool {
    let mut rows_remaining = row_delta;
    let mut chunks_remaining = max_chunks;
    loop {
      let (mut chunk, chunk_position) = match self.current_chunk.take() {
        Some(current_chunk) => current_chunk,
        None => {
          if chunks_remaining == 0 {
            return false;
          }
          chunks_remaining -= 1;
          match self.chunks.next() {
            Some(chunk) => (TabStopChunk::new(chunk), 0),
            None => return false,
          }
        }
      };

      let mut pending_newlines = chunk.newlines & u128::MAX.unbounded_shl(chunk_position);
      let newline_count = pending_newlines.count_ones();
      if newline_count < rows_remaining {
        rows_remaining -= newline_count;
        continue;
      }

      for _ in 1..rows_remaining {
        pending_newlines &= pending_newlines - 1;
      }
      let row_start = pending_newlines.trailing_zeros() + 1;
      // `seek_forward` relies on every remaining tab being at or after the chunk position.
      chunk.tabs &= u128::MAX.unbounded_shl(row_start);
      if (row_start as usize) < chunk.text.len() {
        self.current_chunk = Some((chunk, row_start));
      }
      self.byte_offset = 0;
      self.char_offset = 0;
      return true;
    }
  }

  fn bytes_until_next_char(&self) -> Option<usize> {
    self.current_chunk.as_ref().map(|&(ref chunk, idx)| {
      let higher_chars = chunk.chars.unbounded_shr(idx + 1);

      if higher_chars != 0 {
        higher_chars.trailing_zeros() as usize + 1
      } else {
        chunk.text.len() - idx as usize
      }
    })
  }

  fn is_char_boundary(&self) -> bool {
    self
      .current_chunk
      .as_ref()
      .is_some_and(|&(ref chunk, idx)| {
        (1u128.unbounded_shl(idx) & chunk.chars) != 0 || idx as usize == chunk.text.len()
      })
  }

  /// distance: length to move forward while searching for the next tab stop
  #[ztracing::instrument(skip_all)]
  fn seek_forward(&mut self, distance: u32) -> Option<TabStop> {
    if distance == 0 {
      return None;
    }

    let mut distance_remaining = distance;

    while let Some((mut chunk, chunk_position)) = self.current_chunk.take().or_else(|| {
      self
        .chunks
        .next()
        .map(|chunk| (TabStopChunk::new(chunk), 0))
    }) {
      let chunk_len = chunk.text.len() as u32;

      if chunk.tabs == 0 {
        let chunk_remaining = chunk_len - chunk_position;
        if chunk_remaining >= distance_remaining {
          let end = chunk_position + distance_remaining;
          self.crossed_newline |= chunk.has_newline_in_byte_range(chunk_position..end);
          self.byte_offset += distance_remaining;
          self.char_offset += count_chars_in_byte_range(chunk_position..(end - 1), chunk.chars);
          if end < 128 {
            self.current_chunk = Some((chunk, end));
          }
          return None;
        }

        self.crossed_newline |= chunk.has_newline_in_byte_range(chunk_position..chunk_len);
        self.byte_offset += chunk_remaining;
        self.char_offset += count_chars_in_byte_range(chunk_position..(chunk_len - 1), chunk.chars);
        distance_remaining -= chunk_remaining;
        continue;
      }

      let tab_end = chunk.tabs.trailing_zeros() + 1;
      let bytes_to_tab = tab_end - chunk_position;

      if bytes_to_tab > distance_remaining {
        let end = chunk_position + distance_remaining;
        self.crossed_newline |= chunk.has_newline_in_byte_range(chunk_position..end);
        self.byte_offset += distance_remaining;
        self.char_offset += count_chars_in_byte_range(chunk_position..(end - 1), chunk.chars);
        self.current_chunk = Some((chunk, end));
        return None;
      }

      self.crossed_newline |= chunk.has_newline_in_byte_range(chunk_position..tab_end);
      self.byte_offset += bytes_to_tab;
      self.char_offset += count_chars_in_byte_range(chunk_position..(tab_end - 1), chunk.chars);

      let tabstop = TabStop {
        char_offset: self.char_offset,
        byte_offset: self.byte_offset,
      };

      chunk.tabs = (chunk.tabs - 1) & chunk.tabs;

      if tab_end != chunk_len {
        self.current_chunk = Some((chunk, tab_end));
      }

      return Some(tabstop);
    }

    None
  }

  fn byte_offset(&self) -> u32 {
    self.byte_offset
  }

  fn char_offset(&self) -> u32 {
    self.char_offset
  }
}

#[inline(always)]
fn count_chars_in_byte_range(range: Range<u32>, bitmap: u128) -> u32 {
  let low_mask = u128::MAX << range.start;
  let high_mask = u128::MAX >> (127 - range.end);
  (bitmap & low_mask & high_mask).count_ones()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TabStop {
  char_offset: u32,
  byte_offset: u32,
}
