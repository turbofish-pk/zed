mod outline_panel_settings;

use anyhow::Context as _;
use collections::{BTreeMap, BTreeSet, HashMap, HashSet, IndexMap};
use db::kvp::KeyValueStore;
use editor::{
    AnchorRangeExt, Bias, DisplayPoint, Editor, EditorEvent, ExcerptRange, MultiBufferSnapshot,
    RangeToAnchorExt, SelectionEffects,
    display_map::ToDisplayPoint,
    items::{entry_git_aware_label_color, entry_label_color},
    scroll::{Autoscroll, ScrollAnchor},
};
use file_icons::FileIcons;

use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use git::status::{FileStatus, GitSummary};
use gpui::{
    Action, AnyElement, App, AppContext as _, AsyncWindowContext, Bounds, ClipboardItem, Context,
    DismissEvent, Div, ElementId, Entity, EventEmitter, FocusHandle, Focusable, HighlightStyle,
    InteractiveElement, IntoElement, KeyContext, ListHorizontalSizingBehavior, ListSizingBehavior,
    MouseButton, MouseDownEvent, ParentElement, Pixels, Point, Render, ScrollStrategy,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled, Subscription, Task,
    UniformListScrollHandle, WeakEntity, Window, actions, anchored, deferred, div, point, px, size,
    uniform_list,
};
use language::{Anchor, BufferId, BufferSnapshot, DiskState, OffsetRangeExt, OutlineItem};
use language::{LanguageAwareStyling, language_settings::LanguageSettings};

use menu::{Cancel, SelectFirst, SelectLast, SelectNext, SelectPrevious};
use std::{
    cmp,
    hash::Hash,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{self, AtomicBool},
    },
    time::Duration,
    u32,
};

use outline_panel_settings::{DockSide, FolderIndicator, OutlinePanelSettings, ShowIndentGuides};
use project::{File, Fs, Project, ProjectPath};
use search::{BufferSearchBar, ProjectSearchView};
use serde::{Deserialize, Serialize};
use settings::{Settings, SettingsStore};
use theme::{GlobalTheme, SyntaxTheme};
use theme_settings::ThemeSettings;
use ui::{
    ContextMenu, FluentBuilder, HighlightedLabel, IconButton, IconButtonShape, IndentGuideColors,
    IndentGuideLayout, KeyBinding, ListItem, ScrollAxes, Scrollbars, Tab, Tooltip, WithScrollbar,
    prelude::*,
};
use util::{RangeExt, ResultExt, TryFutureExt, debug_panic, rel_path::RelPath};
use workspace::{
    OpenInTerminal, WeakItemHandle, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    item::ItemHandle,
    searchable::{SearchEvent, SearchableItem},
};
use worktree::{PathProgress, PathTarget, WorktreeId};

use crate::outline_panel_settings::OutlinePanelSettingsScrollbarProxy;

actions!(
    outline_panel,
    [
        /// Collapses all entries in the outline tree.
        CollapseAllEntries,
        /// Collapses the currently selected entry.
        CollapseSelectedEntry,
        /// Expands all entries in the outline tree.
        ExpandAllEntries,
        /// Expands the currently selected entry.
        ExpandSelectedEntry,
        /// Folds the selected directory.
        FoldDirectory,
        /// Opens the selected entry in the editor.
        OpenSelectedEntry,
        /// Reveals the selected item in the system file manager.
        RevealInFileManager,
        /// Scroll half a page upwards
        ScrollUp,
        /// Scroll half a page downwards
        ScrollDown,
        /// Scroll until the cursor displays at the center
        ScrollCursorCenter,
        /// Scroll until the cursor displays at the top
        ScrollCursorTop,
        /// Scroll until the cursor displays at the bottom
        ScrollCursorBottom,
        /// Selects the parent of the current entry.
        SelectParent,
        /// Toggles the pin status of the active editor.
        ToggleActiveEditorPin,
        /// Toggles showing symbols, excerpts and search matches for multi-buffer views.
        ToggleSymbols,
        /// Unfolds the selected directory.
        UnfoldDirectory,
        /// Toggles the outline panel.
        Toggle,
        /// Toggles focus on the outline panel.
        ToggleFocus,
    ]
);

const OUTLINE_PANEL_KEY: &str = "OutlinePanel";
const UPDATE_DEBOUNCE: Duration = Duration::from_millis(50);

type Outline = OutlineItem<language::Anchor>;
type OutlineChildren = HashMap<(Range<Anchor>, usize), bool>;
type HighlightStyleData = Arc<OnceLock<Vec<(Range<usize>, HighlightStyle)>>>;

pub struct OutlinePanel {
    fs: Arc<dyn Fs>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    active: bool,
    pinned: bool,
    scroll_handle: UniformListScrollHandle,
    rendered_entries_len: usize,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    focus_handle: FocusHandle,
    pending_serialization: Task<Option<()>>,

    fs_entries: Vec<FsEntry>,
    fs_children_count: HashMap<WorktreeId, HashMap<Arc<RelPath>, FsChildren>>,
    collapsed_entries: HashSet<CollapsedEntry>,
    unfolded_dirs: HashMap<WorktreeId, BTreeSet<Arc<RelPath>>>,
    selected_entry: SelectedEntry,
    active_item: Option<ActiveItem>,
    _subscriptions: Vec<Subscription>,
    new_entries_for_fs_update: HashSet<BufferId>,
    fs_entries_update_task: Task<()>,
    fs_entries_update_pending: bool,
    cached_entries_update_task: Task<()>,
    cached_entries_update_pending: Option<CachedEntriesUpdate>,
    reveal_selection_task: Task<anyhow::Result<()>>,
    lsp_outline_refresh_task: Task<()>,
    outline_fetch_tasks: HashMap<BufferId, Task<()>>,
    buffers: HashMap<BufferId, BufferOutlines>,
    cached_entries: Vec<CachedEntry>,
    filter_editor: Entity<Editor>,
    mode: ItemsDisplayMode,
    max_width_item_index: Option<usize>,
    preserve_selection_on_buffer_fold_toggles: HashSet<BufferId>,
    pending_default_expansion_depth: Option<usize>,
    default_depth_applied_buffers: HashSet<BufferId>,
    outline_children_cache: HashMap<BufferId, OutlineChildren>,
    hide_symbols_override: Option<bool>,
}

#[derive(Debug)]
enum ItemsDisplayMode {
    Search(SearchState),
    Outline,
}

#[derive(Debug)]
struct SearchState {
    kind: SearchKind,
    query: String,
    matches: Vec<(Range<editor::Anchor>, Arc<OnceLock<SearchData>>)>,
    highlight_search_match_tx: async_channel::Sender<HighlightArguments>,
    _search_match_highlighter: Task<()>,
    _search_match_notify: Task<()>,
}

struct HighlightArguments {
    multi_buffer_snapshot: MultiBufferSnapshot,
    match_range: Range<editor::Anchor>,
    search_data: Arc<OnceLock<SearchData>>,
}

impl SearchState {
    fn new(
        kind: SearchKind,
        query: String,
        previous_matches: HashMap<Range<editor::Anchor>, Arc<OnceLock<SearchData>>>,
        new_matches: Vec<Range<editor::Anchor>>,
        theme: Arc<SyntaxTheme>,
        window: &mut Window,
        cx: &mut Context<OutlinePanel>,
    ) -> Self {
        let (highlight_search_match_tx, highlight_search_match_rx) = async_channel::unbounded();
        let (notify_tx, notify_rx) = async_channel::unbounded::<()>();
        Self {
            kind,
            query,
            matches: new_matches
                .into_iter()
                .map(|range| {
                    let search_data = previous_matches
                        .get(&range)
                        .map(Arc::clone)
                        .unwrap_or_default();
                    (range, search_data)
                })
                .collect(),
            highlight_search_match_tx,
            _search_match_highlighter: cx.background_spawn(async move {
                while let Ok(highlight_arguments) = highlight_search_match_rx.recv().await {
                    let needs_init = highlight_arguments.search_data.get().is_none();
                    let search_data = highlight_arguments.search_data.get_or_init(|| {
                        SearchData::new(
                            &highlight_arguments.match_range,
                            &highlight_arguments.multi_buffer_snapshot,
                        )
                    });
                    if needs_init {
                        notify_tx.try_send(()).ok();
                    }

                    let highlight_data = &search_data.highlights_data;
                    if highlight_data.get().is_some() {
                        continue;
                    }
                    let mut left_whitespaces_count = 0;
                    let mut non_whitespace_symbol_occurred = false;
                    let context_offset_range = search_data
                        .context_range
                        .to_offset(&highlight_arguments.multi_buffer_snapshot);
                    let mut offset = context_offset_range.start;
                    let mut context_text = String::new();
                    let mut highlight_ranges = Vec::new();
                    for mut chunk in highlight_arguments.multi_buffer_snapshot.chunks(
                        context_offset_range.start..context_offset_range.end,
                        LanguageAwareStyling {
                            tree_sitter: true,
                            diagnostics: true,
                        },
                    ) {
                        if !non_whitespace_symbol_occurred {
                            for c in chunk.text.chars() {
                                if c.is_whitespace() {
                                    left_whitespaces_count += c.len_utf8();
                                } else {
                                    non_whitespace_symbol_occurred = true;
                                    break;
                                }
                            }
                        }

                        if chunk.text.len() > context_offset_range.end - offset {
                            chunk.text = &chunk.text[0..(context_offset_range.end - offset)];
                            offset = context_offset_range.end;
                        } else {
                            offset += chunk.text.len();
                        }
                        let style = chunk
                            .syntax_highlight_id
                            .and_then(|highlight| theme.get(highlight).cloned());

                        if let Some(style) = style {
                            let start = context_text.len();
                            let end = start + chunk.text.len();
                            highlight_ranges.push((start..end, style));
                        }
                        context_text.push_str(chunk.text);
                        if offset >= context_offset_range.end {
                            break;
                        }
                    }

                    highlight_ranges.iter_mut().for_each(|(range, _)| {
                        range.start = range.start.saturating_sub(left_whitespaces_count);
                        range.end = range.end.saturating_sub(left_whitespaces_count);
                    });
                    if highlight_data.set(highlight_ranges).ok().is_some() {
                        notify_tx.try_send(()).ok();
                    }

                    let trimmed_text = context_text[left_whitespaces_count..].to_owned();
                    debug_assert_eq!(
                        trimmed_text, search_data.context_text,
                        "Highlighted text that does not match the buffer text"
                    );
                }
            }),
            _search_match_notify: cx.spawn_in(window, async move |outline_panel, cx| {
                loop {
                    match notify_rx.recv().await {
                        Ok(()) => {}
                        Err(_) => break,
                    };
                    while let Ok(()) = notify_rx.try_recv() {
                        //
                    }
                    let update_result = outline_panel.update(cx, |_, cx| {
                        cx.notify();
                    });
                    if update_result.is_err() {
                        break;
                    }
                }
            }),
        }
    }
}

#[derive(Debug)]
enum SelectedEntry {
    Invalidated(Option<PanelEntry>),
    Valid(PanelEntry, usize),
    None,
}

impl SelectedEntry {
    fn invalidate(&mut self) {
        match std::mem::replace(self, SelectedEntry::None) {
            Self::Valid(entry, _) => *self = Self::Invalidated(Some(entry)),
            Self::None => *self = Self::Invalidated(None),
            other => *self = other,
        }
    }

    fn is_invalidated(&self) -> bool {
        matches!(self, Self::Invalidated(_))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct FsChildren {
    files: usize,
    dirs: usize,
}

impl FsChildren {
    fn may_be_fold_part(&self) -> bool {
        self.dirs == 0 || (self.dirs == 1 && self.files == 0)
    }
}

#[derive(PartialEq, Eq)]
enum CachedEntriesUpdate {
    Entries,
    Contents,
}

#[derive(Clone, Debug)]
struct CachedEntry {
    depth: usize,
    string_match: Option<StringMatch>,
    entry: PanelEntry,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum CollapsedEntry {
    Dir(WorktreeId, Arc<RelPath>),
    File(WorktreeId, BufferId),
    ExternalFile(BufferId),
    Excerpt(ExcerptRange<Anchor>),
    Outline(Range<Anchor>),
}

struct BufferOutlines {
    excerpts: Vec<ExcerptRange<Anchor>>,
    outlines: OutlineState,
}

struct IndexedOutline<'a> {
    outline: &'a Outline,
    ranked_range: Range<usize>,
    collapsed: bool,
}

struct ExcerptOutlines<'a> {
    excerpt: &'a ExcerptRange<Anchor>,
    ranked_range: Range<usize>,
    expanded: bool,
    visible: Vec<&'a Outline>,
    children_to_cache: Vec<usize>,
}

impl IndexedOutline<'_> {
    fn is_hidden_by(&self, ancestor: &Self) -> bool {
        self.outline.depth > ancestor.outline.depth
            && ancestor.ranked_range.contains(&self.ranked_range.start)
    }

    fn cache_children(&self, next: Option<&Self>, children: &mut OutlineChildren) {
        children.insert(
            (self.outline.range.clone(), self.outline.depth),
            next.is_some_and(|next| next.outline.depth > self.outline.depth),
        );
    }
}

impl BufferOutlines {
    fn invalidate_outlines(&mut self) {
        if let OutlineState::Outlines(valid_outlines) = &mut self.outlines {
            self.outlines = OutlineState::Invalidated(std::mem::take(valid_outlines));
        }
    }

    fn iter_outlines(&self) -> impl Iterator<Item = &Outline> {
        match &self.outlines {
            OutlineState::Outlines(outlines) => outlines.iter(),
            OutlineState::Invalidated(outlines) => outlines.iter(),
            OutlineState::NotFetched => [].iter(),
        }
    }

    fn should_fetch_outlines(&self) -> bool {
        match &self.outlines {
            OutlineState::Outlines(_) => false,
            OutlineState::Invalidated(_) => true,
            OutlineState::NotFetched => true,
        }
    }

    fn assign_outline_cache_owners(
        outlines: &[IndexedOutline<'_>],
        excerpts: &mut [ExcerptOutlines<'_>],
    ) {
        let mut outline_order = (0..outlines.len()).collect::<Vec<_>>();
        let mut excerpt_order = excerpts
            .iter()
            .enumerate()
            .filter(|(_, excerpt)| excerpt.expanded)
            .collect::<Vec<_>>();
        outline_order
            .sort_unstable_by_key(|&index| cmp::Reverse(outlines[index].ranked_range.start));
        excerpt_order.sort_unstable_by_key(|(_, excerpt)| cmp::Reverse(excerpt.ranked_range.end));
        let mut owners = vec![None; outlines.len()];
        let mut frontier = BTreeMap::new();
        let mut cursor = 0;
        for index in outline_order {
            let outline = &outlines[index];
            while let Some(&(excerpt_index, excerpt)) = excerpt_order.get(cursor)
                && excerpt.ranked_range.end >= outline.ranked_range.start
            {
                Self::insert_outline_prefix_minimum(
                    &mut frontier,
                    excerpt.ranked_range.start,
                    cmp::Reverse(excerpt_index),
                );
                cursor += 1;
            }
            let last = frontier.range(..=outline.ranked_range.end).next_back();
            owners[index] = last.map(|(_, &cmp::Reverse(owner))| owner);
        }
        for (index, owner) in owners.into_iter().enumerate() {
            if let Some(owner) = owner {
                excerpts[owner].children_to_cache.push(index);
            }
        }
    }

    fn outlines_are_well_formed(outlines: &[IndexedOutline<'_>]) -> bool {
        if outlines
            .windows(2)
            .any(|pair| pair[0].ranked_range.start > pair[1].ranked_range.start)
        {
            return false;
        }
        let mut ancestors: Vec<&IndexedOutline<'_>> = Vec::new();
        for outline in outlines {
            if outline.ranked_range.start > outline.ranked_range.end {
                return false;
            }
            while let Some(&parent) = ancestors.last()
                && parent.outline.depth >= outline.outline.depth
            {
                ancestors.pop();
                if parent.ranked_range.end > outline.ranked_range.start {
                    return false;
                }
            }
            if ancestors
                .last()
                .is_some_and(|parent| outline.ranked_range.end > parent.ranked_range.end)
            {
                return false;
            }
            ancestors.push(outline);
        }
        true
    }

    fn collect_nested_outlines<'a>(
        outlines: &[IndexedOutline<'a>],
        excerpts: &mut [ExcerptOutlines<'a>],
        children: &mut OutlineChildren,
    ) {
        let mut collapsed_ancestor = None;
        let unfolded = outlines
            .iter()
            .enumerate()
            .filter_map(|(index, outline)| {
                if collapsed_ancestor.is_some_and(|ancestor| outline.is_hidden_by(ancestor)) {
                    return None;
                }
                collapsed_ancestor = outline.collapsed.then_some(outline);
                Some(index)
            })
            .collect::<BTreeSet<_>>();
        let mut active = BTreeSet::new();
        let mut unfolded_active = BTreeSet::new();
        let mut outline_order = (0..outlines.len()).collect::<Vec<_>>();
        let mut excerpt_order = excerpts
            .iter_mut()
            .filter(|excerpt| excerpt.expanded)
            .collect::<Vec<_>>();
        outline_order.sort_unstable_by_key(|&index| cmp::Reverse(outlines[index].ranked_range.end));
        excerpt_order.sort_unstable_by_key(|excerpt| cmp::Reverse(excerpt.ranked_range.start));
        let mut cursor = 0;
        for excerpt in excerpt_order {
            while let Some(&index) = outline_order.get(cursor)
                && outlines[index].ranked_range.end >= excerpt.ranked_range.start
            {
                active.insert(index);
                if unfolded.contains(&index) {
                    unfolded_active.insert(index);
                }
                cursor += 1;
            }
            let limit = outlines
                .partition_point(|outline| outline.ranked_range.start <= excerpt.ranked_range.end);
            excerpt.visible.extend(
                unfolded_active
                    .range(..limit)
                    .map(|&index| outlines[index].outline),
            );
            for &index in &excerpt.children_to_cache {
                let next = active
                    .range(index + 1..limit)
                    .next()
                    .map(|&next| &outlines[next]);
                outlines[index].cache_children(next, children);
            }
        }
    }

    fn collect_overlapping_outlines<'a>(
        outlines: &[IndexedOutline<'a>],
        excerpts: &mut [ExcerptOutlines<'a>],
        children: &mut OutlineChildren,
    ) {
        let leaf_count = outlines.len().next_power_of_two();
        let mut starts = vec![BTreeSet::new(); 2 * leaf_count];
        let mut depths = vec![BTreeMap::new(); 2 * leaf_count];
        let mut outline_order = (0..outlines.len()).collect::<Vec<_>>();
        let mut excerpt_order = excerpts
            .iter_mut()
            .filter(|excerpt| excerpt.expanded)
            .collect::<Vec<_>>();
        outline_order.sort_unstable_by_key(|&index| cmp::Reverse(outlines[index].ranked_range.end));
        excerpt_order.sort_unstable_by_key(|excerpt| cmp::Reverse(excerpt.ranked_range.start));
        let mut cursor = 0;
        for excerpt in excerpt_order {
            while let Some(&index) = outline_order.get(cursor)
                && outlines[index].ranked_range.end >= excerpt.ranked_range.start
            {
                let outline = &outlines[index];
                let mut node = leaf_count + index;
                while node > 0 {
                    starts[node].insert(outline.ranked_range.start);
                    Self::insert_outline_prefix_minimum(
                        &mut depths[node],
                        outline.ranked_range.start,
                        outline.outline.depth,
                    );
                    node /= 2;
                }
                cursor += 1;
            }
            for &index in &excerpt.children_to_cache {
                let next = Self::next_outline(index + 1, leaf_count, |node| {
                    starts[node]
                        .first()
                        .is_some_and(|&start| start <= excerpt.ranked_range.end)
                });
                outlines[index].cache_children(next.map(|next| &outlines[next]), children);
            }
            let mut collapsed_ancestor: Option<&IndexedOutline<'_>> = None;
            let mut first = 0;
            while let Some(index) = Self::next_outline(first, leaf_count, |node| {
                let Some(&first_start) = starts[node]
                    .first()
                    .filter(|&&start| start <= excerpt.ranked_range.end)
                else {
                    return false;
                };
                let Some(ancestor) = collapsed_ancestor else {
                    return true;
                };
                first_start < ancestor.ranked_range.start
                    || starts[node]
                        .range(..=excerpt.ranked_range.end)
                        .next_back()
                        .is_some_and(|&start| start >= ancestor.ranked_range.end)
                    || depths[node]
                        .range(..=excerpt.ranked_range.end)
                        .next_back()
                        .is_some_and(|(_, &depth)| depth <= ancestor.outline.depth)
            }) {
                let outline = &outlines[index];
                excerpt.visible.push(outline.outline);
                collapsed_ancestor = outline.collapsed.then_some(outline);
                first = index + 1;
            }
        }
    }

    fn next_outline(
        first: usize,
        leaf_count: usize,
        has_match: impl Fn(usize) -> bool,
    ) -> Option<usize> {
        if first >= leaf_count {
            return None;
        }
        let mut node = leaf_count + first;
        loop {
            while node.is_multiple_of(2) {
                node /= 2;
            }
            if has_match(node) {
                while node < leaf_count {
                    node *= 2;
                    if !has_match(node) {
                        node += 1;
                    }
                }
                return Some(node - leaf_count);
            }
            node += 1;
            if node.is_power_of_two() {
                return None;
            }
        }
    }

    fn insert_outline_prefix_minimum<T: Copy + Ord>(
        minimums: &mut BTreeMap<usize, T>,
        position: usize,
        value: T,
    ) {
        let previous = minimums.range(..=position).next_back();
        if previous.is_some_and(|(_, &current)| current <= value) {
            return;
        }
        while let Some((&next_position, &next_value)) = minimums.range(position..).next() {
            if next_value < value {
                break;
            }
            minimums.remove(&next_position);
        }
        minimums.insert(position, value);
    }
}

#[derive(Debug)]
enum OutlineState {
    Outlines(Vec<Outline>),
    Invalidated(Vec<Outline>),
    NotFetched,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FoldedDirsEntry {
    worktree_id: WorktreeId,
    first_buffer_id: BufferId,
    entries: Vec<FsEntryPath>,
}

// TODO: collapse the inner enums into panel entry
#[derive(Clone, Debug)]
enum PanelEntry {
    Fs(FsEntry),
    FoldedDirs(FoldedDirsEntry),
    Outline(OutlineEntry),
    Search(SearchEntry),
}

#[derive(Clone, Debug)]
struct SearchEntry {
    match_range: Range<editor::Anchor>,
    kind: SearchKind,
    render_data: Arc<OnceLock<SearchData>>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum SearchKind {
    Project,
    Buffer,
}

#[derive(Clone, Debug)]
struct SearchData {
    context_range: Range<editor::Anchor>,
    context_text: String,
    truncated_left: bool,
    truncated_right: bool,
    search_match_indices: Vec<Range<usize>>,
    highlights_data: HighlightStyleData,
}

struct SearchPrecomputed {
    multi_buffer_snapshot: MultiBufferSnapshot,
    matches_by_buffer: HashMap<BufferId, Vec<(Range<editor::Anchor>, Arc<OnceLock<SearchData>>)>>,
    folded_buffers: HashSet<BufferId>,
}

impl PartialEq for PanelEntry {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Fs(a), Self::Fs(b)) => a == b,
            (Self::FoldedDirs(left), Self::FoldedDirs(right)) => left == right,
            (Self::Outline(a), Self::Outline(b)) => a == b,
            (
                Self::Search(SearchEntry {
                    match_range: match_range_a,
                    kind: kind_a,
                    ..
                }),
                Self::Search(SearchEntry {
                    match_range: match_range_b,
                    kind: kind_b,
                    ..
                }),
            ) => match_range_a == match_range_b && kind_a == kind_b,
            _ => false,
        }
    }
}

impl Eq for PanelEntry {}

const SEARCH_MATCH_CONTEXT_SIZE: u32 = 40;
const TRUNCATED_CONTEXT_MARK: &str = "…";

impl SearchData {
    fn new(
        match_range: &Range<editor::Anchor>,
        multi_buffer_snapshot: &MultiBufferSnapshot,
    ) -> Self {
        let match_point_range = match_range.to_point(multi_buffer_snapshot);
        let context_left_border = multi_buffer_snapshot.clip_point(
            language::Point::new(
                match_point_range.start.row,
                match_point_range
                    .start
                    .column
                    .saturating_sub(SEARCH_MATCH_CONTEXT_SIZE),
            ),
            Bias::Left,
        );
        let context_right_border = multi_buffer_snapshot.clip_point(
            language::Point::new(
                match_point_range.end.row,
                match_point_range.end.column + SEARCH_MATCH_CONTEXT_SIZE,
            ),
            Bias::Right,
        );

        let context_anchor_range =
            (context_left_border..context_right_border).to_anchors(multi_buffer_snapshot);
        let context_offset_range = context_anchor_range.to_offset(multi_buffer_snapshot);
        let match_offset_range = match_range.to_offset(multi_buffer_snapshot);

        let mut search_match_indices = vec![
            match_offset_range.start - context_offset_range.start
                ..match_offset_range.end - context_offset_range.start,
        ];

        let entire_context_text = multi_buffer_snapshot
            .text_for_range(context_offset_range.clone())
            .collect::<String>();
        let left_whitespaces_offset = entire_context_text
            .chars()
            .take_while(|c| c.is_whitespace())
            .map(|c| c.len_utf8())
            .sum::<usize>();

        let mut extended_context_left_border = context_left_border;
        extended_context_left_border.column = extended_context_left_border.column.saturating_sub(1);
        let extended_context_left_border =
            multi_buffer_snapshot.clip_point(extended_context_left_border, Bias::Left);
        let mut extended_context_right_border = context_right_border;
        extended_context_right_border.column += 1;
        let extended_context_right_border =
            multi_buffer_snapshot.clip_point(extended_context_right_border, Bias::Right);

        let truncated_left = left_whitespaces_offset == 0
            && extended_context_left_border < context_left_border
            && multi_buffer_snapshot
                .chars_at(extended_context_left_border)
                .next()
                .is_some_and(|c| !c.is_whitespace());
        let truncated_right = entire_context_text
            .chars()
            .last()
            .is_none_or(|c| !c.is_whitespace())
            && extended_context_right_border > context_right_border
            && multi_buffer_snapshot
                .chars_at(extended_context_right_border)
                .next()
                .is_some_and(|c| !c.is_whitespace());
        search_match_indices.iter_mut().for_each(|range| {
            range.start = range.start.saturating_sub(left_whitespaces_offset);
            range.end = range.end.saturating_sub(left_whitespaces_offset);
        });

        let trimmed_row_offset_range =
            context_offset_range.start + left_whitespaces_offset..context_offset_range.end;
        let trimmed_text = entire_context_text[left_whitespaces_offset..].to_owned();
        Self {
            highlights_data: Arc::default(),
            search_match_indices,
            context_range: trimmed_row_offset_range.to_anchors(multi_buffer_snapshot),
            context_text: trimmed_text,
            truncated_left,
            truncated_right,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum OutlineEntry {
    Excerpt(ExcerptRange<Anchor>),
    Outline(Outline),
}

impl OutlineEntry {
    fn buffer_id(&self) -> BufferId {
        match self {
            OutlineEntry::Excerpt(excerpt) => excerpt.context.start.buffer_id,
            OutlineEntry::Outline(outline) => outline.range.start.buffer_id,
        }
    }

    fn range(&self) -> Range<Anchor> {
        match self {
            OutlineEntry::Excerpt(excerpt) => excerpt.context.clone(),
            OutlineEntry::Outline(outline) => outline.range.clone(),
        }
    }
}

#[derive(Debug, Clone, Eq)]
struct FsEntryPath {
    path: Arc<RelPath>,
    git_summary: GitSummary,
    is_ignored: bool,
}

impl PartialEq for FsEntryPath {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

#[derive(Debug, Clone, Eq)]
struct FsEntryFile {
    worktree_id: WorktreeId,
    entry: FsEntryPath,
    buffer_id: BufferId,
    excerpts: Arc<[ExcerptRange<Anchor>]>,
    is_deleted: bool,
}

impl PartialEq for FsEntryFile {
    fn eq(&self, other: &Self) -> bool {
        self.buffer_id == other.buffer_id
    }
}

impl Hash for FsEntryFile {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.buffer_id.hash(state);
    }
}

#[derive(Debug, Clone, Eq)]
struct FsEntryDirectory {
    worktree_id: WorktreeId,
    first_buffer_id: BufferId,
    entry: FsEntryPath,
}

impl PartialEq for FsEntryDirectory {
    fn eq(&self, other: &Self) -> bool {
        self.worktree_id == other.worktree_id
            && self.first_buffer_id == other.first_buffer_id
            && self.entry.path == other.entry.path
    }
}

impl Hash for FsEntryDirectory {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (self.worktree_id, self.first_buffer_id, &self.entry.path).hash(state);
    }
}

#[derive(Debug, Clone, Eq)]
struct FsEntryExternalFile {
    buffer_id: BufferId,
    excerpts: Arc<[ExcerptRange<Anchor>]>,
}

impl PartialEq for FsEntryExternalFile {
    fn eq(&self, other: &Self) -> bool {
        self.buffer_id == other.buffer_id
    }
}

impl Hash for FsEntryExternalFile {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.buffer_id.hash(state);
    }
}

#[derive(Clone, Debug, Eq)]
enum FsEntry {
    ExternalFile(FsEntryExternalFile),
    Directory(FsEntryDirectory),
    File(FsEntryFile),
}

impl PartialEq for FsEntry {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Directory(left), Self::Directory(right)) => left == right,
            _ => self
                .buffer_id()
                .is_some_and(|id| Some(id) == other.buffer_id()),
        }
    }
}

impl FsEntry {
    fn buffer_id(&self) -> Option<BufferId> {
        match self {
            Self::File(file) => Some(file.buffer_id),
            Self::ExternalFile(file) => Some(file.buffer_id),
            Self::Directory(_) => None,
        }
    }
}

fn directory_element_id(
    worktree_id: WorktreeId,
    first_buffer_id: BufferId,
    path: &RelPath,
) -> ElementId {
    ElementId::from((
        SharedString::from(format!("directory-{first_buffer_id}-{path}")),
        worktree_id.to_proto() as usize,
    ))
}

fn is_parent_row(entry: &PanelEntry, worktree_id: WorktreeId, parent_path: &RelPath) -> bool {
    match entry {
        PanelEntry::Fs(FsEntry::Directory(directory)) => {
            directory.worktree_id == worktree_id && directory.entry.path.as_ref() == parent_path
        }
        PanelEntry::FoldedDirs(folded_dirs) => {
            folded_dirs.worktree_id == worktree_id
                && folded_dirs
                    .entries
                    .last()
                    .is_some_and(|dir| dir.path.as_ref() == parent_path)
        }
        _ => false,
    }
}

fn is_deleted_file(file: &File, status: Option<FileStatus>, has_base_text: bool) -> bool {
    match file.disk_state {
        DiskState::Deleted => true,
        DiskState::Historic { was_deleted } => was_deleted,
        DiskState::Present { .. } => false,
        DiskState::New => status.is_some_and(|status| status.is_deleted()) || has_base_text,
    }
}

struct BufferExcerpts {
    is_new: bool,
    is_folded: bool,
    excerpts: Vec<ExcerptRange<Anchor>>,
    file: Option<(worktree::Snapshot, FsEntryPath)>,
    is_deleted: bool,
}

struct ActiveItem {
    item_handle: Box<dyn WeakItemHandle>,
    active_editor: WeakEntity<Editor>,
    _buffer_search_subscription: Subscription,
    _editor_subscription: Subscription,
}

#[derive(Debug)]
pub enum Event {
    Focus,
}

#[derive(Serialize, Deserialize)]
struct SerializedOutlinePanel {
    active: Option<bool>,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<OutlinePanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            if !workspace.toggle_panel_focus::<OutlinePanel>(window, cx) {
                workspace.close_panel::<OutlinePanel>(window, cx);
            }
        });
        workspace.register_action(|workspace, _: &ToggleSymbols, window, cx| {
            if let Some(outline_panel) = workspace.panel::<OutlinePanel>(cx) {
                // Defer to avoid updating the panel while the workspace is still being updated.
                window.defer(cx, move |window, cx| {
                    outline_panel.update(cx, |outline_panel, cx| {
                        outline_panel.toggle_symbols(&ToggleSymbols, window, cx);
                    });
                });
            }
        });
    })
    .detach();
}

impl OutlinePanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        let serialized_panel = match workspace
            .read_with(&cx, |workspace, _| {
                OutlinePanel::serialization_key(workspace)
            })
            .ok()
            .flatten()
        {
            Some(serialization_key) => {
                let kvp = cx.update(|_, cx| KeyValueStore::global(cx))?;
                cx.background_spawn(async move { kvp.read_kvp(&serialization_key) })
                    .await
                    .context("loading outline panel")
                    .log_err()
                    .flatten()
                    .map(|panel| serde_json::from_str::<SerializedOutlinePanel>(&panel))
                    .transpose()
                    .log_err()
                    .flatten()
            }
            None => None,
        };

        workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = Self::new(workspace, serialized_panel.as_ref(), window, cx);
            panel.update(cx, |_, cx| cx.notify());
            panel
        })
    }

    fn new(
        workspace: &mut Workspace,
        serialized: Option<&SerializedOutlinePanel>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let workspace_handle = cx.entity().downgrade();

        cx.new(|cx| {
            let filter_editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("Search buffer symbols…", window, cx);
                editor
            });
            let filter_update_subscription = cx.subscribe_in(
                &filter_editor,
                window,
                |outline_panel: &mut Self, _, event, window, cx| {
                    if let editor::EditorEvent::BufferEdited = event {
                        outline_panel.update_cached_entries(Some(UPDATE_DEBOUNCE), window, cx);
                    }
                },
            );

            let focus_handle = cx.focus_handle();
            let focus_subscription = cx.on_focus(&focus_handle, window, Self::focus_in);
            let workspace_subscription = cx.subscribe_in(
                &workspace
                    .weak_handle()
                    .upgrade()
                    .expect("have a &mut Workspace"),
                window,
                move |outline_panel, workspace, event, window, cx| {
                    if let workspace::Event::ActiveItemChanged = event {
                        if let Some((new_active_item, new_active_editor)) =
                            workspace_active_editor(workspace.read(cx), cx)
                        {
                            if outline_panel.should_replace_active_item(new_active_item.as_ref()) {
                                outline_panel.replace_active_editor(
                                    new_active_item,
                                    new_active_editor,
                                    window,
                                    cx,
                                );
                            }
                        } else {
                            outline_panel.clear_previous(window, cx);
                            cx.notify();
                        }
                    }
                },
            );

            let icons_subscription = cx.observe_global::<FileIcons>(|_, cx| {
                cx.notify();
            });

            let theme_subscription =
                cx.observe_global_in::<GlobalTheme>(window, |panel, window, cx| {
                    let buffers = panel.buffers.keys().copied().collect::<Vec<_>>();
                    panel.invalidate_outlines(&buffers);
                    panel.update_contents(Some(UPDATE_DEBOUNCE), window, cx);
                });

            let mut outline_panel_settings = *OutlinePanelSettings::get_global(cx);
            let mut current_theme = ThemeSettings::get_global(cx).clone();
            let mut document_symbols_by_buffer = HashMap::default();
            let settings_subscription =
                cx.observe_global_in::<SettingsStore>(window, move |outline_panel, window, cx| {
                    let new_settings = OutlinePanelSettings::get_global(cx);
                    let new_theme = ThemeSettings::get_global(cx);
                    let auto_fold_dirs_changed =
                        outline_panel_settings.auto_fold_dirs != new_settings.auto_fold_dirs;
                    let mut outlines_invalidated = false;
                    if &current_theme != new_theme {
                        outline_panel_settings = *new_settings;
                        current_theme = new_theme.clone();
                        outline_panel.outline_fetch_tasks.clear();
                        for buffer in outline_panel.buffers.values_mut() {
                            buffer.invalidate_outlines();
                        }
                        outlines_invalidated = true;
                        if matches!(outline_panel.selected_entry(), Some(PanelEntry::Outline(_))) {
                            outline_panel.selected_entry.invalidate();
                        }
                        outline_panel.update_contents(Some(UPDATE_DEBOUNCE), window, cx);
                    } else if &outline_panel_settings != new_settings {
                        let old_expansion_depth = outline_panel_settings.expand_outlines_with_depth;
                        let old_hide_symbols = outline_panel_settings.multi_buffer_hide_symbols;
                        outline_panel_settings = *new_settings;

                        let mut update_cached_entries = false;
                        if old_hide_symbols != outline_panel_settings.multi_buffer_hide_symbols {
                            outline_panel.hide_symbols_override = None;
                            outline_panel.update_non_fs_items(window, cx);
                            outline_panel.remap_selection_for_hidden_symbols(window, cx);
                            update_cached_entries = true;
                        }
                        if old_expansion_depth != outline_panel_settings.expand_outlines_with_depth
                        {
                            let old_collapsed_entries = outline_panel.collapsed_entries.clone();
                            outline_panel
                                .collapsed_entries
                                .retain(|entry| !matches!(entry, CollapsedEntry::Outline(..)));

                            let new_depth = outline_panel_settings.expand_outlines_with_depth;
                            outline_panel.pending_default_expansion_depth = Some(new_depth);
                            outline_panel.default_depth_applied_buffers = outline_panel
                                .buffers
                                .iter()
                                .filter(|(_, buffer)| {
                                    matches!(buffer.outlines, OutlineState::Outlines(_))
                                })
                                .map(|(buffer_id, _)| *buffer_id)
                                .collect();

                            for (buffer_id, buffer) in &outline_panel.buffers {
                                if let OutlineState::Outlines(outlines) = &buffer.outlines {
                                    for outline in outlines {
                                        if outline_panel
                                            .outline_children_cache
                                            .get(buffer_id)
                                            .and_then(|children_map| {
                                                let key = (outline.range.clone(), outline.depth);
                                                children_map.get(&key)
                                            })
                                            .copied()
                                            .unwrap_or(false)
                                            && (new_depth == 0 || outline.depth >= new_depth)
                                        {
                                            outline_panel.collapsed_entries.insert(
                                                CollapsedEntry::Outline(outline.range.clone()),
                                            );
                                        }
                                    }
                                }
                            }

                            if old_collapsed_entries != outline_panel.collapsed_entries {
                                update_cached_entries = true;
                            }
                        }
                        if update_cached_entries {
                            outline_panel.update_cached_entries(Some(UPDATE_DEBOUNCE), window, cx);
                        } else {
                            cx.notify();
                        }
                    }

                    if auto_fold_dirs_changed && let Some(editor) = outline_panel.active_editor() {
                        outline_panel.update_fs_entries(editor, Some(UPDATE_DEBOUNCE), window, cx);
                    }

                    if !outlines_invalidated {
                        let new_document_symbols = outline_panel
                            .buffers
                            .keys()
                            .filter_map(|buffer_id| {
                                let buffer = outline_panel
                                    .project
                                    .read(cx)
                                    .buffer_for_id(*buffer_id, cx)?;
                                let buffer = buffer.read(cx);
                                let doc_symbols =
                                    LanguageSettings::for_buffer(buffer, cx).document_symbols;
                                Some((*buffer_id, doc_symbols))
                            })
                            .collect();
                        if new_document_symbols != document_symbols_by_buffer {
                            document_symbols_by_buffer = new_document_symbols;
                            outline_panel.outline_fetch_tasks.clear();
                            for buffer in outline_panel.buffers.values_mut() {
                                buffer.invalidate_outlines();
                            }
                            if matches!(
                                outline_panel.selected_entry(),
                                Some(PanelEntry::Outline(_))
                            ) {
                                outline_panel.selected_entry.invalidate();
                            }
                            outline_panel.update_contents(Some(UPDATE_DEBOUNCE), window, cx);
                        }
                    }
                });

            let scroll_handle = UniformListScrollHandle::new();

            let mut outline_panel = Self {
                mode: ItemsDisplayMode::Outline,
                active: serialized.and_then(|s| s.active).unwrap_or(false),
                pinned: false,
                workspace: workspace_handle,
                project,
                fs: workspace.app_state().fs.clone(),
                max_width_item_index: None,
                scroll_handle,
                rendered_entries_len: 0,
                focus_handle,
                filter_editor,
                fs_entries: Vec::new(),
                fs_children_count: HashMap::default(),
                collapsed_entries: HashSet::default(),
                unfolded_dirs: HashMap::default(),
                selected_entry: SelectedEntry::None,
                context_menu: None,
                active_item: None,
                pending_serialization: Task::ready(None),
                new_entries_for_fs_update: HashSet::default(),
                preserve_selection_on_buffer_fold_toggles: HashSet::default(),
                pending_default_expansion_depth: None,
                default_depth_applied_buffers: HashSet::default(),
                fs_entries_update_task: Task::ready(()),
                fs_entries_update_pending: false,
                cached_entries_update_task: Task::ready(()),
                cached_entries_update_pending: None,
                reveal_selection_task: Task::ready(Ok(())),
                lsp_outline_refresh_task: Task::ready(()),
                outline_fetch_tasks: HashMap::default(),
                buffers: HashMap::default(),
                cached_entries: Vec::new(),
                _subscriptions: vec![
                    theme_subscription,
                    settings_subscription,
                    icons_subscription,
                    focus_subscription,
                    workspace_subscription,
                    filter_update_subscription,
                ],
                outline_children_cache: HashMap::default(),
                hide_symbols_override: None,
            };
            if let Some((item, editor)) = workspace_active_editor(workspace, cx) {
                outline_panel.replace_active_editor(item, editor, window, cx);
            }
            outline_panel
        })
    }

    fn serialization_key(workspace: &Workspace) -> Option<String> {
        workspace
            .database_id()
            .map(|id| i64::from(id).to_string())
            .or(workspace.session_id())
            .map(|id| format!("{}-{:?}", OUTLINE_PANEL_KEY, id))
    }

    fn serialize(&mut self, cx: &mut Context<Self>) {
        let Some(serialization_key) = self
            .workspace
            .read_with(cx, |workspace, _| {
                OutlinePanel::serialization_key(workspace)
            })
            .ok()
            .flatten()
        else {
            return;
        };
        let active = self.active.then_some(true);
        let kvp = KeyValueStore::global(cx);
        self.pending_serialization = cx.background_spawn(
            async move {
                kvp.write_kvp(
                    serialization_key,
                    serde_json::to_string(&SerializedOutlinePanel { active })?,
                )
                .await?;
                anyhow::Ok(())
            }
            .log_err(),
        );
    }

    fn dispatch_context(&self, window: &mut Window, cx: &mut Context<Self>) -> KeyContext {
        let mut dispatch_context = KeyContext::new_with_defaults(cx.platform_info().os);
        dispatch_context.add("OutlinePanel");
        dispatch_context.add("menu");
        let identifier = if self.filter_editor.focus_handle(cx).is_focused(window) {
            "editing"
        } else {
            "not_editing"
        };
        dispatch_context.add(identifier);
        dispatch_context
    }

    fn unfold_directory(
        &mut self,
        _: &UnfoldDirectory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(PanelEntry::FoldedDirs(FoldedDirsEntry {
            worktree_id,
            entries,
            ..
        })) = self.selected_entry().cloned()
        {
            self.unfolded_dirs
                .entry(worktree_id)
                .or_default()
                .extend(entries.iter().map(|entry| entry.path.clone()));
            self.update_cached_entries(None, window, cx);
        }
    }

    fn fold_directory(&mut self, _: &FoldDirectory, window: &mut Window, cx: &mut Context<Self>) {
        let (worktree_id, entry) = match self.selected_entry().cloned() {
            Some(PanelEntry::Fs(FsEntry::Directory(directory))) => {
                (directory.worktree_id, Some(directory.entry))
            }
            Some(PanelEntry::FoldedDirs(folded_dirs)) => {
                (folded_dirs.worktree_id, folded_dirs.entries.last().cloned())
            }
            _ => return,
        };
        let Some(entry) = entry else {
            return;
        };
        let unfolded_dirs = self.unfolded_dirs.get_mut(&worktree_id);
        let worktree = self
            .project
            .read(cx)
            .worktree_for_id(worktree_id, cx)
            .map(|w| w.read(cx).snapshot());
        let Some((_, unfolded_dirs)) = worktree.zip(unfolded_dirs) else {
            return;
        };

        unfolded_dirs.remove(&entry.path);
        self.update_cached_entries(None, window, cx);
    }

    fn open_selected_entry(
        &mut self,
        _: &OpenSelectedEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.filter_editor.focus_handle(cx).is_focused(window) {
            cx.propagate()
        } else if let Some(selected_entry) = self.selected_entry().cloned() {
            self.scroll_editor_to_entry(&selected_entry, true, true, window, cx);
        }
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter_editor.focus_handle(cx).is_focused(window) {
            self.focus_handle.focus(window, cx);
        } else {
            self.filter_editor.focus_handle(cx).focus(window, cx);
        }

        if self.context_menu.is_some() {
            self.context_menu.take();
            cx.notify();
        }
    }

    fn open_excerpts(
        &mut self,
        action: &editor::actions::OpenExcerpts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.filter_editor.focus_handle(cx).is_focused(window) {
            cx.propagate()
        } else if let Some((active_editor, selected_entry)) =
            self.active_editor().zip(self.selected_entry().cloned())
        {
            self.scroll_editor_to_entry(&selected_entry, true, true, window, cx);
            active_editor.update(cx, |editor, cx| editor.open_excerpts(action, window, cx));
        }
    }

    fn open_excerpts_split(
        &mut self,
        action: &editor::actions::OpenExcerptsSplit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.filter_editor.focus_handle(cx).is_focused(window) {
            cx.propagate()
        } else if let Some((active_editor, selected_entry)) =
            self.active_editor().zip(self.selected_entry().cloned())
        {
            self.scroll_editor_to_entry(&selected_entry, true, true, window, cx);
            active_editor.update(cx, |editor, cx| {
                editor.open_excerpts_in_split(action, window, cx)
            });
        }
    }

    fn scroll_editor_to_entry(
        &mut self,
        entry: &PanelEntry,
        prefer_selection_change: bool,
        prefer_focus_change: bool,
        window: &mut Window,
        cx: &mut Context<OutlinePanel>,
    ) {
        let Some(active_editor) = self.active_editor() else {
            return;
        };
        let active_multi_buffer = active_editor.read(cx).buffer().clone();
        let multi_buffer_snapshot = active_multi_buffer.read(cx).snapshot(cx);
        let mut change_selection = prefer_selection_change;
        let mut change_focus = prefer_focus_change;
        let mut scroll_to_buffer = None;
        let scroll_target = match entry {
            PanelEntry::FoldedDirs(FoldedDirsEntry {
                first_buffer_id, ..
            })
            | PanelEntry::Fs(FsEntry::Directory(FsEntryDirectory {
                first_buffer_id, ..
            })) => {
                change_selection = false;
                change_focus = false;
                scroll_to_buffer = Some(*first_buffer_id);
                multi_buffer_snapshot
                    .excerpts_for_buffer(*first_buffer_id)
                    .next()
                    .and_then(|excerpt_range| {
                        multi_buffer_snapshot.anchor_in_excerpt(excerpt_range.context.start)
                    })
            }
            PanelEntry::Fs(FsEntry::ExternalFile(file)) => {
                change_selection = false;
                scroll_to_buffer = Some(file.buffer_id);
                multi_buffer_snapshot.excerpts().find_map(|excerpt_range| {
                    if excerpt_range.context.start.buffer_id == file.buffer_id {
                        multi_buffer_snapshot.anchor_in_excerpt(excerpt_range.context.start)
                    } else {
                        None
                    }
                })
            }

            PanelEntry::Fs(FsEntry::File(file)) => {
                change_selection = false;
                scroll_to_buffer = Some(file.buffer_id);
                multi_buffer_snapshot
                    .excerpts_for_buffer(file.buffer_id)
                    .next()
                    .and_then(|excerpt_range| {
                        multi_buffer_snapshot.anchor_in_excerpt(excerpt_range.context.start)
                    })
            }
            PanelEntry::Outline(OutlineEntry::Outline(outline)) => multi_buffer_snapshot
                .anchor_in_excerpt(outline.range.start)
                .or_else(|| multi_buffer_snapshot.anchor_in_excerpt(outline.range.end)),
            PanelEntry::Outline(OutlineEntry::Excerpt(excerpt)) => {
                change_selection = false;
                change_focus = false;
                multi_buffer_snapshot.anchor_in_excerpt(excerpt.context.start)
            }
            PanelEntry::Search(search_entry) => Some(search_entry.match_range.start),
        };

        if let Some(anchor) = scroll_target {
            let activate = self
                .workspace
                .update(cx, |workspace, cx| match self.active_item() {
                    Some(active_item) => workspace.activate_item(
                        active_item.as_ref(),
                        true,
                        change_focus,
                        window,
                        cx,
                    ),
                    None => workspace.activate_item(&active_editor, true, change_focus, window, cx),
                });

            if activate.is_ok() {
                self.select_entry(entry.clone(), true, window, cx);
                if change_selection {
                    active_editor.update(cx, |editor, cx| {
                        editor.change_selections(
                            SelectionEffects::scroll(Autoscroll::center()).unfold(),
                            window,
                            cx,
                            |s| s.select_ranges(Some(anchor..anchor)),
                        );
                    });
                } else {
                    let mut offset = Point::default();
                    if let Some(buffer_id) = scroll_to_buffer
                        && multi_buffer_snapshot.as_singleton().is_none()
                        && !active_editor.read(cx).is_buffer_folded(buffer_id, cx)
                    {
                        offset.y = -(active_editor.read(cx).file_header_size() as f64);
                    }

                    active_editor.update(cx, |editor, cx| {
                        editor.set_scroll_anchor(ScrollAnchor { offset, anchor }, window, cx);
                    });
                }

                if change_focus {
                    active_editor.focus_handle(cx).focus(window, cx);
                } else {
                    self.focus_handle.focus(window, cx);
                }
            }
        }
    }

    fn scroll_up(&mut self, _: &ScrollUp, window: &mut Window, cx: &mut Context<Self>) {
        for _ in 0..self.rendered_entries_len / 2 {
            window.dispatch_action(SelectPrevious.boxed_clone(), cx);
        }
    }

    fn scroll_down(&mut self, _: &ScrollDown, window: &mut Window, cx: &mut Context<Self>) {
        for _ in 0..self.rendered_entries_len / 2 {
            window.dispatch_action(SelectNext.boxed_clone(), cx);
        }
    }

    fn scroll_cursor_center(
        &mut self,
        _: &ScrollCursorCenter,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(selected_entry) = self.selected_entry() {
            let index = self
                .cached_entries
                .iter()
                .position(|cached_entry| &cached_entry.entry == selected_entry);
            if let Some(index) = index {
                self.scroll_handle
                    .scroll_to_item_strict(index, ScrollStrategy::Center);
                cx.notify();
            }
        }
    }

    fn scroll_cursor_top(&mut self, _: &ScrollCursorTop, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(selected_entry) = self.selected_entry() {
            let index = self
                .cached_entries
                .iter()
                .position(|cached_entry| &cached_entry.entry == selected_entry);
            if let Some(index) = index {
                self.scroll_handle
                    .scroll_to_item_strict(index, ScrollStrategy::Top);
                cx.notify();
            }
        }
    }

    fn scroll_cursor_bottom(
        &mut self,
        _: &ScrollCursorBottom,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(selected_entry) = self.selected_entry() {
            let index = self
                .cached_entries
                .iter()
                .position(|cached_entry| &cached_entry.entry == selected_entry);
            if let Some(index) = index {
                self.scroll_handle
                    .scroll_to_item_strict(index, ScrollStrategy::Bottom);
                cx.notify();
            }
        }
    }

    fn select_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry_to_select) = self.selected_entry().and_then(|selected_entry| {
            self.cached_entries
                .iter()
                .map(|cached_entry| &cached_entry.entry)
                .skip_while(|entry| entry != &selected_entry)
                .nth(1)
                .cloned()
        }) {
            self.select_entry(entry_to_select, true, window, cx);
        } else {
            self.select_first(&SelectFirst {}, window, cx)
        }
        if let Some(selected_entry) = self.selected_entry().cloned() {
            self.scroll_editor_to_entry(&selected_entry, true, false, window, cx);
        }
    }

    fn select_previous(&mut self, _: &SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry_to_select) = self.selected_entry().and_then(|selected_entry| {
            self.cached_entries
                .iter()
                .rev()
                .map(|cached_entry| &cached_entry.entry)
                .skip_while(|entry| entry != &selected_entry)
                .nth(1)
                .cloned()
        }) {
            self.select_entry(entry_to_select, true, window, cx);
        } else {
            self.select_last(&SelectLast, window, cx)
        }
        if let Some(selected_entry) = self.selected_entry().cloned() {
            self.scroll_editor_to_entry(&selected_entry, true, false, window, cx);
        }
    }

    fn select_parent(&mut self, _: &SelectParent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry_to_select) = self.selected_entry().and_then(|selected_entry| {
            let mut previous_entries = self
                .cached_entries
                .iter()
                .rev()
                .map(|cached_entry| &cached_entry.entry)
                .skip_while(|entry| entry != &selected_entry)
                .skip(1);
            match &selected_entry {
                PanelEntry::Fs(fs_entry) => match fs_entry {
                    FsEntry::ExternalFile(..) => None,
                    FsEntry::File(FsEntryFile {
                        worktree_id, entry, ..
                    })
                    | FsEntry::Directory(FsEntryDirectory {
                        worktree_id, entry, ..
                    }) => entry.path.parent().and_then(|parent_path| {
                        previous_entries
                            .find(|entry| is_parent_row(entry, *worktree_id, parent_path))
                    }),
                },
                PanelEntry::FoldedDirs(folded_dirs) => folded_dirs
                    .entries
                    .first()
                    .and_then(|entry| entry.path.parent())
                    .and_then(|parent_path| {
                        previous_entries.find(|entry| {
                            is_parent_row(entry, folded_dirs.worktree_id, parent_path)
                        })
                    }),
                PanelEntry::Outline(OutlineEntry::Excerpt(excerpt)) => {
                    previous_entries.find(|entry| match entry {
                        PanelEntry::Fs(FsEntry::File(file)) => {
                            file.buffer_id == excerpt.context.start.buffer_id
                                && file.excerpts.contains(&excerpt)
                        }
                        PanelEntry::Fs(FsEntry::ExternalFile(external_file)) => {
                            external_file.buffer_id == excerpt.context.start.buffer_id
                                && external_file.excerpts.contains(&excerpt)
                        }
                        _ => false,
                    })
                }
                PanelEntry::Outline(OutlineEntry::Outline(outline)) => {
                    previous_entries.find(|entry| {
                        if let PanelEntry::Outline(OutlineEntry::Excerpt(excerpt)) = entry {
                            if outline.range.start.buffer_id != excerpt.context.start.buffer_id {
                                return false;
                            }
                            let Some(buffer_snapshot) =
                                self.buffer_snapshot_for_id(outline.range.start.buffer_id, cx)
                            else {
                                return false;
                            };
                            excerpt.contains(&outline.range.start, &buffer_snapshot)
                                || excerpt.contains(&outline.range.end, &buffer_snapshot)
                        } else {
                            false
                        }
                    })
                }
                PanelEntry::Search(_) => {
                    previous_entries.find(|entry| !matches!(entry, PanelEntry::Search(_)))
                }
            }
        }) {
            self.select_entry(entry_to_select.clone(), true, window, cx);
        } else {
            self.select_first(&SelectFirst {}, window, cx);
        }
    }

    fn select_first(&mut self, _: &SelectFirst, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(first_entry) = self.cached_entries.first() {
            self.select_entry(first_entry.entry.clone(), true, window, cx);
        }
    }

    fn select_last(&mut self, _: &SelectLast, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(new_selection) = self
            .cached_entries
            .iter()
            .rev()
            .map(|cached_entry| &cached_entry.entry)
            .next()
        {
            self.select_entry(new_selection.clone(), true, window, cx);
        }
    }

    fn autoscroll(&mut self, cx: &mut Context<Self>) {
        if let Some(selected_entry) = self.selected_entry() {
            let index = self
                .cached_entries
                .iter()
                .position(|cached_entry| &cached_entry.entry == selected_entry);
            if let Some(index) = index {
                self.scroll_handle
                    .scroll_to_item(index, ScrollStrategy::Center);
                cx.notify();
            }
        }
    }

    fn focus_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_handle.contains_focused(window, cx) {
            cx.emit(Event::Focus);
        }
    }

    fn deploy_context_menu(
        &mut self,
        position: Point<Pixels>,
        entry: PanelEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_entry(entry.clone(), true, window, cx);
        let is_root = match &entry {
            PanelEntry::Fs(FsEntry::File(FsEntryFile { entry, .. }))
            | PanelEntry::Fs(FsEntry::Directory(FsEntryDirectory { entry, .. })) => {
                entry.path.is_empty()
            }
            PanelEntry::FoldedDirs(folded_dirs) => folded_dirs
                .entries
                .first()
                .is_some_and(|entry| entry.path.is_empty()),
            PanelEntry::Fs(FsEntry::ExternalFile(_)) => false,
            PanelEntry::Outline(_) | PanelEntry::Search(_) => {
                cx.notify();
                return;
            }
        };
        let auto_fold_dirs = OutlinePanelSettings::get_global(cx).auto_fold_dirs;
        let is_foldable = auto_fold_dirs && !is_root && self.is_foldable(&entry);
        let is_unfoldable = auto_fold_dirs && !is_root && self.is_unfoldable(&entry);

        let context_menu = ContextMenu::build(window, cx, |menu, _, _| {
            menu.context(self.focus_handle.clone())
                .action(
                    "Reveal in File Manager",
                    Box::new(RevealInFileManager),
                )
                .action("Open in Terminal", Box::new(OpenInTerminal))
                .when(is_unfoldable, |menu| {
                    menu.action("Unfold Directory", Box::new(UnfoldDirectory))
                })
                .when(is_foldable, |menu| {
                    menu.action("Fold Directory", Box::new(FoldDirectory))
                })
                .separator()
                .action("Copy Path", Box::new(zed_actions::workspace::CopyPath))
                .action(
                    "Copy Relative Path",
                    Box::new(zed_actions::workspace::CopyRelativePath),
                )
        });
        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&context_menu, |outline_panel, _, _: &DismissEvent, cx| {
            outline_panel.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((context_menu, position, subscription));
        cx.notify();
    }

    fn is_unfoldable(&self, entry: &PanelEntry) -> bool {
        matches!(entry, PanelEntry::FoldedDirs(..))
    }

    fn is_foldable(&self, entry: &PanelEntry) -> bool {
        let (directory_worktree, directory_entry) = match entry {
            PanelEntry::Fs(FsEntry::Directory(FsEntryDirectory {
                worktree_id,
                entry: directory_entry,
                ..
            })) => (*worktree_id, Some(directory_entry)),
            _ => return false,
        };
        let Some(directory_entry) = directory_entry else {
            return false;
        };

        if self
            .unfolded_dirs
            .get(&directory_worktree)
            .is_none_or(|unfolded_dirs| !unfolded_dirs.contains(&directory_entry.path))
        {
            return false;
        }

        let children = self
            .fs_children_count
            .get(&directory_worktree)
            .and_then(|entries| entries.get(&directory_entry.path))
            .copied()
            .unwrap_or_default();

        children.may_be_fold_part() && children.dirs > 0
    }

    fn expand_selected_entry(
        &mut self,
        _: &ExpandSelectedEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active_editor) = self.active_editor() else {
            return;
        };
        let Some(selected_entry) = self.selected_entry().cloned() else {
            return;
        };
        let mut buffers_to_unfold = HashSet::default();
        let entry_to_expand = match &selected_entry {
            PanelEntry::FoldedDirs(FoldedDirsEntry {
                entries: dir_entries,
                worktree_id,
                ..
            }) => dir_entries.last().map(|entry| {
                buffers_to_unfold.extend(self.buffers_inside_directory(*worktree_id, entry));
                CollapsedEntry::Dir(*worktree_id, entry.path.clone())
            }),
            PanelEntry::Fs(FsEntry::Directory(FsEntryDirectory {
                worktree_id, entry, ..
            })) => {
                buffers_to_unfold.extend(self.buffers_inside_directory(*worktree_id, entry));
                Some(CollapsedEntry::Dir(*worktree_id, entry.path.clone()))
            }
            PanelEntry::Fs(FsEntry::File(FsEntryFile {
                worktree_id,
                buffer_id,
                ..
            })) => {
                buffers_to_unfold.insert(*buffer_id);
                Some(CollapsedEntry::File(*worktree_id, *buffer_id))
            }
            PanelEntry::Fs(FsEntry::ExternalFile(external_file)) => {
                buffers_to_unfold.insert(external_file.buffer_id);
                Some(CollapsedEntry::ExternalFile(external_file.buffer_id))
            }
            PanelEntry::Outline(OutlineEntry::Excerpt(excerpt)) => {
                Some(CollapsedEntry::Excerpt(excerpt.clone()))
            }
            PanelEntry::Outline(OutlineEntry::Outline(outline)) => {
                Some(CollapsedEntry::Outline(outline.range.clone()))
            }
            PanelEntry::Search(_) => return,
        };
        let Some(collapsed_entry) = entry_to_expand else {
            return;
        };
        let expanded = self.collapsed_entries.remove(&collapsed_entry);
        if expanded {
            active_editor.update(cx, |editor, cx| {
                buffers_to_unfold.retain(|buffer_id| editor.is_buffer_folded(*buffer_id, cx));
            });
            self.select_entry(selected_entry, true, window, cx);
            if buffers_to_unfold.is_empty() {
                self.update_cached_entries(None, window, cx);
            } else {
                self.toggle_buffers_fold(buffers_to_unfold, false, true, window, cx)
                    .detach();
            }
        } else {
            self.select_next(&SelectNext, window, cx)
        }
    }

    fn collapse_selected_entry(
        &mut self,
        _: &CollapseSelectedEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active_editor) = self.active_editor() else {
            return;
        };
        let Some(selected_entry) = self.selected_entry().cloned() else {
            return;
        };

        let mut buffers_to_fold = HashSet::default();
        let collapsed = match &selected_entry {
            PanelEntry::Fs(FsEntry::Directory(FsEntryDirectory {
                worktree_id, entry, ..
            })) => {
                if self
                    .collapsed_entries
                    .insert(CollapsedEntry::Dir(*worktree_id, entry.path.clone()))
                {
                    buffers_to_fold.extend(self.buffers_inside_directory(*worktree_id, entry));
                    true
                } else {
                    false
                }
            }
            PanelEntry::Fs(FsEntry::File(FsEntryFile {
                worktree_id,
                buffer_id,
                ..
            })) => {
                if self
                    .collapsed_entries
                    .insert(CollapsedEntry::File(*worktree_id, *buffer_id))
                {
                    buffers_to_fold.insert(*buffer_id);
                    true
                } else {
                    false
                }
            }
            PanelEntry::Fs(FsEntry::ExternalFile(external_file)) => {
                if self
                    .collapsed_entries
                    .insert(CollapsedEntry::ExternalFile(external_file.buffer_id))
                {
                    buffers_to_fold.insert(external_file.buffer_id);
                    true
                } else {
                    false
                }
            }
            PanelEntry::FoldedDirs(folded_dirs) => {
                let mut folded = false;
                if let Some(dir_entry) = folded_dirs.entries.last()
                    && self.collapsed_entries.insert(CollapsedEntry::Dir(
                        folded_dirs.worktree_id,
                        dir_entry.path.clone(),
                    ))
                {
                    folded = true;
                    buffers_to_fold
                        .extend(self.buffers_inside_directory(folded_dirs.worktree_id, dir_entry));
                }
                folded
            }
            PanelEntry::Outline(OutlineEntry::Excerpt(excerpt)) => self
                .collapsed_entries
                .insert(CollapsedEntry::Excerpt(excerpt.clone())),
            PanelEntry::Outline(OutlineEntry::Outline(outline)) => self
                .collapsed_entries
                .insert(CollapsedEntry::Outline(outline.range.clone())),
            PanelEntry::Search(_) => false,
        };

        if collapsed {
            active_editor.update(cx, |editor, cx| {
                buffers_to_fold.retain(|buffer_id| !editor.is_buffer_folded(*buffer_id, cx));
            });
            self.select_entry(selected_entry, true, window, cx);
            if buffers_to_fold.is_empty() {
                self.update_cached_entries(None, window, cx);
            } else {
                self.toggle_buffers_fold(buffers_to_fold, true, true, window, cx)
                    .detach();
            }
        } else {
            self.select_parent(&SelectParent, window, cx);
        }
    }

    pub fn expand_all_entries(
        &mut self,
        _: &ExpandAllEntries,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active_editor) = self.active_editor() else {
            return;
        };

        let mut to_uncollapse: HashSet<CollapsedEntry> = HashSet::default();
        let mut buffers_to_unfold: HashSet<BufferId> = HashSet::default();

        for fs_entry in &self.fs_entries {
            match fs_entry {
                FsEntry::File(FsEntryFile {
                    worktree_id,
                    buffer_id,
                    ..
                }) => {
                    to_uncollapse.insert(CollapsedEntry::File(*worktree_id, *buffer_id));
                    buffers_to_unfold.insert(*buffer_id);
                }
                FsEntry::ExternalFile(FsEntryExternalFile { buffer_id, .. }) => {
                    to_uncollapse.insert(CollapsedEntry::ExternalFile(*buffer_id));
                    buffers_to_unfold.insert(*buffer_id);
                }
                FsEntry::Directory(FsEntryDirectory {
                    worktree_id, entry, ..
                }) => {
                    to_uncollapse.insert(CollapsedEntry::Dir(*worktree_id, entry.path.clone()));
                }
            }
        }

        for (_buffer_id, buffer) in &self.buffers {
            match &buffer.outlines {
                OutlineState::Outlines(outlines) => {
                    for outline in outlines {
                        to_uncollapse.insert(CollapsedEntry::Outline(outline.range.clone()));
                    }
                }
                OutlineState::Invalidated(outlines) => {
                    for outline in outlines {
                        to_uncollapse.insert(CollapsedEntry::Outline(outline.range.clone()));
                    }
                }
                OutlineState::NotFetched => {}
            }
            to_uncollapse.extend(
                buffer
                    .excerpts
                    .iter()
                    .map(|excerpt| CollapsedEntry::Excerpt(excerpt.clone())),
            );
        }

        for cached in &self.cached_entries {
            if let PanelEntry::FoldedDirs(FoldedDirsEntry {
                worktree_id,
                entries,
                ..
            }) = &cached.entry
            {
                if let Some(last) = entries.last() {
                    to_uncollapse.insert(CollapsedEntry::Dir(*worktree_id, last.path.clone()));
                }
            }
        }

        self.collapsed_entries
            .retain(|entry| !to_uncollapse.contains(entry));

        active_editor.update(cx, |editor, cx| {
            buffers_to_unfold.retain(|buffer_id| editor.is_buffer_folded(*buffer_id, cx));
        });

        if buffers_to_unfold.is_empty() {
            self.update_cached_entries(None, window, cx);
        } else {
            self.toggle_buffers_fold(buffers_to_unfold, false, true, window, cx)
                .detach();
        }
    }

    pub fn collapse_all_entries(
        &mut self,
        _: &CollapseAllEntries,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active_editor) = self.active_editor() else {
            return;
        };
        let mut buffers_to_fold = HashSet::default();
        self.collapsed_entries
            .extend(self.cached_entries.iter().filter_map(
                |cached_entry| match &cached_entry.entry {
                    PanelEntry::Fs(FsEntry::Directory(FsEntryDirectory {
                        worktree_id,
                        entry,
                        ..
                    })) => Some(CollapsedEntry::Dir(*worktree_id, entry.path.clone())),
                    PanelEntry::Fs(FsEntry::File(FsEntryFile {
                        worktree_id,
                        buffer_id,
                        ..
                    })) => {
                        buffers_to_fold.insert(*buffer_id);
                        Some(CollapsedEntry::File(*worktree_id, *buffer_id))
                    }
                    PanelEntry::Fs(FsEntry::ExternalFile(external_file)) => {
                        buffers_to_fold.insert(external_file.buffer_id);
                        Some(CollapsedEntry::ExternalFile(external_file.buffer_id))
                    }
                    PanelEntry::FoldedDirs(FoldedDirsEntry {
                        worktree_id,
                        entries,
                        ..
                    }) => Some(CollapsedEntry::Dir(
                        *worktree_id,
                        entries.last()?.path.clone(),
                    )),
                    PanelEntry::Outline(OutlineEntry::Excerpt(excerpt)) => {
                        Some(CollapsedEntry::Excerpt(excerpt.clone()))
                    }
                    PanelEntry::Outline(OutlineEntry::Outline(outline)) => {
                        Some(CollapsedEntry::Outline(outline.range.clone()))
                    }
                    PanelEntry::Search(_) => None,
                },
            ));

        active_editor.update(cx, |editor, cx| {
            buffers_to_fold.retain(|buffer_id| !editor.is_buffer_folded(*buffer_id, cx));
        });
        if buffers_to_fold.is_empty() {
            self.update_cached_entries(None, window, cx);
        } else {
            self.toggle_buffers_fold(buffers_to_fold, true, true, window, cx)
                .detach();
        }
    }

    fn toggle_expanded(&mut self, entry: &PanelEntry, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active_editor) = self.active_editor() else {
            return;
        };
        let mut fold = false;
        let mut buffers_to_toggle = HashSet::default();
        match entry {
            PanelEntry::Fs(FsEntry::Directory(FsEntryDirectory {
                worktree_id,
                entry: dir_entry,
                ..
            })) => {
                let directory_path = &dir_entry.path;
                let collapsed_entry = CollapsedEntry::Dir(*worktree_id, directory_path.clone());
                buffers_to_toggle.extend(self.buffers_inside_directory(*worktree_id, dir_entry));
                if !self.collapsed_entries.remove(&collapsed_entry) {
                    self.collapsed_entries.insert(collapsed_entry);
                    fold = true;
                }
            }
            PanelEntry::Fs(FsEntry::File(FsEntryFile {
                worktree_id,
                buffer_id,
                ..
            })) => {
                let collapsed_entry = CollapsedEntry::File(*worktree_id, *buffer_id);
                buffers_to_toggle.insert(*buffer_id);
                if !self.collapsed_entries.remove(&collapsed_entry) {
                    self.collapsed_entries.insert(collapsed_entry);
                    fold = true;
                }
            }
            PanelEntry::Fs(FsEntry::ExternalFile(external_file)) => {
                let collapsed_entry = CollapsedEntry::ExternalFile(external_file.buffer_id);
                buffers_to_toggle.insert(external_file.buffer_id);
                if !self.collapsed_entries.remove(&collapsed_entry) {
                    self.collapsed_entries.insert(collapsed_entry);
                    fold = true;
                }
            }
            PanelEntry::FoldedDirs(FoldedDirsEntry {
                worktree_id,
                entries: dir_entries,
                ..
            }) => {
                if let Some(dir_entry) = dir_entries.first() {
                    let directory_path = &dir_entry.path;
                    let collapsed_entry = CollapsedEntry::Dir(*worktree_id, directory_path.clone());
                    buffers_to_toggle
                        .extend(self.buffers_inside_directory(*worktree_id, dir_entry));
                    if !self.collapsed_entries.remove(&collapsed_entry) {
                        self.collapsed_entries.insert(collapsed_entry);
                        fold = true;
                    }
                }
            }
            PanelEntry::Outline(OutlineEntry::Excerpt(excerpt)) => {
                let collapsed_entry = CollapsedEntry::Excerpt(excerpt.clone());
                if !self.collapsed_entries.remove(&collapsed_entry) {
                    self.collapsed_entries.insert(collapsed_entry);
                }
            }
            PanelEntry::Outline(OutlineEntry::Outline(outline)) => {
                let collapsed_entry = CollapsedEntry::Outline(outline.range.clone());
                if !self.collapsed_entries.remove(&collapsed_entry) {
                    self.collapsed_entries.insert(collapsed_entry);
                }
            }
            PanelEntry::Search(_) => return,
        }

        active_editor.update(cx, |editor, cx| {
            buffers_to_toggle.retain(|buffer_id| {
                let folded = editor.is_buffer_folded(*buffer_id, cx);
                if fold { !folded } else { folded }
            });
        });

        self.select_entry(entry.clone(), true, window, cx);
        if buffers_to_toggle.is_empty() {
            self.update_cached_entries(None, window, cx);
        } else {
            self.toggle_buffers_fold(buffers_to_toggle, fold, false, window, cx)
                .detach();
        }
    }

    fn toggle_buffers_fold(
        &self,
        buffers: HashSet<BufferId>,
        fold: bool,
        scroll_to_selection: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let Some(active_editor) = self.active_editor() else {
            return Task::ready(());
        };
        cx.spawn_in(window, async move |outline_panel, cx| {
            outline_panel
                .update_in(cx, |outline_panel, window, cx| {
                    active_editor.update(cx, |editor, cx| {
                        for buffer_id in buffers {
                            outline_panel
                                .preserve_selection_on_buffer_fold_toggles
                                .insert(buffer_id);
                            if fold {
                                editor.fold_buffer(buffer_id, cx);
                            } else {
                                editor.unfold_buffer(buffer_id, cx);
                            }
                        }
                    });
                    if scroll_to_selection
                        && let Some(selection) = outline_panel.selected_entry().cloned()
                    {
                        outline_panel.scroll_editor_to_entry(&selection, false, false, window, cx);
                    }
                })
                .ok();
        })
    }

    fn copy_path(
        &mut self,
        _: &zed_actions::workspace::CopyPath,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(clipboard_text) = self
            .selected_entry()
            .and_then(|entry| self.abs_path(entry, cx))
            .map(|p| p.to_string_lossy().into_owned())
        {
            cx.write_to_clipboard(ClipboardItem::new_string(clipboard_text));
        }
    }

    fn copy_relative_path(
        &mut self,
        _: &zed_actions::workspace::CopyRelativePath,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path_style = self.project.read(cx).path_style(cx);
        if let Some(clipboard_text) = self
            .selected_entry()
            .and_then(|entry| match entry {
                PanelEntry::Fs(entry) => self.relative_path(entry, cx),
                PanelEntry::FoldedDirs(folded_dirs) => {
                    folded_dirs.entries.last().map(|entry| entry.path.clone())
                }
                PanelEntry::Search(_) | PanelEntry::Outline(..) => None,
            })
            .map(|p| p.display(path_style).to_string())
        {
            cx.write_to_clipboard(ClipboardItem::new_string(clipboard_text));
        }
    }

    fn reveal_in_finder(
        &mut self,
        _: &RevealInFileManager,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(abs_path) = self
            .selected_entry()
            .and_then(|entry| self.abs_path(entry, cx))
        {
            self.project
                .update(cx, |project, cx| project.reveal_path(&abs_path, cx));
        }
    }

    fn open_in_terminal(
        &mut self,
        _: &OpenInTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selected_entry = self.selected_entry();
        let abs_path = selected_entry.and_then(|entry| self.abs_path(entry, cx));
        let working_directory = if let (
            Some(abs_path),
            Some(PanelEntry::Fs(FsEntry::File(..) | FsEntry::ExternalFile(..))),
        ) = (&abs_path, selected_entry)
        {
            abs_path.parent().map(|p| p.to_owned())
        } else {
            abs_path
        };

        if let Some(working_directory) = working_directory {
            window.dispatch_action(
                workspace::OpenTerminal {
                    working_directory,
                    local: false,
                }
                .boxed_clone(),
                cx,
            )
        }
    }

    fn reveal_entry_for_selection(
        &mut self,
        editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.active
            || !OutlinePanelSettings::get_global(cx).auto_reveal_entries
            || self.focus_handle.contains_focused(window, cx)
        {
            return;
        }
        self.reveal_selection_task = cx.spawn_in(window, async move |outline_panel, cx| {
            cx.background_executor().timer(UPDATE_DEBOUNCE).await;
            outline_panel.update_in(cx, |outline_panel, window, cx| {
                let Some(entry) = outline_panel.location_for_editor_selection(&editor, window, cx)
                else {
                    outline_panel.selected_entry = SelectedEntry::None;
                    cx.notify();
                    return;
                };
                let buffer_id = match &entry {
                    PanelEntry::Fs(entry) => entry.buffer_id(),
                    PanelEntry::Outline(outline) => Some(outline.buffer_id()),
                    PanelEntry::Search(search) => editor
                        .read(cx)
                        .buffer()
                        .read(cx)
                        .snapshot(cx)
                        .anchor_to_buffer_anchor(search.match_range.start)
                        .map(|(anchor, _)| anchor.buffer_id),
                    PanelEntry::FoldedDirs(_) => None,
                };
                let Some(buffer_id) = buffer_id else { return };
                let collapsed_count = outline_panel.collapsed_entries.len();
                if let PanelEntry::Outline(outline) = &entry {
                    let range = outline.range();
                    if let Some(snapshot) = outline_panel.buffer_snapshot_for_id(buffer_id, cx) {
                        outline_panel.collapsed_entries.retain(|entry| match entry {
                            CollapsedEntry::Excerpt(excerpt) => {
                                excerpt.context.start.buffer_id != buffer_id
                                    || !(excerpt.contains(&range.start, &snapshot)
                                        || excerpt.contains(&range.end, &snapshot))
                            }
                            _ => true,
                        });
                    }
                }
                let file_path = outline_panel
                    .fs_entries
                    .iter()
                    .find_map(|entry| match entry {
                        FsEntry::File(file) if file.buffer_id == buffer_id => Some(ProjectPath {
                            worktree_id: file.worktree_id,
                            path: file.entry.path.clone(),
                        }),
                        _ => None,
                    });
                let reveal_buffer_contents =
                    matches!(entry, PanelEntry::Outline(_) | PanelEntry::Search(_));
                if reveal_buffer_contents {
                    outline_panel
                        .collapsed_entries
                        .remove(&CollapsedEntry::ExternalFile(buffer_id));
                }
                if let Some(path) = file_path {
                    if reveal_buffer_contents {
                        outline_panel
                            .collapsed_entries
                            .remove(&CollapsedEntry::File(path.worktree_id, buffer_id));
                    }
                    for ancestor in path.path.ancestors().skip(1) {
                        outline_panel
                            .collapsed_entries
                            .remove(&CollapsedEntry::Dir(path.worktree_id, Arc::from(ancestor)));
                    }
                }
                outline_panel.select_entry(entry, false, window, cx);
                if outline_panel.collapsed_entries.len() != collapsed_count {
                    outline_panel.update_cached_entries(None, window, cx);
                }
            })?;
            anyhow::Ok(())
        });
    }

    fn render_excerpt(
        &self,
        excerpt: &ExcerptRange<Anchor>,
        depth: usize,
        window: &mut Window,
        cx: &mut Context<OutlinePanel>,
    ) -> Option<Stateful<Div>> {
        let item_id = ElementId::from(format!("{excerpt:?}"));
        let is_active = match self.selected_entry() {
            Some(PanelEntry::Outline(OutlineEntry::Excerpt(selected_excerpt))) => {
                selected_excerpt == excerpt
            }
            _ => false,
        };
        let has_outlines = self
            .buffers
            .get(&excerpt.context.start.buffer_id)
            .and_then(|buffer| match &buffer.outlines {
                OutlineState::Outlines(outlines) => Some(outlines),
                OutlineState::Invalidated(outlines) => Some(outlines),
                OutlineState::NotFetched => None,
            })
            .is_some_and(|outlines| !outlines.is_empty());
        let is_expanded = !self
            .collapsed_entries
            .contains(&CollapsedEntry::Excerpt(excerpt.clone()));
        let color = entry_label_color(is_active);
        let icon = if has_outlines {
            self.chevron_toggle(
                PanelEntry::Outline(OutlineEntry::Excerpt(excerpt.clone())),
                is_expanded,
                color,
                cx,
            )
        } else {
            empty_icon()
        };

        let label = self.excerpt_label(&excerpt, cx)?;
        let label_element = Label::new(label)
            .single_line()
            .color(color)
            .into_any_element();

        Some(self.entry_element(
            PanelEntry::Outline(OutlineEntry::Excerpt(excerpt.clone())),
            item_id,
            depth,
            icon,
            is_active,
            label_element,
            window,
            cx,
        ))
    }

    fn excerpt_label(&self, range: &ExcerptRange<language::Anchor>, cx: &App) -> Option<String> {
        let buffer_snapshot = self.buffer_snapshot_for_id(range.context.start.buffer_id, cx)?;
        let excerpt_range = range.context.to_point(&buffer_snapshot);
        Some(format!(
            "Lines {}- {}",
            excerpt_range.start.row + 1,
            excerpt_range.end.row + 1,
        ))
    }

    fn render_outline(
        &self,
        outline: &Outline,
        depth: usize,
        string_match: Option<&StringMatch>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let item_id = ElementId::from(SharedString::from(format!(
            "{:?}|{:?}",
            outline.range, outline.text,
        )));

        let label_element = outline::render_item(
            &outline,
            string_match
                .map(|string_match| string_match.ranges().collect::<Vec<_>>())
                .unwrap_or_default(),
            cx,
        )
        .into_any_element();

        let is_active = match self.selected_entry() {
            Some(PanelEntry::Outline(OutlineEntry::Outline(selected))) => outline == selected,
            _ => false,
        };

        let has_children = self
            .outline_children_cache
            .get(&outline.range.start.buffer_id)
            .and_then(|children_map| {
                let key = (outline.range.clone(), outline.depth);
                children_map.get(&key)
            })
            .copied()
            .unwrap_or(false);
        let is_expanded = !self
            .collapsed_entries
            .contains(&CollapsedEntry::Outline(outline.range.clone()));

        let icon = if has_children {
            self.chevron_toggle(
                PanelEntry::Outline(OutlineEntry::Outline(outline.clone())),
                is_expanded,
                entry_label_color(is_active),
                cx,
            )
        } else {
            empty_icon()
        };

        self.entry_element(
            PanelEntry::Outline(OutlineEntry::Outline(outline.clone())),
            item_id,
            depth,
            icon,
            is_active,
            label_element,
            window,
            cx,
        )
    }

    fn render_entry(
        &self,
        rendered_entry: &FsEntry,
        depth: usize,
        string_match: Option<&StringMatch>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let settings = OutlinePanelSettings::get_global(cx);
        let is_active = match self.selected_entry() {
            Some(PanelEntry::Fs(selected_entry)) => selected_entry == rendered_entry,
            _ => false,
        };
        let (item_id, label_element, icon) = match rendered_entry {
            FsEntry::File(FsEntryFile {
                worktree_id,
                entry,
                buffer_id,
                ..
            }) => {
                let name = self.entry_name(worktree_id, entry, cx);
                let color =
                    entry_git_aware_label_color(entry.git_summary, entry.is_ignored, is_active);
                let icon = if settings.file_icons {
                    FileIcons::get_icon(Path::new(&name), cx)
                        .map(|icon_path| Icon::from_path(icon_path).color(color).into_any_element())
                } else {
                    None
                };
                (
                    ElementId::from(("buffer", buffer_id.to_proto())),
                    HighlightedLabel::new(
                        name,
                        string_match
                            .map(|string_match| string_match.positions.clone())
                            .unwrap_or_default(),
                    )
                    .color(color)
                    .into_any_element(),
                    self.file_expand_collapse_icon(
                        rendered_entry,
                        !self
                            .collapsed_entries
                            .contains(&CollapsedEntry::File(*worktree_id, *buffer_id)),
                        icon,
                        color,
                        cx,
                    ),
                )
            }
            FsEntry::Directory(directory) => {
                let name = self.entry_name(&directory.worktree_id, &directory.entry, cx);

                let is_expanded = !self.collapsed_entries.contains(&CollapsedEntry::Dir(
                    directory.worktree_id,
                    directory.entry.path.clone(),
                ));
                let color = entry_git_aware_label_color(
                    directory.entry.git_summary,
                    directory.entry.is_ignored,
                    is_active,
                );
                let icon = self.folder_expand_collapse_icon(
                    PanelEntry::Fs(rendered_entry.clone()),
                    is_expanded,
                    directory.entry.path.as_std_path(),
                    color,
                    cx,
                );
                (
                    directory_element_id(
                        directory.worktree_id,
                        directory.first_buffer_id,
                        &directory.entry.path,
                    ),
                    HighlightedLabel::new(
                        name,
                        string_match
                            .map(|string_match| string_match.positions.clone())
                            .unwrap_or_default(),
                    )
                    .color(color)
                    .into_any_element(),
                    icon,
                )
            }
            FsEntry::ExternalFile(external_file) => {
                let color = entry_label_color(is_active);
                let (icon, name) = match self.buffer_snapshot_for_id(external_file.buffer_id, cx) {
                    Some(buffer_snapshot) => match buffer_snapshot.file() {
                        Some(file) => {
                            let name = file.file_name(cx).to_string();
                            let icon = if settings.file_icons {
                                FileIcons::get_icon(Path::new(&name), cx)
                            } else {
                                None
                            }
                            .map(Icon::from_path)
                            .map(|icon| icon.color(color).into_any_element());
                            (icon, name)
                        }
                        None => (None, "Untitled".to_string()),
                    },
                    None => (None, "Unknown buffer".to_string()),
                };
                (
                    ElementId::from(("buffer", external_file.buffer_id.to_proto())),
                    HighlightedLabel::new(
                        name,
                        string_match
                            .map(|string_match| string_match.positions.clone())
                            .unwrap_or_default(),
                    )
                    .color(color)
                    .into_any_element(),
                    self.file_expand_collapse_icon(
                        rendered_entry,
                        !self
                            .collapsed_entries
                            .contains(&CollapsedEntry::ExternalFile(external_file.buffer_id)),
                        icon,
                        color,
                        cx,
                    ),
                )
            }
        };

        self.entry_element(
            PanelEntry::Fs(rendered_entry.clone()),
            item_id,
            depth,
            icon,
            is_active,
            label_element,
            window,
            cx,
        )
    }

    fn render_folded_dirs(
        &self,
        folded_dir: &FoldedDirsEntry,
        depth: usize,
        string_match: Option<&StringMatch>,
        window: &mut Window,
        cx: &mut Context<OutlinePanel>,
    ) -> Stateful<Div> {
        let is_active = match self.selected_entry() {
            Some(PanelEntry::FoldedDirs(selected_dirs)) => selected_dirs == folded_dir,
            _ => false,
        };
        let (item_id, label_element, icon) = {
            let name = self.dir_names_string(&folded_dir.entries, folded_dir.worktree_id, cx);

            let is_expanded = folded_dir.entries.iter().all(|dir| {
                !self.collapsed_entries.contains(&CollapsedEntry::Dir(
                    folded_dir.worktree_id,
                    dir.path.clone(),
                ))
            });
            let is_ignored = folded_dir.entries.iter().any(|entry| entry.is_ignored);
            let git_status = folded_dir
                .entries
                .first()
                .map(|entry| entry.git_summary)
                .unwrap_or_default();
            let color = entry_git_aware_label_color(git_status, is_ignored, is_active);
            let icon = self.folder_expand_collapse_icon(
                PanelEntry::FoldedDirs(folded_dir.clone()),
                is_expanded,
                Path::new(&name),
                color,
                cx,
            );
            (
                folded_dir
                    .entries
                    .last()
                    .map(|entry| {
                        directory_element_id(
                            folded_dir.worktree_id,
                            folded_dir.first_buffer_id,
                            &entry.path,
                        )
                    })
                    .unwrap_or_else(|| {
                        ElementId::from(("empty-folded-dirs", folded_dir.worktree_id.to_proto()))
                    }),
                HighlightedLabel::new(
                    name,
                    string_match
                        .map(|string_match| string_match.positions.clone())
                        .unwrap_or_default(),
                )
                .color(color)
                .into_any_element(),
                icon,
            )
        };

        self.entry_element(
            PanelEntry::FoldedDirs(folded_dir.clone()),
            item_id,
            depth,
            icon,
            is_active,
            label_element,
            window,
            cx,
        )
    }

    fn render_search_match(
        &mut self,
        multi_buffer_snapshot: Option<&MultiBufferSnapshot>,
        match_range: &Range<editor::Anchor>,
        render_data: &Arc<OnceLock<SearchData>>,
        kind: SearchKind,
        depth: usize,
        string_match: Option<&StringMatch>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Stateful<Div>> {
        let search_data = match render_data.get() {
            Some(search_data) => search_data,
            None => {
                if let ItemsDisplayMode::Search(search_state) = &mut self.mode
                    && let Some(multi_buffer_snapshot) = multi_buffer_snapshot
                {
                    search_state
                        .highlight_search_match_tx
                        .try_send(HighlightArguments {
                            multi_buffer_snapshot: multi_buffer_snapshot.clone(),
                            match_range: match_range.clone(),
                            search_data: Arc::clone(render_data),
                        })
                        .ok();
                }
                return None;
            }
        };
        let search_matches = string_match
            .iter()
            .flat_map(|string_match| string_match.ranges())
            .collect::<Vec<_>>();
        let match_ranges = if search_matches.is_empty() {
            &search_data.search_match_indices
        } else {
            &search_matches
        };
        let label_element = outline::render_item(
            &OutlineItem {
                depth,
                annotation_range: None,
                range: search_data.context_range.clone(),
                selection_range: search_data.context_range.clone(),
                text: search_data.context_text.clone().into(),
                source_range_for_text: search_data.context_range.clone(),
                highlight_ranges: search_data
                    .highlights_data
                    .get()
                    .cloned()
                    .unwrap_or_default(),
                name_ranges: search_data.search_match_indices.clone(),
                body_range: Some(search_data.context_range.clone()),
            },
            match_ranges.iter().cloned(),
            cx,
        );
        let truncated_contents_label = || Label::new(TRUNCATED_CONTEXT_MARK);
        let entire_label = h_flex()
            .justify_center()
            .p_0()
            .when(search_data.truncated_left, |parent| {
                parent.child(truncated_contents_label())
            })
            .child(label_element)
            .when(search_data.truncated_right, |parent| {
                parent.child(truncated_contents_label())
            })
            .into_any_element();

        let is_active = match self.selected_entry() {
            Some(PanelEntry::Search(SearchEntry {
                match_range: selected_match_range,
                ..
            })) => match_range == selected_match_range,
            _ => false,
        };
        Some(self.entry_element(
            PanelEntry::Search(SearchEntry {
                kind,
                match_range: match_range.clone(),
                render_data: render_data.clone(),
            }),
            ElementId::from(SharedString::from(format!("search-{match_range:?}"))),
            depth,
            empty_icon(),
            is_active,
            entire_label,
            window,
            cx,
        ))
    }

    fn chevron_toggle(
        &self,
        entry: PanelEntry,
        is_expanded: bool,
        color: Color,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chevron = FileIcons::get_chevron_icon(is_expanded, cx)
            .map(|path| Icon::from_path(path).color(color).into_any_element())
            .unwrap_or_else(empty_icon);
        self.expand_collapse_icon(entry, is_expanded, chevron, cx)
    }

    fn expand_collapse_icon(
        &self,
        entry: PanelEntry,
        is_expanded: bool,
        icon: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .id("expand-collapse")
            .role(gpui::Role::Button)
            .aria_label(if is_expanded { "Collapse" } else { "Expand" })
            .aria_expanded(is_expanded)
            .flex_none()
            .cursor_pointer()
            .rounded_sm()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .tooltip(Tooltip::text(if is_expanded {
                "Collapse"
            } else {
                "Expand"
            }))
            .on_click(
                cx.listener(move |panel, event: &gpui::ClickEvent, window, cx| {
                    if event.is_right_click() || event.first_focus() {
                        return;
                    }
                    cx.stop_propagation();
                    panel.toggle_expanded(&entry, window, cx);
                }),
            )
            .child(icon)
            .into_any_element()
    }

    fn folder_expand_collapse_icon(
        &self,
        entry: PanelEntry,
        is_expanded: bool,
        path: &Path,
        color: Color,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let settings = OutlinePanelSettings::get_global(cx);
        let indicator = settings.folder_indicator;
        let file_icons = settings.file_icons;
        let indicators = FileIcons::get_folder_indicators(indicator, is_expanded, path, cx);
        let chevron = indicators.chevron.map(|path| {
            self.expand_collapse_icon(
                entry,
                is_expanded,
                Icon::from_path(path).color(color).into_any_element(),
                cx,
            )
        });
        let icon = indicators
            .icon
            .map(|path| Icon::from_path(path).color(color).into_any_element());
        let (leading, trailing) = if file_icons || indicator == FolderIndicator::Both {
            (
                chevron.unwrap_or_else(empty_icon),
                Some(icon.unwrap_or_else(empty_icon)),
            )
        } else {
            (chevron.or(icon).unwrap_or_else(empty_icon), None)
        };
        h_flex()
            .flex_none()
            .gap_0p5()
            .child(leading)
            .children(trailing)
            .into_any_element()
    }

    fn file_expand_collapse_icon(
        &self,
        entry: &FsEntry,
        is_expanded: bool,
        icon: Option<AnyElement>,
        color: Color,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.multi_buffer_active(cx) {
            return icon.unwrap_or_else(empty_icon);
        }
        let settings = OutlinePanelSettings::get_global(cx);
        let icon = icon.or_else(|| {
            (settings.file_icons || settings.folder_indicator == FolderIndicator::Both)
                .then(empty_icon)
        });
        let expand_collapse_icon = if self.hide_symbols_active(cx) {
            empty_icon()
        } else {
            self.chevron_toggle(PanelEntry::Fs(entry.clone()), is_expanded, color, cx)
        };
        h_flex()
            .flex_none()
            .gap_0p5()
            .child(expand_collapse_icon)
            .children(icon)
            .into_any_element()
    }

    fn entry_element(
        &self,
        rendered_entry: PanelEntry,
        item_id: ElementId,
        depth: usize,
        icon_element: AnyElement,
        is_active: bool,
        label_element: gpui::AnyElement,
        window: &mut Window,
        cx: &mut Context<OutlinePanel>,
    ) -> Stateful<Div> {
        let settings = OutlinePanelSettings::get_global(cx);
        let is_deleted = match &rendered_entry {
            PanelEntry::Fs(FsEntry::File(file)) => file.is_deleted,
            _ => false,
        };
        div()
            .text_ui(cx)
            .id(item_id.clone())
            .on_click({
                let clicked_entry = rendered_entry.clone();
                cx.listener(move |outline_panel, event: &gpui::ClickEvent, window, cx| {
                    if event.is_right_click() || event.first_focus() {
                        return;
                    }

                    let change_focus = event.click_count() > 1;
                    outline_panel.select_entry(clicked_entry.clone(), true, window, cx);
                    outline_panel.scroll_editor_to_entry(
                        &clicked_entry,
                        true,
                        change_focus,
                        window,
                        cx,
                    );
                })
            })
            .cursor_pointer()
            .child(
                ListItem::new(item_id)
                    .indent_level(depth)
                    .indent_step_size(px(settings.indent_size))
                    .toggle_state(is_active)
                    .child(
                        h_flex()
                            .child(h_flex().min_w(px(16.)).justify_center().child(icon_element))
                            .child(
                                h_flex()
                                    .h_6()
                                    .child(label_element)
                                    .ml_1()
                                    .when(is_deleted, |this| this.line_through()),
                            ),
                    )
                    .on_secondary_mouse_down(cx.listener(
                        move |outline_panel, event: &MouseDownEvent, window, cx| {
                            // Stop propagation to prevent the catch-all context menu for the project
                            // panel from being deployed.
                            cx.stop_propagation();
                            outline_panel.deploy_context_menu(
                                event.position,
                                rendered_entry.clone(),
                                window,
                                cx,
                            )
                        },
                    )),
            )
            .border_1()
            .border_r_2()
            .rounded_none()
            .hover(|style| {
                if is_active {
                    style
                } else {
                    let hover_color = cx.theme().colors().ghost_element_hover;
                    style.bg(hover_color).border_color(hover_color)
                }
            })
            .when(
                is_active && self.focus_handle.contains_focused(window, cx),
                |div| div.border_color(cx.theme().colors().panel_focused_border),
            )
    }

    fn entry_name(&self, worktree_id: &WorktreeId, entry: &FsEntryPath, cx: &App) -> String {
        match self.project.read(cx).worktree_for_id(*worktree_id, cx) {
            Some(worktree) => file_name(&worktree.read(cx).absolutize(&entry.path)),
            None => file_name(entry.path.as_std_path()),
        }
    }

    fn fs_entry_label(&self, fs_entry: &FsEntry, cx: &App) -> Option<String> {
        match fs_entry {
            FsEntry::ExternalFile(external) => Some(
                self.buffer_snapshot_for_id(external.buffer_id, cx)?
                    .file()?
                    .file_name(cx)
                    .to_string(),
            ),
            FsEntry::Directory(FsEntryDirectory {
                worktree_id, entry, ..
            })
            | FsEntry::File(FsEntryFile {
                worktree_id, entry, ..
            }) => match entry.path.file_name() {
                Some(name) => Some(name.to_string()),
                None => Some(self.entry_name(worktree_id, entry, cx)),
            },
        }
    }

    fn update_fs_entries(
        &mut self,
        active_editor: Entity<Editor>,
        debounce: Option<Duration>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.active || (debounce.is_some() && self.fs_entries_update_pending) {
            return;
        }
        self.fs_entries_update_pending = true;

        self.fs_entries_update_task = cx.spawn_in(window, async move |outline_panel, cx| {
            if let Some(debounce) = debounce {
                cx.background_executor().timer(debounce).await;
            }

            let mut new_buffers = HashMap::<BufferId, BufferOutlines>::default();
            let Ok((
                buffer_excerpts,
                auto_fold_dirs,
                repo_snapshots,
                mut collapsed_entries,
                mut unfolded_dirs,
            )) = outline_panel.update(cx, |outline_panel, cx| {
                outline_panel.fs_entries_update_pending = false;
                let auto_fold_dirs = OutlinePanelSettings::get_global(cx).auto_fold_dirs;
                let git_store = outline_panel.project.read(cx).git_store().clone();
                let repo_snapshots = git_store.read(cx).display_repo_snapshots(cx);
                let editor = active_editor.read(cx);
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let mut buffer_excerpts = IndexMap::<BufferId, BufferExcerpts>::default();

                for excerpt in snapshot.excerpts() {
                    let buffer_id = excerpt.context.start.buffer_id;
                    let Some(buffer) = snapshot.buffer_for_id(buffer_id) else {
                        continue;
                    };
                    buffer_excerpts
                        .entry(buffer_id)
                        .or_insert_with(|| {
                            let status = git_store
                                .read(cx)
                                .display_status_for_buffer_id(buffer_id, cx);
                            let file = File::from_dyn(buffer.file());
                            let is_deleted = file.is_some_and(|file| {
                                is_deleted_file(
                                    file,
                                    status,
                                    snapshot
                                        .diff_for_buffer_id(buffer_id)
                                        .is_some_and(|diff| diff.base_text_exists()),
                                )
                            });
                            let file = file.and_then(|file| {
                                let worktree = file.worktree.read(cx).snapshot();
                                let entry = file
                                    .project_entry_id()
                                    .and_then(|id| worktree.entry_for_id(id));
                                if entry.is_none() && !is_deleted {
                                    return None;
                                }
                                let path_entry = FsEntryPath {
                                    path: file.path.clone(),
                                    git_summary: status.map(GitSummary::from).unwrap_or_default(),
                                    is_ignored: entry.is_some_and(|entry| entry.is_ignored),
                                };
                                Some((worktree, path_entry))
                            });
                            BufferExcerpts {
                                is_new: outline_panel
                                    .new_entries_for_fs_update
                                    .contains(&buffer_id)
                                    || !outline_panel.buffers.contains_key(&buffer_id),
                                is_folded: editor.is_buffer_folded(buffer_id, cx),
                                excerpts: Vec::new(),
                                file,
                                is_deleted,
                            }
                        })
                        .excerpts
                        .push(excerpt.clone());

                    new_buffers
                        .entry(buffer_id)
                        .or_insert_with(|| {
                            let outlines = match outline_panel.buffers.get(&buffer_id) {
                                Some(BufferOutlines {
                                    outlines: OutlineState::Outlines(outlines),
                                    ..
                                }) => OutlineState::Outlines(outlines.clone()),
                                _ => OutlineState::NotFetched,
                            };
                            BufferOutlines {
                                outlines,
                                excerpts: Vec::new(),
                            }
                        })
                        .excerpts
                        .push(excerpt);
                }
                (
                    buffer_excerpts,
                    auto_fold_dirs,
                    repo_snapshots,
                    outline_panel.collapsed_entries.clone(),
                    outline_panel.unfolded_dirs.clone(),
                )
            })
            else {
                return;
            };

            let (fs_entries, children_count, collapsed_entries, unfolded_dirs) = cx
                .background_spawn(async move {
                    let mut fs_entries = Vec::new();
                    let mut directories = HashMap::<ProjectPath, FsEntryPath>::default();
                    let mut parent_paths = Vec::<ProjectPath>::new();
                    let repositories_by_root = repo_snapshots
                        .values()
                        .map(|snapshot| (snapshot.work_directory_abs_path.as_ref(), snapshot))
                        .collect::<BTreeMap<_, _>>();
                    for (buffer_id, buffer) in buffer_excerpts {
                        let collapsed_entry = match &buffer.file {
                            Some((worktree, _)) => CollapsedEntry::File(worktree.id(), buffer_id),
                            None => CollapsedEntry::ExternalFile(buffer_id),
                        };
                        if buffer.is_folded {
                            collapsed_entries.insert(collapsed_entry);
                        } else if buffer.is_new {
                            collapsed_entries.remove(&collapsed_entry);
                        }
                        let Some((worktree, entry)) = buffer.file else {
                            parent_paths.clear();
                            fs_entries.push(FsEntry::ExternalFile(FsEntryExternalFile {
                                buffer_id,
                                excerpts: Arc::from(buffer.excerpts),
                            }));
                            continue;
                        };
                        let worktree_id = worktree.id();
                        let mut ancestors = entry.path.ancestors().skip(1).collect::<Vec<_>>();
                        ancestors.reverse();
                        let shared_depth = parent_paths
                            .last()
                            .filter(|parent| parent.worktree_id == worktree_id)
                            .zip(ancestors.last())
                            .map(|(parent, ancestor)| {
                                parent
                                    .path
                                    .components()
                                    .zip(ancestor.components())
                                    .take_while(|(left, right)| left == right)
                                    .count()
                                    + 1
                            })
                            .unwrap_or(0);
                        parent_paths.truncate(shared_depth);
                        for (depth, path) in ancestors.into_iter().enumerate() {
                            let ancestor =
                                parent_paths
                                    .get(depth)
                                    .cloned()
                                    .unwrap_or_else(|| ProjectPath {
                                        worktree_id,
                                        path: Arc::from(path),
                                    });
                            if buffer.is_new {
                                collapsed_entries.remove(&CollapsedEntry::Dir(
                                    worktree_id,
                                    ancestor.path.clone(),
                                ));
                            }
                            if depth < shared_depth {
                                continue;
                            }
                            let directory =
                                directories.entry(ancestor.clone()).or_insert_with(|| {
                                    let indexed_entry = worktree
                                        .entry_for_path(&ancestor.path)
                                        .filter(|entry| entry.is_dir());
                                    let git_summary = indexed_entry
                                        .and_then(|_| {
                                            let abs_path = worktree.absolutize(&ancestor.path);
                                            for query in abs_path.ancestors() {
                                                let (_, repository) = repositories_by_root
                                                    .range(Path::new("")..=query)
                                                    .next_back()?;
                                                let Some(path) =
                                                    repository.abs_path_to_repo_path(&abs_path)
                                                else {
                                                    continue;
                                                };
                                                let mut statuses = repository
                                                    .statuses_by_path
                                                    .cursor::<PathProgress>(());
                                                statuses.seek_forward(
                                                    &PathTarget::Path(&path),
                                                    Bias::Left,
                                                );
                                                return Some(statuses.summary(
                                                    &PathTarget::Successor(&path),
                                                    Bias::Left,
                                                ));
                                            }
                                            None
                                        })
                                        .unwrap_or_default();
                                    FsEntryPath {
                                        path: ancestor.path.clone(),
                                        git_summary,
                                        is_ignored: indexed_entry
                                            .is_some_and(|entry| entry.is_ignored),
                                    }
                                });
                            fs_entries.push(FsEntry::Directory(FsEntryDirectory {
                                worktree_id,
                                first_buffer_id: buffer_id,
                                entry: directory.clone(),
                            }));
                            parent_paths.push(ancestor);
                        }
                        fs_entries.push(FsEntry::File(FsEntryFile {
                            worktree_id,
                            entry,
                            buffer_id,
                            excerpts: Arc::from(buffer.excerpts),
                            is_deleted: buffer.is_deleted,
                        }));
                    }
                    collapsed_entries.retain(|entry| match entry {
                        CollapsedEntry::Dir(worktree_id, path) => {
                            directories.contains_key(&ProjectPath {
                                worktree_id: *worktree_id,
                                path: path.clone(),
                            })
                        }
                        _ => true,
                    });
                    unfolded_dirs.retain(|worktree_id, paths| {
                        paths.retain(|path| {
                            directories.contains_key(&ProjectPath {
                                worktree_id: *worktree_id,
                                path: path.clone(),
                            })
                        });
                        !paths.is_empty()
                    });
                    let mut children_count =
                        HashMap::<WorktreeId, HashMap<Arc<RelPath>, FsChildren>>::default();
                    for directory in directories.keys() {
                        if let Some(parent) = directory.path.parent() {
                            children_count
                                .entry(directory.worktree_id)
                                .or_default()
                                .entry(Arc::from(parent))
                                .or_default()
                                .dirs += 1;
                        }
                    }
                    for entry in &fs_entries {
                        if let FsEntry::File(file) = entry
                            && let Some(parent) = file.entry.path.parent()
                        {
                            children_count
                                .entry(file.worktree_id)
                                .or_default()
                                .entry(Arc::from(parent))
                                .or_default()
                                .files += 1;
                        }
                    }
                    if auto_fold_dirs {
                        for entry in &fs_entries {
                            let FsEntry::Directory(directory) = entry else {
                                continue;
                            };
                            let unfolded = unfolded_dirs.entry(directory.worktree_id).or_default();
                            let path = &directory.entry.path;
                            let children = children_count
                                .get(&directory.worktree_id)
                                .and_then(|children| children.get(path))
                                .copied()
                                .unwrap_or_default();
                            let parent_unfolded =
                                path.parent().is_none_or(|parent| unfolded.contains(parent));
                            if path.is_empty()
                                || !children.may_be_fold_part()
                                || (children.dirs == 0 && parent_unfolded)
                            {
                                unfolded.insert(path.clone());
                            }
                        }
                    }
                    (fs_entries, children_count, collapsed_entries, unfolded_dirs)
                })
                .await;

            outline_panel
                .update_in(cx, |outline_panel, window, cx| {
                    outline_panel.new_entries_for_fs_update.clear();
                    outline_panel.buffers = new_buffers;
                    outline_panel.collapsed_entries = collapsed_entries;
                    outline_panel.unfolded_dirs = unfolded_dirs;
                    outline_panel.fs_entries = fs_entries;
                    outline_panel.fs_children_count = children_count;
                    outline_panel.update_non_fs_items(window, cx);
                    if outline_panel.buffers_to_fetch(cx).is_empty() {
                        outline_panel.update_cached_entries(debounce, window, cx);
                    }
                    cx.notify();
                })
                .ok();
        });
    }

    fn replace_active_editor(
        &mut self,
        new_active_item: Box<dyn ItemHandle>,
        new_active_editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_previous(window, cx);

        let default_expansion_depth =
            OutlinePanelSettings::get_global(cx).expand_outlines_with_depth;
        // We'll apply the expansion depth after outlines are loaded
        self.pending_default_expansion_depth = Some(default_expansion_depth);

        let buffer_search_subscription = cx.subscribe_in(
            &new_active_editor,
            window,
            |outline_panel: &mut Self,
             _,
             e: &SearchEvent,
             window: &mut Window,
             cx: &mut Context<Self>| {
                if matches!(e, SearchEvent::MatchesInvalidated) {
                    outline_panel.update_contents(Some(UPDATE_DEBOUNCE), window, cx);
                }
            },
        );
        self.active_item = Some(ActiveItem {
            _buffer_search_subscription: buffer_search_subscription,
            _editor_subscription: subscribe_for_editor_events(&new_active_editor, window, cx),
            item_handle: new_active_item.downgrade_item(),
            active_editor: new_active_editor.downgrade(),
        });
        self.new_entries_for_fs_update.extend(
            new_active_editor
                .read(cx)
                .buffer()
                .read(cx)
                .snapshot(cx)
                .excerpts()
                .map(|excerpt| excerpt.context.start.buffer_id),
        );
        self.selected_entry.invalidate();
        self.update_fs_entries(new_active_editor, None, window, cx);
    }

    fn clear_previous(&mut self, window: &mut Window, cx: &mut App) {
        self.fs_entries_update_task = Task::ready(());
        self.fs_entries_update_pending = false;
        self.lsp_outline_refresh_task = Task::ready(());
        self.outline_fetch_tasks.clear();
        self.cached_entries_update_task = Task::ready(());
        self.cached_entries_update_pending = None;
        self.reveal_selection_task = Task::ready(Ok(()));
        self.filter_editor
            .update(cx, |editor, cx| editor.clear(window, cx));
        self.collapsed_entries.clear();
        self.unfolded_dirs.clear();
        self.active_item = None;
        self.fs_entries.clear();
        self.fs_children_count.clear();
        self.buffers.clear();
        self.cached_entries = Vec::new();
        self.selected_entry = SelectedEntry::None;
        self.pinned = false;
        self.mode = ItemsDisplayMode::Outline;
        self.pending_default_expansion_depth = None;
        self.default_depth_applied_buffers.clear();
    }

    fn location_for_editor_selection(
        &self,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<PanelEntry> {
        let editor_snapshot = editor.update(cx, |editor, cx| editor.snapshot(window, cx));
        let multi_buffer = editor.read(cx).buffer();
        let multi_buffer_snapshot = multi_buffer.read(cx).snapshot(cx);
        let anchor = editor.update(cx, |editor, _| editor.selections.newest_anchor().head());
        let selection_display_point = anchor.to_display_point(&editor_snapshot);
        let (anchor, _) = multi_buffer_snapshot.anchor_to_buffer_anchor(anchor)?;

        if editor.read(cx).is_buffer_folded(anchor.buffer_id, cx) {
            return self
                .fs_entries
                .iter()
                .find(|fs_entry| match fs_entry {
                    FsEntry::Directory(..) => false,
                    FsEntry::File(FsEntryFile {
                        buffer_id: other_buffer_id,
                        ..
                    })
                    | FsEntry::ExternalFile(FsEntryExternalFile {
                        buffer_id: other_buffer_id,
                        ..
                    }) => anchor.buffer_id == *other_buffer_id,
                })
                .cloned()
                .map(PanelEntry::Fs);
        }

        match &self.mode {
            ItemsDisplayMode::Search(search_state) => search_state
                .matches
                .iter()
                .rev()
                .min_by_key(|&(match_range, _)| {
                    let match_display_range =
                        match_range.clone().to_display_points(&editor_snapshot);
                    let start_distance = if selection_display_point < match_display_range.start {
                        match_display_range.start - selection_display_point
                    } else {
                        selection_display_point - match_display_range.start
                    };
                    let end_distance = if selection_display_point < match_display_range.end {
                        match_display_range.end - selection_display_point
                    } else {
                        selection_display_point - match_display_range.end
                    };
                    start_distance + end_distance
                })
                .and_then(|(closest_range, _)| {
                    self.cached_entries.iter().find_map(|cached_entry| {
                        if let PanelEntry::Search(SearchEntry { match_range, .. }) =
                            &cached_entry.entry
                        {
                            if match_range == closest_range {
                                Some(cached_entry.entry.clone())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                }),
            ItemsDisplayMode::Outline => self.outline_location(
                anchor,
                multi_buffer_snapshot,
                editor_snapshot,
                selection_display_point,
                cx,
            ),
        }
    }

    fn outline_location(
        &self,
        selection_anchor: Anchor,
        multi_buffer_snapshot: editor::MultiBufferSnapshot,
        editor_snapshot: editor::EditorSnapshot,
        selection_display_point: DisplayPoint,
        cx: &App,
    ) -> Option<PanelEntry> {
        let buffer_snapshot = multi_buffer_snapshot.buffer_for_id(selection_anchor.buffer_id);
        let mut excerpt_ranges = multi_buffer_snapshot
            .excerpts_for_buffer(selection_anchor.buffer_id)
            .map(|excerpt| excerpt.context)
            .collect::<Vec<_>>();
        if let Some(snapshot) = buffer_snapshot {
            excerpt_ranges.sort_unstable_by(|left, right| left.start.cmp(&right.start, snapshot));
            let mut maximum_end = None;
            for range in &mut excerpt_ranges {
                if let Some(end) = maximum_end
                    && range.end.cmp(&end, snapshot).is_lt()
                {
                    range.end = end;
                }
                maximum_end = Some(range.end);
            }
        }

        let excerpt_outlines = self
            .buffers
            .get(&selection_anchor.buffer_id)
            .into_iter()
            .flat_map(|buffer| buffer.iter_outlines())
            .flat_map(|outline| {
                let snapshot = buffer_snapshot?;
                let (start, end) = if outline
                    .range
                    .start
                    .cmp(&outline.range.end, snapshot)
                    .is_gt()
                {
                    (outline.range.end, outline.range.start)
                } else {
                    (outline.range.start, outline.range.end)
                };
                let index = excerpt_ranges
                    .partition_point(|excerpt| excerpt.start.cmp(&start, snapshot).is_le())
                    .checked_sub(1)?;
                if excerpt_ranges.get(index)?.end.cmp(&end, snapshot).is_lt() {
                    return None;
                }
                let range = multi_buffer_snapshot.anchor_range_in_buffer(outline.range.clone())?;
                Some((
                    range.start.to_display_point(&editor_snapshot)
                        ..range.end.to_display_point(&editor_snapshot),
                    outline,
                ))
            })
            .collect::<Vec<_>>();

        let mut matching_outline_indices = Vec::new();
        let mut children = HashMap::default();
        let mut parents_stack = Vec::<(&Range<DisplayPoint>, &&Outline, usize)>::new();

        for (i, (outline_range, outline)) in excerpt_outlines.iter().enumerate() {
            if outline_range
                .to_inclusive()
                .contains(&selection_display_point)
            {
                matching_outline_indices.push(i);
            } else if (outline_range.start.row()..outline_range.end.row())
                .to_inclusive()
                .contains(&selection_display_point.row())
            {
                matching_outline_indices.push(i);
            }

            while let Some((parent_range, parent_outline, _)) = parents_stack.last() {
                if parent_outline.depth >= outline.depth
                    || !parent_range.contains(&outline_range.start)
                {
                    parents_stack.pop();
                } else {
                    break;
                }
            }
            if let Some((_, _, parent_index)) = parents_stack.last_mut() {
                children
                    .entry(*parent_index)
                    .or_insert_with(Vec::new)
                    .push(i);
            }
            parents_stack.push((outline_range, outline, i));
        }

        let outline_item = matching_outline_indices
            .into_iter()
            .flat_map(|i| Some((i, excerpt_outlines.get(i)?)))
            .filter(|(i, _)| {
                children
                    .get(i)
                    .map(|children| {
                        children.iter().all(|child_index| {
                            excerpt_outlines
                                .get(*child_index)
                                .map(|(child_range, _)| child_range.start > selection_display_point)
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(true)
            })
            .min_by_key(|(_, (outline_range, outline))| {
                let distance_from_start = if outline_range.start > selection_display_point {
                    outline_range.start - selection_display_point
                } else {
                    selection_display_point - outline_range.start
                };
                let distance_from_end = if outline_range.end > selection_display_point {
                    outline_range.end - selection_display_point
                } else {
                    selection_display_point - outline_range.end
                };

                // An outline item's range can extend to the same row the next
                // item starts on, so when the cursor is at the start of that
                // row, prefer the item that starts there over any item whose
                // range merely overlaps that row.
                let cursor_not_at_outline_start = outline_range.start != selection_display_point;
                (
                    cursor_not_at_outline_start,
                    cmp::Reverse(outline.depth),
                    distance_from_start,
                    distance_from_end,
                )
            })
            .map(|(_, (_, outline))| *outline)
            .cloned();

        let closest_container = match outline_item {
            Some(outline) => PanelEntry::Outline(OutlineEntry::Outline(outline)),
            None => {
                self.cached_entries.iter().rev().find_map(|cached_entry| {
                    match &cached_entry.entry {
                        PanelEntry::Outline(OutlineEntry::Excerpt(excerpt)) => {
                            if excerpt.context.start.buffer_id == selection_anchor.buffer_id
                                && let Some(buffer_snapshot) =
                                    self.buffer_snapshot_for_id(excerpt.context.start.buffer_id, cx)
                                && excerpt.contains(&selection_anchor, &buffer_snapshot)
                            {
                                Some(cached_entry.entry.clone())
                            } else {
                                None
                            }
                        }
                        PanelEntry::Fs(
                            FsEntry::ExternalFile(FsEntryExternalFile {
                                buffer_id: file_buffer_id,
                                excerpts: file_excerpts,
                                ..
                            })
                            | FsEntry::File(FsEntryFile {
                                buffer_id: file_buffer_id,
                                excerpts: file_excerpts,
                                ..
                            }),
                        ) => {
                            if *file_buffer_id == selection_anchor.buffer_id
                                && let Some(buffer_snapshot) =
                                    self.buffer_snapshot_for_id(*file_buffer_id, cx)
                                && file_excerpts.iter().any(|excerpt| {
                                    excerpt.contains(&selection_anchor, &buffer_snapshot)
                                })
                            {
                                Some(cached_entry.entry.clone())
                            } else {
                                None
                            }
                        }
                        _ => None,
                    }
                })?
            }
        };
        Some(closest_container)
    }

    fn fetch_outdated_outlines(
        &mut self,
        debounce: Option<Duration>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let buffers_to_fetch = self.buffers_to_fetch(cx);
        if buffers_to_fetch.is_empty() {
            return;
        }

        let first_update = Arc::new(AtomicBool::new(true));
        for buffer_id in buffers_to_fetch {
            if self
                .outline_fetch_tasks
                .get(&buffer_id)
                .is_some_and(|task| !task.is_ready())
            {
                continue;
            }
            let first_update = first_update.clone();

            self.outline_fetch_tasks.insert(
                buffer_id,
                cx.spawn_in(window, async move |outline_panel, cx| {
                    if let Some(debounce) = debounce {
                        cx.background_executor().timer(debounce).await;
                    }
                    let Some(outline_task) = outline_panel
                        .update(cx, |panel, cx| {
                            let editor = panel.active_editor()?;
                            Some(editor.update(cx, |editor, cx| {
                                editor.buffer_outline_items(buffer_id, cx)
                            }))
                        })
                        .ok()
                        .flatten()
                    else {
                        return;
                    };
                    let fetched_outlines = outline_task.await;

                    outline_panel
                        .update_in(cx, |outline_panel, window, cx| {
                            let pending_default_depth =
                                outline_panel.pending_default_expansion_depth.filter(|_| {
                                    outline_panel
                                        .default_depth_applied_buffers
                                        .insert(buffer_id)
                                });

                            let mut entries_changed = false;
                            if let Some(buffer) = outline_panel.buffers.get_mut(&buffer_id) {
                                entries_changed = pending_default_depth.is_some()
                                    || match &buffer.outlines {
                                        OutlineState::Outlines(outlines)
                                        | OutlineState::Invalidated(outlines) => {
                                            outlines != &fetched_outlines
                                        }
                                        OutlineState::NotFetched => true,
                                    };
                                buffer.outlines = OutlineState::Outlines(fetched_outlines);

                                if let Some(default_depth) = pending_default_depth
                                    && let OutlineState::Outlines(outlines) = &buffer.outlines
                                {
                                    let outlines_with_children = outlines
                                        .array_windows::<2>()
                                        .filter_map(|[current, next]| {
                                            if next.depth > current.depth {
                                                Some((current.range.clone(), current.depth))
                                            } else {
                                                None
                                            }
                                        })
                                        .collect::<HashSet<_>>();
                                    outlines
                                        .iter()
                                        .filter(|outline| {
                                            (default_depth == 0 || outline.depth >= default_depth)
                                                && outlines_with_children.contains(&(
                                                    outline.range.clone(),
                                                    outline.depth,
                                                ))
                                        })
                                        .for_each(|outline| {
                                            outline_panel.collapsed_entries.insert(
                                                CollapsedEntry::Outline(outline.range.clone()),
                                            );
                                        });
                                }
                            }

                            if entries_changed
                                || matches!(
                                    &outline_panel.selected_entry,
                                    SelectedEntry::Invalidated(Some(PanelEntry::Outline(outline)))
                                        if outline.buffer_id() == buffer_id
                                )
                            {
                                let debounce =
                                    if first_update.fetch_and(false, atomic::Ordering::AcqRel) {
                                        None
                                    } else {
                                        Some(UPDATE_DEBOUNCE)
                                    };
                                if let Some(PanelEntry::Outline(outline)) =
                                    outline_panel.selected_entry()
                                    && outline.buffer_id() == buffer_id
                                {
                                    outline_panel.selected_entry.invalidate();
                                }
                                outline_panel.update_cached_entries(debounce, window, cx);
                            }
                        })
                        .ok();
                }),
            );
        }
    }

    fn is_singleton_active(&self, cx: &App) -> bool {
        self.active_editor()
            .is_some_and(|active_editor| active_editor.read(cx).buffer().read(cx).is_singleton())
    }

    fn multi_buffer_active(&self, cx: &App) -> bool {
        self.active_editor()
            .is_some_and(|active_editor| !active_editor.read(cx).buffer().read(cx).is_singleton())
    }

    fn hide_symbols_active(&self, cx: &App) -> bool {
        self.hide_symbols_override
            .unwrap_or(OutlinePanelSettings::get_global(cx).multi_buffer_hide_symbols)
    }

    fn remap_selection_for_hidden_symbols(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.multi_buffer_active(cx) || !self.hide_symbols_active(cx) {
            return;
        }
        let buffer_id = match self.selected_entry() {
            Some(PanelEntry::Outline(OutlineEntry::Excerpt(excerpt))) => {
                Some(excerpt.context.start.buffer_id)
            }
            Some(PanelEntry::Outline(OutlineEntry::Outline(outline))) => {
                Some(outline.range.start.buffer_id)
            }
            Some(PanelEntry::Search(search_entry)) => {
                let match_start = search_entry.match_range.start;
                self.active_editor().and_then(|editor| {
                    let multi_buffer_snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
                    multi_buffer_snapshot
                        .anchor_to_buffer_anchor(match_start)
                        .map(|(anchor, _)| anchor.buffer_id)
                })
            }
            Some(PanelEntry::Fs(_) | PanelEntry::FoldedDirs(_)) | None => None,
        };
        let Some(buffer_id) = buffer_id else {
            return;
        };
        let file_entry = self
            .fs_entries
            .iter()
            .find(|fs_entry| match fs_entry {
                FsEntry::File(FsEntryFile {
                    buffer_id: file_buffer_id,
                    ..
                })
                | FsEntry::ExternalFile(FsEntryExternalFile {
                    buffer_id: file_buffer_id,
                    ..
                }) => *file_buffer_id == buffer_id,
                FsEntry::Directory(..) => false,
            })
            .cloned();
        if let Some(file_entry) = file_entry {
            self.select_entry(PanelEntry::Fs(file_entry), false, window, cx);
        }
    }

    fn invalidate_outlines(&mut self, ids: &[BufferId]) {
        if let Some(PanelEntry::Outline(outline)) = self.selected_entry()
            && ids.contains(&outline.buffer_id())
        {
            self.selected_entry.invalidate();
        }
        for buffer_id in ids {
            self.outline_fetch_tasks.remove(buffer_id);
            if let Some(buffer) = self.buffers.get_mut(buffer_id) {
                buffer.invalidate_outlines();
            }
        }
    }

    fn buffers_to_fetch(&self, cx: &App) -> HashSet<BufferId> {
        // Hide-symbols mode never renders outline rows, so fetching outlines would be wasted
        // work; `update_fs_entries`'s completion only refreshes cached entries once this is
        // empty, so the gate has to live here rather than in `fetch_outdated_outlines` alone.
        if self.multi_buffer_active(cx) && self.hide_symbols_active(cx) {
            return HashSet::default();
        }
        self.fs_entries
            .iter()
            .fold(HashSet::default(), |mut buffers_to_fetch, fs_entry| {
                match fs_entry {
                    FsEntry::File(FsEntryFile { buffer_id, .. })
                    | FsEntry::ExternalFile(FsEntryExternalFile { buffer_id, .. }) => {
                        if let Some(buffer) = self.buffers.get(buffer_id)
                            && buffer.should_fetch_outlines()
                        {
                            buffers_to_fetch.insert(*buffer_id);
                        }
                    }
                    FsEntry::Directory(..) => {}
                }
                buffers_to_fetch
            })
    }

    fn buffer_snapshot_for_id(&self, buffer_id: BufferId, cx: &App) -> Option<BufferSnapshot> {
        let editor = self.active_editor()?;
        Some(
            editor
                .read(cx)
                .buffer()
                .read(cx)
                .buffer(buffer_id)?
                .read(cx)
                .snapshot(),
        )
    }

    fn abs_path(&self, entry: &PanelEntry, cx: &App) -> Option<PathBuf> {
        match entry {
            PanelEntry::Fs(
                FsEntry::File(FsEntryFile { buffer_id, .. })
                | FsEntry::ExternalFile(FsEntryExternalFile { buffer_id, .. }),
            ) => self
                .buffer_snapshot_for_id(*buffer_id, cx)
                .and_then(|buffer_snapshot| {
                    let file = File::from_dyn(buffer_snapshot.file())?;
                    Some(file.worktree.read(cx).absolutize(&file.path))
                }),
            PanelEntry::Fs(FsEntry::Directory(FsEntryDirectory {
                worktree_id, entry, ..
            })) => Some(
                self.project
                    .read(cx)
                    .worktree_for_id(*worktree_id, cx)?
                    .read(cx)
                    .absolutize(&entry.path),
            ),
            PanelEntry::FoldedDirs(FoldedDirsEntry {
                worktree_id,
                entries: dirs,
                ..
            }) => dirs.last().and_then(|entry| {
                self.project
                    .read(cx)
                    .worktree_for_id(*worktree_id, cx)
                    .map(|worktree| worktree.read(cx).absolutize(&entry.path))
            }),
            PanelEntry::Search(_) | PanelEntry::Outline(..) => None,
        }
    }

    fn relative_path(&self, entry: &FsEntry, cx: &App) -> Option<Arc<RelPath>> {
        match entry {
            FsEntry::ExternalFile(FsEntryExternalFile { buffer_id, .. }) => {
                let buffer_snapshot = self.buffer_snapshot_for_id(*buffer_id, cx)?;
                Some(buffer_snapshot.file()?.path().clone())
            }
            FsEntry::Directory(FsEntryDirectory { entry, .. }) => Some(entry.path.clone()),
            FsEntry::File(FsEntryFile { entry, .. }) => Some(entry.path.clone()),
        }
    }

    fn update_contents(
        &mut self,
        debounce: Option<Duration>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.active {
            return;
        }
        self.cached_entries_update_pending = Some(CachedEntriesUpdate::Contents);
        self.update_cached_entries(debounce, window, cx);
    }

    fn update_cached_entries(
        &mut self,
        debounce: Option<Duration>,
        window: &mut Window,
        cx: &mut Context<OutlinePanel>,
    ) {
        if !self.active {
            return;
        }

        self.cached_entries_update_pending
            .get_or_insert(CachedEntriesUpdate::Entries);
        if !self.cached_entries_update_task.is_ready() {
            return;
        }

        self.cached_entries_update_task = cx.spawn_in(window, async move |outline_panel, cx| {
            let mut debounce = debounce;
            loop {
                if let Some(debounce) = debounce {
                    cx.background_executor().timer(debounce).await;
                }
                let Some(new_cached_entries) = outline_panel
                    .update_in(cx, |outline_panel, window, cx| {
                        if outline_panel.cached_entries_update_pending.take()
                            == Some(CachedEntriesUpdate::Contents)
                        {
                            outline_panel.update_non_fs_items(window, cx);
                        }
                        let is_singleton = outline_panel.is_singleton_active(cx);
                        let query = outline_panel.query(cx);
                        outline_panel.generate_cached_entries(is_singleton, query, window, cx)
                    })
                    .ok()
                else {
                    return;
                };
                let (new_cached_entries, max_width_item_index) = new_cached_entries.await;
                outline_panel
                    .update_in(cx, |outline_panel, window, cx| {
                        outline_panel.cached_entries = new_cached_entries;
                        outline_panel.max_width_item_index = max_width_item_index;
                        if let SelectedEntry::Valid(selected, _) = &outline_panel.selected_entry
                            && let Some((index, cached)) =
                                outline_panel.cached_entry_for_selection(selected)
                        {
                            outline_panel.selected_entry =
                                SelectedEntry::Valid(cached.entry.clone(), index);
                        }
                        if (outline_panel.selected_entry.is_invalidated()
                            || matches!(outline_panel.selected_entry, SelectedEntry::None))
                            && let Some(new_selected_entry) =
                                outline_panel.active_editor().and_then(|active_editor| {
                                    outline_panel.location_for_editor_selection(
                                        &active_editor,
                                        window,
                                        cx,
                                    )
                                })
                        {
                            let awaiting_outlines = match &new_selected_entry {
                                PanelEntry::Outline(outline) => outline_panel
                                    .buffers
                                    .get(&outline.buffer_id())
                                    .is_some_and(BufferOutlines::should_fetch_outlines),
                                _ => false,
                            };
                            outline_panel.select_entry(new_selected_entry, false, window, cx);
                            if awaiting_outlines {
                                outline_panel.selected_entry.invalidate();
                            }
                        }

                        cx.notify();
                    })
                    .ok();
                if !outline_panel
                    .read_with(cx, |panel, _| panel.cached_entries_update_pending.is_some())
                    .unwrap_or(false)
                {
                    break;
                }
                debounce = Some(UPDATE_DEBOUNCE);
            }
        });
    }

    fn cached_entry_for_selection(&self, selected: &PanelEntry) -> Option<(usize, &CachedEntry)> {
        if let Some(entry) = self
            .cached_entries
            .iter()
            .enumerate()
            .find(|(_, cached)| &cached.entry == selected)
        {
            return Some(entry);
        }
        let (worktree_id, path, first_buffer_id) = match selected {
            PanelEntry::Fs(FsEntry::Directory(directory)) => (
                directory.worktree_id,
                &directory.entry.path,
                directory.first_buffer_id,
            ),
            PanelEntry::FoldedDirs(group) => (
                group.worktree_id,
                &group.entries.last()?.path,
                group.first_buffer_id,
            ),
            _ => return None,
        };
        let mut first_section = None;
        let mut current_section = None;
        let mut selected_section = None;
        for entry in &self.fs_entries {
            match entry {
                FsEntry::Directory(directory)
                    if directory.worktree_id == worktree_id && directory.entry.path == *path =>
                {
                    first_section.get_or_insert(directory.first_buffer_id);
                    current_section = Some(directory.first_buffer_id);
                }
                FsEntry::File(file) if file.buffer_id == first_buffer_id => {
                    if file.worktree_id == worktree_id
                        && file
                            .entry
                            .path
                            .parent()
                            .is_some_and(|parent| parent.starts_with(path))
                    {
                        selected_section = current_section;
                        break;
                    }
                }
                _ => {}
            }
        }
        let selected_section = selected_section.or(first_section)?;
        self.cached_entries
            .iter()
            .enumerate()
            .find(|(_, cached)| match &cached.entry {
                PanelEntry::Fs(FsEntry::Directory(directory)) => {
                    directory.worktree_id == worktree_id
                        && directory.first_buffer_id == selected_section
                        && directory.entry.path == *path
                }
                PanelEntry::FoldedDirs(group) => {
                    group.worktree_id == worktree_id
                        && group.first_buffer_id == selected_section
                        && group.entries.iter().any(|entry| entry.path == *path)
                }
                _ => false,
            })
    }

    fn generate_cached_entries(
        &self,
        is_singleton: bool,
        query: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<(Vec<CachedEntry>, Option<usize>)> {
        let Some(active_editor) = self.active_editor() else {
            return Task::ready((Vec::new(), None));
        };
        cx.spawn_in(window, async move |outline_panel, cx| {
            let mut generation_state = GenerationState::default();

            let Ok(()) = outline_panel.update(cx, |outline_panel, cx| {
                let auto_fold_dirs = OutlinePanelSettings::get_global(cx).auto_fold_dirs;
                let hide_symbols = !is_singleton && outline_panel.hide_symbols_active(cx);
                let mut folded_dirs_entry = None::<(usize, FoldedDirsEntry)>;
                let track_matches = query.is_some();

                #[derive(Debug)]
                struct ParentStats {
                    path: Arc<RelPath>,
                    folded: bool,
                    expanded: bool,
                    depth: usize,
                    worktree_id: WorktreeId,
                }

                let search_precomputed = if hide_symbols {
                    None
                } else if let ItemsDisplayMode::Search(search_state) = &outline_panel.mode {
                    let multi_buffer_snapshot =
                        active_editor.read(cx).buffer().read(cx).snapshot(cx);
                    let mut folded_buffers = HashSet::default();
                    let mut not_folded_buffers = HashSet::default();
                    let mut matches_by_buffer = HashMap::default();

                    for (match_range, search_data) in &search_state.matches {
                        let Some((start_anchor, _)) =
                            multi_buffer_snapshot.anchor_to_buffer_anchor(match_range.start)
                        else {
                            continue;
                        };
                        let start_buffer_id = start_anchor.buffer_id;
                        let end_buffer_id = multi_buffer_snapshot
                            .anchor_to_buffer_anchor(match_range.end)
                            .map(|(anchor, _)| anchor.buffer_id);

                        let mut any_folded = false;
                        for buffer_id in
                            [Some(start_buffer_id), end_buffer_id].into_iter().flatten()
                        {
                            if folded_buffers.contains(&buffer_id) {
                                any_folded = true;
                            } else if !not_folded_buffers.contains(&buffer_id) {
                                if active_editor.read(cx).is_buffer_folded(buffer_id, cx) {
                                    folded_buffers.insert(buffer_id);
                                    any_folded = true;
                                } else {
                                    not_folded_buffers.insert(buffer_id);
                                }
                            }
                        }
                        if any_folded {
                            continue;
                        }

                        matches_by_buffer
                            .entry(start_buffer_id)
                            .or_insert_with(Vec::new)
                            .push((match_range.clone(), Arc::clone(search_data)));
                    }

                    Some(SearchPrecomputed {
                        multi_buffer_snapshot,
                        matches_by_buffer,
                        folded_buffers,
                    })
                } else {
                    None
                };

                let mut parent_dirs = Vec::<ParentStats>::new();
                for entry in outline_panel.fs_entries.clone() {
                    let is_expanded = outline_panel.is_expanded(&entry);
                    let (depth, should_add) = match &entry {
                        FsEntry::Directory(directory_entry) => {
                            let mut should_add = true;
                            let is_root = directory_entry.entry.path.is_empty();
                            let folded = auto_fold_dirs
                                && !is_root
                                && outline_panel
                                    .unfolded_dirs
                                    .get(&directory_entry.worktree_id)
                                    .is_none_or(|unfolded_dirs| {
                                        !unfolded_dirs.contains(&directory_entry.entry.path)
                                    });
                            let fs_depth = directory_entry.entry.path.components().count();
                            while let Some(parent) = parent_dirs.last() {
                                if !is_root
                                    && parent.worktree_id == directory_entry.worktree_id
                                    && directory_entry.entry.path.starts_with(&parent.path)
                                {
                                    break;
                                }
                                parent_dirs.pop();
                            }
                            let auto_fold = match parent_dirs.last() {
                                Some(parent) => {
                                    parent.folded
                                        && Some(parent.path.as_ref())
                                            == directory_entry.entry.path.parent()
                                        && outline_panel
                                            .fs_children_count
                                            .get(&directory_entry.worktree_id)
                                            .and_then(|entries| {
                                                entries.get(&directory_entry.entry.path)
                                            })
                                            .copied()
                                            .unwrap_or_default()
                                            .may_be_fold_part()
                                }
                                None => false,
                            };
                            let folded = folded || auto_fold;
                            let (depth, parent_expanded, parent_folded) = match parent_dirs.last() {
                                Some(parent) => {
                                    let parent_folded = parent.folded;
                                    let parent_expanded = parent.expanded;
                                    let new_depth = if parent_folded {
                                        parent.depth
                                    } else {
                                        parent.depth + 1
                                    };
                                    parent_dirs.push(ParentStats {
                                        worktree_id: directory_entry.worktree_id,
                                        path: directory_entry.entry.path.clone(),
                                        folded,
                                        expanded: parent_expanded && is_expanded,
                                        depth: new_depth,
                                    });
                                    (new_depth, parent_expanded, parent_folded)
                                }
                                None => {
                                    parent_dirs.push(ParentStats {
                                        worktree_id: directory_entry.worktree_id,
                                        path: directory_entry.entry.path.clone(),
                                        folded,
                                        expanded: is_expanded,
                                        depth: fs_depth,
                                    });
                                    (fs_depth, true, false)
                                }
                            };

                            if let Some((folded_depth, mut folded_dirs)) = folded_dirs_entry.take()
                            {
                                if folded
                                    && directory_entry.worktree_id == folded_dirs.worktree_id
                                    && directory_entry.entry.path.parent()
                                        == folded_dirs
                                            .entries
                                            .last()
                                            .map(|entry| entry.path.as_ref())
                                {
                                    folded_dirs.entries.push(directory_entry.entry.clone());
                                    folded_dirs_entry = Some((folded_depth, folded_dirs))
                                } else {
                                    if !is_singleton {
                                        let start_of_collapsed_dir_sequence = !parent_expanded
                                            && parent_dirs
                                                .iter()
                                                .rev()
                                                .nth(folded_dirs.entries.len() + 1)
                                                .is_none_or(|parent| parent.expanded);
                                        if start_of_collapsed_dir_sequence
                                            || parent_expanded
                                            || query.is_some()
                                        {
                                            if parent_folded {
                                                folded_dirs
                                                    .entries
                                                    .push(directory_entry.entry.clone());
                                                should_add = false;
                                            }
                                            let new_folded_dirs =
                                                PanelEntry::FoldedDirs(folded_dirs.clone());
                                            outline_panel.push_entry(
                                                &mut generation_state,
                                                track_matches,
                                                new_folded_dirs,
                                                folded_depth,
                                                cx,
                                            );
                                        }
                                    }

                                    folded_dirs_entry = if parent_folded {
                                        None
                                    } else {
                                        Some((
                                            depth,
                                            FoldedDirsEntry {
                                                worktree_id: directory_entry.worktree_id,
                                                first_buffer_id: directory_entry.first_buffer_id,
                                                entries: vec![directory_entry.entry.clone()],
                                            },
                                        ))
                                    };
                                }
                            } else if folded {
                                folded_dirs_entry = Some((
                                    depth,
                                    FoldedDirsEntry {
                                        worktree_id: directory_entry.worktree_id,
                                        first_buffer_id: directory_entry.first_buffer_id,
                                        entries: vec![directory_entry.entry.clone()],
                                    },
                                ));
                            }

                            let should_add =
                                should_add && parent_expanded && folded_dirs_entry.is_none();
                            (depth, should_add)
                        }
                        FsEntry::ExternalFile(..) => {
                            if let Some((folded_depth, folded_dir)) = folded_dirs_entry.take() {
                                let folded_paths = folded_dir
                                    .entries
                                    .iter()
                                    .map(|entry| &entry.path)
                                    .collect::<HashSet<_>>();
                                let parent_expanded = parent_dirs
                                    .iter()
                                    .rev()
                                    .find(|parent| !folded_paths.contains(&parent.path))
                                    .is_none_or(|parent| parent.expanded);
                                if !is_singleton && (parent_expanded || query.is_some()) {
                                    outline_panel.push_entry(
                                        &mut generation_state,
                                        track_matches,
                                        PanelEntry::FoldedDirs(folded_dir),
                                        folded_depth,
                                        cx,
                                    );
                                }
                            }
                            parent_dirs.clear();
                            (0, true)
                        }
                        FsEntry::File(file) => {
                            if let Some((folded_depth, folded_dirs)) = folded_dirs_entry.take() {
                                let folded_paths = folded_dirs
                                    .entries
                                    .iter()
                                    .map(|entry| &entry.path)
                                    .collect::<HashSet<_>>();
                                let parent_expanded = parent_dirs
                                    .iter()
                                    .rev()
                                    .find(|parent| !folded_paths.contains(&parent.path))
                                    .is_none_or(|parent| parent.expanded);
                                if !is_singleton && (parent_expanded || query.is_some()) {
                                    outline_panel.push_entry(
                                        &mut generation_state,
                                        track_matches,
                                        PanelEntry::FoldedDirs(folded_dirs),
                                        folded_depth,
                                        cx,
                                    );
                                }
                            }

                            let fs_depth = file.entry.path.components().count();
                            while let Some(parent) = parent_dirs.last() {
                                if parent.worktree_id == file.worktree_id
                                    && file
                                        .entry
                                        .path
                                        .parent()
                                        .is_some_and(|path| path.starts_with(&parent.path))
                                {
                                    break;
                                }
                                parent_dirs.pop();
                            }
                            match parent_dirs.last() {
                                Some(parent) => {
                                    let new_depth = parent.depth + 1;
                                    (new_depth, parent.expanded)
                                }
                                None => (fs_depth, true),
                            }
                        }
                    };

                    if !is_singleton
                        && (should_add || (query.is_some() && folded_dirs_entry.is_none()))
                    {
                        outline_panel.push_entry(
                            &mut generation_state,
                            track_matches,
                            PanelEntry::Fs(entry.clone()),
                            depth,
                            cx,
                        );
                    }

                    if !hide_symbols {
                        match outline_panel.mode {
                            ItemsDisplayMode::Search(_) => {
                                if (is_singleton || query.is_some() || (should_add && is_expanded))
                                    && let Some(search) = &search_precomputed
                                {
                                    outline_panel.add_search_entries(
                                        &mut generation_state,
                                        search,
                                        &entry,
                                        depth,
                                        query.is_some(),
                                        is_singleton,
                                        cx,
                                    );
                                }
                            }
                            ItemsDisplayMode::Outline => {
                                let excerpts_to_consider = if is_singleton
                                    || query.is_some()
                                    || (should_add && is_expanded)
                                {
                                    match &entry {
                                        FsEntry::File(FsEntryFile {
                                            buffer_id,
                                            excerpts,
                                            ..
                                        })
                                        | FsEntry::ExternalFile(FsEntryExternalFile {
                                            buffer_id,
                                            excerpts,
                                            ..
                                        }) => Some((*buffer_id, excerpts)),
                                        _ => None,
                                    }
                                } else {
                                    None
                                };
                                if let Some((buffer_id, _entry_excerpts)) = excerpts_to_consider
                                    && !active_editor.read(cx).is_buffer_folded(buffer_id, cx)
                                {
                                    outline_panel.add_buffer_entries(
                                        &mut generation_state,
                                        buffer_id,
                                        depth,
                                        track_matches,
                                        is_singleton,
                                        query.as_deref(),
                                        cx,
                                    );
                                }
                            }
                        }
                    }

                    if is_singleton
                        && matches!(entry, FsEntry::File(..) | FsEntry::ExternalFile(..))
                        && !generation_state.entries.iter().any(|item| {
                            matches!(item.entry, PanelEntry::Outline(..) | PanelEntry::Search(_))
                        })
                    {
                        outline_panel.push_entry(
                            &mut generation_state,
                            track_matches,
                            PanelEntry::Fs(entry.clone()),
                            0,
                            cx,
                        );
                    }
                }

                if let Some((folded_depth, folded_dirs)) = folded_dirs_entry.take() {
                    let folded_paths = folded_dirs
                        .entries
                        .iter()
                        .map(|entry| &entry.path)
                        .collect::<HashSet<_>>();
                    let parent_expanded = parent_dirs
                        .iter()
                        .rev()
                        .find(|parent| !folded_paths.contains(&parent.path))
                        .is_none_or(|parent| parent.expanded);
                    if parent_expanded || query.is_some() {
                        outline_panel.push_entry(
                            &mut generation_state,
                            track_matches,
                            PanelEntry::FoldedDirs(folded_dirs),
                            folded_depth,
                            cx,
                        );
                    }
                }
            }) else {
                return (Vec::new(), None);
            };

            let Some(query) = query else {
                return (
                    generation_state.entries,
                    generation_state
                        .max_width_estimate_and_index
                        .map(|(_, index)| index),
                );
            };

            let mut matched_ids = match_strings(
                &generation_state.match_candidates,
                &query,
                true,
                true,
                usize::MAX,
                &AtomicBool::default(),
                cx.background_executor().clone(),
            )
            .await
            .into_iter()
            .map(|string_match| (string_match.candidate_id, string_match))
            .collect::<HashMap<_, _>>();

            let mut id = 0;
            generation_state.entries.retain_mut(|cached_entry| {
                let retain = match matched_ids.remove(&id) {
                    Some(string_match) => {
                        cached_entry.string_match = Some(string_match);
                        true
                    }
                    None => false,
                };
                id += 1;
                retain
            });

            (
                generation_state.entries,
                generation_state
                    .max_width_estimate_and_index
                    .map(|(_, index)| index),
            )
        })
    }

    fn push_entry(
        &self,
        state: &mut GenerationState,
        track_matches: bool,
        entry: PanelEntry,
        depth: usize,
        cx: &mut App,
    ) {
        let entry = if let PanelEntry::FoldedDirs(folded_dirs_entry) = &entry {
            match folded_dirs_entry.entries.len() {
                0 => {
                    debug_panic!("Empty folded dirs receiver");
                    return;
                }
                1 => PanelEntry::Fs(FsEntry::Directory(FsEntryDirectory {
                    worktree_id: folded_dirs_entry.worktree_id,
                    first_buffer_id: folded_dirs_entry.first_buffer_id,
                    entry: folded_dirs_entry.entries[0].clone(),
                })),
                _ => entry,
            }
        } else {
            entry
        };

        if track_matches {
            let id = state.entries.len();
            match &entry {
                PanelEntry::Fs(fs_entry) => {
                    let candidate_label = match fs_entry {
                        FsEntry::Directory(directory) => {
                            directory.entry.path.file_name().map(str::to_string)
                        }
                        _ => self.fs_entry_label(fs_entry, cx),
                    };
                    if let Some(file_name) = candidate_label {
                        state
                            .match_candidates
                            .push(StringMatchCandidate::new(id, &file_name));
                    }
                }
                PanelEntry::FoldedDirs(folded_dir_entry) => {
                    let dir_names = self.dir_names_string(
                        &folded_dir_entry.entries,
                        folded_dir_entry.worktree_id,
                        cx,
                    );
                    {
                        state
                            .match_candidates
                            .push(StringMatchCandidate::new(id, &dir_names));
                    }
                }
                PanelEntry::Outline(OutlineEntry::Outline(outline_entry)) => state
                    .match_candidates
                    .push(StringMatchCandidate::new(id, &outline_entry.text)),
                PanelEntry::Outline(OutlineEntry::Excerpt(_)) => {}
                PanelEntry::Search(new_search_entry) => {
                    if let Some(search_data) = new_search_entry.render_data.get() {
                        state
                            .match_candidates
                            .push(StringMatchCandidate::new(id, &search_data.context_text));
                    }
                }
            }
        }

        let width_estimate = self.width_estimate(depth, &entry, cx);
        if Some(width_estimate)
            > state
                .max_width_estimate_and_index
                .map(|(estimate, _)| estimate)
        {
            state.max_width_estimate_and_index = Some((width_estimate, state.entries.len()));
        }
        state.entries.push(CachedEntry {
            depth,
            entry,
            string_match: None,
        });
    }

    fn dir_names_string(
        &self,
        entries: &[FsEntryPath],
        worktree_id: WorktreeId,
        cx: &App,
    ) -> String {
        let dir_names_segment = entries
            .iter()
            .map(|entry| self.entry_name(&worktree_id, entry, cx))
            .collect::<PathBuf>();
        dir_names_segment.to_string_lossy().into_owned()
    }

    fn query(&self, cx: &App) -> Option<String> {
        let query = self.filter_editor.read(cx).text(cx);
        if query.trim().is_empty() {
            None
        } else {
            Some(query)
        }
    }

    fn is_expanded(&self, entry: &FsEntry) -> bool {
        let entry_to_check = match entry {
            FsEntry::ExternalFile(FsEntryExternalFile { buffer_id, .. }) => {
                CollapsedEntry::ExternalFile(*buffer_id)
            }
            FsEntry::File(FsEntryFile {
                worktree_id,
                buffer_id,
                ..
            }) => CollapsedEntry::File(*worktree_id, *buffer_id),
            FsEntry::Directory(FsEntryDirectory {
                worktree_id, entry, ..
            }) => CollapsedEntry::Dir(*worktree_id, entry.path.clone()),
        };
        !self.collapsed_entries.contains(&entry_to_check)
    }

    fn update_non_fs_items(&mut self, window: &mut Window, cx: &mut Context<OutlinePanel>) {
        if !self.active {
            return;
        }

        if self.cached_entries_update_pending == Some(CachedEntriesUpdate::Contents) {
            self.cached_entries_update_pending = Some(CachedEntriesUpdate::Entries);
        }
        if self.update_search_matches(window, cx) {
            self.selected_entry.invalidate();
        }
        self.fetch_outdated_outlines(None, window, cx);
    }

    fn update_search_matches(
        &mut self,
        window: &mut Window,
        cx: &mut Context<OutlinePanel>,
    ) -> bool {
        if !self.active {
            return false;
        }

        let project_search = self
            .active_item()
            .and_then(|item| item.downcast::<ProjectSearchView>());

        let buffer_search_matches = self
            .active_editor()
            .map(|active_editor| {
                active_editor.update(cx, |editor, cx| editor.get_matches(window, cx).0)
            })
            .unwrap_or_default();
        let project_search_matches = if buffer_search_matches.is_empty() {
            project_search
                .as_ref()
                .map(|project_search| project_search.read(cx).get_matches(cx))
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let mut update_cached_entries = false;
        if buffer_search_matches.is_empty() && project_search_matches.is_empty() {
            if matches!(self.mode, ItemsDisplayMode::Search(_)) {
                self.mode = ItemsDisplayMode::Outline;
                update_cached_entries = true;
            }
        } else {
            let (kind, new_search_matches, new_search_query) = if buffer_search_matches.is_empty() {
                (
                    SearchKind::Project,
                    project_search_matches,
                    project_search
                        .map(|project_search| project_search.read(cx).search_query_text(cx))
                        .unwrap_or_default(),
                )
            } else {
                let buffer_search = self
                    .active_item()
                    .as_deref()
                    .and_then(|active_item| {
                        self.workspace
                            .upgrade()
                            .and_then(|workspace| workspace.read(cx).pane_for(active_item))
                    })
                    .and_then(|pane| {
                        pane.read(cx)
                            .toolbar()
                            .read(cx)
                            .item_of_type::<BufferSearchBar>()
                    });
                (
                    SearchKind::Buffer,
                    buffer_search_matches,
                    buffer_search
                        .map(|buffer_search| buffer_search.read(cx).query(cx))
                        .unwrap_or_default(),
                )
            };

            let changed = match &self.mode {
                ItemsDisplayMode::Search(current) => {
                    current.query != new_search_query
                        || current.kind != kind
                        || current.matches.len() != new_search_matches.len()
                        || current
                            .matches
                            .iter()
                            .zip(&new_search_matches)
                            .any(|((existing, _), incoming)| existing != incoming)
                }
                ItemsDisplayMode::Outline => true,
            };
            if changed {
                let previous_matches = match &mut self.mode {
                    ItemsDisplayMode::Search(current) if current.kind == kind => {
                        current.matches.drain(..).collect()
                    }
                    _ => HashMap::default(),
                };
                self.mode = ItemsDisplayMode::Search(SearchState::new(
                    kind,
                    new_search_query,
                    previous_matches,
                    new_search_matches,
                    cx.theme().syntax().clone(),
                    window,
                    cx,
                ));
                update_cached_entries = true;
            }
        }
        update_cached_entries
    }

    fn add_buffer_entries(
        &mut self,
        state: &mut GenerationState,
        buffer_id: BufferId,
        parent_depth: usize,
        track_matches: bool,
        is_singleton: bool,
        query: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let Some(buffer) = self.buffers.get(&buffer_id) else {
            return;
        };
        let buffer_snapshot = self.buffer_snapshot_for_id(buffer_id, cx);
        let snapshot = buffer_snapshot.as_ref();
        let mut excerpts = buffer
            .excerpts
            .iter()
            .map(|excerpt| ExcerptOutlines {
                excerpt,
                ranked_range: 0..0,
                expanded: is_singleton
                    || query.is_some()
                    || !self
                        .collapsed_entries
                        .contains(&CollapsedEntry::Excerpt(excerpt.clone())),
                visible: Vec::new(),
                children_to_cache: Vec::new(),
            })
            .collect::<Vec<_>>();
        let expanded_excerpts = excerpts.iter().filter(|excerpt| excerpt.expanded).count();
        if expanded_excerpts > 0 {
            let children = self.outline_children_cache.entry(buffer_id).or_default();
            let outlines = buffer.iter_outlines().collect::<Vec<_>>();
            if let Some(snapshot) = snapshot
                && expanded_excerpts > 1
                && !outlines.is_empty()
            {
                let mut coordinates = Vec::new();
                for outline in &outlines {
                    coordinates.extend([outline.range.start, outline.range.end]);
                }
                for excerpt in excerpts.iter().filter(|excerpt| excerpt.expanded) {
                    coordinates
                        .extend([excerpt.excerpt.context.start, excerpt.excerpt.context.end]);
                }
                coordinates.sort_unstable_by(|left, right| left.cmp(right, snapshot));
                coordinates.dedup_by(|left, right| left.cmp(right, snapshot).is_eq());
                let rank = |anchor: Anchor| {
                    coordinates
                        .partition_point(|coordinate| coordinate.cmp(&anchor, snapshot).is_lt())
                };
                let outlines = outlines
                    .iter()
                    .map(|outline| IndexedOutline {
                        outline,
                        ranked_range: rank(outline.range.start)..rank(outline.range.end),
                        collapsed: self
                            .collapsed_entries
                            .contains(&CollapsedEntry::Outline(outline.range.clone())),
                    })
                    .collect::<Vec<_>>();
                for excerpt in excerpts.iter_mut().filter(|excerpt| excerpt.expanded) {
                    excerpt.ranked_range =
                        rank(excerpt.excerpt.context.start)..rank(excerpt.excerpt.context.end);
                }
                BufferOutlines::assign_outline_cache_owners(&outlines, &mut excerpts);
                if BufferOutlines::outlines_are_well_formed(&outlines) {
                    BufferOutlines::collect_nested_outlines(&outlines, &mut excerpts, children);
                } else {
                    BufferOutlines::collect_overlapping_outlines(
                        &outlines,
                        &mut excerpts,
                        children,
                    );
                }
            } else {
                for excerpt in excerpts.iter_mut().filter(|excerpt| excerpt.expanded) {
                    let mut matches = outlines
                        .iter()
                        .copied()
                        .filter(|outline| {
                            snapshot.is_none_or(|snapshot| {
                                outline
                                    .range
                                    .start
                                    .cmp(&excerpt.excerpt.context.end, snapshot)
                                    .is_le()
                                    && outline
                                        .range
                                        .end
                                        .cmp(&excerpt.excerpt.context.start, snapshot)
                                        .is_ge()
                            })
                        })
                        .peekable();
                    let mut collapsed_ancestor: Option<&Outline> = None;
                    while let Some(outline) = matches.next() {
                        children.insert(
                            (outline.range.clone(), outline.depth),
                            matches
                                .peek()
                                .is_some_and(|next| next.depth > outline.depth),
                        );
                        if let Some(ancestor) = collapsed_ancestor
                            && let Some(snapshot) = snapshot
                            && outline.depth > ancestor.depth
                            && outline
                                .range
                                .start
                                .cmp(&ancestor.range.start, snapshot)
                                .is_ge()
                            && outline
                                .range
                                .start
                                .cmp(&ancestor.range.end, snapshot)
                                .is_lt()
                        {
                            continue;
                        }
                        excerpt.visible.push(outline);
                        collapsed_ancestor = self
                            .collapsed_entries
                            .contains(&CollapsedEntry::Outline(outline.range.clone()))
                            .then_some(outline);
                    }
                }
            }
        }
        for excerpt in excerpts {
            let excerpt_depth = parent_depth + 1;
            self.push_entry(
                state,
                track_matches,
                PanelEntry::Outline(OutlineEntry::Excerpt(excerpt.excerpt.clone())),
                excerpt_depth,
                cx,
            );
            let outline_base_depth = if is_singleton {
                state.clear();
                0
            } else {
                excerpt_depth + 1
            };
            for outline in excerpt.visible {
                self.push_entry(
                    state,
                    track_matches,
                    PanelEntry::Outline(OutlineEntry::Outline(outline.clone())),
                    outline_base_depth + outline.depth,
                    cx,
                );
            }
        }
    }

    fn add_search_entries(
        &mut self,
        state: &mut GenerationState,
        search: &SearchPrecomputed,
        parent_entry: &FsEntry,
        parent_depth: usize,
        track_matches: bool,
        is_singleton: bool,
        cx: &mut Context<Self>,
    ) {
        let ItemsDisplayMode::Search(search_state) = &self.mode else {
            return;
        };
        let kind = search_state.kind;

        let (buffer_id, excerpts) = match parent_entry {
            FsEntry::Directory(_) => return,
            FsEntry::ExternalFile(external) => (external.buffer_id, &external.excerpts),
            FsEntry::File(file) => (file.buffer_id, &file.excerpts),
        };

        if search.folded_buffers.contains(&buffer_id) {
            return;
        }
        let Some(buffer_matches) = search.matches_by_buffer.get(&buffer_id) else {
            return;
        };

        let mut excerpt_ranges = excerpts
            .iter()
            .filter_map(|excerpt| {
                let start = search
                    .multi_buffer_snapshot
                    .anchor_in_buffer(excerpt.context.start)?;
                let end = search
                    .multi_buffer_snapshot
                    .anchor_in_buffer(excerpt.context.end)?;
                Some(start..end)
            })
            .collect::<Vec<_>>();

        let snapshot = &search.multi_buffer_snapshot;
        excerpt_ranges.sort_unstable_by(|left, right| left.start.cmp(&right.start, snapshot));
        let mut maximum_end = None;
        for range in &mut excerpt_ranges {
            if let Some(end) = maximum_end
                && range.end.cmp(&end, snapshot).is_lt()
            {
                range.end = end;
            }
            maximum_end = Some(range.end);
        }
        let depth = if is_singleton { 0 } else { parent_depth + 1 };
        for (match_range, search_data) in buffer_matches.iter().filter(|(match_range, _)| {
            let index = excerpt_ranges
                .partition_point(|excerpt| excerpt.start.cmp(&match_range.end, snapshot).is_le());
            index
                .checked_sub(1)
                .and_then(|index| excerpt_ranges.get(index))
                .is_some_and(|excerpt| excerpt.end.cmp(&match_range.start, snapshot).is_ge())
        }) {
            self.push_entry(
                state,
                track_matches,
                PanelEntry::Search(SearchEntry {
                    match_range: match_range.clone(),
                    kind,
                    render_data: Arc::clone(search_data),
                }),
                depth,
                cx,
            );
        }
    }

    fn active_editor(&self) -> Option<Entity<Editor>> {
        self.active_item.as_ref()?.active_editor.upgrade()
    }

    fn active_item(&self) -> Option<Box<dyn ItemHandle>> {
        self.active_item.as_ref()?.item_handle.upgrade()
    }

    fn should_replace_active_item(&self, new_active_item: &dyn ItemHandle) -> bool {
        self.active_item().is_none_or(|active_item| {
            !self.pinned && active_item.item_id() != new_active_item.item_id()
        })
    }

    pub fn toggle_active_editor_pin(
        &mut self,
        _: &ToggleActiveEditorPin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pinned = !self.pinned;
        if !self.pinned
            && let Some((active_item, active_editor)) = self
                .workspace
                .upgrade()
                .and_then(|workspace| workspace_active_editor(workspace.read(cx), cx))
            && self.should_replace_active_item(active_item.as_ref())
        {
            self.replace_active_editor(active_item, active_editor, window, cx);
        }

        cx.notify();
    }

    pub fn toggle_symbols(
        &mut self,
        _: &ToggleSymbols,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.multi_buffer_active(cx) {
            return;
        }
        self.hide_symbols_override = Some(!self.hide_symbols_active(cx));
        self.update_non_fs_items(window, cx);
        self.remap_selection_for_hidden_symbols(window, cx);
        self.update_cached_entries(None, window, cx);
        cx.notify();
    }

    fn selected_entry(&self) -> Option<&PanelEntry> {
        match &self.selected_entry {
            SelectedEntry::Invalidated(entry) => entry.as_ref(),
            SelectedEntry::Valid(entry, _) => Some(entry),
            SelectedEntry::None => None,
        }
    }

    fn select_entry(
        &mut self,
        entry: PanelEntry,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if focus {
            self.focus_handle.focus(window, cx);
        }
        let ix = self
            .cached_entries
            .iter()
            .enumerate()
            .find(|(_, cached_entry)| &cached_entry.entry == &entry)
            .map(|(i, _)| i)
            .unwrap_or_default();

        self.selected_entry = SelectedEntry::Valid(entry, ix);

        self.autoscroll(cx);
        cx.notify();
    }

    fn width_estimate(&self, depth: usize, entry: &PanelEntry, cx: &App) -> u64 {
        let item_text_chars = match entry {
            PanelEntry::Fs(fs_entry) => self
                .fs_entry_label(fs_entry, cx)
                .map(|label| label.len())
                .unwrap_or_default(),
            PanelEntry::FoldedDirs(folded_dirs) => {
                folded_dirs
                    .entries
                    .iter()
                    .map(|dir| {
                        dir.path
                            .file_name()
                            .map(|name| name.len())
                            .unwrap_or_default()
                    })
                    .sum::<usize>()
                    + folded_dirs.entries.len().saturating_sub(1) * "/".len()
            }
            PanelEntry::Outline(OutlineEntry::Excerpt(excerpt)) => self
                .excerpt_label(&excerpt, cx)
                .map(|label| label.len())
                .unwrap_or_default(),
            PanelEntry::Outline(OutlineEntry::Outline(entry)) => entry.text.len(),
            PanelEntry::Search(search) => search
                .render_data
                .get()
                .map(|data| data.context_text.len())
                .unwrap_or_default(),
        };

        (item_text_chars + depth) as u64
    }

    fn render_main_contents(
        &mut self,
        query: Option<String>,
        show_indent_guides: bool,
        indent_size: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let contents = if self.cached_entries.is_empty() {
            let header = if query.is_some() {
                "No matches for query"
            } else {
                "No outlines available"
            };

            v_flex()
                .id("empty-outline-state")
                .gap_0p5()
                .flex_1()
                .justify_center()
                .size_full()
                .child(h_flex().justify_center().child(Label::new(header)))
                .when_some(query, |panel, query| {
                    panel.child(
                        h_flex()
                            .px_0p5()
                            .justify_center()
                            .bg(cx.theme().colors().element_selected.opacity(0.2))
                            .child(Label::new(query)),
                    )
                })
                .child(
                    h_flex()
                        .gap_1()
                        .justify_center()
                        .child(Label::new("Toggle Panel With").color(Color::Muted))
                        .child({
                            let key_binding = match self.position(window, cx) {
                                DockPosition::Left => {
                                    KeyBinding::for_action(&workspace::ToggleLeftDock, cx)
                                        .into_any_element()
                                }
                                DockPosition::Bottom => {
                                    KeyBinding::for_action(&workspace::ToggleBottomDock, cx)
                                        .into_any_element()
                                }
                                DockPosition::Right => {
                                    KeyBinding::for_action(&workspace::ToggleRightDock, cx)
                                        .into_any_element()
                                }
                            };

                            key_binding
                        }),
                )
        } else {
            let list_contents = {
                let items_len = self.cached_entries.len();
                let multi_buffer_snapshot = match self.mode {
                    ItemsDisplayMode::Search(_) => self
                        .active_editor()
                        .map(|editor| editor.read(cx).buffer().read(cx).snapshot(cx)),
                    ItemsDisplayMode::Outline => None,
                };
                uniform_list(
                    "entries",
                    items_len,
                    cx.processor(move |outline_panel, range: Range<usize>, window, cx| {
                        outline_panel.rendered_entries_len = range.end - range.start;
                        let entries = outline_panel.cached_entries.get(range);
                        entries
                            .map(|entries| entries.to_vec())
                            .unwrap_or_default()
                            .into_iter()
                            .filter_map(|cached_entry| match cached_entry.entry {
                                PanelEntry::Fs(entry) => Some(outline_panel.render_entry(
                                    &entry,
                                    cached_entry.depth,
                                    cached_entry.string_match.as_ref(),
                                    window,
                                    cx,
                                )),
                                PanelEntry::FoldedDirs(folded_dirs_entry) => {
                                    Some(outline_panel.render_folded_dirs(
                                        &folded_dirs_entry,
                                        cached_entry.depth,
                                        cached_entry.string_match.as_ref(),
                                        window,
                                        cx,
                                    ))
                                }
                                PanelEntry::Outline(OutlineEntry::Excerpt(excerpt)) => {
                                    outline_panel.render_excerpt(
                                        &excerpt,
                                        cached_entry.depth,
                                        window,
                                        cx,
                                    )
                                }
                                PanelEntry::Outline(OutlineEntry::Outline(entry)) => {
                                    Some(outline_panel.render_outline(
                                        &entry,
                                        cached_entry.depth,
                                        cached_entry.string_match.as_ref(),
                                        window,
                                        cx,
                                    ))
                                }
                                PanelEntry::Search(SearchEntry {
                                    match_range,
                                    render_data,
                                    kind,
                                    ..
                                }) => outline_panel.render_search_match(
                                    multi_buffer_snapshot.as_ref(),
                                    &match_range,
                                    &render_data,
                                    kind,
                                    cached_entry.depth,
                                    cached_entry.string_match.as_ref(),
                                    window,
                                    cx,
                                ),
                            })
                            .collect()
                    }),
                )
                .with_sizing_behavior(ListSizingBehavior::Infer)
                .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
                .with_width_from_item(self.max_width_item_index)
                .track_scroll(&self.scroll_handle)
                .when(show_indent_guides, |list| {
                    list.with_decoration(
                        ui::indent_guides(px(indent_size), IndentGuideColors::panel(cx))
                            .with_compute_indents_fn(cx.entity(), |outline_panel, range, _, _| {
                                let entries = outline_panel.cached_entries.get(range);
                                if let Some(entries) = entries {
                                    entries.iter().map(|item| item.depth).collect()
                                } else {
                                    smallvec::SmallVec::new()
                                }
                            })
                            .with_render_fn(cx.entity(), move |outline_panel, params, _, _| {
                                const LEFT_OFFSET: Pixels = ui::LIST_ITEM_INDENT_GUIDE_LEFT_OFFSET;

                                let indent_size = params.indent_size;
                                let item_height = params.item_height;
                                let active_indent_guide_ix = find_active_indent_guide_ix(
                                    outline_panel,
                                    &params.indent_guides,
                                );

                                params
                                    .indent_guides
                                    .into_iter()
                                    .enumerate()
                                    .map(|(ix, layout)| {
                                        let bounds = Bounds::new(
                                            point(
                                                layout.offset.x * indent_size + LEFT_OFFSET,
                                                layout.offset.y * item_height,
                                            ),
                                            size(px(1.), layout.length * item_height),
                                        );
                                        ui::RenderedIndentGuide {
                                            bounds,
                                            layout,
                                            is_active: active_indent_guide_ix == Some(ix),
                                            hitbox: None,
                                        }
                                    })
                                    .collect()
                            }),
                    )
                })
            };

            v_flex()
                .flex_shrink_1()
                .size_full()
                .child(list_contents.size_full().flex_shrink_1())
                .custom_scrollbars(
                    Scrollbars::for_settings::<OutlinePanelSettingsScrollbarProxy>()
                        .tracked_scroll_handle(&self.scroll_handle.clone())
                        .with_track_along(
                            ScrollAxes::Horizontal,
                            cx.theme().colors().panel_background,
                        )
                        .tracked_entity(cx.entity_id()),
                    window,
                    cx,
                )
        }
        .children(self.context_menu.as_ref().map(|(menu, position, _)| {
            deferred(
                anchored()
                    .position(*position)
                    .anchor(gpui::Anchor::TopLeft)
                    .child(menu.clone()),
            )
            .with_priority(1)
        }));

        v_flex().w_full().flex_1().overflow_hidden().child(contents)
    }

    fn render_filter_footer(&mut self, pinned: bool, cx: &mut Context<Self>) -> Div {
        let (pin_button_id, icon, icon_tooltip) = if pinned {
            ("unpin_button", IconName::Unpin, "Unpin Outline")
        } else {
            ("pin_button", IconName::Pin, "Pin Active Outline")
        };

        let has_query = self.query(cx).is_some();
        let show_symbols_toggle = self.multi_buffer_active(cx);
        let hide_symbols = self.hide_symbols_active(cx);
        let (hide_symbols_icon, hide_symbols_tooltip) = if hide_symbols {
            (IconName::FileCodeOff, "Show Symbols")
        } else {
            (IconName::FileCode, "Hide Symbols")
        };

        h_flex()
            .p_2()
            .h(Tab::container_height(cx))
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .w_full()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(self.filter_editor.clone()),
            )
            .child(
                h_flex()
                    .when(has_query, |this| {
                        this.child(
                            IconButton::new("clear_filter", IconName::Close)
                                .shape(IconButtonShape::Square)
                                .tooltip(Tooltip::text("Clear Filter"))
                                .on_click(cx.listener(|outline_panel, _, window, cx| {
                                    outline_panel.filter_editor.update(cx, |editor, cx| {
                                        editor.set_text("", window, cx);
                                    });
                                    cx.notify();
                                })),
                        )
                    })
                    .when(show_symbols_toggle, |this| {
                        this.child(
                            IconButton::new("toggle_symbols", hide_symbols_icon)
                                .shape(IconButtonShape::Square)
                                .tooltip(Tooltip::for_action_title_in(
                                    hide_symbols_tooltip,
                                    &ToggleSymbols,
                                    &self.focus_handle,
                                ))
                                .on_click(cx.listener(|outline_panel, _, window, cx| {
                                    outline_panel.toggle_symbols(&ToggleSymbols, window, cx);
                                })),
                        )
                    })
                    .child(
                        IconButton::new(pin_button_id, icon)
                            .tooltip(Tooltip::for_action_title_in(
                                icon_tooltip,
                                &ToggleActiveEditorPin,
                                &self.focus_handle,
                            ))
                            .shape(IconButtonShape::Square)
                            .on_click(cx.listener(|outline_panel, _, window, cx| {
                                outline_panel.toggle_active_editor_pin(
                                    &ToggleActiveEditorPin,
                                    window,
                                    cx,
                                );
                            })),
                    ),
            )
    }

    fn buffers_inside_directory(
        &self,
        worktree_id: WorktreeId,
        directory: &FsEntryPath,
    ) -> HashSet<BufferId> {
        self.fs_entries
            .iter()
            .filter_map(|entry| match entry {
                FsEntry::File(file)
                    if file.worktree_id == worktree_id
                        && file.entry.path != directory.path
                        && file.entry.path.starts_with(&directory.path) =>
                {
                    Some(file.buffer_id)
                }
                _ => None,
            })
            .collect()
    }
}

fn workspace_active_editor(
    workspace: &Workspace,
    cx: &App,
) -> Option<(Box<dyn ItemHandle>, Entity<Editor>)> {
    let active_item = workspace.active_item(cx)?;
    let active_editor = active_item
        .act_as::<Editor>(cx)
        .filter(|editor| editor.read(cx).mode().is_full())?;
    Some((active_item, active_editor))
}

fn file_name(path: &Path) -> String {
    let mut current_path = path;
    loop {
        if let Some(file_name) = current_path.file_name() {
            return file_name.to_string_lossy().into_owned();
        }
        match current_path.parent() {
            Some(parent) => current_path = parent,
            None => return path.to_string_lossy().into_owned(),
        }
    }
}

impl Panel for OutlinePanel {
    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.filter_editor.focus_handle(cx)
    }

    fn persistent_name() -> &'static str {
        "Outline Panel"
    }

    fn panel_key() -> &'static str {
        OUTLINE_PANEL_KEY
    }

    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        match OutlinePanelSettings::get_global(cx).dock {
            DockSide::Left => DockPosition::Left,
            DockSide::Right => DockPosition::Right,
        }
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            let dock = match position {
                DockPosition::Left | DockPosition::Bottom => DockSide::Left,
                DockPosition::Right => DockSide::Right,
            };
            settings.outline_panel.get_or_insert_default().dock = Some(dock);
        });
    }

    fn default_size(&self, _: &Window, cx: &App) -> Pixels {
        OutlinePanelSettings::get_global(cx).default_width
    }

    fn icon(&self, _: &Window, cx: &App) -> Option<IconName> {
        OutlinePanelSettings::get_global(cx)
            .button
            .then_some(IconName::ListTree)
    }

    fn icon_tooltip(&self, _window: &Window, _: &App) -> Option<&'static str> {
        Some("Outline Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn starts_open(&self, _window: &Window, _: &App) -> bool {
        self.active
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |outline_panel, cx| {
            outline_panel
                .update_in(cx, |outline_panel, window, cx| {
                    let old_active = outline_panel.active;
                    outline_panel.active = active;
                    if old_active != active {
                        outline_panel.lsp_outline_refresh_task = Task::ready(());
                        outline_panel.outline_fetch_tasks.clear();
                        if active
                            && let Some((active_item, active_editor)) =
                                outline_panel.workspace.upgrade().and_then(|workspace| {
                                    workspace_active_editor(workspace.read(cx), cx)
                                })
                        {
                            if outline_panel.should_replace_active_item(active_item.as_ref()) {
                                outline_panel.replace_active_editor(
                                    active_item,
                                    active_editor,
                                    window,
                                    cx,
                                );
                            } else {
                                outline_panel.update_fs_entries(active_editor, None, window, cx)
                            }
                            return;
                        }

                        if !outline_panel.pinned {
                            outline_panel.clear_previous(window, cx);
                        }
                    }
                    outline_panel.serialize(cx);
                })
                .ok();
        })
        .detach()
    }

    fn activation_priority(&self) -> u32 {
        6
    }

    fn hide_button_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        Some(workspace::HideStatusItem::new(|settings| {
            settings.outline_panel.get_or_insert_default().button = Some(false);
        }))
    }
}

impl Focusable for OutlinePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<Event> for OutlinePanel {}

impl EventEmitter<PanelEvent> for OutlinePanel {}

impl Render for OutlinePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (is_local, is_via_ssh) = self.project.read_with(cx, |project, _| {
            (project.is_local(), project.is_via_remote_server())
        });
        let query = self.query(cx);
        let pinned = self.pinned;
        let settings = OutlinePanelSettings::get_global(cx);
        let indent_size = settings.indent_size;
        let show_indent_guides = settings.indent_guides.show == ShowIndentGuides::Always;

        let search_query = match &self.mode {
            ItemsDisplayMode::Search(search_query) => Some(search_query),
            _ => None,
        };

        let search_query_text = search_query.map(|sq| sq.query.to_string());

        v_flex()
            .id("outline-panel")
            .size_full()
            .overflow_hidden()
            .relative()
            .key_context(self.dispatch_context(window, cx))
            .on_action(cx.listener(Self::open_selected_entry))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::scroll_up))
            .on_action(cx.listener(Self::scroll_down))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::scroll_cursor_center))
            .on_action(cx.listener(Self::scroll_cursor_top))
            .on_action(cx.listener(Self::scroll_cursor_bottom))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::select_parent))
            .on_action(cx.listener(Self::expand_selected_entry))
            .on_action(cx.listener(Self::collapse_selected_entry))
            .on_action(cx.listener(Self::expand_all_entries))
            .on_action(cx.listener(Self::collapse_all_entries))
            .on_action(cx.listener(Self::copy_path))
            .on_action(cx.listener(Self::copy_relative_path))
            .on_action(cx.listener(Self::toggle_active_editor_pin))
            .on_action(cx.listener(Self::toggle_symbols))
            .on_action(cx.listener(Self::unfold_directory))
            .on_action(cx.listener(Self::fold_directory))
            .on_action(cx.listener(Self::open_excerpts))
            .on_action(cx.listener(Self::open_excerpts_split))
            .when(is_local, |el| {
                el.on_action(cx.listener(Self::reveal_in_finder))
            })
            .when(is_local || is_via_ssh, |el| {
                el.on_action(cx.listener(Self::open_in_terminal))
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |outline_panel, event: &MouseDownEvent, window, cx| {
                    if let Some(entry) = outline_panel.selected_entry().cloned() {
                        outline_panel.deploy_context_menu(event.position, entry, window, cx)
                    } else if let Some(entry) = outline_panel.fs_entries.first().cloned() {
                        outline_panel.deploy_context_menu(
                            event.position,
                            PanelEntry::Fs(entry),
                            window,
                            cx,
                        )
                    }
                }),
            )
            .track_focus(&self.focus_handle)
            .child(self.render_filter_footer(pinned, cx))
            .when_some(search_query_text, |outline_panel, query_text| {
                outline_panel.child(
                    h_flex()
                        .py_1p5()
                        .px_2()
                        .h(Tab::container_height(cx))
                        .gap_0p5()
                        .border_b_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(Label::new("Searching:").color(Color::Muted))
                        .child(Label::new(query_text)),
                )
            })
            .child(self.render_main_contents(query, show_indent_guides, indent_size, window, cx))
    }
}

fn find_active_indent_guide_ix(
    outline_panel: &OutlinePanel,
    candidates: &[IndentGuideLayout],
) -> Option<usize> {
    let SelectedEntry::Valid(_, target_ix) = &outline_panel.selected_entry else {
        return None;
    };
    let target_depth = outline_panel
        .cached_entries
        .get(*target_ix)
        .map(|cached_entry| cached_entry.depth)?;

    let (target_ix, target_depth) = if let Some(target_depth) = outline_panel
        .cached_entries
        .get(target_ix + 1)
        .filter(|cached_entry| cached_entry.depth > target_depth)
        .map(|entry| entry.depth)
    {
        (target_ix + 1, target_depth.saturating_sub(1))
    } else {
        (*target_ix, target_depth.saturating_sub(1))
    };

    candidates
        .iter()
        .enumerate()
        .find(|(_, guide)| {
            guide.offset.y <= target_ix
                && target_ix < guide.offset.y + guide.length
                && guide.offset.x == target_depth
        })
        .map(|(ix, _)| ix)
}

fn subscribe_for_editor_events(
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut Context<OutlinePanel>,
) -> Subscription {
    let debounce = Some(UPDATE_DEBOUNCE);
    cx.subscribe_in(
        editor,
        window,
        move |outline_panel, editor, e: &EditorEvent, window, cx| {
            if !outline_panel.active {
                return;
            }
            match e {
                EditorEvent::SelectionsChanged { local: true } => {
                    outline_panel.reveal_entry_for_selection(editor.clone(), window, cx);
                }
                EditorEvent::BuffersRemoved { removed_buffer_ids } => {
                    for buffer_id in removed_buffer_ids {
                        outline_panel.buffers.remove(buffer_id);
                        outline_panel.outline_fetch_tasks.remove(buffer_id);
                    }
                    outline_panel.update_fs_entries(editor.clone(), debounce, window, cx);
                }
                EditorEvent::BufferRangesUpdated { buffer, .. } => {
                    outline_panel
                        .new_entries_for_fs_update
                        .insert(buffer.read(cx).remote_id());
                    outline_panel.invalidate_outlines(&[buffer.read(cx).remote_id()]);
                    outline_panel.update_fs_entries(editor.clone(), debounce, window, cx);
                }
                EditorEvent::BuffersEdited { buffer_ids } => {
                    outline_panel.invalidate_outlines(buffer_ids);
                    outline_panel.update_contents(debounce, window, cx);
                }
                EditorEvent::BufferFoldToggled { ids, .. } => {
                    outline_panel.invalidate_outlines(ids);
                    let mut latest_unfolded_buffer_id = None;
                    let mut latest_folded_buffer_id = None;
                    let mut ignore_selections_change = false;
                    outline_panel.new_entries_for_fs_update.extend(
                        ids.iter()
                            .filter(|id| {
                                if outline_panel.buffers.contains_key(&id) {
                                    ignore_selections_change |= outline_panel
                                        .preserve_selection_on_buffer_fold_toggles
                                        .remove(&id);
                                    if editor.read(cx).is_buffer_folded(**id, cx) {
                                        latest_folded_buffer_id = Some(**id);
                                        false
                                    } else {
                                        latest_unfolded_buffer_id = Some(**id);
                                        true
                                    }
                                } else {
                                    false
                                }
                            })
                            .copied(),
                    );
                    if !ignore_selections_change
                        && let Some(entry_to_select) = latest_unfolded_buffer_id
                            .or(latest_folded_buffer_id)
                            .and_then(|toggled_buffer_id| {
                                outline_panel.fs_entries.iter().find_map(
                                    |fs_entry| match fs_entry {
                                        FsEntry::ExternalFile(external) => {
                                            if external.buffer_id == toggled_buffer_id {
                                                Some(fs_entry.clone())
                                            } else {
                                                None
                                            }
                                        }
                                        FsEntry::File(FsEntryFile { buffer_id, .. }) => {
                                            if *buffer_id == toggled_buffer_id {
                                                Some(fs_entry.clone())
                                            } else {
                                                None
                                            }
                                        }
                                        FsEntry::Directory(..) => None,
                                    },
                                )
                            })
                            .map(PanelEntry::Fs)
                    {
                        outline_panel.select_entry(entry_to_select, true, window, cx);
                    }

                    outline_panel.update_fs_entries(editor.clone(), debounce, window, cx);
                }
                EditorEvent::Reparsed(buffer_id) => {
                    outline_panel.invalidate_outlines(&[*buffer_id]);
                    outline_panel.update_contents(debounce, window, cx);
                }
                EditorEvent::OutlineSymbolsChanged => {
                    if outline_panel.lsp_outline_refresh_task.is_ready() {
                        outline_panel.lsp_outline_refresh_task =
                            cx.spawn_in(window, async move |outline_panel, cx| {
                                cx.background_executor().timer(UPDATE_DEBOUNCE).await;
                                outline_panel
                                    .update_in(cx, |outline_panel, window, cx| {
                                        let Some(editor) = outline_panel.active_editor() else {
                                            return;
                                        };
                                        let editor_buffer = editor.read(cx).buffer().read(cx);
                                        let mut invalidated = false;
                                        for (buffer_id, outlines) in &mut outline_panel.buffers {
                                            if editor_buffer.buffer(*buffer_id).is_some_and(
                                                |buffer| {
                                                    LanguageSettings::for_buffer(
                                                        buffer.read(cx),
                                                        cx,
                                                    )
                                                    .document_symbols
                                                    .lsp_enabled()
                                                },
                                            ) {
                                                outlines.invalidate_outlines();
                                                invalidated = true;
                                            }
                                        }
                                        if invalidated {
                                            outline_panel.fetch_outdated_outlines(None, window, cx);
                                        }
                                    })
                                    .ok();
                            });
                    }
                }
                EditorEvent::TitleChanged => {
                    outline_panel.update_fs_entries(editor.clone(), debounce, window, cx);
                }
                _ => {}
            }
        },
    )
}

fn empty_icon() -> AnyElement {
    h_flex()
        .size(IconSize::default().rems())
        .invisible()
        .flex_none()
        .into_any_element()
}

#[derive(Debug, Default)]
struct GenerationState {
    entries: Vec<CachedEntry>,
    match_candidates: Vec<StringMatchCandidate>,
    max_width_estimate_and_index: Option<(u64, usize)>,
}

impl GenerationState {
    fn clear(&mut self) {
        self.entries.clear();
        self.match_candidates.clear();
        self.max_width_estimate_and_index = None;
    }
}

