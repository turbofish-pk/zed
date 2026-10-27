//! This module defines where the text should be displayed in an [`Editor`][Editor].
//!
//! Not literally though - rendering, layout and all that jazz is a responsibility of [`EditorElement`][EditorElement].
//! Instead, [`DisplayMap`] decides where Inlays/Inlay hints are displayed, when
//! to apply a soft wrap, where to add fold indicators, whether there are any tabs in the buffer that
//! we display as spaces and where to display custom blocks (like diagnostics).
//! Seems like a lot? That's because it is. [`DisplayMap`] is conceptually made up
//! of several smaller structures that form a hierarchy (starting at the bottom):
//! - [`InlayMap`] that decides where the [`Inlay`]s should be displayed.
//! - [`FoldMap`] that decides where the fold indicators should be; it also tracks parts of a source file that are currently folded.
//! - [`TabMap`] that keeps track of hard tabs in a buffer.
//! - [`WrapMap`] that handles soft wrapping.
//! - [`BlockMap`] that tracks custom blocks such as diagnostics that should be displayed within buffer.
//! - [`DisplayMap`] that adds background highlights to the regions of text.
//!   Each one of those builds on top of preceding map.
//!
//! ## Structure of the display map layers
//!
//! Each layer in the map (and the multibuffer itself to some extent) has a few
//! structures that are used to implement the public API available to the layer
//! above:
//! - a `Transform` type - this represents a region of text that the layer in
//!   question is "managing", that it transforms into a more "processed" text
//!   for the layer above. For example, the inlay map has an `enum Transform`
//!   that has two variants:
//!     - `Isomorphic`, representing a region of text that has no inlay hints (i.e.
//!       is passed through the map transparently)
//!     - `Inlay`, representing a location where an inlay hint is to be inserted.
//! - a `TransformSummary` type, which is usually a struct with two fields:
//!   [`input: TextSummary`][`TextSummary`] and [`output: TextSummary`][`TextSummary`]. Here,
//!   `input` corresponds to "text in the layer below", and `output` corresponds to the text
//!   exposed to the layer above. So in the inlay map case, a `Transform::Isomorphic`'s summary is
//!   just `input = output = summary`, where `summary` is the [`TextSummary`] stored in that
//!   variant. Conversely, a `Transform::Inlay` always has an empty `input` summary, because it's
//!   not "replacing" any text that exists on disk. The `output` is the summary of the inlay text
//!   to be injected. - Various newtype wrappers for co-ordinate spaces (e.g. [`WrapRow`]
//!   represents a row index, after soft-wrapping (and all lower layers)).
//! - A `Snapshot` type (e.g. [`InlaySnapshot`]) that captures the state of a layer at a specific
//!   point in time.
//! - various APIs which drill through the layers below to work with the underlying text. Notably:
//!   - `fn text_summary_for_offset()` returns a [`TextSummary`] for the range in the co-ordinate
//!     space that the map in question is responsible for.
//!   - `fn <A>_point_to_<B>_point()` converts a point in co-ordinate space `A` into co-ordinate
//!     space `B`.
//!   - A [`RowInfo`] iterator (e.g. [`InlayBufferRows`]) and a [`Chunk`] iterator
//!     (e.g. [`InlayChunks`])
//!   - A `sync` function (e.g. [`InlayMap::sync`]) that takes a snapshot and list of [`Edit<T>`]s,
//!     and returns a new snapshot and a list of transformed [`Edit<S>`]s. Note that the generic
//!     parameter on `Edit` changes, since these methods take in edits in the co-ordinate space of
//!     the lower layer, and return edits in their own co-ordinate space. The term "edit" is
//!     slightly misleading, since an [`Edit<T>`] doesn't tell you what changed - rather it can be
//!     thought of as a "region to invalidate". In theory, it would be correct to always use a
//!     single edit that covers the entire range. However, this would lead to lots of unnecessary
//!     recalculation.
//!
//! See the docs for the [`inlay_map`] module for a more in-depth explanation of how a single layer
//! works.
//!
//! [Editor]: crate::Editor
//! [EditorElement]: crate::element::EditorElement
//! [`TextSummary`]: multi_buffer::MBTextSummary
//! [`WrapRow`]: wrap_map::WrapRow
//! [`InlayBufferRows`]: inlay_map::InlayBufferRows
//! [`InlayChunks`]: inlay_map::InlayChunks
//! [`Edit<T>`]: text::Edit
//! [`Edit<S>`]: text::Edit
//! [`Chunk`]: language::Chunk

#[macro_use]
mod dimensions;

mod block_map;
mod crease_map;
mod custom_highlights;
mod fold_map;
mod inlay_map;
mod invisibles;
mod tab_map;
mod wrap_map;

pub use crate::display_map::{fold_map::FoldMap, inlay_map::InlayMap, tab_map::TabMap};
pub use block_map::{
    Block, BlockChunks as DisplayChunks, BlockContext, BlockId, BlockMap, BlockPlacement,
    BlockPoint, BlockProperties, BlockRows, BlockStyle, CompanionView, CompanionViewMut,
    CustomBlockId, EditorMargins, RenderBlock, StickyHeaderExcerpt,
};
pub use crease_map::*;
pub use fold_map::{
    ChunkRenderer, ChunkRendererContext, ChunkRendererId, Fold, FoldId, FoldPlaceholder, FoldPoint,
};
pub use inlay_map::{InlayOffset, InlayPoint};
use invisibles::is_standalone_grapheme;
pub use invisibles::{is_invisible, replacement};
pub use wrap_map::{WrapPoint, WrapRow, WrapSnapshot};

use collections::{HashMap, HashSet, IndexSet};
use gpui::{
    App, Context, Entity, EntityId, Font, HighlightStyle, Hsla, LineLayout, Pixels, UnderlineStyle,
    WeakEntity,
};
use language::{
    LanguageAwareStyling, Point, Subscription as BufferSubscription,
    language_settings::{AllLanguageSettings, LanguageSettings, SoftWrapIndent},
};

use multi_buffer::{
    Anchor, AnchorRangeExt, MultiBuffer, MultiBufferOffset, MultiBufferOffsetUtf16,
    MultiBufferPoint, MultiBufferRow, MultiBufferSnapshot, RowInfo, ToOffset, ToPoint,
};
use project::project_settings::DiagnosticSeverity;
use project::{InlayId, lsp_store::LspFoldingRange, lsp_store::TokenType};
use serde::Deserialize;
use settings::Settings;
use smallvec::SmallVec;
use sum_tree::{Bias, TreeMap};
use text::{BufferId, LineIndent, Patch};
use theme::StatusColors;
use ui::{SharedString, px};
use unicode_segmentation::UnicodeSegmentation;
use ztracing::instrument;

use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::{
    any::TypeId,
    borrow::Cow,
    fmt::Debug,
    iter,
    num::NonZeroU32,
    ops::{self, Add, Range, RangeInclusive, Sub},
    slice,
    sync::Arc,
};

use crate::{
    EditorStyle, RowExt, hover_links::InlayHighlight, inlays::Inlay, movement::TextLayoutDetails,
};
use block_map::{BlockPointCursor, BlockRow, BlockSnapshot};
use fold_map::{FoldPointCursor, FoldSnapshot};
use inlay_map::{BufferOffsetToInlayPointCursor, InlaySnapshot};
pub(crate) use tab_map::TabPoint;
use tab_map::{TabPointCursor, TabSnapshot};
use wrap_map::{WrapMap, WrapPatch, WrapPointCursor};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FoldStatus {
    Folded,
    Foldable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NavigationOverlayKey(TypeId);

impl NavigationOverlayKey {
    pub const fn unique<T: 'static>() -> Self {
        Self(TypeId::of::<T>())
    }
}

/// Keys for tagging text highlights.
///
/// Note the order is important as it determines the priority of the highlights, lower means higher priority
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HighlightKey {
    // Note we want semantic tokens > colorized brackets
    // to allow language server highlights to work over brackets.
    ColorizeBracket(usize),
    SemanticToken(u32),
    // below is sorted lexicographically, as there is no relevant ordering for these aside from coming after the above
    BufferSearchHighlights,
    ConsoleAnsiHighlight(usize),
    DebugStackFrameLine,
    DocumentHighlightRead,
    DocumentHighlightWrite,

    Editor,
    HighlightOnYank,
    HighlightsTreeView(usize),
    HoverState,
    HoveredLinkState,
    InlineAssist,
    InputComposition,
    MatchingBracket,
    NavigationOverlay(NavigationOverlayKey),
    PendingInput,
    PickerPreview,
    ProjectSearchView,
    Rename,
    SearchWithinRange,
    SelectedTextHighlight,
    SyntaxTreeView(usize),
    VimExchange,
}

pub trait ToDisplayPoint {
    fn to_display_point(&self, map: &DisplaySnapshot) -> DisplayPoint;
}

type TextHighlights = Arc<HashMap<HighlightKey, Arc<(HighlightStyle, Vec<Range<Anchor>>)>>>;
type SemanticTokensHighlights =
    Arc<HashMap<BufferId, (Arc<[SemanticTokenHighlight]>, Arc<HighlightStyleInterner>)>>;
type InlayHighlights = TreeMap<HighlightKey, TreeMap<InlayId, (HighlightStyle, InlayHighlight)>>;

#[derive(Debug)]
pub struct CompanionExcerptPatch {
    pub patch: Patch<MultiBufferPoint>,
    pub edited_range: Range<MultiBufferPoint>,
    pub source_excerpt_range: Range<MultiBufferPoint>,
    pub target_excerpt_range: Range<MultiBufferPoint>,
}

/// Decides how text in a [`MultiBuffer`] should be displayed in a buffer, handling inlay hints,
/// folding, hard tabs, soft wrapping, custom blocks (like diagnostics), and highlighting.
///
/// See the [module level documentation](self) for more information.
pub struct DisplayMap {
    entity_id: EntityId,
    /// The buffer that we are displaying.
    buffer: Entity<MultiBuffer>,
    buffer_subscription: BufferSubscription<MultiBufferOffset>,
    /// Decides where the [`Inlay`]s should be displayed.
    inlay_map: InlayMap,
    /// Decides where the fold indicators should be and tracks parts of a source file that are currently folded.
    fold_map: FoldMap,
    /// Keeps track of hard tabs in a buffer.
    tab_map: TabMap,
    /// Handles soft wrapping.
    wrap_map: Entity<WrapMap>,
    /// Tracks custom blocks such as diagnostics that should be displayed within buffer.
    block_map: BlockMap,
    /// Regions of text that should be highlighted.
    text_highlights: TextHighlights,
    /// Regions of inlays that should be highlighted.
    inlay_highlights: InlayHighlights,
    /// The semantic tokens from the language server.
    pub semantic_token_highlights: SemanticTokensHighlights,
    /// A container for explicitly foldable ranges, which supersede indentation based fold range suggestions.
    crease_map: CreaseMap,
    pub(crate) fold_placeholder: FoldPlaceholder,
    pub clip_at_line_ends: bool,
    pub(crate) masked: bool,
    pub(crate) diagnostics_max_severity: DiagnosticSeverity,
    pub(crate) companion: Option<(WeakEntity<DisplayMap>, Entity<Companion>)>,
    lsp_folding_crease_ids: HashMap<BufferId, Vec<CreaseId>>,
}

pub(crate) struct Companion {
    rhs_display_map_id: EntityId,
    rhs_custom_block_to_balancing_block: RefCell<HashMap<CustomBlockId, CustomBlockId>>,
    lhs_custom_block_to_balancing_block: RefCell<HashMap<CustomBlockId, CustomBlockId>>,
}

impl Companion {
    pub(crate) fn new(rhs_display_map_id: EntityId) -> Self {
        Self {
            rhs_display_map_id,
            rhs_custom_block_to_balancing_block: Default::default(),
            lhs_custom_block_to_balancing_block: Default::default(),
        }
    }

    pub(crate) fn is_rhs(&self, display_map_id: EntityId) -> bool {
        self.rhs_display_map_id == display_map_id
    }

    pub(crate) fn custom_block_to_balancing_block(
        &self,
        display_map_id: EntityId,
    ) -> &RefCell<HashMap<CustomBlockId, CustomBlockId>> {
        if self.is_rhs(display_map_id) {
            &self.rhs_custom_block_to_balancing_block
        } else {
            &self.lhs_custom_block_to_balancing_block
        }
    }

    pub(crate) fn convert_rows_to_companion(
        &self,
        display_map_id: EntityId,
        companion_snapshot: &MultiBufferSnapshot,
        our_snapshot: &MultiBufferSnapshot,
        bounds: Range<MultiBufferPoint>,
    ) -> Vec<CompanionExcerptPatch> {
        if self.is_rhs(display_map_id) {
            crate::split::patches_for_rhs_range(companion_snapshot, our_snapshot, bounds)
        } else {
            crate::split::patches_for_lhs_range(companion_snapshot, our_snapshot, bounds)
        }
    }

    pub(crate) fn convert_point_from_companion(
        &self,
        display_map_id: EntityId,
        our_snapshot: &MultiBufferSnapshot,
        companion_snapshot: &MultiBufferSnapshot,
        point: MultiBufferPoint,
    ) -> Range<MultiBufferPoint> {
        let patches = if self.is_rhs(display_map_id) {
            crate::split::patches_for_lhs_range(our_snapshot, companion_snapshot, point..point)
        } else {
            crate::split::patches_for_rhs_range(our_snapshot, companion_snapshot, point..point)
        };

        let Some(excerpt) = patches.into_iter().next() else {
            if cfg!(any(test, debug_assertions)) {
                assert!(
                    our_snapshot.max_point() == Point::zero(),
                    "`patches_for_*_in_range` is only allowed to return an empty vec if the multibuffer is empty"
                );
            }
            return Point::zero()..our_snapshot.max_point();
        };
        excerpt.patch.edit_for_old_position(point).new
    }

    pub(crate) fn convert_point_to_companion(
        &self,
        display_map_id: EntityId,
        our_snapshot: &MultiBufferSnapshot,
        companion_snapshot: &MultiBufferSnapshot,
        point: MultiBufferPoint,
    ) -> Range<MultiBufferPoint> {
        let patches = if self.is_rhs(display_map_id) {
            crate::split::patches_for_rhs_range(companion_snapshot, our_snapshot, point..point)
        } else {
            crate::split::patches_for_lhs_range(companion_snapshot, our_snapshot, point..point)
        };

        let Some(excerpt) = patches.into_iter().next() else {
            return Point::zero()..companion_snapshot.max_point();
        };
        excerpt.patch.edit_for_old_position(point).new
    }
}

#[derive(Default, Debug)]
pub struct HighlightStyleInterner {
    styles: IndexSet<HighlightStyle>,
}

impl HighlightStyleInterner {
    pub(crate) fn intern(&mut self, style: HighlightStyle) -> HighlightStyleId {
        HighlightStyleId(self.styles.insert_full(style).0 as u32)
    }
}

impl ops::Index<HighlightStyleId> for HighlightStyleInterner {
    type Output = HighlightStyle;

    fn index(&self, index: HighlightStyleId) -> &Self::Output {
        &self.styles[index.0 as usize]
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct HighlightStyleId(u32);

/// A `SemanticToken`, but positioned to an offset in a buffer, and stylized.
#[derive(Debug, Clone)]
pub struct SemanticTokenHighlight {
    pub range: Range<Anchor>,
    pub style: HighlightStyleId,
    pub token_type: TokenType,
    pub token_modifiers: u32,
    pub server_id: lsp::LanguageServerId,
    pub precedence: u32,
}

impl DisplayMap {
    pub fn new(
        buffer: Entity<MultiBuffer>,
        font: Font,
        font_size: Pixels,
        wrap_width: Option<Pixels>,
        buffer_header_height: u32,
        excerpt_header_height: u32,
        fold_placeholder: FoldPlaceholder,
        diagnostics_max_severity: DiagnosticSeverity,
        cx: &mut Context<Self>,
    ) -> Self {
        let tab_size = Self::tab_size(&buffer, cx);
        // Important: obtain the snapshot BEFORE creating the subscription.
        // snapshot() may call sync() which publishes edits. If we subscribe first,
        // those edits would be captured but the InlayMap would already be at the
        // post-edit state, causing a desync.
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let buffer_subscription = buffer.update(cx, |buffer, _| buffer.subscribe());
        let crease_map = CreaseMap::new(&buffer_snapshot);
        let (inlay_map, snapshot) = InlayMap::new(buffer_snapshot);
        let (fold_map, snapshot) = FoldMap::new(snapshot);
        let (tab_map, snapshot) = TabMap::new(snapshot, tab_size);
        let (wrap_map, snapshot) = WrapMap::new(snapshot, font, font_size, wrap_width, cx);
        let block_map = BlockMap::new(snapshot, buffer_header_height, excerpt_header_height);

        cx.observe(&wrap_map, |_, _, cx| cx.notify()).detach();

        DisplayMap {
            entity_id: cx.entity_id(),
            buffer,
            buffer_subscription,
            fold_map,
            inlay_map,
            tab_map,
            wrap_map,
            block_map,
            crease_map,
            fold_placeholder,
            diagnostics_max_severity,
            text_highlights: Default::default(),
            inlay_highlights: Default::default(),
            semantic_token_highlights: Default::default(),
            clip_at_line_ends: false,
            masked: false,
            companion: None,
            lsp_folding_crease_ids: HashMap::default(),
        }
    }

    pub(crate) fn set_companion(
        &mut self,
        companion: Option<(Entity<DisplayMap>, Entity<Companion>)>,
        cx: &mut Context<Self>,
    ) {
        let this = cx.weak_entity();
        // Reverting to no companion, recompute the block map to clear spacers
        // and balancing blocks.
        let Some((companion_display_map, companion)) = companion else {
            let Some((_, companion)) = self.companion.take() else {
                return;
            };
            assert_eq!(self.entity_id, companion.read(cx).rhs_display_map_id);
            let (snapshot, _edits) = self.sync_through_wrap(cx);
            let edits = Patch::new(vec![text::Edit {
                old: WrapRow(0)
                    ..self.block_map.wrap_snapshot.borrow().max_point().row() + WrapRow(1),
                new: WrapRow(0)..snapshot.max_point().row() + WrapRow(1),
            }]);
            self.block_map.deferred_edits.set(edits);
            self.block_map.retain_blocks_raw(&mut |block| {
                if companion
                    .read(cx)
                    .lhs_custom_block_to_balancing_block
                    .borrow()
                    .values()
                    .any(|id| *id == block.id)
                {
                    return false;
                }
                true
            });
            return;
        };
        assert_eq!(self.entity_id, companion.read(cx).rhs_display_map_id);

        // Note, throwing away the wrap edits because we defer spacer computation to the first render.
        let snapshot = {
            let edits = self.buffer_subscription.consume();
            let snapshot = self.buffer.read(cx).snapshot(cx);
            let tab_size = Self::tab_size(&self.buffer, cx);
            let (snapshot, edits) = self.inlay_map.sync(snapshot, edits.into_inner());
            let (mut writer, snapshot, edits) = self.fold_map.write(snapshot, edits);
            let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
            let (_snapshot, _edits) = self
                .wrap_map
                .update(cx, |wrap_map, cx| wrap_map.sync(snapshot, edits, cx));

            let (snapshot, edits) = writer.unfold_intersecting([Anchor::Min..Anchor::Max], true);
            let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
            let (snapshot, _edits) = self
                .wrap_map
                .update(cx, |wrap_map, cx| wrap_map.sync(snapshot, edits, cx));

            self.block_map.retain_blocks_raw(&mut |block| {
                !matches!(block.placement, BlockPlacement::Replace(_))
            });
            snapshot
        };

        let (companion_wrap_snapshot, _companion_wrap_edits) =
            companion_display_map.update(cx, |dm, cx| dm.sync_through_wrap(cx));

        let edits = Patch::new(vec![text::Edit {
            old: WrapRow(0)..self.block_map.wrap_snapshot.borrow().max_point().row() + WrapRow(1),
            new: WrapRow(0)..snapshot.max_point().row() + WrapRow(1),
        }]);
        self.block_map.deferred_edits.set(edits);

        let all_blocks: Vec<_> = self.block_map.blocks_raw().map(Clone::clone).collect();

        companion_display_map.update(cx, |companion_display_map, cx| {
            // Sync folded buffers from RHS to LHS. Also clean up stale
            // entries: the block map doesn't remove buffers from
            // `folded_buffers` when they leave the multibuffer, so we
            // unfold any RHS buffers whose companion mapping is missing.
            let rhs_snapshot = self.buffer.read(cx).snapshot(cx);
            let mut buffers_to_unfold = Vec::new();
            for my_buffer in self.folded_buffers() {
                let their_buffer = rhs_snapshot
                    .diff_for_buffer_id(*my_buffer)
                    .map(|diff| diff.base_text().remote_id());

                let Some(their_buffer) = their_buffer else {
                    buffers_to_unfold.push(*my_buffer);
                    continue;
                };

                companion_display_map
                    .block_map
                    .folded_buffers
                    .insert(their_buffer);
            }
            for buffer_id in buffers_to_unfold {
                self.block_map.folded_buffers.remove(&buffer_id);
            }

            for block in all_blocks {
                let Some(their_block) = block_map::balancing_block(
                    &block.properties(),
                    snapshot.buffer(),
                    companion_wrap_snapshot.buffer(),
                    self.entity_id,
                    companion.read(cx),
                ) else {
                    continue;
                };
                let their_id = companion_display_map
                    .block_map
                    .insert_block_raw(their_block, companion_wrap_snapshot.buffer());
                companion.update(cx, |companion, _cx| {
                    companion
                        .custom_block_to_balancing_block(self.entity_id)
                        .borrow_mut()
                        .insert(block.id, their_id);
                });
            }
            let companion_edits = Patch::new(vec![text::Edit {
                old: WrapRow(0)
                    ..companion_display_map
                        .block_map
                        .wrap_snapshot
                        .borrow()
                        .max_point()
                        .row()
                        + WrapRow(1),
                new: WrapRow(0)..companion_wrap_snapshot.max_point().row() + WrapRow(1),
            }]);
            companion_display_map
                .block_map
                .deferred_edits
                .set(companion_edits);
            companion_display_map.companion = Some((this, companion.clone()));
        });

        self.companion = Some((companion_display_map.downgrade(), companion));
    }

    pub(crate) fn companion(&self) -> Option<&Entity<Companion>> {
        self.companion.as_ref().map(|(_, c)| c)
    }

    fn sync_through_wrap(&mut self, cx: &mut App) -> (WrapSnapshot, WrapPatch) {
        let tab_size = Self::tab_size(&self.buffer, cx);
        let buffer_snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();

        let (snapshot, edits) = self.inlay_map.sync(buffer_snapshot, edits);
        let (snapshot, edits) = self.fold_map.read(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        self.wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx))
    }

    fn with_synced_companion_mut<R>(
        display_map_id: EntityId,
        companion: &Option<(WeakEntity<DisplayMap>, Entity<Companion>)>,
        cx: &mut App,
        callback: impl FnOnce(Option<CompanionViewMut<'_>>, &mut App) -> R,
    ) -> R {
        let Some((companion_display_map, companion)) = companion else {
            return callback(None, cx);
        };
        let Some(companion_display_map) = companion_display_map.upgrade() else {
            return callback(None, cx);
        };
        companion_display_map.update(cx, |companion_display_map, cx| {
            let (companion_wrap_snapshot, companion_wrap_edits) =
                companion_display_map.sync_through_wrap(cx);
            companion_display_map
                .buffer
                .update(cx, |companion_multibuffer, cx| {
                    companion.update(cx, |companion, cx| {
                        let companion_view = CompanionViewMut::new(
                            display_map_id,
                            companion_display_map.entity_id,
                            &companion_wrap_snapshot,
                            &companion_wrap_edits,
                            companion_multibuffer,
                            companion,
                            &mut companion_display_map.block_map,
                        );
                        callback(Some(companion_view), cx)
                    })
                })
        })
    }

    #[instrument(skip_all)]
    pub fn snapshot(&mut self, cx: &mut Context<Self>) -> DisplaySnapshot {
        let (self_wrap_snapshot, self_wrap_edits) = self.sync_through_wrap(cx);
        let companion_wrap_data = self.companion.as_ref().and_then(|(companion_dm, _)| {
            companion_dm
                .update(cx, |dm, cx| dm.sync_through_wrap(cx))
                .ok()
        });
        let companion_ref = self.companion.as_ref().map(|(_, c)| c.read(cx));
        let companion_view = companion_wrap_data.as_ref().zip(companion_ref).map(
            |((snapshot, edits), companion)| {
                CompanionView::new(self.entity_id, snapshot, edits, companion)
            },
        );

        let block_snapshot = self
            .block_map
            .read(
                self_wrap_snapshot.clone(),
                self_wrap_edits.clone(),
                companion_view,
            )
            .snapshot;

        if let Some((companion_dm, _)) = &self.companion {
            let _ = companion_dm.update(cx, |dm, _cx| {
                if let Some((companion_snapshot, companion_edits)) = companion_wrap_data {
                    let their_companion_ref = dm.companion.as_ref().map(|(_, c)| c.read(_cx));
                    dm.block_map.read(
                        companion_snapshot,
                        companion_edits,
                        their_companion_ref.map(|c| {
                            CompanionView::new(
                                dm.entity_id,
                                &self_wrap_snapshot,
                                &self_wrap_edits,
                                c,
                            )
                        }),
                    );
                }
            });
        }

        let companion_display_snapshot = self.companion.as_ref().and_then(|(companion_dm, _)| {
            companion_dm
                .update(cx, |dm, cx| Arc::new(dm.snapshot_simple(cx)))
                .ok()
        });

        DisplaySnapshot {
            display_map_id: self.entity_id,
            companion_display_snapshot,
            block_snapshot,
            diagnostics_max_severity: self.diagnostics_max_severity,
            crease_snapshot: self.crease_map.snapshot(),
            text_highlights: self.text_highlights.clone(),
            inlay_highlights: self.inlay_highlights.clone(),
            semantic_token_highlights: self.semantic_token_highlights.clone(),
            clip_at_line_ends: self.clip_at_line_ends,
            masked: self.masked,
            use_lsp_folding_ranges: !self.lsp_folding_crease_ids.is_empty(),
            fold_placeholder: self.fold_placeholder.clone(),
        }
    }

    fn snapshot_simple(&mut self, cx: &mut Context<Self>) -> DisplaySnapshot {
        let (wrap_snapshot, wrap_edits) = self.sync_through_wrap(cx);

        let block_snapshot = self
            .block_map
            .read(wrap_snapshot, wrap_edits, None)
            .snapshot;

        DisplaySnapshot {
            display_map_id: self.entity_id,
            companion_display_snapshot: None,
            block_snapshot,
            diagnostics_max_severity: self.diagnostics_max_severity,
            crease_snapshot: self.crease_map.snapshot(),
            text_highlights: self.text_highlights.clone(),
            inlay_highlights: self.inlay_highlights.clone(),
            semantic_token_highlights: self.semantic_token_highlights.clone(),
            clip_at_line_ends: self.clip_at_line_ends,
            masked: self.masked,
            use_lsp_folding_ranges: !self.lsp_folding_crease_ids.is_empty(),
            fold_placeholder: self.fold_placeholder.clone(),
        }
    }

    pub fn crease_snapshot(&self) -> CreaseSnapshot {
        self.crease_map.snapshot()
    }

    #[instrument(skip_all)]
    pub fn set_state(&mut self, other: &DisplaySnapshot, cx: &mut Context<Self>) {
        self.fold(
            other
                .folds_in_range(MultiBufferOffset(0)..other.buffer_snapshot().len())
                .map(|fold| {
                    Crease::simple(
                        fold.range.to_offset(other.buffer_snapshot()),
                        fold.placeholder.clone(),
                    )
                })
                .collect(),
            cx,
        );
        for buffer_id in &other.block_snapshot.buffers_with_disabled_headers {
            self.disable_header_for_buffer(*buffer_id, cx);
        }
    }

    /// Creates folds for the given creases.
    #[instrument(skip_all)]
    pub fn fold<T: Clone + ToOffset>(&mut self, creases: Vec<Crease<T>>, cx: &mut Context<Self>) {
        if self.companion().is_some() {
            return;
        }

        let buffer_snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();
        let tab_size = Self::tab_size(&self.buffer, cx);

        let (snapshot, edits) = self.inlay_map.sync(buffer_snapshot.clone(), edits);
        let (mut fold_map, snapshot, edits) = self.fold_map.write(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        self.block_map.read(snapshot, edits, None);

        let inline = creases.iter().filter_map(|crease| {
            if let Crease::Inline {
                range, placeholder, ..
            } = crease
            {
                Some((range.clone(), placeholder.clone()))
            } else {
                None
            }
        });
        let (snapshot, edits) = fold_map.fold(inline);

        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));

        let blocks = creases
            .into_iter()
            .filter_map(|crease| {
                if let Crease::Block {
                    range,
                    block_height,
                    render_block,
                    block_style,
                    block_priority,
                    ..
                } = crease
                {
                    Some((
                        range,
                        render_block,
                        block_height,
                        block_style,
                        block_priority,
                    ))
                } else {
                    None
                }
            })
            .map(|(range, render, height, style, priority)| {
                let start = buffer_snapshot.anchor_before(range.start);
                let end = buffer_snapshot.anchor_after(range.end);
                BlockProperties {
                    placement: BlockPlacement::Replace(start..=end),
                    render,
                    height: Some(height),
                    style,
                    priority,
                }
            });

        self.block_map.write(snapshot, edits, None).insert(blocks);
    }

    /// Removes any folds with the given ranges.
    #[instrument(skip_all)]
    pub fn remove_folds_with_type<T: ToOffset>(
        &mut self,
        ranges: impl IntoIterator<Item = Range<T>>,
        type_id: TypeId,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();
        let tab_size = Self::tab_size(&self.buffer, cx);

        let (snapshot, edits) = self.inlay_map.sync(snapshot, edits);
        let (mut fold_map, snapshot, edits) = self.fold_map.write(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        self.block_map.read(snapshot, edits, None);

        let (snapshot, edits) = fold_map.remove_folds(ranges, type_id);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (self_new_wrap_snapshot, self_new_wrap_edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));

        self.block_map
            .write(self_new_wrap_snapshot, self_new_wrap_edits, None);
    }

    /// Removes any folds whose ranges intersect any of the given ranges.
    #[instrument(skip_all)]
    pub fn unfold_intersecting<T: ToOffset>(
        &mut self,
        ranges: impl IntoIterator<Item = Range<T>>,
        inclusive: bool,
        cx: &mut Context<Self>,
    ) -> WrapSnapshot {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let offset_ranges = ranges
            .into_iter()
            .map(|range| range.start.to_offset(&snapshot)..range.end.to_offset(&snapshot))
            .collect::<Vec<_>>();
        let edits = self.buffer_subscription.consume().into_inner();
        let tab_size = Self::tab_size(&self.buffer, cx);

        let (snapshot, edits) = self.inlay_map.sync(snapshot, edits);
        let (mut fold_map, snapshot, edits) = self.fold_map.write(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        self.block_map.read(snapshot, edits, None);

        let (snapshot, edits) =
            fold_map.unfold_intersecting(offset_ranges.iter().cloned(), inclusive);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (self_new_wrap_snapshot, self_new_wrap_edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));

        self.block_map
            .write(self_new_wrap_snapshot.clone(), self_new_wrap_edits, None)
            .remove_intersecting_replace_blocks(offset_ranges, inclusive);

        self_new_wrap_snapshot
    }

    #[instrument(skip_all)]
    pub fn disable_header_for_buffer(&mut self, buffer_id: BufferId, cx: &mut Context<Self>) {
        let (self_wrap_snapshot, self_wrap_edits) = self.sync_through_wrap(cx);
        self.block_map
            .write(self_wrap_snapshot, self_wrap_edits, None)
            .disable_header_for_buffer(buffer_id);
    }

    #[instrument(skip_all)]
    pub fn fold_buffers(
        &mut self,
        buffer_ids: impl IntoIterator<Item = language::BufferId>,
        cx: &mut App,
    ) {
        let buffer_ids: Vec<_> = buffer_ids.into_iter().collect();

        let (self_wrap_snapshot, self_wrap_edits) = self.sync_through_wrap(cx);

        Self::with_synced_companion_mut(
            self.entity_id,
            &self.companion,
            cx,
            |companion_view, cx| {
                self.block_map
                    .write(
                        self_wrap_snapshot.clone(),
                        self_wrap_edits.clone(),
                        companion_view,
                    )
                    .fold_buffers(buffer_ids.iter().copied(), self.buffer.read(cx), cx);
            },
        )
    }

    #[instrument(skip_all)]
    pub fn unfold_buffers(
        &mut self,
        buffer_ids: impl IntoIterator<Item = language::BufferId>,
        cx: &mut Context<Self>,
    ) {
        let buffer_ids: Vec<_> = buffer_ids.into_iter().collect();

        let (self_wrap_snapshot, self_wrap_edits) = self.sync_through_wrap(cx);

        Self::with_synced_companion_mut(
            self.entity_id,
            &self.companion,
            cx,
            |companion_view, cx| {
                self.block_map
                    .write(
                        self_wrap_snapshot.clone(),
                        self_wrap_edits.clone(),
                        companion_view,
                    )
                    .unfold_buffers(buffer_ids.iter().copied(), self.buffer.read(cx), cx);
            },
        )
    }

    #[instrument(skip_all)]
    pub(crate) fn is_buffer_folded(&self, buffer_id: language::BufferId) -> bool {
        self.block_map.folded_buffers.contains(&buffer_id)
    }

    #[instrument(skip_all)]
    pub(crate) fn folded_buffers(&self) -> &HashSet<BufferId> {
        &self.block_map.folded_buffers
    }

    #[instrument(skip_all)]
    pub fn insert_creases(
        &mut self,
        creases: impl IntoIterator<Item = Crease<Anchor>>,
        cx: &mut Context<Self>,
    ) -> Vec<CreaseId> {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        self.crease_map.insert(creases, &snapshot)
    }

    #[instrument(skip_all)]
    pub fn remove_creases(
        &mut self,
        crease_ids: impl IntoIterator<Item = CreaseId>,
        cx: &mut Context<Self>,
    ) -> Vec<(CreaseId, Range<Anchor>)> {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        self.crease_map.remove(crease_ids, &snapshot)
    }

    /// Replaces the LSP folding-range creases for a single buffer.
    /// Converts the supplied buffer-anchor ranges into multi-buffer creases
    /// by mapping them through the appropriate excerpts.
    pub(super) fn set_lsp_folding_ranges(
        &mut self,
        buffer_id: BufferId,
        ranges: Vec<LspFoldingRange>,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);

        let old_ids = self
            .lsp_folding_crease_ids
            .remove(&buffer_id)
            .unwrap_or_default();
        if !old_ids.is_empty() {
            self.crease_map.remove(old_ids, &snapshot);
        }

        if ranges.is_empty() {
            return;
        }

        let base_placeholder = self.fold_placeholder.clone();
        let creases = ranges.into_iter().filter_map(|folding_range| {
            let mb_range =
                snapshot.buffer_anchor_range_to_anchor_range(folding_range.range.clone())?;
            let placeholder = if let Some(collapsed_text) = folding_range.collapsed_text {
                FoldPlaceholder {
                    render: Arc::new({
                        let collapsed_text = collapsed_text.clone();
                        move |fold_id, _fold_range, cx: &mut gpui::App| {
                            use gpui::{Element as _, ParentElement as _};
                            FoldPlaceholder::fold_element(fold_id, cx)
                                .child(collapsed_text.clone())
                                .into_any()
                        }
                    }),
                    constrain_width: false,
                    merge_adjacent: base_placeholder.merge_adjacent,
                    type_tag: base_placeholder.type_tag,
                    collapsed_text: Some(collapsed_text),
                }
            } else {
                base_placeholder.clone()
            };
            Some(Crease::simple(mb_range, placeholder))
        });

        let new_ids = self.crease_map.insert(creases, &snapshot);
        if !new_ids.is_empty() {
            self.lsp_folding_crease_ids.insert(buffer_id, new_ids);
        }
    }

    /// Removes all LSP folding-range creases for a single buffer.
    pub(super) fn clear_lsp_folding_ranges(&mut self, buffer_id: BufferId, cx: &mut Context<Self>) {
        if let Some(old_ids) = self.lsp_folding_crease_ids.remove(&buffer_id) {
            let snapshot = self.buffer.read(cx).snapshot(cx);
            self.crease_map.remove(old_ids, &snapshot);
        }
    }

    /// Returns `true` when at least one buffer has LSP folding-range creases.
    pub(super) fn has_lsp_folding_ranges(&self) -> bool {
        !self.lsp_folding_crease_ids.is_empty()
    }

    #[instrument(skip_all)]
    pub fn insert_blocks(
        &mut self,
        blocks: impl IntoIterator<Item = BlockProperties<Anchor>>,
        cx: &mut Context<Self>,
    ) -> Vec<CustomBlockId> {
        let (self_wrap_snapshot, self_wrap_edits) = self.sync_through_wrap(cx);
        Self::with_synced_companion_mut(
            self.entity_id,
            &self.companion,
            cx,
            |companion_view, _cx| {
                self.block_map
                    .write(
                        self_wrap_snapshot.clone(),
                        self_wrap_edits.clone(),
                        companion_view,
                    )
                    .insert(blocks)
            },
        )
    }

    #[instrument(skip_all)]
    pub fn resize_blocks(&mut self, heights: HashMap<CustomBlockId, u32>, cx: &mut Context<Self>) {
        let (self_wrap_snapshot, self_wrap_edits) = self.sync_through_wrap(cx);

        Self::with_synced_companion_mut(
            self.entity_id,
            &self.companion,
            cx,
            |companion_view, _cx| {
                self.block_map
                    .write(
                        self_wrap_snapshot.clone(),
                        self_wrap_edits.clone(),
                        companion_view,
                    )
                    .resize(heights);
            },
        )
    }

    #[instrument(skip_all)]
    pub fn replace_blocks(&mut self, renderers: HashMap<CustomBlockId, RenderBlock>) {
        self.block_map.replace_blocks(renderers);
    }

    #[instrument(skip_all)]
    pub fn remove_blocks(&mut self, ids: HashSet<CustomBlockId>, cx: &mut Context<Self>) {
        let (self_wrap_snapshot, self_wrap_edits) = self.sync_through_wrap(cx);

        Self::with_synced_companion_mut(
            self.entity_id,
            &self.companion,
            cx,
            |companion_view, _cx| {
                self.block_map
                    .write(
                        self_wrap_snapshot.clone(),
                        self_wrap_edits.clone(),
                        companion_view,
                    )
                    .remove(ids);
            },
        )
    }

    #[instrument(skip_all)]
    pub fn row_for_block(
        &mut self,
        block_id: CustomBlockId,
        cx: &mut Context<Self>,
    ) -> Option<DisplayRow> {
        let (self_wrap_snapshot, self_wrap_edits) = self.sync_through_wrap(cx);

        let companion_wrap_data = self.companion.as_ref().and_then(|(companion_dm, _)| {
            companion_dm
                .update(cx, |dm, cx| dm.sync_through_wrap(cx))
                .ok()
        });

        let companion_ref = self.companion.as_ref().map(|(_, c)| c.read(cx));
        let companion_view = companion_wrap_data.as_ref().zip(companion_ref).map(
            |((snapshot, edits), companion)| {
                CompanionView::new(self.entity_id, snapshot, edits, companion)
            },
        );

        let block_map = self.block_map.read(
            self_wrap_snapshot.clone(),
            self_wrap_edits.clone(),
            companion_view,
        );
        let block_row = block_map.row_for_block(block_id)?;

        if let Some((companion_dm, _)) = &self.companion {
            let _ = companion_dm.update(cx, |dm, cx| {
                if let Some((companion_snapshot, companion_edits)) = companion_wrap_data {
                    let their_companion_ref = dm.companion.as_ref().map(|(_, c)| c.read(cx));
                    dm.block_map.read(
                        companion_snapshot,
                        companion_edits,
                        their_companion_ref.map(|c| {
                            CompanionView::new(
                                dm.entity_id,
                                &self_wrap_snapshot,
                                &self_wrap_edits,
                                c,
                            )
                        }),
                    );
                }
            });
        }

        Some(DisplayRow(block_row.0))
    }

    #[instrument(skip_all)]
    pub fn highlight_text(
        &mut self,
        key: HighlightKey,
        mut ranges: Vec<Range<Anchor>>,
        style: HighlightStyle,
        merge: bool,
        cx: &App,
    ) {
        let multi_buffer_snapshot = self.buffer.read(cx).snapshot(cx);
        match Arc::make_mut(&mut self.text_highlights).entry(key) {
            Entry::Occupied(mut slot) => match Arc::get_mut(slot.get_mut()) {
                Some((_, previous_ranges)) if merge => {
                    previous_ranges.extend(ranges);
                    previous_ranges.sort_by(|a, b| a.start.cmp(&b.start, &multi_buffer_snapshot));
                }
                Some((previous_style, previous_ranges)) => {
                    *previous_style = style;
                    *previous_ranges = ranges;
                    previous_ranges.sort_by(|a, b| a.start.cmp(&b.start, &multi_buffer_snapshot));
                }
                None if merge => {
                    ranges.extend(slot.get().1.iter().cloned());
                    ranges.sort_by(|a, b| a.start.cmp(&b.start, &multi_buffer_snapshot));
                    slot.insert(Arc::new((style, ranges)));
                }
                None => {
                    ranges.sort_by(|a, b| a.start.cmp(&b.start, &multi_buffer_snapshot));
                    slot.insert(Arc::new((style, ranges)));
                }
            },
            Entry::Vacant(slot) => {
                ranges.sort_by(|a, b| a.start.cmp(&b.start, &multi_buffer_snapshot));
                slot.insert(Arc::new((style, ranges)));
            }
        }
    }

    #[instrument(skip_all)]
    pub(crate) fn highlight_inlays(
        &mut self,
        key: HighlightKey,
        highlights: Vec<InlayHighlight>,
        style: HighlightStyle,
    ) {
        for highlight in highlights {
            let update = self.inlay_highlights.update(&key, |highlights| {
                highlights.insert(highlight.inlay, (style, highlight.clone()))
            });
            if update.is_none() {
                self.inlay_highlights.insert(
                    key,
                    TreeMap::from_ordered_entries([(highlight.inlay, (style, highlight))]),
                );
            }
        }
    }

    #[instrument(skip_all)]
    pub fn text_highlights(&self, key: HighlightKey) -> Option<(HighlightStyle, &[Range<Anchor>])> {
        let highlights = self.text_highlights.get(&key)?;
        Some((highlights.0, &highlights.1))
    }

    pub fn all_text_highlights(
        &self,
    ) -> impl Iterator<Item = (&HighlightKey, &Arc<(HighlightStyle, Vec<Range<Anchor>>)>)> {
        self.text_highlights.iter()
    }

    pub fn all_semantic_token_highlights(
        &self,
    ) -> impl Iterator<
        Item = (
            &BufferId,
            &(Arc<[SemanticTokenHighlight]>, Arc<HighlightStyleInterner>),
        ),
    > {
        self.semantic_token_highlights.iter()
    }

    pub fn clear_highlights(&mut self, key: HighlightKey) -> bool {
        let mut cleared = Arc::make_mut(&mut self.text_highlights)
            .remove(&key)
            .is_some();
        cleared |= self.inlay_highlights.remove(&key).is_some();
        cleared
    }

    pub fn clear_highlights_with(&mut self, f: &mut dyn FnMut(&HighlightKey) -> bool) -> bool {
        let mut cleared = false;
        Arc::make_mut(&mut self.text_highlights).retain(|k, _| {
            let b = !f(k);
            cleared |= b;
            b
        });
        self.inlay_highlights.retain(|k, _| {
            let b = !f(k);
            cleared |= b;
            b
        });
        cleared
    }

    pub fn set_font(&self, font: Font, font_size: Pixels, cx: &mut Context<Self>) -> bool {
        self.wrap_map
            .update(cx, |map, cx| map.set_font_with_size(font, font_size, cx))
    }

    pub fn set_wrap_width(&self, width: Option<Pixels>, cx: &mut Context<Self>) -> bool {
        self.wrap_map
            .update(cx, |map, cx| map.set_wrap_width(width, cx))
    }

    pub fn set_soft_wrap_indent(&self, indent: SoftWrapIndent, cx: &mut Context<Self>) -> bool {
        self.wrap_map
            .update(cx, |map, cx| map.set_soft_wrap_indent(indent, cx))
    }

    #[instrument(skip_all)]
    pub fn update_fold_widths(
        &mut self,
        widths: impl IntoIterator<Item = (ChunkRendererId, Pixels)>,
        cx: &mut Context<Self>,
    ) -> bool {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();
        let tab_size = Self::tab_size(&self.buffer, cx);

        let (snapshot, edits) = self.inlay_map.sync(snapshot, edits);
        let (mut fold_map, snapshot, edits) = self.fold_map.write(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        self.block_map.read(snapshot, edits, None);

        let (snapshot, edits) = fold_map.update_fold_widths(widths);
        let widths_changed = !edits.is_empty();
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (self_new_wrap_snapshot, self_new_wrap_edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));

        self.block_map
            .read(self_new_wrap_snapshot, self_new_wrap_edits, None);

        widths_changed
    }

    pub(crate) fn current_inlays(&self) -> slice::Iter<'_, Inlay> {
        self.inlay_map.current_inlays()
    }

    #[instrument(skip_all)]
    pub(crate) fn splice_inlays(
        &mut self,
        to_remove: &[InlayId],
        to_insert: Vec<Inlay>,
        cx: &mut Context<Self>,
    ) {
        if to_remove.is_empty() && to_insert.is_empty() {
            return;
        }
        let buffer_snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();
        let tab_size = Self::tab_size(&self.buffer, cx);

        let companion_wrap_data = self.companion.as_ref().and_then(|(companion_dm, _)| {
            companion_dm
                .update(cx, |dm, cx| dm.sync_through_wrap(cx))
                .ok()
        });

        let (snapshot, edits) = self.inlay_map.sync(buffer_snapshot, edits);
        let (snapshot, edits) = self.fold_map.read(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));

        {
            let companion_ref = self.companion.as_ref().map(|(_, c)| c.read(cx));
            let companion_view = companion_wrap_data.as_ref().zip(companion_ref).map(
                |((snapshot, edits), companion)| {
                    CompanionView::new(self.entity_id, snapshot, edits, companion)
                },
            );
            self.block_map.read(snapshot, edits, companion_view);
        }

        let (snapshot, edits) = self.inlay_map.splice(to_remove, to_insert);
        let (snapshot, edits) = self.fold_map.read(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (self_new_wrap_snapshot, self_new_wrap_edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));

        let (self_wrap_snapshot, self_wrap_edits) =
            (self_new_wrap_snapshot.clone(), self_new_wrap_edits.clone());

        {
            let companion_ref = self.companion.as_ref().map(|(_, c)| c.read(cx));
            let companion_view = companion_wrap_data.as_ref().zip(companion_ref).map(
                |((snapshot, edits), companion)| {
                    CompanionView::new(self.entity_id, snapshot, edits, companion)
                },
            );
            self.block_map
                .read(self_new_wrap_snapshot, self_new_wrap_edits, companion_view);
        }

        if let Some((companion_dm, _)) = &self.companion {
            let _ = companion_dm.update(cx, |dm, cx| {
                if let Some((companion_snapshot, companion_edits)) = companion_wrap_data {
                    let their_companion_ref = dm.companion.as_ref().map(|(_, c)| c.read(cx));
                    dm.block_map.read(
                        companion_snapshot,
                        companion_edits,
                        their_companion_ref.map(|c| {
                            CompanionView::new(
                                dm.entity_id,
                                &self_wrap_snapshot,
                                &self_wrap_edits,
                                c,
                            )
                        }),
                    );
                }
            });
        }
    }

    #[instrument(skip_all)]
    fn tab_size(buffer: &Entity<MultiBuffer>, cx: &App) -> NonZeroU32 {
        if let Some(buffer) = buffer.read(cx).as_singleton().map(|buffer| buffer.read(cx)) {
            LanguageSettings::for_buffer(buffer, cx).tab_size
        } else {
            AllLanguageSettings::get_global(cx).defaults.tab_size
        }
    }

    pub fn is_rewrapping(&self, cx: &gpui::App) -> bool {
        self.wrap_map.read(cx).is_rewrapping()
    }

    pub fn invalidate_semantic_highlights(&mut self, buffer_id: BufferId) {
        Arc::make_mut(&mut self.semantic_token_highlights).remove(&buffer_id);
    }
}

#[derive(Debug, Default)]
pub(crate) struct Highlights<'a> {
    pub text_highlights: Option<&'a TextHighlights>,
    pub inlay_highlights: Option<&'a InlayHighlights>,
    pub semantic_token_highlights: Option<&'a SemanticTokensHighlights>,
    pub styles: HighlightStyles,
}


#[derive(Default, Debug, Clone, Copy)]
pub struct HighlightStyles {
    pub inlay_hint: Option<HighlightStyle>,
}

#[derive(Clone)]
pub enum ChunkReplacement {
    Renderer(ChunkRenderer),
    Str(SharedString),
}

pub struct HighlightedChunk<'a> {
    pub text: &'a str,
    pub style: Option<HighlightStyle>,
    pub(crate) diagnostic_underline_severity: Option<lsp::DiagnosticSeverity>,
    pub is_tab: bool,
    pub is_inlay: bool,
    pub replacement: Option<ChunkReplacement>,
}

impl<'a> HighlightedChunk<'a> {
    #[instrument(skip_all)]
    fn highlight_invisibles(
        self,
        editor_style: &'a EditorStyle,
    ) -> impl Iterator<Item = Self> + 'a {
        let mut text = self.text;
        let style = self.style;
        let diagnostic_underline_severity = self.diagnostic_underline_severity;
        let is_tab = self.is_tab;
        let renderer = self.replacement;
        let is_inlay = self.is_inlay;
        iter::from_fn(move || {
            if text.is_empty() {
                return None;
            }
            for (offset, ch) in text.char_indices() {
                if !is_invisible(ch) {
                    continue;
                }
                let ch_end = offset + ch.len_utf8();
                if !is_standalone_grapheme(text, offset, ch_end) {
                    continue;
                }
                if offset > 0 {
                    let (prefix, suffix) = text.split_at(offset);
                    text = suffix;
                    return Some(HighlightedChunk {
                        text: prefix,
                        style,
                        diagnostic_underline_severity,
                        is_tab,
                        is_inlay,
                        replacement: renderer.clone(),
                    });
                }
                let (invisible_text, suffix) = text.split_at(ch_end);
                text = suffix;
                let invisible_highlight = HighlightStyle {
                    background_color: Some(editor_style.status.hint_background),
                    underline: Some(UnderlineStyle {
                        color: Some(editor_style.status.hint),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    ..Default::default()
                };
                let invisible_style = if let Some(style) = style {
                    style.highlight(invisible_highlight)
                } else {
                    invisible_highlight
                };
                return Some(HighlightedChunk {
                    text: invisible_text,
                    style: Some(invisible_style),
                    diagnostic_underline_severity: None,
                    is_tab: false,
                    is_inlay,
                    replacement: match replacement(ch) {
                        Some(replacement) => {
                            Some(ChunkReplacement::Str(SharedString::from(replacement)))
                        }
                        None => renderer.clone(),
                    },
                });
            }
            let remainder = text;
            text = "";
            Some(HighlightedChunk {
                text: remainder,
                style,
                diagnostic_underline_severity,
                is_tab,
                is_inlay,
                replacement: renderer.clone(),
            })
        })
    }
}

#[derive(Clone)]
pub struct DisplaySnapshot {
    pub display_map_id: EntityId,
    pub companion_display_snapshot: Option<Arc<DisplaySnapshot>>,
    pub crease_snapshot: CreaseSnapshot,
    block_snapshot: BlockSnapshot,
    text_highlights: TextHighlights,
    inlay_highlights: InlayHighlights,
    semantic_token_highlights: SemanticTokensHighlights,
    clip_at_line_ends: bool,
    masked: bool,
    diagnostics_max_severity: DiagnosticSeverity,
    pub(crate) fold_placeholder: FoldPlaceholder,
    /// When true, LSP folding ranges are used via the crease map and the
    /// indent-based fallback in `crease_for_buffer_row` is skipped.
    pub(crate) use_lsp_folding_ranges: bool,
}

impl DisplaySnapshot {
    pub fn companion_snapshot(&self) -> Option<&DisplaySnapshot> {
        self.companion_display_snapshot.as_deref()
    }

    pub fn inlay_hint_at(&self, point: DisplayPoint) -> Option<(&Inlay, usize)> {
        if point.row() > self.max_point().row()
            || self.is_block_line(point.row())
            || point.column() >= self.line_len(point.row())
        {
            return None;
        }
        let wrap_row = self
            .block_snapshot
            .to_wrap_point(BlockPoint::new(BlockRow(point.row().0), 0), Bias::Left)
            .row();
        let wrap_indent = wrap_row
            .0
            .checked_sub(1)
            .and_then(|previous_row| self.wrap_snapshot().soft_wrap_indent(WrapRow(previous_row)));
        if wrap_indent.is_some_and(|indent| point.column() < indent) {
            return None;
        }
        let fold_point = self.display_point_to_fold_point(point, Bias::Left);
        if self
            .fold_snapshot()
            .placeholder_range_at(fold_point)
            .is_some()
        {
            return None;
        }
        let inlay_offset = self
            .inlay_snapshot()
            .to_offset(fold_point.to_inlay_point(self.fold_snapshot()));
        if self.fold_snapshot().is_inlay_offset_folded(inlay_offset) {
            return None;
        }
        self.inlay_snapshot().inlay_hint_at_offset(inlay_offset)
    }

    pub fn is_inlay_visible(&self, inlay_id: InlayId, position: Anchor) -> bool {
        if !position.is_valid(self.buffer_snapshot()) {
            return false;
        }
        let Some(rendered_range) = self.inlay_snapshot().inlay_offset_range(inlay_id, position)
        else {
            return false;
        };
        let start = self.inlay_offset_to_display_point(rendered_range.start, Bias::Right);
        let end = self.inlay_offset_to_display_point(rendered_range.end, Bias::Left);
        (start.row().0..=end.row().0).any(|row| {
            let row = DisplayRow(row);
            if self.is_block_line(row) {
                return false;
            }
            let point = if row == start.row() {
                start
            } else {
                DisplayPoint::new(row, 0)
            };
            let offset = self.display_point_to_inlay_offset(point, Bias::Left);
            rendered_range.contains(&offset) && !self.fold_snapshot().is_inlay_offset_folded(offset)
        })
    }

    pub(crate) fn diagnostics_visible(&self) -> bool {
        self.diagnostics_max_severity.into_lsp().is_some()
    }

    fn diagnostic_severity_is_visible(&self, severity: lsp::DiagnosticSeverity) -> bool {
        self.diagnostics_max_severity
            .into_lsp()
            .is_some_and(|max_severity| severity <= max_severity)
    }

    pub(crate) fn diagnostic_underline_style(
        &self,
        severity: lsp::DiagnosticSeverity,
        underline: bool,
        is_unnecessary: bool,
        editor_style: &EditorStyle,
    ) -> Option<UnderlineStyle> {
        (underline
            && editor_style.show_underlines
            && self.diagnostic_severity_is_visible(severity)
            && !(is_unnecessary && severity > lsp::DiagnosticSeverity::WARNING))
            .then(|| UnderlineStyle {
                color: Some(diagnostic_style(severity, &editor_style.status)),
                thickness: 1.0.into(),
                wavy: true,
            })
    }

    pub fn wrap_snapshot(&self) -> &WrapSnapshot {
        &self.block_snapshot.wrap_snapshot
    }
    pub fn tab_snapshot(&self) -> &TabSnapshot {
        &self.block_snapshot.wrap_snapshot.tab_snapshot
    }

    pub fn fold_snapshot(&self) -> &FoldSnapshot {
        &self.block_snapshot.wrap_snapshot.tab_snapshot.fold_snapshot
    }

    #[inline(always)]
    pub fn has_collapsed_content(&self) -> bool {
        self.fold_snapshot().has_folds() || self.block_snapshot.has_replacement_blocks()
    }

    pub fn inlay_snapshot(&self) -> &InlaySnapshot {
        &self
            .block_snapshot
            .wrap_snapshot
            .tab_snapshot
            .fold_snapshot
            .inlay_snapshot
    }

    pub fn buffer_snapshot(&self) -> &MultiBufferSnapshot {
        &self
            .block_snapshot
            .wrap_snapshot
            .tab_snapshot
            .fold_snapshot
            .inlay_snapshot
            .buffer
    }



    pub fn is_empty(&self) -> bool {
        self.buffer_snapshot().len() == MultiBufferOffset(0)
    }

    /// Returns whether tree-sitter syntax highlighting should be used.
    /// Returns `false` if any buffer with semantic token highlights has the "full" mode setting,
    /// meaning LSP semantic tokens should replace tree-sitter highlighting.
    pub fn use_tree_sitter_for_syntax(&self, position: DisplayRow, cx: &App) -> bool {
        let position = DisplayPoint::new(position, 0);
        let Some((buffer_snapshot, ..)) = self.point_to_buffer_point(position.to_point(self))
        else {
            return false;
        };
        let settings = LanguageSettings::for_buffer_snapshot(&buffer_snapshot, None, cx);
        settings.semantic_tokens.use_tree_sitter()
    }

    pub fn row_infos(&self, start_row: DisplayRow) -> impl Iterator<Item = RowInfo> + '_ {
        self.block_snapshot.row_infos(BlockRow(start_row.0))
    }

    pub fn widest_line_number(&self) -> u32 {
        self.buffer_snapshot().widest_line_number()
    }

    #[instrument(skip_all)]
    pub fn prev_line_boundary(&self, mut point: MultiBufferPoint) -> (Point, DisplayPoint) {
        loop {
            let mut inlay_point = self.inlay_snapshot().to_inlay_point(point);
            let mut fold_point = self.fold_snapshot().to_fold_point(inlay_point, Bias::Left);
            fold_point.0.column = 0;
            inlay_point = fold_point.to_inlay_point(self.fold_snapshot());
            point = self.inlay_snapshot().to_buffer_point(inlay_point);

            let mut display_point = self.point_to_display_point(point, Bias::Left);
            *display_point.column_mut() = 0;
            let next_point = self.display_point_to_point(display_point, Bias::Left);
            if next_point == point {
                return (point, display_point);
            }
            point = next_point;
        }
    }

    #[instrument(skip_all)]
    pub fn next_line_boundary(
        &self,
        mut point: MultiBufferPoint,
    ) -> (MultiBufferPoint, DisplayPoint) {
        let original_point = point;
        loop {
            let mut inlay_point = self.inlay_snapshot().to_inlay_point(point);
            let mut fold_point = self.fold_snapshot().to_fold_point(inlay_point, Bias::Right);
            fold_point.0.column = self.fold_snapshot().line_len(fold_point.row());
            inlay_point = fold_point.to_inlay_point(self.fold_snapshot());
            point = self.inlay_snapshot().to_buffer_point(inlay_point);

            let mut display_point = self.point_to_display_point(point, Bias::Right);
            *display_point.column_mut() = self.line_len(display_point.row());
            let next_point = self.display_point_to_point(display_point, Bias::Right);
            if next_point == point || original_point == point || original_point == next_point {
                return (point, display_point);
            }
            point = next_point;
        }
    }

    // used by line_mode selections and tries to match vim behavior
    pub fn expand_to_line(&self, range: Range<Point>) -> Range<Point> {
        let new_start = MultiBufferPoint::new(range.start.row, 0);
        let new_end = if range.end.column > 0 {
            MultiBufferPoint::new(
                range.end.row,
                self.buffer_snapshot()
                    .line_len(MultiBufferRow(range.end.row)),
            )
        } else {
            range.end
        };

        new_start..new_end
    }

    #[instrument(skip_all)]
    pub fn point_to_display_point(&self, point: MultiBufferPoint, bias: Bias) -> DisplayPoint {
        let inlay_point = self.inlay_snapshot().to_inlay_point(point);
        let fold_point = self.fold_snapshot().to_fold_point(inlay_point, bias);
        let tab_point = self.tab_snapshot().fold_point_to_tab_point(fold_point);
        let wrap_point = self.wrap_snapshot().tab_point_to_wrap_point(tab_point);
        let block_point = self.block_snapshot.to_block_point(wrap_point);
        DisplayPoint(block_point)
    }

    /// Converts a buffer offset range into one or more `DisplayPoint` ranges
    /// that cover only actual buffer text, excluding any inlay hint text that
    /// falls within the range.
    pub fn isomorphic_display_point_ranges_for_buffer_range(
        &self,
        range: Range<MultiBufferOffset>,
    ) -> SmallVec<[Range<DisplayPoint>; 1]> {
        self.display_point_converter().map(range)
    }

    /// Converts a non-empty buffer range into one contiguous display range.
    /// Inlays at either boundary are excluded, while inlays between selected
    /// buffer characters are included.
    pub fn contiguous_display_point_range_for_buffer_range(
        &self,
        range: Range<MultiBufferOffset>,
    ) -> Option<Range<DisplayPoint>> {
        if range.is_empty() {
            return None;
        }

        let buffer = self.buffer_snapshot();
        let first_character_end =
            buffer.clip_offset((range.start + 1usize).min(range.end), Bias::Right);
        let last_character_start = buffer.clip_offset(
            range.end.saturating_sub_usize(1).max(range.start),
            Bias::Left,
        );

        let mut converter = self.display_point_converter();
        let first_ranges = converter.map(range.start..first_character_end);
        let start = first_ranges.first()?.start;
        if first_character_end == range.end {
            return Some(start..first_ranges.last()?.end);
        }

        let last_ranges = converter.map(last_character_start..range.end);
        Some(start..last_ranges.last()?.end)
    }

    /// Returns a converter that maps buffer offset ranges to `DisplayPoint`
    /// ranges (as in [`Self::isomorphic_display_point_ranges_for_buffer_range`])
    /// while reusing cursor state across calls. Use this when converting many
    /// ranges in a single pass; the inputs must be supplied with non-decreasing
    /// offsets so the underlying cursors only advance forward.
    pub fn display_point_converter(&self) -> DisplayPointConverter<'_> {
        DisplayPointConverter {
            inlay_cursor: self.inlay_snapshot().buffer_offset_to_inlay_point_cursor(),
            fold_point_cursor: self.fold_snapshot().fold_point_cursor(),
            tab_point_cursor: self.tab_snapshot().tab_point_cursor(),
            wrap_point_cursor: self.wrap_snapshot().wrap_point_cursor(),
            block_point_cursor: self.block_snapshot.block_point_cursor(),
            prev_end: None,
        }
    }

    pub fn display_point_to_point(&self, point: DisplayPoint, bias: Bias) -> Point {
        self.inlay_snapshot()
            .to_buffer_point(self.display_point_to_inlay_point(point, bias))
    }

    pub fn display_point_to_inlay_offset(&self, point: DisplayPoint, bias: Bias) -> InlayOffset {
        self.inlay_snapshot()
            .to_offset(self.display_point_to_inlay_point(point, bias))
    }

    pub fn anchor_to_inlay_offset(&self, anchor: Anchor) -> InlayOffset {
        self.inlay_snapshot()
            .to_inlay_offset(anchor.to_offset(self.buffer_snapshot()))
    }

    pub fn display_point_to_anchor(&self, point: DisplayPoint, bias: Bias) -> Anchor {
        self.buffer_snapshot()
            .anchor_at(point.to_offset(self, bias), bias)
    }

    #[instrument(skip_all)]
    fn display_point_to_inlay_point(&self, point: DisplayPoint, bias: Bias) -> InlayPoint {
        let block_point = point.0;
        let wrap_point = self.block_snapshot.to_wrap_point(block_point, bias);
        let tab_point = self.wrap_snapshot().to_tab_point(wrap_point);
        let fold_point = self
            .tab_snapshot()
            .tab_point_to_fold_point(tab_point, bias)
            .0;
        fold_point.to_inlay_point(self.fold_snapshot())
    }

    #[instrument(skip_all)]
    pub fn display_point_to_fold_point(&self, point: DisplayPoint, bias: Bias) -> FoldPoint {
        let block_point = point.0;
        let wrap_point = self.block_snapshot.to_wrap_point(block_point, bias);
        let tab_point = self.wrap_snapshot().to_tab_point(wrap_point);
        self.tab_snapshot()
            .tab_point_to_fold_point(tab_point, bias)
            .0
    }

    #[instrument(skip_all)]
    pub fn fold_point_to_display_point(&self, fold_point: FoldPoint) -> DisplayPoint {
        let tab_point = self.tab_snapshot().fold_point_to_tab_point(fold_point);
        let wrap_point = self.wrap_snapshot().tab_point_to_wrap_point(tab_point);
        let block_point = self.block_snapshot.to_block_point(wrap_point);
        DisplayPoint(block_point)
    }

    pub fn max_point(&self) -> DisplayPoint {
        DisplayPoint(self.block_snapshot.max_point())
    }

    /// Returns text chunks starting at the given display row until the end of the file
    #[instrument(skip_all)]
    pub fn text_chunks(&self, display_row: DisplayRow) -> impl Iterator<Item = &str> {
        self.block_snapshot
            .chunks(
                BlockRow(display_row.0)..BlockRow(self.max_point().row().next_row().0),
                LanguageAwareStyling {
                    tree_sitter: false,
                    diagnostics: false,
                },
                self.masked,
                Highlights::default(),
            )
            .map(|h| h.text)
    }

    /// Returns text chunks starting at the end of the given display row in reverse until the start of the file
    #[instrument(skip_all)]
    pub fn reverse_text_chunks(&self, display_row: DisplayRow) -> impl Iterator<Item = &str> {
        (0..=display_row.0).rev().flat_map(move |row| {
            self.block_snapshot
                .chunks(
                    BlockRow(row)..BlockRow(row + 1),
                    LanguageAwareStyling {
                        tree_sitter: false,
                        diagnostics: false,
                    },
                    self.masked,
                    Highlights::default(),
                )
                .map(|h| h.text)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
        })
    }

    #[instrument(skip_all)]
    pub fn chunks(
        &self,
        display_rows: Range<DisplayRow>,
        language_aware: LanguageAwareStyling,
        highlight_styles: HighlightStyles,
    ) -> DisplayChunks<'_> {
        self.block_snapshot.chunks(
            BlockRow(display_rows.start.0)..BlockRow(display_rows.end.0),
            language_aware,
            self.masked,
            Highlights {
                text_highlights: Some(&self.text_highlights),
                inlay_highlights: Some(&self.inlay_highlights),
                semantic_token_highlights: Some(&self.semantic_token_highlights),
                styles: highlight_styles,
            },
        )
    }

    #[instrument(skip_all)]
    pub fn highlighted_chunks<'a>(
        &'a self,
        display_rows: Range<DisplayRow>,
        language_aware: LanguageAwareStyling,
        editor_style: &'a EditorStyle,
    ) -> impl Iterator<Item = HighlightedChunk<'a>> {
        self.chunks(
            display_rows,
            language_aware,
            HighlightStyles {
                inlay_hint: Some(editor_style.inlay_hints_style),
               },
        )
        .flat_map({
            // track the current underline style so that we can apply it to
            // inlay hints within the diagnostic's span
            let mut current_diagnostic_underline: Option<UnderlineStyle> = None;
            let mut current_diagnostic_severity: Option<lsp::DiagnosticSeverity> = None;

            move |chunk| {
                let syntax_highlight_style = chunk
                    .syntax_highlight_id
                    .and_then(|id| editor_style.syntax.get(id).cloned());

                let chunk_highlight = chunk.highlight_style.map(|chunk_highlight| {
                    HighlightStyle {
                        // For color inlays, blend the color with the editor background
                        // if the color has transparency (alpha < 1.0)
                        color: chunk_highlight.color.map(|color| {
                            if chunk.is_inlay && !color.is_opaque() {
                                editor_style.background.blend(color)
                            } else {
                                color
                            }
                        }),
                        underline: chunk_highlight
                            .underline
                            .filter(|_| editor_style.show_underlines),
                        ..chunk_highlight
                    }
                });

                let (diagnostic_highlight, diagnostic_severity) = if chunk.is_inlay {
                    (
                        current_diagnostic_underline.map(|underline| HighlightStyle {
                            underline: Some(underline),
                            ..Default::default()
                        }),
                        current_diagnostic_severity,
                    )
                } else {
                    let severity = chunk
                        .diagnostic_severity
                        .filter(|severity| self.diagnostic_severity_is_visible(*severity));
                    let highlight = severity.map(|severity| HighlightStyle {
                        fade_out: chunk
                            .is_unnecessary
                            .then_some(editor_style.unnecessary_code_fade),
                        underline: self.diagnostic_underline_style(
                            severity,
                            chunk.underline,
                            chunk.is_unnecessary,
                            editor_style,
                        ),
                        ..Default::default()
                    });

                    current_diagnostic_underline = highlight.as_ref().and_then(|h| h.underline);
                    current_diagnostic_severity =
                        current_diagnostic_underline.and_then(|_| severity);
                    (highlight, current_diagnostic_severity)
                };

                let style = [
                    syntax_highlight_style,
                    chunk_highlight,
                    diagnostic_highlight,
                ]
                .into_iter()
                .flatten()
                .reduce(|acc, highlight| acc.highlight(highlight));

                HighlightedChunk {
                    text: chunk.text,
                    style,
                    diagnostic_underline_severity: diagnostic_severity,
                    is_tab: chunk.is_tab,
                    is_inlay: chunk.is_inlay,
                    replacement: chunk.renderer.map(ChunkReplacement::Renderer),
                }
                .highlight_invisibles(editor_style)
            }
        })
    }

    /// Returns combined highlight styles (tree-sitter syntax + semantic tokens)
    /// for a byte range within the specified buffer.
    /// Returned ranges are 0-based relative to `buffer_range.start`.
    pub(super) fn combined_highlights(
        &self,
        multibuffer_range: Range<MultiBufferOffset>,
        syntax_theme: &theme::SyntaxTheme,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let multibuffer = self.buffer_snapshot();

        let chunks = custom_highlights::CustomHighlightsChunks::new(
            multibuffer_range,
            LanguageAwareStyling {
                tree_sitter: true,
                diagnostics: true,
            },
            None,
            Some(&self.semantic_token_highlights),
            multibuffer,
        );

        let mut highlights = Vec::new();
        let mut offset = 0usize;
        for chunk in chunks {
            let chunk_len = chunk.text.len();
            if chunk_len == 0 {
                continue;
            }

            let syntax_style = chunk
                .syntax_highlight_id
                .and_then(|id| syntax_theme.get(id).cloned());

            let overlay_style = chunk.highlight_style;

            let combined = match (syntax_style, overlay_style) {
                (Some(syntax), Some(overlay)) => Some(syntax.highlight(overlay)),
                (some @ Some(_), None) | (None, some @ Some(_)) => some,
                (None, None) => None,
            };

            if let Some(style) = combined {
                highlights.push((offset..offset + chunk_len, style));
            }
            offset += chunk_len;
        }
        highlights
    }

    #[instrument(skip_all)]
    pub fn layout_row(
        &self,
        display_row: DisplayRow,
        TextLayoutDetails {
            text_system,
            editor_style,
            rem_size,
            scroll_anchor: _,
            visible_rows: _,
            vertical_scroll_margin: _,
        }: &TextLayoutDetails,
    ) -> Arc<LineLayout> {
        let mut runs = Vec::new();
        let mut line = String::new();

        let range = display_row..display_row.next_row();
        for chunk in self.highlighted_chunks(
            range,
            LanguageAwareStyling {
                tree_sitter: false,
                diagnostics: false,
            },
            editor_style,
        ) {
            line.push_str(chunk.text);

            let text_style = if let Some(style) = chunk.style {
                Cow::Owned(editor_style.text.clone().highlight(style))
            } else {
                Cow::Borrowed(&editor_style.text)
            };

            runs.push(text_style.to_run(chunk.text.len()))
        }

        if line.ends_with('\n') {
            line.pop();
            if let Some(last_run) = runs.last_mut() {
                last_run.len -= 1;
                if last_run.len == 0 {
                    runs.pop();
                }
            }
        }

        let font_size = editor_style.text.font_size.to_pixels(*rem_size);
        text_system.layout_line(&line, font_size, &runs, None)
    }

    pub fn x_for_display_point(
        &self,
        display_point: DisplayPoint,
        text_layout_details: &TextLayoutDetails,
    ) -> Pixels {
        let line = self.layout_row(display_point.row(), text_layout_details);
        line.x_for_index(display_point.column() as usize)
    }

    pub fn display_column_for_x(
        &self,
        display_row: DisplayRow,
        x: Pixels,
        details: &TextLayoutDetails,
    ) -> u32 {
        let layout_line = self.layout_row(display_row, details);
        layout_line.closest_index_for_x(x) as u32
    }

    #[instrument(skip_all)]
    pub fn grapheme_at(&self, mut point: DisplayPoint) -> Option<SharedString> {
        point = DisplayPoint(self.block_snapshot.clip_point(point.0, Bias::Left));
        let chars = self
            .text_chunks(point.row())
            .flat_map(str::chars)
            .skip_while({
                let mut column = 0;
                move |char| {
                    let at_point = column >= point.column();
                    column += char.len_utf8() as u32;
                    !at_point
                }
            })
            .take_while({
                let mut prev = false;
                move |char| {
                    let now = char.is_ascii();
                    let end = char.is_ascii() && (char.is_ascii_whitespace() || prev);
                    prev = now;
                    !end
                }
            });
        chars.collect::<String>().graphemes(true).next().map(|s| {
            if let Some(invisible) = s.chars().next().filter(|&c| is_invisible(c)) {
                replacement(invisible).map_or_else(|| s.to_owned().into(), SharedString::from)
            } else if s == "\n" {
                " ".into()
            } else {
                s.to_owned().into()
            }
        })
    }

    pub fn buffer_chars_at(
        &self,
        mut offset: MultiBufferOffset,
    ) -> impl Iterator<Item = (char, MultiBufferOffset)> + '_ {
        self.buffer_snapshot().chars_at(offset).map(move |ch| {
            let ret = (ch, offset);
            offset += ch.len_utf8();
            ret
        })
    }

    pub fn reverse_buffer_chars_at(
        &self,
        mut offset: MultiBufferOffset,
    ) -> impl Iterator<Item = (char, MultiBufferOffset)> + '_ {
        self.buffer_snapshot()
            .reversed_chars_at(offset)
            .map(move |ch| {
                offset -= ch.len_utf8();
                (ch, offset)
            })
    }

    pub fn clip_point(&self, point: DisplayPoint, bias: Bias) -> DisplayPoint {
        let mut clipped = self.block_snapshot.clip_point(point.0, bias);
        if self.clip_at_line_ends {
            clipped = self.clip_at_line_end(DisplayPoint(clipped)).0
        }
        DisplayPoint(clipped)
    }

    pub fn clip_ignoring_line_ends(&self, point: DisplayPoint, bias: Bias) -> DisplayPoint {
        DisplayPoint(self.block_snapshot.clip_point(point.0, bias))
    }

    pub fn inlay_bias_at(&self, point: DisplayPoint) -> Option<Bias> {
        let wrap_point = self.block_snapshot.to_wrap_point(point.0, Bias::Left);
        let tab_point = self.block_snapshot.to_tab_point(wrap_point);
        let (fold_point, _, _) = self
            .block_snapshot
            .tab_snapshot
            .tab_point_to_fold_point(tab_point, Bias::Left);
        let inlay_point =
            fold_point.to_inlay_point(&self.block_snapshot.tab_snapshot.fold_snapshot);
        self.block_snapshot
            .tab_snapshot
            .fold_snapshot
            .inlay_bias_at_point(inlay_point)
    }

    pub fn clip_at_line_end(&self, display_point: DisplayPoint) -> DisplayPoint {
        let mut point = self.display_point_to_point(display_point, Bias::Left);

        if point.column != self.buffer_snapshot().line_len(MultiBufferRow(point.row)) {
            return display_point;
        }
        point.column = point.column.saturating_sub(1);
        point = self.buffer_snapshot().clip_point(point, Bias::Left);
        self.point_to_display_point(point, Bias::Left)
    }

    pub fn folds_in_range<T>(&self, range: Range<T>) -> impl Iterator<Item = &Fold>
    where
        T: ToOffset,
    {
        self.fold_snapshot().folds_in_range(range)
    }

    pub fn blocks_in_range(
        &self,
        rows: Range<DisplayRow>,
    ) -> impl Iterator<Item = (DisplayRow, &Block)> {
        self.block_snapshot
            .blocks_in_range(BlockRow(rows.start.0)..BlockRow(rows.end.0))
            .map(|(row, block)| (DisplayRow(row.0), block))
    }

    pub fn sticky_header_excerpt(&self, row: f64) -> Option<StickyHeaderExcerpt<'_>> {
        self.block_snapshot.sticky_header_excerpt(row)
    }

    pub fn block_for_id(&self, id: BlockId) -> Option<Block> {
        self.block_snapshot.block_for_id(id)
    }

    pub fn intersects_fold<T: ToOffset>(&self, offset: T) -> bool {
        self.fold_snapshot().intersects_fold(offset)
    }

    pub fn is_line_folded(&self, buffer_row: MultiBufferRow) -> bool {
        self.block_snapshot.is_line_replaced(buffer_row)
            || self.fold_snapshot().is_line_folded(buffer_row)
    }

    pub fn is_block_line(&self, display_row: DisplayRow) -> bool {
        self.block_snapshot.is_block_line(BlockRow(display_row.0))
    }

    pub fn is_folded_buffer_header(&self, display_row: DisplayRow) -> bool {
        self.block_snapshot
            .is_folded_buffer_header(BlockRow(display_row.0))
    }

    pub fn soft_wrap_indent(&self, display_row: DisplayRow) -> Option<u32> {
        let wrap_row = self
            .block_snapshot
            .to_wrap_point(BlockPoint::new(BlockRow(display_row.0), 0), Bias::Left)
            .row();
        self.wrap_snapshot().soft_wrap_indent(wrap_row)
    }

    pub fn text(&self) -> String {
        self.text_chunks(DisplayRow(0)).collect()
    }

    pub fn line(&self, display_row: DisplayRow) -> String {
        let mut result = String::new();
        for chunk in self.text_chunks(display_row) {
            if let Some(ix) = chunk.find('\n') {
                result.push_str(&chunk[0..ix]);
                break;
            } else {
                result.push_str(chunk);
            }
        }
        result
    }

    pub fn line_indent_for_buffer_row(&self, buffer_row: MultiBufferRow) -> LineIndent {
        self.buffer_snapshot().line_indent_for_row(buffer_row)
    }

    pub fn line_len(&self, row: DisplayRow) -> u32 {
        self.block_snapshot.line_len(BlockRow(row.0))
    }

    pub fn longest_row(&self) -> DisplayRow {
        DisplayRow(self.block_snapshot.longest_row().0)
    }

    pub fn longest_row_in_range(&self, range: Range<DisplayRow>) -> DisplayRow {
        let block_range = BlockRow(range.start.0)..BlockRow(range.end.0);
        let longest_row = self.block_snapshot.longest_row_in_range(block_range);
        DisplayRow(longest_row.0)
    }

    pub fn starts_indent(&self, buffer_row: MultiBufferRow) -> bool {
        let max_row = self.buffer_snapshot().max_row();
        if buffer_row >= max_row {
            return false;
        }

        let line_indent = self.line_indent_for_buffer_row(buffer_row);
        if line_indent.is_line_blank() {
            return false;
        }

        (buffer_row.0 + 1..=max_row.0)
            .find_map(|next_row| {
                let next_line_indent = self.line_indent_for_buffer_row(MultiBufferRow(next_row));
                if next_line_indent.raw_len() > line_indent.raw_len() {
                    Some(true)
                } else if !next_line_indent.is_line_blank() {
                    Some(false)
                } else {
                    None
                }
            })
            .unwrap_or(false)
    }

    /// Returns the indent length of `row` if it starts with a closing bracket.
    fn closing_bracket_indent_len(&self, row: u32) -> Option<u32> {
        let snapshot = self.buffer_snapshot();
        let indent_len = self
            .line_indent_for_buffer_row(MultiBufferRow(row))
            .raw_len();
        let content_start = Point::new(row, indent_len);
        let line_text: String = snapshot
            .chars_at(content_start)
            .take_while(|ch| *ch != '\n')
            .collect();

        let scope = snapshot.language_scope_at(Point::new(row, 0))?;
        if scope
            .brackets()
            .any(|(pair, _)| line_text.starts_with(&pair.end))
        {
            return Some(indent_len);
        }

        None
    }

    #[instrument(skip_all)]
    pub fn crease_for_buffer_row(&self, buffer_row: MultiBufferRow) -> Option<Crease<Point>> {
        let start =
            MultiBufferPoint::new(buffer_row.0, self.buffer_snapshot().line_len(buffer_row));
        if let Some(crease) = self
            .crease_snapshot
            .query_row(buffer_row, self.buffer_snapshot())
        {
            match crease {
                Crease::Inline {
                    range,
                    placeholder,
                    render_toggle,
                    render_trailer,
                    metadata,
                } => Some(Crease::Inline {
                    range: range.to_point(self.buffer_snapshot()),
                    placeholder: placeholder.clone(),
                    render_toggle: render_toggle.clone(),
                    render_trailer: render_trailer.clone(),
                    metadata: metadata.clone(),
                }),
                Crease::Block {
                    range,
                    block_height,
                    block_style,
                    render_block,
                    block_priority,
                    render_toggle,
                } => Some(Crease::Block {
                    range: range.to_point(self.buffer_snapshot()),
                    block_height: *block_height,
                    block_style: *block_style,
                    render_block: render_block.clone(),
                    block_priority: *block_priority,
                    render_toggle: render_toggle.clone(),
                }),
            }
        } else if !self.use_lsp_folding_ranges
            && self.starts_indent(MultiBufferRow(start.row))
            && !self.is_line_folded(MultiBufferRow(start.row))
        {
            let start_line_indent = self.line_indent_for_buffer_row(buffer_row);
            let snapshot = self.buffer_snapshot();
            let max_point = snapshot.max_point();
            let mut closing_row = None;

            // End byte of the smallest syntactic node enclosing `buffer_row`.
            // Used to tell standalone top-level comments (which terminate the
            // fold) apart from unindented content inside a multi-line string
            // or block comment belonging to the folded node (which does not).
            let foldable_node_end = {
                let row_start = Point::new(buffer_row.0, 0);
                let row_end = Point::new(buffer_row.0, snapshot.line_len(buffer_row));
                snapshot
                    .syntax_ancestor(row_start..row_end)
                    .map(|(_, range)| range.end)
            };

            for row in (buffer_row.0 + 1)..=max_point.row {
                let line_indent = self.line_indent_for_buffer_row(MultiBufferRow(row));
                if !line_indent.is_line_blank()
                    && line_indent.raw_len() <= start_line_indent.raw_len()
                {
                    let in_string_or_comment_scope = snapshot
                        .language_scope_at(Point::new(row, 0))
                        .is_some_and(|scope| {
                            matches!(
                                scope.override_name(),
                                Some("string") | Some("comment") | Some("comment.inclusive")
                            )
                        });
                    if in_string_or_comment_scope
                        && let Some(end) = foldable_node_end
                        && Point::new(row, 0).to_offset(snapshot) < end
                    {
                        continue;
                    }

                    closing_row = Some(row);
                    break;
                }
            }

            let last_non_blank_row = |from_row: u32| -> Point {
                let mut row = from_row;
                while row > start.row && self.buffer_snapshot().is_line_blank(MultiBufferRow(row)) {
                    row -= 1;
                }
                Point::new(row, self.buffer_snapshot().line_len(MultiBufferRow(row)))
            };

            let end = if let Some(row) = closing_row {
                if let Some(indent_len) = self.closing_bracket_indent_len(row) {
                    // Include newline and whitespace before closing delimiter,
                    // so it appears on the same display line as the fold placeholder
                    Point::new(row, indent_len)
                } else {
                    last_non_blank_row(row - 1)
                }
            } else {
                last_non_blank_row(max_point.row)
            };

            Some(Crease::Inline {
                range: start..end,
                placeholder: self.fold_placeholder.clone(),
                render_toggle: None,
                render_trailer: None,
                metadata: None,
            })
        } else {
            None
        }
    }

    #[cfg(feature = "test-support")]
    #[instrument(skip_all)]
    pub fn text_highlight_ranges(
        &self,
        key: HighlightKey,
    ) -> Option<Arc<(HighlightStyle, Vec<Range<Anchor>>)>> {
        self.text_highlights.get(&key).cloned()
    }

    #[cfg(feature = "test-support")]
    #[instrument(skip_all)]
    pub fn all_text_highlight_ranges(
        &self,
        f: &dyn Fn(&HighlightKey) -> bool,
    ) -> Vec<(gpui::Hsla, Range<Point>)> {
        use itertools::Itertools;

        self.text_highlights
            .iter()
            .filter(|(key, _)| f(key))
            .map(|(_, value)| value.clone())
            .flat_map(|ranges| {
                ranges
                    .1
                    .iter()
                    .flat_map(|range| {
                        Some((ranges.0.color?, range.to_point(self.buffer_snapshot())))
                    })
                    .collect::<Vec<_>>()
            })
            .sorted_by_key(|(_, range)| range.start)
            .collect()
    }

    #[allow(unused)]
    #[cfg(feature = "test-support")]
    pub(crate) fn inlay_highlights(
        &self,
        key: HighlightKey,
    ) -> Option<&TreeMap<InlayId, (HighlightStyle, InlayHighlight)>> {
        self.inlay_highlights.get(&key)
    }

    pub fn buffer_header_height(&self) -> u32 {
        self.block_snapshot.buffer_header_height
    }

    pub fn excerpt_header_height(&self) -> u32 {
        self.block_snapshot.excerpt_header_height
    }

    /// Given a `DisplayPoint`, returns another `DisplayPoint` corresponding to
    /// the start of the buffer row that is a given number of buffer rows away
    /// from the provided point.
    ///
    /// This moves by buffer rows instead of display rows, a distinction that is
    /// important when soft wrapping is enabled.
    #[instrument(skip_all)]
    pub fn start_of_relative_buffer_row(&self, point: DisplayPoint, times: isize) -> DisplayPoint {
        let start = self.display_point_to_fold_point(point, Bias::Left);
        let target = start.row() as isize + times;
        let new_row = (target.max(0) as u32).min(self.fold_snapshot().max_point().row());

        self.clip_point(
            self.fold_point_to_display_point(
                self.fold_snapshot()
                    .clip_point(FoldPoint::new(new_row, 0), Bias::Right),
            ),
            Bias::Right,
        )
    }

    pub(crate) fn fully_replaced_tab_rows(&self, row: u32) -> Option<RangeInclusive<u32>> {
        if !self.block_snapshot.has_replacement_blocks()
            || row > self.tab_snapshot().max_point().row()
        {
            return None;
        }
        let wraps = self.wrap_snapshot();
        let wrap_point = wraps.tab_point_to_wrap_point(TabPoint::new(row, 0));
        let input_range = self
            .block_snapshot
            .replacement_block_input_range(wrap_point.row())?;
        let start = wraps.to_tab_point(WrapPoint::new(input_range.start, 0));
        let start_row = start.row().checked_add(u32::from(start.column() > 0))?;
        let end_row = if input_range.end > wraps.max_point().row() {
            self.tab_snapshot().max_point().row()
        } else {
            wraps
                .to_tab_point(WrapPoint::new(input_range.end, 0))
                .row()
                .checked_sub(1)?
        };
        let rows = start_row..=end_row;
        rows.contains(&row).then_some(rows)
    }

    fn inlay_offset_to_display_point(&self, offset: InlayOffset, bias: Bias) -> DisplayPoint {
        let inlay_point = self.inlay_snapshot().to_point(offset);
        let fold_point = self.fold_snapshot().to_fold_point(inlay_point, bias);
        self.fold_point_to_display_point(fold_point)
    }
}

fn diagnostic_style(severity: lsp::DiagnosticSeverity, colors: &StatusColors) -> Hsla {
    match severity {
        lsp::DiagnosticSeverity::ERROR => colors.error,
        lsp::DiagnosticSeverity::WARNING => colors.warning,
        lsp::DiagnosticSeverity::INFORMATION => colors.info,
        lsp::DiagnosticSeverity::HINT => colors.hint,
        _ => colors.ignored,
    }
}

impl std::ops::Deref for DisplaySnapshot {
    type Target = BlockSnapshot;

    fn deref(&self) -> &Self::Target {
        &self.block_snapshot
    }
}

/// A zero-indexed point in a text buffer consisting of a row and column adjusted for inserted blocks.
#[derive(Copy, Clone, Default, Eq, Ord, PartialOrd, PartialEq)]
pub struct DisplayPoint(BlockPoint);

impl Debug for DisplayPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "DisplayPoint({}, {})",
            self.row().0,
            self.column()
        ))
    }
}

impl Add for DisplayPoint {
    type Output = Self;

    fn add(self, other: Self) -> Self::Output {
        DisplayPoint(BlockPoint(self.0.0 + other.0.0))
    }
}

impl Sub for DisplayPoint {
    type Output = Self;

    fn sub(self, other: Self) -> Self::Output {
        DisplayPoint(BlockPoint(self.0.0 - other.0.0))
    }
}

#[derive(Debug, Copy, Clone, Default, Eq, Ord, PartialOrd, PartialEq, Deserialize, Hash)]
#[serde(transparent)]
pub struct DisplayRow(pub u32);

impl DisplayRow {
    pub(crate) fn as_display_point(&self) -> DisplayPoint {
        DisplayPoint::new(*self, 0)
    }
}

impl Add<DisplayRow> for DisplayRow {
    type Output = Self;

    fn add(self, other: Self) -> Self::Output {
        DisplayRow(self.0 + other.0)
    }
}

impl Add<u32> for DisplayRow {
    type Output = Self;

    fn add(self, other: u32) -> Self::Output {
        DisplayRow(self.0 + other)
    }
}

impl Sub<DisplayRow> for DisplayRow {
    type Output = Self;

    fn sub(self, other: Self) -> Self::Output {
        DisplayRow(self.0 - other.0)
    }
}

impl Sub<u32> for DisplayRow {
    type Output = Self;

    fn sub(self, other: u32) -> Self::Output {
        DisplayRow(self.0 - other)
    }
}

impl DisplayPoint {
    pub fn new(row: DisplayRow, column: u32) -> Self {
        Self(BlockPoint(Point::new(row.0, column)))
    }

    pub fn zero() -> Self {
        Self::new(DisplayRow(0), 0)
    }

    pub fn is_zero(&self) -> bool {
        self.0.is_zero()
    }

    pub fn row(self) -> DisplayRow {
        DisplayRow(self.0.row)
    }

    pub fn column(self) -> u32 {
        self.0.column
    }

    pub fn row_mut(&mut self) -> &mut u32 {
        &mut self.0.row
    }

    pub fn column_mut(&mut self) -> &mut u32 {
        &mut self.0.column
    }

    pub fn to_point(self, map: &DisplaySnapshot) -> Point {
        map.display_point_to_point(self, Bias::Left)
    }

    pub fn to_offset(self, map: &DisplaySnapshot, bias: Bias) -> MultiBufferOffset {
        let wrap_point = map.block_snapshot.to_wrap_point(self.0, bias);
        let tab_point = map.wrap_snapshot().to_tab_point(wrap_point);
        let fold_point = map
            .tab_snapshot()
            .tab_point_to_fold_point(tab_point, bias)
            .0;
        let inlay_point = fold_point.to_inlay_point(map.fold_snapshot());
        map.inlay_snapshot()
            .to_buffer_offset(map.inlay_snapshot().to_offset(inlay_point))
    }
}

impl ToDisplayPoint for MultiBufferOffset {
    fn to_display_point(&self, map: &DisplaySnapshot) -> DisplayPoint {
        map.point_to_display_point(self.to_point(map.buffer_snapshot()), Bias::Left)
    }
}

impl ToDisplayPoint for MultiBufferOffsetUtf16 {
    fn to_display_point(&self, map: &DisplaySnapshot) -> DisplayPoint {
        self.to_offset(map.buffer_snapshot()).to_display_point(map)
    }
}

impl ToDisplayPoint for Point {
    fn to_display_point(&self, map: &DisplaySnapshot) -> DisplayPoint {
        map.point_to_display_point(*self, Bias::Left)
    }
}

impl ToDisplayPoint for Anchor {
    fn to_display_point(&self, map: &DisplaySnapshot) -> DisplayPoint {
        self.to_point(map.buffer_snapshot()).to_display_point(map)
    }
}

/// Maps buffer offset ranges to `DisplayPoint` ranges covering only buffer text
/// (excluding inlay text), reusing cursor state across calls.
///
/// Created via [`DisplaySnapshot::display_point_converter`]. Each layer
/// (inlay -> fold -> tab -> wrap -> block) is backed by a forward-only cursor,
/// so it is most efficient when ranges are supplied with non-decreasing
/// offsets. If a range starts before the previous one ended, the cursors are
/// transparently reset so the result stays correct (at the cost of an extra
/// seek), which keeps the converter robust to overlapping inputs such as the
/// base and buffer word diffs of an inline modified hunk.
pub struct DisplayPointConverter<'a> {
    inlay_cursor: BufferOffsetToInlayPointCursor<'a>,
    fold_point_cursor: FoldPointCursor<'a>,
    tab_point_cursor: TabPointCursor<'a>,
    wrap_point_cursor: WrapPointCursor<'a>,
    block_point_cursor: BlockPointCursor<'a>,
    prev_end: Option<MultiBufferOffset>,
}

impl DisplayPointConverter<'_> {
    pub fn map(&mut self, range: Range<MultiBufferOffset>) -> SmallVec<[Range<DisplayPoint>; 1]> {
        if self.prev_end.is_some_and(|prev_end| range.start < prev_end) {
            // The input went backward relative to where the cursors are
            // positioned; reset them so they can seek freely.
            self.inlay_cursor.reset();
            self.fold_point_cursor.reset();
            self.tab_point_cursor.reset();
            self.wrap_point_cursor.reset();
            self.block_point_cursor.reset();
        }
        self.prev_end = Some(range.end);

        let inlay_ranges = self.inlay_cursor.map(range);
        inlay_ranges
            .into_iter()
            .map(|inlay_range| {
                let start = self.inlay_point_to_display_point(inlay_range.start);
                let end = self.inlay_point_to_display_point(inlay_range.end);
                start..end
            })
            .collect()
    }

    fn inlay_point_to_display_point(&mut self, inlay_point: InlayPoint) -> DisplayPoint {
        let fold_point = self.fold_point_cursor.map(inlay_point, Bias::Left);
        let tab_point = self.tab_point_cursor.map(fold_point);
        let wrap_point = self.wrap_point_cursor.map(tab_point);
        DisplayPoint(self.block_point_cursor.map(wrap_point))
    }
}
