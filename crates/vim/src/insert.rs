use crate::{Vim, state::Mode};
use editor::{Bias, Editor};
use gpui::{Action, Context, Window, actions};
use language::SelectionGoal;
use settings::Settings;
use text::Point;
use vim_mode_setting::HelixModeSetting;
use workspace::searchable::Direction;

actions!(
    vim,
    [
        /// Switches to normal mode with cursor positioned before the current character.
        NormalBefore,
        /// Temporarily switches to normal mode for one command.
        TemporaryNormal,
        /// Inserts the next character from the line above into the current line.
        InsertFromAbove,
        /// Inserts the next character from the line below into the current line.
        InsertFromBelow
    ]
);

pub fn register(editor: &mut Editor, cx: &mut Context<Vim>) {
    Vim::action(editor, cx, Vim::normal_before);
    Vim::action(editor, cx, Vim::temporary_normal);
    Vim::action(editor, cx, |vim, _: &InsertFromAbove, window, cx| {
        vim.insert_around(Direction::Prev, window, cx)
    });
    Vim::action(editor, cx, |vim, _: &InsertFromBelow, window, cx| {
        vim.insert_around(Direction::Next, window, cx)
    })
}

impl Vim {
    pub(crate) fn normal_before(
        &mut self,
        action: &NormalBefore,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_operator().is_some() {
            self.operator_stack.clear();
            self.sync_vim_settings(window, cx);
            return;
        }
        let count = Vim::take_count(cx).unwrap_or(1);
        Vim::take_forced_motion(cx);
        self.stop_recording_immediately(action.boxed_clone(), cx);
        if count <= 1 || Vim::globals(cx).dot_replaying {
            self.create_mark("^".into(), window, cx);

            if HelixModeSetting::get_global(cx).0 {
                // Restore the pre-append selections only if nothing happened in
                // insert mode: no edits (tracked by current_tx/current_anchor)
                // and the cursors are still where the append left them.
                let no_insert_transaction =
                    self.current_tx.is_none() && self.current_anchor.is_none();
                let restore_append_selection = self
                    .helix_append_state
                    .take()
                    .filter(|_| no_insert_transaction);
                self.update_editor(cx, |_, editor, cx| {
                    editor.dismiss_menus_and_popups(false, window, cx);
                    if let Some(append_state) = restore_append_selection {
                        let snapshot = editor.display_snapshot(cx);
                        let current = editor
                            .selections
                            .all_anchors(&snapshot)
                            .iter()
                            .map(|selection| selection.range())
                            .collect::<Vec<_>>();

                        if current == append_state.cursors_after_append {
                            editor.change_selections(Default::default(), window, cx, |s| {
                                s.select_anchor_ranges(append_state.selections_before_append);
                            });
                        }
                    }
                });
                self.switch_mode(Mode::HelixNormal, false, window, cx);
                return;
            }

            self.update_editor(cx, |_, editor, cx| {
                editor.dismiss_menus_and_popups(false, window, cx);

                editor.change_selections(Default::default(), window, cx, |s| {
                    s.move_cursors_with(&mut |map, mut cursor, _| {
                        *cursor.column_mut() = cursor.column().saturating_sub(1);
                        (map.clip_point(cursor, Bias::Left), SelectionGoal::None)
                    });
                });
            });

            self.switch_mode(Mode::Normal, false, window, cx);
            return;
        }

        self.repeat(true, window, cx)
    }

    fn temporary_normal(
        &mut self,
        _: &TemporaryNormal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.switch_mode(Mode::Normal, true, window, cx);
        self.temp_mode = true;
    }

    fn insert_around(&mut self, direction: Direction, _: &mut Window, cx: &mut Context<Self>) {
        self.update_editor(cx, |_, editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let mut edits = Vec::new();
            for selection in editor.selections.all::<Point>(&editor.display_snapshot(cx)) {
                let point = selection.head();
                let new_row = match direction {
                    Direction::Next => point.row + 1,
                    Direction::Prev if point.row > 0 => point.row - 1,
                    _ => continue,
                };
                let source = snapshot.clip_point(Point::new(new_row, point.column), Bias::Left);
                if let Some(c) = snapshot.chars_at(source).next()
                    && c != '\n'
                {
                    edits.push((point..point, c.to_string()))
                }
            }

            editor.edit(edits, cx);
        });
    }
}

