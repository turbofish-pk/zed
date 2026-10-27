use language::{Bias, LanguageAwareStyling, Point};
use multi_buffer::{MBTextSummary, MultiBufferRow};
use std::ops::Range;
use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};

use crate::{
  DisplayPoint,
  display_map::{DisplaySnapshot, FoldPoint, Highlights, TabPoint, ToDisplayPoint as _},
};

pub(crate) struct ColumnarSelectionRows<'a> {
  snapshot: &'a DisplaySnapshot,
}

impl<'a> ColumnarSelectionRows<'a> {
  pub(crate) fn new(snapshot: &'a DisplaySnapshot) -> Self {
    Self { snapshot }
  }

  pub(crate) fn columns_for_ranges(&mut self, ranges: &[Range<Point>]) -> Vec<Range<u32>> {
    let tabs = self.snapshot.tab_snapshot();
    let mut endpoints = ranges
      .iter()
      .enumerate()
      .flat_map(|(index, range)| {
        [(range.start, false), (range.end, true)]
          .map(|(point, end)| (tabs.point_to_tab_point(point, Bias::Left), index, end))
      })
      .collect::<Vec<_>>();
    endpoints.sort_unstable();
    let mut answers = vec![0..0; ranges.len()];
    for endpoints in endpoints.chunk_by(|left, right| left.0.row() == right.0.row()) {
      let mut text = self.row(endpoints[0].0.row());
      let mut cursor = text.forward_cursor();
      for &(point, index, end) in endpoints {
        text.floor_forward(&mut cursor, point.column());
        if end {
          answers[index].end = cursor.column;
        } else {
          answers[index].start = cursor.column;
        }
      }
    }
    for answer in &mut answers {
      *answer = answer.start.min(answer.end)..answer.start.max(answer.end);
    }
    answers
  }

  pub(crate) fn points_for_rows(
    &mut self,
    queries: &[(u32, Range<u32>)],
  ) -> Vec<Option<(Point, Point)>> {
    let tabs = self.snapshot.tab_snapshot();
    let max_row = tabs.max_point().row();
    let mut endpoints = Vec::new();
    for (index, (row, columns)) in queries.iter().enumerate() {
      if *row <= max_row && columns.start <= columns.end {
        endpoints.push((*row, columns.start, false, index));
        if columns.start != columns.end {
          endpoints.push((*row, columns.end, true, index));
        }
      }
    }
    endpoints.sort_unstable();
    let mut answers = vec![None::<(Point, Point)>; queries.len()];
    for endpoints in endpoints.chunk_by(|left, right| left.0 == right.0) {
      let row = endpoints[0].0;
      let length = tabs.line_len(row);
      let mut text = self.row(row);
      let mut cursor = text.cursor(0);
      let mut column = 0;
      let mut preceding = None;
      let mut buffer_cursor = None;
      for &(_, goal, end, index) in endpoints {
        let columns = &queries[index].1;
        if (end && answers[index].is_none())
          || (!end && columns.start != columns.end && goal >= length)
        {
          continue;
        }
        text.advance_columns(&mut cursor, goal - column);
        column = goal;
        if !end && columns.start != columns.end && cursor.byte() == length {
          continue;
        }
        let point = self.point_for_cursor(
          row,
          &mut text,
          cursor.clone(),
          preceding,
          &mut buffer_cursor,
        );
        preceding = Some((cursor.byte(), point));
        answers[index] = if end {
          answers[index]
            .zip(point)
            .map(|((start, _), end)| (start.min(end), start.max(end)))
        } else {
          point.map(|point| (point, point))
        };
      }
    }
    answers
  }

  fn point_for_cursor(
    &self,
    row: u32,
    text: &mut RowText<'a>,
    mut cursor: BoundaryCursor,
    preceding: Option<(u32, Option<Point>)>,
    buffer_cursor: &mut Option<BufferRowCursor<'a>>,
  ) -> Option<Point> {
    let frontier = preceding.map_or(0, |(byte, _)| byte);
    let mut backward_buffer_cursor = None;
    if let Some(buffer_cursor) = buffer_cursor {
      buffer_cursor.reverse = None;
    }
    loop {
      if let Some((byte, point)) = preceding
        && cursor.byte() <= byte
      {
        return point;
      }
      let tab_point = TabPoint::new(row, cursor.byte());
      let tabs = self.snapshot.tab_snapshot();
      let fold_point = tabs.tab_point_to_fold_point(tab_point, Bias::Left).0;
      if let Some(range) = self
        .snapshot
        .fold_snapshot()
        .placeholder_range_at(fold_point)
        && range.start < fold_point
      {
        let start = tabs.fold_point_to_tab_point(range.start);
        text.retreat_to(&mut cursor, start.column().max(frontier));
        continue;
      }
      let point = tabs.tab_point_to_point(tab_point, Bias::Left);
      let display_point = point.to_display_point(self.snapshot);
      let hidden = self.snapshot.is_block_line(display_point.row());
      let canonical = tabs.point_to_tab_point(point, Bias::Left);
      if !hidden
        && canonical == tab_point
        && self.floor_buffer_point(buffer_cursor, &mut backward_buffer_cursor, point) == point
      {
        return Some(point);
      }
      let preceding = if hidden && canonical == tab_point {
        let hidden_start = self
          .snapshot
          .display_point_to_fold_point(DisplayPoint::new(display_point.row(), 0), Bias::Left);
        let hidden_start = tabs.fold_point_to_tab_point(hidden_start);
        if hidden_start.row() != row {
          return None;
        }
        if hidden_start < tab_point {
          hidden_start
        } else {
          TabPoint::new(row, hidden_start.column().checked_sub(1)?)
        }
      } else if canonical < tab_point {
        canonical
      } else {
        let previous = if let Some(byte) = point.column.checked_sub(1) {
          self.floor_buffer_point(
            buffer_cursor,
            &mut backward_buffer_cursor,
            Point::new(point.row, byte),
          )
        } else {
          let row = point.row.checked_sub(1)?;
          Point::new(
            row,
            self
              .snapshot
              .buffer_snapshot()
              .line_len(MultiBufferRow(row)),
          )
        };
        tabs.point_to_tab_point(previous, Bias::Left)
      };
      if preceding.row() != row {
        return None;
      }
      if preceding < tab_point {
        text.retreat_to(&mut cursor, preceding.column().max(frontier));
      } else {
        text.previous_boundary(&mut cursor)?;
      }
    }
  }

  fn floor_buffer_point(
    &self,
    forward: &mut Option<BufferRowCursor<'a>>,
    backward: &mut Option<BufferRowCursor<'a>>,
    point: Point,
  ) -> Point {
    if forward.as_ref().is_none_or(|cursor| cursor.row < point.row) {
      *backward = forward.replace(self.buffer_row_cursor(point.row));
    }
    let cursor = if let Some(cursor) = forward.as_mut().filter(|cursor| cursor.row == point.row) {
      cursor
    } else {
      if backward
        .as_ref()
        .is_none_or(|cursor| cursor.row != point.row)
      {
        let cursor = backward.insert(self.buffer_row_cursor(point.row));
        cursor.reverse = Some(cursor.text.floor_at(point.column));
      }
      backward.as_mut().expect("backward buffer row initialized")
    };
    if cursor.reverse.is_none() {
      cursor.text.floor_forward(&mut cursor.forward, point.column);
      cursor.reverse = Some(cursor.forward.boundary.clone());
    }
    let boundary = cursor
      .reverse
      .as_mut()
      .expect("buffer boundary initialized");
    cursor.text.retreat_to(boundary, point.column);
    Point::new(point.row, boundary.byte())
  }

  fn buffer_row_cursor(&self, row: u32) -> BufferRowCursor<'a> {
    let text = self.buffer_row(row);
    let forward = text.forward_cursor();
    BufferRowCursor {
      row,
      text,
      forward,
      reverse: None,
    }
  }

  fn buffer_row(&self, row: u32) -> RowText<'a> {
    let buffer = self.snapshot.buffer_snapshot();
    let length = buffer.line_len(MultiBufferRow(row));
    let range = Point::new(row, 0)..Point::new(row, length);
    let summary = buffer.text_summary_for_range::<MBTextSummary, _>(range.clone());
    if summary.len.0 == summary.chars {
      RowText::Ascii(length)
    } else {
      RowText::new(length, buffer.text_for_range(range))
    }
  }

  fn row(&self, row: u32) -> RowText<'a> {
    let tabs = self.snapshot.tab_snapshot();
    let folds = self.snapshot.fold_snapshot();
    let summary = folds
      .text_summary_for_range(FoldPoint::new(row, 0)..FoldPoint::new(row, folds.line_len(row)));
    let length = tabs.line_len(row);
    if summary.len.0 == summary.chars {
      RowText::Ascii(length)
    } else {
      let chunks = tabs
        .chunks(
          TabPoint::new(row, 0)..TabPoint::new(row, length),
          LanguageAwareStyling {
            tree_sitter: false,
            diagnostics: false,
          },
          Highlights::default(),
        )
        .map(|chunk| (chunk.text, chunk.is_tab));
      RowText::with_tabs(length, chunks)
    }
  }
}

enum RowText<'a> {
  Ascii(u32),
  Unicode(RowChunks<'a>),
}

struct RowChunks<'a> {
  length: u32,
  chunks: Vec<TextChunk<'a>>,
  remaining: Box<dyn Iterator<Item = (&'a str, bool)> + 'a>,
}

#[derive(Clone, Copy)]
struct TextChunk<'a> {
  start: usize,
  text: &'a str,
}

#[derive(Clone)]
struct BoundaryCursor {
  graphemes: GraphemeCursor,
  chunk_index: usize,
}

struct ForwardCursor {
  boundary: BoundaryCursor,
  next: BoundaryCursor,
  column: u32,
}

struct BufferRowCursor<'a> {
  row: u32,
  text: RowText<'a>,
  forward: ForwardCursor,
  reverse: Option<BoundaryCursor>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BoundaryQuery {
  Next,
  Previous,
  Check,
}

impl<'a> RowText<'a> {
  fn new(length: u32, chunks: impl Iterator<Item = &'a str> + 'a) -> Self {
    Self::with_tabs(length, chunks.map(|text| (text, false)))
  }

  fn with_tabs(length: u32, chunks: impl Iterator<Item = (&'a str, bool)> + 'a) -> Self {
    Self::Unicode(RowChunks {
      length,
      chunks: Vec::new(),
      remaining: Box::new(chunks),
    })
  }

  fn cursor(&self, byte: u32) -> BoundaryCursor {
    let length = match self {
      Self::Ascii(length) => *length,
      Self::Unicode(chunks) => chunks.length,
    };
    BoundaryCursor {
      graphemes: GraphemeCursor::new(byte.min(length) as usize, length as usize, true),
      chunk_index: 0,
    }
  }

  fn forward_cursor(&self) -> ForwardCursor {
    let boundary = self.cursor(0);
    ForwardCursor {
      next: boundary.clone(),
      boundary,
      column: 0,
    }
  }

  fn floor_forward(&mut self, cursor: &mut ForwardCursor, byte: u32) {
    debug_assert!(byte >= cursor.boundary.byte());
    if let Self::Ascii(length) = self {
      cursor.column = byte.min(*length);
      cursor.boundary.graphemes.set_cursor(cursor.column as usize);
      return;
    }
    while cursor.boundary.byte() < byte {
      if cursor.next.byte() == cursor.boundary.byte()
        && self.next_boundary(&mut cursor.next).is_none()
      {
        break;
      }
      if cursor.next.byte() > byte {
        break;
      }
      cursor.boundary = cursor.next.clone();
      cursor.column += 1;
    }
  }

  fn advance_columns(&mut self, cursor: &mut BoundaryCursor, count: u32) {
    if let Self::Ascii(length) = self {
      cursor
        .graphemes
        .set_cursor(cursor.byte().saturating_add(count).min(*length) as usize);
    } else {
      for _ in 0..count {
        if self.next_boundary(cursor).is_none() {
          break;
        }
      }
    }
  }

  fn floor_at(&mut self, byte: u32) -> BoundaryCursor {
    let mut cursor = self.cursor(byte);
    if let Self::Unicode(chunks) = self
      && cursor.byte() > 0
      && cursor.byte() < chunks.length
    {
      cursor.chunk_index = chunks.locate(cursor.byte() as usize);
      let chunk = chunks.chunk(cursor.chunk_index);
      let mut offset = cursor.byte() as usize - chunk.start;
      while !chunk.text.is_char_boundary(offset) {
        offset -= 1;
      }
      cursor.graphemes.set_cursor(chunk.start + offset);
      if chunks.resolve(&mut cursor, BoundaryQuery::Check).is_none() {
        chunks
          .resolve(&mut cursor, BoundaryQuery::Previous)
          .expect("non-boundary has a preceding boundary");
      }
    }
    cursor
  }

  fn retreat_to(&mut self, cursor: &mut BoundaryCursor, byte: u32) {
    if let Self::Ascii(_) = self {
      cursor
        .graphemes
        .set_cursor(cursor.byte().min(byte) as usize);
    } else {
      while cursor.byte() > byte {
        if self.previous_boundary(cursor).is_none() {
          break;
        }
      }
    }
  }

  fn next_boundary(&mut self, cursor: &mut BoundaryCursor) -> Option<u32> {
    match self {
      Self::Ascii(length) => {
        if cursor.byte() >= *length {
          return None;
        }
        cursor.graphemes.set_cursor(cursor.byte() as usize + 1);
        Some(cursor.byte())
      }
      Self::Unicode(chunks) => chunks.resolve(cursor, BoundaryQuery::Next),
    }
  }

  fn previous_boundary(&mut self, cursor: &mut BoundaryCursor) -> Option<u32> {
    match self {
      Self::Ascii(_) => {
        let byte = cursor.byte().checked_sub(1)?;
        cursor.graphemes.set_cursor(byte as usize);
        Some(byte)
      }
      Self::Unicode(chunks) => chunks.resolve(cursor, BoundaryQuery::Previous),
    }
  }
}

impl<'a> RowChunks<'a> {
  fn chunk(&mut self, index: usize) -> TextChunk<'a> {
    const TAB_CHARACTERS: [u8; 128] = [b'\t'; 128];
    while self.chunks.len() <= index {
      let (text, is_tab) = self
        .remaining
        .find(|(text, _)| !text.is_empty())
        .expect("row chunks must cover the row");
      let text = if is_tab {
        std::str::from_utf8(&TAB_CHARACTERS[..text.len()]).expect("tab controls are UTF-8")
      } else {
        text
      };
      let start = self
        .chunks
        .last()
        .map_or(0, |chunk| chunk.start + chunk.text.len());
      self.chunks.push(TextChunk { start, text });
    }
    self.chunks[index]
  }

  fn locate(&mut self, byte: usize) -> usize {
    while self
      .chunks
      .last()
      .is_none_or(|chunk| chunk.start + chunk.text.len() <= byte)
    {
      self.chunk(self.chunks.len());
    }
    self.chunks.partition_point(|chunk| chunk.start <= byte) - 1
  }

  fn resolve(&mut self, cursor: &mut BoundaryCursor, query: BoundaryQuery) -> Option<u32> {
    loop {
      let offset = cursor.graphemes.cur_cursor();
      if query == BoundaryQuery::Check && (offset == 0 || offset == self.length as usize) {
        return Some(offset as u32);
      }
      if (query == BoundaryQuery::Next && offset == self.length as usize)
        || (query == BoundaryQuery::Previous && offset == 0)
      {
        return None;
      }
      let chunk = loop {
        let chunk = self.chunk(cursor.chunk_index);
        if offset < chunk.start || (query == BoundaryQuery::Previous && offset == chunk.start) {
          cursor.chunk_index -= 1;
        } else if offset > chunk.start + chunk.text.len()
          || (query != BoundaryQuery::Previous && offset == chunk.start + chunk.text.len())
        {
          cursor.chunk_index += 1;
        } else {
          break chunk;
        }
      };
      let mut bridge = [0; 8];
      let (text, start) = if query != BoundaryQuery::Previous && offset == chunk.start && offset > 0
      {
        let previous = self.chunk(cursor.chunk_index - 1);
        let previous = previous.text.chars().next_back().expect("nonempty chunk");
        let following = chunk.text.chars().next().expect("nonempty chunk");
        let previous_length = previous.encode_utf8(&mut bridge).len();
        let following_length = following.encode_utf8(&mut bridge[previous_length..]).len();
        (
          std::str::from_utf8(&bridge[..previous_length + following_length])
            .expect("encoded scalars are UTF-8"),
          offset - previous_length,
        )
      } else {
        (chunk.text, chunk.start)
      };
      loop {
        let result = match query {
          BoundaryQuery::Next => cursor.graphemes.next_boundary(text, start),
          BoundaryQuery::Previous => cursor.graphemes.prev_boundary(text, start),
          BoundaryQuery::Check => cursor
            .graphemes
            .is_boundary(text, start)
            .map(|boundary| boundary.then_some(cursor.graphemes.cur_cursor())),
        };
        match result {
          Ok(boundary) => return boundary.map(|byte| byte as u32),
          Err(GraphemeIncomplete::PreContext(end)) => {
            let index = self.locate(end - 1);
            let context = self.chunk(index);
            let context_text = &context.text[..end - context.start];

            cursor
              .graphemes
              .provide_context(context_text, context.start);
          }
          Err(GraphemeIncomplete::NextChunk | GraphemeIncomplete::PrevChunk) => break,
          Err(error) => unreachable!("row grapheme cursor: {error:?}"),
        }
      }
    }
  }
}

impl BoundaryCursor {
  fn byte(&self) -> u32 {
    self.graphemes.cur_cursor() as u32
  }
}
