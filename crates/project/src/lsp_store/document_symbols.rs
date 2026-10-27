use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use collections::{HashMap, HashSet};
use futures::FutureExt as _;
use futures::future::Shared;
use gpui::{AppContext as _, Context, Entity, Task};
use itertools::Itertools;
use language::{Buffer, BufferSnapshot, OutlineItem};
use lsp::LanguageServerId;
use rpc::proto;
use text::{Anchor, Bias, PointUtf16};
use util::ResultExt;

use crate::DocumentSymbol;
use crate::lsp_command::GetDocumentSymbols;
use crate::lsp_store::{
    LspStore, LspStoreEvent, RunningFetch, missing_servers_to_query, next_lsp_fetch_id,
};

pub(super) type DocumentSymbolsTask =
    Shared<Task<std::result::Result<Vec<OutlineItem<Anchor>>, Arc<anyhow::Error>>>>;

#[derive(Debug, Default)]
pub(super) struct DocumentSymbolsData {
    symbols: HashMap<LanguageServerId, Vec<OutlineItem<Anchor>>>,
    fetched_servers: HashSet<LanguageServerId>,
    symbols_update: Option<RunningFetch<DocumentSymbolsTask>>,
}

impl DocumentSymbolsData {
    pub(super) fn remove_server_data(&mut self, for_server: LanguageServerId) {
        self.symbols.remove(&for_server);
        self.fetched_servers.remove(&for_server);
        RunningFetch::discard_if_queried(&mut self.symbols_update, for_server);
    }

    fn evict(&mut self, for_server: Option<LanguageServerId>) {
        match for_server {
            Some(server_id) => self.remove_server_data(server_id),
            None => {
                self.symbols.clear();
                self.fetched_servers.clear();
                self.symbols_update = None;
            }
        }
    }
}

impl LspStore {
    pub(super) fn refresh_document_symbols(
        &mut self,
        for_server: Option<LanguageServerId>,
        cx: &mut Context<Self>,
    ) {
        for lsp_data in self.lsp_data.values_mut() {
            if let Some(document_symbols) = &mut lsp_data.document_symbols {
                document_symbols.evict(for_server);
            }
        }

        cx.emit(LspStoreEvent::RefreshDocumentSymbols {
            server_id: for_server,
        });
        if let Some((downstream_client, project_id)) = self.downstream_client.as_ref() {
            downstream_client
                .send(proto::RefreshDocumentSymbols {
                    project_id: *project_id,
                    server_id: for_server.map(|server_id| server_id.to_proto()),
                })
                .context("sending refresh document symbols downstream")
                .log_err();
        }
    }

    /// Returns a task that resolves to the document symbol outline items for
    /// the given buffer.
    ///
    /// Caches results per buffer version so repeated calls for the same version
    /// return immediately. Deduplicates concurrent in-flight requests.
    ///
    /// The returned items contain text and ranges but no syntax highlights.
    /// Callers (e.g. the editor) are responsible for applying highlights
    /// via the buffer's tree-sitter data and the active theme.
    pub fn fetch_document_symbols(
        &mut self,
        buffer: &Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> Task<Vec<OutlineItem<Anchor>>> {
        let version_queried_for = buffer.read(cx).version();
        let buffer_id = buffer.read(cx).remote_id();

        let current_servers = self.language_server_ids_for_request(buffer, &GetDocumentSymbols, cx);

        let mut servers_to_query = None;
        if let Some(lsp_data) = self.current_lsp_data(buffer_id) {
            if !version_queried_for.changed_since(&lsp_data.buffer_version)
                && let Some(cached) = &mut lsp_data.document_symbols
            {
                match missing_servers_to_query(
                    &mut cached.symbols,
                    &mut cached.fetched_servers,
                    &current_servers,
                ) {
                    Some(missing_servers) => servers_to_query = Some(missing_servers),
                    None => {
                        let snapshot = buffer.read(cx).snapshot();
                        return Task::ready(
                            cached
                                .symbols
                                .values()
                                .flatten()
                                .unique()
                                .cloned()
                                .sorted_by(|a, b| a.range.start.cmp(&b.range.start, &snapshot))
                                .collect(),
                        );
                    }
                }
            }
            if let Some(document_symbols) = &lsp_data.document_symbols
                && let Some(running) = &document_symbols.symbols_update
                && !version_queried_for.changed_since(&running.version)
                && servers_to_query
                    .as_ref()
                    .is_none_or(|missing| missing.is_subset(&running.servers))
            {
                let running = running.task.clone();
                return cx
                    .background_spawn(async move { running.await.log_err().unwrap_or_default() });
            }
        }

        let doc_symbols_data = self
            .latest_lsp_data(buffer, cx)
            .document_symbols
            .get_or_insert_default();
        let fetch_id = next_lsp_fetch_id();
        let queried_servers = servers_to_query
            .clone()
            .unwrap_or_else(|| current_servers.clone());
        let buffer = buffer.clone();
        let query_version = version_queried_for.clone();
        let new_task = cx
            .spawn({
                let queried_servers = queried_servers.clone();
                async move |lsp_store, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(30))
                        .await;

                    let fetched = lsp_store
                        .update(cx, |lsp_store, cx| {
                            lsp_store.fetch_document_symbols_for_buffer(
                                &buffer,
                                servers_to_query,
                                cx,
                            )
                        })
                        .map_err(Arc::new)?
                        .await
                        .context("fetching document symbols")
                        .map_err(Arc::new);

                    let fetched = match fetched {
                        Ok(fetched) => fetched,
                        Err(e) => {
                            lsp_store
                                .update(cx, |lsp_store, _| {
                                    if let Some(lsp_data) = lsp_store.lsp_data.get_mut(&buffer_id)
                                        && let Some(document_symbols) =
                                            &mut lsp_data.document_symbols
                                    {
                                        RunningFetch::take_finished(
                                            &mut document_symbols.symbols_update,
                                            fetch_id,
                                        );
                                    }
                                })
                                .ok();
                            return Err(e);
                        }
                    };

                    lsp_store
                        .update(cx, |lsp_store, cx| {
                            let snapshot = buffer.read(cx).snapshot();
                            let lsp_data = lsp_store.latest_lsp_data(&buffer, cx);
                            let doc_symbols = lsp_data.document_symbols.get_or_insert_default();

                            if RunningFetch::take_finished(
                                &mut doc_symbols.symbols_update,
                                fetch_id,
                            ) && let Some(fetched_symbols) = fetched
                            {
                                let converted = fetched_symbols
                                    .iter()
                                    .map(|(&server_id, symbols)| {
                                        let mut items = Vec::new();
                                        flatten_document_symbols(symbols, &snapshot, 0, &mut items);
                                        (server_id, items)
                                    })
                                    .collect();
                                if lsp_data.buffer_version == query_version {
                                    doc_symbols.symbols.extend(converted);
                                    doc_symbols.fetched_servers.extend(queried_servers);
                                } else if !lsp_data.buffer_version.changed_since(&query_version) {
                                    lsp_data.buffer_version = query_version;
                                    doc_symbols.symbols = converted;
                                    doc_symbols.fetched_servers = queried_servers;
                                }
                            }
                            doc_symbols
                                .symbols
                                .values()
                                .flatten()
                                .unique()
                                .cloned()
                                .sorted_by(|a, b| a.range.start.cmp(&b.range.start, &snapshot))
                                .collect()
                        })
                        .map_err(Arc::new)
                }
            })
            .shared();

        doc_symbols_data.symbols_update = Some(RunningFetch {
            id: fetch_id,
            version: version_queried_for,
            servers: queried_servers,
            task: new_task.clone(),
        });

        cx.background_spawn(async move { new_task.await.log_err().unwrap_or_default() })
    }

    fn fetch_document_symbols_for_buffer(
        &mut self,
        buffer: &Entity<Buffer>,
        for_servers: Option<HashSet<LanguageServerId>>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<Option<HashMap<LanguageServerId, Vec<DocumentSymbol>>>>> {
        let symbols_task = self.request_filtered_lsp_locally(
            buffer,
            None::<usize>,
            GetDocumentSymbols,
            for_servers.as_ref(),
            cx,
        );
        cx.background_spawn(async move { Ok(Some(symbols_task.await.into_iter().collect())) })
    }
}

fn flatten_document_symbols(
    symbols: &[DocumentSymbol],
    snapshot: &BufferSnapshot,
    depth: usize,
    output: &mut Vec<OutlineItem<Anchor>>,
) {
    for symbol in symbols {
        let name = super::collapse_newlines(&symbol.name, " ");

        let start = snapshot.clip_point_utf16(symbol.range.start, Bias::Right);
        let end = snapshot.clip_point_utf16(symbol.range.end, Bias::Left);
        let selection_start = snapshot.clip_point_utf16(symbol.selection_range.start, Bias::Right);
        let selection_end = snapshot.clip_point_utf16(symbol.selection_range.end, Bias::Left);

        let range = snapshot.anchor_after(start)..snapshot.anchor_before(end);
        let selection_range =
            snapshot.anchor_after(selection_start)..snapshot.anchor_before(selection_end);

        let (text, name_ranges, source_range_for_text) =
            enriched_symbol_text(&name, start, selection_start, selection_end, snapshot)
                .unwrap_or_else(|| {
                    let name_len = name.len();
                    (name.clone(), vec![0..name_len], selection_range.clone())
                });

        output.push(OutlineItem {
            depth,
            range,
            selection_range: selection_range.clone(),
            source_range_for_text,
            text: text.into(),
            highlight_ranges: Vec::new(),
            name_ranges,
            body_range: None,
            annotation_range: None,
        });

        if !symbol.children.is_empty() {
            flatten_document_symbols(&symbol.children, snapshot, depth + 1, output);
        }
    }
}

/// Tries to build an enriched label by including buffer text from the symbol
/// range start to the selection range end (e.g., "struct Foo" instead of just "Foo").
/// Only uses same-line prefix to avoid pulling in attributes/decorators.
fn enriched_symbol_text(
    name: &str,
    range_start: PointUtf16,
    selection_start: PointUtf16,
    selection_end: PointUtf16,
    snapshot: &BufferSnapshot,
) -> Option<(String, Vec<Range<usize>>, Range<Anchor>)> {
    let text_start = if range_start.row == selection_start.row {
        range_start
    } else {
        PointUtf16::new(selection_start.row, 0)
    };

    let start_offset = snapshot.point_utf16_to_offset(text_start);
    let end_offset = snapshot.point_utf16_to_offset(selection_end);
    if start_offset >= end_offset {
        return None;
    }

    let raw: String = snapshot.text_for_range(start_offset..end_offset).collect();
    let trimmed = raw.trim_start();
    if trimmed.len() <= name.len() || !trimmed.ends_with(name) {
        return None;
    }

    let name_start = trimmed.len() - name.len();
    let leading_ws = raw.len() - trimmed.len();
    let adjusted_start = start_offset + leading_ws;

    Some((
        trimmed.to_string(),
        vec![name_start..trimmed.len()],
        snapshot.anchor_after(adjusted_start)..snapshot.anchor_before(end_offset),
    ))
}

