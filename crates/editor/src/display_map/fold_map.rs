use crate::display_map::inlay_map::InlayChunk;

use super::{
  Highlights,
  inlay_map::{InlayBufferRows, InlayChunks, InlayEdit, InlayOffset, InlayPoint, InlaySnapshot},
};
use gpui::{AnyElement, App, ElementId, HighlightStyle, Pixels, SharedString, Stateful, Window};
use language::{Edit, HighlightId, LanguageAwareStyling, Point};
use multi_buffer::{
  Anchor, AnchorRangeExt, MBTextSummary, MultiBufferOffset, MultiBufferRow, MultiBufferSnapshot,
  RowInfo, ToOffset,
};
use project::InlayId;
use std::{
  any::TypeId,
  cmp::{self, Ordering},
  fmt, iter,
  ops::{Add, AddAssign, Deref, DerefMut, Range, Sub, SubAssign},
  sync::Arc,
};
use sum_tree::{Bias, Cursor, Dimensions, FilterCursor, SumTree, Summary, TreeMap};
use ui::IntoElement as _;
use util::post_inc;

#[derive(Clone)]
pub struct FoldPlaceholder {
  /// Creates an element to represent this fold's placeholder.
  pub render: Arc<dyn Send + Sync + Fn(FoldId, Range<Anchor>, &mut App) -> AnyElement>,
  /// If true, the element is constrained to the shaped width of an ellipsis.
  pub constrain_width: bool,
  /// If true, merges the fold with an adjacent one.
  pub merge_adjacent: bool,
  /// Category of the fold. Useful for carefully removing from overlapping folds.
  pub type_tag: Option<TypeId>,
  /// Text provided by the language server to display in place of the folded range.
  /// When set, this is used instead of the default "⋯" ellipsis.
  pub collapsed_text: Option<SharedString>,
}

impl Default for FoldPlaceholder {
  fn default() -> Self {
    Self {
      render: Arc::new(|_, _, _| gpui::Empty.into_any_element()),
      constrain_width: true,
      merge_adjacent: true,
      type_tag: None,
      collapsed_text: None,
    }
  }
}

impl FoldPlaceholder {
  /// Returns a styled `Div` container with the standard fold‐placeholder
  /// look (background, hover, active, rounded corners, full size).
  /// Callers add children and event handlers on top.
  pub fn fold_element(fold_id: FoldId, cx: &App) -> Stateful<gpui::Div> {
    use gpui::{InteractiveElement as _, StatefulInteractiveElement as _, Styled as _};
    use settings::Settings as _;
    use theme::ActiveTheme as _;
    use theme_settings::ThemeSettings;
    let settings = ThemeSettings::get_global(cx);
    gpui::div()
      .id(fold_id)
      .font(settings.buffer_font.clone())
      .text_color(cx.theme().colors().text_placeholder)
      .bg(cx.theme().colors().ghost_element_background)
      .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
      .active(|style| style.bg(cx.theme().colors().ghost_element_active))
      .rounded_xs()
      .size_full()
  }

  #[cfg(feature = "test-support")]
  pub fn test() -> Self {
    Self {
      render: Arc::new(|_id, _range, _cx| gpui::Empty.into_any_element()),
      constrain_width: true,
      merge_adjacent: true,
      type_tag: None,
      collapsed_text: None,
    }
  }
}

impl fmt::Debug for FoldPlaceholder {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("FoldPlaceholder")
      .field("constrain_width", &self.constrain_width)
      .field("collapsed_text", &self.collapsed_text)
      .finish()
  }
}

impl Eq for FoldPlaceholder {}

impl PartialEq for FoldPlaceholder {
  fn eq(&self, other: &Self) -> bool {
    Arc::ptr_eq(&self.render, &other.render)
      && self.constrain_width == other.constrain_width
      && self.collapsed_text == other.collapsed_text
  }
}

#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq)]
pub struct FoldPoint(pub Point);

impl FoldPoint {
  pub fn new(row: u32, column: u32) -> Self {
    Self(Point::new(row, column))
  }

  pub fn row(self) -> u32 {
    self.0.row
  }

  pub fn column(self) -> u32 {
    self.0.column
  }

  pub fn row_mut(&mut self) -> &mut u32 {
    &mut self.0.row
  }

  #[ztracing::instrument(skip_all)]
  pub fn to_inlay_point(self, snapshot: &FoldSnapshot) -> InlayPoint {
    let (start, _, _) = snapshot
      .transforms
      .find::<Dimensions<FoldPoint, InlayPoint>, _>((), &self, Bias::Right);
    let overshoot = self.0 - start.0.0;
    InlayPoint(start.1.0 + overshoot)
  }

  #[ztracing::instrument(skip_all)]
  pub fn to_offset(self, snapshot: &FoldSnapshot) -> FoldOffset {
    let (start, _, item) = snapshot
      .transforms
      .find::<Dimensions<FoldPoint, TransformSummary>, _>((), &self, Bias::Right);
    let overshoot = self.0 - start.1.output.lines;
    let mut offset = start.1.output.len;
    if !overshoot.is_zero() {
      let transform = item.expect("display point out of range");
      assert!(transform.placeholder.is_none());
      let end_inlay_offset = snapshot
        .inlay_snapshot
        .to_offset(InlayPoint(start.1.input.lines + overshoot));
      offset += end_inlay_offset.0 - start.1.input.len;
    }
    FoldOffset(offset)
  }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for FoldPoint {
  fn zero(_cx: ()) -> Self {
    Default::default()
  }

  fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
    self.0 += &summary.output.lines;
  }
}

pub(crate) struct FoldMapWriter<'a>(&'a mut FoldMap);

impl FoldMapWriter<'_> {
  #[ztracing::instrument(skip_all)]
  pub(crate) fn fold<T: ToOffset>(
    &mut self,
    ranges: impl IntoIterator<Item = (Range<T>, FoldPlaceholder)>,
  ) -> (FoldSnapshot, Vec<FoldEdit>) {
    let mut edits = Vec::new();
    let mut folds = Vec::new();
    let snapshot = self.0.snapshot.inlay_snapshot.clone();
    for (range, fold_text) in ranges.into_iter() {
      let buffer = &snapshot.buffer;
      let range = range.start.to_offset(buffer)..range.end.to_offset(buffer);

      // Ignore any empty ranges.
      if range.start == range.end {
        continue;
      }

      let fold_range = buffer.anchor_after(range.start)..buffer.anchor_before(range.end);
      // For now, ignore any ranges that span an excerpt boundary.
      if buffer
        .anchor_range_to_buffer_anchor_range(fold_range.clone())
        .is_none()
      {
        continue;
      }

      folds.push(Fold {
        id: FoldId(post_inc(&mut self.0.next_fold_id.0)),
        range: FoldRange(fold_range),
        placeholder: fold_text,
      });

      let inlay_range = snapshot.to_inlay_offset(range.start)..snapshot.to_inlay_offset(range.end);
      edits.push(InlayEdit {
        old: inlay_range.clone(),
        new: inlay_range,
      });
    }

    let buffer = &snapshot.buffer;
    folds.sort_unstable_by(|a, b| sum_tree::SeekTarget::cmp(&a.range, &b.range, buffer));

    self.0.snapshot.folds = {
      let mut new_tree = SumTree::new(buffer);
      let mut cursor = self.0.snapshot.folds.cursor::<FoldRange>(buffer);
      for fold in folds {
        self.0.snapshot.fold_metadata_by_id.insert(
          fold.id,
          FoldMetadata {
            range: fold.range.clone(),
            width: None,
          },
        );
        new_tree.append(cursor.slice(&fold.range, Bias::Right), buffer);
        new_tree.push(fold, buffer);
      }
      new_tree.append(cursor.suffix(), buffer);
      new_tree
    };

    let edits = consolidate_inlay_edits(edits);
    let edits = self.0.sync(snapshot.clone(), edits);
    (self.0.snapshot.clone(), edits)
  }

  /// Removes any folds with the given ranges.
  #[ztracing::instrument(skip_all)]
  pub(crate) fn remove_folds<T: ToOffset>(
    &mut self,
    ranges: impl IntoIterator<Item = Range<T>>,
    type_id: TypeId,
  ) -> (FoldSnapshot, Vec<FoldEdit>) {
    self.remove_folds_with(
      ranges,
      |fold| fold.placeholder.type_tag == Some(type_id),
      false,
    )
  }

  /// Removes any folds whose ranges intersect the given ranges.
  #[ztracing::instrument(skip_all)]
  pub(crate) fn unfold_intersecting<T: ToOffset>(
    &mut self,
    ranges: impl IntoIterator<Item = Range<T>>,
    inclusive: bool,
  ) -> (FoldSnapshot, Vec<FoldEdit>) {
    self.remove_folds_with(ranges, |_| true, inclusive)
  }

  /// Removes any folds that intersect the given ranges and for which the given predicate
  /// returns true.
  #[ztracing::instrument(skip_all)]
  fn remove_folds_with<T: ToOffset>(
    &mut self,
    ranges: impl IntoIterator<Item = Range<T>>,
    should_unfold: impl Fn(&Fold) -> bool,
    inclusive: bool,
  ) -> (FoldSnapshot, Vec<FoldEdit>) {
    let mut edits = Vec::new();
    let mut fold_ixs_to_delete = Vec::new();
    let snapshot = self.0.snapshot.inlay_snapshot.clone();
    let buffer = &snapshot.buffer;
    for range in ranges.into_iter() {
      let range = range.start.to_offset(buffer)..range.end.to_offset(buffer);
      let mut folds_cursor =
        intersecting_folds(&snapshot, &self.0.snapshot.folds, range.clone(), inclusive);
      while let Some(fold) = folds_cursor.item() {
        let offset_range = fold.range.start.to_offset(buffer)..fold.range.end.to_offset(buffer);
        if should_unfold(fold) {
          if offset_range.end > offset_range.start {
            let inlay_range = snapshot.to_inlay_offset(offset_range.start)
              ..snapshot.to_inlay_offset(offset_range.end);
            edits.push(InlayEdit {
              old: inlay_range.clone(),
              new: inlay_range,
            });
          }
          fold_ixs_to_delete.push(*folds_cursor.start());
          self.0.snapshot.fold_metadata_by_id.remove(&fold.id);
        }
        folds_cursor.next();
      }
    }

    fold_ixs_to_delete.sort_unstable();
    fold_ixs_to_delete.dedup();

    self.0.snapshot.folds = {
      let mut cursor = self.0.snapshot.folds.cursor::<MultiBufferOffset>(buffer);
      let mut folds = SumTree::new(buffer);
      for fold_ix in fold_ixs_to_delete {
        folds.append(cursor.slice(&fold_ix, Bias::Right), buffer);
        cursor.next();
      }
      folds.append(cursor.suffix(), buffer);
      folds
    };

    let edits = consolidate_inlay_edits(edits);
    let edits = self.0.sync(snapshot.clone(), edits);
    (self.0.snapshot.clone(), edits)
  }

  #[ztracing::instrument(skip_all)]
  pub(crate) fn update_fold_widths(
    &mut self,
    new_widths: impl IntoIterator<Item = (ChunkRendererId, Pixels)>,
  ) -> (FoldSnapshot, Vec<FoldEdit>) {
    let mut edits = Vec::new();
    let inlay_snapshot = self.0.snapshot.inlay_snapshot.clone();
    let buffer = &inlay_snapshot.buffer;

    for (id, new_width) in new_widths {
      let ChunkRendererId::Fold(id) = id else {
        continue;
      };
      if let Some(metadata) = self.0.snapshot.fold_metadata_by_id.get(&id).cloned()
        && Some(new_width) != metadata.width
      {
        let buffer_start = metadata.range.start.to_offset(buffer);
        let buffer_end = metadata.range.end.to_offset(buffer);
        let inlay_range =
          inlay_snapshot.to_inlay_offset(buffer_start)..inlay_snapshot.to_inlay_offset(buffer_end);
        edits.push(InlayEdit {
          old: inlay_range.clone(),
          new: inlay_range.clone(),
        });

        self.0.snapshot.fold_metadata_by_id.insert(
          id,
          FoldMetadata {
            range: metadata.range,
            width: Some(new_width),
          },
        );
      }
    }

    let edits = consolidate_inlay_edits(edits);
    let edits = self.0.sync(inlay_snapshot, edits);
    (self.0.snapshot.clone(), edits)
  }
}

/// Decides where the fold indicators should be; also tracks parts of a source file that are currently folded.
///
/// See the [`display_map` module documentation](crate::display_map) for more information.
pub struct FoldMap {
  snapshot: FoldSnapshot,
  next_fold_id: FoldId,
}

impl FoldMap {
  #[ztracing::instrument(skip_all)]
  pub fn new(inlay_snapshot: InlaySnapshot) -> (Self, FoldSnapshot) {
    let this = Self {
      snapshot: FoldSnapshot {
        folds: SumTree::new(&inlay_snapshot.buffer),
        transforms: SumTree::from_item(
          Transform {
            summary: TransformSummary {
              input: inlay_snapshot.text_summary(),
              output: inlay_snapshot.text_summary(),
            },
            placeholder: None,
          },
          (),
        ),
        inlay_snapshot: inlay_snapshot,
        version: 0,
        fold_metadata_by_id: TreeMap::default(),
      },
      next_fold_id: FoldId::default(),
    };
    let snapshot = this.snapshot.clone();
    (this, snapshot)
  }

  #[ztracing::instrument(skip_all)]
  pub fn read(
    &mut self,
    inlay_snapshot: InlaySnapshot,
    edits: Vec<InlayEdit>,
  ) -> (FoldSnapshot, Vec<FoldEdit>) {
    let edits = self.sync(inlay_snapshot, edits);
    self.check_invariants();
    (self.snapshot.clone(), edits)
  }

  #[ztracing::instrument(skip_all)]
  pub(crate) fn write(
    &mut self,
    inlay_snapshot: InlaySnapshot,
    edits: Vec<InlayEdit>,
  ) -> (FoldMapWriter<'_>, FoldSnapshot, Vec<FoldEdit>) {
    let (snapshot, edits) = self.read(inlay_snapshot, edits);
    (FoldMapWriter(self), snapshot, edits)
  }

  #[ztracing::instrument(skip_all)]
  fn check_invariants(&self) {}

  #[ztracing::instrument(skip_all)]
  fn sync(&mut self, inlay_snapshot: InlaySnapshot, inlay_edits: Vec<InlayEdit>) -> Vec<FoldEdit> {
    if inlay_edits.is_empty() {
      if self.snapshot.inlay_snapshot.version != inlay_snapshot.version {
        self.snapshot.version += 1;
      }
      self.snapshot.inlay_snapshot = inlay_snapshot;
      Vec::new()
    } else {
      let mut inlay_edits_iter = inlay_edits.iter().cloned().peekable();

      let mut new_transforms = SumTree::<Transform>::default();
      let mut cursor = self.snapshot.transforms.cursor::<InlayOffset>(());
      cursor.seek(&InlayOffset(MultiBufferOffset(0)), Bias::Right);

      while let Some(mut edit) = inlay_edits_iter.next() {
        if let Some(item) = cursor.item()
          && !item.is_fold()
        {
          new_transforms.update_last(
            |transform| {
              if !transform.is_fold() {
                transform.summary.add_summary(&item.summary, ());
                cursor.next();
              }
            },
            (),
          );
        }
        new_transforms.append(cursor.slice(&edit.old.start, Bias::Left), ());
        edit.new.start -= edit.old.start - *cursor.start();
        edit.old.start = *cursor.start();

        cursor.seek(&edit.old.end, Bias::Right);
        cursor.next();

        let mut delta = edit.new_len() as isize - edit.old_len() as isize;
        loop {
          edit.old.end = *cursor.start();

          if let Some(next_edit) = inlay_edits_iter.peek() {
            if next_edit.old.start > edit.old.end {
              break;
            }

            let next_edit = inlay_edits_iter.next().unwrap();
            delta += next_edit.new_len() as isize - next_edit.old_len() as isize;

            if next_edit.old.end >= edit.old.end {
              edit.old.end = next_edit.old.end;
              cursor.seek(&edit.old.end, Bias::Right);
              cursor.next();
            }
          } else {
            break;
          }
        }

        edit.new.end = InlayOffset(MultiBufferOffset(
          ((edit.new.start + edit.old_len()).0.0 as isize + delta) as usize,
        ));

        let mut folds_cursor = self
          .snapshot
          .folds
          .cursor::<FoldRange>(&inlay_snapshot.buffer);
        folds_cursor.seek(&inlay_snapshot.to_buffer_offset(edit.new.start), Bias::Left);

        let mut folds = iter::from_fn({
          let inlay_snapshot = &inlay_snapshot;
          move || {
            let item = folds_cursor.item().map(|fold| {
              let buffer_start = fold.range.start.to_offset(&inlay_snapshot.buffer);
              let buffer_end = fold.range.end.to_offset(&inlay_snapshot.buffer);
              (
                fold.clone(),
                inlay_snapshot.to_inlay_offset(buffer_start)
                  ..inlay_snapshot.to_inlay_offset(buffer_end),
              )
            });
            folds_cursor.next();
            item
          }
        })
        .peekable();

        while folds
          .peek()
          .is_some_and(|(_, fold_range)| fold_range.start < edit.new.end)
        {
          let (fold, mut fold_range) = folds.next().unwrap();
          let sum = new_transforms.summary();

          assert!(fold_range.start.0 >= sum.input.len);

          while folds.peek().is_some_and(|(next_fold, next_fold_range)| {
            next_fold_range.start < fold_range.end
              || (next_fold_range.start == fold_range.end
                && fold.placeholder.merge_adjacent
                && next_fold.placeholder.merge_adjacent)
          }) {
            let (_, next_fold_range) = folds.next().unwrap();
            if next_fold_range.end > fold_range.end {
              fold_range.end = next_fold_range.end;
            }
          }

          if fold_range.start.0 > sum.input.len {
            let text_summary =
              inlay_snapshot.text_summary_for_range(InlayOffset(sum.input.len)..fold_range.start);
            push_isomorphic(&mut new_transforms, text_summary);
          }

          if fold_range.end > fold_range.start {
            const ELLIPSIS: &str = "⋯";

            let placeholder_text: SharedString = fold
              .placeholder
              .collapsed_text
              .clone()
              .unwrap_or_else(|| ELLIPSIS.into());
            let chars_bitmap = placeholder_text
              .char_indices()
              .fold(0u128, |bitmap, (idx, _)| {
                bitmap | 1u128.unbounded_shl(idx as u32)
              });

            let fold_id = fold.id;
            new_transforms.push(
              Transform {
                summary: TransformSummary {
                  output: MBTextSummary::from(placeholder_text.as_ref()),
                  input: inlay_snapshot.text_summary_for_range(fold_range.start..fold_range.end),
                },
                placeholder: Some(TransformPlaceholder {
                  text: placeholder_text,
                  chars: chars_bitmap,
                  renderer: ChunkRenderer {
                    id: ChunkRendererId::Fold(fold.id),
                    render: Arc::new(move |cx| {
                      (fold.placeholder.render)(fold_id, fold.range.0.clone(), cx.context)
                    }),
                    constrain_width: fold.placeholder.constrain_width,
                    measured_width: self.snapshot.fold_width(&fold_id),
                  },
                }),
              },
              (),
            );
          }
        }

        let sum = new_transforms.summary();
        if sum.input.len < edit.new.end.0 {
          let text_summary =
            inlay_snapshot.text_summary_for_range(InlayOffset(sum.input.len)..edit.new.end);
          push_isomorphic(&mut new_transforms, text_summary);
        }
      }

      new_transforms.append(cursor.suffix(), ());
      if new_transforms.is_empty() {
        let text_summary = inlay_snapshot.text_summary();
        push_isomorphic(&mut new_transforms, text_summary);
      }

      drop(cursor);

      let mut fold_edits = Vec::with_capacity(inlay_edits.len());
      {
        let old_len = self.snapshot.inlay_snapshot.len();
        let new_len = inlay_snapshot.len();
        let mut old_transforms = self
          .snapshot
          .transforms
          .cursor::<Dimensions<InlayOffset, FoldOffset>>(());
        let mut new_transforms = new_transforms.cursor::<Dimensions<InlayOffset, FoldOffset>>(());

        let mut widened_edits = Vec::<InlayEdit>::with_capacity(inlay_edits.len());
        let mut inlay_edits_iter = inlay_edits.into_iter().peekable();
        while let Some(mut edit) = inlay_edits_iter.next() {
          let previous_edit = widened_edits.last();
          let mut merge_with_previous =
            previous_edit.is_some_and(|previous| previous.old.end >= edit.old.start);
          if !merge_with_previous {
            let mut limit = match previous_edit {
              Some(previous) => {
                (edit.old.start - previous.old.end).min(edit.new.start - previous.new.end)
              }
              None => edit.old.start.0.0.min(edit.new.start.0.0),
            };
            loop {
              old_transforms.seek(&edit.old.start, Bias::Left);
              new_transforms.seek(&edit.new.start, Bias::Left);
              let old_pull = if old_transforms.item().is_some_and(|t| t.is_fold()) {
                edit.old.start - old_transforms.start().0
              } else {
                0
              };
              let new_pull = if new_transforms.item().is_some_and(|t| t.is_fold()) {
                edit.new.start - new_transforms.start().0
              } else {
                0
              };
              let pull = old_pull.max(new_pull).min(limit);
              if pull == 0 {
                break;
              }
              edit.old.start -= pull;
              edit.new.start -= pull;
              limit -= pull;
            }
            merge_with_previous = previous_edit.is_some() && limit == 0;
          }

          let mut limit = match inlay_edits_iter.peek() {
            Some(next) => (next.old.start - edit.old.end).min(next.new.start - edit.new.end),
            None => (old_len - edit.old.end).min(new_len - edit.new.end),
          };
          loop {
            old_transforms.seek_forward(&edit.old.end, Bias::Right);
            new_transforms.seek_forward(&edit.new.end, Bias::Right);
            let old_extension = if old_transforms.item().is_some_and(|t| t.is_fold()) {
              old_transforms.end().0 - edit.old.end
            } else {
              0
            };
            let new_extension = if new_transforms.item().is_some_and(|t| t.is_fold()) {
              new_transforms.end().0 - edit.new.end
            } else {
              0
            };
            let extension = old_extension.max(new_extension).min(limit);
            if extension == 0 {
              break;
            }
            edit.old.end += extension;
            edit.new.end += extension;
            limit -= extension;
          }

          if merge_with_previous {
            if let Some(previous) = widened_edits.last_mut() {
              previous.old.end = edit.old.end;
              previous.new.end = edit.new.end;
            }
          } else {
            widened_edits.push(edit);
          }
        }

        for edit in widened_edits {
          old_transforms.seek(&edit.old.start, Bias::Left);
          let old_start = old_transforms.start().1.0 + (edit.old.start - old_transforms.start().0);
          old_transforms.seek_forward(&edit.old.end, Bias::Right);
          let old_end = old_transforms.start().1.0 + (edit.old.end - old_transforms.start().0);

          new_transforms.seek(&edit.new.start, Bias::Left);
          let new_start = new_transforms.start().1.0 + (edit.new.start - new_transforms.start().0);
          new_transforms.seek_forward(&edit.new.end, Bias::Right);
          let new_end = new_transforms.start().1.0 + (edit.new.end - new_transforms.start().0);

          fold_edits.push(FoldEdit {
            old: FoldOffset(old_start)..FoldOffset(old_end),
            new: FoldOffset(new_start)..FoldOffset(new_end),
          });
        }

        fold_edits = consolidate_fold_edits(fold_edits);
      }

      self.snapshot.transforms = new_transforms;
      self.snapshot.inlay_snapshot = inlay_snapshot;
      self.snapshot.version += 1;
      fold_edits
    }
  }
}

#[derive(Clone)]
pub struct FoldSnapshot {
  pub inlay_snapshot: InlaySnapshot,
  transforms: SumTree<Transform>,
  folds: SumTree<Fold>,
  fold_metadata_by_id: TreeMap<FoldId, FoldMetadata>,
  pub version: usize,
}

impl Deref for FoldSnapshot {
  type Target = InlaySnapshot;

  fn deref(&self) -> &Self::Target {
    &self.inlay_snapshot
  }
}

impl FoldSnapshot {
  pub fn buffer(&self) -> &MultiBufferSnapshot {
    &self.inlay_snapshot.buffer
  }

  pub(super) fn is_inlay_offset_folded(&self, offset: InlayOffset) -> bool {
    let (_, _, item) = self
      .transforms
      .find::<InlayOffset, _>((), &offset, Bias::Right);
    item.is_some_and(|transform| transform.placeholder.is_some())
  }

  #[ztracing::instrument(skip_all)]
  fn fold_width(&self, fold_id: &FoldId) -> Option<Pixels> {
    self.fold_metadata_by_id.get(fold_id)?.width
  }

  #[inline(always)]
  pub fn has_folds(&self) -> bool {
    !self.folds.is_empty()
  }

  #[ztracing::instrument(skip_all)]
  pub fn text_summary_for_range(&self, range: Range<FoldPoint>) -> MBTextSummary {
    let mut summary = MBTextSummary::default();

    let mut cursor = self
      .transforms
      .cursor::<Dimensions<FoldPoint, InlayPoint>>(());
    cursor.seek(&range.start, Bias::Right);
    if let Some(transform) = cursor.item() {
      let start_in_transform = range.start.0 - cursor.start().0.0;
      let end_in_transform = cmp::min(range.end, cursor.end().0).0 - cursor.start().0.0;
      if let Some(placeholder) = transform.placeholder.as_ref() {
        summary = MBTextSummary::from(
          &placeholder.text.as_ref()
            [start_in_transform.column as usize..end_in_transform.column as usize],
        );
      } else {
        let inlay_start = self
          .inlay_snapshot
          .to_offset(InlayPoint(cursor.start().1.0 + start_in_transform));
        let inlay_end = self
          .inlay_snapshot
          .to_offset(InlayPoint(cursor.start().1.0 + end_in_transform));
        summary = self
          .inlay_snapshot
          .text_summary_for_range(inlay_start..inlay_end);
      }
    }

    if range.end > cursor.end().0 {
      cursor.next();
      summary += cursor
        .summary::<_, TransformSummary>(&range.end, Bias::Right)
        .output;
      if let Some(transform) = cursor.item() {
        let end_in_transform = range.end.0 - cursor.start().0.0;
        if let Some(placeholder) = transform.placeholder.as_ref() {
          summary +=
            MBTextSummary::from(&placeholder.text.as_ref()[..end_in_transform.column as usize]);
        } else {
          let inlay_start = self.inlay_snapshot.to_offset(cursor.start().1);
          let inlay_end = self
            .inlay_snapshot
            .to_offset(InlayPoint(cursor.start().1.0 + end_in_transform));
          summary += self
            .inlay_snapshot
            .text_summary_for_range(inlay_start..inlay_end);
        }
      }
    }

    summary
  }

  #[ztracing::instrument(skip_all)]
  pub fn to_fold_point(&self, point: InlayPoint, bias: Bias) -> FoldPoint {
    let (start, end, item) = self
      .transforms
      .find::<Dimensions<InlayPoint, FoldPoint>, _>((), &point, Bias::Right);
    if item.is_some_and(|t| t.is_fold()) {
      if bias == Bias::Left || point == start.0 {
        start.1
      } else {
        end.1
      }
    } else {
      let overshoot = point.0 - start.0.0;
      FoldPoint(cmp::min(start.1.0 + overshoot, end.1.0))
    }
  }

  #[ztracing::instrument(skip_all)]
  pub fn fold_point_cursor(&self) -> FoldPointCursor<'_> {
    let cursor = self
      .transforms
      .cursor::<Dimensions<InlayPoint, FoldPoint>>(());
    FoldPointCursor { cursor }
  }

  #[ztracing::instrument(skip_all)]
  pub fn len(&self) -> FoldOffset {
    FoldOffset(self.transforms.summary().output.len)
  }

  #[ztracing::instrument(skip_all)]
  pub fn line_len(&self, row: u32) -> u32 {
    let line_start = FoldPoint::new(row, 0).to_offset(self).0;
    let line_end = if row >= self.max_point().row() {
      self.len().0
    } else {
      FoldPoint::new(row + 1, 0).to_offset(self).0 - 1
    };
    (line_end - line_start) as u32
  }

  #[ztracing::instrument(skip_all)]
  pub fn row_infos(&self, start_row: u32) -> FoldRows<'_> {
    if start_row > self.transforms.summary().output.lines.row {
      panic!("invalid display row {}", start_row);
    }

    let fold_point = FoldPoint::new(start_row, 0);
    let mut cursor = self
      .transforms
      .cursor::<Dimensions<FoldPoint, InlayPoint>>(());
    cursor.seek(&fold_point, Bias::Left);

    let overshoot = fold_point.0 - cursor.start().0.0;
    let inlay_point = InlayPoint(cursor.start().1.0 + overshoot);
    let input_rows = self.inlay_snapshot.row_infos(inlay_point.row());

    FoldRows {
      fold_point,
      input_rows,
      cursor,
    }
  }

  #[ztracing::instrument(skip_all)]
  pub fn max_point(&self) -> FoldPoint {
    FoldPoint(self.transforms.summary().output.lines)
  }

  #[ztracing::instrument(skip_all)]
  pub fn folds_in_range<T>(&self, range: Range<T>) -> impl Iterator<Item = &Fold>
  where
    T: ToOffset,
  {
    let buffer = &self.inlay_snapshot.buffer;
    let range = range.start.to_offset(buffer)..range.end.to_offset(buffer);
    let mut folds = intersecting_folds(&self.inlay_snapshot, &self.folds, range, false);
    iter::from_fn(move || {
      let item = folds.item();
      folds.next();
      item
    })
  }

  #[ztracing::instrument(skip_all)]
  pub fn intersects_fold<T>(&self, offset: T) -> bool
  where
    T: ToOffset,
  {
    let buffer_offset = offset.to_offset(&self.inlay_snapshot.buffer);
    let inlay_offset = self.inlay_snapshot.to_inlay_offset(buffer_offset);
    self.is_inlay_offset_folded(inlay_offset)
  }

  #[ztracing::instrument(skip_all)]
  pub fn is_line_folded(&self, buffer_row: MultiBufferRow) -> bool {
    let mut inlay_point = self
      .inlay_snapshot
      .to_inlay_point(Point::new(buffer_row.0, 0));
    let mut cursor = self.transforms.cursor::<InlayPoint>(());
    cursor.seek(&inlay_point, Bias::Right);
    loop {
      match cursor.item() {
        Some(transform) => {
          let buffer_point = self.inlay_snapshot.to_buffer_point(inlay_point);
          if buffer_point.row != buffer_row.0 {
            return false;
          } else if transform.placeholder.is_some() {
            return true;
          }
        }
        None => return false,
      }

      if cursor.end().row() == inlay_point.row() {
        cursor.next();
      } else {
        inlay_point.0 += Point::new(1, 0);
        cursor.seek(&inlay_point, Bias::Right);
      }
    }
  }

  pub(crate) fn placeholder_range_at(&self, point: FoldPoint) -> Option<Range<FoldPoint>> {
    let (start, end, item) = self
      .transforms
      .find::<FoldPoint, _>((), &point, Bias::Right);
    item
      .filter(|transform| transform.placeholder.is_some())
      .map(|_| start..end)
  }

  #[ztracing::instrument(skip_all)]
  pub(crate) fn chunks<'a>(
    &'a self,
    range: Range<FoldOffset>,
    language_aware: LanguageAwareStyling,
    highlights: Highlights<'a>,
  ) -> FoldChunks<'a> {
    let mut transform_cursor = self
      .transforms
      .cursor::<Dimensions<FoldOffset, InlayOffset>>(());
    transform_cursor.seek(&range.start, Bias::Right);

    let inlay_start = {
      let overshoot = range.start - transform_cursor.start().0;
      transform_cursor.start().1 + overshoot
    };

    let transform_end = transform_cursor.end();

    let inlay_end = if transform_cursor
      .item()
      .is_none_or(|transform| transform.is_fold())
    {
      inlay_start
    } else if range.end < transform_end.0 {
      let overshoot = range.end - transform_cursor.start().0;
      transform_cursor.start().1 + overshoot
    } else {
      transform_end.1
    };

    FoldChunks {
      transform_cursor,
      inlay_chunks: self
        .inlay_snapshot
        .chunks(inlay_start..inlay_end, language_aware, highlights),
      inlay_chunk: None,
      inlay_offset: inlay_start,
      output_offset: range.start,
      max_output_offset: range.end,
    }
  }

  #[ztracing::instrument(skip_all)]
  pub fn chars_at(&self, start: FoldPoint) -> impl '_ + Iterator<Item = char> {
    self
      .chunks(
        start.to_offset(self)..self.len(),
        LanguageAwareStyling {
          tree_sitter: false,
          diagnostics: false,
        },
        Highlights::default(),
      )
      .flat_map(|chunk| chunk.text.chars())
  }

  #[ztracing::instrument(skip_all)]
  pub fn chunks_at(&self, start: FoldPoint) -> FoldChunks<'_> {
    self.chunks(
      start.to_offset(self)..self.len(),
      LanguageAwareStyling {
        tree_sitter: false,
        diagnostics: false,
      },
      Highlights::default(),
    )
  }

  #[ztracing::instrument(skip_all)]
  pub fn clip_point(&self, point: FoldPoint, bias: Bias) -> FoldPoint {
    let (start, end, item) = self
      .transforms
      .find::<Dimensions<FoldPoint, InlayPoint>, _>((), &point, Bias::Right);
    if let Some(transform) = item {
      let transform_start = start.0.0;
      if transform.placeholder.is_some() {
        if point.0 == transform_start || matches!(bias, Bias::Left) {
          FoldPoint(transform_start)
        } else {
          FoldPoint(end.0.0)
        }
      } else {
        let overshoot = InlayPoint(point.0 - transform_start);
        let inlay_point = start.1 + overshoot;
        let clipped_inlay_point = self.inlay_snapshot.clip_point(inlay_point, bias);
        FoldPoint(start.0.0 + (clipped_inlay_point - start.1).0)
      }
    } else {
      FoldPoint(self.transforms.summary().output.lines)
    }
  }
}

pub struct FoldPointCursor<'transforms> {
  cursor: Cursor<'transforms, 'static, Transform, Dimensions<InlayPoint, FoldPoint>>,
}

impl FoldPointCursor<'_> {
  /// Resets the cursor to the start so it can seek backward again.
  pub fn reset(&mut self) {
    self.cursor.reset();
  }

  #[ztracing::instrument(skip_all)]
  pub fn map(&mut self, point: InlayPoint, bias: Bias) -> FoldPoint {
    let cursor = &mut self.cursor;
    if cursor.did_seek() {
      cursor.seek_forward(&point, Bias::Right);
    } else {
      cursor.seek(&point, Bias::Right);
    }
    if cursor.item().is_some_and(|t| t.is_fold()) {
      if bias == Bias::Left || point == cursor.start().0 {
        cursor.start().1
      } else {
        cursor.end().1
      }
    } else {
      let overshoot = point.0 - cursor.start().0.0;
      FoldPoint(cmp::min(cursor.start().1.0 + overshoot, cursor.end().1.0))
    }
  }
}

fn push_isomorphic(transforms: &mut SumTree<Transform>, summary: MBTextSummary) {
  let mut did_merge = false;
  transforms.update_last(
    |last| {
      if !last.is_fold() {
        last.summary.input += summary;
        last.summary.output += summary;
        did_merge = true;
      }
    },
    (),
  );
  if !did_merge {
    transforms.push(
      Transform {
        summary: TransformSummary {
          input: summary,
          output: summary,
        },
        placeholder: None,
      },
      (),
    )
  }
}

fn intersecting_folds<'a>(
  inlay_snapshot: &'a InlaySnapshot,
  folds: &'a SumTree<Fold>,
  range: Range<MultiBufferOffset>,
  inclusive: bool,
) -> FilterCursor<'a, 'a, impl 'a + FnMut(&FoldSummary) -> bool, Fold, MultiBufferOffset> {
  let buffer = &inlay_snapshot.buffer;
  let start = buffer.anchor_before(range.start.to_offset(buffer));
  let end = buffer.anchor_after(range.end.to_offset(buffer));
  let mut cursor = folds.filter::<_, MultiBufferOffset>(buffer, move |summary| {
    let start_cmp = start.cmp(&summary.max_end, buffer);
    let end_cmp = end.cmp(&summary.min_start, buffer);

    if inclusive {
      start_cmp <= Ordering::Equal && end_cmp >= Ordering::Equal
    } else {
      start_cmp == Ordering::Less && end_cmp == Ordering::Greater
    }
  });
  cursor.next();
  cursor
}

fn consolidate_inlay_edits(mut edits: Vec<InlayEdit>) -> Vec<InlayEdit> {
  edits.sort_unstable_by(|a, b| {
    a.old
      .start
      .cmp(&b.old.start)
      .then_with(|| b.old.end.cmp(&a.old.end))
  });

  let _old_alloc_ptr = edits.as_ptr();
  let mut inlay_edits = edits.into_iter();

  if let Some(mut first_edit) = inlay_edits.next() {
    // This code relies on reusing allocations from the Vec<_> - at the time of writing .flatten() prevents them.
    #[allow(clippy::filter_map_identity)]
    let mut v: Vec<_> = inlay_edits
      .scan(&mut first_edit, |prev_edit, edit| {
        if prev_edit.old.end >= edit.old.start {
          prev_edit.old.end = prev_edit.old.end.max(edit.old.end);
          prev_edit.new.start = prev_edit.new.start.min(edit.new.start);
          prev_edit.new.end = prev_edit.new.end.max(edit.new.end);
          Some(None) // Skip this edit, it's merged
        } else {
          let prev = std::mem::replace(*prev_edit, edit);
          Some(Some(prev)) // Yield the previous edit
        }
      })
      .filter_map(|x| x)
      .collect();
    v.push(first_edit.clone());
    debug_assert_eq!(_old_alloc_ptr, v.as_ptr(), "Inlay edits were reallocated");
    v
  } else {
    vec![]
  }
}

fn consolidate_fold_edits(mut edits: Vec<FoldEdit>) -> Vec<FoldEdit> {
  edits.sort_unstable_by(|a, b| {
    a.old
      .start
      .cmp(&b.old.start)
      .then_with(|| b.old.end.cmp(&a.old.end))
  });
  let _old_alloc_ptr = edits.as_ptr();
  let mut fold_edits = edits.into_iter();

  if let Some(mut first_edit) = fold_edits.next() {
    // This code relies on reusing allocations from the Vec<_> - at the time of writing .flatten() prevents them.
    #[allow(clippy::filter_map_identity)]
    let mut v: Vec<_> = fold_edits
      .scan(&mut first_edit, |prev_edit, edit| {
        if prev_edit.old.end >= edit.old.start {
          prev_edit.old.end = prev_edit.old.end.max(edit.old.end);
          prev_edit.new.start = prev_edit.new.start.min(edit.new.start);
          prev_edit.new.end = prev_edit.new.end.max(edit.new.end);
          Some(None) // Skip this edit, it's merged
        } else {
          let prev = std::mem::replace(*prev_edit, edit);
          Some(Some(prev)) // Yield the previous edit
        }
      })
      .filter_map(|x| x)
      .collect();
    v.push(first_edit.clone());
    v
  } else {
    vec![]
  }
}

#[derive(Clone, Debug, Default)]
struct Transform {
  summary: TransformSummary,
  placeholder: Option<TransformPlaceholder>,
}

#[derive(Clone, Debug)]
struct TransformPlaceholder {
  text: SharedString,
  chars: u128,
  renderer: ChunkRenderer,
}

impl Transform {
  fn is_fold(&self) -> bool {
    self.placeholder.is_some()
  }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TransformSummary {
  output: MBTextSummary,
  input: MBTextSummary,
}

impl sum_tree::Item for Transform {
  type Summary = TransformSummary;

  fn summary(&self, _cx: ()) -> Self::Summary {
    self.summary.clone()
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

#[derive(Copy, Clone, Eq, PartialEq, Debug, Default, Ord, PartialOrd, Hash)]
pub struct FoldId(pub(super) usize);

impl From<FoldId> for ElementId {
  fn from(val: FoldId) -> Self {
    val.0.into()
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fold {
  pub id: FoldId,
  pub range: FoldRange,
  pub placeholder: FoldPlaceholder,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FoldRange(pub(crate) Range<Anchor>);

impl Deref for FoldRange {
  type Target = Range<Anchor>;

  fn deref(&self) -> &Self::Target {
    &self.0
  }
}

impl DerefMut for FoldRange {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut self.0
  }
}

impl Default for FoldRange {
  fn default() -> Self {
    Self(Anchor::Min..Anchor::Max)
  }
}

#[derive(Clone, Debug)]
struct FoldMetadata {
  range: FoldRange,
  width: Option<Pixels>,
}

impl sum_tree::Item for Fold {
  type Summary = FoldSummary;

  fn summary(&self, _cx: &MultiBufferSnapshot) -> Self::Summary {
    FoldSummary {
      start: self.range.start,
      end: self.range.end,
      min_start: self.range.start,
      max_end: self.range.end,
      count: 1,
    }
  }
}

#[derive(Clone, Debug)]
pub struct FoldSummary {
  start: Anchor,
  end: Anchor,
  min_start: Anchor,
  max_end: Anchor,
  count: usize,
}

impl Default for FoldSummary {
  fn default() -> Self {
    Self {
      start: Anchor::Min,
      end: Anchor::Max,
      min_start: Anchor::Max,
      max_end: Anchor::Min,
      count: 0,
    }
  }
}

impl sum_tree::Summary for FoldSummary {
  type Context<'a> = &'a MultiBufferSnapshot;

  fn zero(_cx: &MultiBufferSnapshot) -> Self {
    Default::default()
  }

  fn add_summary(&mut self, other: &Self, buffer: Self::Context<'_>) {
    if other.min_start.cmp(&self.min_start, buffer) == Ordering::Less {
      self.min_start = other.min_start;
    }
    if other.max_end.cmp(&self.max_end, buffer) == Ordering::Greater {
      self.max_end = other.max_end;
    }

    #[cfg(debug_assertions)]
    {
      let start_comparison = self.start.cmp(&other.start, buffer);
      assert!(start_comparison <= Ordering::Equal);
      if start_comparison == Ordering::Equal {
        assert!(self.end.cmp(&other.end, buffer) >= Ordering::Equal);
      }
    }

    self.start = other.start;
    self.end = other.end;
    self.count += other.count;
  }
}

impl<'a> sum_tree::Dimension<'a, FoldSummary> for FoldRange {
  fn zero(_cx: &MultiBufferSnapshot) -> Self {
    Default::default()
  }

  fn add_summary(&mut self, summary: &'a FoldSummary, _: &MultiBufferSnapshot) {
    self.0.start = summary.start;
    self.0.end = summary.end;
  }
}

impl sum_tree::SeekTarget<'_, FoldSummary, FoldRange> for FoldRange {
  fn cmp(&self, other: &Self, buffer: &MultiBufferSnapshot) -> Ordering {
    AnchorRangeExt::cmp(&self.0, &other.0, buffer)
  }
}

impl sum_tree::SeekTarget<'_, FoldSummary, FoldRange> for MultiBufferOffset {
  fn cmp(&self, cursor_location: &FoldRange, buffer: &MultiBufferSnapshot) -> Ordering {
    Ord::cmp(self, &cursor_location.start.to_offset(buffer))
  }
}

impl<'a> sum_tree::Dimension<'a, FoldSummary> for MultiBufferOffset {
  fn zero(_cx: &MultiBufferSnapshot) -> Self {
    Default::default()
  }

  fn add_summary(&mut self, summary: &'a FoldSummary, _: &MultiBufferSnapshot) {
    *self += summary.count;
  }
}

#[derive(Clone)]
pub struct FoldRows<'a> {
  cursor: Cursor<'a, 'static, Transform, Dimensions<FoldPoint, InlayPoint>>,
  input_rows: InlayBufferRows<'a>,
  fold_point: FoldPoint,
}

impl FoldRows<'_> {
  #[ztracing::instrument(skip_all)]
  pub(crate) fn seek(&mut self, row: u32) {
    let fold_point = FoldPoint::new(row, 0);
    self.cursor.seek(&fold_point, Bias::Left);
    let overshoot = fold_point.0 - self.cursor.start().0.0;
    let inlay_point = InlayPoint(self.cursor.start().1.0 + overshoot);
    self.input_rows.seek(inlay_point.row());
    self.fold_point = fold_point;
  }
}

impl Iterator for FoldRows<'_> {
  type Item = RowInfo;

  #[ztracing::instrument(skip_all)]
  fn next(&mut self) -> Option<Self::Item> {
    let mut traversed_fold = false;
    while self.fold_point > self.cursor.end().0 {
      self.cursor.next();
      traversed_fold = true;
      if self.cursor.item().is_none() {
        break;
      }
    }

    if self.cursor.item().is_some() {
      if traversed_fold {
        self.input_rows.seek(self.cursor.start().1.0.row);
        self.input_rows.next();
      }
      *self.fold_point.row_mut() += 1;
      self.input_rows.next()
    } else {
      None
    }
  }
}

/// A chunk of a buffer's text, along with its syntax highlight and
/// diagnostic status.
#[derive(Clone, Debug, Default)]
pub struct Chunk<'a> {
  /// The text of the chunk.
  pub text: &'a str,
  /// The syntax highlighting style of the chunk.
  pub syntax_highlight_id: Option<HighlightId>,
  /// The highlight style that has been applied to this chunk in
  /// the editor.
  pub highlight_style: Option<HighlightStyle>,
  /// The severity of diagnostic associated with this chunk, if any.
  pub diagnostic_severity: Option<lsp::DiagnosticSeverity>,
  /// Whether this chunk of text is marked as unnecessary.
  pub is_unnecessary: bool,
  /// Whether this chunk of text should be underlined.
  pub underline: bool,
  /// Whether this chunk of text was originally a tab character.
  pub is_tab: bool,
  /// Whether this chunk of text was originally a tab character.
  pub is_inlay: bool,
  /// An optional recipe for how the chunk should be presented.
  pub renderer: Option<ChunkRenderer>,
  /// Bitmap of tab character locations in chunk
  pub tabs: u128,
  /// Bitmap of character locations in chunk
  pub chars: u128,
  /// Bitmap of newline locations in chunk
  pub newlines: u128,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChunkRendererId {
  Fold(FoldId),
  Inlay(InlayId),
}

/// A recipe for how the chunk should be presented.
#[derive(Clone)]
pub struct ChunkRenderer {
  /// The id of the renderer associated with this chunk.
  pub id: ChunkRendererId,
  /// Creates a custom element to represent this chunk.
  pub render: Arc<dyn Send + Sync + Fn(&mut ChunkRendererContext) -> AnyElement>,
  /// If true, the element is constrained to the shaped width of the text.
  pub constrain_width: bool,
  /// The width of the element, as measured during the last layout pass.
  ///
  /// This is None if the element has not been laid out yet.
  pub measured_width: Option<Pixels>,
}

pub struct ChunkRendererContext<'a, 'b> {
  pub window: &'a mut Window,
  pub context: &'b mut App,
  pub max_width: Pixels,
}

impl fmt::Debug for ChunkRenderer {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    f.debug_struct("ChunkRenderer")
      .field("constrain_width", &self.constrain_width)
      .finish()
  }
}

impl Deref for ChunkRendererContext<'_, '_> {
  type Target = App;

  fn deref(&self) -> &Self::Target {
    self.context
  }
}

impl DerefMut for ChunkRendererContext<'_, '_> {
  fn deref_mut(&mut self) -> &mut Self::Target {
    self.context
  }
}

pub struct FoldChunks<'a> {
  transform_cursor: Cursor<'a, 'static, Transform, Dimensions<FoldOffset, InlayOffset>>,
  inlay_chunks: InlayChunks<'a>,
  inlay_chunk: Option<(InlayOffset, InlayChunk<'a>)>,
  inlay_offset: InlayOffset,
  output_offset: FoldOffset,
  max_output_offset: FoldOffset,
}

impl FoldChunks<'_> {
  #[ztracing::instrument(skip_all)]
  pub(crate) fn seek(&mut self, range: Range<FoldOffset>) {
    self.transform_cursor.seek(&range.start, Bias::Right);

    let inlay_start = {
      let overshoot = range.start - self.transform_cursor.start().0;
      self.transform_cursor.start().1 + overshoot
    };

    let transform_end = self.transform_cursor.end();

    let inlay_end = if self
      .transform_cursor
      .item()
      .is_none_or(|transform| transform.is_fold())
    {
      inlay_start
    } else if range.end < transform_end.0 {
      let overshoot = range.end - self.transform_cursor.start().0;
      self.transform_cursor.start().1 + overshoot
    } else {
      transform_end.1
    };

    self.inlay_chunks.seek(inlay_start..inlay_end);
    self.inlay_chunk = None;
    self.inlay_offset = inlay_start;
    self.output_offset = range.start;
    self.max_output_offset = range.end;
  }
}

impl<'a> Iterator for FoldChunks<'a> {
  type Item = Chunk<'a>;

  #[ztracing::instrument(skip_all)]
  fn next(&mut self) -> Option<Self::Item> {
    if self.output_offset >= self.max_output_offset {
      return None;
    }

    let transform = self.transform_cursor.item()?;

    // If we're in a fold, then return the fold's display text and
    // advance the transform and buffer cursors to the end of the fold.
    if let Some(placeholder) = transform.placeholder.as_ref() {
      self.inlay_chunk.take();
      self.inlay_offset += InlayOffset(transform.summary.input.len);

      while self.inlay_offset >= self.transform_cursor.end().1
        && self.transform_cursor.item().is_some()
      {
        self.transform_cursor.next();
      }

      self.output_offset.0 += placeholder.text.len();
      return Some(Chunk {
        text: &placeholder.text,
        chars: placeholder.chars,
        renderer: Some(placeholder.renderer.clone()),
        ..Default::default()
      });
    }

    // When we reach a non-fold region, seek the underlying text
    // chunk iterator to the next unfolded range.
    if self.inlay_offset == self.transform_cursor.start().1
      && self.inlay_chunks.offset() != self.inlay_offset
    {
      let transform_start = self.transform_cursor.start();
      let transform_end = self.transform_cursor.end();
      let inlay_end = if self.max_output_offset < transform_end.0 {
        let overshoot = self.max_output_offset - transform_start.0;
        transform_start.1 + overshoot
      } else {
        transform_end.1
      };

      self.inlay_chunks.seek(self.inlay_offset..inlay_end);
    }

    // Retrieve a chunk from the current location in the buffer.
    if self.inlay_chunk.is_none() {
      let chunk_offset = self.inlay_chunks.offset();
      self.inlay_chunk = self.inlay_chunks.next().map(|chunk| (chunk_offset, chunk));
    }

    // Otherwise, take a chunk from the buffer's text.
    if let Some((buffer_chunk_start, mut inlay_chunk)) = self.inlay_chunk.clone() {
      let chunk = &mut inlay_chunk.chunk;
      let buffer_chunk_end = buffer_chunk_start + chunk.text.len();
      let transform_end = self.transform_cursor.end().1;
      let chunk_end = buffer_chunk_end.min(transform_end);

      let bit_start = self.inlay_offset - buffer_chunk_start;
      let bit_end = chunk_end - buffer_chunk_start;
      chunk.text = &chunk.text[bit_start..bit_end];

      let bit_end = chunk_end - buffer_chunk_start;
      let mask = 1u128.unbounded_shl(bit_end as u32).wrapping_sub(1);

      chunk.tabs = (chunk.tabs >> bit_start) & mask;
      chunk.chars = (chunk.chars >> bit_start) & mask;
      chunk.newlines = (chunk.newlines >> bit_start) & mask;

      if chunk_end == transform_end {
        self.transform_cursor.next();
      } else if chunk_end == buffer_chunk_end {
        self.inlay_chunk.take();
      }

      self.inlay_offset = chunk_end;
      self.output_offset.0 += chunk.text.len();
      return Some(Chunk {
        text: chunk.text,
        tabs: chunk.tabs,
        chars: chunk.chars,
        newlines: chunk.newlines,
        syntax_highlight_id: chunk.syntax_highlight_id,
        highlight_style: chunk.highlight_style,
        diagnostic_severity: chunk.diagnostic_severity,
        is_unnecessary: chunk.is_unnecessary,
        is_tab: chunk.is_tab,
        is_inlay: chunk.is_inlay,
        underline: chunk.underline,
        renderer: inlay_chunk.renderer,
      });
    }

    None
  }
}

#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq)]
pub struct FoldOffset(pub MultiBufferOffset);

impl FoldOffset {
  #[ztracing::instrument(skip_all)]
  pub fn to_point(self, snapshot: &FoldSnapshot) -> FoldPoint {
    let (start, _, item) = snapshot
      .transforms
      .find::<Dimensions<FoldOffset, TransformSummary>, _>((), &self, Bias::Right);
    let overshoot = if item.is_none_or(|t| t.is_fold()) {
      Point::new(0, (self.0 - start.0.0) as u32)
    } else {
      let inlay_offset = start.1.input.len + (self - start.0);
      let inlay_point = snapshot.inlay_snapshot.to_point(InlayOffset(inlay_offset));
      inlay_point.0 - start.1.input.lines
    };
    FoldPoint(start.1.output.lines + overshoot)
  }
}

impl Add for FoldOffset {
  type Output = Self;

  fn add(self, rhs: Self) -> Self::Output {
    Self(self.0 + rhs.0)
  }
}

impl Sub for FoldOffset {
  type Output = <MultiBufferOffset as Sub>::Output;

  fn sub(self, rhs: Self) -> Self::Output {
    self.0 - rhs.0
  }
}

impl<T> SubAssign<T> for FoldOffset
where
  MultiBufferOffset: SubAssign<T>,
{
  fn sub_assign(&mut self, rhs: T) {
    self.0 -= rhs;
  }
}

impl<T> Add<T> for FoldOffset
where
  MultiBufferOffset: Add<T, Output = MultiBufferOffset>,
{
  type Output = Self;

  fn add(self, rhs: T) -> Self::Output {
    Self(self.0 + rhs)
  }
}

impl AddAssign for FoldOffset {
  fn add_assign(&mut self, rhs: Self) {
    self.0 += rhs.0;
  }
}

impl<T> AddAssign<T> for FoldOffset
where
  MultiBufferOffset: AddAssign<T>,
{
  fn add_assign(&mut self, rhs: T) {
    self.0 += rhs;
  }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for FoldOffset {
  fn zero(_cx: ()) -> Self {
    Default::default()
  }

  fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
    self.0 += summary.output.len;
  }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for InlayPoint {
  fn zero(_cx: ()) -> Self {
    Default::default()
  }

  fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
    self.0 += &summary.input.lines;
  }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for InlayOffset {
  fn zero(_cx: ()) -> Self {
    Default::default()
  }

  fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
    self.0 += summary.input.len;
  }
}

pub type FoldEdit = Edit<FoldOffset>;
