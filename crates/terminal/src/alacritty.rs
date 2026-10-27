use std::os::fd::AsRawFd;
use std::{borrow::Cow, io, ops::RangeInclusive, path::PathBuf, sync::Arc};

mod hyperlinks;

use alacritty_terminal::{
  event::{Event as AlacTermEvent, EventListener, Notify, WindowSize},
  event_loop::{EventLoop, Msg, Notifier},
  grid::{Dimensions, Grid, GridIterator, Row, Scroll as AlacScroll},
  index::{Boundary, Column, Direction as AlacDirection, Line, Point as AlacPoint},
  selection::{
    Selection as AlacSelection, SelectionRange as AlacSelectionRange,
    SelectionType as AlacSelectionType,
  },
  sync::FairMutex,
  term::{
    Config, Osc52, RenderableCursor, SEMANTIC_ESCAPE_CHARS, Term, TermMode,
    cell::{Cell as AlacCell, Flags, Hyperlink as AlacHyperlink},
    search::{Match, RegexIter, RegexSearch},
  },
  tty,
  vte::ansi::{
    ClearMode, CursorShape as AlacCursorShape, CursorStyle as AlacCursorStyle, NamedPrivateMode,
    PrivateMode,
  },
};
use anyhow::{Context as _, Result};
use futures::channel::mpsc::UnboundedSender;
use util::paths::PathStyle;
use vte::ansi::Handler;

use crate::{
  Cell, Color, Content, Cursor, CursorShape, GridLinesChange, HoveredWord, Hyperlink,
  HyperlinkData, IndexedCell, Modes, Point, PtyEvent, Range, RenderableCells, Scroll, Search,
  Selection, SelectionRange, SelectionSide, SelectionType, TerminalBackendEvent, TerminalBounds,
  pty_info::ProcessIdGetter,
  terminal_settings::{AlternateScroll, CursorShape as SettingsCursorShape},
};

pub(super) use hyperlinks::{HyperlinkMatch, RegexSearches};

pub(super) type AlacrittyPty = tty::Pty;
pub(super) type AlacrittyTerm = Term<ZedListener>;
pub(super) type AlacrittyTermConfig = Config;
pub(super) type AlacrittyTermLock = FairMutex<AlacrittyTerm>;
pub(super) type AlacrittyCell = AlacCell;
pub(super) type AlacrittyGridIterator<'a> = GridIterator<'a, AlacCell>;
pub(super) type AlacrittyHyperlink = AlacHyperlink;

#[derive(Clone)]
pub(super) struct ZedListener {
  sender: UnboundedSender<PtyEvent>,
  generation: u64,
}

#[derive(Clone, Debug)]
pub(super) struct AlacrittySearch {
  search: RegexSearch,
}

impl From<&AlacrittyPty> for ProcessIdGetter {
  fn from(pty: &AlacrittyPty) -> Self {
    Self::new(pty.file().as_raw_fd(), pty.child().id())
  }
}

pub(super) struct PtySender {
  notifier: Notifier,
}

impl PtySender {
  pub(super) fn notify(&self, input: impl Into<Cow<'static, [u8]>>) {
    self.notifier.notify(input);
  }

  pub(super) fn resize(&self, bounds: TerminalBounds) {
    if let Err(error) = self
      .notifier
      .0
      .send(Msg::Resize(window_size_from_terminal_bounds(bounds)))
    {
      log::error!("failed to resize alacritty pty: {error}");
    }
  }

  pub(super) fn shutdown(&self) {
    if let Err(error) = self.notifier.0.send(Msg::Shutdown) {
      log::debug!("failed to shut down alacritty pty loop: {error}");
    }
  }
}

fn window_size_from_terminal_bounds(bounds: TerminalBounds) -> WindowSize {
  WindowSize {
    num_lines: bounds.num_lines() as u16,
    num_cols: bounds.num_columns() as u16,
    cell_width: f32::from(bounds.cell_width()) as u16,
    cell_height: f32::from(bounds.line_height()) as u16,
  }
}

pub(super) fn display_only_term_config(
  scrolling_history: usize,
  cursor_shape: SettingsCursorShape,
) -> AlacrittyTermConfig {
  Config {
    scrolling_history,
    default_cursor_style: alacritty_cursor_style(cursor_shape),
    semantic_escape_chars: format!("{SEMANTIC_ESCAPE_CHARS}─"),
    osc52: Osc52::Disabled,
    ..Config::default()
  }
}

pub(super) fn pty_term_config(
  scrolling_history: usize,
  cursor_shape: SettingsCursorShape,
) -> AlacrittyTermConfig {
  Config {
    scrolling_history,
    default_cursor_style: alacritty_cursor_style(cursor_shape),
    semantic_escape_chars: format!("{SEMANTIC_ESCAPE_CHARS}─"),
    ..Config::default()
  }
}

pub(super) fn set_default_cursor_style(
  config: &mut AlacrittyTermConfig,
  cursor_shape: SettingsCursorShape,
) {
  config.default_cursor_style = alacritty_cursor_style(cursor_shape);
}

pub(super) fn apply_config(term: &AlacrittyTermLock, config: &AlacrittyTermConfig) {
  term.lock().set_options(config.clone());
}


pub(super) fn current_child_signal_mask() -> io::Result<tty::SignalMask> {
  tty::SignalMask::current()
}

pub(super) fn pty_options(
  shell: Option<(String, Vec<String>)>,
  working_directory: Option<PathBuf>,
  env: impl IntoIterator<Item = (String, String)>,
  child_signal_mask: Option<tty::SignalMask>,
) -> tty::Options {
  tty::Options {
    shell: shell.map(|(program, args)| tty::Shell::new(program, args)),
    working_directory,
    drain_on_exit: true,
    env: env.into_iter().collect(),
    child_signal_mask,
  }
}

pub(super) fn open_pty(
  options: &tty::Options,
  bounds: TerminalBounds,
  window_id: u64,
) -> io::Result<AlacrittyPty> {
  tty::new(options, window_size_from_terminal_bounds(bounds), window_id)
}

pub(super) fn new_term(
  config: &AlacrittyTermConfig,
  bounds: TerminalBounds,
  events_tx: UnboundedSender<PtyEvent>,
  alternate_scroll: AlternateScroll,
  generation: u64,
) -> Arc<AlacrittyTermLock> {
  let mut term = Term::new(
    config.clone(),
    &bounds,
    ZedListener {
      sender: events_tx,
      generation,
    },
  );

  if let AlternateScroll::Off = alternate_scroll {
    term.unset_private_mode(PrivateMode::Named(NamedPrivateMode::AlternateScroll));
  }

  Arc::new(FairMutex::new(term))
}

pub(super) fn spawn_event_loop(
  term: Arc<AlacrittyTermLock>,
  events_tx: UnboundedSender<PtyEvent>,
  pty: AlacrittyPty,
  drain_on_exit: bool,
) -> Result<PtySender> {
  let event_loop = EventLoop::new(
    term,
    ZedListener {
      sender: events_tx,
      generation: 0,
    },
    pty,
    drain_on_exit,
    false,
  )
  .context("failed to create event loop")?;
  let pty_tx = event_loop.channel();
  let _io_thread = event_loop.spawn();

  Ok(PtySender {
    notifier: Notifier(pty_tx),
  })
}

pub(super) fn resize(term: &mut AlacrittyTerm, bounds: TerminalBounds) {
  term.resize(bounds);
}

pub(super) fn display_offset(term: &AlacrittyTerm) -> usize {
  term.grid().display_offset()
}

pub(super) fn scroll_display(term: &mut AlacrittyTerm, scroll: Scroll) {
  term.scroll_display(scroll.to_alacritty());
}

pub(super) fn set_selection(term: &mut AlacrittyTerm, selection: Option<&Selection>) {
  term.selection = selection.map(Selection::to_alacritty);
}

pub(super) fn update_selection(
  term: &mut AlacrittyTerm,
  point: Point,
  side: SelectionSide,
) -> bool {
  let Some(mut selection) = term.selection.take() else {
    return false;
  };
  selection.update(point.to_alacritty(), side.to_alacritty());
  term.selection = Some(selection);
  true
}

pub(super) fn selection_text(term: &AlacrittyTerm) -> Option<String> {
  term.selection_to_string()
}

pub(super) fn scroll_to_point(term: &mut AlacrittyTerm, point: Point) {
  term.scroll_to_point(point.to_alacritty());
}

fn alacritty_cursor_style(cursor_shape: SettingsCursorShape) -> AlacCursorStyle {
  AlacCursorStyle {
    shape: alacritty_cursor_shape(cursor_shape),
    blinking: false,
  }
}

fn alacritty_cursor_shape(cursor_shape: SettingsCursorShape) -> AlacCursorShape {
  match cursor_shape {
    SettingsCursorShape::Block => AlacCursorShape::Block,
    SettingsCursorShape::Underline => AlacCursorShape::Underline,
    SettingsCursorShape::Bar => AlacCursorShape::Beam,
    SettingsCursorShape::Hollow => AlacCursorShape::HollowBlock,
  }
}

impl Dimensions for TerminalBounds {
  /// Note: this is supposed to be for the back buffer's length,
  /// but we exclusively use it to resize the terminal, which does not
  /// use this method. We still have to implement it for the trait though,
  /// hence, this comment.
  fn total_lines(&self) -> usize {
    self.screen_lines()
  }

  fn screen_lines(&self) -> usize {
    self.num_lines()
  }

  fn columns(&self) -> usize {
    self.num_columns()
  }
}

impl From<AlacTermEvent> for TerminalBackendEvent {
  fn from(event: AlacTermEvent) -> Self {
    match event {
      AlacTermEvent::MouseCursorDirty => Self::MouseCursorDirty,
      AlacTermEvent::Title(title) => Self::Title(title),
      AlacTermEvent::ResetTitle => Self::ResetTitle,
      AlacTermEvent::ClipboardStore(_, data) => Self::ClipboardStore(data),
      AlacTermEvent::ClipboardLoad(_, format) => Self::ClipboardLoad(format),
      AlacTermEvent::ColorRequest(index, format) => Self::ColorRequest(index, format),
      AlacTermEvent::PtyWrite(output) => Self::PtyWrite(output),
      AlacTermEvent::TextAreaSizeRequest(format) => {
        Self::TextAreaSizeRequest(Arc::new(move |bounds| {
          format(window_size_from_terminal_bounds(bounds))
        }))
      }
      AlacTermEvent::CursorBlinkingChange => Self::CursorBlinkingChange,
      AlacTermEvent::Wakeup => Self::Wakeup,
      AlacTermEvent::Bell => Self::Bell,
      AlacTermEvent::Exit => Self::Exit,
      AlacTermEvent::ChildExit(status) => Self::ChildExit(status),
    }
  }
}

impl EventListener for ZedListener {
  fn send_event(&self, event: AlacTermEvent) {
    self
      .sender
      .unbounded_send(PtyEvent::Event(event.into(), self.generation))
      .ok();
  }
}

impl Scroll {
  fn to_alacritty(self) -> AlacScroll {
    match self {
      Self::Delta(delta) => AlacScroll::Delta(delta),
      Self::PageUp => AlacScroll::PageUp,
      Self::PageDown => AlacScroll::PageDown,
      Self::Top => AlacScroll::Top,
      Self::Bottom => AlacScroll::Bottom,
    }
  }
}

impl Search {
  pub fn new(search: &str) -> Option<Self> {
    Some(Self {
      search: AlacrittySearch {
        search: RegexSearch::new(search).ok()?,
      },
    })
  }

  fn into_alacritty(self) -> RegexSearch {
    self.search.search
  }
}

impl SelectionSide {
  fn to_alacritty(self) -> AlacDirection {
    match self {
      Self::Left => AlacDirection::Left,
      Self::Right => AlacDirection::Right,
    }
  }
}

impl SelectionType {
  fn to_alacritty(self) -> AlacSelectionType {
    match self {
      Self::Simple => AlacSelectionType::Simple,
      Self::Semantic => AlacSelectionType::Semantic,
      Self::Lines => AlacSelectionType::Lines,
    }
  }
}

impl Selection {
  fn to_alacritty(&self) -> AlacSelection {
    let mut selection = AlacSelection::new(
      self.ty.to_alacritty(),
      self.start.point.to_alacritty(),
      self.start.side.to_alacritty(),
    );
    if self.start.point != self.end.point || self.start.side != self.end.side {
      selection.update(self.end.point.to_alacritty(), self.end.side.to_alacritty());
    }
    selection
  }
}

impl Hyperlink {
  pub fn new<T: ToString>(id: Option<T>, uri: String) -> Self {
    Self {
      data: HyperlinkData::Owned {
        id: id.map(|id| Arc::from(id.to_string())),
        uri: Arc::from(uri),
      },
    }
  }

  pub fn id(&self) -> Option<&str> {
    match &self.data {
      HyperlinkData::Alacritty(hyperlink) => Some(hyperlink.id()),
      HyperlinkData::Owned { id, .. } => id.as_deref(),
    }
  }

  pub fn uri(&self) -> &str {
    match &self.data {
      HyperlinkData::Alacritty(hyperlink) => hyperlink.uri(),
      HyperlinkData::Owned { uri, .. } => uri,
    }
  }

  fn from_alacritty(hyperlink: AlacHyperlink) -> Self {
    Self {
      data: HyperlinkData::Alacritty(hyperlink),
    }
  }
}

fn terminal_hyperlink_from_alacritty(hyperlink: AlacHyperlink) -> Hyperlink {
  Hyperlink::from_alacritty(hyperlink)
}

impl From<Hyperlink> for AlacHyperlink {
  fn from(hyperlink: Hyperlink) -> Self {
    match hyperlink.data {
      HyperlinkData::Alacritty(hyperlink) => hyperlink,
      HyperlinkData::Owned { id, uri } => Self::new(id.as_deref(), uri.to_string()),
    }
  }
}

fn terminal_cell_from_alacritty(cell: &AlacCell) -> Cell {
  Cell { cell: cell.clone() }
}

impl Cell {
  #[inline]
  pub fn character(&self) -> char {
    self.cell.c
  }

  #[inline]
  pub fn foreground(&self) -> Color {
    self.cell.fg
  }

  #[inline]
  pub fn background(&self) -> Color {
    self.cell.bg
  }

  #[inline]
  pub fn zerowidth(&self) -> Option<&[char]> {
    self.cell.zerowidth()
  }

  #[inline]
  pub fn hyperlink(&self) -> Option<Hyperlink> {
    self.cell.hyperlink().map(terminal_hyperlink_from_alacritty)
  }

  #[inline]
  pub fn is_inverse(&self) -> bool {
    self.cell.flags.contains(Flags::INVERSE)
  }

  #[inline]
  pub fn is_wide_char_spacer(&self) -> bool {
    self.cell.flags.contains(Flags::WIDE_CHAR_SPACER)
  }

  #[inline]
  pub fn is_dim(&self) -> bool {
    self.cell.flags.intersects(Flags::DIM)
  }

  #[inline]
  pub fn has_underline(&self) -> bool {
    self.cell.flags.intersects(Flags::ALL_UNDERLINES)
  }

  #[inline]
  pub fn has_undercurl(&self) -> bool {
    self.cell.flags.contains(Flags::UNDERCURL)
  }

  #[inline]
  pub fn has_strikeout(&self) -> bool {
    self.cell.flags.intersects(Flags::STRIKEOUT)
  }

  #[inline]
  pub fn is_bold(&self) -> bool {
    self.cell.flags.intersects(Flags::BOLD)
  }

  #[inline]
  pub fn is_italic(&self) -> bool {
    self.cell.flags.intersects(Flags::ITALIC)
  }

  #[inline]
  pub fn has_visible_style_modifier(&self) -> bool {
    self
      .cell
      .flags
      .intersects(Flags::ALL_UNDERLINES | Flags::INVERSE | Flags::STRIKEOUT)
  }
}

impl<'a> RenderableCells<'a> {
  pub(super) fn new(cells: GridIterator<'a, AlacCell>) -> Self {
    Self { cells }
  }
}

impl Iterator for RenderableCells<'_> {
  type Item = IndexedCell;

  fn next(&mut self) -> Option<Self::Item> {
    self.cells.next().map(|cell| IndexedCell {
      point: terminal_point_from_alacritty(cell.point),
      cell: terminal_cell_from_alacritty(cell.cell),
    })
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    self.cells.size_hint()
  }
}

fn terminal_modes_from_alacritty(mode: TermMode) -> Modes {
  let mut terminal_modes = Modes::empty();
  add_terminal_mode(
    &mut terminal_modes,
    mode,
    TermMode::APP_CURSOR,
    Modes::APP_CURSOR,
  );
  add_terminal_mode(
    &mut terminal_modes,
    mode,
    TermMode::APP_KEYPAD,
    Modes::APP_KEYPAD,
  );
  add_terminal_mode(
    &mut terminal_modes,
    mode,
    TermMode::SHOW_CURSOR,
    Modes::SHOW_CURSOR,
  );
  add_terminal_mode(
    &mut terminal_modes,
    mode,
    TermMode::LINE_WRAP,
    Modes::LINE_WRAP,
  );
  add_terminal_mode(&mut terminal_modes, mode, TermMode::ORIGIN, Modes::ORIGIN);
  add_terminal_mode(&mut terminal_modes, mode, TermMode::INSERT, Modes::INSERT);
  add_terminal_mode(
    &mut terminal_modes,
    mode,
    TermMode::LINE_FEED_NEW_LINE,
    Modes::LINE_FEED_NEW_LINE,
  );
  add_terminal_mode(
    &mut terminal_modes,
    mode,
    TermMode::FOCUS_IN_OUT,
    Modes::FOCUS_IN_OUT,
  );
  add_terminal_mode(
    &mut terminal_modes,
    mode,
    TermMode::ALTERNATE_SCROLL,
    Modes::ALTERNATE_SCROLL,
  );
  add_terminal_mode(
    &mut terminal_modes,
    mode,
    TermMode::BRACKETED_PASTE,
    Modes::BRACKETED_PASTE,
  );
  add_terminal_mode(
    &mut terminal_modes,
    mode,
    TermMode::SGR_MOUSE,
    Modes::SGR_MOUSE,
  );
  add_terminal_mode(
    &mut terminal_modes,
    mode,
    TermMode::UTF8_MOUSE,
    Modes::UTF8_MOUSE,
  );
  add_terminal_mode(
    &mut terminal_modes,
    mode,
    TermMode::ALT_SCREEN,
    Modes::ALT_SCREEN,
  );
  add_terminal_mode(
    &mut terminal_modes,
    mode,
    TermMode::MOUSE_REPORT_CLICK,
    Modes::MOUSE_REPORT_CLICK,
  );
  add_terminal_mode(
    &mut terminal_modes,
    mode,
    TermMode::MOUSE_DRAG,
    Modes::MOUSE_DRAG,
  );
  add_terminal_mode(
    &mut terminal_modes,
    mode,
    TermMode::MOUSE_MOTION,
    Modes::MOUSE_MOTION,
  );
  terminal_modes
}

fn add_terminal_mode(
  terminal_modes: &mut Modes,
  alacritty_modes: TermMode,
  alacritty_mode: TermMode,
  terminal_mode: Modes,
) {
  if alacritty_modes.contains(alacritty_mode) {
    terminal_modes.insert(terminal_mode);
  }
}

impl Cursor {
  fn from_alacritty(cursor: RenderableCursor) -> Self {
    Self {
      shape: terminal_cursor_shape_from_alacritty(cursor.shape),
      point: terminal_point_from_alacritty(cursor.point),
    }
  }
}

fn terminal_cursor_shape_from_alacritty(shape: AlacCursorShape) -> CursorShape {
  match shape {
    AlacCursorShape::Block => CursorShape::Block,
    AlacCursorShape::Underline => CursorShape::Underline,
    AlacCursorShape::Beam => CursorShape::Bar,
    AlacCursorShape::HollowBlock => CursorShape::HollowBlock,
    AlacCursorShape::Hidden => CursorShape::Hidden,
  }
}

impl Point {
  fn to_alacritty(self) -> AlacPoint {
    AlacPoint::new(Line(self.line), Column(self.column))
  }
}

fn terminal_point_from_alacritty(point: AlacPoint) -> Point {
  Point {
    line: point.line.0,
    column: point.column.0,
  }
}

impl Range {
  fn from_alacritty(range: RangeInclusive<AlacPoint>) -> Self {
    Self {
      start: terminal_point_from_alacritty(*range.start()),
      end: terminal_point_from_alacritty(*range.end()),
    }
  }
}

fn terminal_selection_range_from_alacritty(range: AlacSelectionRange) -> SelectionRange {
  SelectionRange {
    start: terminal_point_from_alacritty(range.start),
    end: terminal_point_from_alacritty(range.end),
    is_block: range.is_block,
  }
}

pub(super) fn clear_saved_screen(term: &mut Term<ZedListener>) {
  term.clear_screen(ClearMode::Saved);

  let cursor = term.grid().cursor.point;

  term.grid_mut().reset_region(..cursor.line);

  let line = term.grid()[cursor.line][..Column(term.grid().columns())]
    .iter()
    .cloned()
    .enumerate()
    .collect::<Vec<(usize, AlacCell)>>();

  for (index, cell) in line {
    term.grid_mut()[Line(0)][Column(index)] = cell;
  }

  term.grid_mut().cursor.point = AlacPoint::new(Line(0), term.grid_mut().cursor.point.column);
  let new_cursor = term.grid().cursor.point;

  if (new_cursor.line.0 as usize) < term.screen_lines() - 1 {
    term.grid_mut().reset_region((new_cursor.line + 1)..);
  }
}

pub(super) fn shrink_to_used(term: &mut Term<ZedListener>) {
  term.grid_mut().truncate();
}

pub(super) fn used_lines(term: &Term<ZedListener>) -> usize {
  if term.mode().contains(TermMode::ALT_SCREEN) {
    return term.total_lines();
  }
  let grid = term.grid();
  let cursor_line = grid.cursor.point.line.0.max(0) as usize;
  let last_occupied_line = (cursor_line + 1..term.screen_lines())
    .rev()
    .find(|&line| !grid[Line(line as i32)].is_clear())
    .unwrap_or(cursor_line);
  term.history_size() + last_occupied_line + 1
}

fn adjusted_last_hovered_word(
  grid: &Grid<AlacCell>,
  last_content: &Content,
) -> (Option<HoveredWord>, GridLinesChange) {
  let grid_size_changed =
    grid.columns() != last_content.columns || grid.screen_lines() != last_content.screen_lines;

  let Some(total_lines_delta) = grid
    .total_lines()
    .checked_signed_diff(last_content.total_lines)
  else {
    return (None, GridLinesChange::Unchanged);
  };

  let Some(display_offset_delta) = grid
    .display_offset()
    .checked_signed_diff(last_content.display_offset)
  else {
    return (None, GridLinesChange::Unchanged);
  };

  let grid_visible_lines_changed = total_lines_delta != display_offset_delta;

  let grid_lines_change = if grid_size_changed || grid_visible_lines_changed {
    GridLinesChange::Changed
  } else {
    GridLinesChange::Unchanged
  };

  let hovered_word = if let Some(last_hovered_word) = last_content.last_hovered_word.as_ref()
    && grid_lines_change == GridLinesChange::Unchanged
  {
    if total_lines_delta == 0 {
      // Viewport and content are the same, we can reuse existing match
      Some(last_hovered_word.clone())
    } else {
      // Content is changed, but viewport is shifted the same, reuse
      // the match, but need to shift the match lines by the same amount.
      let mut adjusted_hovered_word = last_hovered_word.clone();
      adjusted_hovered_word.word_match = Range::new(
        Point::new(
          last_hovered_word.word_match.start().line - display_offset_delta as i32,
          last_hovered_word.word_match.start().column,
        ),
        Point::new(
          last_hovered_word.word_match.end().line - display_offset_delta as i32,
          last_hovered_word.word_match.end().column,
        ),
      );

      Some(adjusted_hovered_word)
    }
  } else {
    None
  };

  (hovered_word, grid_lines_change)
}

pub(super) fn make_content(term: &Term<ZedListener>, last_content: &Content) -> Content {
  let content = term.renderable_content();

  let estimated_size = content.display_iter.size_hint().0;
  let mut cells = Vec::with_capacity(estimated_size);

  cells.extend(content.display_iter.map(|ic| IndexedCell {
    point: terminal_point_from_alacritty(ic.point),
    cell: terminal_cell_from_alacritty(ic.cell),
  }));

  let selection_text = if content.selection.is_some() {
    term.selection_to_string()
  } else {
    None
  };

  let grid = term.grid();
  let (last_hovered_word, grid_lines_change) = adjusted_last_hovered_word(grid, last_content);

  let bottom_line = term.screen_lines() as i32 - 1 - content.display_offset as i32;
  let bottom_row_occupied = content.cursor.point.line.0 >= bottom_line
    || cells
      .iter()
      .rev()
      .take_while(|cell| cell.point.line >= bottom_line)
      .any(|cell| cell.cell.character() != ' ');

  Content {
    cells,
    mode: terminal_modes_from_alacritty(content.mode),
    total_lines: grid.total_lines(),
    display_offset: grid.display_offset(),
    columns: grid.columns(),
    screen_lines: grid.screen_lines(),
    selection_text,
    selection: content
      .selection
      .map(terminal_selection_range_from_alacritty),
    cursor: Cursor::from_alacritty(content.cursor),
    cursor_char: term.grid()[content.cursor.point].c,
    terminal_bounds: last_content.terminal_bounds,
    last_hovered_word,
    grid_lines_change,
    scrolled_to_top: content.display_offset == term.history_size(),
    scrolled_to_bottom: content.display_offset == 0,
    bottom_row_occupied,
  }
}

pub(super) fn content_text(term: &Term<ZedListener>) -> String {
  let start = AlacPoint::new(term.topmost_line(), Column(0));
  let end = AlacPoint::new(term.bottommost_line(), term.last_column());
  term.bounds_to_string(start, end)
}

pub(super) fn total_lines(term: &Term<ZedListener>) -> usize {
  term.total_lines()
}

pub(super) fn screen_lines(term: &Term<ZedListener>) -> usize {
  term.screen_lines()
}

pub(super) fn full_content_range(term: &Term<ZedListener>) -> Range {
  let start = AlacPoint::new(term.topmost_line(), Column(0));
  let end = AlacPoint::new(term.bottommost_line(), term.last_column());
  Range::from_alacritty(start..=end)
}

pub(super) fn last_non_empty_lines(term: &Term<ZedListener>, line_count: usize) -> Vec<String> {
  let grid = term.grid();
  let mut lines = Vec::new();

  let mut current_line = grid.bottommost_line().0;
  let topmost_line = grid.topmost_line().0;

  while current_line >= topmost_line && lines.len() < line_count {
    let (logical_line_start, logical_line) = logical_line_for_row(grid, current_line, topmost_line);

    if let Some(line) = process_line(logical_line) {
      lines.push(line);
    }

    current_line = logical_line_start - 1;
  }

  lines.reverse();
  lines
}

pub(super) fn find_from_terminal_point(
  term: &AlacrittyTerm,
  point: Point,
  regex_searches: &mut RegexSearches,
  path_style: PathStyle,
) -> Option<HyperlinkMatch> {
  let point = point.to_alacritty().grid_clamp(term, Boundary::Grid);
  hyperlinks::find_from_grid_point(term, point, regex_searches, path_style)
}

fn logical_line_for_row(grid: &Grid<AlacCell>, current: i32, topmost: i32) -> (i32, String) {
  let start = find_logical_line_start(grid, current, topmost);
  let mut line = String::new();
  for row in start..=current {
    line.push_str(&row_to_string(&grid[Line(row)]));
  }
  (start, line)
}

fn find_logical_line_start(grid: &Grid<AlacCell>, current: i32, topmost: i32) -> i32 {
  let mut line_start = current;
  while line_start > topmost {
    let previous_line = Line(line_start - 1);
    let last_cell = &grid[previous_line][Column(grid.columns() - 1)];
    if !last_cell.flags.contains(Flags::WRAPLINE) {
      break;
    }
    line_start -= 1;
  }
  line_start
}

fn row_to_string(row: &Row<AlacCell>) -> String {
  row[..Column(row.len())]
    .iter()
    .map(|cell| cell.c)
    .collect::<String>()
}

fn process_line(line: String) -> Option<String> {
  let trimmed = line.trim_end().to_string();
  if !trimmed.is_empty() {
    Some(trimmed)
  } else {
    None
  }
}

/// Appends a stringified task summary to the terminal, after its output.
///
/// SAFETY: This function should only be called after terminal's PTY is no longer alive.
/// New text being added to the terminal here, uses "less public" APIs,
/// which are not maintaining the entire terminal state intact.
///
///
/// The library
///
/// * does not increment inner grid cursor's _lines_ on `input` calls
///   (but displaying the lines correctly and incrementing cursor's columns)
///
/// * ignores `\n` and \r` character input, requiring the `newline` call instead
///
/// * does not alter grid state after `newline` call
///   so its `bottommost_line` is always the same additions, and
///   the cursor's `point` is not updated to the new line and column values
///
/// * ??? there could be more consequences, and any further "proper" streaming from the PTY might bug and/or panic.
///   Still, subsequent `append_text_to_term` invocations are possible and display the contents correctly.
///
/// Despite the quirks, this is the simplest approach to appending text to the terminal: its alternative, `grid_mut` manipulations,
/// do not properly set the scrolling state and display odd text after appending; also those manipulations are more tedious and error-prone.
/// The function achieves proper display and scrolling capabilities, at a cost of grid state not properly synchronized.
/// This is enough for printing moderately-sized texts like task summaries, but might break or perform poorly for larger texts.
pub(super) unsafe fn append_text_to_term(term: &mut Term<ZedListener>, text_lines: &[&str]) {
  term.newline();
  term.grid_mut().cursor.point.column = Column(0);
  for line in text_lines {
    for character in line.chars() {
      term.input(character);
    }
    term.newline();
    term.grid_mut().cursor.point.column = Column(0);
  }
}

pub(super) fn search_matches(term: &AlacrittyTerm, searcher: Search) -> Vec<Range> {
  let mut searcher = searcher.into_alacritty();
  all_search_matches(term, &mut searcher)
    .map(Range::from_alacritty)
    .collect()
}

fn all_search_matches<'a, T>(
  term: &'a Term<T>,
  regex: &'a mut RegexSearch,
) -> impl Iterator<Item = Match> + 'a {
  let start = AlacPoint::new(term.grid().topmost_line(), Column(0));
  let end = AlacPoint::new(term.grid().bottommost_line(), term.grid().last_column());
  RegexIter::new(start, end, AlacDirection::Right, term, regex)
}
