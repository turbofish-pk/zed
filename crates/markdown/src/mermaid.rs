use collections::HashMap;
use gpui::{
  Animation, AnimationExt, AnyElement, App, ClipboardItem, Context, Entity, ImageSource, ParsedSvg,
  RenderImage, SMOOTH_SVG_SCALE_FACTOR, ScrollDelta, ScrollEvent, ScrollHandle, Size, Stateful,
  StyledText, Task, Window, img, pulsating_between, size,
};
use std::collections::BTreeMap;
use std::ops::Range;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use ui::{CopyButton, ScrollAxes, Scrollbars, TintColor, Tooltip, WithScrollbar, prelude::*};

use crate::parser::{CodeBlockKind, MarkdownEvent, MarkdownTag};
use settings::Settings as _;
use theme_settings::ThemeSettings;

use super::{CopyButtonVisibility, Markdown, MarkdownStyle, MermaidZoomCallback, ParsedMarkdown};

type MermaidDiagramCache = HashMap<ParsedMarkdownMermaidDiagramContents, Arc<CachedMermaidDiagram>>;

/// Per scroll tick, zoom changes by 10 percentage points, regardless of how
/// large a delta the platform reports for the tick.
const MERMAID_ZOOM_STEP: f32 = 0.1;
/// How many pixels of precise (trackpad) scroll make up one zoom tick.
const PIXELS_PER_ZOOM_TICK: f32 = 20.0;

#[derive(Clone, Debug)]
pub(crate) struct ParsedMarkdownMermaidDiagram {
  pub(crate) content_range: Range<usize>,
  pub(crate) contents: ParsedMarkdownMermaidDiagramContents,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ParsedMarkdownMermaidDiagramContents {
  pub(crate) contents: SharedString,
  pub(crate) scale: u32,
}

#[derive(Default, Clone)]
pub(crate) struct MermaidState {
  cache: MermaidDiagramCache,
  order: Vec<ParsedMarkdownMermaidDiagramContents>,
}

struct CachedMermaidDiagram {
  render_image: Arc<OnceLock<anyhow::Result<Arc<RenderImage>>>>,
  parsed_svg: Arc<OnceLock<Arc<ParsedSvg>>>,
  /// The previous raster shown while `render_image` is pending, along with
  /// the scale it was rasterized at, so it can be displayed at the same
  /// logical size as the raster that will replace it.
  fallback_image: Option<(Arc<RenderImage>, f32)>,
  /// The scale, relative to the diagram's natural size, that `render_image`
  /// was (or is being) rasterized at.
  rasterized_scale: f32,
  _task: Task<()>,
}

impl MermaidState {
  pub(crate) fn clear(&mut self, cx: &mut App) {
    for cached in self.cache.values() {
      cached.drop_images(cx);
    }
    self.cache.clear();
    self.order.clear();
  }

  fn get_fallback_image(
    idx: usize,
    old_order: &[ParsedMarkdownMermaidDiagramContents],
    new_order_len: usize,
    cache: &MermaidDiagramCache,
  ) -> Option<(Arc<RenderImage>, f32)> {
    if old_order.len() != new_order_len {
      return None;
    }

    old_order.get(idx).and_then(|old_content| {
      cache.get(old_content).and_then(|old_cached| {
        old_cached
          .render_image
          .get()
          .and_then(|result| result.as_ref().ok().cloned())
          .map(|image| (image, old_cached.rasterized_scale))
          .or_else(|| old_cached.fallback_image.clone())
      })
    })
  }

  pub(crate) fn update(
    &mut self,
    parsed: &ParsedMarkdown,
    zoom_for_offset: impl Fn(usize) -> f32,
    cx: &mut Context<Markdown>,
  ) {
    let mut new_order = Vec::new();
    let mut source_offsets = Vec::new();
    for (source_offset, mermaid_diagram) in parsed.mermaid_diagrams.iter() {
      new_order.push(mermaid_diagram.contents.clone());
      source_offsets.push(*source_offset);
    }

    for (idx, new_content) in new_order.iter().enumerate() {
      if !self.cache.contains_key(new_content) {
        let fallback = Self::get_fallback_image(idx, &self.order, new_order.len(), &self.cache);
        // The cache is keyed by contents, so duplicate diagrams share
        // one entry; if duplicates have different zooms, the zoom of
        // the first occurrence wins.
        let zoom = source_offsets
          .get(idx)
          .copied()
          .map_or(1.0, &zoom_for_offset);
        self.cache.insert(
          new_content.clone(),
          Arc::new(CachedMermaidDiagram::new(
            new_content.clone(),
            zoom,
            fallback,
            cx,
          )),
        );
      }
    }

    let new_order_set: std::collections::HashSet<_> = new_order.iter().cloned().collect();
    self.cache.retain(|content, cached| {
      let keep = new_order_set.contains(content);
      if !keep {
        cached.drop_images(cx);
      }
      keep
    });
    self.order = new_order;
  }

  pub(crate) fn has_pending_render(&self) -> bool {
    self
      .cache
      .values()
      .any(|cached| cached.render_image.get().is_none())
  }

  /// The natural (zoom 1.0) logical size of a diagram, if any raster of it
  /// is available to recover the size from.
  pub(crate) fn natural_size(
    &self,
    contents: &ParsedMarkdownMermaidDiagramContents,
  ) -> Option<Size<Pixels>> {
    let cached = self.cache.get(contents)?;
    let (image, rasterized_scale) = cached
      .render_image
      .get()
      .and_then(|result| result.as_ref().ok())
      .map(|image| (image.clone(), cached.rasterized_scale))
      .or_else(|| cached.fallback_image.clone())?;
    Some(mermaid_base_size(&image, rasterized_scale))
  }

  /// Re-rasterizes a single cached diagram at exactly the scale it is
  /// displayed at (`zoom` times its natural size), reusing the cached
  /// parsed SVG so that neither mermaid layout nor SVG parsing is re-run.
  /// No-ops if the entry is already rasterized at the target scale, or if
  /// it's still mid-render or failed to render.
  pub(crate) fn rerasterize_diagram(
    &mut self,
    contents: &ParsedMarkdownMermaidDiagramContents,
    zoom: f32,
    cx: &mut Context<Markdown>,
  ) {
    let Some(cached) = self.cache.get_mut(contents) else {
      return;
    };
    let Some(parsed_svg) = cached.parsed_svg.get().cloned() else {
      return;
    };
    let target_scale = zoom;
    if (cached.rasterized_scale - target_scale).abs() < 0.001 {
      return;
    }
    // The old render image lives on as the new entry's fallback, so it
    // must not be dropped here; the old fallback is no longer painted.
    // If the old raster is still pending, keep showing the previous
    // fallback so the diagram doesn't disappear mid-zoom.
    let new_fallback = match cached
      .render_image
      .get()
      .and_then(|result| result.as_ref().ok())
    {
      Some(image) => {
        if let Some((old_fallback, _)) = cached.fallback_image.clone() {
          cx.drop_image(old_fallback, None);
        }
        Some((image.clone(), cached.rasterized_scale))
      }
      None => cached.fallback_image.clone(),
    };
    let scale_factor = contents.scale as f32 / 100.0 * target_scale;
    *cached = Arc::new(CachedMermaidDiagram::new_from_parsed(
      parsed_svg,
      scale_factor,
      target_scale,
      new_fallback,
      cx,
    ));
  }
}

impl CachedMermaidDiagram {
  fn new(
    contents: ParsedMarkdownMermaidDiagramContents,
    zoom: f32,
    fallback_image: Option<(Arc<RenderImage>, f32)>,
    cx: &mut Context<Markdown>,
  ) -> Self {
    let render_image = Arc::new(OnceLock::<anyhow::Result<Arc<RenderImage>>>::new());
    let parsed_svg = Arc::new(OnceLock::<Arc<ParsedSvg>>::new());
    let svg_renderer = cx.svg_renderer();
    let mermaid_theme = build_mermaid_theme(cx);

    let task = cx.spawn({
      let render_image = render_image.clone();
      let parsed_svg = parsed_svg.clone();
      let fallback_image = fallback_image.clone();
      async move |this, cx| {
        let value = cx
          .background_spawn(async move {
            let svg_string = mermaid_render::render_to_svg(&contents.contents, &mermaid_theme)?;
            let tree = svg_renderer
              .parse_svg(svg_string.as_bytes())
              .map_err(|error| anyhow::anyhow!("{error}"))?;
            let tree = Arc::new(tree);
            let scale = contents.scale as f32 / 100.0 * zoom;
            let image = svg_renderer
              .render_parsed(&tree, scale)
              .map_err(|error| anyhow::anyhow!("{error}"))?;
            anyhow::Ok((tree, image))
          })
          .await;
        let value = match value {
          Ok((tree, image)) => {
            parsed_svg.set(tree).ok();
            Ok(image)
          }
          Err(error) => Err(error),
        };
        render_image.set(value).ok();
        Self::on_render_complete(this, fallback_image, cx);
      }
    });

    Self {
      render_image,
      parsed_svg,
      fallback_image,
      rasterized_scale: zoom,
      _task: task,
    }
  }

  fn new_from_parsed(
    parsed_svg: Arc<ParsedSvg>,
    scale_factor: f32,
    rasterized_scale: f32,
    fallback_image: Option<(Arc<RenderImage>, f32)>,
    cx: &mut Context<Markdown>,
  ) -> Self {
    let render_image = Arc::new(OnceLock::<anyhow::Result<Arc<RenderImage>>>::new());
    let parsed_svg_cell = Arc::new(OnceLock::new());
    parsed_svg_cell.set(parsed_svg.clone()).ok();
    let svg_renderer = cx.svg_renderer();

    let task = cx.spawn({
      let render_image = render_image.clone();
      let fallback_image = fallback_image.clone();
      async move |this, cx| {
        let value = cx
          .background_spawn(async move {
            svg_renderer
              .render_parsed(&parsed_svg, scale_factor)
              .map_err(|error| anyhow::anyhow!("{error}"))
          })
          .await;
        render_image.set(value).ok();
        Self::on_render_complete(this, fallback_image, cx);
      }
    });

    Self {
      render_image,
      parsed_svg: parsed_svg_cell,
      fallback_image,
      rasterized_scale,
      _task: task,
    }
  }

  fn on_render_complete(
    this: gpui::WeakEntity<Markdown>,
    fallback_image: Option<(Arc<RenderImage>, f32)>,
    cx: &mut gpui::AsyncApp,
  ) {
    this
      .update(cx, |_, cx| {
        // The fallback will no longer be painted now that the real render
        // is available, so its GPU texture can be released.
        if let Some((fallback_image, _)) = fallback_image {
          cx.drop_image(fallback_image, None);
        }
        cx.notify();
      })
      .ok();
  }

  fn drop_images(&self, cx: &mut App) {
    if let Some(Ok(render_image)) = self.render_image.get() {
      cx.drop_image(render_image.clone(), None);
    }
    if let Some((fallback_image, _)) = self.fallback_image.clone() {
      cx.drop_image(fallback_image, None);
    }
  }
}

/// Merman has somewhat limited text measurement capabilities.
///
/// When it doesn't have metrics for any of the specified fonts, it chooses a
/// fairly narrow width, which causes visible overflow. Adding `sans-serif`
/// allows it to fall back to a more conservative (i.e. wider) measurement.
///
/// This isn't perfect - very wide fonts will likely still cause overflow. A
/// proper fix would involve somehow piping `resvg`'s actual measurements into
/// `merman`, but that is a lot of work for a fairly uncommon edge case.
fn mermaid_font_family(font_family: &str) -> String {
  let font_family = gpui::font_name_with_fallbacks(font_family, "system-ui");
  if font_family
    .split(',')
    .any(|family| family.trim().eq_ignore_ascii_case("sans-serif"))
  {
    font_family.to_string()
  } else {
    format!("{font_family}, sans-serif")
  }
}

fn build_mermaid_theme(cx: &Context<Markdown>) -> mermaid_render::MermaidTheme {
  let colors = cx.theme().colors();
  let theme_settings = ThemeSettings::get_global(cx);
  let is_dark = !cx.theme().appearance.is_light();

  let players = cx.theme().players();
  let git_branch_colors = std::array::from_fn(|i| players.0[i % players.0.len()].cursor);
  let git_branch_label_colors = git_branch_colors.map(mermaid_render::text_color_for_background);

  mermaid_render::MermaidTheme {
    dark_mode: is_dark,
    font_family: mermaid_font_family(theme_settings.mermaid_font_family().as_ref()),
    background: colors.editor_background,
    primary_color: colors.surface_background,
    primary_text_color: colors.text,
    primary_border_color: colors.border,
    secondary_color: colors.element_background,
    tertiary_color: colors.ghost_element_hover,
    line_color: colors.border,
    text_color: colors.text,
    edge_label_background: colors.editor_background,
    cluster_background: colors.panel_background,
    cluster_border: colors.border_variant,
    note_background: colors.surface_background,
    note_border: colors.border_variant,
    actor_background: colors.element_background,
    actor_border: colors.border,
    activation_background: colors.ghost_element_hover,
    activation_border: colors.border,
    git_branch_colors,
    git_branch_label_colors,
    er_attr_bg_odd: colors.surface_background,
    er_attr_bg_even: colors.element_background,
    error_color: cx.theme().status().error,
    warning_color: cx.theme().status().warning,
    accent_colors: players
      .0
      .iter()
      .map(|player| mermaid_render::AccentColor {
        foreground: player.cursor,
        background: player.background,
      })
      .collect(),
  }
}

fn parse_mermaid_info(info: &str) -> Option<u32> {
  let mut parts = info.split_whitespace();
  if parts.next()? != "mermaid" {
    return None;
  }

  Some(
    parts
      .next()
      .and_then(|scale| scale.parse().ok())
      .unwrap_or(100)
      .clamp(10, 500),
  )
}

/// We deliberately block rendering of some diagram types, even though `merman`
/// supports them, because we have not yet written custom CSS to ensure text is
/// readable.
fn is_supported_diagram_type(source: &str) -> bool {
  /// If updating this list, also update the system prompt!
  const SUPPORTED_PREFIXES: &[&str] = &[
    "flowchart",
    "graph",
    "sequenceDiagram",
    "classDiagram",
    "stateDiagram",
    "stateDiagram-v2",
    "erDiagram",
    "gantt",
    "pie",
    "gitGraph",
    "mindmap",
    "timeline",
    "quadrantChart",
    "xychart-beta",
    "journey",
  ];
  let first_token = source
    .trim_start()
    .split(|c: char| c.is_whitespace() || c == '\n')
    .next()
    .unwrap_or("");
  SUPPORTED_PREFIXES
    .iter()
    .any(|prefix| first_token.eq_ignore_ascii_case(prefix))
}

pub(crate) fn extract_mermaid_diagrams(
  source: &str,
  events: &[(Range<usize>, MarkdownEvent)],
) -> BTreeMap<usize, ParsedMarkdownMermaidDiagram> {
  let mut mermaid_diagrams = BTreeMap::default();

  for (source_range, event) in events {
    let MarkdownEvent::Start(MarkdownTag::CodeBlock { kind, metadata }) = event else {
      continue;
    };
    if !metadata.is_fenced_closed {
      continue;
    }
    let scale = match kind {
      CodeBlockKind::FencedLang(info) => match parse_mermaid_info(info.as_ref()) {
        Some(scale) => scale,
        None => continue,
      },
      CodeBlockKind::FencedSrc(path_range) => {
        let path = Path::new(path_range.path.as_ref());
        match path.extension().and_then(|ext| ext.to_str()) {
          Some("mermaid" | "mmd") => 100,
          _ => continue,
        }
      }
      _ => continue,
    };

    let contents = source[metadata.content_range.clone()]
      .strip_suffix('\n')
      .unwrap_or(&source[metadata.content_range.clone()])
      .to_string();
    if !is_supported_diagram_type(&contents) {
      continue;
    }
    mermaid_diagrams.insert(
      source_range.start,
      ParsedMarkdownMermaidDiagram {
        content_range: metadata.content_range.clone(),
        contents: ParsedMarkdownMermaidDiagramContents {
          contents: contents.into(),
          scale,
        },
      },
    );
  }

  mermaid_diagrams
}

/// The natural (zoom 1.0, unfitted) logical size of a diagram, recovered from
/// one of its rasters.
///
/// Mermaid rasters always come from `SvgRenderer::render_parsed`, whose images
/// have a device scale of [`SMOOTH_SVG_SCALE_FACTOR`]; dividing by it and by
/// the scale the raster was made at recovers the natural size, regardless of
/// which raster is currently cached.
fn mermaid_base_size(image: &RenderImage, rasterized_scale: f32) -> Size<Pixels> {
  let device_size = image.size(0);
  let device_scale = SMOOTH_SVG_SCALE_FACTOR * rasterized_scale;
  size(
    px(device_size.width.0 as f32 / device_scale),
    px(device_size.height.0 as f32 / device_scale),
  )
}

fn mermaid_display_size(base_size: Size<Pixels>, display_scale: f32) -> Size<Pixels> {
  size(
    base_size.width * display_scale,
    base_size.height * display_scale,
  )
}

/// The number of zoom ticks represented by a scroll-wheel event.
///
/// A discrete wheel notch arrives as one `Lines` event whose magnitude varies
/// by platform and device (1 or 3 lines are both common), so the delta is
/// clamped to a single tick: every notch is exactly one step. Precise (pixel)
/// deltas from trackpads arrive as many small events, each contributing a
/// fractional tick, so a gesture zooms smoothly at the same overall rate.
fn mermaid_zoom_ticks(delta: ScrollDelta) -> f32 {
  match delta {
    ScrollDelta::Lines(lines) => lines.y,
    ScrollDelta::Pixels(pixels) => f32::from(pixels.y) / PIXELS_PER_ZOOM_TICK,
  }
  .clamp(-1.0, 1.0)
}

fn on_mermaid_zoom_scroll(
  markdown: Entity<Markdown>,
  source_offset: usize,
  on_zoom: Option<MermaidZoomCallback>,
) -> impl Fn(&ScrollEvent, &mut Window, &mut App) + 'static {
  move |event, window, cx| {
    if !(event.modifiers.control || event.modifiers.platform) {
      return;
    }
    let scroll_ticks = mermaid_zoom_ticks(event.delta);
    if scroll_ticks != 0.0 {
      let zoom_changed = markdown.update(cx, |markdown, cx| {
        let current_zoom = markdown.mermaid_zoom_level(source_offset);
        let new_zoom = current_zoom + scroll_ticks * MERMAID_ZOOM_STEP;
        markdown.set_mermaid_zoom_level(source_offset, new_zoom, cx);
        markdown.mermaid_zoom_level(source_offset) != current_zoom
      });
      // Only notify when the zoom actually changed. A no-op zoom (e.g.
      // clamped at the min/max) must not pause tail-following, since that
      // would disable following while still at the bottom.
      if zoom_changed && let Some(on_zoom) = &on_zoom {
        on_zoom(window, cx);
      }
    }
    cx.stop_propagation();
  }
}

pub(crate) fn render_mermaid_diagram(
  parsed: &ParsedMarkdownMermaidDiagram,
  mermaid_state: &MermaidState,
  style: &MarkdownStyle,
  markdown: Entity<Markdown>,
  source_offset: usize,
  showing_code: bool,
  zoom: f32,
  copy_button_visibility: CopyButtonVisibility,
  on_zoom: Option<MermaidZoomCallback>,
  window: &mut Window,
  cx: &mut App,
) -> AnyElement {
  let cached = mermaid_state.cache.get(&parsed.contents);
  let render_result = cached.and_then(|cached| cached.render_image.get());
  let show_interactive = copy_button_visibility != CopyButtonVisibility::Hidden;

  let code = parsed.contents.contents.clone();

  let mut container = div().group("code_block").relative().w_full().rounded_lg();
  container.style().refine(&style.code_block);

  match render_result {
    Some(Ok(render_image)) => {
      let body = if showing_code {
        render_mermaid_code_view(&parsed.contents.contents)
      } else {
        let rasterized_scale = cached.map_or(1.0, |cached| cached.rasterized_scale);
        let image_element = img(ImageSource::Render(render_image.clone()))
          .with_fallback(|| Label::new("Failed to Load Mermaid Diagram").into_any_element());
        let scroll_handle = markdown.update(cx, |markdown, _| {
          markdown.mermaid_scroll_handle(source_offset)
        });
        let base_size = mermaid_base_size(render_image, rasterized_scale);
        let display_size = mermaid_display_size(base_size, zoom);
        let body = mermaid_scroll_container(
          markdown.clone(),
          source_offset,
          &scroll_handle,
          on_zoom.clone(),
        )
        .child(image_element.w(display_size.width).h(display_size.height))
        .into_any_element();
        with_mermaid_horizontal_scrollbar(source_offset, &scroll_handle, body, window, cx)
      };

      container
        .when(show_interactive, |container| {
          container.child(render_mermaid_tab_header(
            source_offset,
            showing_code,
            markdown.clone(),
          ))
        })
        .child(body)
        .when(show_interactive, |container| {
          container.child(render_mermaid_overlay_controls(
            source_offset,
            code.to_string(),
            (!showing_code && zoom != 1.0).then_some(zoom),
            markdown,
            on_zoom,
          ))
        })
        .into_any_element()
    }
    Some(Err(_)) => {
      // Render failed — show the source code without tabs
      container
        .child(render_mermaid_code_view(&parsed.contents.contents))
        .when(show_interactive, |container| {
          container.child(render_mermaid_overlay_controls(
            source_offset,
            code.to_string(),
            None,
            markdown,
            on_zoom,
          ))
        })
        .into_any_element()
    }
    None => {
      // Still rendering
      if let Some((fallback, fallback_scale)) =
        cached.and_then(|cached| cached.fallback_image.clone())
      {
        let fallback_element = img(ImageSource::Render(fallback.clone())).with_fallback(|| {
          div()
            .child(Label::new("Failed to load mermaid diagram"))
            .into_any_element()
        });
        let scroll_handle = markdown.update(cx, |markdown, _| {
          markdown.mermaid_scroll_handle(source_offset)
        });
        let base_size = mermaid_base_size(&fallback, fallback_scale);
        let display_size = mermaid_display_size(base_size, zoom);
        // The fallback is the prior raster shown at the new size while
        // the sharper one renders; keep it static so swapping rasters
        // (e.g. on zoom) is seamless rather than flashing.
        let inner = mermaid_scroll_container(
          markdown.clone(),
          source_offset,
          &scroll_handle,
          on_zoom.clone(),
        )
        .child(
          fallback_element
            .w(display_size.width)
            .h(display_size.height),
        )
        .into_any_element();
        let body =
          with_mermaid_horizontal_scrollbar(source_offset, &scroll_handle, inner, window, cx);
        container
          .when(show_interactive, |container| {
            container.child(render_mermaid_tab_header(
              source_offset,
              showing_code,
              markdown.clone(),
            ))
          })
          .child(body)
          .when(show_interactive, |container| {
            container.child(render_mermaid_overlay_controls(
              source_offset,
              code.to_string(),
              (zoom != 1.0).then_some(zoom),
              markdown,
              on_zoom,
            ))
          })
          .into_any_element()
      } else {
        // No fallback — show the code so the user has something to look at
        container
          .child(render_mermaid_code_view(&parsed.contents.contents))
          .child(
            div().absolute().top_1().right_2().child(
              Label::new("Rendering...")
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .with_animation(
                  "mermaid-loading-pulse",
                  Animation::new(Duration::from_secs(2))
                    .repeat()
                    .with_easing(pulsating_between(0.4, 0.8)),
                  |label, delta| label.alpha(delta),
                ),
            ),
          )
          .when(show_interactive, |container| {
            container.child(render_mermaid_overlay_controls(
              source_offset,
              code.to_string(),
              None,
              markdown,
              on_zoom,
            ))
          })
          .into_any_element()
      }
    }
  }
}

/// The horizontal scroll container wrapping a mermaid raster. The element id
/// and the [`ScrollHandle`] identity are both stable per source offset so the
/// scroll position survives raster swaps and re-rasters. Diagrams keep their
/// natural (zoomed) size and scroll instead of being crushed via max_w_full
/// (#61051).
fn mermaid_scroll_container(
  markdown: Entity<Markdown>,
  source_offset: usize,
  scroll_handle: &ScrollHandle,
  on_zoom: Option<MermaidZoomCallback>,
) -> Stateful<Div> {
  let mut container = div()
    .id(ElementId::named_usize(
      "mermaid-diagram-body",
      source_offset,
    ))
    .w_full()
    .overflow_x_scroll()
    .track_scroll(scroll_handle)
    .on_scroll(on_mermaid_zoom_scroll(markdown, source_offset, on_zoom));
  // Without this, gpui maps vertical wheel deltas onto the x axis for
  // x-only scroll containers (see `paint_scroll_listener` in gpui's div),
  // hijacking plain vertical scrolls. Restricting to the actual axis lets
  // vertical deltas propagate to the surrounding view, and means a
  // ctrl+vertical-scroll (zoom gesture) never moves this container either,
  // since its delta.x is zero.
  container.style().restrict_scroll_to_axis = Some(true);
  container
}

fn with_mermaid_horizontal_scrollbar(
  source_offset: usize,
  scroll_handle: &ScrollHandle,
  body: AnyElement,
  window: &mut Window,
  cx: &mut App,
) -> AnyElement {
  // Always show the scrollbar (rather than autohiding) so it's clear a
  // zoomed diagram can be scrolled. It only appears when the content
  // actually overflows, since the thumb isn't drawn when there's nothing to
  // scroll.
  let scrollbars = Scrollbars::always_visible(ScrollAxes::Horizontal)
    .id(("mermaid-diagram-scrollbar", source_offset))
    .tracked_scroll_handle(scroll_handle)
    .with_track_along(
      ScrollAxes::Horizontal,
      cx.theme().colors().editor_background,
    )
    .notify_content();

  div()
    .w_full()
    .custom_scrollbars(scrollbars, window, cx)
    .child(body)
    .into_any_element()
}

fn render_mermaid_tab_header(
  source_offset: usize,
  showing_code: bool,
  markdown: Entity<Markdown>,
) -> impl IntoElement {
  let preview_id = ElementId::NamedChild(
    Arc::new(ElementId::from((
      "mermaid-tab-preview",
      markdown.entity_id(),
    ))),
    source_offset.to_string().into(),
  );
  let code_id = ElementId::NamedChild(
    Arc::new(ElementId::from(("mermaid-tab-code", markdown.entity_id()))),
    source_offset.to_string().into(),
  );
  let preview_markdown = markdown.clone();
  let code_markdown = markdown;

  h_flex()
    .gap_0p5()
    .mb_2p5()
    .child(
      Button::new(preview_id, "Preview")
        .label_size(LabelSize::Small)
        .selected_style(ButtonStyle::Tinted(TintColor::Accent))
        .toggle_state(!showing_code)
        .on_click(move |_event, _window, cx| {
          preview_markdown.update(cx, |md, cx| {
            if md.is_mermaid_showing_code(source_offset) {
              md.toggle_mermaid_tab(source_offset);
              cx.notify();
            }
          });
        }),
    )
    .child(
      Button::new(code_id, "Code")
        .label_size(LabelSize::Small)
        .selected_style(ButtonStyle::Tinted(TintColor::Accent))
        .toggle_state(showing_code)
        .on_click(move |_event, _window, cx| {
          code_markdown.update(cx, |md, cx| {
            if !md.is_mermaid_showing_code(source_offset) {
              md.toggle_mermaid_tab(source_offset);
              cx.notify();
            }
          });
        }),
    )
}

/// The overlay controls anchored to the top-right corner of a diagram: an
/// optional "Zoom NNN%" readout with a reset button (shown only while zoomed
/// away from the natural size) followed by the hover-revealed copy button.
fn render_mermaid_overlay_controls(
  source_offset: usize,
  code: String,
  zoom: Option<f32>,
  markdown: Entity<Markdown>,
  on_zoom: Option<MermaidZoomCallback>,
) -> impl IntoElement {
  h_flex()
    .absolute()
    .top_1()
    .right_1()
    .gap_0p5()
    .when_some(zoom, |this, zoom| {
      this.child(render_mermaid_zoom_indicator(
        source_offset,
        zoom,
        markdown.clone(),
        on_zoom,
      ))
    })
    .child(render_mermaid_copy_button(source_offset, code, markdown))
}

/// A "Zoom NNN%" readout paired with a reset button, styled like the adjacent
/// copy button. Shown only while the diagram is zoomed away from its natural
/// size, so it doubles as an affordance that the zoom can be reset.
fn render_mermaid_zoom_indicator(
  source_offset: usize,
  zoom: f32,
  markdown: Entity<Markdown>,
  on_zoom: Option<MermaidZoomCallback>,
) -> impl IntoElement {
  let percentage = (zoom * 100.0).round() as i32;

  h_flex()
    .gap_0p5()
    .child(
      Label::new(format!("Zoom {percentage}%"))
        .size(LabelSize::Small)
        .color(Color::Muted),
    )
    .child(
      IconButton::new(
        ElementId::named_usize("mermaid-zoom-reset", source_offset),
        IconName::RotateCcw,
      )
      .icon_size(IconSize::Small)
      .icon_color(Color::Muted)
      .tooltip(Tooltip::text("Reset Zoom"))
      .on_click(move |_event, window, cx| {
        let zoom_changed = markdown.update(cx, |markdown, cx| {
          let current_zoom = markdown.mermaid_zoom_level(source_offset);
          markdown.set_mermaid_zoom_level(source_offset, 1.0, cx);
          markdown.mermaid_zoom_level(source_offset) != current_zoom
        });
        if zoom_changed && let Some(on_zoom) = &on_zoom {
          on_zoom(window, cx);
        }
      }),
    )
}

fn render_mermaid_copy_button(
  source_offset: usize,
  code: String,
  markdown: Entity<Markdown>,
) -> impl IntoElement {
  let id = ElementId::NamedChild(
    Arc::new(ElementId::from(("copy-mermaid-code", markdown.entity_id()))),
    source_offset.to_string().into(),
  );

  CopyButton::new(id.clone(), code.clone())
    .visible_on_hover("code_block")
    .custom_on_click({
      move |_window, cx| {
        let id = id.clone();
        markdown.update(cx, |this, cx| {
          this.copied_code_blocks.insert(id.clone());
          cx.write_to_clipboard(ClipboardItem::new_string(code.clone()));
          cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            cx.update(|cx| {
              this.update(cx, |this, cx| {
                this.copied_code_blocks.remove(&id);
                cx.notify();
              })
            })
            .ok();
          })
          .detach();
        });
      }
    })
}

fn render_mermaid_code_view(contents: &SharedString) -> AnyElement {
  div()
    .w_full()
    .child(StyledText::new(contents.clone()))
    .into_any_element()
}
