use indexmap::{IndexMap, IndexSet};

use anyhow::Context as _;
use serde_json::value::RawValue;

use crate::{
    TabularDataPreviewPane,
    types::TableLikeContent,
    types::{LineNumber, TableCell},
};
use editor::Editor;
use gpui::{App, AppContext, Context, Entity, Subscription, Task};
use std::path::Path;
use std::time::{Duration, Instant};
use text::BufferSnapshot;
use ui::{SharedString, table_row::TableRow};

pub(crate) const REPARSE_DEBOUNCE: Duration = Duration::from_millis(200);

pub(crate) struct EditorState {
    pub editor: Entity<Editor>,
    pub _subscription: Subscription,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TabularFormat {
    Csv,
    Tsv,
    Psv,
    Ssv,
    JsonLines,
}

const TABULAR_FORMATS: &[(&str, TabularFormat)] = &[
    ("csv", TabularFormat::Csv),
    ("tsv", TabularFormat::Tsv),
    ("psv", TabularFormat::Psv),
    ("ssv", TabularFormat::Ssv),
    ("jsonl", TabularFormat::JsonLines),
    ("ndjson", TabularFormat::JsonLines),
];

impl TabularFormat {
    pub(crate) fn from_editor(editor: &Entity<Editor>, cx: &App) -> Option<Self> {
        Self::from_extension(editor_file_extension(editor, cx)?)
    }

    pub(crate) fn from_extension(ext: &str) -> Option<Self> {
        TABULAR_FORMATS
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(ext))
            .map(|(_, format)| *format)
    }

    fn parse(self, buffer_snapshot: &BufferSnapshot) -> anyhow::Result<TableLikeContent> {
        let delimiter = match self {
            Self::Csv => ',',
            Self::Tsv => '\t',
            Self::Psv => '|',
            Self::Ssv => ';',
            Self::JsonLines => return from_json_lines(buffer_snapshot),
        };
        Ok(from_buffer_with_delimiter(buffer_snapshot, delimiter))
    }
}

pub(crate) fn editor_file_extension<'a>(editor: &Entity<Editor>, cx: &'a App) -> Option<&'a str> {
    let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
    let file = buffer.read(cx).file()?;
    Path::new(file.file_name(cx)).extension()?.to_str()
}

impl TabularDataPreviewPane {
    pub(crate) fn parse_from_active_editor(
        &mut self,
        wait_for_debounce: bool,
        cx: &mut Context<Self>,
    ) {
        let editor = self.active_editor_state.editor.clone();
        self.is_parsing = true;
        self.table
            .update(cx, |table, cx| table.set_loading(true, cx));
        self.parsing_task = Some(self.parse_in_background(wait_for_debounce, editor, cx));
    }

    fn parse_in_background(
        &mut self,
        wait_for_debounce: bool,
        editor: Entity<Editor>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        cx.spawn(async move |view, cx| {
            if wait_for_debounce {
                // Smart debouncing: check if cooldown period has already passed
                let now = Instant::now();
                let should_wait = view.update(cx, |view, _| {
                    if let Some(last_end) = view.last_parse_end_time {
                        let cooldown_until = last_end + REPARSE_DEBOUNCE;
                        if now < cooldown_until {
                            Some(cooldown_until - now)
                        } else {
                            None // Cooldown already passed, parse immediately
                        }
                    } else {
                        None // First parse, no debounce
                    }
                })?;

                if let Some(wait_duration) = should_wait {
                    cx.background_executor().timer(wait_duration).await;
                }
            }

            let (buffer_snapshot, format) = view.update(cx, |_, cx| {
                let buffer_ref = editor
                    .read(cx)
                    .buffer()
                    .read(cx)
                    .as_singleton()
                    .map(|b| b.read(cx).text_snapshot());

                let extension = editor_file_extension(&editor, cx);

                let format = extension
                    .and_then(TabularFormat::from_extension)
                    .unwrap_or_else(|| {
                        log::warn!(
                            "unrecognized tabular data extension {extension:?}, defaulting to CSV"
                        );
                        TabularFormat::Csv
                    });

                (buffer_ref, format)
            })?;

            let Some(buffer_snapshot) = buffer_snapshot else {
                view.update(cx, |view, cx| {
                    view.is_parsing = false;
                    view.parse_error = Some("Preview requires a single file".into());
                    cx.notify();
                })?;
                return Ok(());
            };

            let instant = Instant::now();
            let parsed_contents = cx
                .background_spawn(async move { format.parse(&buffer_snapshot) })
                .await;
            let parse_duration = instant.elapsed();
            let parse_end_time: Instant = Instant::now();
            log::debug!("Parsed data in {}ms", parse_duration.as_millis());
            view.update(cx, move |view, cx| {
                view.last_parse_end_time = Some(parse_end_time);
                view.is_parsing = false;
                let parsed_contents = match parsed_contents {
                    Ok(contents) => contents,
                    Err(error) => {
                        view.parse_error = Some(format!("{error:#}").into());
                        view.table
                            .update(cx, |table, cx| table.set_loading(false, cx));
                        cx.notify();
                        return;
                    }
                };

                log::debug!("Parsed {} rows", parsed_contents.rows.len());
                view.parse_error = None;
                view.table.update(cx, |table, cx| {
                    table
                        .performance_metrics
                        .timings
                        .insert("Parsing", (parse_duration, Instant::now()));
                    // `set_contents` recomputes filters/widths/mapping and clears the loading state.
                    table.set_contents(parsed_contents, cx);
                });
                cx.notify();
            })
        })
    }
}

fn from_json_lines(buffer_snapshot: &BufferSnapshot) -> anyhow::Result<TableLikeContent> {
    let text = buffer_snapshot.text();
    let mut records = Vec::new();
    let mut columns = IndexSet::new();
    let mut line_numbers = Vec::new();

    for (line_index, line) in text.lines().enumerate() {
        // Ignore blank lines, but keep physical line numbers for the source gutter.
        if line.trim().is_empty() {
            continue;
        }
        let record: IndexMap<String, &RawValue> =
            serde_json::from_str(line).with_context(|| {
                format!(
                    "Cannot preview JSONL line {}: expected a JSON object",
                    line_index + 1
                )
            })?;
        columns.extend(record.keys().cloned());
        records.push(record);
        line_numbers.push(LineNumber::Line(line_index + 1));
    }

    let number_of_cols = columns.len();
    let headers = TableRow::from_vec(
        columns
            .iter()
            .map(|name| TableCell::Generated(name.clone().into()))
            .collect(),
        number_of_cols,
    );
    let rows = records
        .into_iter()
        .map(|record| {
            let cells = columns
                .iter()
                .map(|name| {
                    let Some(value) = record.get(name) else {
                        return Ok(TableCell::Virtual);
                    };
                    let raw = value.get();
                    // RawValue borrows the original text, so its span stays exact even
                    // for repeated values, escaped strings, and large numbers.
                    let start = (raw.as_ptr() as usize)
                        .checked_sub(text.as_ptr() as usize)
                        .context("JSON value is outside the source buffer")?;
                    Ok(TableCell::from_buffer_position(
                        compact_json(raw).into(),
                        start,
                        start + raw.len(),
                        buffer_snapshot,
                    ))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(TableRow::from_vec(cells, number_of_cols))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    Ok(TableLikeContent {
        headers,
        rows,
        line_numbers,
        number_of_cols,
    })
}

fn compact_json(raw: &str) -> String {
    // Reserializing through Value can round numbers. Remove only insignificant
    // whitespace from the already validated JSON instead.
    let mut compact = String::with_capacity(raw.len());
    let mut in_string = false;
    let mut escaped = false;
    for character in raw.chars() {
        if in_string {
            compact.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
        } else if character == '"' {
            in_string = true;
            compact.push(character);
        } else if !matches!(character, ' ' | '\t' | '\r' | '\n') {
            compact.push(character);
        }
    }
    compact
}

pub fn from_buffer_with_delimiter(
    buffer_snapshot: &BufferSnapshot,
    delimiter: char,
) -> TableLikeContent {
    let text = buffer_snapshot.text();

    if text.trim().is_empty() {
        return TableLikeContent::default();
    }

    let (parsed_cells_with_positions, line_numbers) =
        parse_delimited_text_with_positions(&text, delimiter);
    if parsed_cells_with_positions.is_empty() {
        return TableLikeContent::default();
    }
    let raw_headers = parsed_cells_with_positions[0].clone();

    // Calculating the longest row, as the data might have fewer headers than max row width
    let Some(max_number_of_cols) = parsed_cells_with_positions.iter().map(|r| r.len()).max() else {
        return TableLikeContent::default();
    };

    // Convert to TableCell objects with buffer positions
    let headers = create_table_row(&buffer_snapshot, max_number_of_cols, raw_headers);

    let rows = parsed_cells_with_positions
        .into_iter()
        .skip(1)
        .map(|row| create_table_row(&buffer_snapshot, max_number_of_cols, row))
        .collect();

    let row_line_numbers = line_numbers.into_iter().skip(1).collect();

    TableLikeContent {
        headers,
        rows,
        line_numbers: row_line_numbers,
        number_of_cols: max_number_of_cols,
    }
}

/// Parse delimited text and track byte positions for each cell
fn parse_delimited_text_with_positions(
    text: &str,
    delimiter: char,
) -> (
    Vec<Vec<(SharedString, std::ops::Range<usize>)>>,
    Vec<LineNumber>,
) {
    let mut rows = Vec::new();
    let mut line_numbers = Vec::new();
    let mut current_row: Vec<(SharedString, std::ops::Range<usize>)> = Vec::new();
    let mut current_field = String::new();
    let mut field_start_offset = 0;
    let mut current_offset = 0;
    let mut in_quotes = false;
    let mut current_line = 1; // 1-based line numbering
    let mut row_start_line = 1;
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        let char_byte_len = ch.len_utf8();

        match ch {
            '"' => {
                if in_quotes {
                    if chars.peek() == Some(&'"') {
                        // Escaped quote
                        chars.next();
                        current_field.push('"');
                        current_offset += 1; // Skip the second quote
                    } else {
                        // End of quoted field
                        in_quotes = false;
                    }
                } else {
                    // Start of quoted field
                    in_quotes = true;
                    if current_field.is_empty() {
                        // Include the opening quote in the range
                        field_start_offset = current_offset;
                    }
                }
            }
            c if c == delimiter && !in_quotes => {
                // Field separator
                let field_end_offset = current_offset;
                if current_field.is_empty() && !in_quotes {
                    field_start_offset = current_offset;
                }
                current_row.push((
                    current_field.clone().into(),
                    field_start_offset..field_end_offset,
                ));
                current_field.clear();
                field_start_offset = current_offset + char_byte_len;
            }
            '\n' => {
                current_line += 1;
                if !in_quotes {
                    // Row separator (only when not inside quotes)
                    let field_end_offset = current_offset;
                    if current_field.is_empty() && current_row.is_empty() {
                        field_start_offset = 0;
                    }
                    current_row.push((
                        current_field.clone().into(),
                        field_start_offset..field_end_offset,
                    ));
                    current_field.clear();

                    // Only add non-empty rows
                    if !current_row.is_empty()
                        && !current_row.iter().all(|(field, _)| field.trim().is_empty())
                    {
                        rows.push(current_row);
                        // Add line number info for this row
                        let line_info = if row_start_line == current_line - 1 {
                            LineNumber::Line(row_start_line)
                        } else {
                            LineNumber::LineRange(row_start_line, current_line - 1)
                        };
                        line_numbers.push(line_info);
                    }
                    current_row = Vec::new();
                    row_start_line = current_line;
                    field_start_offset = current_offset + char_byte_len;
                } else {
                    // Newline inside quotes - preserve it
                    current_field.push(ch);
                }
            }
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    // Handle Windows line endings (\r\n): account for \r byte, let \n be handled next
                    current_offset += char_byte_len;
                    continue;
                } else {
                    // Standalone \r
                    current_line += 1;
                    if !in_quotes {
                        // Row separator (only when not inside quotes)
                        let field_end_offset = current_offset;
                        current_row.push((
                            current_field.clone().into(),
                            field_start_offset..field_end_offset,
                        ));
                        current_field.clear();

                        // Only add non-empty rows
                        if !current_row.is_empty()
                            && !current_row.iter().all(|(field, _)| field.trim().is_empty())
                        {
                            rows.push(current_row);
                            // Add line number info for this row
                            let line_info = if row_start_line == current_line - 1 {
                                LineNumber::Line(row_start_line)
                            } else {
                                LineNumber::LineRange(row_start_line, current_line - 1)
                            };
                            line_numbers.push(line_info);
                        }
                        current_row = Vec::new();
                        row_start_line = current_line;
                        field_start_offset = current_offset + char_byte_len;
                    } else {
                        // \r inside quotes - preserve it
                        current_field.push(ch);
                    }
                }
            }
            _ => {
                if current_field.is_empty() && !in_quotes {
                    field_start_offset = current_offset;
                }
                current_field.push(ch);
            }
        }

        current_offset += char_byte_len;
    }

    // Add the last field and row if not empty
    if !current_field.is_empty() || !current_row.is_empty() {
        let field_end_offset = current_offset;
        current_row.push((
            current_field.clone().into(),
            field_start_offset..field_end_offset,
        ));
    }
    if !current_row.is_empty() && !current_row.iter().all(|(field, _)| field.trim().is_empty()) {
        rows.push(current_row);
        // Add line number info for the last row
        let line_info = if row_start_line == current_line {
            LineNumber::Line(row_start_line)
        } else {
            LineNumber::LineRange(row_start_line, current_line)
        };
        line_numbers.push(line_info);
    }

    (rows, line_numbers)
}

fn create_table_row(
    buffer_snapshot: &BufferSnapshot,
    max_number_of_cols: usize,
    row: Vec<(SharedString, std::ops::Range<usize>)>,
) -> TableRow<TableCell> {
    let mut raw_row = row
        .into_iter()
        .map(|(content, range)| {
            TableCell::from_buffer_position(content, range.start, range.end, &buffer_snapshot)
        })
        .collect::<Vec<_>>();

    let append_elements = max_number_of_cols - raw_row.len();
    if append_elements > 0 {
        for _ in 0..append_elements {
            raw_row.push(TableCell::Virtual);
        }
    }

    TableRow::from_vec(raw_row, max_number_of_cols)
}


impl TableLikeContent {

}
