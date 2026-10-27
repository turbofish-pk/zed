use crate::{
    Anchor, DisplayPoint, Editor, EditorSettings, EditorSnapshot, FindAllReferences,
    GotoDefinitionKind, HighlightKey, Navigated, PointForPosition, SelectPhase,
    editor_settings::GoToDefinitionFallback, scroll::ScrollAmount,
};
use gpui::{
    Action, App, AsyncWindowContext, Context, Entity, Focusable, HighlightStyle, Modifiers, Pixels,
    Task, UnderlineStyle, WeakEntity, Window, px,
};
use language::{Bias, ToOffset};
use linkify::{LinkFinder, LinkKind};
use lsp::LanguageServerId;
use project::{InlayId, Location, LocationLink, Project, ProjectPath, ResolvedPath};
use regex::Regex;
use settings::{OpenResultsIn, Settings};
use std::{
    ops::Range,
    str::FromStr as _,
    sync::{Arc, LazyLock},
};
use text::OffsetRangeExt;
use theme::ActiveTheme as _;
use util::{
    ResultExt, TryFutureExt as _, markdown::source_position_from_fragment, paths::PathWithPosition,
};
use workspace::pane::NavigationEntry;

#[derive(Clone, Action)]
#[action(no_json, no_register)]
pub struct OpenDefinitionLocations(pub Arc<DefinitionLocations>);

pub struct DefinitionLocations {
    pub editor: WeakEntity<Editor>,
    pub position: Anchor,
    pub kind: GotoDefinitionKind,
    pub locations: Vec<Location>,
    pub origin: Option<NavigationEntry>,
}

impl PartialEq for OpenDefinitionLocations {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[derive(Debug)]
pub struct HoveredLinkState {
    pub last_trigger_point: TriggerPoint,
    pub preferred_kind: GotoDefinitionKind,
    pub symbol_range: Option<RangeInEditor>,
    pub links: Vec<HoverLink>,
    pub task: Option<Task<Option<()>>>,
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum RangeInEditor {
    Text(Range<Anchor>),
    Inlay(InlayHighlight),
}

impl RangeInEditor {
    pub fn as_text_range(&self) -> Option<Range<Anchor>> {
        match self {
            Self::Text(range) => Some(range.clone()),
            Self::Inlay(_) => None,
        }
    }

    pub fn point_within_range(
        &self,
        trigger_point: &TriggerPoint,
        snapshot: &EditorSnapshot,
    ) -> bool {
        match (self, trigger_point) {
            (Self::Text(range), TriggerPoint::Text(point)) => {
                let buffer_snapshot = snapshot.buffer_snapshot();
                if !range.start.is_valid(&buffer_snapshot)
                    || !range.end.is_valid(&buffer_snapshot)
                    || !point.is_valid(&buffer_snapshot)
                {
                    return false;
                }
                let point_after_start = range.start.cmp(point, &buffer_snapshot).is_le();
                let point_after_end = range.end.cmp(point, &buffer_snapshot).is_ge();
                point_after_start && point_after_end
            }
            (Self::Inlay(highlight), TriggerPoint::InlayHint(point, _, _)) => {
                highlight.inlay == point.inlay
                    && highlight.range.contains(&point.range.start)
                    && highlight.range.contains(&point.range.end)
            }
            (Self::Inlay(_), TriggerPoint::Text(_))
            | (Self::Text(_), TriggerPoint::InlayHint(_, _, _)) => false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum HoverLink {
    Url(String),
    File(ResolvedFileTarget),
    Text(LocationLink),
    /// Navigate to an LSP-given location whose buffer may not be loaded yet.
    /// Used by inlay-hint hover, code-lens references, and document-link
    /// targets that point inside a workspace file (e.g. `file:///foo#9,16`).
    LspLocation(lsp::Location, LanguageServerId),
}

/// Convert a `documentLink` target URI into a [`HoverLink`], reusing the
/// existing navigation paths: `file://` URIs go through the LSP location
/// pipeline (so an optional `#line[,column]` fragment is honored), while
/// any other scheme is opened as a regular URL.
pub fn document_link_target_to_hover_link(target: &str, server_id: LanguageServerId) -> HoverLink {
    if let Ok(url) = url::Url::parse(target)
        && url.scheme() == "file"
        && let Ok(uri) = lsp::Uri::from_str(target)
    {
        let position = url
            .fragment()
            .and_then(source_position_from_fragment)
            .map(|(line, character)| lsp::Position { line, character })
            .unwrap_or_default();
        return HoverLink::LspLocation(
            lsp::Location {
                uri,
                range: lsp::Range::new(position, position),
            },
            server_id,
        );
    }
    HoverLink::Url(target.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlayHighlight {
    pub inlay: InlayId,
    pub inlay_position: Anchor,
    pub range: Range<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TriggerPoint {
    Text(Anchor),
    InlayHint(InlayHighlight, lsp::Location, LanguageServerId),
}

impl TriggerPoint {
    fn anchor(&self) -> &Anchor {
        match self {
            TriggerPoint::Text(anchor) => anchor,
            TriggerPoint::InlayHint(inlay_range, _, _) => &inlay_range.inlay_position,
        }
    }
}

pub fn exclude_link_to_position(
    buffer: &Entity<language::Buffer>,
    current_position: &text::Anchor,
    location: &LocationLink,
    cx: &App,
) -> bool {
    // Exclude definition links that points back to cursor position.
    // (i.e., currently cursor upon definition).
    let snapshot = buffer.read(cx).snapshot();
    !(buffer == &location.target.buffer
        && current_position
            .bias_right(&snapshot)
            .cmp(&location.target.range.start, &snapshot)
            .is_ge()
        && current_position
            .cmp(&location.target.range.end, &snapshot)
            .is_le())
}

impl Editor {
    pub(crate) fn update_hovered_link(
        &mut self,
        point_for_position: PointForPosition,
        hint_glyph: Option<DisplayPoint>,
        mouse_position: Option<gpui::Point<Pixels>>,
        snapshot: &EditorSnapshot,
        modifiers: Modifiers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hovered_link_modifier = Editor::is_cmd_or_ctrl_pressed(&modifiers, cx);
        if !hovered_link_modifier || self.has_pending_selection() {
            self.hide_hovered_link(cx);
            return;
        }

        if !cx.is_cursor_visible() {
            self.hide_hovered_link(cx);
            return;
        }

        match point_for_position.as_valid() {
            Some(point) if hint_glyph.is_none() => {
                let trigger_point = TriggerPoint::Text(
                    snapshot
                        .buffer_snapshot()
                        .anchor_before(point.to_offset(&snapshot.display_snapshot, Bias::Left)),
                );

                show_link_definition(modifiers.shift, self, trigger_point, snapshot, window, cx);
            }
            _ => {
                self.update_inlay_link_and_hover_points(
                    snapshot,
                    hint_glyph,
                    mouse_position,
                    hovered_link_modifier,
                    modifiers.shift,
                    window,
                    cx,
                );
            }
        }
    }

    pub(crate) fn hide_hovered_link(&mut self, cx: &mut Context<Self>) {
        self.hovered_link_state.take();
        self.clear_highlights(HighlightKey::HoveredLinkState, cx);
    }

    pub(crate) fn handle_click_hovered_link(
        &mut self,
        point: PointForPosition,
        modifiers: Modifiers,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        let kind = if modifiers.shift {
            GotoDefinitionKind::Type
        } else {
            GotoDefinitionKind::Symbol
        };
        let split = Self::is_alt_pressed(&modifiers, cx);
        let snapshot = self.snapshot(window, cx);
        let position = snapshot
            .buffer_snapshot()
            .anchor_before(point.next_valid.to_point(&snapshot.display_snapshot));
        let origin = self.navigation_entry(position, cx);
        let cached = self.hovered_link_state.take();
        self.hide_hovered_link(cx);
        let refresh = cached.as_ref().is_none_or(|state| {
            state.links.is_empty()
                || (matches!(state.last_trigger_point, TriggerPoint::Text(_))
                    && (state.preferred_kind != kind
                        || state.task.as_ref().is_some_and(|task| !task.is_ready())))
        });
        let mut links = cached.map(|state| state.links).unwrap_or_default();
        if refresh || !self.lsp_data_enabled() {
            links.retain(|link| {
                matches!(link, HoverLink::Url(_) | HoverLink::File(_))
                    || (self.lsp_data_enabled() && matches!(link, HoverLink::LspLocation(..)))
            });
        }
        if refresh && point.as_valid().is_some() {
            self.select(
                SelectPhase::Begin {
                    position: point.next_valid,
                    add: false,
                    click_count: 1,
                },
                window,
                cx,
            );
        }
        self.select(SelectPhase::End, window, cx);
        let Some((buffer, anchor)) = self.buffer.read(cx).text_anchor_for_position(position, cx)
        else {
            return;
        };
        links.retain(|link| match link {
            HoverLink::Text(link) => exclude_link_to_position(&buffer, &anchor, link, cx),
            // A file link resolving to the buffer we are already in is not a navigation:
            // opening it returns `Navigated::Yes`, which suppresses the go-to-definition
            // fallback, so the click appears to do nothing. Same rule as
            // `exclude_link_to_position` above, one level coarser — file rather than position.
            HoverLink::File(target) => match &target.resolved_path {
                ResolvedPath::ProjectPath { project_path, .. } => {
                    let current_project_path = buffer
                        .read(cx)
                        .file()
                        .map(|file| ProjectPath::from_file(file.as_ref(), cx));
                    Some(project_path) != current_project_path.as_ref()
                }
                ResolvedPath::AbsPath { .. } => true,
            },
            _ => true,
        });
        let definitions = (refresh && self.lsp_data_enabled() && point.as_valid().is_some())
            .then(|| {
                self.semantics_provider
                    .as_ref()?
                    .definitions(&buffer, anchor, kind, cx)
            })
            .flatten();
        if let Some(definitions) = definitions {
            let workspace = self.workspace().map(|workspace| workspace.downgrade());
            cx.spawn_in(window, async move |editor, cx| {
                let definitions = definitions.await.log_err().flatten().unwrap_or_default();
                editor
                    .update_in(cx, |editor, window, cx| {
                        if editor.workspace().map(|workspace| workspace.downgrade()) != workspace
                            || !editor.buffer.read(cx).snapshot(cx).can_resolve(&position)
                        {
                            return;
                        }
                        if editor.lsp_data_enabled() {
                            links.extend(
                                definitions
                                    .into_iter()
                                    .filter(|link| {
                                        exclude_link_to_position(&buffer, &anchor, link, cx)
                                    })
                                    .map(HoverLink::Text),
                            );
                        } else {
                            links.retain(|link| {
                                matches!(link, HoverLink::Url(_) | HoverLink::File(_))
                            });
                        }
                        editor
                            .reveal_clicked_links(kind, links, position, origin, split, window, cx);
                    })
                    .ok();
            })
            .detach();
        } else {
            self.reveal_clicked_links(kind, links, position, origin, split, window, cx);
        }
    }

    pub(crate) fn open_definition_locations(
        &mut self,
        action: &OpenDefinitionLocations,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let action = &action.0;
        if action.editor != cx.weak_entity() {
            cx.propagate();
            return;
        }
        if !self.lsp_data_enabled()
            || !self
                .buffer
                .read(cx)
                .snapshot(cx)
                .can_resolve(&action.position)
        {
            return;
        }
        let links = action
            .locations
            .iter()
            .cloned()
            .map(|target| {
                HoverLink::Text(LocationLink {
                    origin: None,
                    target,
                })
            })
            .collect::<Vec<_>>();
        self.open_clicked_links(action.kind, links, action.origin.clone(), false, window, cx);
    }

    pub fn scroll_hover(
        &mut self,
        amount: ScrollAmount,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let selection = self.selections.newest_anchor().head();
        let snapshot = self.snapshot(window, cx);

        if let Some(popover) = self.hover_state.info_popovers.iter().find(|popover| {
            popover
                .symbol_range
                .point_within_range(&TriggerPoint::Text(selection), &snapshot)
        }) {
            popover.scroll(amount, window, cx);
            true
        } else if let Some(context_menu) = self.context_menu.borrow_mut().as_mut() {
            context_menu.scroll_aside(amount, window, cx);
            true
        } else {
            false
        }
    }

    fn reveal_clicked_links(
        &mut self,
        kind: GotoDefinitionKind,
        links: Vec<HoverLink>,
        position: Anchor,
        origin: Option<NavigationEntry>,
        split: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !split
            && self.lsp_data_enabled()
            && self.workspace().is_some()
            && EditorSettings::get_global(cx).lsp_results_location == OpenResultsIn::Picker
            && links.iter().all(|link| matches!(link, HoverLink::Text(_)))
        {
            let action = OpenDefinitionLocations(Arc::new(DefinitionLocations {
                editor: cx.weak_entity(),
                position,
                kind,
                locations: links
                    .into_iter()
                    .filter_map(|link| match link {
                        HoverLink::Text(link) => Some(link.target),
                        _ => None,
                    })
                    .collect(),
                origin,
            }));
            let focus_handle = self.focus_handle(cx);
            window.defer(cx, move |window, cx| {
                if window.is_action_available_in(&action, &focus_handle) {
                    focus_handle.dispatch_action(&action, window, cx);
                } else {
                    action
                        .0
                        .editor
                        .update(cx, |editor, cx| {
                            editor.open_definition_locations(&action, window, cx);
                        })
                        .ok();
                }
            });
        } else {
            self.open_clicked_links(kind, links, origin, split, window, cx);
        }
    }

    fn open_clicked_links(
        &mut self,
        kind: GotoDefinitionKind,
        links: Vec<HoverLink>,
        origin: Option<NavigationEntry>,
        split: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let navigation = self.navigate_to_hover_links(Some(kind), links, origin, split, window, cx);
        cx.spawn_in(window, async move |editor, cx| {
            if navigation.await.log_err().unwrap_or(Navigated::No) == Navigated::Yes {
                return;
            }
            let focus_handle = editor
                .read_with(cx, |editor, cx| {
                    editor.lsp_data_enabled().then(|| editor.focus_handle(cx))
                })
                .ok()
                .flatten();
            if let Some(focus_handle) = focus_handle {
                cx.update(|window, cx| {
                    if EditorSettings::get_global(cx).go_to_definition_fallback
                        == GoToDefinitionFallback::FindAllReferences
                    {
                        focus_handle.dispatch_action(&FindAllReferences::default(), window, cx);
                    }
                })
                .ok();
            }
        })
        .detach();
    }
}

pub fn show_link_definition(
    shift_held: bool,
    editor: &mut Editor,
    trigger_point: TriggerPoint,
    snapshot: &EditorSnapshot,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let preferred_kind = match trigger_point {
        TriggerPoint::Text(_) if !shift_held => GotoDefinitionKind::Symbol,
        _ => GotoDefinitionKind::Type,
    };

    let (mut hovered_link_state, is_cached) =
        if let Some(existing) = editor.hovered_link_state.take() {
            (existing, true)
        } else {
            (
                HoveredLinkState {
                    last_trigger_point: trigger_point.clone(),
                    symbol_range: None,
                    preferred_kind,
                    links: vec![],
                    task: None,
                },
                false,
            )
        };

    if editor.pending_rename.is_some() {
        return;
    }

    let anchor = trigger_point.anchor().bias_left(snapshot.buffer_snapshot());
    let Some((anchor, _)) = snapshot.buffer_snapshot().anchor_to_buffer_anchor(anchor) else {
        return;
    };
    let Some(buffer) = editor.buffer.read(cx).buffer(anchor.buffer_id) else {
        return;
    };
    let same_kind = hovered_link_state.preferred_kind == preferred_kind
        || hovered_link_state
            .links
            .first()
            .is_some_and(|d| matches!(d, HoverLink::Url(_) | HoverLink::LspLocation(_, _)));

    if same_kind {
        if is_cached && (hovered_link_state.last_trigger_point == trigger_point)
            || hovered_link_state
                .symbol_range
                .as_ref()
                .is_some_and(|symbol_range| {
                    symbol_range.point_within_range(&trigger_point, snapshot)
                })
        {
            editor.hovered_link_state = Some(hovered_link_state);
            return;
        }
    } else {
        editor.hide_hovered_link(cx)
    }
    let project = editor.project.clone();
    let provider = editor.semantics_provider.clone();

    // Record the requested position so a mouse move on the same point short-circuits
    // instead of re-querying, even when the server returns no `originSelectionRange`
    // (which would otherwise leave `symbol_range` empty).
    hovered_link_state.last_trigger_point = trigger_point.clone();

    hovered_link_state.task = Some(cx.spawn_in(window, async move |this, cx| {
        async move {
            // LSP document links take priority: the server explicitly
            // declares which ranges are clickable, so they are more
            // accurate than the heuristic-based URL/file detection.
            //
            // Resolution is deduplicated by `LspStore`; awaiting here only
            // blocks until either the cached resolved entry is returned or
            // the in-flight `Shared` task completes.
            let resolved_document_links = this
                .update(cx, |editor, cx| {
                    editor.document_links_at(buffer.clone(), anchor, cx)
                })
                .ok()
                .flatten();
            let resolved_document_links = match resolved_document_links {
                Some(task) => task.await,
                None => Vec::new(),
            };
            let snapshot = this.read_with(cx, |editor, cx| editor.buffer.read(cx).snapshot(cx))?;
            let detected_document_link =
                resolved_document_links
                    .into_iter()
                    .find_map(|(server_id, link)| {
                        let multi_buffer_range =
                            snapshot.buffer_anchor_range_to_anchor_range(link.range.clone())?;
                        Some((link.range, multi_buffer_range, link.target, server_id))
                    });
            drop(snapshot);

            let result = match &trigger_point {
                TriggerPoint::Text(_) => {
                    let mut links = Vec::new();
                    let mut symbol_range = None;

                    // LSP-provided document link wins over heuristic URL/file
                    // detection at the same position: the server tells us the
                    // exact range and target, while `find_url`/`find_file` are
                    // best-effort text matches.
                    if let Some((_, multi_buffer_range, Some(target), server_id)) =
                        detected_document_link.clone()
                    {
                        symbol_range = Some(RangeInEditor::Text(multi_buffer_range));
                        links.push(document_link_target_to_hover_link(&target, server_id));
                    } else if let Some((url_range, url)) = find_url(&buffer, anchor, cx) {
                        let snapshot =
                            this.read_with(cx, |editor, cx| editor.buffer.read(cx).snapshot(cx))?;
                        if let Some(range) = snapshot.buffer_anchor_range_to_anchor_range(url_range)
                        {
                            symbol_range = Some(RangeInEditor::Text(range));
                        }
                        links.push(HoverLink::Url(url));
                    } else if let Some((filename_range, file_target)) =
                        find_file(&buffer, project.clone(), anchor, cx).await
                    {
                        let snapshot =
                            this.read_with(cx, |editor, cx| editor.buffer.read(cx).snapshot(cx))?;
                        if let Some(range) =
                            snapshot.buffer_anchor_range_to_anchor_range(filename_range)
                        {
                            symbol_range = Some(RangeInEditor::Text(range));
                        }
                        links.push(HoverLink::File(file_target));
                    }

                    // Always also collect LSP definitions so that cmd-click
                    // reveals every applicable target (e.g. a position that
                    // carries both a document link and a definition).
                    if let Some(provider) = provider {
                        let task = cx.update(|_, cx| {
                            provider.definitions(&buffer, anchor, preferred_kind, cx)
                        })?;
                        if let Some(task) = task
                            && let Some(definition_result) = task.await.ok().flatten()
                        {
                            if symbol_range.is_none() {
                                let snapshot = this.read_with(cx, |editor, cx| {
                                    editor.buffer.read(cx).snapshot(cx)
                                })?;
                                symbol_range = definition_result.iter().find_map(|link| {
                                    link.origin.as_ref().and_then(|origin| {
                                        let range = snapshot.buffer_anchor_range_to_anchor_range(
                                            origin.range.clone(),
                                        )?;
                                        Some(RangeInEditor::Text(range))
                                    })
                                });
                            }
                            links.extend(definition_result.into_iter().map(HoverLink::Text));
                        }
                    }

                    if links.is_empty() {
                        None
                    } else {
                        Some((symbol_range, links))
                    }
                }
                TriggerPoint::InlayHint(highlight, lsp_location, server_id) => Some((
                    Some(RangeInEditor::Inlay(highlight.clone())),
                    vec![HoverLink::LspLocation(lsp_location.clone(), *server_id)],
                )),
            };

            this.update(cx, |editor, cx| {
                // Clear any existing highlights
                editor.clear_highlights(HighlightKey::HoveredLinkState, cx);
                let Some(hovered_link_state) = editor.hovered_link_state.as_mut() else {
                    editor.hide_hovered_link(cx);
                    return;
                };
                hovered_link_state.preferred_kind = preferred_kind;
                hovered_link_state.symbol_range = result
                    .as_ref()
                    .and_then(|(symbol_range, _)| symbol_range.clone())
                    .or_else(|| {
                        // Even if we have no click target yet (e.g. an
                        // unresolved document link), record the link's range
                        // so subsequent mouse moves on the same link
                        // short-circuit in `show_link_definition`.
                        detected_document_link
                            .as_ref()
                            .map(|(_, multi_buffer_range, _, _)| {
                                RangeInEditor::Text(multi_buffer_range.clone())
                            })
                    });

                if let Some((symbol_range, definitions)) = result {
                    hovered_link_state.links = definitions;

                    let underline_hovered_link = !hovered_link_state.links.is_empty()
                        || hovered_link_state.symbol_range.is_some();

                    if underline_hovered_link {
                        let style = HighlightStyle {
                            underline: Some(UnderlineStyle {
                                thickness: px(1.),
                                ..UnderlineStyle::default()
                            }),
                            color: Some(cx.theme().colors().link_text_hover),
                            ..HighlightStyle::default()
                        };
                        let highlight_range =
                            symbol_range.unwrap_or_else(|| match &trigger_point {
                                TriggerPoint::Text(trigger_anchor) => {
                                    let snapshot = editor.buffer.read(cx).snapshot(cx);
                                    // If no symbol range returned from language server, use the surrounding word.
                                    let (offset_range, _) =
                                        snapshot.surrounding_word(*trigger_anchor, None);
                                    RangeInEditor::Text(
                                        snapshot.anchor_before(offset_range.start)
                                            ..snapshot.anchor_after(offset_range.end),
                                    )
                                }
                                TriggerPoint::InlayHint(highlight, _, _) => {
                                    RangeInEditor::Inlay(highlight.clone())
                                }
                            });

                        // When the server reports no `originSelectionRange`, fall back
                        // to the highlighted word as the symbol range so that hovering
                        // elsewhere within the same symbol reuses this result instead
                        // of issuing another request.
                        if let Some(hovered_link_state) = editor.hovered_link_state.as_mut()
                            && hovered_link_state.symbol_range.is_none()
                        {
                            hovered_link_state.symbol_range = Some(highlight_range.clone());
                        }

                        match highlight_range {
                            RangeInEditor::Text(text_range) => editor.highlight_text(
                                HighlightKey::HoveredLinkState,
                                vec![text_range],
                                style,
                                cx,
                            ),
                            RangeInEditor::Inlay(highlight) => editor.highlight_inlays(
                                HighlightKey::HoveredLinkState,
                                vec![highlight],
                                style,
                                cx,
                            ),
                        }
                    }
                } else if let Some((_, multi_buffer_range, _, _)) = detected_document_link.as_ref()
                {
                    let style = HighlightStyle {
                        underline: Some(UnderlineStyle {
                            thickness: px(1.),
                            ..UnderlineStyle::default()
                        }),
                        color: Some(cx.theme().colors().link_text_hover),
                        ..HighlightStyle::default()
                    };
                    editor.highlight_text(
                        HighlightKey::HoveredLinkState,
                        vec![multi_buffer_range.clone()],
                        style,
                        cx,
                    );
                } else {
                    // When no links are found, we don't want to completely
                    // throw away the `HoveredLinkState`, we'll want to at least
                    // keep the `trigger_point` around in order to avoid sending
                    // multiple requests for the same point.
                    hovered_link_state.links.clear();
                }
            })?;

            anyhow::Ok(())
        }
        .log_err()
        .await
    }));

    editor.hovered_link_state = Some(hovered_link_state);
}

pub(crate) fn find_url(
    buffer: &Entity<language::Buffer>,
    position: text::Anchor,
    cx: &AsyncWindowContext,
) -> Option<(Range<text::Anchor>, String)> {
    const LIMIT: usize = 2048;

    let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());

    let offset = position.to_offset(&snapshot);
    let mut token_start = offset;
    let mut token_end = offset;
    let mut found_start = false;
    let mut found_end = false;

    for ch in snapshot.reversed_chars_at(offset).take(LIMIT) {
        if ch.is_whitespace() {
            found_start = true;
            break;
        }
        token_start -= ch.len_utf8();
    }
    // Check if we didn't find the starting whitespace or if we didn't reach the start of the buffer
    if !found_start && token_start != 0 {
        return None;
    }

    for ch in snapshot
        .chars_at(offset)
        .take(LIMIT - (offset - token_start))
    {
        if ch.is_whitespace() {
            found_end = true;
            break;
        }
        token_end += ch.len_utf8();
    }
    // Check if we didn't find the ending whitespace or if we read more or equal than LIMIT
    // which at this point would happen only if we reached the end of buffer
    if !found_end && (token_end - token_start >= LIMIT) {
        return None;
    }

    let mut finder = LinkFinder::new();
    finder.kinds(&[LinkKind::Url]);
    let input = snapshot
        .text_for_range(token_start..token_end)
        .collect::<String>();

    let relative_offset = offset - token_start;
    for link in finder.links(&input) {
        if link.start() <= relative_offset && link.end() >= relative_offset {
            let range = snapshot.anchor_before(token_start + link.start())
                ..snapshot.anchor_after(token_start + link.end());
            return Some((range, link.as_str().to_string()));
        }
    }
    None
}

pub(crate) fn find_url_from_range(
    buffer: &Entity<language::Buffer>,
    range: Range<text::Anchor>,
    cx: &AsyncWindowContext,
) -> Option<String> {
    const LIMIT: usize = 2048;

    let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());

    let start_offset = range.start.to_offset(&snapshot);
    let end_offset = range.end.to_offset(&snapshot);

    let mut token_start = start_offset.min(end_offset);
    let mut token_end = start_offset.max(end_offset);

    let range_len = token_end - token_start;

    if range_len >= LIMIT {
        return None;
    }

    // Skip leading whitespace
    for ch in snapshot.chars_at(token_start).take(range_len) {
        if !ch.is_whitespace() {
            break;
        }
        token_start += ch.len_utf8();
    }

    // Skip trailing whitespace
    for ch in snapshot.reversed_chars_at(token_end).take(range_len) {
        if !ch.is_whitespace() {
            break;
        }
        token_end -= ch.len_utf8();
    }

    if token_start >= token_end {
        return None;
    }

    let text = snapshot
        .text_for_range(token_start..token_end)
        .collect::<String>();

    let mut finder = LinkFinder::new();
    finder.kinds(&[LinkKind::Url]);

    if let Some(link) = finder.links(&text).next()
        && link.start() == 0
        && link.end() == text.len()
    {
        return Some(link.as_str().to_string());
    }

    None
}

#[derive(Debug, Clone)]
pub struct ResolvedFileTarget {
    pub resolved_path: ResolvedPath,
    pub row: Option<u32>,
    pub column: Option<u32>,
}

impl ResolvedFileTarget {
    /// After opening a file, navigate the editor to the row/column position if present.
    pub fn navigate_item_to_position(
        &self,
        item: Box<dyn crate::ItemHandle>,
        cx: &mut AsyncWindowContext,
    ) {
        if let Some(row) = self.row {
            let col = self.column.unwrap_or(0);
            if let Some(active_editor) = item.downcast::<crate::Editor>() {
                active_editor
                    .downgrade()
                    .update_in(cx, |editor, window, cx| {
                        let row = row.saturating_sub(1);
                        let col = col.saturating_sub(1);
                        let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
                            return;
                        };
                        let point = buffer
                            .read(cx)
                            .snapshot()
                            .point_from_external_input(row, col);
                        editor.go_to_singleton_buffer_point_silently(point, window, cx);
                    })
                    .log_err();
            }
        }
    }
}

pub(crate) async fn find_file(
    buffer: &Entity<language::Buffer>,
    project: Option<Entity<Project>>,
    position: text::Anchor,
    cx: &mut AsyncWindowContext,
) -> Option<(Range<text::Anchor>, ResolvedFileTarget)> {
    let project = project?;
    let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
    let scope = snapshot.language_scope_at(position);
    let (range, candidate_file_path) = surrounding_filename(&snapshot, position)?;
    let candidate_len = candidate_file_path.len();

    async fn check_path(
        candidate_file_path: &str,
        project: &Entity<Project>,
        buffer: &Entity<language::Buffer>,
        cx: &mut AsyncWindowContext,
    ) -> Option<ResolvedPath> {
        project
            .update(cx, |project, cx| {
                project.resolve_path_in_buffer(candidate_file_path, buffer, cx)
            })
            .await
            .filter(|s| s.is_file())
    }

    let pattern_candidates = link_pattern_file_candidates(&candidate_file_path);

    // Compute the highlight range for a pattern_range within the candidate string.
    let make_range = |pattern_range: &Range<usize>| -> Range<text::Anchor> {
        let offset_range = range.to_offset(&snapshot);
        let actual_start = offset_range.start + pattern_range.start;
        let actual_end = offset_range.end - (candidate_len - pattern_range.end);
        snapshot.anchor_before(actual_start)..snapshot.anchor_after(actual_end)
    };

    // For each candidate extracted by link_pattern_file_candidates, try resolving in order:
    // 1. The raw candidate string
    // 2. The path portion after stripping `:row:col` suffix
    // 3. With language-specific file extensions appended to raw candidate
    // 4. With language-specific file extensions appended to stripped path
    for (pattern_candidate, pattern_range) in &pattern_candidates {
        // Try the raw candidate first.
        if let Some(existing_path) = check_path(&pattern_candidate, &project, buffer, cx).await {
            return Some((
                make_range(pattern_range),
                ResolvedFileTarget {
                    resolved_path: existing_path,
                    row: None,
                    column: None,
                },
            ));
        }

        // Parse row:col suffix once per candidate for use in fallback attempts.
        // This handles patterns like `file.rs:83:1`, `file.rs:83`, and `file.rs:20:in`.
        let parsed = PathWithPosition::parse_str(pattern_candidate);
        let parsed_path = parsed.path.to_string_lossy();

        // Try resolving just the path portion (without :row:col).
        if parsed.row.is_some() {
            if let Some(existing_path) = check_path(&parsed_path, &project, buffer, cx).await {
                return Some((
                    make_range(pattern_range),
                    ResolvedFileTarget {
                        resolved_path: existing_path,
                        row: parsed.row,
                        column: parsed.column,
                    },
                ));
            }
        }

        // Try with language-specific suffixes.
        if let Some(scope) = &scope {
            for suffix in scope.path_suffixes() {
                if pattern_candidate.ends_with(format!(".{suffix}").as_str()) {
                    continue;
                }

                let suffixed_candidate = format!("{pattern_candidate}.{suffix}");
                if let Some(existing_path) =
                    check_path(&suffixed_candidate, &project, buffer, cx).await
                {
                    return Some((
                        make_range(pattern_range),
                        ResolvedFileTarget {
                            resolved_path: existing_path,
                            row: None,
                            column: None,
                        },
                    ));
                }
            }

            // Try with language-specific suffixes on the stripped path.
            if parsed.row.is_some() {
                for suffix in scope.path_suffixes() {
                    if parsed_path.ends_with(&format!(".{suffix}")) {
                        continue;
                    }

                    let suffixed_candidate = format!("{parsed_path}.{suffix}");
                    if let Some(existing_path) =
                        check_path(&suffixed_candidate, &project, buffer, cx).await
                    {
                        return Some((
                            make_range(pattern_range),
                            ResolvedFileTarget {
                                resolved_path: existing_path,
                                row: parsed.row,
                                column: parsed.column,
                            },
                        ));
                    }
                }
            }
        }
    }
    None
}

// Generates candidate file paths by stripping common punctuation wrappers.
// Handles markdown patterns like [title](path), `path`, (path), as well as
// partial wrappers where punctuation only appears on one side (e.g. path) or path`).
// Returns candidates ordered from most-specific (most trimmed) to least-specific (raw).
fn link_pattern_file_candidates(candidate: &str) -> Vec<(String, Range<usize>)> {
    static MD_LINK_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"]\(([^)]*)\)").expect("Failed to create REGEX"));

    // Punctuation that commonly wraps file paths in prose/markdown
    const LEADING_PUNCTUATION: &[char] = &['`', '(', '[', '{', '<', '"', '\''];
    const TRAILING_PUNCTUATION: &[char] = &[
        '`', ')', ']', '}', '>', '"', '\'', '.', ',', ':', ';', '!', '?',
    ];

    let candidate_len = candidate.len();
    let mut candidates = Vec::new();

    // Trim leading and trailing punctuation iteratively
    let mut start = 0;
    let mut end = candidate_len;

    // Trim leading punctuation
    for ch in candidate.chars() {
        if LEADING_PUNCTUATION.contains(&ch) {
            start += ch.len_utf8();
        } else {
            break;
        }
    }

    // Trim trailing punctuation
    for ch in candidate.chars().rev() {
        if TRAILING_PUNCTUATION.contains(&ch) {
            end -= ch.len_utf8();
        } else {
            break;
        }
    }

    // Add trimmed candidate first (highest priority) if it differs from original
    if start < end && (start > 0 || end < candidate_len) {
        candidates.push((candidate[start..end].to_string(), start..end));
    }

    // Extract markdown link destination: [title](path) or ](path) -> path
    // This also handles bare (path) wrapping.
    if let Some(captures) = MD_LINK_REGEX.captures(candidate) {
        if let Some(link) = captures.get(1) {
            let link_str = link.as_str().to_string();
            let link_range = link.range();
            // Avoid duplicate if punctuation trimming already found this
            if !candidates.iter().any(|(s, _)| s == &link_str) {
                candidates.push((link_str, link_range));
            }
        }
    }

    // Always include the raw candidate as fallback (lowest priority)
    candidates.push((candidate.to_string(), 0..candidate_len));

    candidates
}

fn surrounding_filename(
    snapshot: &language::BufferSnapshot,
    position: text::Anchor,
) -> Option<(Range<text::Anchor>, String)> {
    const LIMIT: usize = 2048;

    let offset = position.to_offset(&snapshot);
    let mut token_start = offset;
    let mut token_end = offset;
    let mut found_start = false;
    let mut found_end = false;
    let mut inside_quotes = false;

    let mut filename = String::new();

    let mut backwards = snapshot.reversed_chars_at(offset).take(LIMIT).peekable();
    while let Some(ch) = backwards.next() {
        // Escaped whitespace
        if ch.is_whitespace() && backwards.peek() == Some(&'\\') {
            filename.push(ch);
            token_start -= ch.len_utf8();
            backwards.next();
            token_start -= '\\'.len_utf8();
            continue;
        }
        if ch.is_whitespace() {
            found_start = true;
            break;
        }
        // Quote characters open a quoted region that is stripped from the
        // returned filename. Backticks and parens are NOT treated this way —
        // they are kept as part of the token so that downstream candidate
        // generation (link_pattern_file_candidates) can trim them and produce
        // a tight highlight range via make_range.
        if (ch == '"' || ch == '\'') && !inside_quotes {
            found_start = true;
            inside_quotes = true;
            break;
        }

        filename.push(ch);
        token_start -= ch.len_utf8();
    }
    if !found_start && token_start != 0 {
        return None;
    }

    filename = filename.chars().rev().collect();

    let mut forwards = snapshot
        .chars_at(offset)
        .take(LIMIT - (offset - token_start))
        .peekable();
    while let Some(ch) = forwards.next() {
        // Skip escaped whitespace
        if ch == '\\' && forwards.peek().is_some_and(|ch| ch.is_whitespace()) {
            token_end += ch.len_utf8();
            let whitespace = forwards.next().unwrap();
            token_end += whitespace.len_utf8();
            filename.push(whitespace);
            continue;
        }

        if ch.is_whitespace() {
            found_end = true;
            break;
        }
        if ch == '"' || ch == '\'' {
            // If we're inside quotes, we stop when we come across the next quote
            if inside_quotes {
                found_end = true;
                break;
            } else {
                // Otherwise, we skip the quote
                inside_quotes = true;
                token_end += ch.len_utf8();
                continue;
            }
        }
        filename.push(ch);
        token_end += ch.len_utf8();
    }

    if !found_end && (token_end - token_start >= LIMIT) {
        return None;
    }

    if filename.is_empty() {
        return None;
    }

    let range = snapshot.anchor_before(token_start)..snapshot.anchor_after(token_end);

    Some((range, filename))
}

