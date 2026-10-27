use regex::Regex;
use std::{num::NonZeroU32, sync::LazyLock};

/// The settings extracted from an emacs/vim modelines.
///
/// The parsing tries to best match the modeline directives and
/// variables to Zed, matching LanguageSettings fields.
/// The mode mapping is done later thanks to the LanguageRegistry.
///
/// It is not exhaustive, but covers the most common settings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelineSettings {
    /// The emacs mode or vim filetype.
    pub mode: Option<String>,
    /// How many columns a tab should occupy.
    pub tab_size: Option<NonZeroU32>,
    /// Whether to indent lines using tab characters, as opposed to multiple
    /// spaces.
    pub hard_tabs: Option<bool>,
    /// The number of bytes that comprise the indentation.
    pub indent_size: Option<NonZeroU32>,
    /// Whether to auto-indent lines.
    pub auto_indent: Option<bool>,
    /// The column at which to soft-wrap lines.
    pub preferred_line_length: Option<NonZeroU32>,
    /// Whether to ensure a final newline at the end of the file.
    pub ensure_final_newline: Option<bool>,
    /// Whether to show trailing whitespace on the editor.
    pub show_trailing_whitespace: Option<bool>,

    /// Emacs modeline variables that were parsed but not mapped to Zed settings.
    /// Stored as (variable-name, value) pairs.
    pub emacs_extra_variables: Vec<(String, String)>,
    /// Vim modeline options that were parsed but not mapped to Zed settings.
    /// Stored as (option-name, value) pairs.
    pub vim_extra_variables: Vec<(String, Option<String>)>,
}

impl ModelineSettings {
    fn has_settings(&self) -> bool {
        self != &Self::default()
    }
}

/// Parse modelines from file content.
///
/// Supports:
/// - Emacs modelines: -*- mode: rust; tab-width: 4; indent-tabs-mode: nil; -*- and "Local Variables"
/// - Vim modelines: vim: set ft=rust ts=4 sw=4 et:
pub fn parse_modeline(first_lines: &[&str], last_lines: &[&str]) -> Option<ModelineSettings> {
    let mut settings = ModelineSettings::default();

    parse_modelines(first_lines, &mut settings);

    // Parse Emacs Local Variables in last lines
    parse_emacs_local_variables(last_lines, &mut settings);

    // Also check for vim modelines in last lines if we don't have settings yet
    if !settings.has_settings() {
        parse_vim_modelines(last_lines, &mut settings);
    }

    Some(settings).filter(|s| s.has_settings())
}

fn parse_modelines(modelines: &[&str], settings: &mut ModelineSettings) {
    for line in modelines {
        parse_emacs_modeline(line, settings);
        // if emacs is set, do not check for vim modelines
        if settings.has_settings() {
            return;
        }
    }

    parse_vim_modelines(modelines, settings);
}

static EMACS_MODELINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-\*-\s*(.+?)\s*-\*-").expect("valid regex"));

/// Parse Emacs-style modelines
/// Format: -*- mode: rust; tab-width: 4; indent-tabs-mode: nil; -*-
/// See Emacs (set-auto-mode)
fn parse_emacs_modeline(line: &str, settings: &mut ModelineSettings) {
    let Some(captures) = EMACS_MODELINE_RE.captures(line) else {
        return;
    };
    let Some(modeline_content) = captures.get(1).map(|m| m.as_str()) else {
        return;
    };
    for part in modeline_content.split(';') {
        parse_emacs_key_value(part, settings, true);
    }
}

/// Parse Emacs-style Local Variables block
///
/// Emacs supports a "Local Variables" block at the end of files:
/// ```text
/// /* Local Variables: */
/// /* mode: c */
/// /* tab-width: 4 */
/// /* End: */
/// ```
///
/// Emacs related code is hack-local-variables--find-variables in
/// https://cgit.git.savannah.gnu.org/cgit/emacs.git/tree/lisp/files.el#n4346
fn parse_emacs_local_variables(lines: &[&str], settings: &mut ModelineSettings) {
    const LOCAL_VARIABLES: &str = "Local Variables:";

    let Some((start_idx, prefix, suffix)) = lines.iter().enumerate().find_map(|(i, line)| {
        let prefix_len = line.find(LOCAL_VARIABLES)?;
        let suffix_start = prefix_len + LOCAL_VARIABLES.len();
        Some((i, line.get(..prefix_len)?, line.get(suffix_start..)?))
    }) else {
        return;
    };

    let mut continuation = String::new();

    for line in &lines[start_idx + 1..] {
        let Some(content) = line
            .strip_prefix(prefix)
            .and_then(|l| l.strip_suffix(suffix))
            .map(str::trim)
        else {
            return;
        };

        if let Some(continued) = content.strip_suffix('\\') {
            continuation.push_str(continued);
            continue;
        }

        let to_parse = if continuation.is_empty() {
            content
        } else {
            continuation.push_str(content);
            &continuation
        };

        if to_parse == "End:" {
            return;
        }

        parse_emacs_key_value(to_parse, settings, false);
        continuation.clear();
    }
}

fn parse_emacs_key_value(part: &str, settings: &mut ModelineSettings, bare: bool) {
    let part = part.trim();
    if part.is_empty() {
        return;
    }

    if let Some((key, value)) = part.split_once(':') {
        let key = key.trim();
        let value = value.trim();

        match key.to_lowercase().as_str() {
            "mode" => {
                settings.mode = Some(value.to_string());
            }
            "c-basic-offset" | "python-indent-offset" => {
                if let Ok(size) = value.parse::<NonZeroU32>() {
                    settings.indent_size = Some(size);
                }
            }
            "fill-column" => {
                if let Ok(size) = value.parse::<NonZeroU32>() {
                    settings.preferred_line_length = Some(size);
                }
            }
            "tab-width" => {
                if let Ok(size) = value.parse::<NonZeroU32>() {
                    settings.tab_size = Some(size);
                }
            }
            "indent-tabs-mode" => {
                settings.hard_tabs = Some(value != "nil");
            }
            "electric-indent-mode" => {
                settings.auto_indent = Some(value != "nil");
            }
            "require-final-newline" => {
                settings.ensure_final_newline = Some(value != "nil");
            }
            "show-trailing-whitespace" => {
                settings.show_trailing_whitespace = Some(value != "nil");
            }
            key => settings
                .emacs_extra_variables
                .push((key.to_string(), value.to_string())),
        }
    } else if bare {
        // Handle bare mode specification (e.g., -*- rust -*-)
        settings.mode = Some(part.to_string());
    }
}

fn parse_vim_modelines(modelines: &[&str], settings: &mut ModelineSettings) {
    for line in modelines {
        parse_vim_modeline(line, settings);
    }
}

// Vim requires whitespace before the `{vi:|vim:|ex:}` marker. As an exception,
// `vi:` and `vim:` may also appear at the start of a line, but `ex:` may not as
// Vim ignores a leading `ex:` since it could be short for `example:`.
static VIM_MODELINE_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // Second form: [text{white}]{vi:|vim:|Vim:|ex:}[white]se[t] {options}:[text]
        // Allow escaped colons in options: match non-colon chars or backslash followed by any char
        r"(?:(?:^|\s)(?:vi|vim|Vim)|\sex):\s*se(?:t)?\s+((?:[^\\:]|\\.)*):",
        // First form: [text{white}]{vi:|vim:|ex:}[white]{options}
        r"(?:(?:^|\s)(?:vi|vim)|\sex):\s*(.+)",
    ]
    .iter()
    .map(|pattern| Regex::new(pattern).expect("valid regex"))
    .collect()
});

/// Parse Vim-style modelines
/// Supports both forms:
/// 1. First form: vi:noai:sw=3 ts=6
/// 2. Second form: vim: set ft=rust ts=4 sw=4 et:
fn parse_vim_modeline(line: &str, settings: &mut ModelineSettings) {
    for re in VIM_MODELINE_PATTERNS.iter() {
        if let Some(captures) = re.captures(line) {
            if let Some(options) = captures.get(1) {
                parse_vim_settings(options.as_str().trim(), settings);
                break;
            }
        }
    }
}

fn parse_vim_settings(content: &str, settings: &mut ModelineSettings) {
    fn split_colon_unescape(input: &str) -> Vec<String> {
        let mut split = Vec::new();
        let mut str = String::new();
        let mut chars = input.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some(escaped_char) => str.push(escaped_char),
                    None => str.push('\\'),
                }
            } else if c == ':' {
                split.push(std::mem::take(&mut str));
            } else {
                str.push(c);
            }
        }
        split.push(str);
        split
    }

    let parts = split_colon_unescape(content);
    for colon_part in parts {
        let colon_part = colon_part.trim();
        if colon_part.is_empty() {
            continue;
        }

        // Each colon part might contain space-separated options
        for part in colon_part.split_whitespace() {
            if let Some((key, value)) = part.split_once('=') {
                match key {
                    "ft" | "filetype" => {
                        settings.mode = Some(value.to_string());
                    }
                    "ts" | "tabstop" => {
                        if let Ok(size) = value.parse::<NonZeroU32>() {
                            settings.tab_size = Some(size);
                        }
                    }
                    "sw" | "shiftwidth" => {
                        if let Ok(size) = value.parse::<NonZeroU32>() {
                            settings.indent_size = Some(size);
                        }
                    }
                    "tw" | "textwidth" => {
                        if let Ok(size) = value.parse::<NonZeroU32>() {
                            settings.preferred_line_length = Some(size);
                        }
                    }
                    _ => {
                        settings
                            .vim_extra_variables
                            .push((key.to_string(), Some(value.to_string())));
                    }
                }
            } else {
                match part {
                    "ai" | "autoindent" => {
                        settings.auto_indent = Some(true);
                    }
                    "noai" | "noautoindent" => {
                        settings.auto_indent = Some(false);
                    }
                    "et" | "expandtab" => {
                        settings.hard_tabs = Some(false);
                    }
                    "noet" | "noexpandtab" => {
                        settings.hard_tabs = Some(true);
                    }
                    "eol" | "endofline" => {
                        settings.ensure_final_newline = Some(true);
                    }
                    "noeol" | "noendofline" => {
                        settings.ensure_final_newline = Some(false);
                    }
                    "set" => {
                        // Ignore the "set" keyword itself
                    }
                    _ => {
                        settings.vim_extra_variables.push((part.to_string(), None));
                    }
                }
            }
        }
    }
}

