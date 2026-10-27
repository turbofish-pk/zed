use collections::BTreeMap;
use gpui::HighlightStyle;
use language::{Chunk, LanguageAwareStyling};
use multi_buffer::{MultiBufferChunks, MultiBufferOffset, MultiBufferSnapshot};
use std::{cmp, ops::Range};

use crate::display_map::{HighlightKey, SemanticTokensHighlights, TextHighlights};

pub struct CustomHighlightsChunks<'a> {
    buffer_chunks: MultiBufferChunks<'a>,
    buffer_chunk: Option<Chunk<'a>>,
    offset: MultiBufferOffset,
    multibuffer_snapshot: &'a MultiBufferSnapshot,

    highlight_endpoints: Vec<HighlightEndpoint>,
    active_highlights: BTreeMap<HighlightKey, HighlightStyle>,
    text_highlights: Option<&'a TextHighlights>,
    semantic_token_highlights: Option<&'a SemanticTokensHighlights>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
struct HighlightEndpoint {
    offset: MultiBufferOffset,
    tag: HighlightKey,
    style: Option<HighlightStyle>,
}

impl<'a> CustomHighlightsChunks<'a> {
    #[ztracing::instrument(skip_all)]
    pub fn new(
        range: Range<MultiBufferOffset>,
        language_aware: LanguageAwareStyling,
        text_highlights: Option<&'a TextHighlights>,
        semantic_token_highlights: Option<&'a SemanticTokensHighlights>,
        multibuffer_snapshot: &'a MultiBufferSnapshot,
    ) -> Self {
        let mut highlight_endpoints = Vec::new();
        create_highlight_endpoints(
            &range,
            text_highlights,
            semantic_token_highlights,
            multibuffer_snapshot,
            &mut highlight_endpoints,
        );
        Self {
            buffer_chunks: multibuffer_snapshot.chunks(range.clone(), language_aware),
            buffer_chunk: None,
            offset: range.start,
            text_highlights,
            highlight_endpoints,
            active_highlights: Default::default(),
            multibuffer_snapshot,
            semantic_token_highlights,
        }
    }

    #[ztracing::instrument(skip_all)]
    pub fn seek(&mut self, new_range: Range<MultiBufferOffset>) {
        create_highlight_endpoints(
            &new_range,
            self.text_highlights,
            self.semantic_token_highlights,
            self.multibuffer_snapshot,
            &mut self.highlight_endpoints,
        );
        self.offset = new_range.start;
        self.buffer_chunks.seek(new_range);
        self.buffer_chunk.take();
        self.active_highlights.clear()
    }
}

fn create_highlight_endpoints(
    range: &Range<MultiBufferOffset>,
    text_highlights: Option<&TextHighlights>,
    semantic_token_highlights: Option<&SemanticTokensHighlights>,
    buffer: &MultiBufferSnapshot,
    highlight_endpoints: &mut Vec<HighlightEndpoint>,
) {
    highlight_endpoints.clear();
    if let Some(text_highlights) = text_highlights {
        let start = buffer.anchor_after(range.start);
        let end = buffer.anchor_after(range.end);
        let mut text_highlights_scratch = Vec::new();

        for (&tag, text_highlights) in text_highlights.iter() {
            let style = text_highlights.0;
            let ranges = &text_highlights.1;

            let start_ix = ranges
                .binary_search_by(|probe| probe.end.cmp(&start, buffer).then(cmp::Ordering::Less))
                .unwrap_or_else(|i| i);
            let end_ix = ranges[start_ix..]
                .binary_search_by(|probe| {
                    probe.start.cmp(&end, buffer).then(cmp::Ordering::Greater)
                })
                .unwrap_or_else(|i| i);

            let ranges_ = &ranges[start_ix..][..end_ix];
            text_highlights_scratch.clear();
            text_highlights_scratch.reserve(ranges_.len());
            highlight_endpoints.reserve(2 * ranges_.len());

            let mut iter = ranges_.iter();
            buffer.summaries_for_anchors_cb(
                ranges_.iter().map(|r| &r.start),
                |start: MultiBufferOffset| {
                    text_highlights_scratch.push((start, iter.next().unwrap().end));
                },
            );
            text_highlights_scratch.sort_by(|a, b| a.1.cmp(&b.1, buffer));
            let mut iter = text_highlights_scratch.iter();
            buffer.summaries_for_anchors_cb(
                text_highlights_scratch.iter().map(|(_, end)| end),
                |end: MultiBufferOffset| {
                    let start = iter.next().unwrap().0;
                    if start == end {
                        return;
                    }
                    highlight_endpoints.push(HighlightEndpoint {
                        offset: start,
                        tag,
                        style: Some(style),
                    });
                    highlight_endpoints.push(HighlightEndpoint {
                        offset: end,
                        tag,
                        style: None,
                    });
                },
            );
        }
    }
    if let Some(semantic_token_highlights) = semantic_token_highlights {
        let start = buffer.anchor_after(range.start);
        let end = buffer.anchor_after(range.end);
        let mut semantic_highlights_scratch = Vec::new();
        for buffer_id in buffer.buffer_ids_for_range(range.clone()) {
            let Some((semantic_token_highlights, interner)) =
                semantic_token_highlights.get(&buffer_id)
            else {
                continue;
            };
            let start_ix = semantic_token_highlights
                .binary_search_by(|probe| {
                    probe
                        .range
                        .end
                        .cmp(&start, buffer)
                        .then(cmp::Ordering::Less)
                })
                .unwrap_or_else(|i| i);
            let end_ix = semantic_token_highlights[start_ix..]
                .binary_search_by(|probe| {
                    probe
                        .range
                        .start
                        .cmp(&end, buffer)
                        .then(cmp::Ordering::Greater)
                })
                .unwrap_or_else(|i| i);

            let ranges_ = &semantic_token_highlights[start_ix..][..end_ix];
            semantic_highlights_scratch.clear();
            semantic_highlights_scratch.reserve(ranges_.len());
            highlight_endpoints.reserve(2 * ranges_.len());

            let mut iter = ranges_.iter();
            buffer.summaries_for_anchors_cb(
                ranges_.iter().map(|token| &token.range.start),
                |start: MultiBufferOffset| {
                    semantic_highlights_scratch.push((start, iter.next().unwrap()));
                },
            );
            semantic_highlights_scratch.sort_by(|a, b| a.1.range.end.cmp(&b.1.range.end, buffer));
            let mut iter = semantic_highlights_scratch.iter();
            buffer.summaries_for_anchors_cb(
                semantic_highlights_scratch
                    .iter()
                    .map(|(_, token)| &token.range.end),
                |end: MultiBufferOffset| {
                    let (start, token) = iter.next().unwrap();
                    if *start == end {
                        return;
                    }
                    highlight_endpoints.push(HighlightEndpoint {
                        offset: *start,
                        tag: HighlightKey::SemanticToken(token.precedence),
                        style: Some(interner[token.style]),
                    });
                    highlight_endpoints.push(HighlightEndpoint {
                        offset: end,
                        tag: HighlightKey::SemanticToken(token.precedence),
                        style: None,
                    });
                },
            );
        }
    }
    highlight_endpoints.sort_by(|a, b| a.cmp(b).reverse());
}

impl<'a> Iterator for CustomHighlightsChunks<'a> {
    type Item = Chunk<'a>;

    #[ztracing::instrument(skip_all)]
    fn next(&mut self) -> Option<Self::Item> {
        let mut next_highlight_endpoint = MultiBufferOffset(usize::MAX);
        while let Some(endpoint) = self.highlight_endpoints.last().copied() {
            if endpoint.offset <= self.offset {
                if let Some(style) = endpoint.style {
                    self.active_highlights.insert(endpoint.tag, style);
                } else {
                    self.active_highlights.remove(&endpoint.tag);
                }
                self.highlight_endpoints.pop();
            } else {
                next_highlight_endpoint = endpoint.offset;
                break;
            }
        }

        let chunk = match &mut self.buffer_chunk {
            Some(it) => it,
            slot => slot.insert(self.buffer_chunks.next()?),
        };
        while chunk.text.is_empty() {
            *chunk = self.buffer_chunks.next()?;
        }

        let split_idx = chunk.text.len().min(next_highlight_endpoint - self.offset);
        let (prefix, suffix) = chunk.text.split_at(split_idx);
        self.offset += prefix.len();

        let mask = 1u128.unbounded_shl(split_idx as u32).wrapping_sub(1);
        let chars = chunk.chars & mask;
        let tabs = chunk.tabs & mask;
        let newlines = chunk.newlines & mask;
        let mut prefix = Chunk {
            text: prefix,
            chars,
            tabs,
            newlines,
            ..chunk.clone()
        };

        chunk.chars = chunk.chars.unbounded_shr(split_idx as u32);
        chunk.tabs = chunk.tabs.unbounded_shr(split_idx as u32);
        chunk.newlines = chunk.newlines.unbounded_shr(split_idx as u32);
        chunk.text = suffix;
        if !self.active_highlights.is_empty() {
            let mut active_highlights = self.active_highlights.iter().peekable();
            while let Some((tag, style)) = active_highlights.next() {
                let overridden_by_later_semantic_token =
                    matches!(tag, HighlightKey::SemanticToken(_))
                        && matches!(
                            active_highlights.peek(),
                            Some((HighlightKey::SemanticToken(_), _))
                        );
                if overridden_by_later_semantic_token {
                    continue;
                }
                prefix.highlight_style = Some(match prefix.highlight_style {
                    Some(active_highlight) => active_highlight.highlight(*style),
                    None => *style,
                });
            }
        }
        Some(prefix)
    }
}

impl PartialOrd for HighlightEndpoint {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HighlightEndpoint {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        self.offset
            .cmp(&other.offset)
            .then_with(|| self.style.is_some().cmp(&other.style.is_some()))
            .then_with(|| self.tag.cmp(&other.tag))
    }
}

