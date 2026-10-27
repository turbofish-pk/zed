use std::{
    ops::{Range, RangeInclusive},
    sync::Arc,
};

use buffer_diff::{BufferDiff, BufferDiffSnapshot, DiffHunkStatus};
use collections::HashMap;

use fs::Fs;
use gpui::{
    Action, AnyElement, Entity, EventEmitter, Focusable, Font, Pixels, Subscription, WeakEntity,
    canvas, prelude::*,
};

use language::{Buffer, Capability, HighlightedText};
use multi_buffer::{
    Anchor, AnchorRangeExt as _, BufferOffset, ExcerptRange, ExpandExcerptDirection, MultiBuffer,
    MultiBufferPoint, MultiBufferSnapshot, PathKey,
};
use project::Project;
use rope::Point;
use settings::{DiffViewStyle, SeedQuerySetting, Settings, SettingsStore, update_settings_file};
use text::{Bias, BufferId, OffsetRangeExt as _, Patch, ToPoint as _};

use crate::{
    display_map::CompanionExcerptPatch,
    element::SplitSide,
    split_editor_view::{SplitEditorState, SplitEditorView},
};
use ui::{Toggleable as _, Tooltip, prelude::*, render_modifiers};
use workspace::{
    ActivatePaneLeft, ActivatePaneRight, Item, ToolbarItemLocation, Workspace,
    item::{ItemBufferKind, ItemEvent, SaveOptions, TabContentParams},
    searchable::{SearchEvent, SearchToken, SearchableItem, SearchableItemHandle},
};

use crate::{
    Autoscroll, DefaultDiffHunkRenderer, DiffHunkRenderer, Editor, EditorEvent, EditorSettings,
    ToggleSoftWrap,
    actions::{DisableBreakpoint, EditLogBreakpoint, EnableBreakpoint, ToggleBreakpoint},
    display_map::Companion,
};
use zed_actions::OpenSettingsAt;

pub(crate) fn patches_for_lhs_range(
    rhs_snapshot: &MultiBufferSnapshot,
    lhs_snapshot: &MultiBufferSnapshot,
    lhs_bounds: Range<MultiBufferPoint>,
) -> Vec<CompanionExcerptPatch> {
    patches_for_range(
        lhs_snapshot,
        rhs_snapshot,
        lhs_bounds,
        |diff, range, buffer| diff.patch_for_base_text_range(range, buffer),
    )
}

pub(crate) fn patches_for_rhs_range(
    lhs_snapshot: &MultiBufferSnapshot,
    rhs_snapshot: &MultiBufferSnapshot,
    rhs_bounds: Range<MultiBufferPoint>,
) -> Vec<CompanionExcerptPatch> {
    patches_for_range(
        rhs_snapshot,
        lhs_snapshot,
        rhs_bounds,
        |diff, range, buffer| diff.patch_for_buffer_range(range, buffer),
    )
}

fn buffer_range_to_base_text_range(
    rhs_range: &Range<Point>,
    diff_snapshot: &BufferDiffSnapshot,
    rhs_buffer_snapshot: &text::BufferSnapshot,
) -> Range<Point> {
    let start = diff_snapshot
        .buffer_point_to_base_text_range(Point::new(rhs_range.start.row, 0), rhs_buffer_snapshot)
        .start;
    let end = diff_snapshot
        .buffer_point_to_base_text_range(Point::new(rhs_range.end.row, 0), rhs_buffer_snapshot)
        .end;
    let end_column = diff_snapshot.base_text().line_len(end.row);
    Point::new(start.row, 0)..Point::new(end.row, end_column)
}

fn translate_lhs_selections_to_rhs(
    selections_by_buffer: &HashMap<BufferId, (Vec<Range<BufferOffset>>, Option<u32>)>,
    splittable: &SplittableEditor,
    cx: &App,
) -> HashMap<Entity<Buffer>, (Vec<Range<BufferOffset>>, Option<u32>)> {
    let Some(lhs) = &splittable.lhs else {
        return HashMap::default();
    };
    let lhs_snapshot = lhs.multibuffer.read(cx).snapshot(cx);

    let mut translated: HashMap<Entity<Buffer>, (Vec<Range<BufferOffset>>, Option<u32>)> =
        HashMap::default();

    for (lhs_buffer_id, (ranges, scroll_offset)) in selections_by_buffer {
        let Some(diff) = lhs_snapshot.diff_for_buffer_id(*lhs_buffer_id) else {
            continue;
        };
        let rhs_buffer_id = diff.buffer_id();

        let Some(rhs_buffer) = splittable
            .rhs_editor
            .read(cx)
            .buffer()
            .read(cx)
            .buffer(rhs_buffer_id)
        else {
            continue;
        };

        let Some(diff) = splittable
            .rhs_editor
            .read(cx)
            .buffer()
            .read(cx)
            .diff_for(rhs_buffer_id)
        else {
            continue;
        };

        let diff_snapshot = diff.read(cx).snapshot(cx);
        let rhs_buffer_snapshot = rhs_buffer.read(cx).snapshot();
        let base_text_buffer = diff.read(cx).base_text_buffer();
        let base_text_snapshot = base_text_buffer.read(cx).snapshot();

        let translated_ranges: Vec<Range<BufferOffset>> = ranges
            .iter()
            .map(|range| {
                let start_point = base_text_snapshot.offset_to_point(range.start.0);
                let end_point = base_text_snapshot.offset_to_point(range.end.0);

                let rhs_start = diff_snapshot
                    .base_text_point_to_buffer_point(start_point, &rhs_buffer_snapshot);
                let rhs_end =
                    diff_snapshot.base_text_point_to_buffer_point(end_point, &rhs_buffer_snapshot);

                BufferOffset(rhs_buffer_snapshot.point_to_offset(rhs_start))
                    ..BufferOffset(rhs_buffer_snapshot.point_to_offset(rhs_end))
            })
            .collect();

        translated.insert(rhs_buffer, (translated_ranges, *scroll_offset));
    }

    translated
}

struct SplitLhsDiffHunkRenderer {
    splittable: WeakEntity<SplittableEditor>,
}

impl DiffHunkRenderer for SplitLhsDiffHunkRenderer {
    fn render_hunk_controls(
        &self,
        row: u32,
        status: &DiffHunkStatus,
        hunk_range: Range<Anchor>,
        is_created_file: bool,
        line_height: Pixels,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let Some(splittable) = self.splittable.upgrade() else {
            return gpui::Empty.into_any_element();
        };
        let renderer = splittable.read(cx).rhs_editor.read(cx).diff_hunk_renderer();
        renderer.render_hunk_controls(
            row,
            status,
            hunk_range,
            is_created_file,
            line_height,
            editor,
            window,
            cx,
        )
    }

    fn render_hunk_as_staged(&self, status: &DiffHunkStatus, cx: &App) -> bool {
        let Some(splittable) = self.splittable.upgrade() else {
            return false;
        };
        let renderer = splittable.read(cx).rhs_editor.read(cx).diff_hunk_renderer();
        renderer.render_hunk_as_staged(status, cx)
    }
}

fn patches_for_range<F>(
    source_snapshot: &MultiBufferSnapshot,
    target_snapshot: &MultiBufferSnapshot,
    source_bounds: Range<MultiBufferPoint>,
    translate_fn: F,
) -> Vec<CompanionExcerptPatch>
where
    F: Fn(&BufferDiffSnapshot, RangeInclusive<Point>, &text::BufferSnapshot) -> Patch<Point>,
{
    struct PendingExcerpt<'a> {
        source_buffer_snapshot: &'a language::BufferSnapshot,
        source_excerpt_range: ExcerptRange<text::Anchor>,
        buffer_point_range: Range<Point>,
    }

    let mut result = Vec::new();
    let mut current_buffer_id: Option<BufferId> = None;
    let mut pending_excerpts: Vec<PendingExcerpt<'_>> = Vec::new();
    let mut union_context_start: Option<Point> = None;
    let mut union_context_end: Option<Point> = None;

    let flush_buffer = |pending: &mut Vec<PendingExcerpt>,
                        union_start: Point,
                        union_end: Point,
                        result: &mut Vec<CompanionExcerptPatch>| {
        let Some(first) = pending.first() else {
            return;
        };

        let source_buffer_id = first.source_buffer_snapshot.remote_id();
        let Some(diff) = source_snapshot.diff_for_buffer_id(source_buffer_id) else {
            pending.clear();
            return;
        };
        let source_is_lhs = source_buffer_id == diff.base_text().remote_id();
        let target_buffer_id = if source_is_lhs {
            diff.buffer_id()
        } else {
            diff.base_text().remote_id()
        };
        let Some(target_buffer) = target_snapshot.buffer_for_id(target_buffer_id) else {
            pending.clear();
            return;
        };
        let rhs_buffer = if source_is_lhs {
            target_buffer
        } else {
            &first.source_buffer_snapshot
        };

        let patch = translate_fn(diff, union_start..=union_end, rhs_buffer);

        let mut source_excerpts = source_snapshot
            .excerpts_for_buffer(source_buffer_id)
            .peekable();
        let mut target_excerpts = target_snapshot
            .excerpts_for_buffer(target_buffer_id)
            .peekable();

        for excerpt in pending.drain(..) {
            while let Some(source_excerpt_range) = source_excerpts.peek()
                && source_excerpt_range != &excerpt.source_excerpt_range
            {
                source_excerpts.next();
                target_excerpts.next();
            }
            if let Some(source_excerpt_range) = source_excerpts.peek()
                && let Some(target_excerpt_range) = target_excerpts.peek()
            {
                result.push(patch_for_excerpt(
                    source_snapshot,
                    target_snapshot,
                    &excerpt.source_buffer_snapshot,
                    target_buffer,
                    source_excerpt_range.clone(),
                    target_excerpt_range.clone(),
                    &patch,
                    excerpt.buffer_point_range,
                ));
            }
        }
    };

    for (buffer_snapshot, source_range, source_excerpt_range) in
        source_snapshot.range_to_buffer_ranges(source_bounds)
    {
        let buffer_id = buffer_snapshot.remote_id();

        if current_buffer_id != Some(buffer_id) {
            if let (Some(start), Some(end)) = (union_context_start.take(), union_context_end.take())
            {
                flush_buffer(&mut pending_excerpts, start, end, &mut result);
            }
            current_buffer_id = Some(buffer_id);
        }

        let buffer_point_range = source_range.to_point(&buffer_snapshot);
        let source_context_range = source_excerpt_range.context.to_point(&buffer_snapshot);

        union_context_start = Some(union_context_start.map_or(source_context_range.start, |s| {
            s.min(source_context_range.start)
        }));
        union_context_end = Some(union_context_end.map_or(source_context_range.end, |e| {
            e.max(source_context_range.end)
        }));

        pending_excerpts.push(PendingExcerpt {
            source_buffer_snapshot: buffer_snapshot,
            source_excerpt_range,
            buffer_point_range,
        });
    }

    if let (Some(start), Some(end)) = (union_context_start, union_context_end) {
        flush_buffer(&mut pending_excerpts, start, end, &mut result);
    }

    result
}

fn patch_for_excerpt(
    source_snapshot: &MultiBufferSnapshot,
    target_snapshot: &MultiBufferSnapshot,
    source_buffer_snapshot: &language::BufferSnapshot,
    target_buffer_snapshot: &language::BufferSnapshot,
    source_excerpt_range: ExcerptRange<text::Anchor>,
    target_excerpt_range: ExcerptRange<text::Anchor>,
    patch: &Patch<Point>,
    source_edited_range: Range<Point>,
) -> CompanionExcerptPatch {
    let source_buffer_range = source_excerpt_range
        .context
        .to_point(source_buffer_snapshot);
    let source_multibuffer_range = (source_snapshot
        .anchor_in_buffer(source_excerpt_range.context.start)
        .expect("buffer should exist in multibuffer")
        ..source_snapshot
            .anchor_in_buffer(source_excerpt_range.context.end)
            .expect("buffer should exist in multibuffer"))
        .to_point(source_snapshot);
    let target_buffer_range = target_excerpt_range
        .context
        .to_point(target_buffer_snapshot);
    let target_multibuffer_range = (target_snapshot
        .anchor_in_buffer(target_excerpt_range.context.start)
        .expect("buffer should exist in multibuffer")
        ..target_snapshot
            .anchor_in_buffer(target_excerpt_range.context.end)
            .expect("buffer should exist in multibuffer"))
        .to_point(target_snapshot);

    let edits = patch
        .edits()
        .iter()
        .skip_while(|edit| edit.old.end < source_buffer_range.start)
        .take_while(|edit| edit.old.start <= source_buffer_range.end)
        .map(|edit| {
            let clamped_source_start = edit.old.start.max(source_buffer_range.start);
            let clamped_source_end = edit.old.end.min(source_buffer_range.end);
            let source_multibuffer_start =
                source_multibuffer_range.start + (clamped_source_start - source_buffer_range.start);
            let source_multibuffer_end =
                source_multibuffer_range.start + (clamped_source_end - source_buffer_range.start);
            let clamped_target_start = edit
                .new
                .start
                .max(target_buffer_range.start)
                .min(target_buffer_range.end);
            let clamped_target_end = edit
                .new
                .end
                .max(target_buffer_range.start)
                .min(target_buffer_range.end);
            let target_multibuffer_start =
                target_multibuffer_range.start + (clamped_target_start - target_buffer_range.start);
            let target_multibuffer_end =
                target_multibuffer_range.start + (clamped_target_end - target_buffer_range.start);
            text::Edit {
                old: source_multibuffer_start..source_multibuffer_end,
                new: target_multibuffer_start..target_multibuffer_end,
            }
        });

    let edits = [text::Edit {
        old: source_multibuffer_range.start..source_multibuffer_range.start,
        new: target_multibuffer_range.start..target_multibuffer_range.start,
    }]
    .into_iter()
    .chain(edits);

    let mut merged_edits: Vec<text::Edit<Point>> = Vec::new();
    for edit in edits {
        if let Some(last) = merged_edits.last_mut() {
            if edit.new.start <= last.new.end || edit.old.start <= last.old.end {
                last.old.end = last.old.end.max(edit.old.end);
                last.new.end = last.new.end.max(edit.new.end);
                continue;
            }
        }
        merged_edits.push(edit);
    }

    let edited_range = source_multibuffer_range.start
        + (source_edited_range.start - source_buffer_range.start)
        ..source_multibuffer_range.start + (source_edited_range.end - source_buffer_range.start);

    let source_excerpt_end =
        source_multibuffer_range.start + (source_buffer_range.end - source_buffer_range.start);
    let target_excerpt_end =
        target_multibuffer_range.start + (target_buffer_range.end - target_buffer_range.start);

    CompanionExcerptPatch {
        patch: Patch::new(merged_edits),
        edited_range,
        source_excerpt_range: source_multibuffer_range.start..source_excerpt_end,
        target_excerpt_range: target_multibuffer_range.start..target_excerpt_end,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Action, Default)]
#[action(namespace = editor)]
pub struct ToggleSplitDiff;

/// Unified/split diff view toggle buttons, shared by the toolbars of every
/// diff view (project diff, branch diff, solo diff) so they can't drift apart.
#[derive(gpui::IntoElement)]
pub struct DiffStyleControls {
    splittable_editor: Entity<SplittableEditor>,
}

impl DiffStyleControls {
    pub fn new(splittable_editor: Entity<SplittableEditor>) -> Self {
        Self { splittable_editor }
    }

    fn set_diff_view_style(
        splittable_editor: &Entity<SplittableEditor>,
        diff_view_style: DiffViewStyle,
        window: &mut Window,
        cx: &mut App,
    ) {
        update_settings_file(<dyn Fs>::global(cx), cx, move |settings, _| {
            settings.editor.diff_view_style = Some(diff_view_style);
        });

        splittable_editor.update(cx, |editor, cx| {
            if editor.diff_view_style() != diff_view_style {
                editor.toggle_split(&ToggleSplitDiff, window, cx);
            }
        });
    }
}

impl RenderOnce for DiffStyleControls {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl gpui::IntoElement {
        let editor = self.splittable_editor.read(cx);
        let diff_view_style = editor.diff_view_style();
        let is_split_set = diff_view_style == DiffViewStyle::Split;
        let is_split_pending = is_split_set && !editor.is_split();
        let min_columns = EditorSettings::get_global(cx).minimum_split_diff_width as u32;

        let split_icon = if is_split_pending {
            IconName::DiffSplitAuto
        } else {
            IconName::DiffSplit
        };

        h_flex()
            .gap_1()
            .child(
                IconButton::new("diff-style-unified", IconName::DiffUnified)
                    .icon_size(IconSize::Small)
                    .toggle_state(diff_view_style == DiffViewStyle::Unified)
                    .tooltip(Tooltip::text("Unified"))
                    .on_click({
                        let splittable_editor = self.splittable_editor.clone();
                        move |_, window, cx| {
                            Self::set_diff_view_style(
                                &splittable_editor,
                                DiffViewStyle::Unified,
                                window,
                                cx,
                            );
                        }
                    }),
            )
            .child(
                IconButton::new("diff-style-split", split_icon)
                    .icon_size(IconSize::Small)
                    .toggle_state(is_split_set)
                    .tooltip(Tooltip::element(move |_, cx| {
                        let message = if is_split_pending {
                            format!("Split when wider than {} columns", min_columns).into()
                        } else {
                            SharedString::from("Split")
                        };

                        v_flex()
                            .child(message)
                            .child(
                                h_flex()
                                    .gap_0p5()
                                    .text_ui_sm(cx)
                                    .text_color(Color::Muted.color(cx))
                                    .children(render_modifiers(
                                        &gpui::Modifiers::secondary_key(cx),
                                        PlatformStyle::platform(),
                                        None,
                                        Some(TextSize::Small.rems(cx).into()),
                                        false,
                                    ))
                                    .child("click to change min width"),
                            )
                            .into_any_element()
                    }))
                    .on_click({
                        let splittable_editor = self.splittable_editor.clone();
                        move |_, window, cx| {
                            if window.modifiers().secondary(cx) {
                                window.dispatch_action(
                                    OpenSettingsAt {
                                        path: "minimum_split_diff_width".to_string(),
                                        target: None,
                                    }
                                    .boxed_clone(),
                                    cx,
                                );
                            } else {
                                Self::set_diff_view_style(
                                    &splittable_editor,
                                    DiffViewStyle::Split,
                                    window,
                                    cx,
                                );
                            }
                        }
                    }),
            )
    }
}

pub struct SplittableEditor {
    rhs_multibuffer: Entity<MultiBuffer>,
    rhs_editor: Entity<Editor>,
    lhs: Option<LhsEditor>,
    workspace: WeakEntity<Workspace>,
    split_state: Entity<SplitEditorState>,
    searched_side: Option<SplitSide>,
    /// The preferred diff style.
    diff_view_style: DiffViewStyle,
    /// True when the current width is below the minimum threshold for split
    /// mode, regardless of the current diff view style setting.
    too_narrow_for_split: bool,
    last_width: Option<Pixels>,
    _subscriptions: Vec<Subscription>,
}

struct LhsEditor {
    multibuffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    was_last_focused: bool,
    _subscriptions: Vec<Subscription>,
}

impl SplittableEditor {
    pub fn rhs_editor(&self) -> &Entity<Editor> {
        &self.rhs_editor
    }

    pub fn lhs_editor(&self) -> Option<&Entity<Editor>> {
        self.lhs.as_ref().map(|s| &s.editor)
    }

    pub fn update_editors(
        &self,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Editor, &mut Context<Editor>),
    ) {
        if let Some(lhs) = &self.lhs {
            lhs.editor.update(cx, &f);
        }
        self.rhs_editor.update(cx, &f);
    }

    pub fn diff_view_style(&self) -> DiffViewStyle {
        self.diff_view_style
    }

    pub fn is_split(&self) -> bool {
        self.lhs.is_some()
    }

    pub fn set_diff_hunk_renderer(
        &self,
        renderer: Option<Arc<dyn DiffHunkRenderer>>,
        cx: &mut Context<Self>,
    ) {
        self.rhs_editor.update(cx, |editor, cx| {
            editor.set_diff_hunk_renderer(renderer, cx);
        });
    }

    fn focused_side(&self) -> SplitSide {
        if let Some(lhs) = &self.lhs
            && lhs.was_last_focused
        {
            SplitSide::Left
        } else {
            SplitSide::Right
        }
    }

    pub fn focused_editor(&self) -> &Entity<Editor> {
        if let Some(lhs) = &self.lhs
            && lhs.was_last_focused
        {
            &lhs.editor
        } else {
            &self.rhs_editor
        }
    }

    pub fn new(
        style: DiffViewStyle,
        rhs_multibuffer: Entity<MultiBuffer>,
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let rhs_editor = cx.new(|cx| {
            let mut editor =
                Editor::for_multibuffer(rhs_multibuffer.clone(), Some(project.clone()), window, cx);
            editor.set_expand_all_diff_hunks(cx);
            editor.disable_runnables();
            editor.disable_code_lens(cx);
            editor.disable_inline_diagnostics();
            editor.disable_mouse_wheel_zoom();
            editor.set_minimap_visibility(crate::MinimapVisibility::Disabled, window, cx);
            editor.set_diff_hunk_renderer(Some(Arc::new(DefaultDiffHunkRenderer)), cx);
            editor
        });
        // TODO(split-diff) we might want to tag editor events with whether they came from rhs/lhs
        let subscriptions = vec![
            cx.subscribe(
                &rhs_editor,
                |this, _, event: &EditorEvent, cx| match event {
                    EditorEvent::ExpandExcerptsRequested {
                        excerpt_anchors,
                        lines,
                        direction,
                    } => {
                        this.expand_excerpts(
                            excerpt_anchors.iter().copied(),
                            *lines,
                            *direction,
                            cx,
                        );
                    }
                    _ => cx.emit(event.clone()),
                },
            ),
            cx.subscribe(&rhs_editor, |this, _, event: &SearchEvent, cx| {
                if this.searched_side.is_none() || this.searched_side == Some(SplitSide::Right) {
                    cx.emit(event.clone());
                }
            }),
            cx.observe_global_in::<SettingsStore>(window, move |this, window, cx| {
                let diff_view_style = EditorSettings::get_global(cx).diff_view_style;
                if this.diff_view_style() != diff_view_style {
                    this.toggle_split(&ToggleSplitDiff, window, cx);
                    cx.notify();
                }
            }),
        ];

        let this = cx.weak_entity();
        window.defer(cx, {
            let workspace = workspace.downgrade();
            let rhs_editor = rhs_editor.downgrade();
            move |window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        rhs_editor
                            .update(cx, |editor, cx| {
                                editor.added_to_workspace(workspace, window, cx);
                            })
                            .ok();
                    })
                    .ok();
                if style == DiffViewStyle::Split {
                    this.update(cx, |this, cx| {
                        this.split(window, cx);
                    })
                    .ok();
                }
            }
        });
        let split_state = cx.new(|cx| SplitEditorState::new(cx));
        Self {
            diff_view_style: style,
            rhs_editor,
            rhs_multibuffer,
            lhs: None,
            workspace: workspace.downgrade(),
            split_state,
            searched_side: None,
            too_narrow_for_split: false,
            last_width: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.lhs.is_some() {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = workspace.read(cx).project().clone();
        let all_paths = self.diff_paths(cx);
        if all_paths.is_empty() && !self.rhs_multibuffer.read(cx).is_empty() {
            return;
        }

        let rhs_has_headers = self.rhs_multibuffer.read(cx).snapshot(cx).show_headers();
        let lhs_multibuffer = cx.new(|cx| {
            let mut multibuffer = if !rhs_has_headers {
                MultiBuffer::without_headers(Capability::ReadOnly)
            } else {
                MultiBuffer::new(Capability::ReadOnly)
            };
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });

        let splittable = cx.weak_entity();
        let rhs_editor = self.rhs_editor.downgrade();
        let lhs_editor = cx.new(|cx| {
            let mut editor =
                Editor::for_multibuffer(lhs_multibuffer.clone(), Some(project.clone()), window, cx);
            editor.set_number_deleted_lines(true, cx);
            editor.set_delegate_expand_excerpts(true);
            editor.set_diff_hunk_renderer(
                Some(Arc::new(SplitLhsDiffHunkRenderer {
                    splittable: splittable.clone(),
                })),
                cx,
            );
            editor.set_diff_hunk_action_target(Some(rhs_editor));
            editor.set_delegate_open_excerpts(true);
            editor.set_show_vertical_scrollbar(false, cx);
            editor.disable_lsp_data();
            editor.disable_runnables();
            editor.disable_diagnostics(cx);
            editor.disable_mouse_wheel_zoom();
            editor.set_minimap_visibility(crate::MinimapVisibility::Disabled, window, cx);
            editor
        });

        let mut subscriptions = vec![cx.subscribe_in(
            &lhs_editor,
            window,
            |this, _, event: &EditorEvent, window, cx| match event {
                EditorEvent::ExpandExcerptsRequested {
                    excerpt_anchors,
                    lines,
                    direction,
                } => {
                    if let Some(lhs) = &this.lhs {
                        let rhs_snapshot = this.rhs_multibuffer.read(cx).snapshot(cx);
                        let lhs_snapshot = lhs.multibuffer.read(cx).snapshot(cx);
                        let rhs_anchors = excerpt_anchors
                            .iter()
                            .filter_map(|anchor| {
                                let (anchor, lhs_buffer) =
                                    lhs_snapshot.anchor_to_buffer_anchor(*anchor)?;
                                let diff = lhs_snapshot.diff_for_buffer_id(anchor.buffer_id)?;
                                let rhs_buffer_id = diff.buffer_id();
                                let rhs_buffer = rhs_snapshot.buffer_for_id(rhs_buffer_id)?;
                                let rhs_point = diff.base_text_point_to_buffer_point(
                                    anchor.to_point(&lhs_buffer),
                                    &rhs_buffer,
                                );
                                rhs_snapshot.anchor_in_excerpt(rhs_buffer.anchor_before(rhs_point))
                            })
                            .collect::<Vec<_>>();
                        this.expand_excerpts(rhs_anchors.into_iter(), *lines, *direction, cx);
                    }
                }

                EditorEvent::BufferFoldToggled { .. } => {
                    this.rhs_editor.update(cx, |editor, cx| {
                        editor.inlay_hint_visibility_changed(cx);
                    });
                    cx.emit(event.clone());
                }
                EditorEvent::OpenExcerptsRequested {
                    selections_by_buffer,
                    split,
                } => {
                    if this.lhs.is_some() {
                        let translated =
                            translate_lhs_selections_to_rhs(selections_by_buffer, this, cx);
                        if !translated.is_empty() {
                            let workspace = this.workspace.clone();
                            let split = *split;
                            Editor::open_buffers_in_workspace(
                                workspace, translated, split, window, cx,
                            );
                        }
                    }
                }
                _ => cx.emit(event.clone()),
            },
        )];

        subscriptions.push(
            cx.subscribe(&lhs_editor, |this, _, event: &SearchEvent, cx| {
                if this.searched_side == Some(SplitSide::Left) {
                    cx.emit(event.clone());
                }
            }),
        );

        let lhs_focus_handle = lhs_editor.read(cx).focus_handle(cx);
        subscriptions.push(
            cx.on_focus_in(&lhs_focus_handle, window, |this, _window, cx| {
                if let Some(lhs) = &mut this.lhs {
                    if !lhs.was_last_focused {
                        lhs.was_last_focused = true;
                        cx.notify();
                    }
                }
            }),
        );

        let rhs_focus_handle = self.rhs_editor.read(cx).focus_handle(cx);
        subscriptions.push(
            cx.on_focus_in(&rhs_focus_handle, window, |this, _window, cx| {
                if let Some(lhs) = &mut this.lhs {
                    if lhs.was_last_focused {
                        lhs.was_last_focused = false;
                        cx.notify();
                    }
                }
            }),
        );

        let rhs_display_map = self.rhs_editor.read(cx).display_map.clone();
        let lhs_display_map = lhs_editor.read(cx).display_map.clone();
        let rhs_display_map_id = rhs_display_map.entity_id();
        let companion = cx.new(|_| Companion::new(rhs_display_map_id));
        let lhs = LhsEditor {
            editor: lhs_editor,
            multibuffer: lhs_multibuffer,
            was_last_focused: false,
            _subscriptions: subscriptions,
        };

        self.rhs_editor.update(cx, |editor, cx| {
            editor.set_delegate_expand_excerpts(true);
            editor.buffer().update(cx, |rhs_multibuffer, cx| {
                rhs_multibuffer.set_show_deleted_hunks(false, cx);
                rhs_multibuffer.set_use_extended_diff_range(true, cx);
            })
        });

        self.lhs = Some(lhs);

        self.sync_lhs_for_paths(all_paths, cx);

        rhs_display_map.update(cx, |dm, cx| {
            dm.set_companion(Some((lhs_display_map, companion.clone())), cx);
        });
        self.rhs_editor.update(cx, |editor, cx| {
            editor.inlay_hint_visibility_changed(cx);
        });

        let lhs = self.lhs.as_ref().unwrap();

        let shared_scroll_anchor = self
            .rhs_editor
            .read(cx)
            .scroll_manager
            .scroll_anchor_entity();
        lhs.editor.update(cx, |editor, _cx| {
            editor
                .scroll_manager
                .set_shared_scroll_anchor(shared_scroll_anchor);
        });

        let this = cx.entity().downgrade();
        self.rhs_editor.update(cx, |editor, _cx| {
            let this = this.clone();
            editor.set_on_local_selections_changed(Some(Box::new(
                move |cursor_position, window, cx| {
                    let this = this.clone();
                    window.defer(cx, move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.sync_cursor_to_other_side(true, cursor_position, window, cx);
                        })
                        .ok();
                    })
                },
            )));
        });
        lhs.editor.update(cx, |editor, _cx| {
            let this = this.clone();
            editor.set_on_local_selections_changed(Some(Box::new(
                move |cursor_position, window, cx| {
                    let this = this.clone();
                    window.defer(cx, move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.sync_cursor_to_other_side(false, cursor_position, window, cx);
                        })
                        .ok();
                    })
                },
            )));
        });

        // Copy soft wrap state from rhs (source of truth) to lhs
        let rhs_soft_wrap_override = self.rhs_editor.read(cx).soft_wrap_mode_override;
        lhs.editor.update(cx, |editor, cx| {
            editor.soft_wrap_mode_override = rhs_soft_wrap_override;
            cx.notify();
        });

        cx.notify();
    }

    fn diff_paths(&self, cx: &App) -> Vec<(PathKey, Entity<BufferDiff>)> {
        let rhs_multibuffer = self.rhs_multibuffer.read(cx);
        let rhs_multibuffer_snapshot = rhs_multibuffer.snapshot(cx);
        rhs_multibuffer_snapshot
            .buffers_with_paths()
            .filter_map(|(buffer, path)| {
                let diff = rhs_multibuffer.diff_for(buffer.remote_id())?;
                Some((path.clone(), diff))
            })
            .collect()
    }

    fn activate_pane_left(
        &mut self,
        _: &ActivatePaneLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(lhs) = &self.lhs {
            if !lhs.was_last_focused {
                lhs.editor.read(cx).focus_handle(cx).focus(window, cx);
                lhs.editor.update(cx, |editor, cx| {
                    editor.request_autoscroll(Autoscroll::fit(), cx);
                });
            } else {
                cx.propagate();
            }
        } else {
            cx.propagate();
        }
    }

    fn activate_pane_right(
        &mut self,
        _: &ActivatePaneRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(lhs) = &self.lhs {
            if lhs.was_last_focused {
                self.rhs_editor.read(cx).focus_handle(cx).focus(window, cx);
                self.rhs_editor.update(cx, |editor, cx| {
                    editor.request_autoscroll(Autoscroll::fit(), cx);
                });
            } else {
                cx.propagate();
            }
        } else {
            cx.propagate();
        }
    }

    fn sync_cursor_to_other_side(
        &mut self,
        from_rhs: bool,
        source_point: Point,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(lhs) = &self.lhs else {
            return;
        };

        let (source_editor, target_editor) = if from_rhs {
            (&self.rhs_editor, &lhs.editor)
        } else {
            (&lhs.editor, &self.rhs_editor)
        };

        let source_snapshot = source_editor.update(cx, |editor, cx| editor.snapshot(window, cx));
        let target_snapshot = target_editor.update(cx, |editor, cx| editor.snapshot(window, cx));

        let display_point = source_snapshot
            .display_snapshot
            .point_to_display_point(source_point, Bias::Right);
        let display_point = target_snapshot.clip_point(display_point, Bias::Right);
        let target_point = target_snapshot.display_point_to_point(display_point, Bias::Right);

        target_editor.update(cx, |editor, cx| {
            editor.set_suppress_selection_callback(true);
            editor.change_selections(crate::SelectionEffects::no_scroll(), window, cx, |s| {
                s.select_ranges([target_point..target_point]);
            });
            editor.set_suppress_selection_callback(false);
        });
    }

    pub fn toggle_split(
        &mut self,
        _: &ToggleSplitDiff,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.diff_view_style {
            DiffViewStyle::Unified => {
                self.diff_view_style = DiffViewStyle::Split;
                if !self.too_narrow_for_split {
                    self.split(window, cx);
                }
            }
            DiffViewStyle::Split => {
                self.diff_view_style = DiffViewStyle::Unified;
                if self.is_split() {
                    self.unsplit(window, cx);
                }
            }
        }
    }

    fn intercept_toggle_breakpoint(
        &mut self,
        _: &ToggleBreakpoint,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only block breakpoint actions when the left (lhs) editor has focus
        if let Some(lhs) = &self.lhs {
            if lhs.was_last_focused {
                cx.stop_propagation();
            } else {
                cx.propagate();
            }
        } else {
            cx.propagate();
        }
    }

    fn intercept_enable_breakpoint(
        &mut self,
        _: &EnableBreakpoint,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only block breakpoint actions when the left (lhs) editor has focus
        if let Some(lhs) = &self.lhs {
            if lhs.was_last_focused {
                cx.stop_propagation();
            } else {
                cx.propagate();
            }
        } else {
            cx.propagate();
        }
    }

    fn intercept_disable_breakpoint(
        &mut self,
        _: &DisableBreakpoint,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only block breakpoint actions when the left (lhs) editor has focus
        if let Some(lhs) = &self.lhs {
            if lhs.was_last_focused {
                cx.stop_propagation();
            } else {
                cx.propagate();
            }
        } else {
            cx.propagate();
        }
    }

    fn intercept_edit_log_breakpoint(
        &mut self,
        _: &EditLogBreakpoint,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only block breakpoint actions when the left (lhs) editor has focus
        if let Some(lhs) = &self.lhs {
            if lhs.was_last_focused {
                cx.stop_propagation();
            } else {
                cx.propagate();
            }
        } else {
            cx.propagate();
        }
    }

    pub fn toggle_soft_wrap(
        &mut self,
        _: &ToggleSoftWrap,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(lhs) = &self.lhs {
            cx.stop_propagation();

            let is_lhs_focused = lhs.was_last_focused;
            let (focused_editor, other_editor) = if is_lhs_focused {
                (&lhs.editor, &self.rhs_editor)
            } else {
                (&self.rhs_editor, &lhs.editor)
            };

            // Toggle the focused editor
            focused_editor.update(cx, |editor, cx| {
                editor.toggle_soft_wrap(&ToggleSoftWrap, window, cx);
            });

            // Copy the soft wrap state from the focused editor to the other editor
            let soft_wrap_override = focused_editor.read(cx).soft_wrap_mode_override;
            other_editor.update(cx, |editor, cx| {
                editor.soft_wrap_mode_override = soft_wrap_override;
                cx.notify();
            });
        } else {
            cx.propagate();
        }
    }

    fn unsplit(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let Some(lhs) = self.lhs.take() else {
            return;
        };

        // Detach the stale lhs editor from the shared scroll anchor while the split companion still exists,
        // so its anchor can be converted to lhs native before rhs tears down split specific state.
        lhs.editor.update(cx, |editor, cx| {
            let lhs_snapshot = editor.display_map.update(cx, |dm, cx| dm.snapshot(cx));
            editor
                .scroll_manager
                .unshare_scroll_anchor(&lhs_snapshot, cx);
            editor.set_on_local_selections_changed(None);
        });

        self.rhs_editor.update(cx, |rhs, cx| {
            let rhs_snapshot = rhs.display_map.update(cx, |dm, cx| dm.snapshot(cx));
            let native_anchor = rhs.scroll_manager.native_anchor(&rhs_snapshot, cx);
            let rhs_display_map_id = rhs_snapshot.display_map_id;
            rhs.scroll_manager
                .scroll_anchor_entity()
                .update(cx, |shared, _| {
                    shared.scroll_anchor = native_anchor;
                    shared.display_map_id = Some(rhs_display_map_id);
                });

            rhs.set_on_local_selections_changed(None);
            rhs.set_delegate_expand_excerpts(false);
            rhs.buffer().update(cx, |buffer, cx| {
                buffer.set_show_deleted_hunks(true, cx);
                buffer.set_use_extended_diff_range(false, cx);
            });
            rhs.display_map.update(cx, |dm, cx| {
                dm.set_companion(None, cx);
            });
            rhs.inlay_hint_visibility_changed(cx);
        });
        cx.notify();
    }

    pub fn update_excerpts_for_path(
        &mut self,
        path: PathKey,
        buffer: Entity<Buffer>,
        ranges: impl IntoIterator<Item = Range<Point>> + Clone,
        context_line_count: u32,
        diff: Entity<BufferDiff>,
        cx: &mut Context<Self>,
    ) -> bool {
        let has_ranges = ranges.clone().into_iter().next().is_some();
        if self.lhs.is_none() {
            return self.rhs_multibuffer.update(cx, |rhs_multibuffer, cx| {
                let added_a_new_excerpt = rhs_multibuffer.update_excerpts_for_path(
                    path,
                    buffer.clone(),
                    ranges,
                    context_line_count,
                    cx,
                );
                if has_ranges
                    && rhs_multibuffer
                        .diff_for(buffer.read(cx).remote_id())
                        .is_none_or(|old_diff| old_diff.entity_id() != diff.entity_id())
                {
                    rhs_multibuffer.add_diff(diff, cx);
                }
                added_a_new_excerpt
            });
        }

        let result = self.rhs_multibuffer.update(cx, |rhs_multibuffer, cx| {
            let added_a_new_excerpt = rhs_multibuffer.update_excerpts_for_path(
                path.clone(),
                buffer.clone(),
                ranges,
                context_line_count,
                cx,
            );
            if has_ranges
                && rhs_multibuffer
                    .diff_for(buffer.read(cx).remote_id())
                    .is_none_or(|old_diff| old_diff.entity_id() != diff.entity_id())
            {
                rhs_multibuffer.add_diff(diff.clone(), cx);
            }
            added_a_new_excerpt
        });

        self.sync_lhs_for_paths(vec![(path, diff)], cx);
        result
    }

    fn expand_excerpts(
        &mut self,
        excerpt_anchors: impl Iterator<Item = Anchor> + Clone,
        lines: u32,
        direction: ExpandExcerptDirection,
        cx: &mut Context<Self>,
    ) {
        if self.lhs.is_none() {
            self.rhs_multibuffer.update(cx, |rhs_multibuffer, cx| {
                rhs_multibuffer.expand_excerpts(excerpt_anchors, lines, direction, cx);
            });
            return;
        }

        let paths: Vec<_> = self.rhs_multibuffer.update(cx, |rhs_multibuffer, cx| {
            let snapshot = rhs_multibuffer.snapshot(cx);
            let paths = excerpt_anchors
                .clone()
                .filter_map(|anchor| {
                    let (anchor, _) = snapshot.anchor_to_buffer_anchor(anchor)?;
                    let path = snapshot.path_for_buffer(anchor.buffer_id)?;
                    let diff = rhs_multibuffer.diff_for(anchor.buffer_id)?;
                    Some((path.clone(), diff))
                })
                .collect::<HashMap<_, _>>()
                .into_iter()
                .collect();
            rhs_multibuffer.expand_excerpts(excerpt_anchors, lines, direction, cx);
            paths
        });

        self.sync_lhs_for_paths(paths, cx);
    }

    pub fn remove_excerpts_for_path(&mut self, path: PathKey, cx: &mut Context<Self>) {
        self.rhs_multibuffer.update(cx, |rhs_multibuffer, cx| {
            rhs_multibuffer.remove_excerpts(path.clone(), cx);
        });

        if let Some(lhs) = &self.lhs {
            lhs.multibuffer.update(cx, |lhs_multibuffer, cx| {
                lhs_multibuffer.remove_excerpts(path, cx);
            });
        }
    }

    fn search_token(&self) -> SearchToken {
        SearchToken::new(self.focused_side() as u64)
    }

    fn editor_for_token(&self, token: SearchToken) -> Option<&Entity<Editor>> {
        if token.value() == SplitSide::Left as u64 {
            return self.lhs.as_ref().map(|lhs| &lhs.editor);
        }
        Some(&self.rhs_editor)
    }

    fn sync_lhs_for_paths(
        &self,
        paths: Vec<(PathKey, Entity<BufferDiff>)>,
        cx: &mut Context<Self>,
    ) {
        let Some(lhs) = &self.lhs else { return };

        self.rhs_multibuffer.update(cx, |rhs_multibuffer, cx| {
            for (path, diff) in paths {
                let main_buffer_id = diff.read(cx).buffer_id;
                let Some(main_buffer) = rhs_multibuffer.buffer(diff.read(cx).buffer_id) else {
                    lhs.multibuffer.update(cx, |lhs_multibuffer, lhs_cx| {
                        lhs_multibuffer.remove_excerpts(path, lhs_cx);
                    });
                    continue;
                };
                let main_buffer_snapshot = main_buffer.read(cx).snapshot();

                let base_text_buffer = diff.read(cx).base_text_buffer().clone();
                let diff_snapshot = diff.read(cx).snapshot(cx);
                let base_text_buffer_snapshot = base_text_buffer.read(cx).snapshot();

                let mut paired_ranges: Vec<(Range<Point>, ExcerptRange<text::Anchor>)> = Vec::new();

                let mut have_excerpt = false;
                let mut did_merge = false;
                let rhs_multibuffer_snapshot = rhs_multibuffer.snapshot(cx);
                for info in rhs_multibuffer_snapshot.excerpts_for_buffer(main_buffer_id) {
                    have_excerpt = true;
                    let rhs_context = info.context.to_point(&main_buffer_snapshot);
                    let lhs_context = buffer_range_to_base_text_range(
                        &rhs_context,
                        &diff_snapshot,
                        &main_buffer_snapshot,
                    );

                    if let Some((prev_lhs_context, prev_rhs_range)) = paired_ranges.last_mut()
                        && prev_lhs_context.end >= lhs_context.start
                    {
                        did_merge = true;
                        prev_lhs_context.end = lhs_context.end;
                        prev_rhs_range.context.end = info.context.end;
                        continue;
                    }

                    paired_ranges.push((lhs_context, info));
                }

                let (lhs_ranges, rhs_ranges): (Vec<_>, Vec<_>) = paired_ranges.into_iter().unzip();
                let lhs_ranges = lhs_ranges
                    .into_iter()
                    .map(|range| {
                        ExcerptRange::new(base_text_buffer_snapshot.anchor_range_outside(range))
                    })
                    .collect::<Vec<_>>();

                lhs.multibuffer.update(cx, |lhs_multibuffer, lhs_cx| {
                    lhs_multibuffer.update_path_excerpts(
                        path.clone(),
                        base_text_buffer,
                        &base_text_buffer_snapshot,
                        &lhs_ranges,
                        lhs_cx,
                    );
                    if have_excerpt
                        && lhs_multibuffer
                            .diff_for(base_text_buffer_snapshot.remote_id())
                            .is_none_or(|old_diff| old_diff.entity_id() != diff.entity_id())
                    {
                        lhs_multibuffer.add_inverted_diff(
                            diff.clone(),
                            main_buffer.clone(),
                            lhs_cx,
                        );
                    }
                });

                if did_merge {
                    rhs_multibuffer.update_path_excerpts(
                        path,
                        main_buffer,
                        &main_buffer_snapshot,
                        &rhs_ranges,
                        cx,
                    );
                }
            }
        });
    }

    fn width_changed(&mut self, width: Pixels, window: &mut Window, cx: &mut Context<Self>) {
        self.last_width = Some(width);

        let min_ems = EditorSettings::get_global(cx).minimum_split_diff_width;

        let style = self.rhs_editor.read(cx).create_style(cx);
        let font_id = window.text_system().resolve_font(&style.text.font());
        let font_size = style.text.font_size.to_pixels(window.rem_size());
        let em_advance = window
            .text_system()
            .em_advance(font_id, font_size)
            .unwrap_or(font_size);
        let min_width = em_advance * min_ems;
        let is_split = self.lhs.is_some();

        self.too_narrow_for_split = min_ems > 0.0 && width < min_width;

        match self.diff_view_style {
            DiffViewStyle::Unified => {}
            DiffViewStyle::Split => {
                if self.too_narrow_for_split && is_split {
                    self.unsplit(window, cx);
                } else if !self.too_narrow_for_split && !is_split {
                    self.split(window, cx);
                }
            }
        }
    }

    pub fn remove_excerpts_for_buffer(
        &mut self,
        buffer_id: BufferId,
        cx: &mut Context<'_, SplittableEditor>,
    ) {
        let snapshot = self.rhs_multibuffer.read(cx).snapshot(cx);
        let Some(path) = snapshot.path_for_buffer(buffer_id) else {
            return;
        };
        self.remove_excerpts_for_path(path.clone(), cx);
    }
}

impl Item for SplittableEditor {
    type Event = EditorEvent;

    fn tab_content_text(&self, detail: usize, cx: &App) -> ui::SharedString {
        self.rhs_editor.read(cx).tab_content_text(detail, cx)
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<ui::SharedString> {
        self.rhs_editor.read(cx).tab_tooltip_text(cx)
    }

    fn tab_icon(&self, window: &Window, cx: &App) -> Option<ui::Icon> {
        self.rhs_editor.read(cx).tab_icon(window, cx)
    }

    fn tab_content(&self, params: TabContentParams, window: &Window, cx: &App) -> gpui::AnyElement {
        self.rhs_editor.read(cx).tab_content(params, window, cx)
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.rhs_editor.read(cx).for_each_project_item(cx, f)
    }

    fn buffer_kind(&self, cx: &App) -> ItemBufferKind {
        self.rhs_editor.read(cx).buffer_kind(cx)
    }

    fn active_project_path(&self, cx: &App) -> Option<project::ProjectPath> {
        self.rhs_editor.read(cx).active_project_path(cx)
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.rhs_editor.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.rhs_editor.read(cx).has_conflict(cx)
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        self.rhs_editor.read(cx).has_deleted_file(cx)
    }

    fn capability(&self, cx: &App) -> language::Capability {
        self.rhs_editor.read(cx).capability(cx)
    }

    fn can_save(&self, cx: &App) -> bool {
        self.rhs_editor.read(cx).can_save(cx)
    }

    fn can_save_as(&self, cx: &App) -> bool {
        self.rhs_editor.read(cx).can_save_as(cx)
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Task<anyhow::Result<()>> {
        self.rhs_editor
            .update(cx, |editor, cx| editor.save(options, project, window, cx))
    }

    fn save_as(
        &mut self,
        project: Entity<Project>,
        path: project::ProjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Task<anyhow::Result<()>> {
        self.rhs_editor
            .update(cx, |editor, cx| editor.save_as(project, path, window, cx))
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Task<anyhow::Result<()>> {
        self.rhs_editor
            .update(cx, |editor, cx| editor.reload(project, window, cx))
    }

    fn navigate(
        &mut self,
        data: Arc<dyn std::any::Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.focused_editor()
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focused_editor().update(cx, |editor, cx| {
            editor.deactivated(window, cx);
        });
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace = workspace.weak_handle();
        self.rhs_editor.update(cx, |rhs_editor, cx| {
            rhs_editor.added_to_workspace(workspace, window, cx);
        });
        if let Some(lhs) = &self.lhs {
            lhs.editor.update(cx, |lhs_editor, cx| {
                lhs_editor.added_to_workspace(workspace, window, cx);
            });
        }
    }

    fn as_searchable(
        &self,
        handle: &Entity<Self>,
        _: &App,
    ) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(handle.clone()))
    }

    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation {
        self.rhs_editor.read(cx).breadcrumb_location(cx)
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        self.rhs_editor.read(cx).breadcrumbs(cx)
    }

    fn pixel_position_of_cursor(&self, cx: &App) -> Option<gpui::Point<gpui::Pixels>> {
        self.focused_editor().read(cx).pixel_position_of_cursor(cx)
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: std::any::TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == std::any::TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == std::any::TypeId::of::<Editor>() {
            Some(self.rhs_editor.clone().into())
        } else {
            None
        }
    }
}

impl SearchableItem for SplittableEditor {
    type Match = Range<Anchor>;

    fn clear_matches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rhs_editor.update(cx, |editor, cx| {
            editor.clear_matches(window, cx);
        });
        if let Some(lhs_editor) = self.lhs_editor() {
            lhs_editor.update(cx, |editor, cx| {
                editor.clear_matches(window, cx);
            })
        }
    }

    fn update_matches(
        &mut self,
        matches: &[Self::Match],
        active_match_index: Option<usize>,
        token: SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.editor_for_token(token) else {
            return;
        };
        target.update(cx, |editor, cx| {
            editor.update_matches(matches, active_match_index, token, window, cx);
        });
    }

    fn search_bar_visibility_changed(
        &mut self,
        visible: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if visible {
            let side = self.focused_side();
            self.searched_side = Some(side);
            match side {
                SplitSide::Left => {
                    self.rhs_editor.update(cx, |editor, cx| {
                        editor.clear_matches(window, cx);
                    });
                }
                SplitSide::Right => {
                    if let Some(lhs) = &self.lhs {
                        lhs.editor.update(cx, |editor, cx| {
                            editor.clear_matches(window, cx);
                        });
                    }
                }
            }
        } else {
            self.searched_side = None;
        }
    }

    fn query_suggestion(
        &mut self,
        seed_query_override: Option<SeedQuerySetting>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> String {
        self.focused_editor().update(cx, |editor, cx| {
            editor.query_suggestion(seed_query_override, window, cx)
        })
    }

    fn activate_match(
        &mut self,
        index: usize,
        matches: &[Self::Match],
        token: SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.editor_for_token(token) else {
            return;
        };
        target.update(cx, |editor, cx| {
            editor.activate_match(index, matches, token, window, cx);
        });
    }

    fn select_matches(
        &mut self,
        matches: &[Self::Match],
        token: SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.editor_for_token(token) else {
            return;
        };
        target.update(cx, |editor, cx| {
            editor.select_matches(matches, token, window, cx);
        });
    }

    fn replace(
        &mut self,
        identifier: &Self::Match,
        query: &project::search::SearchQuery,
        token: SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.editor_for_token(token) else {
            return;
        };
        target.update(cx, |editor, cx| {
            editor.replace(identifier, query, token, window, cx);
        });
    }

    fn find_matches(
        &mut self,
        query: Arc<project::search::SearchQuery>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Task<Vec<Self::Match>> {
        self.focused_editor()
            .update(cx, |editor, cx| editor.find_matches(query, window, cx))
    }

    fn find_matches_with_token(
        &mut self,
        query: Arc<project::search::SearchQuery>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Task<(Vec<Self::Match>, SearchToken)> {
        let token = self.search_token();
        let editor = self.focused_editor().downgrade();
        cx.spawn_in(window, async move |_, cx| {
            let Some(matches) = editor
                .update_in(cx, |editor, window, cx| {
                    editor.find_matches(query, window, cx)
                })
                .ok()
            else {
                return (Vec::new(), token);
            };
            (matches.await, token)
        })
    }

    fn active_match_index(
        &mut self,
        direction: workspace::searchable::Direction,
        matches: &[Self::Match],
        token: SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        self.editor_for_token(token)?.update(cx, |editor, cx| {
            editor.active_match_index(direction, matches, token, window, cx)
        })
    }
}

impl EventEmitter<EditorEvent> for SplittableEditor {}
impl EventEmitter<SearchEvent> for SplittableEditor {}
impl Focusable for SplittableEditor {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.focused_editor().read(cx).focus_handle(cx)
    }
}

impl Render for SplittableEditor {
    fn render(
        &mut self,
        _window: &mut ui::Window,
        cx: &mut ui::Context<Self>,
    ) -> impl ui::IntoElement {
        let is_split = self.lhs.is_some();
        let inner = if is_split {
            let style = self.rhs_editor.read(cx).create_style(cx);
            SplitEditorView::new(cx.entity(), style, self.split_state.clone()).into_any_element()
        } else {
            self.rhs_editor.clone().into_any_element()
        };

        let this = cx.entity().downgrade();
        let last_width = self.last_width;

        div()
            .id("splittable-editor")
            .on_action(cx.listener(Self::toggle_split))
            .on_action(cx.listener(Self::activate_pane_left))
            .on_action(cx.listener(Self::activate_pane_right))
            .on_action(cx.listener(Self::intercept_toggle_breakpoint))
            .on_action(cx.listener(Self::intercept_enable_breakpoint))
            .on_action(cx.listener(Self::intercept_disable_breakpoint))
            .on_action(cx.listener(Self::intercept_edit_log_breakpoint))
            .capture_action(cx.listener(Self::toggle_soft_wrap))
            .size_full()
            .child(inner)
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let width = bounds.size.width;
                        if last_width == Some(width) {
                            return;
                        }
                        window.defer(cx, move |window, cx| {
                            this.update(cx, |this, cx| {
                                this.width_changed(width, window, cx);
                            })
                            .ok();
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
    }
}
