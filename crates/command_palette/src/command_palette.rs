mod command_palette_settings;
mod persistence;

use std::{
    cmp::{self, Reverse},
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use anyhow::Context as _;
use client::parse_zed_link;
use command_palette_hooks::{
    CommandInterceptItem, CommandInterceptResult, CommandPaletteFilter,
    GlobalCommandPaletteInterceptor,
};
use command_palette_settings::CommandPaletteSettings;

use fuzzy_nucleo::{StringMatch, StringMatchCandidate};
use gpui::{
    Action, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, MouseButton,
    ParentElement, Render, Styled, Task, TaskExt, WeakEntity, Window, actions,
};
use persistence::CommandPaletteDB;
use picker::Direction;
use picker::{Picker, PickerDelegate};
use postage::{sink::Sink, stream::Stream};
use settings::Settings;
use ui::{
    ButtonLike, HighlightedLabel, KeyBinding, ListItem, ListItemSpacing, Tooltip, prelude::*,
};
use util::ResultExt;
use workspace::{ModalView, Workspace, WorkspaceSettings};
use zed_actions::{OpenZedUrl, command_palette::Toggle};

actions!(command_palette, [RemoveSelected]);

pub fn init(cx: &mut App) {
    CommandPaletteSettings::register(cx);
    command_palette_hooks::init(cx);
    cx.observe_new(CommandPalette::register).detach();
}

impl ModalView for CommandPalette {}

pub struct CommandPalette {
    picker: Entity<Picker<CommandPaletteDelegate>>,
}

/// Removes subsequent whitespace characters and double colons from the query, and converts
/// underscores to spaces.
///
/// This improves the likelihood of a match by either humanized name or keymap-style name.
/// Underscores are converted to spaces because `humanize_action_name` converts them to spaces
/// when building the search candidates (e.g. `terminal_panel::Toggle` -> `terminal panel: toggle`).
pub fn normalize_action_query(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut last_char = None;

    for char in input.trim().chars() {
        let normalized_char = if char == '_' { ' ' } else { char };
        match (last_char, normalized_char) {
            (Some(':'), ':') => continue,
            (Some(last_char), c) if last_char.is_whitespace() && c.is_whitespace() => {
                continue;
            }
            _ => {
                last_char = Some(normalized_char);
            }
        }
        result.push(normalized_char);
    }

    result
}

impl CommandPalette {
    fn register(
        workspace: &mut Workspace,
        _window: Option<&mut Window>,
        _: &mut Context<Workspace>,
    ) {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            Self::toggle(workspace, "", window, cx)
        });
    }

    pub fn toggle(
        workspace: &mut Workspace,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if workspace.active_modal::<CommandPalette>(cx).is_some() {
            workspace.hide_modal(window, cx);
            return;
        }

        if workspace.has_active_modal(window, cx) && !workspace.hide_modal(window, cx) {
            return;
        }

        let Some(previous_focus_handle) = window.focused(cx) else {
            return;
        };

        let entity = cx.weak_entity();
        workspace.toggle_modal(window, cx, move |window, cx| {
            CommandPalette::new(previous_focus_handle, query, entity, window, cx)
        });
    }

    fn new(
        previous_focus_handle: FocusHandle,
        query: &str,
        entity: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = CommandPaletteFilter::try_global(cx);

        let commands = window
            .available_actions(cx)
            .into_iter()
            .filter_map(|action| {
                if filter.is_some_and(|filter| filter.is_hidden(&*action)) {
                    return None;
                }

                Some(Command {
                    name: SharedString::from(humanize_action_name(action.name())),
                    action,
                    usage: None,
                })
            })
            .collect();

        let delegate = CommandPaletteDelegate::new(
            cx.entity().downgrade(),
            entity,
            commands,
            previous_focus_handle,
        );

        let picker = cx.new(|cx| {
            // One-shot action; there's nothing to reopen.
            let picker = Picker::uniform_list(delegate, window, cx)
                .initial_width(rems(38.0))
                .reopenable(false, cx)
                .show_scrollbar(true);
            picker.set_query(query, window, cx);
            picker
        });
        Self { picker }
    }

    pub fn set_query(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.picker
            .update(cx, |picker, cx| picker.set_query(query, window, cx))
    }

    fn remove_selected(&mut self, _: &RemoveSelected, window: &mut Window, cx: &mut Context<Self>) {
        self.picker.update(cx, |picker, cx| {
            let delegate = &mut picker.delegate;
            let Some(command_name) = delegate
                .selected_command()
                .filter(|command| command.usage.is_some())
                .map(|command| command.name.clone())
            else {
                return;
            };
            delegate
                .remove_command_history(command_name, window, cx)
                .detach();
        });
    }
}

impl EventEmitter<DismissEvent> for CommandPalette {}

impl Focusable for CommandPalette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("CommandPalette")
            .on_action(cx.listener(Self::remove_selected))
            .child(self.picker.clone())
    }
}

pub struct CommandPaletteDelegate {
    latest_query: String,
    command_palette: WeakEntity<CommandPalette>,
    workspace: WeakEntity<Workspace>,
    all_commands: Vec<Command>,
    commands: Vec<Command>,
    matches: Vec<StringMatch>,
    selected_ix: usize,
    previous_focus_handle: FocusHandle,
    updating_matches: Option<(
        Task<()>,
        postage::dispatch::Receiver<(Vec<Command>, Vec<StringMatch>, CommandInterceptResult)>,
    )>,
    query_history: QueryHistory,
}

struct Command {
    name: SharedString,
    action: Box<dyn Action>,
    usage: Option<CommandUsage>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CommandUsage {
    last_invoked: i64,
    invocations: u16,
}

#[derive(Default)]
struct QueryHistory {
    history: Option<VecDeque<String>>,
    cursor: Option<usize>,
    prefix: Option<String>,
}

impl QueryHistory {
    fn history(&mut self, cx: &App) -> &mut VecDeque<String> {
        self.history.get_or_insert_with(|| {
            CommandPaletteDB::global(cx)
                .list_recent_queries()
                .unwrap_or_default()
                .into_iter()
                .collect()
        })
    }

    fn add(&mut self, query: String, cx: &App) {
        if let Some(pos) = self.history(cx).iter().position(|h| h == &query) {
            self.history(cx).remove(pos);
        }
        self.history(cx).push_back(query);
        self.cursor = None;
        self.prefix = None;
    }

    fn validate_cursor(&mut self, current_query: &str, cx: &App) -> Option<usize> {
        if let Some(pos) = self.cursor {
            if self.history(cx).get(pos).map(|s| s.as_str()) != Some(current_query) {
                self.cursor = None;
                self.prefix = None;
            }
        }
        self.cursor
    }

    fn previous(&mut self, current_query: &str, cx: &App) -> Option<&str> {
        if self.validate_cursor(current_query, cx).is_none() {
            self.prefix = Some(current_query.to_string());
        }

        let prefix = self.prefix.clone().unwrap_or_default();
        let start_index = self.cursor.unwrap_or(self.history(cx).len());

        for i in (0..start_index).rev() {
            if self
                .history(cx)
                .get(i)
                .is_some_and(|e| e.starts_with(&prefix))
            {
                self.cursor = Some(i);
                return self.history(cx).get(i).map(|s| s.as_str());
            }
        }
        None
    }

    fn next(&mut self, current_query: &str, cx: &App) -> Option<&str> {
        let selected = self.validate_cursor(current_query, cx)?;
        let prefix = self.prefix.clone().unwrap_or_default();

        for i in (selected + 1)..self.history(cx).len() {
            if self
                .history(cx)
                .get(i)
                .is_some_and(|e| e.starts_with(&prefix))
            {
                self.cursor = Some(i);
                return self.history(cx).get(i).map(|s| s.as_str());
            }
        }
        None
    }

    fn reset_cursor(&mut self) {
        self.cursor = None;
        self.prefix = None;
    }

    fn is_navigating(&self) -> bool {
        self.cursor.is_some()
    }
}

impl Clone for Command {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            action: self.action.boxed_clone(),
            usage: self.usage,
        }
    }
}

impl CommandPaletteDelegate {
    fn new(
        command_palette: WeakEntity<CommandPalette>,
        workspace: WeakEntity<Workspace>,
        commands: Vec<Command>,
        previous_focus_handle: FocusHandle,
    ) -> Self {
        Self {
            command_palette,
            workspace,
            all_commands: commands.clone(),
            matches: Vec::new(),
            commands,
            selected_ix: 0,
            previous_focus_handle,
            latest_query: String::new(),
            updating_matches: None,
            query_history: QueryHistory::default(),
        }
    }

    fn matches_updated(
        &mut self,
        query: String,
        mut commands: Vec<Command>,
        mut matches: Vec<StringMatch>,
        intercept_result: CommandInterceptResult,
        _: &mut Context<Picker<Self>>,
    ) {
        self.updating_matches.take();
        self.latest_query = query;

        let mut new_matches = Vec::new();

        for CommandInterceptItem {
            action,
            string,
            positions,
        } in intercept_result.results
        {
            if let Some(idx) = matches
                .iter()
                .position(|m| commands[m.candidate_id].action.partial_eq(&*action))
            {
                matches.remove(idx);
            }
            let string = SharedString::from(string);
            commands.push(Command {
                name: string.clone(),
                action,
                usage: None,
            });
            new_matches.push(StringMatch {
                candidate_id: commands.len() - 1,
                string,
                positions,
                score: 0.0,
            })
        }
        if !intercept_result.exclusive {
            new_matches.append(&mut matches);
        }
        self.commands = commands;
        self.matches = new_matches;
        if self.matches.is_empty() {
            self.selected_ix = 0;
        } else {
            self.selected_ix = cmp::min(self.selected_ix, self.matches.len() - 1);
        }
    }

    /// Hit count for each command in the palette.
    /// We only account for commands triggered directly via command palette and not by e.g. keystrokes because
    /// if a user already knows a keystroke for a command, they are unlikely to use a command palette to look for it.
    fn command_usage(&self, cx: &App) -> HashMap<SharedString, CommandUsage> {
        if !CommandPaletteSettings::get_global(cx).use_command_history {
            return HashMap::new();
        }

        if let Ok(commands) = CommandPaletteDB::global(cx).list_commands_used() {
            commands
                .into_iter()
                .map(|command| {
                    (
                        SharedString::from(command.command_name),
                        CommandUsage {
                            last_invoked: command.last_invoked.unix_timestamp(),
                            invocations: command.invocations,
                        },
                    )
                })
                .collect()
        } else {
            HashMap::new()
        }
    }

    fn remove_command_history(
        &mut self,
        command_name: SharedString,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let db = CommandPaletteDB::global(cx);
        cx.spawn_in(window, async move |picker, cx| {
            if db
                .delete_command_history(command_name.to_string())
                .await
                .with_context(|| format!("removing {command_name:?} from command history"))
                .log_err()
                .is_none()
            {
                return;
            }
            picker
                .update_in(cx, |picker, window, cx| {
                    picker.delegate.query_history = QueryHistory::default();
                    picker.refresh(window, cx);
                })
                .ok();
        })
    }

    fn render_history_button(
        ix: usize,
        command_name: SharedString,
        cx: &mut Context<Picker<Self>>,
    ) -> impl IntoElement {
        h_flex()
            .id(("command-history", ix))
            .debug_selector(move || format!("command-history-{ix}"))
            .group("command-history-button")
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                cx.stop_propagation();
                window.prevent_default();
            })
            .on_mouse_up(MouseButton::Right, |_, window, cx| {
                cx.stop_propagation();
                window.prevent_default();
            })
            .child(
                ButtonLike::new(("remove-command-history", ix))
                    .aria_label("Remove from Command History")
                    .tooltip(Tooltip::for_action_title(
                        "Remove from Command History",
                        &RemoveSelected,
                    ))
                    .child(
                        h_flex()
                            .relative()
                            .child(
                                h_flex()
                                    .group_hover("command-history-button", |this| this.invisible())
                                    .child(
                                        Icon::new(IconName::HistoryRerun)
                                            .color(Color::Muted)
                                            .size(IconSize::Small),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .absolute()
                                    .inset_0()
                                    .visible_on_hover("command-history-button")
                                    .child(Icon::new(IconName::Close).size(IconSize::Small)),
                            ),
                    )
                    .on_click(cx.listener(move |picker, _, window, cx| {
                        window.prevent_default();
                        picker
                            .delegate
                            .remove_command_history(command_name.clone(), window, cx)
                            .detach();
                    })),
            )
    }

    fn selected_command(&self) -> Option<&Command> {
        if self.matches.is_empty() {
            return None;
        }
        let action_ix = self
            .matches
            .get(self.selected_ix)
            .map(|m| m.candidate_id)
            .unwrap_or(self.selected_ix);
        // this gets called in headless tests where there are no commands loaded
        // so we need to return an Option here
        self.commands.get(action_ix)
    }

    #[cfg(feature = "test-support")]
    pub fn seed_history(&mut self, queries: &[&str]) {
        self.query_history.history = Some(queries.iter().map(|s| s.to_string()).collect());
    }
}

impl PickerDelegate for CommandPaletteDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "command palette"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Execute a command...".into()
    }

    fn select_history(
        &mut self,
        direction: Direction,
        query: &str,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<String> {
        match direction {
            Direction::Up => {
                let should_use_history =
                    self.selected_ix == 0 || self.query_history.is_navigating();
                if should_use_history {
                    if let Some(query) = self
                        .query_history
                        .previous(query, cx)
                        .map(|s| s.to_string())
                    {
                        return Some(query);
                    }
                }
            }
            Direction::Down => {
                if self.query_history.is_navigating() {
                    if let Some(query) = self.query_history.next(query, cx).map(|s| s.to_string()) {
                        return Some(query);
                    } else {
                        let prefix = self.query_history.prefix.take().unwrap_or_default();
                        self.query_history.reset_cursor();
                        return Some(prefix);
                    }
                }
            }
        }
        None
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_ix
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) {
        self.selected_ix = ix;
    }

    fn update_matches(
        &mut self,
        mut query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> gpui::Task<()> {
        let settings = WorkspaceSettings::get_global(cx);
        if let Some(alias) = settings.command_aliases.get(&query) {
            query = alias.as_ref().to_owned();
        }

        let workspace = self.workspace.clone();

        let intercept_task = GlobalCommandPaletteInterceptor::intercept(&query, workspace, cx);

        let (mut tx, mut rx) = postage::dispatch::channel(1);

        let query_str = query.as_str();
        let is_zed_link = parse_zed_link(query_str, cx).is_some();

        let task = cx.background_spawn({
            let mut commands = self.all_commands.clone();
            let command_usage = self.command_usage(cx);
            let executor = cx.background_executor().clone();
            let query = normalize_action_query(query_str);
            let query_for_link = query_str.to_string();
            async move {
                for command in &mut commands {
                    command.usage = command_usage.get(&command.name).copied();
                }
                commands.sort_by_key(|command| (Reverse(command.usage), command.name.clone()));

                let candidates = commands
                    .iter()
                    .enumerate()
                    .map(|(ix, command)| StringMatchCandidate::new(ix, &command.name))
                    .collect::<Vec<_>>();

                let mut matches = fuzzy_nucleo::match_strings_async(
                    &candidates,
                    &query,
                    fuzzy_nucleo::Case::Smart,
                    fuzzy_nucleo::LengthPenalty::On,
                    10000,
                    &Default::default(),
                    executor,
                )
                .await;

                let used_commands = commands
                    .iter()
                    .take_while(|command| command.usage.is_some())
                    .count();
                matches.sort_by_key(|string_match| {
                    if string_match.candidate_id < used_commands {
                        string_match.candidate_id
                    } else {
                        usize::MAX
                    }
                });

                let intercept_result = if is_zed_link {
                    CommandInterceptResult {
                        results: vec![CommandInterceptItem {
                            action: OpenZedUrl {
                                url: query_for_link.clone().into(),
                            }
                            .boxed_clone(),
                            string: query_for_link,
                            positions: vec![],
                        }],
                        exclusive: false,
                    }
                } else if let Some(task) = intercept_task {
                    task.await
                } else {
                    CommandInterceptResult::default()
                };

                tx.send((commands, matches, intercept_result))
                    .await
                    .log_err();
            }
        });

        self.updating_matches = Some((task, rx.clone()));

        cx.spawn_in(window, async move |picker, cx| {
            let Some((commands, matches, intercept_result)) = rx.recv().await else {
                return;
            };

            picker
                .update(cx, |picker, cx| {
                    picker
                        .delegate
                        .matches_updated(query, commands, matches, intercept_result, cx)
                })
                .ok();
        })
    }

    fn finalize_update_matches(
        &mut self,
        query: String,
        duration: Duration,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> bool {
        let Some((task, rx)) = self.updating_matches.take() else {
            return true;
        };

        match cx
            .foreground_executor()
            .block_with_timeout(duration, rx.clone().recv())
        {
            Ok(Some((commands, matches, interceptor_result))) => {
                self.matches_updated(query, commands, matches, interceptor_result, cx);
                true
            }
            _ => {
                self.updating_matches = Some((task, rx));
                false
            }
        }
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.command_palette
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .ok();
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if secondary {
            if self.matches.is_empty() {
                return;
            }
            let Some(selected_command) = self.selected_command() else {
                return;
            };
            let action_name = selected_command.action.name();
            let open_keymap = Box::new(zed_actions::ChangeKeybinding {
                action: action_name.to_string(),
            });
            window.dispatch_action(open_keymap, cx);
            self.dismissed(window, cx);
            return;
        }

        if self.matches.is_empty() {
            self.dismissed(window, cx);
            return;
        }

        if !self.latest_query.is_empty() {
            self.query_history.add(self.latest_query.clone(), cx);
            self.query_history.reset_cursor();
        }

        let action_ix = self.matches[self.selected_ix].candidate_id;
        let command = self.commands.swap_remove(action_ix);

        self.matches.clear();
        self.commands.clear();
        let command_name = command.name.clone();
        let latest_query = self.latest_query.clone();
        let db = CommandPaletteDB::global(cx);
        cx.background_spawn(async move {
            db.write_command_invocation(command_name, latest_query)
                .await
        })
        .detach_and_log_err(cx);
        let action = command.action;
        window.focus(&self.previous_focus_handle, cx);
        self.dismissed(window, cx);
        window.dispatch_action(action, cx);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let matching_command = self.matches.get(ix)?;
        let command = self.commands.get(matching_command.candidate_id)?;

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .w_full()
                        .py_px()
                        .justify_between()
                        .gap_2()
                        .child(
                            HighlightedLabel::new(
                                command.name.clone(),
                                matching_command.positions.clone(),
                            )
                            .truncate(),
                        )
                        .child(
                            h_flex()
                                .flex_shrink_0()
                                .gap_2()
                                .child(KeyBinding::for_action_in(
                                    &*command.action,
                                    &self.previous_focus_handle,
                                    cx,
                                ))
                                .when(command.usage.is_some(), |this| {
                                    this.child(Self::render_history_button(
                                        ix,
                                        command.name.clone(),
                                        cx,
                                    ))
                                }),
                        ),
                ),
        )
    }

    fn render_footer(
        &self,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let selected_command = self.selected_command()?;
        let keybind =
            KeyBinding::for_action_in(&*selected_command.action, &self.previous_focus_handle, cx);

        let focus_handle = &self.previous_focus_handle;
        let keybinding_buttons = if keybind.has_binding(window) {
            Button::new("change", "Change Keybinding…")
                .key_binding(
                    KeyBinding::for_action_in(&menu::SecondaryConfirm, focus_handle, cx)
                        .map(|kb| kb.size(rems_from_px(12_f32))),
                )
                .on_click(move |_, window, cx| {
                    window.dispatch_action(menu::SecondaryConfirm.boxed_clone(), cx);
                })
        } else {
            Button::new("add", "Add Keybinding…")
                .key_binding(
                    KeyBinding::for_action_in(&menu::SecondaryConfirm, focus_handle, cx)
                        .map(|kb| kb.size(rems_from_px(12_f32))),
                )
                .on_click(move |_, window, cx| {
                    window.dispatch_action(menu::SecondaryConfirm.boxed_clone(), cx);
                })
        };

        Some(
            h_flex()
                .w_full()
                .p_1p5()
                .gap_1()
                .justify_end()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(keybinding_buttons)
                .child(
                    Button::new("run-action", "Run")
                        .key_binding(
                            KeyBinding::for_action_in(&menu::Confirm, &focus_handle, cx)
                                .map(|kb| kb.size(rems_from_px(12_f32))),
                        )
                        .on_click(|_, window, cx| {
                            window.dispatch_action(menu::Confirm.boxed_clone(), cx)
                        }),
                )
                .into_any(),
        )
    }
}

pub fn humanize_action_name(name: &str) -> String {
    let chars = name.chars().collect::<Vec<_>>();
    let capacity = name.len() + chars.iter().filter(|c| c.is_uppercase()).count();
    let mut result = String::with_capacity(capacity);
    let mut index = 0;

    while index < chars.len() {
        let char = chars[index];
        if char == ':' {
            if result.ends_with(':') {
                result.push(' ');
            } else {
                result.push(':');
            }
            index += 1;
        } else if char == '_' {
            result.push(' ');
            index += 1;
        } else if char.is_uppercase() {
            let start = index;
            index += 1;
            while chars
                .get(index)
                .is_some_and(|next_char| next_char.is_uppercase())
            {
                index += 1;
            }

            let uppercase_run = &chars[start..index];
            if uppercase_run.len() > 1 {
                let split_before_last = chars
                    .get(index)
                    .is_some_and(|next_char| next_char.is_lowercase());
                let acronym_end = if split_before_last {
                    uppercase_run.len() - 1
                } else {
                    uppercase_run.len()
                };

                if acronym_end > 0 {
                    if !result.ends_with(' ') {
                        result.push(' ');
                    }
                    result.extend(&uppercase_run[..acronym_end]);
                }

                if split_before_last {
                    if !result.ends_with(' ') {
                        result.push(' ');
                    }
                    result.extend(uppercase_run[acronym_end].to_lowercase());
                }
            } else {
                if !result.ends_with(' ') {
                    result.push(' ');
                }
                result.extend(char.to_lowercase());
            }
        } else {
            result.push(char);
            index += 1;
        }
    }

    result
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Command")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

