use super::{
  Highlights,
  dimensions::RowDelta,
  fold_map::{Chunk, FoldRows},
  invisibles::{is_invisible, is_standalone_grapheme, replacement},
  tab_map::{self, TabEdit, TabPoint, TabSnapshot},
};

use collections::HashMap;
use futures_lite::future::yield_now;
use gpui::{
  App, AppContext as _, Context, Entity, Font, FontId, IndentAdjustment, LineWrapper, Pixels, Task,
  TextSystem,
};
use language::{LanguageAwareStyling, Point, language_settings::SoftWrapIndent};
use multi_buffer::RowInfo;
use std::{
  cmp,
  collections::VecDeque,
  mem,
  num::NonZeroU32,
  ops::Range,
  sync::{Arc, LazyLock},
  time::Duration,
};
use sum_tree::{Bias, Cursor, Dimensions, SumTree};
use text::Patch;

pub use super::tab_map::TextSummary;
pub type WrapEdit = text::Edit<WrapRow>;
pub type WrapPatch = text::Patch<WrapRow>;

#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq)]
pub struct WrapRow(pub u32);

const WRAP_YIELD_ROW_INTERVAL: usize = 100;

impl_for_row_types! {
    WrapRow => RowDelta
}

/// Handles soft wrapping of text.
///
/// See the [`display_map` module documentation](crate::display_map) for more information.
pub struct WrapMap {
  snapshot: WrapSnapshot,
  pending_edits: VecDeque<(TabSnapshot, Vec<TabEdit>)>,
  interpolated_edits: WrapPatch,
  edits_since_sync: WrapPatch,
  wrap_width: Option<Pixels>,
  soft_wrap_indent: SoftWrapIndent,
  background_task: Option<Task<()>>,
  font_with_size: (Font, Pixels),
}

#[derive(Clone)]
pub struct WrapSnapshot {
  pub(super) tab_snapshot: TabSnapshot,
  transforms: SumTree<Transform>,
  interpolated: bool,
}

impl std::ops::Deref for WrapSnapshot {
  type Target = TabSnapshot;

  fn deref(&self) -> &Self::Target {
    &self.tab_snapshot
  }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Transform {
  summary: TransformSummary,
  display_text: Option<&'static str>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TransformSummary {
  input: TextSummary,
  output: TextSummary,
}

impl TransformSummary {
  fn has_wraps(&self) -> bool {
    self.input.lines != self.output.lines
  }
}

#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq)]
pub struct WrapPoint(pub Point);

struct LineFragmentBuilder {
  text_system: Arc<TextSystem>,
  font_id: FontId,
  font_size: Pixels,
  cached_replacement_widths: HashMap<char, Pixels>,
}

impl LineFragmentBuilder {
  fn new(text_system: Arc<TextSystem>, font: &Font, font_size: Pixels) -> Self {
    let font_id = text_system.resolve_font(font);
    Self {
      text_system,
      font_id,
      font_size,
      cached_replacement_widths: HashMap::default(),
    }
  }

  fn push_fragments<'a>(&mut self, fragments: &mut Vec<gpui::LineFragment<'a>>, text: &'a str) {
    let mut prefix_start = 0;
    for (offset, ch) in text.char_indices() {
      if !is_invisible(ch) {
        continue;
      }
      let ch_end = offset + ch.len_utf8();
      if !is_standalone_grapheme(text, offset, ch_end) {
        continue;
      }
      let Some(width) = self.replacement_width(ch) else {
        continue;
      };
      if prefix_start < offset {
        fragments.push(gpui::LineFragment::text(&text[prefix_start..offset]));
      }
      fragments.push(gpui::LineFragment::element(width, ch_end - offset));
      prefix_start = ch_end;
    }
    if prefix_start < text.len() || text.is_empty() {
      fragments.push(gpui::LineFragment::text(&text[prefix_start..]));
    }
  }

  fn replacement_width(&mut self, ch: char) -> Option<Pixels> {
    let replacement_char = replacement(ch)?;
    let width = *self
      .cached_replacement_widths
      .entry(replacement_char)
      .or_insert_with(|| {
        self
          .text_system
          .layout_width(self.font_id, self.font_size, replacement_char)
      });
    Some(width)
  }
}

pub struct WrapChunks<'a> {
  input_chunks: tab_map::TabChunks<'a>,
  input_chunk: Chunk<'a>,
  output_position: WrapPoint,
  max_output_row: WrapRow,
  transforms: Cursor<'a, 'static, Transform, Dimensions<WrapPoint, TabPoint>>,
  snapshot: &'a WrapSnapshot,
}

#[derive(Clone)]
pub struct WrapRows<'a> {
  input_buffer_rows: FoldRows<'a>,
  input_buffer_row: RowInfo,
  output_row: WrapRow,
  soft_wrapped: bool,
  max_output_row: WrapRow,
  transforms: Cursor<'a, 'static, Transform, Dimensions<WrapPoint, TabPoint>>,
}

impl WrapRows<'_> {
  #[ztracing::instrument(skip_all)]
  pub(crate) fn seek(&mut self, start_row: WrapRow) {
    self
      .transforms
      .seek(&WrapPoint::new(start_row, 0), Bias::Left);
    let mut input_row = self.transforms.start().1.row();
    if self.transforms.item().is_some_and(|t| t.is_isomorphic()) {
      input_row += (start_row - self.transforms.start().0.row()).0;
    }
    self.soft_wrapped = self.transforms.item().is_some_and(|t| !t.is_isomorphic());
    self.input_buffer_rows.seek(input_row);
    self.input_buffer_row = self.input_buffer_rows.next().unwrap();
    self.output_row = start_row;
  }
}

impl WrapMap {
  #[ztracing::instrument(skip_all)]
  pub fn new(
    tab_snapshot: TabSnapshot,
    font: Font,
    font_size: Pixels,
    wrap_width: Option<Pixels>,
    cx: &mut App,
  ) -> (Entity<Self>, WrapSnapshot) {
    let handle = cx.new(|cx| {
      let mut this = Self {
        font_with_size: (font, font_size),
        wrap_width: None,
        soft_wrap_indent: SoftWrapIndent::default(),
        pending_edits: Default::default(),
        interpolated_edits: Default::default(),
        edits_since_sync: Default::default(),
        snapshot: WrapSnapshot::new(tab_snapshot),
        background_task: None,
      };
      this.set_wrap_width(wrap_width, cx);
      mem::take(&mut this.edits_since_sync);
      this
    });
    let snapshot = handle.read(cx).snapshot.clone();
    (handle, snapshot)
  }

  pub fn is_rewrapping(&self) -> bool {
    self.background_task.is_some() || (self.wrap_width.is_some() && self.snapshot.interpolated)
  }

  #[ztracing::instrument(skip_all)]
  pub fn sync(
    &mut self,
    tab_snapshot: TabSnapshot,
    edits: Vec<TabEdit>,
    cx: &mut Context<Self>,
  ) -> (WrapSnapshot, WrapPatch) {
    if self.wrap_width.is_some() {
      self.pending_edits.push_back((tab_snapshot, edits));
      self.flush_edits(cx);
    } else {
      self.edits_since_sync = self
        .edits_since_sync
        .compose(self.snapshot.interpolate(tab_snapshot, &edits));
      self.snapshot.interpolated = false;
    }

    debug_assert!(
      self.background_task.is_some() || self.wrap_width.is_none() || !self.snapshot.interpolated,
      "an interpolated snapshot must always have a background task rewrapping it, \
             otherwise is_rewrapping never settles and frozen scrollbar ranges leak"
    );
    (self.snapshot.clone(), mem::take(&mut self.edits_since_sync))
  }

  #[ztracing::instrument(skip_all)]
  pub fn set_font_with_size(
    &mut self,
    font: Font,
    font_size: Pixels,
    cx: &mut Context<Self>,
  ) -> bool {
    let font_with_size = (font, font_size);

    if font_with_size == self.font_with_size {
      false
    } else {
      self.font_with_size = font_with_size;
      self.rewrap(cx);
      true
    }
  }

  #[ztracing::instrument(skip_all)]
  pub fn set_wrap_width(&mut self, wrap_width: Option<Pixels>, cx: &mut Context<Self>) -> bool {
    if wrap_width == self.wrap_width {
      return false;
    }

    self.wrap_width = wrap_width;
    self.rewrap(cx);
    true
  }

  #[ztracing::instrument(skip_all)]
  pub fn set_soft_wrap_indent(&mut self, indent: SoftWrapIndent, cx: &mut Context<Self>) -> bool {
    if indent == self.soft_wrap_indent {
      return false;
    }

    self.soft_wrap_indent = indent;
    self.rewrap(cx);
    true
  }

  fn indent_adjustment(&self, tab_size: NonZeroU32) -> IndentAdjustment {
    indent_adjustment_for(self.soft_wrap_indent, tab_size)
  }

  #[ztracing::instrument(skip_all)]
  fn rewrap(&mut self, cx: &mut Context<Self>) {
    self.background_task.take();
    self.interpolated_edits.clear();
    self.pending_edits.clear();

    if let Some(wrap_width) = self.wrap_width {
      let mut new_snapshot = self.snapshot.clone();

      let text_system = cx.text_system();
      let (font, font_size) = self.font_with_size.clone();
      let mut fragment_builder = LineFragmentBuilder::new(text_system.clone(), &font, font_size);
      let mut line_wrapper = text_system.line_wrapper(font, font_size);
      let tab_snapshot = new_snapshot.tab_snapshot.clone();
      let total_rows = tab_snapshot.max_point().row() as usize + 1;
      let range = TabPoint::zero()..tab_snapshot.max_point();
      let tab_edits = [TabEdit {
        old: range.clone(),
        new: range,
      }];

      if total_rows < WRAP_YIELD_ROW_INTERVAL {
        let edits = gpui::block_on(new_snapshot.update(
          tab_snapshot.clone(),
          &tab_edits,
          wrap_width,
          self.indent_adjustment(tab_snapshot.tab_size),
          &mut line_wrapper,
          &mut fragment_builder,
        ));
        self.snapshot = new_snapshot;
        self.edits_since_sync = self.edits_since_sync.compose(&edits);
      } else {
        let indent_adjustment = self.indent_adjustment(tab_snapshot.tab_size);
        let task = cx.background_spawn(async move {
          let edits = new_snapshot
            .update(
              tab_snapshot,
              &tab_edits,
              wrap_width,
              indent_adjustment,
              &mut line_wrapper,
              &mut fragment_builder,
            )
            .await;
          (new_snapshot, edits)
        });

        match cx
          .foreground_executor()
          .block_with_timeout(Duration::from_millis(5), task)
        {
          Ok((snapshot, edits)) => {
            self.snapshot = snapshot;
            self.edits_since_sync = self.edits_since_sync.compose(&edits);
          }
          Err(wrap_task) => {
            self.background_task = Some(cx.spawn(async move |this, cx| {
              let (snapshot, edits) = wrap_task.await;
              this
                .update(cx, |this, cx| {
                  this.snapshot = snapshot;
                  this.edits_since_sync = this
                    .edits_since_sync
                    .compose(mem::take(&mut this.interpolated_edits).invert())
                    .compose(&edits);
                  this.background_task = None;
                  this.flush_edits(cx);
                  cx.notify();
                })
                .ok();
            }));
          }
        }
      }
    } else {
      let old_rows = self.snapshot.transforms.summary().output.lines.row + 1;
      self.snapshot.transforms = SumTree::default();
      let summary = self.snapshot.tab_snapshot.text_summary();
      if !summary.lines.is_zero() {
        self
          .snapshot
          .transforms
          .push(Transform::isomorphic(summary), ());
      }
      let new_rows = self.snapshot.transforms.summary().output.lines.row + 1;
      self.snapshot.interpolated = false;
      self.edits_since_sync = self.edits_since_sync.compose(Patch::new(vec![WrapEdit {
        old: WrapRow(0)..WrapRow(old_rows),
        new: WrapRow(0)..WrapRow(new_rows),
      }]));
    }
  }

  #[ztracing::instrument(skip_all)]
  fn flush_edits(&mut self, cx: &mut Context<Self>) {
    if !self.snapshot.interpolated {
      let mut to_remove_len = 0;
      for (tab_snapshot, _) in &self.pending_edits {
        if tab_snapshot.version <= self.snapshot.tab_snapshot.version {
          to_remove_len += 1;
        } else {
          break;
        }
      }
      self.pending_edits.drain(..to_remove_len);
    }

    if self.pending_edits.is_empty() {
      return;
    }

    if let Some(wrap_width) = self.wrap_width
      && self.background_task.is_none()
    {
      let pending_edits = self.pending_edits.clone();
      let mut snapshot = self.snapshot.clone();
      let text_system = cx.text_system().clone();
      let (font, font_size) = self.font_with_size.clone();
      let mut fragment_builder = LineFragmentBuilder::new(text_system.clone(), &font, font_size);
      let mut line_wrapper = text_system.line_wrapper(font, font_size);

      let update_passes = pending_edits.len();
      let total_new_rows = pending_edits
        .iter()
        .flat_map(|(_, tab_edits)| tab_edits.iter())
        .map(|edit| (edit.new.end.row().saturating_sub(edit.new.start.row()) + 1) as usize)
        .sum::<usize>();
      if update_passes + total_new_rows < WRAP_YIELD_ROW_INTERVAL {
        let mut wrap_edits = Patch::default();
        for (tab_snapshot, tab_edits) in pending_edits {
          let indent_adjustment = self.indent_adjustment(tab_snapshot.tab_size);
          let edits = gpui::block_on(snapshot.update(
            tab_snapshot,
            &tab_edits,
            wrap_width,
            indent_adjustment,
            &mut line_wrapper,
            &mut fragment_builder,
          ));
          wrap_edits = wrap_edits.compose(&edits);
        }
        self.snapshot = snapshot;
        self.edits_since_sync = self.edits_since_sync.compose(&wrap_edits);
      } else {
        let soft_wrap_indent = self.soft_wrap_indent;
        let update_task = cx.background_spawn(async move {
          let mut edits = Patch::default();
          for (tab_snapshot, tab_edits) in pending_edits {
            let indent_adjustment = indent_adjustment_for(soft_wrap_indent, tab_snapshot.tab_size);
            let wrap_edits = snapshot
              .update(
                tab_snapshot,
                &tab_edits,
                wrap_width,
                indent_adjustment,
                &mut line_wrapper,
                &mut fragment_builder,
              )
              .await;
            edits = edits.compose(&wrap_edits);
          }
          (snapshot, edits)
        });

        match cx
          .foreground_executor()
          .block_with_timeout(Duration::from_millis(1), update_task)
        {
          Ok((snapshot, output_edits)) => {
            self.snapshot = snapshot;
            self.edits_since_sync = self.edits_since_sync.compose(&output_edits);
          }
          Err(update_task) => {
            self.background_task = Some(cx.spawn(async move |this, cx| {
              let (snapshot, edits) = update_task.await;
              this
                .update(cx, |this, cx| {
                  this.snapshot = snapshot;
                  this.edits_since_sync = this
                    .edits_since_sync
                    .compose(mem::take(&mut this.interpolated_edits).invert())
                    .compose(&edits);
                  this.background_task = None;
                  this.flush_edits(cx);
                  cx.notify();
                })
                .ok();
            }));
          }
        }
      }
    }

    let was_interpolated = self.snapshot.interpolated;
    let mut to_remove_len = 0;
    for (tab_snapshot, edits) in &self.pending_edits {
      if tab_snapshot.version <= self.snapshot.tab_snapshot.version {
        to_remove_len += 1;
      } else {
        let interpolated_edits = self.snapshot.interpolate(tab_snapshot.clone(), edits);
        self.edits_since_sync = self.edits_since_sync.compose(&interpolated_edits);
        self.interpolated_edits = self.interpolated_edits.compose(&interpolated_edits);
      }
    }

    if !was_interpolated {
      self.pending_edits.drain(..to_remove_len);
    }
  }
}

impl WrapSnapshot {
  #[ztracing::instrument(skip_all)]
  fn new(tab_snapshot: TabSnapshot) -> Self {
    let mut transforms = SumTree::default();
    let extent = tab_snapshot.text_summary();
    if !extent.lines.is_zero() {
      transforms.push(Transform::isomorphic(extent), ());
    }
    Self {
      transforms,
      tab_snapshot,
      interpolated: true,
    }
  }

  #[ztracing::instrument(skip_all)]
  fn interpolate(&mut self, new_tab_snapshot: TabSnapshot, tab_edits: &[TabEdit]) -> WrapPatch {
    let mut new_transforms;
    if tab_edits.is_empty() {
      new_transforms = self.transforms.clone();
    } else if !self.transforms.summary().has_wraps()
      && !new_tab_snapshot.text_summary().lines.is_zero()
    {
      // Fast path: without existing wraps, interpolation is a passthrough over the new tab snapshot.
      new_transforms = SumTree::default();
      new_transforms.push(Transform::isomorphic(new_tab_snapshot.text_summary()), ());
    } else {
      let mut old_cursor = self.transforms.cursor::<TabPoint>(());

      let mut tab_edits_iter = tab_edits.iter().peekable();
      new_transforms = old_cursor.slice(&tab_edits_iter.peek().unwrap().old.start, Bias::Right);

      while let Some(edit) = tab_edits_iter.next() {
        if edit.new.start > TabPoint::from(new_transforms.summary().input.lines) {
          let summary = new_tab_snapshot.text_summary_for_range(
            TabPoint::from(new_transforms.summary().input.lines)..edit.new.start,
          );
          new_transforms.push_or_extend(Transform::isomorphic(summary));
        }

        if !edit.new.is_empty() {
          new_transforms.push_or_extend(Transform::isomorphic(
            new_tab_snapshot.text_summary_for_range(edit.new.clone()),
          ));
        }

        old_cursor.seek_forward(&edit.old.end, Bias::Right);
        if let Some(next_edit) = tab_edits_iter.peek() {
          if next_edit.old.start > old_cursor.end() {
            if old_cursor.end() > edit.old.end {
              let summary = self
                .tab_snapshot
                .text_summary_for_range(edit.old.end..old_cursor.end());
              new_transforms.push_or_extend(Transform::isomorphic(summary));
            }

            old_cursor.next();
            new_transforms.append(old_cursor.slice(&next_edit.old.start, Bias::Right), ());
          }
        } else {
          if old_cursor.end() > edit.old.end {
            let summary = self
              .tab_snapshot
              .text_summary_for_range(edit.old.end..old_cursor.end());
            new_transforms.push_or_extend(Transform::isomorphic(summary));
          }
          old_cursor.next();
          new_transforms.append(old_cursor.suffix(), ());
        }
      }
    }

    let old_snapshot = mem::replace(
      self,
      WrapSnapshot {
        tab_snapshot: new_tab_snapshot,
        transforms: new_transforms,
        interpolated: true,
      },
    );
    self.check_invariants();
    old_snapshot.compute_edits(tab_edits, self)
  }

  #[ztracing::instrument(skip_all)]
  async fn update(
    &mut self,
    new_tab_snapshot: TabSnapshot,
    tab_edits: &[TabEdit],
    wrap_width: Pixels,
    indent_adjustment: IndentAdjustment,
    line_wrapper: &mut LineWrapper,
    fragment_builder: &mut LineFragmentBuilder,
  ) -> WrapPatch {
    #[derive(Debug)]
    struct RowEdit {
      old_rows: Range<u32>,
      new_rows: Range<u32>,
    }

    let mut tab_edits_iter = tab_edits.iter().peekable();
    let mut row_edits = Vec::with_capacity(tab_edits.len());
    while let Some(edit) = tab_edits_iter.next() {
      let mut row_edit = RowEdit {
        old_rows: edit.old.start.row()..edit.old.end.row() + 1,
        new_rows: edit.new.start.row()..edit.new.end.row() + 1,
      };

      while let Some(next_edit) = tab_edits_iter.peek() {
        if next_edit.old.start.row() <= row_edit.old_rows.end {
          row_edit.old_rows.end = cmp::max(row_edit.old_rows.end, next_edit.old.end.row() + 1);
          row_edit.new_rows.end = cmp::max(row_edit.new_rows.end, next_edit.new.end.row() + 1);
          tab_edits_iter.next();
        } else {
          break;
        }
      }

      row_edits.push(row_edit);
    }

    let mut new_transforms;
    if row_edits.is_empty() {
      new_transforms = self.transforms.clone();
    } else {
      let mut row_edits = row_edits.into_iter().peekable();
      let mut old_cursor = self.transforms.cursor::<TabPoint>(());

      new_transforms = old_cursor.slice(
        &TabPoint::new(row_edits.peek().unwrap().old_rows.start, 0),
        Bias::Right,
      );

      while let Some(edit) = row_edits.next() {
        if edit.new_rows.start > new_transforms.summary().input.lines.row {
          let summary = new_tab_snapshot.text_summary_for_range(
            TabPoint(new_transforms.summary().input.lines)..TabPoint::new(edit.new_rows.start, 0),
          );
          new_transforms.push_or_extend(Transform::isomorphic(summary));
        }

        let mut line = String::new();
        let mut line_fragments = Vec::new();
        let mut remaining = None;
        let mut chunks = new_tab_snapshot.chunks(
          TabPoint::new(edit.new_rows.start, 0)..new_tab_snapshot.max_point(),
          LanguageAwareStyling {
            tree_sitter: false,
            diagnostics: false,
          },
          Highlights::default(),
        );
        let mut edit_transforms = Vec::<Transform>::new();
        for (i, _) in (edit.new_rows.start..edit.new_rows.end).enumerate() {
          while let Some(chunk) = remaining.take().or_else(|| chunks.next()) {
            if let Some(ix) = chunk.text.find('\n') {
              let (prefix, suffix) = chunk.text.split_at(ix + 1);
              fragment_builder.push_fragments(&mut line_fragments, prefix);
              line.push_str(prefix);
              remaining = Some(Chunk {
                text: suffix,
                ..chunk
              });
              break;
            } else {
              if let Some(width) = chunk.renderer.as_ref().and_then(|r| r.measured_width) {
                line_fragments.push(gpui::LineFragment::element(width, chunk.text.len()));
              } else {
                fragment_builder.push_fragments(&mut line_fragments, chunk.text);
              }
              line.push_str(chunk.text);
            }
          }

          if line.is_empty() {
            break;
          }

          let mut prev_boundary_ix = 0;
          for boundary in line_wrapper.wrap_line(&line_fragments, wrap_width, indent_adjustment) {
            let wrapped = &line[prev_boundary_ix..boundary.ix];
            push_isomorphic(&mut edit_transforms, TextSummary::from(wrapped));
            edit_transforms.push(Transform::wrap(boundary.next_indent));
            prev_boundary_ix = boundary.ix;
          }

          if prev_boundary_ix < line.len() {
            push_isomorphic(
              &mut edit_transforms,
              TextSummary::from(&line[prev_boundary_ix..]),
            );
          }

          line.clear();
          line_fragments.clear();
          if i % WRAP_YIELD_ROW_INTERVAL == WRAP_YIELD_ROW_INTERVAL - 1 {
            yield_now().await;
          }
        }

        let mut edit_transforms = edit_transforms.into_iter();
        if let Some(transform) = edit_transforms.next() {
          new_transforms.push_or_extend(transform);
        }
        new_transforms.extend(edit_transforms, ());

        old_cursor.seek_forward(&TabPoint::new(edit.old_rows.end, 0), Bias::Right);
        if let Some(next_edit) = row_edits.peek() {
          if next_edit.old_rows.start > old_cursor.end().row() {
            if old_cursor.end() > TabPoint::new(edit.old_rows.end, 0) {
              let summary = self
                .tab_snapshot
                .text_summary_for_range(TabPoint::new(edit.old_rows.end, 0)..old_cursor.end());
              new_transforms.push_or_extend(Transform::isomorphic(summary));
            }
            old_cursor.next();
            new_transforms.append(
              old_cursor.slice(&TabPoint::new(next_edit.old_rows.start, 0), Bias::Right),
              (),
            );
          }
          yield_now().await;
        } else {
          if old_cursor.end() > TabPoint::new(edit.old_rows.end, 0) {
            let summary = self
              .tab_snapshot
              .text_summary_for_range(TabPoint::new(edit.old_rows.end, 0)..old_cursor.end());
            new_transforms.push_or_extend(Transform::isomorphic(summary));
          }
          old_cursor.next();
          new_transforms.append(old_cursor.suffix(), ());
        }
      }
    }

    let old_snapshot = mem::replace(
      self,
      WrapSnapshot {
        tab_snapshot: new_tab_snapshot,
        transforms: new_transforms,
        interpolated: false,
      },
    );
    self.check_invariants();
    old_snapshot.compute_edits(tab_edits, self)
  }

  #[ztracing::instrument(skip_all)]
  fn compute_edits(&self, tab_edits: &[TabEdit], new_snapshot: &WrapSnapshot) -> WrapPatch {
    let mut wrap_edits = Vec::with_capacity(tab_edits.len());
    let mut old_cursor = self.transforms.cursor::<TransformSummary>(());
    let mut new_cursor = new_snapshot.transforms.cursor::<TransformSummary>(());
    for mut tab_edit in tab_edits.iter().cloned() {
      tab_edit.old.start.0.column = 0;
      tab_edit.old.end.0 += Point::new(1, 0);
      tab_edit.new.start.0.column = 0;
      tab_edit.new.end.0 += Point::new(1, 0);

      old_cursor.seek(&tab_edit.old.start, Bias::Right);
      let mut old_start = old_cursor.start().output.lines;
      old_start += tab_edit.old.start.0 - old_cursor.start().input.lines;

      old_cursor.seek_forward(&tab_edit.old.end, Bias::Right);
      let mut old_end = old_cursor.start().output.lines;
      old_end += tab_edit.old.end.0 - old_cursor.start().input.lines;

      new_cursor.seek(&tab_edit.new.start, Bias::Right);
      let mut new_start = new_cursor.start().output.lines;
      new_start += tab_edit.new.start.0 - new_cursor.start().input.lines;

      new_cursor.seek_forward(&tab_edit.new.end, Bias::Right);
      let mut new_end = new_cursor.start().output.lines;
      new_end += tab_edit.new.end.0 - new_cursor.start().input.lines;

      wrap_edits.push(WrapEdit {
        old: WrapRow(old_start.row)..WrapRow(old_end.row),
        new: WrapRow(new_start.row)..WrapRow(new_end.row),
      });
    }

    wrap_edits = consolidate_wrap_edits(wrap_edits);
    Patch::new(wrap_edits)
  }

  #[ztracing::instrument(skip_all)]
  pub(crate) fn chunks<'a>(
    &'a self,
    rows: Range<WrapRow>,
    language_aware: LanguageAwareStyling,
    highlights: Highlights<'a>,
  ) -> WrapChunks<'a> {
    let output_start = WrapPoint::new(rows.start, 0);
    let output_end = WrapPoint::new(rows.end, 0);
    let mut transforms = self
      .transforms
      .cursor::<Dimensions<WrapPoint, TabPoint>>(());
    transforms.seek(&output_start, Bias::Right);
    let mut input_start = TabPoint(transforms.start().1.0);
    if transforms.item().is_some_and(|t| t.is_isomorphic()) {
      input_start.0 += output_start.0 - transforms.start().0.0;
    }
    let input_end = self.to_tab_point(output_end);
    let max_point = self.tab_snapshot.max_point();
    let input_start = input_start.min(max_point);
    let input_end = input_end.min(max_point);
    WrapChunks {
      input_chunks: self
        .tab_snapshot
        .chunks(input_start..input_end, language_aware, highlights),
      input_chunk: Default::default(),
      output_position: output_start,
      max_output_row: rows.end,
      transforms,
      snapshot: self,
    }
  }

  #[ztracing::instrument(skip_all)]
  pub fn max_point(&self) -> WrapPoint {
    WrapPoint(self.transforms.summary().output.lines)
  }

  #[ztracing::instrument(skip_all)]
  pub fn line_len(&self, row: WrapRow) -> u32 {
    let (start, _, item) = self.transforms.find::<Dimensions<WrapPoint, TabPoint>, _>(
      (),
      &WrapPoint::new(row + WrapRow(1), 0),
      Bias::Left,
    );
    if item.is_some_and(|transform| transform.is_isomorphic()) {
      let overshoot = row - start.0.row();
      let tab_row = start.1.row() + overshoot.0;
      let tab_line_len = self.tab_snapshot.line_len(tab_row);
      if overshoot.0 == 0 {
        start.0.column() + (tab_line_len - start.1.column())
      } else {
        tab_line_len
      }
    } else {
      start.0.column()
    }
  }

  #[ztracing::instrument(skip_all, fields(rows))]
  pub fn text_summary_for_range(&self, rows: Range<WrapRow>) -> TextSummary {
    let mut summary = TextSummary::default();

    let start = WrapPoint::new(rows.start, 0);
    let end = WrapPoint::new(rows.end, 0);

    let mut cursor = self
      .transforms
      .cursor::<Dimensions<WrapPoint, TabPoint>>(());
    cursor.seek(&start, Bias::Right);
    if let Some(transform) = cursor.item() {
      let start_in_transform = start.0 - cursor.start().0.0;
      let end_in_transform = cmp::min(end, cursor.end().0).0 - cursor.start().0.0;
      if transform.is_isomorphic() {
        let tab_start = TabPoint(cursor.start().1.0 + start_in_transform);
        let tab_end = TabPoint(cursor.start().1.0 + end_in_transform);
        summary += &self.tab_snapshot.text_summary_for_range(tab_start..tab_end);
      } else {
        debug_assert_eq!(start_in_transform.row, end_in_transform.row);
        let indent_len = end_in_transform.column - start_in_transform.column;
        summary += &TextSummary {
          lines: Point::new(0, indent_len),
          first_line_chars: indent_len,
          last_line_chars: indent_len,
          longest_row: 0,
          longest_row_chars: indent_len,
        };
      }

      cursor.next();
    }

    if rows.end > cursor.start().0.row() {
      summary += &cursor
        .summary::<_, TransformSummary>(&WrapPoint::new(rows.end, 0), Bias::Right)
        .output;

      if let Some(transform) = cursor.item() {
        let end_in_transform = end.0 - cursor.start().0.0;
        if transform.is_isomorphic() {
          let char_start = cursor.start().1;
          let char_end = TabPoint(char_start.0 + end_in_transform);
          summary += &self
            .tab_snapshot
            .text_summary_for_range(char_start..char_end);
        } else {
          debug_assert_eq!(end_in_transform, Point::new(1, 0));
          summary += &TextSummary {
            lines: Point::new(1, 0),
            first_line_chars: 0,
            last_line_chars: 0,
            longest_row: 0,
            longest_row_chars: 0,
          };
        }
      }
    }

    summary
  }

  #[ztracing::instrument(skip_all)]
  pub fn soft_wrap_indent(&self, row: WrapRow) -> Option<u32> {
    let (.., item) =
      self
        .transforms
        .find::<WrapPoint, _>((), &WrapPoint::new(row + WrapRow(1), 0), Bias::Left);
    item.and_then(|transform| {
      if transform.is_isomorphic() {
        None
      } else {
        Some(transform.summary.output.lines.column)
      }
    })
  }

  #[ztracing::instrument(skip_all)]
  pub fn longest_row(&self) -> u32 {
    self.transforms.summary().output.longest_row
  }

  #[ztracing::instrument(skip_all)]
  pub fn row_infos(&self, start_row: WrapRow) -> WrapRows<'_> {
    let mut transforms = self
      .transforms
      .cursor::<Dimensions<WrapPoint, TabPoint>>(());
    transforms.seek(&WrapPoint::new(start_row, 0), Bias::Left);
    let mut input_row = transforms.start().1.row();
    if transforms.item().is_some_and(|t| t.is_isomorphic()) {
      input_row += (start_row - transforms.start().0.row()).0;
    }
    let soft_wrapped = transforms.item().is_some_and(|t| !t.is_isomorphic());
    let mut input_buffer_rows = self.tab_snapshot.rows(input_row);
    let input_buffer_row = input_buffer_rows.next().unwrap();
    WrapRows {
      transforms,
      input_buffer_row,
      input_buffer_rows,
      output_row: start_row,
      soft_wrapped,
      max_output_row: self.max_point().row(),
    }
  }

  #[ztracing::instrument(skip_all)]
  pub fn to_tab_point(&self, point: WrapPoint) -> TabPoint {
    let (start, _, item) =
      self
        .transforms
        .find::<Dimensions<WrapPoint, TabPoint>, _>((), &point, Bias::Right);
    let mut tab_point = start.1.0;
    if item.is_some_and(|t| t.is_isomorphic()) {
      tab_point += point.0 - start.0.0;
    }
    TabPoint(tab_point)
  }

  #[ztracing::instrument(skip_all)]
  pub fn to_point(&self, point: WrapPoint, bias: Bias) -> Point {
    self
      .tab_snapshot
      .tab_point_to_point(self.to_tab_point(point), bias)
  }

  #[ztracing::instrument(skip_all)]
  pub fn make_wrap_point(&self, point: Point, bias: Bias) -> WrapPoint {
    self.tab_point_to_wrap_point(self.tab_snapshot.point_to_tab_point(point, bias))
  }

  #[ztracing::instrument(skip_all)]
  pub fn tab_point_to_wrap_point(&self, point: TabPoint) -> WrapPoint {
    let (start, ..) =
      self
        .transforms
        .find::<Dimensions<TabPoint, WrapPoint>, _>((), &point, Bias::Right);
    WrapPoint(start.1.0 + (point.0 - start.0.0))
  }

  #[ztracing::instrument(skip_all)]
  pub fn wrap_point_cursor(&self) -> WrapPointCursor<'_> {
    WrapPointCursor {
      cursor: self
        .transforms
        .cursor::<Dimensions<TabPoint, WrapPoint>>(()),
    }
  }

  #[ztracing::instrument(skip_all)]
  pub fn clip_point(&self, mut point: WrapPoint, bias: Bias) -> WrapPoint {
    if bias == Bias::Left {
      let (start, _, item) = self
        .transforms
        .find::<WrapPoint, _>((), &point, Bias::Right);
      if item.is_some_and(|t| !t.is_isomorphic()) {
        point = start;
        *point.column_mut() -= 1;
      }
    }

    self.tab_point_to_wrap_point(self.tab_snapshot.clip_point(self.to_tab_point(point), bias))
  }

  /// Try to find a TabRow start that is also a WrapRow start
  /// Every TabRow start is a WrapRow start
  #[ztracing::instrument(skip_all, fields(point=?point))]
  pub fn prev_row_boundary(&self, point: WrapPoint) -> WrapRow {
    if self.transforms.is_empty() {
      return WrapRow(0);
    }

    let point = WrapPoint::new(point.row(), 0);

    let mut cursor = self
      .transforms
      .cursor::<Dimensions<WrapPoint, TabPoint>>(());

    cursor.seek(&point, Bias::Right);
    if cursor.item().is_none() {
      cursor.prev();
    }

    //                          real newline     fake          fake
    // text:      helloworldasldlfjasd\njdlasfalsk\naskdjfasdkfj\n
    // dimensions v       v           v            v            v
    // transforms |-------|-----NW----|-----W------|-----W------|
    // cursor    ^        ^^^^^^^^^^^^^                          ^
    //                               (^)           ^^^^^^^^^^^^^^
    // point:                                            ^
    // point(col_zero):                           (^)

    while let Some(transform) = cursor.item() {
      if transform.is_isomorphic() {
        // this transform only has real linefeeds
        let tab_summary = &transform.summary.input;
        // is the wrap just before the end of the transform a tab row?
        // thats only if this transform has at least one newline
        //
        // "this wrap row is a tab row" <=> self.to_tab_point(WrapPoint::new(wrap_row, 0)).column() == 0

        // Note on comparison:
        // We have code that relies on this to be row > 1
        // It should work with row >= 1 but it does not :(
        //
        // That means that if every line is wrapped we walk back all the
        // way to the start. Which invalidates the entire state triggering
        // a full re-render.
        if tab_summary.lines.row > 1 {
          let wrap_point_at_end = cursor.end().0.row();
          return cmp::min(wrap_point_at_end - RowDelta(1), point.row());
        } else if cursor.start().1.column() == 0 {
          return cmp::min(cursor.end().0.row(), point.row());
        }
      }

      cursor.prev();
    }

    WrapRow(0)
  }

  #[ztracing::instrument(skip_all)]
  pub fn next_row_boundary(&self, mut point: WrapPoint) -> Option<WrapRow> {
    point.0 += Point::new(1, 0);

    let mut cursor = self
      .transforms
      .cursor::<Dimensions<WrapPoint, TabPoint>>(());
    cursor.seek(&point, Bias::Right);
    while let Some(transform) = cursor.item() {
      if transform.is_isomorphic() && cursor.start().1.column() == 0 {
        return Some(cmp::max(cursor.start().0.row(), point.row()));
      } else {
        cursor.next();
      }
    }

    None
  }

  #[ztracing::instrument(skip_all)]
  fn check_invariants(&self) {

  }
}

pub struct WrapPointCursor<'transforms> {
  cursor: Cursor<'transforms, 'static, Transform, Dimensions<TabPoint, WrapPoint>>,
}

impl WrapPointCursor<'_> {
  /// Resets the cursor to the start so it can seek backward again.
  pub fn reset(&mut self) {
    self.cursor.reset();
  }

  #[ztracing::instrument(skip_all)]
  pub fn map(&mut self, point: TabPoint) -> WrapPoint {
    let cursor = &mut self.cursor;
    if cursor.did_seek() && point >= cursor.start().0 {
      cursor.seek_forward(&point, Bias::Right);
    } else {
      cursor.seek(&point, Bias::Right);
    }
    WrapPoint(cursor.start().1.0 + (point.0 - cursor.start().0.0))
  }
}

impl WrapChunks<'_> {
  #[ztracing::instrument(skip_all)]
  pub(crate) fn seek(&mut self, rows: Range<WrapRow>) {
    let output_start = WrapPoint::new(rows.start, 0);
    let output_end = WrapPoint::new(rows.end, 0);
    self.transforms.seek(&output_start, Bias::Right);
    let mut input_start = TabPoint(self.transforms.start().1.0);
    if self.transforms.item().is_some_and(|t| t.is_isomorphic()) {
      input_start.0 += output_start.0 - self.transforms.start().0.0;
    }
    let input_end = self.snapshot.to_tab_point(output_end);
    let max_point = self.snapshot.tab_snapshot.max_point();
    let input_start = input_start.min(max_point);
    let input_end = input_end.min(max_point);
    self.input_chunks.seek(input_start..input_end);
    self.input_chunk = Chunk::default();
    self.output_position = output_start;
    self.max_output_row = rows.end;
  }
}

impl<'a> Iterator for WrapChunks<'a> {
  type Item = Chunk<'a>;

  #[ztracing::instrument(skip_all)]
  fn next(&mut self) -> Option<Self::Item> {
    if self.output_position.row() >= self.max_output_row {
      return None;
    }

    let transform = self.transforms.item()?;
    if let Some(display_text) = transform.display_text {
      let mut start_ix = 0;
      let mut end_ix = display_text.len();
      let mut summary = transform.summary.output.lines;

      if self.output_position > self.transforms.start().0 {
        // Exclude newline starting prior to the desired row.
        start_ix = 1;
        summary.row = 0;
      } else if self.output_position.row() + WrapRow(1) >= self.max_output_row {
        // Exclude soft indentation ending after the desired row.
        end_ix = 1;
        summary.column = 0;
      }

      self.output_position.0 += summary;
      self.transforms.next();
      return Some(Chunk {
        text: &display_text[start_ix..end_ix],
        ..Default::default()
      });
    }

    if self.input_chunk.text.is_empty() {
      self.input_chunk = self.input_chunks.next()?;
    }

    let mut input_len = 0;
    let transform_end = self.transforms.end().0;
    for c in self.input_chunk.text.chars() {
      let char_len = c.len_utf8();
      input_len += char_len;
      if c == '\n' {
        *self.output_position.row_mut() += 1;
        *self.output_position.column_mut() = 0;
      } else {
        *self.output_position.column_mut() += char_len as u32;
      }

      if self.output_position >= transform_end {
        self.transforms.next();
        break;
      }
    }

    let (prefix, suffix) = self.input_chunk.text.split_at(input_len);

    let mask = 1u128.unbounded_shl(input_len as u32).wrapping_sub(1);
    let chars = self.input_chunk.chars & mask;
    let tabs = self.input_chunk.tabs & mask;
    let newlines = self.input_chunk.newlines & mask;
    self.input_chunk.tabs = self.input_chunk.tabs.unbounded_shr(input_len as u32);
    self.input_chunk.chars = self.input_chunk.chars.unbounded_shr(input_len as u32);
    self.input_chunk.newlines = self.input_chunk.newlines.unbounded_shr(input_len as u32);

    self.input_chunk.text = suffix;
    Some(Chunk {
      text: prefix,
      chars,
      tabs,
      newlines,
      ..self.input_chunk.clone()
    })
  }
}

impl Iterator for WrapRows<'_> {
  type Item = RowInfo;

  #[ztracing::instrument(skip_all)]
  fn next(&mut self) -> Option<Self::Item> {
    if self.output_row > self.max_output_row {
      return None;
    }

    let buffer_row = self.input_buffer_row;
    let soft_wrapped = self.soft_wrapped;
    let diff_status = self.input_buffer_row.diff_status;

    self.output_row += WrapRow(1);
    self
      .transforms
      .seek_forward(&WrapPoint::new(self.output_row, 0), Bias::Left);
    if self.transforms.item().is_some_and(|t| t.is_isomorphic()) {
      self.input_buffer_row = self.input_buffer_rows.next().unwrap();
      self.soft_wrapped = false;
    } else {
      self.soft_wrapped = true;
    }

    Some(if soft_wrapped {
      RowInfo {
        buffer_id: None,
        buffer_row: None,
        multibuffer_row: None,
        diff_status,
        expand_info: None,
        wrapped_buffer_row: buffer_row.buffer_row,
      }
    } else {
      buffer_row
    })
  }
}

fn indent_adjustment_for(indent: SoftWrapIndent, tab_size: NonZeroU32) -> IndentAdjustment {
  match indent {
    SoftWrapIndent::None => IndentAdjustment::NoIndent,
    SoftWrapIndent::Same => IndentAdjustment::SameIndent,
    SoftWrapIndent::ExtraOne => IndentAdjustment::ExtraColumns(tab_size.get()),
    SoftWrapIndent::ExtraTwo => IndentAdjustment::ExtraColumns(tab_size.get() * 2),
  }
}

impl Transform {
  #[ztracing::instrument(skip_all)]
  fn isomorphic(summary: TextSummary) -> Self {


    Self {
      summary: TransformSummary {
        input: summary.clone(),
        output: summary,
      },
      display_text: None,
    }
  }

  #[ztracing::instrument(skip_all)]
  fn wrap(indent: u32) -> Self {
    static WRAP_TEXT: LazyLock<String> = LazyLock::new(|| {
      let mut wrap_text = String::new();
      wrap_text.push('\n');
      wrap_text.extend((0..LineWrapper::MAX_INDENT as usize).map(|_| ' '));
      wrap_text
    });

    Self {
      summary: TransformSummary {
        input: TextSummary::default(),
        output: TextSummary {
          lines: Point::new(1, indent),
          first_line_chars: 0,
          last_line_chars: indent,
          longest_row: 1,
          longest_row_chars: indent,
        },
      },
      display_text: Some(&WRAP_TEXT[..1 + indent as usize]),
    }
  }

  fn is_isomorphic(&self) -> bool {
    self.display_text.is_none()
  }
}

impl sum_tree::Item for Transform {
  type Summary = TransformSummary;

  fn summary(&self, _cx: ()) -> Self::Summary {
    self.summary.clone()
  }
}

fn push_isomorphic(transforms: &mut Vec<Transform>, summary: TextSummary) {
  if let Some(last_transform) = transforms.last_mut()
    && last_transform.is_isomorphic()
  {
    last_transform.summary.input += &summary;
    last_transform.summary.output += &summary;
    return;
  }
  transforms.push(Transform::isomorphic(summary));
}

trait SumTreeExt {
  fn push_or_extend(&mut self, transform: Transform);
}

impl SumTreeExt for SumTree<Transform> {
  #[ztracing::instrument(skip_all)]
  fn push_or_extend(&mut self, transform: Transform) {
    let mut transform = Some(transform);
    self.update_last(
      |last_transform| {
        if last_transform.is_isomorphic() && transform.as_ref().unwrap().is_isomorphic() {
          let transform = transform.take().unwrap();
          last_transform.summary.input += &transform.summary.input;
          last_transform.summary.output += &transform.summary.output;
        }
      },
      (),
    );

    if let Some(transform) = transform {
      self.push(transform, ());
    }
  }
}

impl WrapPoint {
  pub fn new(row: WrapRow, column: u32) -> Self {
    Self(Point::new(row.0, column))
  }

  pub fn row(self) -> WrapRow {
    WrapRow(self.0.row)
  }

  pub fn row_mut(&mut self) -> &mut u32 {
    &mut self.0.row
  }

  pub fn column(self) -> u32 {
    self.0.column
  }

  pub fn column_mut(&mut self) -> &mut u32 {
    &mut self.0.column
  }
}

impl sum_tree::ContextLessSummary for TransformSummary {
  fn zero() -> Self {
    Default::default()
  }

  fn add_summary(&mut self, other: &Self) {
    self.input += &other.input;
    self.output += &other.output;
  }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for TabPoint {
  fn zero(_cx: ()) -> Self {
    Default::default()
  }

  fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
    self.0 += summary.input.lines;
  }
}

impl sum_tree::SeekTarget<'_, TransformSummary, TransformSummary> for TabPoint {
  #[ztracing::instrument(skip_all)]
  fn cmp(&self, cursor_location: &TransformSummary, _: ()) -> std::cmp::Ordering {
    Ord::cmp(&self.0, &cursor_location.input.lines)
  }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for WrapPoint {
  fn zero(_cx: ()) -> Self {
    Default::default()
  }

  fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
    self.0 += summary.output.lines;
  }
}

fn consolidate_wrap_edits(edits: Vec<WrapEdit>) -> Vec<WrapEdit> {
  let _old_alloc_ptr = edits.as_ptr();
  let mut wrap_edits = edits.into_iter();

  if let Some(mut first_edit) = wrap_edits.next() {
    // This code relies on reusing allocations from the Vec<_> - at the time of writing .flatten() prevents them.
    #[allow(clippy::filter_map_identity)]
    let mut v: Vec<_> = wrap_edits
      .scan(&mut first_edit, |prev_edit, edit| {
        if prev_edit.old.end >= edit.old.start {
          prev_edit.old.end = edit.old.end;
          prev_edit.new.end = edit.new.end;
          Some(None) // Skip this edit, it's merged
        } else {
          let prev = std::mem::replace(*prev_edit, edit);
          Some(Some(prev)) // Yield the previous edit
        }
      })
      .filter_map(|x| x)
      .collect();
    v.push(first_edit.clone());
    debug_assert_eq!(v.as_ptr(), _old_alloc_ptr, "Wrap edits were reallocated");
    v
  } else {
    vec![]
  }
}
