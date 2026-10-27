use futures::future::join_all;
use itertools::Itertools;
use language::language_settings::LanguageSettings;
use text::BufferId;
use ui::{Context, Window};

use crate::{Editor, LSP_REQUEST_DEBOUNCE_TIMEOUT};

impl Editor {
    pub(super) fn refresh_folding_ranges(
        &mut self,
        for_buffer: Option<BufferId>,
        _window: &Window,
        cx: &mut Context<Self>,
    ) {
        if !self.lsp_data_enabled() || !self.use_document_folding_ranges {
            return;
        }
        let Some(project) = self.project.as_ref().map(|p| p.downgrade()) else {
            return;
        };

        let buffers_to_query = self
            .visible_buffers(cx)
            .into_iter()
            .filter(|buffer| self.is_lsp_relevant(buffer.read(cx).file(), cx))
            .chain(for_buffer.and_then(|id| self.buffer.read(cx).buffer(id)))
            .filter(|buffer| {
                let id = buffer.read(cx).remote_id();
                (for_buffer.is_none_or(|target| target == id))
                    && self.registered_buffers.contains_key(&id)
                    && LanguageSettings::for_buffer(buffer.read(cx), cx)
                        .document_folding_ranges
                        .enabled()
            })
            .unique_by(|buffer| buffer.read(cx).remote_id())
            .collect::<Vec<_>>();

        self.refresh_folding_ranges_task = cx.spawn(async move |editor, cx| {
            cx.background_executor()
                .timer(LSP_REQUEST_DEBOUNCE_TIMEOUT)
                .await;

            let Some(tasks) = editor
                .update(cx, |_, cx| {
                    let project = project.upgrade()?;
                    Some(project.read(cx).lsp_store().update(cx, |lsp_store, cx| {
                        buffers_to_query
                            .into_iter()
                            .map(|buffer| {
                                let buffer_id = buffer.read(cx).remote_id();
                                let task = lsp_store.fetch_folding_ranges(&buffer, cx);
                                async move { (buffer_id, task.await) }
                            })
                            .collect::<Vec<_>>()
                    }))
                })
                .ok()
                .flatten()
            else {
                return;
            };

            let results = join_all(tasks).await;
            if results.is_empty() {
                return;
            }

            editor
                .update(cx, |editor, cx| {
                    editor.display_map.update(cx, |display_map, cx| {
                        for (buffer_id, ranges) in results {
                            display_map.set_lsp_folding_ranges(buffer_id, ranges, cx);
                        }
                    });
                    cx.notify();
                })
                .ok();
        });
    }

    pub fn document_folding_ranges_enabled(&self, cx: &ui::App) -> bool {
        self.use_document_folding_ranges && self.display_map.read(cx).has_lsp_folding_ranges()
    }

    /// Removes LSP folding creases for buffers whose `lsp_folding_ranges`
    /// setting has been turned off, and triggers a refresh so newly-enabled
    /// buffers get their ranges fetched.
    pub(super) fn clear_disabled_lsp_folding_ranges(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.use_document_folding_ranges {
            return;
        }

        let buffers_to_clear = self
            .buffer
            .read(cx)
            .all_buffers()
            .into_iter()
            .filter(|buffer| {
                let buffer = buffer.read(cx);
                !LanguageSettings::for_buffer(&buffer, cx)
                    .document_folding_ranges
                    .enabled()
            })
            .map(|buffer| buffer.read(cx).remote_id())
            .collect::<Vec<_>>();

        if !buffers_to_clear.is_empty() {
            self.display_map.update(cx, |display_map, cx| {
                for buffer_id in buffers_to_clear {
                    display_map.clear_lsp_folding_ranges(buffer_id, cx);
                }
            });
            cx.notify();
        }

        self.refresh_folding_ranges(None, window, cx);
    }
}

