use crate::parser::{MarkdownEvent, MarkdownTag, MarkdownTagEnd};
use std::ops::Range;

struct InlineSpan {
    range: Range<usize>,
    content: Range<usize>,
    is_code: bool,
}

impl InlineSpan {
    fn opening<'a>(&self, source: &'a str) -> &'a str {
        source
            .get(self.range.start..self.content.start)
            .unwrap_or("")
    }

    fn closing<'a>(&self, source: &'a str) -> &'a str {
        source.get(self.content.end..self.range.end).unwrap_or("")
    }
}

fn is_inline_span_tag(tag: &MarkdownTag) -> bool {
    matches!(
        tag,
        MarkdownTag::Emphasis
            | MarkdownTag::Strong
            | MarkdownTag::Strikethrough
            | MarkdownTag::Superscript
            | MarkdownTag::Subscript
            | MarkdownTag::Link { .. }
    )
}

fn is_inline_span_tag_end(tag: &MarkdownTagEnd) -> bool {
    matches!(
        tag,
        MarkdownTagEnd::Emphasis
            | MarkdownTagEnd::Strong
            | MarkdownTagEnd::Strikethrough
            | MarkdownTagEnd::Superscript
            | MarkdownTagEnd::Subscript
            | MarkdownTagEnd::Link
    )
}

fn inline_code_full_range(source: &str, content: &Range<usize>) -> Range<usize> {
    let opening_ticks = source[..content.start]
        .bytes()
        .rev()
        .take_while(|&byte| byte == b'`')
        .count();
    let closing_ticks = source[content.end..]
        .bytes()
        .take_while(|&byte| byte == b'`')
        .count();
    let ticks = opening_ticks.min(closing_ticks);
    content.start - ticks..content.end + ticks
}

fn collect_inline_spans(source: &str, events: &[(Range<usize>, MarkdownEvent)]) -> Vec<InlineSpan> {
    fn note_child(stack: &mut [(Range<usize>, Option<Range<usize>>)], child: &Range<usize>) {
        for (_, content) in stack.iter_mut() {
            match content {
                Some(content) => content.end = content.end.max(child.end),
                None => *content = Some(child.clone()),
            }
        }
    }

    let mut spans = Vec::new();
    let mut stack: Vec<(Range<usize>, Option<Range<usize>>)> = Vec::new();
    for (event_range, event) in events {
        match event {
            MarkdownEvent::Start(tag) if is_inline_span_tag(tag) => {
                note_child(&mut stack, event_range);
                stack.push((event_range.clone(), None));
            }
            MarkdownEvent::End(tag) if is_inline_span_tag_end(tag) => {
                if let Some((range, content)) = stack.pop() {
                    let content = content.unwrap_or(range.clone());
                    spans.push(InlineSpan {
                        range,
                        content,
                        is_code: false,
                    });
                }
                note_child(&mut stack, event_range);
            }
            MarkdownEvent::Code | MarkdownEvent::SubstitutedCode(_) => {
                let range = inline_code_full_range(source, event_range);
                note_child(&mut stack, &range);
                spans.push(InlineSpan {
                    range,
                    content: event_range.clone(),
                    is_code: true,
                });
            }
            _ => note_child(&mut stack, event_range),
        }
    }
    spans
}

pub(crate) fn rebalanced_markdown_for_selection(
    source: &str,
    events: &[(Range<usize>, MarkdownEvent)],
    root_block_starts: &[usize],
    selection: Range<usize>,
) -> String {
    let Some(selection) = snap_to_char_boundaries(source, selection) else {
        return String::new();
    };

    let (start_events, end_events) = boundary_block_events(events, root_block_starts, &selection);
    let mut spans = collect_inline_spans(source, start_events);
    spans.extend(collect_inline_spans(source, end_events));

    let Some(selection) = snap_out_of_delimiters(&spans, selection) else {
        return String::new();
    };

    if selection_is_only_inside_code_spans(&spans, &selection) {
        return source[selection].to_string();
    }

    rebalance_delimiters(source, &spans, &selection)
}

/// Returns the events of the root blocks containing each selection boundary.
/// The second slice is empty when both boundaries share a block.
fn boundary_block_events<'a>(
    events: &'a [(Range<usize>, MarkdownEvent)],
    root_block_starts: &[usize],
    selection: &Range<usize>,
) -> (
    &'a [(Range<usize>, MarkdownEvent)],
    &'a [(Range<usize>, MarkdownEvent)],
) {
    if root_block_starts.is_empty() {
        return (events, &[]);
    }
    let start_block = root_block_index(root_block_starts, selection.start);
    let end_block = root_block_index(root_block_starts, selection.end);
    let start_events = root_block_events(events, root_block_starts, start_block);
    if end_block == start_block {
        (start_events, &[])
    } else {
        (
            start_events,
            root_block_events(events, root_block_starts, end_block),
        )
    }
}

fn root_block_index(root_block_starts: &[usize], offset: usize) -> usize {
    root_block_starts
        .partition_point(|block_start| *block_start <= offset)
        .saturating_sub(1)
}

fn root_block_events<'a>(
    events: &'a [(Range<usize>, MarkdownEvent)],
    root_block_starts: &[usize],
    block: usize,
) -> &'a [(Range<usize>, MarkdownEvent)] {
    let Some(&block_start) = root_block_starts.get(block) else {
        return events;
    };
    let start = events.partition_point(|(range, _)| range.start < block_start);
    let end = match root_block_starts.get(block + 1) {
        Some(&next_block_start) => {
            events.partition_point(|(range, _)| range.start < next_block_start)
        }
        None => events.len(),
    };
    events.get(start..end).unwrap_or(events)
}

fn snap_to_char_boundaries(source: &str, selection: Range<usize>) -> Option<Range<usize>> {
    let mut start = selection.start.min(source.len());
    let mut end = selection.end.min(source.len());
    if start >= end {
        return None;
    }
    while start > 0 && !source.is_char_boundary(start) {
        start -= 1;
    }
    while end < source.len() && !source.is_char_boundary(end) {
        end += 1;
    }
    Some(start..end)
}

/// Shrinks selection boundaries that fall inside delimiter syntax (`**`,
/// etc.) so no delimiter is left half-selected:
///
/// - an end in `**bold*|*` snaps back to `**bold|**`
/// - a start in `*|*bold**` snaps forward to `**|bold**`
///
/// This repeats until stable, since snapping can land inside a nested span's
/// delimiter. Returns `None` if the selection becomes empty.
fn snap_out_of_delimiters(spans: &[InlineSpan], selection: Range<usize>) -> Option<Range<usize>> {
    let mut start = selection.start;
    let mut end = selection.end;
    loop {
        let mut changed = false;
        for span in spans {
            if end > span.range.start && end <= span.content.start {
                end = span.range.start;
                changed = true;
            } else if end > span.content.end && end <= span.range.end {
                end = span.content.end;
                changed = true;
            }
            if start >= span.range.start && start < span.content.start {
                start = span.content.start;
                changed = true;
            } else if start >= span.content.end && start < span.range.end {
                start = span.range.end;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    (start < end).then(|| start..end)
}

fn selection_is_only_inside_code_spans(spans: &[InlineSpan], selection: &Range<usize>) -> bool {
    let contains = |span: &InlineSpan| {
        span.content.start <= selection.start && selection.end <= span.content.end
    };
    spans.iter().any(|span| span.is_code && contains(span))
        && !spans.iter().any(|span| !span.is_code && contains(span))
}

/// Re-adds delimiters cut off by the selection so the result is well-formed
/// markdown:
///
/// - selecting `old te` in `**bold text**` yields `**old te**`
/// - nested spans are reopened outermost first: selecting `alic` in
///   `**bold _italic_**` yields `**_alic_**`
fn rebalance_delimiters(source: &str, spans: &[InlineSpan], selection: &Range<usize>) -> String {
    let nesting_order = |a: &&InlineSpan, b: &&InlineSpan| {
        a.range
            .start
            .cmp(&b.range.start)
            .then(b.range.end.cmp(&a.range.end))
    };

    let mut open_at_start = spans
        .iter()
        .filter(|span| span.content.start <= selection.start && selection.start < span.content.end)
        .collect::<Vec<_>>();
    open_at_start.sort_by(nesting_order);

    let mut open_at_end = spans
        .iter()
        .filter(|span| span.content.start < selection.end && selection.end <= span.content.end)
        .collect::<Vec<_>>();
    open_at_end.sort_by(nesting_order);

    let mut result = String::new();
    for span in &open_at_start {
        result.push_str(span.opening(source));
    }
    result.push_str(&source[selection.clone()]);
    for span in open_at_end.iter().rev() {
        result.push_str(span.closing(source));
    }

    result
}

