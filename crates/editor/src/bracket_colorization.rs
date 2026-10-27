//! Bracket highlights, also known as "rainbow brackets".
//! Uses tree-sitter queries from brackets.scm to capture bracket pairs,
//! and theme accents to colorize those.

use std::cmp::Ordering;
use std::ops::Range;
use std::sync::Arc;

use crate::{Editor, HighlightKey};
use collections::{HashMap, HashSet};
use gpui::{AppContext as _, Context, HighlightStyle, Hsla};
use language::{BufferRow, BufferSnapshot, language_settings::LanguageSettings};
use multi_buffer::{Anchor, BufferOffset, ExcerptRange, MultiBufferSnapshot};
use text::OffsetRangeExt as _;
use theme::{Appearance, Oklab, Oklch, hsla_to_oklab, hsla_to_oklch, oklch_to_hsla};
use ui::utils::apca_contrast;

impl Editor {
  pub(crate) fn colorize_brackets(&mut self, invalidate: bool, cx: &mut Context<Editor>) {
    if !self.mode.is_full() {
      return;
    }

    if invalidate {
      self.bracket_fetched_tree_sitter_chunks.clear();
    }

    let Some(accent_data) = self.accent_data.as_ref() else {
      return;
    };
    let accents = accent_data.colors.0.clone();
    let multi_buffer_snapshot = self.buffer().read(cx).snapshot(cx);

    let visible_excerpts = self.visible_buffer_ranges(cx);
    let excerpt_data: Vec<(
      BufferSnapshot,
      Range<BufferOffset>,
      ExcerptRange<text::Anchor>,
    )> = visible_excerpts
      .into_iter()
      .filter(|(buffer_snapshot, _, _)| {
        let Some(buffer) = self.buffer().read(cx).buffer(buffer_snapshot.remote_id()) else {
          return false;
        };
        let buffer = buffer.read(cx);
        buffer
          .language()
          .is_some_and(|language| language.grammar().is_some())
          && LanguageSettings::for_buffer(buffer, cx).colorize_brackets
      })
      .collect();

    if !invalidate && (accents.is_empty() || excerpt_data.is_empty()) {
      return;
    }

    let mut fetched_tree_sitter_chunks = excerpt_data
      .iter()
      .filter_map(|(_, _, excerpt_range)| {
        let key = excerpt_range.context.clone();
        Some((
          key.clone(),
          self.bracket_fetched_tree_sitter_chunks.get(&key).cloned()?,
        ))
      })
      .collect::<HashMap<Range<text::Anchor>, HashSet<Range<BufferRow>>>>();

    let accents_count = accents.len();
    let bracket_matches_by_accent = cx.background_spawn(async move {
      if accents_count == 0 {
        return (HashMap::default(), fetched_tree_sitter_chunks);
      }

      let bracket_matches_by_accent: HashMap<usize, Vec<Range<Anchor>>> =
        excerpt_data.into_iter().fold(
          HashMap::default(),
          |mut acc, (buffer_snapshot, buffer_range, excerpt_range)| {
            let fetched_chunks = fetched_tree_sitter_chunks
              .entry(excerpt_range.context.clone())
              .or_default();

            let brackets_by_accent = compute_bracket_ranges(
              &multi_buffer_snapshot,
              &buffer_snapshot,
              buffer_range,
              excerpt_range,
              fetched_chunks,
              accents_count,
            );

            for (accent_number, new_ranges) in brackets_by_accent {
              let ranges = acc
                .entry(accent_number)
                .or_insert_with(Vec::<Range<Anchor>>::new);

              for new_range in new_ranges {
                let i = ranges
                  .binary_search_by(|probe| {
                    probe.start.cmp(&new_range.start, &multi_buffer_snapshot)
                  })
                  .unwrap_or_else(|i| i);
                ranges.insert(i, new_range);
              }
            }

            acc
          },
        );

      (bracket_matches_by_accent, fetched_tree_sitter_chunks)
    });

    self.colorize_brackets_task = cx.spawn(async move |editor, cx| {
      let (bracket_matches_by_accent, updated_chunks) = bracket_matches_by_accent.await;

      editor
        .update(cx, |editor, cx| {
          if invalidate {
            editor.clear_highlights_with(
              &mut |key| matches!(key, HighlightKey::ColorizeBracket(_)),
              cx,
            );
          }

          editor
            .bracket_fetched_tree_sitter_chunks
            .extend(updated_chunks);
          for (accent_number, bracket_highlights) in bracket_matches_by_accent {
            let Some(&bracket_color) = accents.get(accent_number) else {
              continue;
            };
            let style = HighlightStyle {
              color: Some(bracket_color),
              ..HighlightStyle::default()
            };

            editor.highlight_text_key(
              HighlightKey::ColorizeBracket(accent_number),
              bracket_highlights,
              style,
              true,
              cx,
            );
          }
        })
        .ok();
    });
  }
}

const BACKGROUND_APCA_LIGHT: f32 = 35.0;
const BACKGROUND_APCA_DARK: f32 = 30.0;
const ADJACENT_OKLAB_LIGHT: f32 = 0.10;
const ADJACENT_OKLAB_DARK: f32 = 0.08;
const ADJACENT_OKLAB_LIGHT_INTERVENTION: f32 = 0.095;
const ADJACENT_OKLAB_DARK_INTERVENTION: f32 = 0.08;
const LIGHTNESS_CLAMP_MIN: f32 = 0.18;
const LIGHTNESS_CLAMP_MAX: f32 = 0.92;

pub(crate) fn bracket_colorization_accents(
  accents: &[Hsla],
  appearance: Appearance,
  background: Hsla,
) -> Arc<[Hsla]> {
  let (intervention_distance, comfortable_distance, min_background_contrast) = match appearance {
    Appearance::Light => (
      ADJACENT_OKLAB_LIGHT_INTERVENTION,
      ADJACENT_OKLAB_LIGHT,
      BACKGROUND_APCA_LIGHT,
    ),
    Appearance::Dark => (
      ADJACENT_OKLAB_DARK_INTERVENTION,
      ADJACENT_OKLAB_DARK,
      BACKGROUND_APCA_DARK,
    ),
  };
  let background_adjusted = accents
    .iter()
    .copied()
    .map(|accent| adjust_color_for_background(accent, background, min_background_contrast))
    .collect::<Vec<_>>();
  let adjusted_min_adj = min_adjacent_oklab_distance(&background_adjusted, background);

  if accents.len() < 3 || adjusted_min_adj >= intervention_distance {
    return Arc::from(background_adjusted);
  }

  let reordered = maximize_adjacent_separation(&background_adjusted, background);
  if min_adjacent_oklab_distance(&reordered, background) >= comfortable_distance {
    Arc::from(reordered)
  } else {
    Arc::from(background_adjusted)
  }
}

fn maximize_adjacent_separation(accents: &[Hsla], background: Hsla) -> Vec<Hsla> {
  let Some((&first, rest)) = accents.split_first() else {
    return Vec::new();
  };
  let mut remaining = rest.to_vec();
  let mut order = Vec::with_capacity(accents.len());
  order.push(first);
  let mut last = first;

  while !remaining.is_empty() {
    let Some((position, &next)) = remaining
      .iter()
      .enumerate()
      .max_by(|&(_, &left), &(_, &right)| compare_candidates(background, last, first, left, right))
    else {
      break;
    };
    remaining.swap_remove(position);
    order.push(next);
    last = next;
  }

  order
}

fn compare_candidates(
  background: Hsla,
  last: Hsla,
  first: Hsla,
  left: Hsla,
  right: Hsla,
) -> Ordering {
  adjacent_distance(last, left, background)
    .partial_cmp(&adjacent_distance(last, right, background))
    .unwrap_or(Ordering::Equal)
    .then_with(|| {
      adjacent_distance(first, left, background)
        .partial_cmp(&adjacent_distance(first, right, background))
        .unwrap_or(Ordering::Equal)
    })
}

fn min_adjacent_oklab_distance(accents: &[Hsla], background: Hsla) -> f32 {
  if accents.len() < 2 {
    return f32::MAX;
  }
  accents
    .iter()
    .copied()
    .zip(accents.iter().copied().cycle().skip(1))
    .take(accents.len())
    .map(|(left, right)| adjacent_distance(left, right, background))
    .fold(f32::MAX, f32::min)
}

fn oklab_distance(left: Oklab, right: Oklab) -> f32 {
  let dl = left.l - right.l;
  let da = left.a - right.a;
  let db = left.b - right.b;
  (dl * dl + da * da + db * db).sqrt()
}

fn adjacent_distance(left: Hsla, right: Hsla, background: Hsla) -> f32 {
  oklab_distance(
    hsla_to_oklab(background.blend(left)),
    hsla_to_oklab(background.blend(right)),
  )
}

fn adjust_color_for_background(
  color: Hsla,
  background: Hsla,
  minimum_background_contrast: f32,
) -> Hsla {
  if background_contrast(color, background) >= minimum_background_contrast {
    return color;
  }

  let original = hsla_to_oklab(color);
  let darker_candidate = adjusted_lightness_candidate(
    color,
    background,
    minimum_background_contrast,
    LIGHTNESS_CLAMP_MIN,
  );
  let lighter_candidate = adjusted_lightness_candidate(
    color,
    background,
    minimum_background_contrast,
    LIGHTNESS_CLAMP_MAX,
  );

  match (darker_candidate, lighter_candidate) {
    (Some(darker_candidate), Some(lighter_candidate)) => {
      let darker_distance = oklab_distance(original, hsla_to_oklab(darker_candidate));
      let lighter_distance = oklab_distance(original, hsla_to_oklab(lighter_candidate));
      if darker_distance <= lighter_distance {
        darker_candidate
      } else {
        lighter_candidate
      }
    }
    (Some(darker_candidate), None) => darker_candidate,
    (None, Some(lighter_candidate)) => lighter_candidate,
    (None, None) => color,
  }
}

fn adjusted_lightness_candidate(
  color: Hsla,
  background: Hsla,
  minimum_background_contrast: f32,
  target_lightness: f32,
) -> Option<Hsla> {
  let original = hsla_to_oklch(color);
  let lightness_delta = target_lightness - original.l;

  if lightness_delta.abs() <= f32::EPSILON {
    return None;
  }

  (1..=128).find_map(|step| {
    let amount = step as f32 / 128.0;
    let candidate = oklch_to_hsla(
      Oklch {
        l: (original.l + lightness_delta * amount).clamp(0.0, 1.0),
        chroma: original.chroma,
        hue: original.hue,
      },
      color.a,
    );
    (background_contrast(candidate, background) >= minimum_background_contrast).then_some(candidate)
  })
}

fn background_contrast(foreground: Hsla, background: Hsla) -> f32 {
  apca_contrast(background.blend(foreground), background).abs()
}

fn compute_bracket_ranges(
  multi_buffer_snapshot: &MultiBufferSnapshot,
  buffer_snapshot: &BufferSnapshot,
  buffer_range: Range<BufferOffset>,
  excerpt_range: ExcerptRange<text::Anchor>,
  fetched_chunks: &mut HashSet<Range<BufferRow>>,
  accents_count: usize,
) -> Vec<(usize, Vec<Range<Anchor>>)> {
  let context = excerpt_range.context.to_offset(buffer_snapshot);

  buffer_snapshot
    .fetch_bracket_ranges(
      buffer_range.start.0..buffer_range.end.0,
      Some(fetched_chunks),
    )
    .into_iter()
    .flat_map(|(chunk_range, pairs)| {
      if fetched_chunks.insert(chunk_range) {
        pairs
      } else {
        Vec::new()
      }
    })
    .filter_map(|pair| {
      let color_index = pair.color_index?;

      let mut ranges = Vec::new();

      if context.start <= pair.open_range.start && pair.open_range.end <= context.end {
        let anchors = buffer_snapshot.anchor_range_inside(pair.open_range);
        ranges.push(
          multi_buffer_snapshot.anchor_in_buffer(anchors.start)?
            ..multi_buffer_snapshot.anchor_in_buffer(anchors.end)?,
        );
      };

      if context.start <= pair.close_range.start && pair.close_range.end <= context.end {
        let anchors = buffer_snapshot.anchor_range_inside(pair.close_range);
        ranges.push(
          multi_buffer_snapshot.anchor_in_buffer(anchors.start)?
            ..multi_buffer_snapshot.anchor_in_buffer(anchors.end)?,
        );
      };

      Some((color_index % accents_count, ranges))
    })
    .collect()
}
