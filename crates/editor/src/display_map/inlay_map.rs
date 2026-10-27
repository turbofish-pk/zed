//! The inlay map. See the [`display_map`][super] docs for an overview of how the inlay map fits
//! into the rest of the [`DisplayMap`][super::DisplayMap]. Much of the documentation for this
//! module generalizes to other layers.
//!
//! The core of this module is the [`InlayMap`] struct, which maintains a vec of [`Inlay`]s, and
//! [`InlaySnapshot`], which holds a sum tree of [`Transform`]s.

use crate::{
  ChunkRenderer, HighlightStyles,
  inlays::{Inlay, InlayContent},
};
use collections::BTreeSet;
use language::{Chunk, Edit, LanguageAwareStyling, Point, TextSummary};
use multi_buffer::{
  Anchor, MBTextSummary, MultiBufferOffset, MultiBufferRow, MultiBufferRows, MultiBufferSnapshot,
  RowInfo, ToOffset,
};
use project::InlayId;
use smallvec::SmallVec;
use std::{
  cmp, iter,
  ops::{Add, AddAssign, Range, Sub, SubAssign},
  slice,
  sync::Arc,
};
use sum_tree::{Bias, Cursor, Dimensions, SumTree};
use text::{ChunkBitmaps, Patch};
use ui::{ActiveTheme, IntoElement as _, ParentElement as _, Styled as _, div};

use super::{Highlights, custom_highlights::CustomHighlightsChunks, fold_map::ChunkRendererId};

/// Decides where the [`Inlay`]s should be displayed.
///
/// See the [`display_map` module documentation](crate::display_map) for more information.
pub struct InlayMap {
  snapshot: InlaySnapshot,
  inlays: Vec<Inlay>,
}

#[derive(Clone)]
pub struct InlaySnapshot {
  pub buffer: MultiBufferSnapshot,
  transforms: SumTree<Transform>,
  pub version: usize,
}

impl std::ops::Deref for InlaySnapshot {
  type Target = MultiBufferSnapshot;

  fn deref(&self) -> &Self::Target {
    &self.buffer
  }
}

#[derive(Clone, Debug)]
enum Transform {
  Isomorphic(MBTextSummary),
  Inlay(Inlay),
}

impl sum_tree::Item for Transform {
  type Summary = TransformSummary;

  #[ztracing::instrument(skip_all)]
  fn summary(&self, _: ()) -> Self::Summary {
    match self {
      Transform::Isomorphic(summary) => TransformSummary {
        input: *summary,
        output: *summary,
      },
      Transform::Inlay(inlay) => TransformSummary {
        input: MBTextSummary::default(),
        output: MBTextSummary::from(inlay.text().summary()),
      },
    }
  }
}

#[derive(Clone, Debug, Default)]
struct TransformSummary {
  /// Summary of the text before inlays have been applied.
  input: MBTextSummary,
  /// Summary of the text after inlays have been applied.
  output: MBTextSummary,
}

impl TransformSummary {
  fn has_inlays(&self) -> bool {
    self.input.len != self.output.len
  }
}

impl sum_tree::ContextLessSummary for TransformSummary {
  fn zero() -> Self {
    Default::default()
  }

  fn add_summary(&mut self, other: &Self) {
    self.input += other.input;
    self.output += other.output;
  }
}

pub type InlayEdit = Edit<InlayOffset>;

#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq)]
pub struct InlayOffset(pub MultiBufferOffset);

impl Add for InlayOffset {
  type Output = Self;

  fn add(self, rhs: Self) -> Self::Output {
    Self(self.0 + rhs.0)
  }
}

impl Sub for InlayOffset {
  type Output = <MultiBufferOffset as Sub>::Output;

  fn sub(self, rhs: Self) -> Self::Output {
    self.0 - rhs.0
  }
}

impl<T> SubAssign<T> for InlayOffset
where
  MultiBufferOffset: SubAssign<T>,
{
  fn sub_assign(&mut self, rhs: T) {
    self.0 -= rhs;
  }
}

impl<T> Add<T> for InlayOffset
where
  MultiBufferOffset: Add<T, Output = MultiBufferOffset>,
{
  type Output = Self;

  fn add(self, rhs: T) -> Self::Output {
    Self(self.0 + rhs)
  }
}

impl AddAssign for InlayOffset {
  fn add_assign(&mut self, rhs: Self) {
    self.0 += rhs.0;
  }
}

impl<T> AddAssign<T> for InlayOffset
where
  MultiBufferOffset: AddAssign<T>,
{
  fn add_assign(&mut self, rhs: T) {
    self.0 += rhs;
  }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for InlayOffset {
  fn zero(_cx: ()) -> Self {
    Default::default()
  }

  fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
    self.0 += summary.output.len;
  }
}

#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq)]
pub struct InlayPoint(pub Point);

impl Add for InlayPoint {
  type Output = Self;

  fn add(self, rhs: Self) -> Self::Output {
    Self(self.0 + rhs.0)
  }
}

impl Sub for InlayPoint {
  type Output = Self;

  fn sub(self, rhs: Self) -> Self::Output {
    Self(self.0 - rhs.0)
  }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for InlayPoint {
  fn zero(_cx: ()) -> Self {
    Default::default()
  }

  fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
    self.0 += &summary.output.lines;
  }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for MultiBufferOffset {
  fn zero(_cx: ()) -> Self {
    Default::default()
  }

  fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
    *self += summary.input.len;
  }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for Point {
  fn zero(_cx: ()) -> Self {
    Default::default()
  }

  fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
    *self += &summary.input.lines;
  }
}

#[derive(Clone)]
pub struct InlayBufferRows<'a> {
  transforms: Cursor<'a, 'static, Transform, Dimensions<InlayPoint, Point>>,
  buffer_rows: MultiBufferRows<'a>,
  inlay_row: u32,
  max_buffer_row: MultiBufferRow,
}

pub struct InlayChunks<'a> {
  transforms: Cursor<'a, 'static, Transform, Dimensions<InlayOffset, MultiBufferOffset>>,
  buffer_chunks: CustomHighlightsChunks<'a>,
  buffer_chunk: Option<Chunk<'a>>,
  inlay_chunks: Option<text::ChunkWithBitmaps<'a>>,
  /// text, char bitmap, tabs bitmap
  inlay_chunk: Option<ChunkBitmaps<'a>>,
  output_offset: InlayOffset,
  max_output_offset: InlayOffset,
  highlight_styles: HighlightStyles,
  highlights: Highlights<'a>,
  snapshot: &'a InlaySnapshot,
}

#[derive(Clone)]
pub struct InlayChunk<'a> {
  pub chunk: Chunk<'a>,
  /// Whether the inlay should be customly rendered.
  pub renderer: Option<ChunkRenderer>,
}

impl InlayChunks<'_> {
  #[ztracing::instrument(skip_all)]
  pub fn seek(&mut self, new_range: Range<InlayOffset>) {
    self.transforms.seek(&new_range.start, Bias::Right);

    let buffer_range = self.snapshot.to_buffer_offset(new_range.start)
      ..self.snapshot.to_buffer_offset(new_range.end);
    self.buffer_chunks.seek(buffer_range);
    self.inlay_chunks = None;
    self.buffer_chunk = None;
    self.output_offset = new_range.start;
    self.max_output_offset = new_range.end;
  }

  pub fn offset(&self) -> InlayOffset {
    self.output_offset
  }
}

impl<'a> Iterator for InlayChunks<'a> {
  type Item = InlayChunk<'a>;

  #[ztracing::instrument(skip_all)]
  fn next(&mut self) -> Option<Self::Item> {
    if self.output_offset == self.max_output_offset {
      return None;
    }

    let chunk = match self.transforms.item()? {
      Transform::Isomorphic(_) => {
        let chunk = self
          .buffer_chunk
          .get_or_insert_with(|| self.buffer_chunks.next().unwrap());
        if chunk.text.is_empty() {
          *chunk = self.buffer_chunks.next().unwrap();
        }

        let desired_bytes = self.transforms.end().0.0 - self.output_offset.0;

        // If we're already at the transform boundary, skip to the next transform
        if desired_bytes == 0 {
          self.inlay_chunks = None;
          self.transforms.next();
          return self.next();
        }

        // Determine split index handling edge cases
        let split_index = if desired_bytes >= chunk.text.len() {
          chunk.text.len()
        } else {
          chunk.text.ceil_char_boundary(desired_bytes)
        };

        let (prefix, suffix) = chunk.text.split_at(split_index);
        self.output_offset.0 += prefix.len();

        let mask = 1u128.unbounded_shl(split_index as u32).wrapping_sub(1);
        let chars = chunk.chars & mask;
        let tabs = chunk.tabs & mask;
        let newlines = chunk.newlines & mask;

        chunk.chars = chunk.chars.unbounded_shr(split_index as u32);
        chunk.tabs = chunk.tabs.unbounded_shr(split_index as u32);
        chunk.newlines = chunk.newlines.unbounded_shr(split_index as u32);
        chunk.text = suffix;

        InlayChunk {
          chunk: Chunk {
            text: prefix,
            chars,
            tabs,
            newlines,
            ..chunk.clone()
          },
          renderer: None,
        }
      }
      Transform::Inlay(inlay) => {
        let mut inlay_style_and_highlight = None;
        if let Some(inlay_highlights) = self.highlights.inlay_highlights {
          for (_, inlay_id_to_data) in inlay_highlights.iter() {
            let style_and_highlight = inlay_id_to_data.get(&inlay.id);
            if style_and_highlight.is_some() {
              inlay_style_and_highlight = style_and_highlight;
              break;
            }
          }
        }

        let mut renderer = None;
        let mut highlight_style = match inlay.id {
          InlayId::Hint(_) => self.highlight_styles.inlay_hint,
          InlayId::DebuggerValue(_) => self.highlight_styles.inlay_hint,
          InlayId::ReplResult(_) => {
            let text = inlay.text().to_string();
            renderer = Some(ChunkRenderer {
              id: ChunkRendererId::Inlay(inlay.id),
              render: Arc::new(move |cx| {
                let colors = cx.theme().colors();
                div()
                  .flex()
                  .flex_row()
                  .items_center()
                  .child(div().w_4())
                  .child(
                    div()
                      .px_1()
                      .rounded_sm()
                      .bg(colors.surface_background)
                      .text_color(colors.text_muted)
                      .text_xs()
                      .child(text.trim().to_string()),
                  )
                  .into_any_element()
              }),
              constrain_width: false,
              measured_width: None,
            });
            self.highlight_styles.inlay_hint
          }
          InlayId::Color(_) => {
            if let InlayContent::Color(color) = inlay.content {
              renderer = Some(ChunkRenderer {
                id: ChunkRendererId::Inlay(inlay.id),
                render: Arc::new(move |cx| {
                  div()
                    .relative()
                    .size_3p5()
                    .child(
                      div()
                        .absolute()
                        .right_1()
                        .size_3()
                        .border_1()
                        .border_color(if cx.theme().appearance().is_light() {
                          gpui::black().opacity(0.5)
                        } else {
                          gpui::white().opacity(0.5)
                        })
                        .bg(color),
                    )
                    .into_any_element()
                }),
                constrain_width: false,
                measured_width: None,
              });
            }
            self.highlight_styles.inlay_hint
          }
        };
        let next_inlay_highlight_endpoint;
        let offset_in_inlay = self.output_offset - self.transforms.start().0;
        if let Some((style, highlight)) = inlay_style_and_highlight {
          let range = &highlight.range;
          if offset_in_inlay < range.start {
            next_inlay_highlight_endpoint = range.start - offset_in_inlay;
          } else if offset_in_inlay >= range.end {
            next_inlay_highlight_endpoint = usize::MAX;
          } else {
            next_inlay_highlight_endpoint = range.end - offset_in_inlay;
            highlight_style = highlight_style
              .map(|highlight| highlight.highlight(*style))
              .or_else(|| Some(*style));
          }
        } else {
          next_inlay_highlight_endpoint = usize::MAX;
        }

        let inlay_chunks = self.inlay_chunks.get_or_insert_with(|| {
          let start = offset_in_inlay;
          let end =
            cmp::min(self.max_output_offset, self.transforms.end().0) - self.transforms.start().0;
          let chunks = inlay.text().chunks_in_range(start..end);
          text::ChunkWithBitmaps(chunks)
        });
        let ChunkBitmaps {
          text: inlay_chunk,
          chars,
          tabs,
          newlines,
        } = self
          .inlay_chunk
          .get_or_insert_with(|| inlay_chunks.next().unwrap());

        // Determine split index handling edge cases
        let split_index = if next_inlay_highlight_endpoint >= inlay_chunk.len() {
          inlay_chunk.len()
        } else if next_inlay_highlight_endpoint == 0 {
          // Need to take at least one character to make progress
          inlay_chunk
            .chars()
            .next()
            .map(|c| c.len_utf8())
            .unwrap_or(1)
        } else {
          inlay_chunk.ceil_char_boundary(next_inlay_highlight_endpoint)
        };

        let (chunk, remainder) = inlay_chunk.split_at(split_index);
        *inlay_chunk = remainder;

        let mask = 1u128.unbounded_shl(split_index as u32).wrapping_sub(1);
        let new_chars = *chars & mask;
        let new_tabs = *tabs & mask;
        let new_newlines = *newlines & mask;

        *chars = chars.unbounded_shr(split_index as u32);
        *tabs = tabs.unbounded_shr(split_index as u32);
        *newlines = newlines.unbounded_shr(split_index as u32);

        if inlay_chunk.is_empty() {
          self.inlay_chunk = None;
        }

        self.output_offset.0 += chunk.len();

        InlayChunk {
          chunk: Chunk {
            text: chunk,
            chars: new_chars,
            tabs: new_tabs,
            newlines: new_newlines,
            highlight_style,
            is_inlay: true,
            ..Chunk::default()
          },
          renderer,
        }
      }
    };

    if self.output_offset >= self.transforms.end().0 {
      self.inlay_chunks = None;
      self.transforms.next();
    }

    Some(chunk)
  }
}

impl InlayBufferRows<'_> {
  #[ztracing::instrument(skip_all)]
  pub fn seek(&mut self, row: u32) {
    let inlay_point = InlayPoint::new(row, 0);
    self.transforms.seek(&inlay_point, Bias::Left);

    let mut buffer_point = self.transforms.start().1;
    let buffer_row = MultiBufferRow(if row == 0 {
      0
    } else {
      match self.transforms.item() {
        Some(Transform::Isomorphic(_)) => {
          buffer_point += inlay_point.0 - self.transforms.start().0.0;
          buffer_point.row
        }
        _ => cmp::min(buffer_point.row + 1, self.max_buffer_row.0),
      }
    });
    self.inlay_row = inlay_point.row();
    self.buffer_rows.seek(buffer_row);
  }
}

impl Iterator for InlayBufferRows<'_> {
  type Item = RowInfo;

  #[ztracing::instrument(skip_all)]
  fn next(&mut self) -> Option<Self::Item> {
    let buffer_row = if self.inlay_row == 0 {
      self.buffer_rows.next().unwrap()
    } else {
      match self.transforms.item()? {
        Transform::Inlay(_) => Default::default(),
        Transform::Isomorphic(_) => self.buffer_rows.next().unwrap(),
      }
    };

    self.inlay_row += 1;
    self
      .transforms
      .seek_forward(&InlayPoint::new(self.inlay_row, 0), Bias::Left);

    Some(buffer_row)
  }
}

impl InlayPoint {
  pub fn new(row: u32, column: u32) -> Self {
    Self(Point::new(row, column))
  }

  pub fn row(self) -> u32 {
    self.0.row
  }
}

impl InlayMap {
  #[ztracing::instrument(skip_all)]
  pub fn new(buffer: MultiBufferSnapshot) -> (Self, InlaySnapshot) {
    let version = 0;
    let snapshot = InlaySnapshot {
      transforms: SumTree::from_iter(iter::once(Transform::Isomorphic(buffer.text_summary())), ()),
      buffer,
      version,
    };

    (
      Self {
        snapshot: snapshot.clone(),
        inlays: Vec::new(),
      },
      snapshot,
    )
  }

  #[ztracing::instrument(skip_all)]
  pub fn sync(
    &mut self,
    buffer_snapshot: MultiBufferSnapshot,
    mut buffer_edits: Vec<text::Edit<MultiBufferOffset>>,
  ) -> (InlaySnapshot, Vec<InlayEdit>) {
    let snapshot = &mut self.snapshot;

    if buffer_edits.is_empty()
      && snapshot.buffer.trailing_excerpt_update_count()
        != buffer_snapshot.trailing_excerpt_update_count()
    {
      buffer_edits.push(Edit {
        old: snapshot.buffer.len()..snapshot.buffer.len(),
        new: buffer_snapshot.len()..buffer_snapshot.len(),
      });
    }

    if buffer_edits.is_empty() {
      if snapshot.buffer.edit_count() != buffer_snapshot.edit_count()
        || snapshot.buffer.non_text_state_update_count()
          != buffer_snapshot.non_text_state_update_count()
        || snapshot.buffer.trailing_excerpt_update_count()
          != buffer_snapshot.trailing_excerpt_update_count()
      {
        snapshot.version += 1;
      }

      snapshot.buffer = buffer_snapshot;
      (snapshot.clone(), Vec::new())
    } else if self.inlays.is_empty() && !snapshot.transforms.summary().has_inlays() {
      // Fast path: without inlays, the InlayMap is a passthrough, so rebuild a single
      // isomorphic transform and forward buffer edits as inlay edits verbatim.
      let mut new_transforms = SumTree::default();
      push_isomorphic(&mut new_transforms, buffer_snapshot.text_summary());
      if new_transforms.is_empty() {
        new_transforms.push(Transform::Isomorphic(Default::default()), ());
      }

      let mut inlay_edits = Patch::default();
      for buffer_edit in &buffer_edits {
        inlay_edits.push(Edit {
          old: InlayOffset(buffer_edit.old.start)..InlayOffset(buffer_edit.old.end),
          new: InlayOffset(buffer_edit.new.start)..InlayOffset(buffer_edit.new.end),
        });
      }

      snapshot.transforms = new_transforms;
      snapshot.version += 1;
      snapshot.buffer = buffer_snapshot;
      snapshot.check_invariants();

      (snapshot.clone(), inlay_edits.into_inner())
    } else {
      let mut inlay_edits = Patch::default();
      let mut new_transforms = SumTree::default();
      let mut cursor = snapshot
        .transforms
        .cursor::<Dimensions<MultiBufferOffset, InlayOffset>>(());
      let mut buffer_edits_iter = buffer_edits.iter().peekable();
      while let Some(buffer_edit) = buffer_edits_iter.next() {
        new_transforms.append(cursor.slice(&buffer_edit.old.start, Bias::Left), ());
        if let Some(Transform::Isomorphic(transform)) = cursor.item()
          && cursor.end().0 == buffer_edit.old.start
        {
          push_isomorphic(&mut new_transforms, *transform);
          cursor.next();
        }

        // Remove all the inlays and transforms contained by the edit.
        let old_start = cursor.start().1 + (buffer_edit.old.start - cursor.start().0);
        cursor.seek(&buffer_edit.old.end, Bias::Right);
        let old_end = cursor.start().1 + (buffer_edit.old.end - cursor.start().0);

        // Push the unchanged prefix.
        let prefix_start = new_transforms.summary().input.len;
        let prefix_end = buffer_edit.new.start;
        push_isomorphic(
          &mut new_transforms,
          buffer_snapshot.text_summary_for_range(prefix_start..prefix_end),
        );
        let new_start = InlayOffset(new_transforms.summary().output.len);

        let start_ix = match self.inlays.binary_search_by(|probe| {
          probe
            .position
            .to_offset(&buffer_snapshot)
            .cmp(&buffer_edit.new.start)
            .then(std::cmp::Ordering::Greater)
        }) {
          Ok(ix) | Err(ix) => ix,
        };

        for inlay in &self.inlays[start_ix..] {
          if !inlay.position.is_valid(&buffer_snapshot) {
            continue;
          }
          let buffer_offset = inlay.position.to_offset(&buffer_snapshot);
          if buffer_offset > buffer_edit.new.end {
            break;
          }

          let prefix_start = new_transforms.summary().input.len;
          let prefix_end = buffer_offset;
          push_isomorphic(
            &mut new_transforms,
            buffer_snapshot.text_summary_for_range(prefix_start..prefix_end),
          );

          new_transforms.push(Transform::Inlay(inlay.clone()), ());
        }

        // Apply the rest of the edit.
        let transform_start = new_transforms.summary().input.len;
        push_isomorphic(
          &mut new_transforms,
          buffer_snapshot.text_summary_for_range(transform_start..buffer_edit.new.end),
        );
        let new_end = InlayOffset(new_transforms.summary().output.len);
        inlay_edits.push(Edit {
          old: old_start..old_end,
          new: new_start..new_end,
        });

        // If the next edit doesn't intersect the current isomorphic transform, then
        // we can push its remainder.
        if buffer_edits_iter
          .peek()
          .is_none_or(|edit| edit.old.start >= cursor.end().0)
        {
          let transform_start = new_transforms.summary().input.len;
          let transform_end = buffer_edit.new.end + (cursor.end().0 - buffer_edit.old.end);
          push_isomorphic(
            &mut new_transforms,
            buffer_snapshot.text_summary_for_range(transform_start..transform_end),
          );
          cursor.next();
        }
      }

      new_transforms.append(cursor.suffix(), ());
      if new_transforms.is_empty() {
        new_transforms.push(Transform::Isomorphic(Default::default()), ());
      }

      drop(cursor);
      snapshot.transforms = new_transforms;
      snapshot.version += 1;
      snapshot.buffer = buffer_snapshot;
      snapshot.check_invariants();

      (snapshot.clone(), inlay_edits.into_inner())
    }
  }

  #[ztracing::instrument(skip_all)]
  pub fn splice(
    &mut self,
    to_remove: &[InlayId],
    to_insert: Vec<Inlay>,
  ) -> (InlaySnapshot, Vec<InlayEdit>) {
    let snapshot = &mut self.snapshot;
    let mut edits = BTreeSet::new();

    self.inlays.retain(|inlay| {
      let retain = !to_remove.contains(&inlay.id);
      if !retain {
        let offset = inlay.position.to_offset(&snapshot.buffer);
        edits.insert(offset);
      }
      retain
    });

    for inlay_to_insert in to_insert {
      // Avoid inserting empty inlays.
      if inlay_to_insert.text().is_empty() {
        continue;
      }

      let offset = inlay_to_insert.position.to_offset(&snapshot.buffer);
      match self.inlays.binary_search_by(|probe| {
        probe
          .position
          .cmp(&inlay_to_insert.position, &snapshot.buffer)
          .then(std::cmp::Ordering::Less)
      }) {
        Ok(ix) | Err(ix) => {
          self.inlays.insert(ix, inlay_to_insert);
        }
      }

      edits.insert(offset);
    }

    let buffer_edits = edits
      .into_iter()
      .map(|offset| Edit {
        old: offset..offset,
        new: offset..offset,
      })
      .collect();
    let buffer_snapshot = snapshot.buffer.clone();
    let (snapshot, edits) = self.sync(buffer_snapshot, buffer_edits);
    (snapshot, edits)
  }

  #[ztracing::instrument(skip_all)]
  pub fn current_inlays(&self) -> slice::Iter<'_, Inlay> {
    self.inlays.iter()
  }
}

impl InlaySnapshot {
  #[ztracing::instrument(skip_all)]
  pub fn to_point(&self, offset: InlayOffset) -> InlayPoint {
    let (start, _, item) = self
      .transforms
      .find::<Dimensions<InlayOffset, InlayPoint, MultiBufferOffset>, _>((), &offset, Bias::Right);
    let overshoot = offset.0 - start.0.0;
    match item {
      Some(Transform::Isomorphic(_)) => {
        let buffer_offset_start = start.2;
        let buffer_offset_end = buffer_offset_start + overshoot;
        let buffer_start = self.buffer.offset_to_point(buffer_offset_start);
        let buffer_end = self.buffer.offset_to_point(buffer_offset_end);
        InlayPoint(start.1.0 + (buffer_end - buffer_start))
      }
      Some(Transform::Inlay(inlay)) => {
        let overshoot = inlay.text().offset_to_point(overshoot);
        InlayPoint(start.1.0 + overshoot)
      }
      None => self.max_point(),
    }
  }

  #[ztracing::instrument(skip_all)]
  pub fn len(&self) -> InlayOffset {
    InlayOffset(self.transforms.summary().output.len)
  }

  #[ztracing::instrument(skip_all)]
  pub fn max_point(&self) -> InlayPoint {
    InlayPoint(self.transforms.summary().output.lines)
  }

  #[ztracing::instrument(skip_all, fields(point))]
  pub fn to_offset(&self, point: InlayPoint) -> InlayOffset {
    let (start, _, item) = self
      .transforms
      .find::<Dimensions<InlayPoint, InlayOffset, Point>, _>((), &point, Bias::Right);
    let overshoot = point.0 - start.0.0;
    match item {
      Some(Transform::Isomorphic(_)) => {
        let buffer_point_start = start.2;
        let buffer_point_end = buffer_point_start + overshoot;
        let buffer_offset_start = self.buffer.point_to_offset(buffer_point_start);
        let buffer_offset_end = self.buffer.point_to_offset(buffer_point_end);
        InlayOffset(start.1.0 + (buffer_offset_end - buffer_offset_start))
      }
      Some(Transform::Inlay(inlay)) => {
        let overshoot = inlay.text().point_to_offset(overshoot);
        InlayOffset(start.1.0 + overshoot)
      }
      None => self.len(),
    }
  }
  #[ztracing::instrument(skip_all)]
  pub fn to_buffer_point(&self, point: InlayPoint) -> Point {
    let (start, _, item) =
      self
        .transforms
        .find::<Dimensions<InlayPoint, Point>, _>((), &point, Bias::Right);
    match item {
      Some(Transform::Isomorphic(_)) => {
        let overshoot = point.0 - start.0.0;
        start.1 + overshoot
      }
      Some(Transform::Inlay(_)) => start.1,
      None => self.buffer.max_point(),
    }
  }
  #[ztracing::instrument(skip_all)]
  pub fn to_buffer_offset(&self, offset: InlayOffset) -> MultiBufferOffset {
    let (start, _, item) = self
      .transforms
      .find::<Dimensions<InlayOffset, MultiBufferOffset>, _>((), &offset, Bias::Right);
    match item {
      Some(Transform::Isomorphic(_)) => {
        let overshoot = offset - start.0;
        start.1 + overshoot
      }
      Some(Transform::Inlay(_)) => start.1,
      None => self.buffer.len(),
    }
  }

  #[ztracing::instrument(skip_all)]
  pub fn to_inlay_offset(&self, offset: MultiBufferOffset) -> InlayOffset {
    let mut cursor = self
      .transforms
      .cursor::<Dimensions<MultiBufferOffset, InlayOffset>>(());
    cursor.seek(&offset, Bias::Left);
    loop {
      match cursor.item() {
        Some(Transform::Isomorphic(_)) => {
          if offset == cursor.end().0 {
            while let Some(Transform::Inlay(inlay)) = cursor.next_item() {
              if inlay.position.bias() == Bias::Right {
                break;
              } else {
                cursor.next();
              }
            }
            return cursor.end().1;
          } else {
            let overshoot = offset - cursor.start().0;
            return InlayOffset(cursor.start().1.0 + overshoot);
          }
        }
        Some(Transform::Inlay(inlay)) => {
          if inlay.position.bias() == Bias::Left {
            cursor.next();
          } else {
            return cursor.start().1;
          }
        }
        None => {
          return self.len();
        }
      }
    }
  }

  #[ztracing::instrument(skip_all)]
  pub fn to_inlay_point(&self, point: Point) -> InlayPoint {
    self.inlay_point_cursor().map(point, Bias::Left)
  }

  /// Converts a buffer offset range into one or more `InlayOffset` ranges that
  /// cover only the actual buffer text, skipping any inlay hint text that falls
  /// within the range. When there are no inlays the returned vec contains a
  /// single element identical to the input mapped into inlay-offset space.
  pub fn buffer_offset_to_inlay_ranges(
    &self,
    range: Range<MultiBufferOffset>,
  ) -> impl Iterator<Item = Range<InlayOffset>> {
    let mut cursor = self
      .transforms
      .cursor::<Dimensions<MultiBufferOffset, InlayOffset>>(());
    cursor.seek(&range.start, Bias::Right);

    std::iter::from_fn(move || {
      loop {
        match cursor.item()? {
          Transform::Isomorphic(_) => {
            let seg_buffer_start = cursor.start().0;
            let seg_buffer_end = cursor.end().0;
            let seg_inlay_start = cursor.start().1;

            let overlap_start = cmp::max(range.start, seg_buffer_start);
            let overlap_end = cmp::min(range.end, seg_buffer_end);

            let past_end = seg_buffer_end >= range.end;
            cursor.next();

            if overlap_start < overlap_end {
              let inlay_start = InlayOffset(seg_inlay_start.0 + (overlap_start - seg_buffer_start));
              let inlay_end = InlayOffset(seg_inlay_start.0 + (overlap_end - seg_buffer_start));
              return Some(inlay_start..inlay_end);
            }

            if past_end {
              return None;
            }
          }
          Transform::Inlay(_) => cursor.next(),
        }
      }
    })
  }

  pub fn buffer_offset_to_inlay_point_cursor(&self) -> BufferOffsetToInlayPointCursor<'_> {
    BufferOffsetToInlayPointCursor {
      snapshot: self,
      cursor: self
        .transforms
        .cursor::<Dimensions<MultiBufferOffset, InlayPoint>>(()),
    }
  }

  #[ztracing::instrument(skip_all)]
  pub fn inlay_point_cursor(&self) -> InlayPointCursor<'_> {
    let cursor = self.transforms.cursor::<Dimensions<Point, InlayPoint>>(());
    InlayPointCursor {
      cursor,
      transforms: &self.transforms,
    }
  }

  #[ztracing::instrument(skip_all)]
  pub fn clip_point(&self, mut point: InlayPoint, mut bias: Bias) -> InlayPoint {
    let mut cursor = self.transforms.cursor::<Dimensions<InlayPoint, Point>>(());
    cursor.seek(&point, Bias::Left);
    loop {
      match cursor.item() {
        Some(Transform::Isomorphic(transform)) => {
          if cursor.start().0 == point {
            if let Some(Transform::Inlay(inlay)) = cursor.prev_item() {
              if inlay.position.bias() == Bias::Left {
                return point;
              } else if bias == Bias::Left {
                cursor.prev();
              } else if transform.first_line_chars == 0 {
                point.0 += Point::new(1, 0);
              } else {
                point.0 += Point::new(0, 1);
              }
            } else {
              return point;
            }
          } else if cursor.end().0 == point {
            if let Some(Transform::Inlay(inlay)) = cursor.next_item() {
              if inlay.position.bias() == Bias::Right {
                return point;
              } else if bias == Bias::Right {
                cursor.next();
              } else if point.0.column == 0 {
                point.0.row -= 1;
                point.0.column = self.line_len(point.0.row);
              } else {
                point.0.column -= 1;
              }
            } else {
              return point;
            }
          } else {
            let overshoot = point.0 - cursor.start().0.0;
            let buffer_point = cursor.start().1 + overshoot;
            let clipped_buffer_point = self.buffer.clip_point(buffer_point, bias);
            let clipped_overshoot = clipped_buffer_point - cursor.start().1;
            let clipped_point = InlayPoint(cursor.start().0.0 + clipped_overshoot);
            if clipped_point == point {
              return clipped_point;
            } else {
              point = clipped_point;
            }
          }
        }
        Some(Transform::Inlay(inlay)) => {
          if point == cursor.start().0 && inlay.position.bias() == Bias::Right {
            match cursor.prev_item() {
              Some(Transform::Inlay(inlay)) => {
                if inlay.position.bias() == Bias::Left {
                  return point;
                }
              }
              _ => return point,
            }
          } else if point == cursor.end().0 && inlay.position.bias() == Bias::Left {
            match cursor.next_item() {
              Some(Transform::Inlay(inlay)) => {
                if inlay.position.bias() == Bias::Right {
                  return point;
                }
              }
              _ => return point,
            }
          }

          if bias == Bias::Left {
            point = cursor.start().0;
            cursor.prev();
          } else {
            cursor.next();
            point = cursor.start().0;
          }
        }
        None => {
          bias = bias.invert();
          if bias == Bias::Left {
            point = cursor.start().0;
            cursor.prev();
          } else {
            cursor.next();
            point = cursor.start().0;
          }
        }
      }
    }
  }

  pub fn inlay_bias_at_point(&self, point: InlayPoint) -> Option<Bias> {
    let mut cursor = self.transforms.cursor::<Dimensions<InlayPoint, Point>>(());
    cursor.seek(&point, Bias::Left);
    match cursor.item() {
      Some(Transform::Inlay(inlay)) => Some(inlay.position.bias()),
      _ => None,
    }
  }

  #[ztracing::instrument(skip_all)]
  pub fn text_summary(&self) -> MBTextSummary {
    self.transforms.summary().output
  }

  #[ztracing::instrument(skip_all)]
  pub fn text_summary_for_range(&self, range: Range<InlayOffset>) -> MBTextSummary {
    let mut summary = MBTextSummary::default();

    let mut cursor = self
      .transforms
      .cursor::<Dimensions<InlayOffset, MultiBufferOffset>>(());
    cursor.seek(&range.start, Bias::Right);

    let overshoot = range.start.0 - cursor.start().0.0;
    match cursor.item() {
      Some(Transform::Isomorphic(_)) => {
        let buffer_start = cursor.start().1;
        let suffix_start = buffer_start + overshoot;
        let suffix_end =
          buffer_start + (cmp::min(cursor.end().0, range.end).0 - cursor.start().0.0);
        summary = self.buffer.text_summary_for_range(suffix_start..suffix_end);
        cursor.next();
      }
      Some(Transform::Inlay(inlay)) => {
        let suffix_start = overshoot;
        let suffix_end = cmp::min(cursor.end().0, range.end).0 - cursor.start().0.0;
        summary = MBTextSummary::from(
          inlay
            .text()
            .cursor(suffix_start)
            .summary::<TextSummary>(suffix_end),
        );
        cursor.next();
      }
      None => {}
    }

    if range.end > cursor.start().0 {
      summary += cursor
        .summary::<_, TransformSummary>(&range.end, Bias::Right)
        .output;

      let overshoot = range.end.0 - cursor.start().0.0;
      match cursor.item() {
        Some(Transform::Isomorphic(_)) => {
          let prefix_start = cursor.start().1;
          let prefix_end = prefix_start + overshoot;
          summary += self
            .buffer
            .text_summary_for_range::<MBTextSummary, _>(prefix_start..prefix_end);
        }
        Some(Transform::Inlay(inlay)) => {
          let prefix_end = overshoot;
          summary += inlay.text().cursor(0).summary::<TextSummary>(prefix_end);
        }
        None => {}
      }
    }

    summary
  }

  #[ztracing::instrument(skip_all)]
  pub fn row_infos(&self, row: u32) -> InlayBufferRows<'_> {
    let mut cursor = self.transforms.cursor::<Dimensions<InlayPoint, Point>>(());
    let inlay_point = InlayPoint::new(row, 0);
    cursor.seek(&inlay_point, Bias::Left);

    let max_buffer_row = self.buffer.max_row();
    let mut buffer_point = cursor.start().1;
    let buffer_row = if row == 0 {
      MultiBufferRow(0)
    } else {
      match cursor.item() {
        Some(Transform::Isomorphic(_)) => {
          buffer_point += inlay_point.0 - cursor.start().0.0;
          MultiBufferRow(buffer_point.row)
        }
        _ => cmp::min(MultiBufferRow(buffer_point.row + 1), max_buffer_row),
      }
    };

    InlayBufferRows {
      transforms: cursor,
      inlay_row: inlay_point.row(),
      buffer_rows: self.buffer.row_infos(buffer_row),
      max_buffer_row,
    }
  }

  #[ztracing::instrument(skip_all)]
  pub fn line_len(&self, row: u32) -> u32 {
    let line_start = self.to_offset(InlayPoint::new(row, 0)).0;
    let line_end = if row >= self.max_point().row() {
      self.len().0
    } else {
      self.to_offset(InlayPoint::new(row + 1, 0)).0 - 1
    };
    (line_end - line_start) as u32
  }

  #[ztracing::instrument(skip_all)]
  pub(crate) fn chunks<'a>(
    &'a self,
    range: Range<InlayOffset>,
    language_aware: LanguageAwareStyling,
    highlights: Highlights<'a>,
  ) -> InlayChunks<'a> {
    let mut cursor = self
      .transforms
      .cursor::<Dimensions<InlayOffset, MultiBufferOffset>>(());
    cursor.seek(&range.start, Bias::Right);

    let buffer_range = self.to_buffer_offset(range.start)..self.to_buffer_offset(range.end);
    let buffer_chunks = CustomHighlightsChunks::new(
      buffer_range,
      language_aware,
      highlights.text_highlights,
      highlights.semantic_token_highlights,
      &self.buffer,
    );

    InlayChunks {
      transforms: cursor,
      buffer_chunks,
      inlay_chunks: None,
      inlay_chunk: None,
      buffer_chunk: None,
      output_offset: range.start,
      max_output_offset: range.end,
      highlight_styles: highlights.styles,
      highlights,
      snapshot: self,
    }
  }

  pub(super) fn inlay_offset_range(
    &self,
    inlay_id: InlayId,
    position: Anchor,
  ) -> Option<Range<InlayOffset>> {
    let buffer_offset = position.to_offset(&self.buffer);
    let mut cursor = self
      .transforms
      .cursor::<Dimensions<MultiBufferOffset, InlayOffset>>(());
    cursor.seek(&buffer_offset, Bias::Left);
    while let Some(transform) = cursor.item() {
      if cursor.start().0 > buffer_offset {
        break;
      }
      if let Transform::Inlay(inlay) = transform
        && inlay.id == inlay_id
      {
        return Some(cursor.start().1..cursor.end().1);
      }
      cursor.next();
    }
    None
  }

  pub(super) fn inlay_hint_at_offset(&self, offset: InlayOffset) -> Option<(&Inlay, usize)> {
    let (start, _, transform) = self
      .transforms
      .find::<InlayOffset, _>((), &offset, Bias::Right);
    match transform {
      Some(Transform::Inlay(inlay)) if matches!(inlay.id, InlayId::Hint(_)) => {
        Some((inlay, offset - start))
      }
      _ => None,
    }
  }

  #[ztracing::instrument(skip_all)]
  fn check_invariants(&self) {
    #[cfg(any(debug_assertions, feature = "test-support"))]
    {
      assert_eq!(self.transforms.summary().input, self.buffer.text_summary());
      let mut transforms = self.transforms.iter().peekable();
      while let Some(transform) = transforms.next() {
        let transform_is_isomorphic = matches!(transform, Transform::Isomorphic(_));
        if let Some(next_transform) = transforms.peek() {
          let next_transform_is_isomorphic = matches!(next_transform, Transform::Isomorphic(_));
          assert!(
            !transform_is_isomorphic || !next_transform_is_isomorphic,
            "two adjacent isomorphic transforms"
          );
        }
      }
    }
  }
}

pub struct InlayPointCursor<'transforms> {
  cursor: Cursor<'transforms, 'static, Transform, Dimensions<Point, InlayPoint>>,
  transforms: &'transforms SumTree<Transform>,
}

impl InlayPointCursor<'_> {
  #[ztracing::instrument(skip_all)]
  pub fn map(&mut self, point: Point, bias: Bias) -> InlayPoint {
    let cursor = &mut self.cursor;
    if cursor.did_seek() {
      cursor.seek_forward(&point, Bias::Left);
    } else {
      cursor.seek(&point, Bias::Left);
    }
    loop {
      match cursor.item() {
        Some(Transform::Isomorphic(_)) => {
          if point == cursor.end().0 {
            while let Some(Transform::Inlay(inlay)) = cursor.next_item() {
              if bias == Bias::Left && inlay.position.bias() == Bias::Right {
                break;
              } else {
                cursor.next();
              }
            }
            return cursor.end().1;
          } else {
            let overshoot = point - cursor.start().0;
            return InlayPoint(cursor.start().1.0 + overshoot);
          }
        }
        Some(Transform::Inlay(inlay)) => {
          if inlay.position.bias() == Bias::Left || bias == Bias::Right {
            cursor.next();
          } else {
            return cursor.start().1;
          }
        }
        None => {
          return InlayPoint(self.transforms.summary().output.lines);
        }
      }
    }
  }
}

/// Forward-only cursor that maps buffer-offset ranges to the inlay-point ranges
/// covering only actual buffer text (excluding inlay text), reusing its tree
/// position across calls.
///
/// This is the streaming equivalent of
/// [`InlaySnapshot::buffer_offset_to_inlay_ranges`] composed with
/// [`InlaySnapshot::to_point`]. Because the cursor only seeks forward, callers
/// must provide ranges with non-decreasing offsets.
pub struct BufferOffsetToInlayPointCursor<'a> {
  snapshot: &'a InlaySnapshot,
  cursor: Cursor<'a, 'static, Transform, Dimensions<MultiBufferOffset, InlayPoint>>,
}

impl BufferOffsetToInlayPointCursor<'_> {
  /// Resets the cursor to the start so it can seek backward again.
  pub fn reset(&mut self) {
    self.cursor.reset();
  }

  pub fn map(&mut self, range: Range<MultiBufferOffset>) -> SmallVec<[Range<InlayPoint>; 1]> {
    let buffer = &self.snapshot.buffer;
    let cursor = &mut self.cursor;
    if cursor.did_seek() {
      cursor.seek_forward(&range.start, Bias::Right);
    } else {
      cursor.seek(&range.start, Bias::Right);
    }

    let mut result = SmallVec::new();
    loop {
      match cursor.item() {
        Some(Transform::Isomorphic(_)) => {
          let seg_buffer_start = cursor.start().0;
          let seg_buffer_end = cursor.end().0;
          let seg_inlay_point_start = cursor.start().1;

          let overlap_start = cmp::max(range.start, seg_buffer_start);
          let overlap_end = cmp::min(range.end, seg_buffer_end);

          if overlap_start < overlap_end {
            let seg_point_start = buffer.offset_to_point(seg_buffer_start);
            let start = InlayPoint(
              seg_inlay_point_start.0 + (buffer.offset_to_point(overlap_start) - seg_point_start),
            );
            let end = InlayPoint(
              seg_inlay_point_start.0 + (buffer.offset_to_point(overlap_end) - seg_point_start),
            );
            result.push(start..end);
          }

          // Leave the cursor on the transform containing `range.end`
          // rather than advancing past it, so a subsequent call with a
          // larger (but possibly same-transform) start does not seek
          // backward.
          if seg_buffer_end >= range.end {
            break;
          }
          cursor.next();
        }
        Some(Transform::Inlay(_)) => cursor.next(),
        None => break,
      }
    }
    result
  }
}

fn push_isomorphic(sum_tree: &mut SumTree<Transform>, summary: MBTextSummary) {
  if summary.len == MultiBufferOffset(0) {
    return;
  }

  let mut summary = Some(summary);
  sum_tree.update_last(
    |transform| {
      if let Transform::Isomorphic(transform) = transform {
        *transform += summary.take().unwrap();
      }
    },
    (),
  );

  if let Some(summary) = summary {
    sum_tree.push(Transform::Isomorphic(summary), ());
  }
}
