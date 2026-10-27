use std::sync::Arc;

use db::kvp::KeyValueStore;
use editor::Editor;
use extension_host::{ExtensionSettings, ExtensionStore};
use extension_suggest::SuggestedExtension;
use gpui::{App, AppContext as _, Context, Entity, SharedString};
use language::{Buffer, PLAIN_TEXT};
use markdown::{Markdown, MarkdownElement};
use project::lsp_store::LspStoreEvent;
use settings::Settings as _;
use ui::prelude::*;
use util::ResultExt;
use workspace::notifications::{
    NotificationId, markdown_style, simple_message_notification::MessageNotification,
};
use workspace::{AppState, Event as WorkspaceEvent, Workspace};

struct ExtensionSuggestionNotification;

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        if window.is_none() {
            return;
        }
        let lsp_store = workspace.project().read(cx).lsp_store();
        cx.subscribe(&lsp_store, |workspace, _, event, cx| {
            if let LspStoreEvent::LanguageDetected { buffer, .. } = event {
                suggest_for_buffer(workspace, buffer.clone(), cx);
            }
        })
        .detach();
        cx.subscribe_self(|workspace, event, cx| {
            if let WorkspaceEvent::ItemAdded { item } = event
                && let Some(editor) = item.downcast::<Editor>()
                && let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton()
            {
                suggest_for_buffer(workspace, buffer, cx);
            }
        })
        .detach();
        cx.subscribe(
            &ExtensionStore::global(cx),
            |workspace, extension_store, event, cx| {
                if let extension_host::Event::ExtensionsUpdated = event {
                    let installed = extension_store
                        .read(cx)
                        .installed_extensions()
                        .keys()
                        .map(|extension_id| notification_id(extension_id))
                        .collect::<Vec<_>>();
                    for installed in installed {
                        workspace.dismiss_notification(&installed, cx);
                    }
                }
            },
        )
        .detach();
    })
    .detach();
}

fn language_extension_key(extension_id: &str) -> String {
    format!("{extension_id}_extension_suggest")
}

fn suggestion_dismissed(extension_id: &str, cx: &App) -> bool {
    KeyValueStore::global(cx)
        .read_kvp(&language_extension_key(extension_id))
        .log_err()
        != Some(None)
}

fn dismiss_suggestion(extension_id: &str, cx: &mut App) {
    let key = language_extension_key(extension_id);
    let kvp = KeyValueStore::global(cx);
    db::write_and_log(cx, move || async move {
        kvp.write_kvp(key, "dismissed".to_string()).await
    });

    let notification_id = notification_id(extension_id);
    let workspaces = AppState::global(cx)
        .workspace_store
        .read(cx)
        .workspaces()
        .cloned()
        .collect::<Vec<_>>();
    for workspace in workspaces {
        workspace
            .update(cx, |workspace, cx| {
                workspace.dismiss_notification(&notification_id, cx);
            })
            .ok();
    }
}

fn notification_id(extension_id: &str) -> NotificationId {
    NotificationId::composite::<ExtensionSuggestionNotification>(SharedString::from(extension_id))
}

fn suggest_for_buffer(
    workspace: &mut Workspace,
    buffer: Entity<Buffer>,
    cx: &mut Context<Workspace>,
) {
    if !is_active_in_some_pane(workspace, &buffer, cx) {
        return;
    }

    let buffer = buffer.read(cx);
    let Some(file) = buffer.file().cloned() else {
        return;
    };
    let language_name = buffer
        .language()
        .filter(|language| **language != *PLAIN_TEXT)
        .map(|language| language.name());

    match language_name {
        Some(language_name) => {
            let Some(suggestion) =
                extension_suggest::additional_suggestion_for_language(language_name.as_ref())
            else {
                return;
            };
            show_suggestion(workspace, suggestion.extension_id, cx, |cx| {
                let markdown = cx.new(|cx| {
                    Markdown::new(
                        SharedString::new_static(suggestion.description),
                        None,
                        None,
                        cx,
                    )
                });
                MessageNotification::new_from_builder(cx, move |window, cx| {
                    MarkdownElement::new(markdown.clone(), markdown_style(window, cx))
                        .text_size(TextSize::Default.rems(cx))
                        .into_any_element()
                })
                .with_title(suggestion.title)
                .more_info_message("Learn more")
                .more_info_url(suggestion.docs_url)
                .primary_message(suggestion.install_message)
                .secondary_message("Don't show again")
            });
        }
        None => {
            let language_exists = workspace
                .project()
                .read(cx)
                .languages()
                .language_for_file(&file, Some(buffer.as_rope()), cx)
                .is_some();
            if language_exists {
                return;
            }
            let Some(SuggestedExtension {
                extension_id,
                file_name_or_extension,
            }) = extension_suggest::suggest_extension(file.path())
            else {
                return;
            };
            show_suggestion(workspace, &extension_id, cx, |cx| {
                MessageNotification::new(
                    format!(
                        "Do you want to install the recommended '{extension_id}' extension for '{file_name_or_extension}' files?"
                    ),
                    cx,
                )
                .primary_message("Yes, install extension")
                .secondary_message("No, don't install it")
            });
        }
    }
}

fn is_active_in_some_pane(workspace: &Workspace, buffer: &Entity<Buffer>, cx: &App) -> bool {
    workspace.panes().iter().any(|pane| {
        pane.read(cx)
            .active_item()
            .and_then(|item| item.downcast::<Editor>())
            .is_some_and(|editor| {
                editor.read(cx).buffer().read(cx).as_singleton().as_ref() == Some(buffer)
            })
    })
}

fn show_suggestion(
    workspace: &mut Workspace,
    extension_id: &str,
    cx: &mut Context<Workspace>,
    build_notification: impl FnOnce(&mut Context<MessageNotification>) -> MessageNotification,
) {
    let notification_id = notification_id(extension_id);
    if workspace.has_notification(&notification_id) {
        return;
    }
    if !ExtensionSettings::get_global(cx).suggest_extensions {
        return;
    }

    let extension_store = ExtensionStore::global(cx);
    let extension_store = extension_store.read(cx);
    if extension_store
        .installed_extensions()
        .contains_key(extension_id)
        || extension_store
            .outstanding_operations()
            .contains_key(extension_id)
        || ExtensionSettings::get_global(cx)
            .auto_install_extensions
            .get(extension_id)
            == Some(&true)
        || suggestion_dismissed(extension_id, cx)
    {
        return;
    }

    let extension_id = Arc::<str>::from(extension_id);
    workspace.show_notification(notification_id, cx, |cx| {
        cx.new(|cx| {
            build_notification(cx)
                .primary_icon(IconName::Check)
                .primary_icon_color(Color::Success)
                .primary_on_click({
                    let extension_id = extension_id.clone();
                    move |_window, cx| {
                        ExtensionStore::global(cx).update(cx, |store, cx| {
                            store.install_latest_extension(extension_id.clone(), cx);
                        });
                    }
                })
                .secondary_icon(IconName::Close)
                .secondary_icon_color(Color::Error)
                .secondary_on_click(move |_window, cx| dismiss_suggestion(&extension_id, cx))
                .secondary_menu_entry("Disable Suggestions", |_window, cx| {
                    disable_extension_suggestions(cx)
                })
        })
    });
}

fn disable_extension_suggestions(cx: &mut App) {
    let fs = AppState::global(cx).fs.clone();
    settings::update_settings_file(fs, cx, |content, _| {
        content.extension.suggest_extensions = false;
    });
}

