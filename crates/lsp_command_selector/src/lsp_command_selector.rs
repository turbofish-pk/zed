use std::{
    collections::BTreeSet,
    sync::{Arc, atomic::AtomicBool},
};

use editor::Editor;
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    ParentElement, Render, SharedString, Task, WeakEntity, Window, actions,
};
use language::Buffer;
use lsp::{LanguageServerId, LanguageServerName};
use picker::{Picker, PickerDelegate};
use project::{File, LspStore, Project};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use workspace::{ModalView, Workspace};

actions!(lsp_command_selector, [Toggle, ToggleArgumentsFocus]);

pub fn init(cx: &mut App) {
    cx.observe_new(LspCommandSelector::register).detach();
}

pub struct LspCommandSelector {
    picker: Entity<Picker<LspCommandSelectorDelegate>>,
}

impl LspCommandSelector {
    fn register(workspace: &mut Workspace, _: Option<&mut Window>, _: &mut Context<Workspace>) {
        workspace.register_action_renderer(|div, workspace, _, cx| {
            let commands_available = active_buffer(workspace, cx).is_some_and(|buffer| {
                available_commands(&buffer, workspace.project(), cx)
                    .next()
                    .is_some()
            });
            if !commands_available {
                return div;
            }
            div.on_action(cx.listener(|workspace, _: &Toggle, window, cx| {
                Self::toggle(workspace, window, cx);
            }))
        });
    }

    fn toggle(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let Some(buffer) = active_buffer(workspace, cx) else {
            return;
        };
        let project = workspace.project();
        let commands = available_commands(&buffer, project, cx).collect::<BTreeSet<_>>();
        let lsp_store = project.read(cx).lsp_store();
        workspace.toggle_modal(window, cx, move |window, cx| {
            LspCommandSelector::new(lsp_store, commands, window, cx)
        });
    }

    fn new(
        lsp_store: Entity<LspStore>,
        commands: BTreeSet<AvailableCommand>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let arguments_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(
                "JSON arguments (42, {\"key\": \"value\"}, …) or plain text for one string",
                window,
                cx,
            );
            editor
        });
        let delegate = LspCommandSelectorDelegate::new(
            cx.entity().downgrade(),
            lsp_store,
            arguments_editor,
            commands,
        );
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }

    fn toggle_arguments_focus(
        &mut self,
        _: &ToggleArgumentsFocus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let arguments_focus_handle = self
            .picker
            .read(cx)
            .delegate
            .arguments_editor
            .focus_handle(cx);
        if arguments_focus_handle.contains_focused(window, cx) {
            let picker_focus_handle = self.picker.focus_handle(cx);
            window.focus(&picker_focus_handle, cx);
        } else {
            window.focus(&arguments_focus_handle, cx);
        }
    }
}

impl Render for LspCommandSelector {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("LspCommandSelector")
            .on_action(cx.listener(Self::toggle_arguments_focus))
            .child(self.picker.clone())
    }
}

impl Focusable for LspCommandSelector {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for LspCommandSelector {}
impl ModalView for LspCommandSelector {}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct AvailableCommand {
    server_name: LanguageServerName,
    server_id: LanguageServerId,
    command: SharedString,
}

enum Execution {
    Idle,
    Running { _task: Task<()> },
    Failed(SharedString),
}

pub struct LspCommandSelectorDelegate {
    selector: WeakEntity<LspCommandSelector>,
    lsp_store: Entity<LspStore>,
    arguments_editor: Entity<Editor>,
    commands: Arc<[AvailableCommand]>,
    execution: Execution,
    candidates: Arc<[StringMatchCandidate]>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl LspCommandSelectorDelegate {
    fn new(
        selector: WeakEntity<LspCommandSelector>,
        lsp_store: Entity<LspStore>,
        arguments_editor: Entity<Editor>,
        commands: BTreeSet<AvailableCommand>,
    ) -> Self {
        let commands = commands.into_iter().collect::<Arc<[_]>>();
        let candidates = commands
            .iter()
            .enumerate()
            .map(|(index, command)| StringMatchCandidate::new(index, &command.command))
            .collect::<Arc<[_]>>();
        Self {
            selector,
            lsp_store,
            arguments_editor,
            commands,
            execution: Execution::Idle,
            candidates,
            matches: Vec::new(),
            selected_index: 0,
        }
    }

    fn is_executing(&self) -> bool {
        matches!(self.execution, Execution::Running { .. })
    }

    fn parse_arguments(&self, cx: &App) -> Vec<serde_json::Value> {
        let arguments_text = self.arguments_editor.read(cx).text(cx);
        let trimmed_arguments = arguments_text.trim();
        if trimmed_arguments.is_empty() {
            return Vec::new();
        }
        serde_json::Deserializer::from_str(trimmed_arguments)
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|_| vec![serde_json::Value::String(trimmed_arguments.to_owned())])
    }
}

impl PickerDelegate for LspCommandSelectorDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "lsp command selector"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Select a language server command…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn can_select(&self, ix: usize, _window: &mut Window, _cx: &mut Context<Picker<Self>>) -> bool {
        !self.is_executing() && self.matches.get(ix).is_some()
    }

    fn has_another_open_menu(&self, window: &Window, cx: &App) -> bool {
        self.arguments_editor
            .focus_handle(cx)
            .contains_focused(window, cx)
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if self.is_executing() {
            return;
        }
        let Some(mat) = self.matches.get(self.selected_index) else {
            return;
        };
        let Some(command) = self.commands.get(mat.candidate_id).cloned() else {
            return;
        };
        let arguments = self.parse_arguments(cx);
        let task = self.lsp_store.update(cx, |lsp_store, cx| {
            lsp_store.execute_lsp_command(
                command.server_id,
                command.command.to_string(),
                arguments,
                cx,
            )
        });
        self.arguments_editor
            .update(cx, |editor, _| editor.set_read_only(true));
        let task = cx.spawn_in(window, async move |picker, cx| {
            let execution = match task.await {
                Ok(result) => {
                    if let Some(result) = result {
                        log::info!("LSP command {} returned {result}", command.command);
                    }
                    Execution::Idle
                }
                Err(error) => {
                    let error = format!(
                        "Failed to execute LSP command {}: {error:#}",
                        command.command
                    );
                    log::error!("{error}");
                    Execution::Failed(SharedString::from(error))
                }
            };
            picker
                .update_in(cx, |picker, window, cx| {
                    let succeeded = matches!(execution, Execution::Idle);
                    picker.delegate.execution = execution;
                    picker
                        .delegate
                        .arguments_editor
                        .update(cx, |editor, _| editor.set_read_only(false));
                    cx.notify();
                    if succeeded {
                        picker.delegate.dismissed(window, cx);
                    }
                })
                .ok();
        });
        self.execution = Execution::Running { _task: task };
        cx.notify();
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selector.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let background = cx.background_executor().clone();
        let candidates = self.candidates.clone();
        cx.spawn_in(window, async move |picker, cx| {
            let matches = if query.is_empty() {
                candidates
                    .iter()
                    .enumerate()
                    .map(|(index, candidate)| StringMatch {
                        candidate_id: index,
                        string: candidate.string.clone(),
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    100,
                    &AtomicBool::new(false),
                    background,
                )
                .await
            };

            picker
                .update_in(cx, |picker, window, cx| {
                    picker.delegate.matches = matches;
                    picker.set_selected_index(0, None, false, window, cx);
                    cx.notify();
                })
                .ok();
        })
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let mat = self.matches.get(ix)?;
        let command = self.commands.get(mat.candidate_id)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .disabled(self.is_executing())
                .child(HighlightedLabel::new(
                    mat.string.clone(),
                    mat.positions.clone(),
                ))
                .end_slot(
                    Label::new(command.server_name.0.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
        )
    }

    fn render_footer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        Some(
            v_flex()
                .p_2()
                .gap_1()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    h_flex()
                        .gap_2()
                        .justify_between()
                        .child(
                            Label::new("Arguments")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            h_flex()
                                .gap_1()
                                .child(ui::KeyBinding::for_action_in(
                                    &ToggleArgumentsFocus,
                                    &self.arguments_editor.focus_handle(cx),
                                    cx,
                                ))
                                .child(
                                    Label::new("to switch focus")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        ),
                )
                .child(self.arguments_editor.clone())
                .map(|footer| match &self.execution {
                    Execution::Idle => footer,
                    Execution::Running { .. } => footer.child(
                        Label::new("Executing command…")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                    Execution::Failed(error) => footer.child(
                        Label::new(error.clone())
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    ),
                })
                .into_any_element(),
        )
    }
}

fn active_buffer(workspace: &Workspace, cx: &App) -> Option<Entity<Buffer>> {
    let editor = workspace.active_item(cx)?.act_as::<Editor>(cx)?;
    let editor = editor.read(cx);
    editor
        .buffer()
        .read(cx)
        .as_singleton()
        .or_else(|| editor.active_buffer(cx))
}

fn available_commands<'a>(
    buffer: &Entity<Buffer>,
    project: &Entity<Project>,
    cx: &'a App,
) -> impl Iterator<Item = AvailableCommand> + 'a {
    let project = project.read(cx);
    let lsp_store = project.lsp_store().read(cx);
    let buffer = buffer.read(cx);
    let adapters = buffer
        .language()
        .map(|language| project.languages().lsp_adapters(&language.name()))
        .unwrap_or_default();
    let worktree_owning_servers = File::from_dyn(buffer.file())
        .and_then(|file| project.worktree_for_id(file.worktree_id(cx), cx))
        .map(|worktree| worktree.read(cx))
        .filter(|worktree| worktree.is_visible())
        .map(|worktree| worktree.id());
    let opened_in_servers = lsp_store.language_server_ids_for_opened_buffer(buffer.remote_id());
    project
        .language_server_statuses(cx)
        .filter(move |(server_id, status)| {
            adapters.iter().any(|adapter| adapter.name() == status.name)
                && match (opened_in_servers, worktree_owning_servers, status.worktree) {
                    (Some(server_ids), _, _) => server_ids.contains(server_id),
                    (None, Some(buffer_worktree), Some(server_worktree)) => {
                        buffer_worktree == server_worktree
                    }
                    (None, _, _) => true,
                }
        })
        .filter_map(move |(server_id, status)| {
            let provider = lsp_store
                .lsp_server_capabilities
                .get(&server_id)?
                .execute_command_provider
                .as_ref()?;
            Some((server_id, status.name.clone(), &provider.commands))
        })
        .flat_map(|(server_id, server_name, commands)| {
            commands.iter().map(move |command| AvailableCommand {
                server_name: server_name.clone(),
                server_id,
                command: SharedString::from(command.clone()),
            })
        })
}

