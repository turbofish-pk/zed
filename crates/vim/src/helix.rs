mod boundary;
mod duplicate;
mod object;
mod paste;
mod select;
mod surround;

use editor::display_map::{DisplayRow, DisplaySnapshot};
use editor::{
    DisplayPoint, Editor, EditorSettings, MultiBufferOffset, NavigationOverlayLabel,
    NavigationTargetOverlay, SelectionEffects, ToOffset, ToPoint, movement,
};
use gpui::actions;
use gpui::{App, Context, Font, Hsla, Pixels, TaskExt, Window, WindowTextSystem};
use language::{CharClassifier, CharKind, Point, Selection};
use multi_buffer::MultiBufferSnapshot;
use search::{BufferSearchBar, SearchOptions};
use settings::Settings;
use text::{Bias, LineEnding, SelectionGoal};
use theme::ActiveTheme as _;
use ui::px;
use workspace::searchable::{self, Direction, FilteredSearchRange};

use crate::motion::{self, MotionKind};
use crate::state::{HelixJumpBehaviour, HelixJumpLabel, Mode, Operator, SearchState};
use crate::{
    HelixAppendState, PushHelixSurroundAdd, PushHelixSurroundDelete, PushHelixSurroundReplace, Vim,
    motion::{Motion, right},
};
use std::ops::Range;

actions!(
    vim,
    [
        /// Yanks the current selection or character if no selection.
        HelixYank,
        /// Inserts at the beginning of the selection.
        HelixInsert,
        /// Appends at the end of the selection.
        HelixAppend,
        /// Inserts at the end of the current Helix cursor line.
        HelixInsertEndOfLine,
        /// Goes to the location of the last modification.
        HelixGotoLastModification,
        /// Goes to the line specified by the count.
        HelixGotoLine,
        /// Select entire line or multiple lines, extending downwards.
        HelixSelectLine,
        /// Select all matches of a given pattern within the current selection.
        HelixSelectRegex,
        /// Removes all but the one selection that was created last.
        /// `Newest` can eventually be `Primary`.
        HelixKeepNewestSelection,
        /// Copies all selections below.
        HelixDuplicateBelow,
        /// Copies all selections above.
        HelixDuplicateAbove,
        /// Delete the selection and enter edit mode.
        HelixSubstitute,
        /// Delete the selection and enter edit mode, without yanking the selection.
        HelixSubstituteNoYank,
        /// Activate Helix-style word jump labels.
        HelixJumpToWord,
        /// Select the next match for the current search query.
        HelixSelectNext,
        /// Select the previous match for the current search query.
        HelixSelectPrevious,
        /// Trim leading and trailing whitespace from each selection.
        /// Originally-empty selections (cursors) are dropped before trimming.
        HelixTrimSelections,
    ]
);

pub fn register(editor: &mut Editor, cx: &mut Context<Vim>) {
    Vim::action(editor, cx, Vim::helix_select_lines);
    Vim::action(editor, cx, Vim::helix_insert);
    Vim::action(editor, cx, Vim::helix_append);
    Vim::action(editor, cx, Vim::helix_insert_end_of_line);
    Vim::action(editor, cx, Vim::helix_yank);
    Vim::action(editor, cx, Vim::helix_goto_last_modification);
    Vim::action(editor, cx, Vim::helix_goto_line);
    Vim::action(editor, cx, Vim::helix_paste);
    Vim::action(editor, cx, Vim::helix_select_regex);
    Vim::action(editor, cx, Vim::helix_keep_newest_selection);
    Vim::action(editor, cx, |vim, _: &HelixDuplicateBelow, window, cx| {
        let times = Vim::take_count(cx);
        vim.helix_duplicate_selections_below(times, window, cx);
    });
    Vim::action(editor, cx, |vim, _: &HelixDuplicateAbove, window, cx| {
        let times = Vim::take_count(cx);
        vim.helix_duplicate_selections_above(times, window, cx);
    });
    Vim::action(editor, cx, Vim::helix_substitute);
    Vim::action(editor, cx, Vim::helix_substitute_no_yank);
    Vim::action(editor, cx, Vim::helix_jump_to_word);
    Vim::action(editor, cx, Vim::helix_select_next);
    Vim::action(editor, cx, Vim::helix_select_previous);
    Vim::action(editor, cx, Vim::helix_trim_selections);
    Vim::action(editor, cx, |vim, _: &PushHelixSurroundAdd, window, cx| {
        vim.clear_operator(window, cx);
        vim.push_operator(Operator::HelixSurroundAdd, window, cx);
    });
    Vim::action(
        editor,
        cx,
        |vim, _: &PushHelixSurroundReplace, window, cx| {
            vim.clear_operator(window, cx);
            vim.push_operator(
                Operator::HelixSurroundReplace {
                    replaced_char: None,
                },
                window,
                cx,
            );
        },
    );
    Vim::action(
        editor,
        cx,
        |vim, _: &PushHelixSurroundDelete, window, cx| {
            vim.clear_operator(window, cx);
            vim.push_operator(Operator::HelixSurroundDelete, window, cx);
        },
    );
}

/// Returns column 0 of the document's first line, imitating `gg` in Helix.
///
/// With a count, Helix treats it as a (1-based) line number, so a count of `n`
/// targets buffer row `n - 1`, clamped to the last line.
fn start_of_document(map: &DisplaySnapshot, times: Option<usize>) -> DisplayPoint {
    let buffer_row = match times {
        None => 0,
        Some(times) => (times.saturating_sub(1) as u32).min(map.max_row().0),
    };
    map.point_to_display_point(Point::new(buffer_row, 0), Bias::Left)
}

/// Returns column 0 of the document's last line, imitating `ge` in Helix.
///
/// Helix ignores any count for `ge`, so it is not taken here.
fn end_of_document(map: &DisplaySnapshot) -> DisplayPoint {
    map.point_to_display_point(Point::new(map.max_row().0, 0), Bias::Left)
}

impl Vim {
    pub fn helix_normal_motion(
        &mut self,
        motion: Motion,
        times: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.helix_move_cursor(motion, times, window, cx);
    }

    pub fn helix_select_motion(
        &mut self,
        motion: Motion,
        times: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_editor(cx, |_, editor, cx| {
            let text_layout_details = editor.text_layout_details(window, cx);
            editor.change_selections(Default::default(), window, cx, |s| {
                if let Motion::ZedSearchResult { new_selections, .. } = &motion {
                    s.select_anchor_ranges(new_selections.clone());
                    return;
                };

                s.move_with(&mut |map, selection| {
                    let was_reversed = selection.reversed;
                    let mut current_head = selection.head();

                    // our motions assume the current character is after the cursor,
                    // but in (forward) visual mode the current character is just
                    // before the end of the selection.
                    if !selection.reversed && !selection.is_empty() {
                        current_head = movement::left(map, selection.end)
                    }

                    let (new_head, goal) = match motion {
                        Motion::StartOfDocument => {
                            (start_of_document(map, times), SelectionGoal::None)
                        }
                        Motion::EndOfDocument => (end_of_document(map), SelectionGoal::None),
                        // EndOfLine positions after the last character, but in
                        // helix visual mode we want the selection to end ON the
                        // last character. Adjust left here so the subsequent
                        // right-expansion (below) includes the last char without
                        // spilling into the newline.
                        Motion::EndOfLine { .. } => {
                            let (point, goal) = motion
                                .move_point(
                                    map,
                                    current_head,
                                    selection.goal,
                                    times,
                                    &text_layout_details,
                                )
                                .unwrap_or((current_head, selection.goal));
                            (movement::saturating_left(map, point), goal)
                        }
                        // Going to next word start is special cased
                        // since Vim differs from Helix in that motion
                        // Vim: `w` goes to the first character of a word
                        // Helix: `w` goes to the character before a word
                        Motion::NextWordStart { ignore_punctuation } => {
                            let mut head = movement::right(map, current_head);
                            let classifier =
                                map.buffer_snapshot().char_classifier_at(head.to_point(map));
                            for _ in 0..times.unwrap_or(1) {
                                let (_, new_head) =
                                    movement::find_boundary_trail(map, head, &mut |left, right| {
                                        Self::is_boundary_right(ignore_punctuation)(
                                            left,
                                            right,
                                            &classifier,
                                        )
                                    });
                                head = new_head;
                            }
                            head = movement::left(map, head);
                            (head, SelectionGoal::None)
                        }
                        _ => motion
                            .move_point(
                                map,
                                current_head,
                                selection.goal,
                                times,
                                &text_layout_details,
                            )
                            .unwrap_or((current_head, selection.goal)),
                    };

                    selection.set_head(new_head, goal);

                    // ensure the current character is included in the selection.
                    if !selection.reversed {
                        let next_point = movement::right(map, selection.end);

                        selection.end = next_point;
                    }

                    // vim always ensures the anchor character stays selected.
                    // if our selection has reversed, we need to move the opposite end
                    // to ensure the anchor is still selected.
                    if was_reversed && !selection.reversed {
                        selection.start = movement::left(map, selection.start);
                    } else if !was_reversed && selection.reversed {
                        selection.end = movement::right(map, selection.end);
                    }
                })
            });
        });
    }

    /// Updates all selections based on where the cursors are.
    fn helix_new_selections(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        change: &mut dyn FnMut(
            // the start of the cursor
            DisplayPoint,
            &DisplaySnapshot,
        ) -> Option<(DisplayPoint, DisplayPoint)>,
    ) {
        self.update_editor(cx, |_, editor, cx| {
            editor.change_selections(Default::default(), window, cx, |s| {
                s.move_with(&mut |map, selection| {
                    let cursor_start = if selection.reversed || selection.is_empty() {
                        selection.head()
                    } else {
                        movement::left(map, selection.head())
                    };
                    let Some((head, tail)) = change(cursor_start, map) else {
                        return;
                    };

                    selection.set_head_tail(head, tail, SelectionGoal::None);
                });
            });
        });
    }

    fn helix_find_range_forward(
        &mut self,
        times: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
        is_boundary: &mut dyn FnMut(char, char, &CharClassifier) -> bool,
    ) {
        let times = times.unwrap_or(1);
        self.helix_new_selections(window, cx, &mut |cursor, map| {
            let mut head = movement::right(map, cursor);
            let mut tail = cursor;
            let classifier = map.buffer_snapshot().char_classifier_at(head.to_point(map));
            if head == map.max_point() {
                return None;
            }
            for _ in 0..times {
                let (maybe_next_tail, next_head) =
                    movement::find_boundary_trail(map, head, &mut |left, right| {
                        is_boundary(left, right, &classifier)
                    });

                if next_head == head && maybe_next_tail.unwrap_or(next_head) == tail {
                    break;
                }

                head = next_head;
                if let Some(next_tail) = maybe_next_tail {
                    tail = next_tail;
                }
            }
            Some((head, tail))
        });
    }

    fn helix_find_range_backward(
        &mut self,
        times: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
        is_boundary: &mut dyn FnMut(char, char, &CharClassifier) -> bool,
    ) {
        let times = times.unwrap_or(1);
        self.helix_new_selections(window, cx, &mut |cursor, map| {
            let mut head = cursor;
            // The original cursor was one character wide,
            // but the search starts from the left side of it,
            // so to include that space the selection must end one character to the right.
            let mut tail = movement::right(map, cursor);
            let classifier = map.buffer_snapshot().char_classifier_at(head.to_point(map));
            if head == DisplayPoint::zero() {
                return None;
            }
            for _ in 0..times {
                let (maybe_next_tail, next_head) =
                    movement::find_preceding_boundary_trail(map, head, &mut |left, right| {
                        is_boundary(left, right, &classifier)
                    });

                if next_head == head && maybe_next_tail.unwrap_or(next_head) == tail {
                    break;
                }

                head = next_head;
                if let Some(next_tail) = maybe_next_tail {
                    tail = next_tail;
                }
            }
            Some((head, tail))
        });
    }

    pub fn helix_move_and_collapse(
        &mut self,
        motion: Motion,
        times: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_editor(cx, |_, editor, cx| {
            let text_layout_details = editor.text_layout_details(window, cx);
            editor.change_selections(Default::default(), window, cx, |s| {
                s.move_with(&mut |map, selection| {
                    let goal = selection.goal;
                    let cursor = if selection.is_empty() || selection.reversed {
                        selection.head()
                    } else {
                        movement::left(map, selection.head())
                    };

                    let (point, goal) = motion
                        .move_point(map, cursor, selection.goal, times, &text_layout_details)
                        .unwrap_or((cursor, goal));

                    selection.collapse_to(point, goal)
                })
            });
        });
    }

    fn is_boundary_right(
        ignore_punctuation: bool,
    ) -> impl FnMut(char, char, &CharClassifier) -> bool {
        move |left, right, classifier| {
            let left_kind = classifier.kind_with(left, ignore_punctuation);
            let right_kind = classifier.kind_with(right, ignore_punctuation);
            let at_newline = (left == '\n') ^ (right == '\n');

            (left_kind != right_kind && right_kind != CharKind::Whitespace) || at_newline
        }
    }

    fn is_boundary_left(
        ignore_punctuation: bool,
    ) -> impl FnMut(char, char, &CharClassifier) -> bool {
        move |left, right, classifier| {
            let left_kind = classifier.kind_with(left, ignore_punctuation);
            let right_kind = classifier.kind_with(right, ignore_punctuation);
            let at_newline = (left == '\n') ^ (right == '\n');

            (left_kind != right_kind && left_kind != CharKind::Whitespace) || at_newline
        }
    }

    /// When `reversed` is true (used with `helix_find_range_backward`), the
    /// `left` and `right` characters are yielded in reverse text order, so the
    /// camelCase transition check must be flipped accordingly.
    fn subword_boundary_start(
        ignore_punctuation: bool,
        reversed: bool,
    ) -> impl FnMut(char, char, &CharClassifier) -> bool {
        move |left, right, classifier| {
            let left_kind = classifier.kind_with(left, ignore_punctuation);
            let right_kind = classifier.kind_with(right, ignore_punctuation);
            let at_newline = (left == '\n') ^ (right == '\n');
            let is_separator = |c: char| "_$=".contains(c);

            let is_word = left_kind != right_kind && right_kind != CharKind::Whitespace;
            let is_subword = (is_separator(left) && !is_separator(right))
                || if reversed {
                    right.is_lowercase() && left.is_uppercase()
                } else {
                    left.is_lowercase() && right.is_uppercase()
                };

            is_word || (is_subword && !right.is_whitespace()) || at_newline
        }
    }

    /// When `reversed` is true (used with `helix_find_range_backward`), the
    /// `left` and `right` characters are yielded in reverse text order, so the
    /// camelCase transition check must be flipped accordingly.
    fn subword_boundary_end(
        ignore_punctuation: bool,
        reversed: bool,
    ) -> impl FnMut(char, char, &CharClassifier) -> bool {
        move |left, right, classifier| {
            let left_kind = classifier.kind_with(left, ignore_punctuation);
            let right_kind = classifier.kind_with(right, ignore_punctuation);
            let at_newline = (left == '\n') ^ (right == '\n');
            let is_separator = |c: char| "_$=".contains(c);

            let is_word = left_kind != right_kind && left_kind != CharKind::Whitespace;
            let is_subword = (!is_separator(left) && is_separator(right))
                || if reversed {
                    right.is_lowercase() && left.is_uppercase()
                } else {
                    left.is_lowercase() && right.is_uppercase()
                };

            is_word || (is_subword && !left.is_whitespace()) || at_newline
        }
    }

    pub fn helix_move_cursor(
        &mut self,
        motion: Motion,
        times: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match motion {
            Motion::NextWordStart { ignore_punctuation } => {
                let mut is_boundary = Self::is_boundary_right(ignore_punctuation);
                self.helix_find_range_forward(times, window, cx, &mut is_boundary)
            }
            Motion::NextWordEnd { ignore_punctuation } => {
                let mut is_boundary = Self::is_boundary_left(ignore_punctuation);
                self.helix_find_range_forward(times, window, cx, &mut is_boundary)
            }
            Motion::PreviousWordStart { ignore_punctuation } => {
                let mut is_boundary = Self::is_boundary_left(ignore_punctuation);
                self.helix_find_range_backward(times, window, cx, &mut is_boundary)
            }
            Motion::PreviousWordEnd { ignore_punctuation } => {
                let mut is_boundary = Self::is_boundary_right(ignore_punctuation);
                self.helix_find_range_backward(times, window, cx, &mut is_boundary)
            }
            // The subword motions implementation is based off of the same
            // commands present in Helix itself, namely:
            //
            // * `move_next_sub_word_start`
            // * `move_next_sub_word_end`
            // * `move_prev_sub_word_start`
            // * `move_prev_sub_word_end`
            Motion::NextSubwordStart { ignore_punctuation } => {
                let mut is_boundary = Self::subword_boundary_start(ignore_punctuation, false);
                self.helix_find_range_forward(times, window, cx, &mut is_boundary)
            }
            Motion::NextSubwordEnd { ignore_punctuation } => {
                let mut is_boundary = Self::subword_boundary_end(ignore_punctuation, false);
                self.helix_find_range_forward(times, window, cx, &mut is_boundary)
            }
            Motion::PreviousSubwordStart { ignore_punctuation } => {
                let mut is_boundary = Self::subword_boundary_end(ignore_punctuation, true);
                self.helix_find_range_backward(times, window, cx, &mut is_boundary)
            }
            Motion::PreviousSubwordEnd { ignore_punctuation } => {
                let mut is_boundary = Self::subword_boundary_start(ignore_punctuation, true);
                self.helix_find_range_backward(times, window, cx, &mut is_boundary)
            }
            Motion::StartOfDocument => {
                self.update_editor(cx, |_, editor, cx| {
                    editor.change_selections(Default::default(), window, cx, |s| {
                        s.move_with(&mut |map, selection| {
                            selection
                                .collapse_to(start_of_document(map, times), SelectionGoal::None)
                        })
                    });
                });
            }
            Motion::EndOfDocument => {
                self.update_editor(cx, |_, editor, cx| {
                    editor.change_selections(Default::default(), window, cx, |s| {
                        s.move_with(&mut |map, selection| {
                            selection.collapse_to(end_of_document(map), SelectionGoal::None)
                        })
                    });
                });
            }
            Motion::EndOfLine { .. } => {
                // In Helix mode, EndOfLine should position cursor ON the last character,
                // not after it. We therefore need special handling for it.
                self.update_editor(cx, |_, editor, cx| {
                    let text_layout_details = editor.text_layout_details(window, cx);
                    editor.change_selections(Default::default(), window, cx, |s| {
                        s.move_with(&mut |map, selection| {
                            let goal = selection.goal;
                            let cursor = if selection.is_empty() || selection.reversed {
                                selection.head()
                            } else {
                                movement::left(map, selection.head())
                            };

                            let (point, _goal) = motion
                                .move_point(map, cursor, goal, times, &text_layout_details)
                                .unwrap_or((cursor, goal));

                            // Move left by one character to position on the last character
                            let adjusted_point = movement::saturating_left(map, point);
                            selection.collapse_to(adjusted_point, SelectionGoal::None)
                        })
                    });
                });
            }
            Motion::FindForward {
                before,
                char,
                mode,
                smartcase,
            } => {
                self.helix_new_selections(window, cx, &mut |cursor, map| {
                    let start = cursor;
                    let mut last_boundary = start;
                    for _ in 0..times.unwrap_or(1) {
                        last_boundary = movement::find_boundary(
                            map,
                            movement::right(map, last_boundary),
                            mode,
                            &mut |left, right| {
                                let current_char = if before { right } else { left };
                                motion::is_character_match(char, current_char, smartcase)
                            },
                        );
                    }
                    Some((last_boundary, start))
                });
            }
            Motion::FindBackward {
                after,
                char,
                mode,
                smartcase,
            } => {
                self.helix_new_selections(window, cx, &mut |cursor, map| {
                    let start = cursor;
                    let mut last_boundary = start;
                    for _ in 0..times.unwrap_or(1) {
                        last_boundary = movement::find_preceding_boundary_display_point(
                            map,
                            last_boundary,
                            mode,
                            &mut |left, right| {
                                let current_char = if after { left } else { right };
                                motion::is_character_match(char, current_char, smartcase)
                            },
                        );
                    }
                    // The original cursor was one character wide,
                    // but the search started from the left side of it,
                    // so to include that space the selection must end one character to the right.
                    Some((last_boundary, movement::right(map, start)))
                });
            }
            _ => self.helix_move_and_collapse(motion, times, window, cx),
        }
    }

    pub fn helix_yank(&mut self, _: &HelixYank, window: &mut Window, cx: &mut Context<Self>) {
        self.update_editor(cx, |vim, editor, cx| {
            let has_selection = editor
                .selections
                .all_adjusted(&editor.display_snapshot(cx))
                .iter()
                .any(|selection| !selection.is_empty());

            if !has_selection {
                // If no selection, expand to current character (like 'v' does)
                editor.change_selections(Default::default(), window, cx, |s| {
                    s.move_with(&mut |map, selection| {
                        let head = selection.head();
                        let new_head = movement::saturating_right(map, head);
                        selection.set_tail(head, SelectionGoal::None);
                        selection.set_head(new_head, SelectionGoal::None);
                    });
                });
                vim.yank_selections_content(
                    editor,
                    crate::motion::MotionKind::Exclusive,
                    window,
                    cx,
                );
                editor.change_selections(Default::default(), window, cx, |s| {
                    s.move_with(&mut |_map, selection| {
                        selection.collapse_to(selection.start, SelectionGoal::None);
                    });
                });
            } else {
                // Yank the selection(s)
                vim.yank_selections_content(
                    editor,
                    crate::motion::MotionKind::Exclusive,
                    window,
                    cx,
                );
            }
        });

        // Drop back to normal mode after yanking
        self.switch_mode(Mode::HelixNormal, true, window, cx);
    }

    fn helix_insert(&mut self, _: &HelixInsert, window: &mut Window, cx: &mut Context<Self>) {
        self.start_recording(cx);
        self.update_editor(cx, |_, editor, cx| {
            editor.change_selections(Default::default(), window, cx, |s| {
                s.move_with(&mut |_map, selection| {
                    // In helix normal mode, move cursor to start of selection and collapse
                    if !selection.is_empty() {
                        selection.collapse_to(selection.start, SelectionGoal::None);
                    }
                });
            });
        });
        self.switch_mode(Mode::Insert, false, window, cx);
    }

    fn helix_select_regex(
        &mut self,
        _: &HelixSelectRegex,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        Vim::take_forced_motion(cx);
        let Some(pane) = self.pane(window, cx) else {
            return;
        };
        let prior_selections = self.editor_selections(window, cx);
        pane.update(cx, |pane, cx| {
            if let Some(search_bar) = pane.toolbar().read(cx).item_of_type::<BufferSearchBar>() {
                search_bar.update(cx, |search_bar, cx| {
                    if !search_bar.show(window, cx) {
                        return;
                    }

                    search_bar.select_query(window, cx);
                    cx.focus_self(window);

                    search_bar.set_replacement(None, cx);
                    let mut options = SearchOptions::NONE;
                    options |= SearchOptions::REGEX;
                    if EditorSettings::get_global(cx).search.case_sensitive {
                        options |= SearchOptions::CASE_SENSITIVE;
                    }
                    search_bar.set_search_options(options, cx);
                    if let Some(search) = search_bar.set_search_within_selection(
                        Some(FilteredSearchRange::Selection),
                        window,
                        cx,
                    ) {
                        cx.spawn_in(window, async move |search_bar, cx| {
                            if search.await.is_ok() {
                                search_bar.update_in(cx, |search_bar, window, cx| {
                                    search_bar.activate_current_match(window, cx)
                                })
                            } else {
                                Ok(())
                            }
                        })
                        .detach_and_log_err(cx);
                    }
                    self.search = SearchState {
                        direction: searchable::Direction::Next,
                        count: 1,
                        cmd_f_search: false,
                        prior_selections,
                        prior_operator: self.operator_stack.last().cloned(),
                        prior_mode: self.mode,
                        helix_select: true,
                        _dismiss_subscription: None,
                    }
                });
            }
        });
        self.start_recording(cx);
    }

    fn helix_append(&mut self, _: &HelixAppend, window: &mut Window, cx: &mut Context<Self>) {
        self.start_recording(cx);
        self.switch_mode(Mode::Insert, false, window, cx);
        self.update_editor(cx, |vim, editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let selections_before_append = editor
                .selections
                .all_anchors(&snapshot)
                .iter()
                .map(|selection| selection.tail()..selection.head())
                .collect();

            editor.change_selections(Default::default(), window, cx, |s| {
                s.move_with(&mut |map, selection| {
                    let point = if selection.is_empty() {
                        right(map, selection.head(), 1)
                    } else {
                        selection.end
                    };
                    selection.collapse_to(point, SelectionGoal::None);
                });
            });

            let snapshot = editor.display_snapshot(cx);
            let cursors_after_append = editor
                .selections
                .all_anchors(&snapshot)
                .iter()
                .map(|selection| selection.range())
                .collect();
            vim.helix_append_state = Some(HelixAppendState {
                selections_before_append,
                cursors_after_append,
            });
        });
    }

    /// Helix-specific implementation of `shift-a` that accounts for Helix's
    /// selection model, where selecting a line with `x` creates a selection
    /// from column 0 of the current row to column 0 of the next row, so the
    /// default [`vim::normal::InsertEndOfLine`] would move the cursor to the
    /// end of the wrong line.
    fn helix_insert_end_of_line(
        &mut self,
        _: &HelixInsertEndOfLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_recording(cx);
        self.switch_mode(Mode::Insert, false, window, cx);
        self.update_editor(cx, |_, editor, cx| {
            editor.change_selections(Default::default(), window, cx, |s| {
                s.move_with(&mut |map, selection| {
                    let cursor = if !selection.is_empty() && !selection.reversed {
                        movement::left(map, selection.head())
                    } else {
                        selection.head()
                    };
                    selection
                        .collapse_to(motion::next_line_end(map, cursor, 1), SelectionGoal::None);
                });
            });
        });
    }

    pub fn helix_replace(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.update_editor(cx, |_, editor, cx| {
            editor.transact(window, cx, |editor, window, cx| {
                let display_map = editor.display_snapshot(cx);
                let selections = editor.selections.all_display(&display_map);

                let mut edits = Vec::new();
                let mut selection_info = Vec::new();
                for selection in &selections {
                    let mut range = selection.range();
                    let was_empty = range.is_empty();
                    let was_reversed = selection.reversed;

                    if was_empty {
                        range.end = movement::saturating_right(&display_map, range.start);
                    }

                    let byte_range = range.start.to_offset(&display_map, Bias::Left)
                        ..range.end.to_offset(&display_map, Bias::Left);

                    let snapshot = display_map.buffer_snapshot();
                    let grapheme_count = snapshot.grapheme_count_for_range(&byte_range);
                    let anchor = snapshot.anchor_before(byte_range.start);
                    let mut replacement_len = 0;

                    if !byte_range.is_empty() {
                        let mut replacement_text = text.repeat(grapheme_count);
                        LineEnding::normalize(&mut replacement_text);
                        replacement_len = replacement_text.len();
                        edits.push((byte_range, replacement_text));
                    }

                    selection_info.push((anchor, replacement_len, was_empty, was_reversed));
                }

                editor.edit(edits, cx);

                // Restore selections based on original info
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let ranges: Vec<_> = selection_info
                    .into_iter()
                    .map(|(start_anchor, replacement_len, was_empty, was_reversed)| {
                        let start_point = start_anchor.to_point(&snapshot);
                        if was_empty {
                            start_point..start_point
                        } else {
                            let end_offset = start_anchor.to_offset(&snapshot) + replacement_len;
                            let end_point = snapshot.offset_to_point(end_offset);
                            if was_reversed {
                                end_point..start_point
                            } else {
                                start_point..end_point
                            }
                        }
                    })
                    .collect();

                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
                    s.select_ranges(ranges);
                });
            });
        });
        self.switch_mode(Mode::HelixNormal, true, window, cx);
    }

    pub fn helix_goto_last_modification(
        &mut self,
        _: &HelixGotoLastModification,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.jump(".".into(), false, false, window, cx);
    }

    pub fn helix_goto_line(
        &mut self,
        _: &HelixGotoLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(line_number) = Vim::take_count(cx) {
            self.helix_move_cursor(Motion::StartOfDocument, Some(line_number), window, cx);
        }
    }

    pub fn helix_select_lines(
        &mut self,
        _: &HelixSelectLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = Vim::take_count(cx).unwrap_or(1);
        self.update_editor(cx, |_, editor, cx| {
            let display_map = editor.display_map.update(cx, |map, cx| map.snapshot(cx));
            let mut selections = editor.selections.all::<Point>(&display_map);
            let max_point = display_map.buffer_snapshot().max_point();
            let buffer_snapshot = &display_map.buffer_snapshot();

            for selection in &mut selections {
                // Start always goes to column 0 of the first selected line
                let start_row = selection.start.row;
                let current_end_row = selection.end.row;

                // Check if cursor is on empty line by checking first character
                let line_start_offset = buffer_snapshot.point_to_offset(Point::new(start_row, 0));
                let first_char = buffer_snapshot.chars_at(line_start_offset).next();
                let extra_line = if first_char == Some('\n') && selection.is_empty() {
                    1
                } else {
                    0
                };

                let end_row = current_end_row + count as u32 + extra_line;

                selection.start = Point::new(start_row, 0);
                selection.end = if end_row > max_point.row {
                    max_point
                } else {
                    Point::new(end_row, 0)
                };
                selection.reversed = false;
            }

            editor.change_selections(Default::default(), window, cx, |s| {
                s.select(selections);
            });
        });
    }

    fn helix_keep_newest_selection(
        &mut self,
        _: &HelixKeepNewestSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_editor(cx, |_, editor, cx| {
            let newest = editor
                .selections
                .newest::<MultiBufferOffset>(&editor.display_snapshot(cx));
            editor.change_selections(Default::default(), window, cx, |s| s.select(vec![newest]));
        });
    }

    fn helix_trim_selections(
        &mut self,
        _: &HelixTrimSelections,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        Vim::take_count(cx);
        Vim::take_forced_motion(cx);
        self.update_editor(cx, |_, editor, cx| {
            let display_snapshot = editor.display_snapshot(cx);
            let buffer = display_snapshot.buffer_snapshot();
            let selections = editor
                .selections
                .all::<MultiBufferOffset>(&display_snapshot);
            let mut trimmed = Vec::new();

            for selection in &selections {
                if selection.is_empty() {
                    continue;
                }
                let all_whitespace = buffer
                    .text_for_range(selection.start..selection.end)
                    .flat_map(|chunk| chunk.chars())
                    .all(|ch| ch.is_whitespace());
                if all_whitespace {
                    continue;
                }

                let mut new_start = selection.start;
                for ch in buffer.chars_at(selection.start) {
                    if !ch.is_whitespace() {
                        break;
                    }
                    new_start += ch.len_utf8();
                }

                let mut new_end = selection.end;
                for ch in buffer.reversed_chars_at(selection.end) {
                    if !ch.is_whitespace() {
                        break;
                    }
                    new_end -= ch.len_utf8();
                }

                let mut new_selection = *selection;
                new_selection.start = new_start;
                new_selection.end = new_end;
                trimmed.push(new_selection);
            }

            if !trimmed.is_empty() {
                editor.change_selections(Default::default(), window, cx, |s| {
                    s.select(trimmed);
                });
                return;
            }

            // All selections were empty or entirely whitespace. Match Helix's
            // fallback: `collapse_selection` followed by `keep_primary_selection`.
            // Take the newest selection and collapse it to a one-char cursor at
            // the head (head - 1 for a forward, non-empty selection; head as-is
            // otherwise).
            let newest = editor
                .selections
                .newest::<MultiBufferOffset>(&display_snapshot);
            let head = if newest.reversed {
                newest.start
            } else {
                newest.end
            };
            let cursor_offset = if !newest.reversed && !newest.is_empty() {
                let mut p = head;
                if let Some(ch) = buffer.reversed_chars_at(head).next() {
                    p -= ch.len_utf8();
                }
                p
            } else {
                head
            };
            let mut collapsed = newest;
            collapsed.start = cursor_offset;
            collapsed.end = cursor_offset;
            editor.change_selections(Default::default(), window, cx, |s| {
                s.select(vec![collapsed]);
            });
        });
    }

    fn do_helix_substitute(&mut self, yank: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.update_editor(cx, |vim, editor, cx| {
            editor.set_clip_at_line_ends(false, cx);
            editor.transact(window, cx, |editor, window, cx| {
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
                    s.move_with(&mut |map, selection| {
                        if selection.start == selection.end {
                            selection.end = movement::right(map, selection.end);
                        }

                        // If the selection starts and ends on a newline, we exclude the last one.
                        if !selection.is_empty()
                            && selection.start.column() == 0
                            && selection.end.column() == 0
                        {
                            selection.end = movement::left(map, selection.end);
                        }
                    })
                });
                if yank {
                    vim.copy_selections_content(editor, MotionKind::Exclusive, window, cx);
                }
                let selections = editor
                    .selections
                    .all::<Point>(&editor.display_snapshot(cx))
                    .into_iter();
                let edits = selections.map(|selection| (selection.start..selection.end, ""));
                editor.edit(edits, cx);
            });
        });
        self.switch_mode(Mode::Insert, true, window, cx);
    }

    fn helix_substitute(
        &mut self,
        _: &HelixSubstitute,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.do_helix_substitute(true, window, cx);
    }

    fn helix_substitute_no_yank(
        &mut self,
        _: &HelixSubstituteNoYank,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.do_helix_substitute(false, window, cx);
    }

    fn helix_select_next(
        &mut self,
        _: &HelixSelectNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.do_helix_select(Direction::Next, window, cx);
    }

    fn helix_select_previous(
        &mut self,
        _: &HelixSelectPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.do_helix_select(Direction::Prev, window, cx);
    }

    fn do_helix_select(
        &mut self,
        direction: searchable::Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane(window, cx) else {
            return;
        };
        let count = Vim::take_count(cx).unwrap_or(1);
        Vim::take_forced_motion(cx);
        let prior_selections = self.editor_selections(window, cx);

        let success = pane.update(cx, |pane, cx| {
            let Some(search_bar) = pane.toolbar().read(cx).item_of_type::<BufferSearchBar>() else {
                return false;
            };
            search_bar.update(cx, |search_bar, cx| {
                if !search_bar.has_active_match() || !search_bar.show(window, cx) {
                    return false;
                }
                search_bar.select_match(direction, count, window, cx);
                true
            })
        });

        if !success {
            return;
        }
        if self.mode == Mode::HelixSelect {
            self.update_editor(cx, |_vim, editor, cx| {
                let snapshot = editor.snapshot(window, cx);
                editor.change_selections(SelectionEffects::default(), window, cx, |s| {
                    let buffer = snapshot.buffer_snapshot();

                    s.select_ranges(
                        prior_selections
                            .iter()
                            .cloned()
                            .chain(s.all_anchors(&snapshot).iter().map(|s| s.range()))
                            .map(|range| {
                                let start = range.start.to_offset(buffer);
                                let end = range.end.to_offset(buffer);
                                start..end
                            }),
                    );
                })
            });
        }
    }

    pub fn helix_jump_to_word(
        &mut self,
        _: &HelixJumpToWord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let behaviour = match self.mode {
            // Vim normal mode treats jump-to-word as a cursor motion, while Helix
            // normal mode treats the cursor as a single-character selection.
            Mode::Normal => HelixJumpBehaviour::MoveToWordStart,
            // Vim visual mode extends like a motion, so the cursor stops at the
            // same word boundary as normal mode instead of selecting the word.
            Mode::Visual | Mode::VisualLine | Mode::VisualBlock => {
                HelixJumpBehaviour::ExtendToWordStart
            }
            Mode::HelixSelect => HelixJumpBehaviour::Extend,
            _ => HelixJumpBehaviour::Move,
        };
        self.start_helix_jump(behaviour, window, cx);
    }

    fn start_helix_jump(
        &mut self,
        behaviour: HelixJumpBehaviour,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let allow_targets_in_selection = self.mode.has_selection();
        let Some(data) = self.collect_helix_jump_data(allow_targets_in_selection, window, cx)
        else {
            return;
        };

        if data.labels.is_empty() {
            self.clear_helix_jump_ui(window, cx);
            return;
        }

        if !self.apply_helix_jump_ui(data.overlays, window, cx) {
            return;
        }

        self.push_operator(
            Operator::HelixJump {
                behaviour,
                first_char: None,
                labels: data.labels,
            },
            window,
            cx,
        );
    }

    fn collect_helix_jump_data(
        &mut self,
        allow_targets_in_selection: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<HelixJumpUiData> {
        self.update_editor(cx, |_, editor, cx| {
            let snapshot = editor.snapshot(window, cx);
            let display_snapshot = &snapshot.display_snapshot;
            let buffer_snapshot = display_snapshot.buffer_snapshot();
            let visible_range = Self::visible_jump_range(editor, &snapshot, display_snapshot, cx);
            let start_offset = buffer_snapshot.point_to_offset(visible_range.start);
            let end_offset = buffer_snapshot.point_to_offset(visible_range.end);

            let selections = editor.selections.all::<Point>(&display_snapshot);
            let skip_data = Self::selection_skip_offsets(
                buffer_snapshot,
                &selections,
                allow_targets_in_selection,
            );

            // Get the primary cursor position for alternating forward/backward labeling
            let cursor_offset = selections
                .first()
                .map(|s| buffer_snapshot.point_to_offset(s.head()))
                .unwrap_or(start_offset);

            let style = editor.style(cx);
            let font = style.text.font();
            let font_size = style.text.font_size.to_pixels(window.rem_size());
            let label_color = cx.theme().colors().vim_helix_jump_label_foreground;

            Self::build_helix_jump_ui_data(
                buffer_snapshot,
                start_offset,
                end_offset,
                cursor_offset,
                label_color,
                &skip_data,
                window.text_system(),
                font,
                font_size,
            )
        })
    }

    fn visible_jump_range(
        editor: &Editor,
        snapshot: &editor::EditorSnapshot,
        display_snapshot: &DisplaySnapshot,
        cx: &App,
    ) -> Range<Point> {
        let visible_range = editor.multi_buffer_visible_range(display_snapshot, cx);
        if editor.visible_line_count().is_some() || visible_range.start != visible_range.end {
            return visible_range;
        }

        let scroll_position = snapshot.scroll_position();
        let top_row = scroll_position.y.floor().max(0.0) as u32;
        let visible_rows = display_snapshot
            .max_point()
            .row()
            .0
            .saturating_sub(top_row)
            .saturating_add(1);
        let start_display_point = DisplayPoint::new(DisplayRow(top_row), 0);
        let end_display_point =
            DisplayPoint::new(DisplayRow(top_row.saturating_add(visible_rows)), 0);

        display_snapshot.display_point_to_point(start_display_point, Bias::Left)
            ..display_snapshot.display_point_to_point(end_display_point, Bias::Right)
    }

    fn build_helix_jump_ui_data(
        buffer: &MultiBufferSnapshot,
        start_offset: MultiBufferOffset,
        end_offset: MultiBufferOffset,
        cursor_offset: MultiBufferOffset,
        label_color: Hsla,
        skip_data: &HelixJumpSkipData,
        text_system: &WindowTextSystem,
        font: Font,
        font_size: Pixels,
    ) -> HelixJumpUiData {
        if start_offset >= end_offset {
            return HelixJumpUiData::default();
        }

        // First pass: collect all word candidates without assigning labels
        let candidates = Self::collect_jump_candidates(buffer, start_offset, end_offset, skip_data);

        if candidates.is_empty() {
            return HelixJumpUiData::default();
        }

        let ordered_candidates = Self::order_jump_candidates(candidates, cursor_offset);

        // Now assign labels and build UI data
        let mut labels = Vec::with_capacity(ordered_candidates.len());
        let mut overlays = Vec::with_capacity(ordered_candidates.len());

        let width_of = |text: &str| -> Pixels {
            if text.is_empty() {
                return px(0.0);
            }

            let run = gpui::TextRun {
                len: text.len(),
                font: font.clone(),
                color: Hsla::default(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };

            text_system.layout_line(text, font_size, &[run], None).width
        };

        let is_monospace = Self::is_monospace_jump_font(text_system, &font, font_size);

        for (label_index, candidate) in ordered_candidates.into_iter().enumerate() {
            let start_anchor = buffer.anchor_after(candidate.word_start);
            let end_anchor = buffer.anchor_after(candidate.word_end);
            let label = Self::jump_label_for_index(label_index);
            let label_text = label.iter().collect::<String>();
            // Monospace fonts: the label always matches the width of the first two characters,
            // so no per-word measurement is needed.
            // Proportional fonts: a label like "mw" can be wider than a short word like "if",
            // so we hide enough of the word (and possibly trailing whitespace) to make room,
            // or shift the label left into preceding whitespace.
            let fit = if is_monospace {
                JumpLabelFit::monospace(candidate.first_two_end)
            } else {
                let label_width = width_of(&label_text);
                Self::fit_proportional_jump_label(
                    buffer,
                    &candidate,
                    end_offset,
                    label_width,
                    &width_of,
                )
            };

            let hide_end_anchor = buffer.anchor_after(fit.hide_end_offset);

            labels.push(HelixJumpLabel {
                label,
                range: start_anchor..end_anchor,
            });

            overlays.push(NavigationTargetOverlay {
                target_range: start_anchor..end_anchor,
                label: NavigationOverlayLabel {
                    text: label_text.into(),
                    text_color: label_color,
                    x_offset: -fit.left_shift,
                    scale_factor: fit.scale_factor,
                },
                covered_text_range: Some(start_anchor..hide_end_anchor),
            });
        }

        HelixJumpUiData { labels, overlays }
    }

    fn collect_jump_candidates(
        buffer: &MultiBufferSnapshot,
        start_offset: MultiBufferOffset,
        end_offset: MultiBufferOffset,
        skip_data: &HelixJumpSkipData,
    ) -> Vec<JumpCandidate> {
        let mut candidates = Vec::new();

        let mut offset = start_offset;
        let mut in_word = false;
        let mut word_start = start_offset;
        let mut first_two_end = start_offset;
        let mut char_count = 0;

        for chunk in buffer.text_for_range(start_offset..end_offset) {
            for (idx, ch) in chunk.char_indices() {
                let absolute = offset + idx;
                let is_word = is_jump_word_char(ch);
                if is_word {
                    if !in_word {
                        in_word = true;
                        word_start = absolute;
                        char_count = 0;
                    }
                    if char_count == 1 {
                        first_two_end = absolute + ch.len_utf8();
                    }
                    char_count += 1;
                }

                if !is_word && in_word {
                    if char_count >= 2
                        && !Self::should_skip_jump_candidate(word_start, absolute, skip_data)
                    {
                        candidates.push(JumpCandidate {
                            word_start,
                            word_end: absolute,
                            first_two_end,
                        });
                    }
                    in_word = false;
                }
            }
            offset += chunk.len();
        }

        // Handle word at end of buffer
        if in_word
            && char_count >= 2
            && !Self::should_skip_jump_candidate(word_start, end_offset, skip_data)
        {
            candidates.push(JumpCandidate {
                word_start,
                word_end: end_offset,
                first_two_end,
            });
        }

        candidates
    }

    fn selection_skip_offsets(
        buffer: &MultiBufferSnapshot,
        selections: &[Selection<Point>],
        allow_targets_in_selection: bool,
    ) -> HelixJumpSkipData {
        let mut skip_points = Vec::with_capacity(selections.len());
        let mut skip_ranges = Vec::new();

        for selection in selections {
            let head_offset = buffer.point_to_offset(selection.head());
            skip_points.push(head_offset);

            if !allow_targets_in_selection && selection.start != selection.end {
                let mut start = buffer.point_to_offset(selection.start);
                let mut end = buffer.point_to_offset(selection.end);
                if start > end {
                    std::mem::swap(&mut start, &mut end);
                }
                skip_ranges.push(start..end);
            }
        }

        skip_points.sort_unstable();

        skip_ranges.sort_unstable_by_key(|range| range.start);
        let mut merged_ranges: Vec<Range<MultiBufferOffset>> =
            Vec::with_capacity(skip_ranges.len());
        for range in skip_ranges {
            if let Some(previous_range) = merged_ranges.last_mut()
                && range.start <= previous_range.end
            {
                previous_range.end = previous_range.end.max(range.end);
            } else {
                merged_ranges.push(range);
            }
        }

        HelixJumpSkipData {
            points: skip_points,
            ranges: merged_ranges,
        }
    }

    fn should_skip_jump_candidate(
        word_start: MultiBufferOffset,
        word_end: MultiBufferOffset,
        skip_data: &HelixJumpSkipData,
    ) -> bool {
        // word_end is exclusive, so points at the following delimiter should not skip the word.
        let point_index = skip_data
            .points
            .partition_point(|offset| *offset < word_start);
        if skip_data
            .points
            .get(point_index)
            .is_some_and(|offset| *offset < word_end)
        {
            return true;
        }

        let range_index = skip_data
            .ranges
            .partition_point(|range| range.end <= word_start);
        skip_data
            .ranges
            .get(range_index)
            .is_some_and(|range| range.start < word_end)
    }

    /// Interleave candidates so forward targets get even label indices (aa, ac, ae...)
    /// and backward targets get odd indices (ab, ad, af...), matching Helix's algorithm.
    /// This keeps the earliest label assignments close to the cursor in both directions.
    fn order_jump_candidates(
        candidates: Vec<JumpCandidate>,
        cursor_offset: MultiBufferOffset,
    ) -> Vec<JumpCandidate> {
        let mut forward = Vec::with_capacity(candidates.len());
        let mut backward = Vec::new();

        for candidate in candidates {
            if candidate.word_start < cursor_offset {
                backward.push(candidate);
            } else {
                forward.push(candidate);
            }
        }

        backward.reverse();

        let mut ordered_candidates =
            Vec::with_capacity((forward.len() + backward.len()).min(HELIX_JUMP_LABEL_LIMIT));
        let mut forward_candidates = forward.into_iter();
        let mut backward_candidates = backward.into_iter();

        loop {
            let mut pushed_candidate = false;

            if ordered_candidates.len() < HELIX_JUMP_LABEL_LIMIT
                && let Some(candidate) = forward_candidates.next()
            {
                ordered_candidates.push(candidate);
                pushed_candidate = true;
            }

            if ordered_candidates.len() < HELIX_JUMP_LABEL_LIMIT
                && let Some(candidate) = backward_candidates.next()
            {
                ordered_candidates.push(candidate);
                pushed_candidate = true;
            }

            if !pushed_candidate {
                break;
            }
        }

        ordered_candidates
    }

    fn jump_label_for_index(index: usize) -> [char; 2] {
        [
            HELIX_JUMP_ALPHABET[index / HELIX_JUMP_ALPHABET.len()],
            HELIX_JUMP_ALPHABET[index % HELIX_JUMP_ALPHABET.len()],
        ]
    }

    fn is_monospace_jump_font(
        text_system: &WindowTextSystem,
        font: &Font,
        font_size: Pixels,
    ) -> bool {
        let font_id = text_system.resolve_font(font);
        let width_of_char = |ch| {
            text_system
                .advance(font_id, font_size, ch)
                .map(|size| size.width)
                .unwrap_or_else(|_| text_system.layout_width(font_id, font_size, ch))
        };

        let a = width_of_char('i');
        let b = width_of_char('w');
        let c = width_of_char('0');
        let d = width_of_char('1');
        let diff_1 = if a > b { a - b } else { b - a };
        let diff_2 = if c > d { c - d } else { d - c };
        diff_1 <= HELIX_JUMP_MONOSPACE_TOLERANCE && diff_2 <= HELIX_JUMP_MONOSPACE_TOLERANCE
    }

    /// Fit a jump label over a word in a proportional font.
    ///
    /// Prefer fitting within the word itself, using available whitespace to the left
    /// before consuming trailing whitespace after the word. If the label still cannot
    /// fit cleanly, allow a small amount of scaling.
    fn fit_proportional_jump_label<F: Fn(&str) -> Pixels>(
        buffer: &MultiBufferSnapshot,
        candidate: &JumpCandidate,
        end_offset: MultiBufferOffset,
        label_width: Pixels,
        width_of: &F,
    ) -> JumpLabelFit {
        let fit_budget = Self::jump_label_fit_budget(buffer, candidate, end_offset, width_of);

        let mut hidden_prefix = HiddenPrefixFitState::new(candidate.first_two_end);
        let min_label_scale = if fit_budget.preserve_full_scale {
            1.0
        } else {
            HELIX_JUMP_MIN_LABEL_SCALE
        };

        hidden_prefix.extend_to_fit(
            buffer,
            candidate.word_start,
            candidate.word_end,
            candidate.word_end,
            label_width,
            fit_budget.max_left_shift,
            min_label_scale,
            width_of,
        );

        if label_width > px(0.0)
            && hidden_prefix.needs_more_width(label_width, fit_budget.max_left_shift)
            && fit_budget.allowed_trailing_hide_end > candidate.word_end
        {
            hidden_prefix.extend_to_fit(
                buffer,
                candidate.word_end,
                fit_budget.allowed_trailing_hide_end,
                candidate.word_end,
                label_width,
                fit_budget.max_left_shift,
                min_label_scale,
                width_of,
            );
        }

        // Jump candidates always contain at least two word characters, and the initial
        // scan above always measures through that second character before we read the width.
        let hidden_width = hidden_prefix.hidden_width;

        let left_shift = if label_width > hidden_width {
            (label_width - hidden_width).min(fit_budget.max_left_shift)
        } else {
            px(0.0)
        };

        let scale_factor = if label_width > px(0.0) {
            let scale = ((hidden_width + left_shift) / label_width).min(1.0);
            if scale < 1.0 { scale * 0.99 } else { 1.0 }
        } else {
            1.0
        };

        JumpLabelFit {
            hide_end_offset: hidden_prefix.hide_end_offset,
            left_shift,
            scale_factor: if fit_budget.preserve_full_scale {
                1.0
            } else {
                scale_factor
            },
        }
    }

    fn jump_label_fit_budget<F: Fn(&str) -> Pixels>(
        buffer: &MultiBufferSnapshot,
        candidate: &JumpCandidate,
        end_offset: MultiBufferOffset,
        width_of: &F,
    ) -> JumpLabelFitBudget {
        let mut left_ws_rev = String::new();
        let mut left_ws_count = 0usize;
        let mut left_stopped_at_line_break = false;
        let mut left_stopped_at_non_ws = false;
        let mut left_hit_limit = false;

        for ch in buffer.reversed_chars_at(candidate.word_start) {
            if ch == '\n' || ch == '\r' {
                left_stopped_at_line_break = true;
                break;
            }

            if !ch.is_whitespace() {
                left_stopped_at_non_ws = true;
                break;
            }

            left_ws_count += 1;
            if left_ws_count > HELIX_JUMP_MAX_LEFT_WS_CHARS {
                left_hit_limit = true;
                break;
            }

            left_ws_rev.push(ch);
        }

        let left_ws: String = left_ws_rev.chars().rev().collect();
        let left_ws_width = width_of(&left_ws);
        let left_is_indentation =
            left_stopped_at_line_break || (!left_stopped_at_non_ws && !left_hit_limit);
        // Between tokens leave a small gap so the label doesn't touch the previous word;
        // for line-leading indentation the full width is safe.
        let min_left_gap = if left_is_indentation {
            px(0.0)
        } else {
            px(2.0)
        };
        let max_left_shift = (left_ws_width - min_left_gap).max(px(0.0));

        let mut allowed_trailing_hide_end = candidate.word_end;
        let mut ws_count = 0usize;
        let mut last_ws_start = candidate.word_end;
        let mut ws_end_offset = candidate.word_end;
        let mut next_non_ws = None;
        let mut hit_line_break_after_word = false;

        let mut ws_scan_offset = candidate.word_end;
        'scan: for chunk in buffer.text_for_range(candidate.word_end..end_offset) {
            for (idx, ch) in chunk.char_indices() {
                let absolute = ws_scan_offset + idx;
                if ch == '\n' || ch == '\r' {
                    hit_line_break_after_word = true;
                    break 'scan;
                }
                if !ch.is_whitespace() {
                    next_non_ws = Some(ch);
                    break 'scan;
                }

                ws_count += 1;
                last_ws_start = absolute;
                ws_end_offset = absolute + ch.len_utf8();
            }
            ws_scan_offset += chunk.len();
        }

        let preserve_full_scale = hit_line_break_after_word && next_non_ws.is_none()
            || matches!(
                buffer.chars_at(candidate.word_end).next(),
                None | Some('\n') | Some('\r')
            );

        if ws_count > 0 {
            let next_is_word = match next_non_ws {
                Some(ch) => is_jump_word_char(ch),
                None => false,
            };

            if next_is_word {
                // Keep at least one whitespace character visible so adjacent labels
                // remain visually separated.
                if ws_count > 1 {
                    allowed_trailing_hide_end = last_ws_start;
                }
            } else {
                // Next token is punctuation or end-of-range — safe to hide all whitespace.
                allowed_trailing_hide_end = ws_end_offset;
            }
        }

        JumpLabelFitBudget {
            max_left_shift,
            allowed_trailing_hide_end,
            preserve_full_scale,
        }
    }
}

const HELIX_JUMP_ALPHABET: &[char; 26] = &[
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's',
    't', 'u', 'v', 'w', 'x', 'y', 'z',
];
const HELIX_JUMP_LABEL_LIMIT: usize = HELIX_JUMP_ALPHABET.len() * HELIX_JUMP_ALPHABET.len();
const HELIX_JUMP_MONOSPACE_TOLERANCE: Pixels = px(0.5);
const HELIX_JUMP_MIN_LABEL_SCALE: f32 = 1.0;
const HELIX_JUMP_MAX_HIDDEN_CHARS: usize = 16;
const HELIX_JUMP_MAX_LEFT_WS_CHARS: usize = 32;

fn is_jump_word_char(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

/// A word candidate for jump labels, before label assignment.
#[derive(Clone)]
struct JumpCandidate {
    word_start: MultiBufferOffset,
    word_end: MultiBufferOffset,
    first_two_end: MultiBufferOffset,
}

struct HelixJumpSkipData {
    points: Vec<MultiBufferOffset>,
    ranges: Vec<Range<MultiBufferOffset>>,
}

struct JumpLabelFit {
    hide_end_offset: MultiBufferOffset,
    left_shift: Pixels,
    scale_factor: f32,
}

struct JumpLabelFitBudget {
    max_left_shift: Pixels,
    allowed_trailing_hide_end: MultiBufferOffset,
    preserve_full_scale: bool,
}

struct HiddenPrefixFitState {
    text: String,
    hide_end_offset: MultiBufferOffset,
    hidden_width: Pixels,
    total_char_count: usize,
    word_char_count: usize,
}

impl JumpLabelFit {
    fn monospace(hide_end_offset: MultiBufferOffset) -> Self {
        Self {
            hide_end_offset,
            left_shift: px(0.0),
            scale_factor: 1.0,
        }
    }
}

impl HiddenPrefixFitState {
    fn new(hide_end_offset: MultiBufferOffset) -> Self {
        Self {
            text: String::new(),
            hide_end_offset,
            hidden_width: px(0.0),
            total_char_count: 0,
            word_char_count: 0,
        }
    }

    fn needs_more_width(&self, label_width: Pixels, max_left_shift: Pixels) -> bool {
        (self.hidden_width + max_left_shift) / label_width < HELIX_JUMP_MIN_LABEL_SCALE
    }

    fn extend_to_fit<F: Fn(&str) -> Pixels>(
        &mut self,
        buffer: &MultiBufferSnapshot,
        range_start: MultiBufferOffset,
        range_end: MultiBufferOffset,
        word_end: MultiBufferOffset,
        label_width: Pixels,
        max_left_shift: Pixels,
        min_label_scale: f32,
        width_of: &F,
    ) {
        let mut offset = range_start;
        for chunk in buffer.text_for_range(range_start..range_end) {
            for (idx, ch) in chunk.char_indices() {
                let absolute = offset + idx;

                self.total_char_count += 1;
                if self.total_char_count > HELIX_JUMP_MAX_HIDDEN_CHARS {
                    return;
                }

                self.text.push(ch);
                let end_offset = absolute + ch.len_utf8();

                if absolute < word_end && is_jump_word_char(ch) {
                    self.word_char_count += 1;
                }

                if self.word_char_count < 2 {
                    continue;
                }

                self.hide_end_offset = end_offset;
                self.hidden_width = width_of(self.text.as_str());

                let effective_width = self.hidden_width + max_left_shift;
                let scale_needed = if label_width > px(0.0) {
                    (effective_width / label_width).min(1.0)
                } else {
                    1.0
                };

                if scale_needed >= min_label_scale {
                    return;
                }
            }
            offset += chunk.len();
        }
    }
}

#[derive(Default)]
struct HelixJumpUiData {
    labels: Vec<HelixJumpLabel>,
    overlays: Vec<NavigationTargetOverlay>,
}

