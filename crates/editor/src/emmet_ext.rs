use std::ops::Range;
use std::time::Duration;

use futures::future::join_all;
use gpui::{App, AsyncWindowContext, Context, Entity, Global, SharedString, Task, Window};
use language::{Buffer, Language};
use lsp::LanguageServerId;
use multi_buffer::{
    Anchor, MultiBufferOffset, MultiBufferRow, MultiBufferSnapshot, ToOffset, ToPoint,
};
use project::Project;
use project::lsp_store::emmet_ext::{EMMET_SERVER_NAME, ExpandAbbreviation};
use snippet::Snippet;
use text::Point;
use util::ResultExt as _;

use crate::scroll::Autoscroll;
use crate::{
    Editor, InlineInputPreview, SelectionEffects, WrapWithAbbreviation, element::register_action,
};

const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(150);
const MAX_HISTORY_ENTRIES: usize = 20;

const ELEMENT_NODE_KINDS: [&str; 5] = [
    "element",
    "jsx_element",
    "jsx_self_closing_element",
    "script_element",
    "style_element",
];

const TAG_NODE_KINDS: [&str; 5] = [
    "start_tag",
    "end_tag",
    "self_closing_tag",
    "jsx_opening_element",
    "jsx_closing_element",
];

const STYLESHEET_LANGUAGE_IDS: [&str; 6] = ["css", "sass", "scss", "less", "sss", "stylus"];

#[derive(Clone, Copy, Default)]
struct WrapFilters {
    trim: bool,
    comment: bool,
    bem: bool,
}

#[derive(Default)]
struct WrapAbbreviationHistory(Vec<SharedString>);

impl Global for WrapAbbreviationHistory {}

pub fn apply_related_actions(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    let editor_ref = editor.read(cx);
    let Some(project) = editor_ref.project() else {
        return;
    };
    let project = project.read(cx);
    if !project
        .language_server_statuses(cx)
        .any(|(_, status)| status.name == EMMET_SERVER_NAME)
    {
        return;
    }
    let supports_wrap = editor_ref
        .buffer()
        .read(cx)
        .all_buffers_iter()
        .any(|buffer| {
            buffer
                .read(cx)
                .language()
                .is_some_and(|language| language_supports_wrap(language, project))
        });
    if supports_wrap {
        register_action(editor, window, wrap_with_abbreviation);
    }
}

pub fn wrap_with_abbreviation(
    editor: &mut Editor,
    _: &WrapWithAbbreviation,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    if editor.read_only(cx) {
        return;
    }

    let selections = editor.selections.disjoint_anchors_arc();
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let mut offset_ranges = Vec::<Range<MultiBufferOffset>>::new();
    for selection in selections.iter() {
        let mut range = selection.start.to_offset(&snapshot)..selection.end.to_offset(&snapshot);
        if range.start == range.end {
            range = enclosing_element_range(range.start, &snapshot).unwrap_or_else(|| {
                let row = MultiBufferRow(selection.start.to_point(&snapshot).row);
                Point::new(row.0, 0).to_offset(&snapshot)
                    ..Point::new(row.0, snapshot.line_len(row)).to_offset(&snapshot)
            });
        } else {
            range = expand_over_partial_tags(range, &snapshot);
        }
        offset_ranges.push(trim_whitespace(range, &snapshot));
    }
    offset_ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged_ranges = Vec::<Range<MultiBufferOffset>>::with_capacity(offset_ranges.len());
    for range in offset_ranges {
        match merged_ranges.last_mut() {
            Some(last) if range.start < last.end || range.start == last.start => {
                last.end = last.end.max(range.end);
            }
            _ => merged_ranges.push(range),
        }
    }
    let ranges = merged_ranges
        .into_iter()
        .map(|range| snapshot.anchor_before(range.start)..snapshot.anchor_after(range.end))
        .collect::<Vec<_>>();
    if ranges.is_empty() {
        return;
    }
    if !ranges.iter().any(|range| {
        build_wrap_target(editor, range, WrapFilters::default(), &snapshot, cx).is_some()
    }) {
        log_unavailable();
        return;
    }

    editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
        selections.select_anchor_ranges(ranges.iter().cloned())
    });
    let newest = editor.selections.newest_anchor();
    let start_column = newest.start.to_point(&snapshot).column;
    let end_row = MultiBufferRow(newest.end.to_point(&snapshot).row);
    let position = snapshot.anchor_before(Point::new(
        end_row.0,
        start_column.min(snapshot.line_len(end_row)),
    ));
    let preview_language = snapshot.language_at(position).cloned();
    let history = cx
        .try_global::<WrapAbbreviationHistory>()
        .map(|history| history.0.clone())
        .unwrap_or_default();
    let confirm_ranges = ranges.clone();
    let preview_ranges = ranges;
    editor.show_inline_input(
        "Emmet abbreviation, e.g. ul>li*",
        position,
        preview_language,
        history,
        move |editor, text, window, cx| {
            wrap_targets_in_expanded_abbreviation(editor, &confirm_ranges, text, window, cx)
        },
        move |editor, text, window, cx| {
            update_expansion_preview(editor, &preview_ranges, text, window, cx)
        },
        window,
        cx,
    );
}

struct WrapTarget {
    range: Range<Anchor>,
    text: Option<Vec<String>>,
    buffer: Entity<Buffer>,
    server_id: LanguageServerId,
    language: String,
    indent: String,
    base_indent: String,
}

fn language_supports_wrap(language: &Language, project: &Project) -> bool {
    project
        .languages()
        .lsp_adapter(&language.name(), &EMMET_SERVER_NAME)
        .is_some_and(|adapter| {
            !STYLESHEET_LANGUAGE_IDS.contains(&adapter.language_id(&language.name()).as_str())
        })
}

fn build_wrap_target(
    editor: &Editor,
    range: &Range<Anchor>,
    filters: WrapFilters,
    snapshot: &MultiBufferSnapshot,
    cx: &App,
) -> Option<WrapTarget> {
    let project = editor.project.as_ref()?.read(cx);
    let multi_buffer = editor.buffer().read(cx);
    let (buffer, start) = multi_buffer.text_anchor_for_position(range.start, cx)?;
    let (end_buffer, _) = multi_buffer.text_anchor_for_position(range.end, cx)?;
    if buffer != end_buffer {
        return None;
    }
    let server_id = project.language_server_id_for_name(buffer.read(cx), &EMMET_SERVER_NAME, cx)?;
    let language = buffer
        .read(cx)
        .language_at(start)
        .or_else(|| buffer.read(cx).language().cloned())?;
    let adapter = project
        .lsp_store()
        .read(cx)
        .language_server_adapter_for_id(server_id)?;
    let language = adapter.language_id(&language.name());
    if STYLESHEET_LANGUAGE_IDS.contains(&language.as_str()) {
        return None;
    }
    let text = snapshot.text_for_range(range.clone()).collect::<String>();
    let settings = snapshot.language_settings_at(range.start, cx);
    let indent = if settings.hard_tabs {
        "\t".to_string()
    } else {
        " ".repeat(settings.tab_size.get() as usize)
    };
    let base_indent = base_indent_for(range.start, snapshot);
    Some(WrapTarget {
        text: selection_lines(&text, &base_indent, filters.trim),
        range: range.clone(),
        buffer,
        server_id,
        language,
        indent,
        base_indent,
    })
}

fn selection_lines(text: &str, base_indent: &str, trim_markers: bool) -> Option<Vec<String>> {
    if text.is_empty() {
        return None;
    }
    let lines = text
        .split('\n')
        .enumerate()
        .map(|(ix, line)| {
            let line = line.trim_end();
            let line = if ix == 0 {
                line
            } else {
                line.strip_prefix(base_indent).unwrap_or(line)
            };
            let line = if trim_markers {
                trim_list_marker(line)
            } else {
                line
            };
            line.replace('\\', "\\\\").replace('$', "\\$")
        })
        .collect::<Vec<_>>();
    Some(lines)
}

fn trim_list_marker(line: &str) -> &str {
    let trimmed = line.trim_start_matches([' ', '\t', '\u{a0}']);
    let marker_len = trimmed
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || matches!(ch, '#' | '-' | '*' | '•'))
        .map(char::len_utf8)
        .sum::<usize>();
    if marker_len == 0 {
        return line.trim();
    }
    let rest = &trimmed[marker_len..];
    let rest = rest.strip_prefix(['.', ')']).unwrap_or(rest);
    rest.trim()
}

fn parse_abbreviation(input: &str) -> Option<(String, WrapFilters)> {
    let mut abbreviation = input.trim();
    let mut filters = WrapFilters::default();
    loop {
        if let Some(stripped) = abbreviation.strip_suffix("|t") {
            filters.trim = true;
            abbreviation = stripped.trim_end();
        } else if let Some(stripped) = abbreviation.strip_suffix("|c") {
            filters.comment = true;
            abbreviation = stripped.trim_end();
        } else if let Some(stripped) = abbreviation.strip_suffix("|bem") {
            filters.bem = true;
            abbreviation = stripped.trim_end();
        } else {
            break;
        }
    }
    if abbreviation.is_empty() {
        return None;
    }
    Some((abbreviation.to_string(), filters))
}

fn wrap_targets_in_expanded_abbreviation(
    editor: &mut Editor,
    ranges: &[Range<Anchor>],
    text: String,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Option<Task<()>> {
    let input = text.trim().to_string();
    let (abbreviation, filters) = parse_abbreviation(&input)?;
    let project = editor.project.clone()?;

    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let targets = ranges
        .iter()
        .filter_map(|range| build_wrap_target(editor, range, filters, &snapshot, cx))
        .collect::<Vec<_>>();
    if targets.is_empty() {
        log_unavailable();
        editor.take_inline_input(window, cx);
        return None;
    }

    Some(cx.spawn_in(window, async move |editor, cx| {
        let requests = targets
            .into_iter()
            .map(|target| {
                let range = target.range.clone();
                let request =
                    request_expansion(&project, abbreviation.clone(), filters, target, cx);
                async move { anyhow::Ok((range, request.await?)) }
            })
            .collect::<Vec<_>>();
        let mut expansions = Vec::new();
        let mut errors = Vec::new();
        for result in join_all(requests).await {
            match result {
                Ok((range, Some(expansion))) => expansions.push((range, expansion)),
                Ok((_, None)) => {}
                Err(error) => errors.push(error),
            }
        }

        editor
            .update_in(cx, |editor, window, cx| {
                for error in &errors {
                    log::error!("{error:#}");
                }
                if expansions.is_empty() {
                    let message = match errors.first() {
                        Some(error) => error.root_cause().to_string(),
                        None => no_expansion_message(&abbreviation),
                    };
                    editor.set_inline_input_preview(Some(InlineInputPreview::Error(message)), cx);
                    return anyhow::Ok(());
                }
                remember_abbreviation(SharedString::from(input), cx);
                if let Some(mut state) = editor.take_inline_input(window, cx)
                    && let Some(confirm_task) = state.take_confirm_task()
                {
                    confirm_task.detach();
                }
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                if let [(range, expansion)] = expansions.as_slice() {
                    if let Ok(snippet) = Snippet::parse(expansion) {
                        let range =
                            range.start.to_offset(&snapshot)..range.end.to_offset(&snapshot);
                        editor.insert_snippet_with_autoindent(
                            &[range],
                            snippet,
                            None,
                            window,
                            cx,
                        )?;
                        return anyhow::Ok(());
                    }
                }
                let edits = expansions
                    .into_iter()
                    .map(|(range, expansion)| (range, expansion_text(&expansion)))
                    .collect::<Vec<_>>();
                editor.transact(window, cx, |editor, _, cx| {
                    editor.buffer().update(cx, |buffer, cx| {
                        buffer.edit(edits, None, cx);
                    });
                });
                editor.request_autoscroll(Autoscroll::fit(), cx);
                anyhow::Ok(())
            })
            .and_then(|result| result)
            .log_err();
    }))
}

fn update_expansion_preview(
    editor: &mut Editor,
    ranges: &[Range<Anchor>],
    text: String,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Option<Task<()>> {
    let Some((abbreviation, filters)) = parse_abbreviation(&text) else {
        editor.set_inline_input_preview(None, cx);
        return None;
    };
    let Some(project) = editor.project.clone() else {
        editor.set_inline_input_preview(None, cx);
        return None;
    };
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let Some(mut target) = ranges
        .iter()
        .find_map(|range| build_wrap_target(editor, range, filters, &snapshot, cx))
    else {
        editor.set_inline_input_preview(None, cx);
        return None;
    };
    target.base_indent.clear();

    Some(cx.spawn_in(window, async move |editor, cx| {
        cx.background_executor().timer(PREVIEW_DEBOUNCE).await;
        let result = request_expansion(&project, abbreviation.clone(), filters, target, cx).await;
        editor
            .update(cx, |editor, cx| {
                let preview = match result {
                    Ok(Some(expansion)) => InlineInputPreview::Text(expansion_text(&expansion)),
                    Ok(None) => InlineInputPreview::Error(no_expansion_message(&abbreviation)),
                    Err(error) => InlineInputPreview::Error(error.root_cause().to_string()),
                };
                editor.set_inline_input_preview(Some(preview), cx);
            })
            .ok();
    }))
}

fn request_expansion(
    project: &Entity<Project>,
    abbreviation: String,
    filters: WrapFilters,
    target: WrapTarget,
    cx: &mut AsyncWindowContext,
) -> Task<anyhow::Result<Option<String>>> {
    project.update(cx, |project, cx| {
        project.request_lsp(
            target.buffer,
            project::LanguageServerToQuery::Other(target.server_id),
            ExpandAbbreviation {
                abbreviation,
                text: target.text,
                language: target.language,
                server_id: target.server_id,
                indent: target.indent,
                base_indent: target.base_indent,
                comment_filter: filters.comment,
                bem_filter: filters.bem,
            },
            cx,
        )
    })
}

fn expansion_text(expansion: &str) -> String {
    Snippet::parse(expansion)
        .map(|snippet| snippet.text)
        .unwrap_or_else(|_| expansion.to_string())
}

fn no_expansion_message(abbreviation: &str) -> String {
    format!("No Emmet expansion for {abbreviation:?}")
}

fn remember_abbreviation(abbreviation: SharedString, cx: &mut App) {
    let history = &mut cx.default_global::<WrapAbbreviationHistory>().0;
    history.retain(|entry| *entry != abbreviation);
    history.insert(0, abbreviation);
    history.truncate(MAX_HISTORY_ENTRIES);
}

fn log_unavailable() {
    log::info!("Wrapping with an Emmet abbreviation is not available for the selected text");
}

fn base_indent_for(range_start: Anchor, snapshot: &MultiBufferSnapshot) -> String {
    let row = MultiBufferRow(range_start.to_point(snapshot).row);
    snapshot.indent_size_for_line(row).chars().collect()
}

fn enclosing_element_range(
    position: MultiBufferOffset,
    snapshot: &MultiBufferSnapshot,
) -> Option<Range<MultiBufferOffset>> {
    let mut range = position..position;
    while let Some((node, node_range)) = snapshot.syntax_ancestor(range.clone()) {
        if ELEMENT_NODE_KINDS.contains(&node.kind()) {
            return Some(node_range);
        }
        if node_range == range {
            return None;
        }
        range = node_range;
    }
    None
}

fn expand_over_partial_tags(
    range: Range<MultiBufferOffset>,
    snapshot: &MultiBufferSnapshot,
) -> Range<MultiBufferOffset> {
    let mut expanded = range.clone();
    for position in [range.start, range.end] {
        if let Some(element_range) = element_range_for_partial_tag(position, snapshot) {
            expanded.start = expanded.start.min(element_range.start);
            expanded.end = expanded.end.max(element_range.end);
        }
    }
    expanded
}

fn element_range_for_partial_tag(
    position: MultiBufferOffset,
    snapshot: &MultiBufferSnapshot,
) -> Option<Range<MultiBufferOffset>> {
    let mut range = position..position;
    let mut in_tag = false;
    while let Some((node, node_range)) = snapshot.syntax_ancestor(range.clone()) {
        if TAG_NODE_KINDS.contains(&node.kind()) {
            if node_range.start < position && position < node_range.end {
                in_tag = true;
            }
        } else if ELEMENT_NODE_KINDS.contains(&node.kind()) {
            return in_tag.then_some(node_range);
        }
        if node_range == range {
            return None;
        }
        range = node_range;
    }
    None
}

fn trim_whitespace(
    mut range: Range<MultiBufferOffset>,
    snapshot: &MultiBufferSnapshot,
) -> Range<MultiBufferOffset> {
    for ch in snapshot.chars_at(range.start) {
        if range.start == range.end || !ch.is_whitespace() {
            break;
        }
        range.start += ch.len_utf8();
    }
    for ch in snapshot.reversed_chars_at(range.end) {
        if range.end == range.start || !ch.is_whitespace() {
            break;
        }
        range.end -= ch.len_utf8();
    }
    range
}

