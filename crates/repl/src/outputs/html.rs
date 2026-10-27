use anyhow::Result;
use html_to_markdown::markdown::{
    CodeHandler, HeadingHandler, ListHandler, ParagraphHandler, StyledTextHandler, TableHandler,
    WebpageChromeRemover,
};
use html_to_markdown::{TagHandler, convert_html_to_markdown};
use std::cell::RefCell;
use std::rc::Rc;

/// Convert HTML to Markdown for rendering in the REPL.
pub fn html_to_markdown(html: &str) -> Result<String> {
    let mut handlers: Vec<TagHandler> = vec![
        // WebpageChromeRemover must come first to skip style, script, head, nav tags
        Rc::new(RefCell::new(WebpageChromeRemover)),
        Rc::new(RefCell::new(ParagraphHandler)),
        Rc::new(RefCell::new(HeadingHandler)),
        Rc::new(RefCell::new(ListHandler)),
        Rc::new(RefCell::new(TableHandler::new())),
        Rc::new(RefCell::new(StyledTextHandler)),
        Rc::new(RefCell::new(CodeHandler)),
    ];

    let markdown = convert_html_to_markdown(html.as_bytes(), &mut handlers)?;
    Ok(clean_markdown_tables(&markdown))
}

/// Clean up markdown table formatting and ensure tables have separator rows.
fn clean_markdown_tables(markdown: &str) -> String {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut result: Vec<String> = Vec::new();
    let mut in_table = false;
    let mut has_separator = false;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();

        if trimmed.starts_with('|') {
            let normalized = normalize_table_row(trimmed);

            if !in_table {
                // Starting a new table
                in_table = true;
                has_separator = false;
            }

            // Check if this line is a separator row
            if trimmed.contains("---") {
                has_separator = true;
            }

            result.push(normalized.clone());

            // If this is the first row and no separator exists yet,
            // check if next row is a table row (not separator) and add one
            if !has_separator {
                let next_is_table_row = i + 1 < lines.len()
                    && lines[i + 1].trim().starts_with('|')
                    && !lines[i + 1].contains("---");

                if next_is_table_row {
                    // Insert separator after first row
                    let col_count = normalized.matches('|').count().saturating_sub(1);
                    if col_count > 0 {
                        let separator = (0..col_count)
                            .map(|_| "---")
                            .collect::<Vec<_>>()
                            .join(" | ");
                        result.push(format!("| {} |", separator));
                        has_separator = true;
                    }
                }
            }
        } else {
            // Not a table row
            if !trimmed.is_empty() {
                result.push(trimmed.to_string());
            }
            in_table = false;
            has_separator = false;
        }
    }

    result.join("\n")
}

/// Normalize a table row by trimming cells and ensuring consistent spacing.
fn normalize_table_row(row: &str) -> String {
    let parts: Vec<&str> = row.split('|').collect();
    let normalized: Vec<String> = parts.iter().map(|cell| cell.trim().to_string()).collect();
    normalized.join(" | ").trim().to_string()
}

