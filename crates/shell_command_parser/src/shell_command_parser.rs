use brush_parser::ast;
use brush_parser::ast::SourceLocation;
use brush_parser::word::WordPiece;
use brush_parser::{Parser, ParserOptions, SourceInfo};
use std::io::BufReader;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalCommandPrefix {
    pub normalized: String,
    pub display: String,
    pub tokens: Vec<String>,
    pub command: String,
    pub subcommand: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalCommandValidation {
    Safe,
    Unsafe,
    Unsupported,
}

pub fn extract_commands(command: &str) -> Option<Vec<String>> {
    let reader = BufReader::new(command.as_bytes());
    let options = ParserOptions::default();
    let source_info = SourceInfo::default();
    let mut parser = Parser::new(reader, &options, &source_info);

    let program = parser.parse_program().ok()?;

    let mut commands = Vec::new();
    extract_commands_from_program(&program, &mut commands)?;

    Some(commands)
}

pub fn extract_terminal_command_prefix(command: &str) -> Option<TerminalCommandPrefix> {
    let reader = BufReader::new(command.as_bytes());
    let options = ParserOptions::default();
    let source_info = SourceInfo::default();
    let mut parser = Parser::new(reader, &options, &source_info);

    let program = parser.parse_program().ok()?;
    let simple_command = first_simple_command(&program)?;

    let mut normalized_tokens = Vec::new();
    let mut display_start = None;
    let mut display_end = None;

    if let Some(prefix) = &simple_command.prefix {
        for item in &prefix.0 {
            if let ast::CommandPrefixOrSuffixItem::AssignmentWord(assignment, word) = item {
                match normalize_assignment_for_command_prefix(assignment, word)? {
                    NormalizedAssignment::Included(normalized_assignment) => {
                        normalized_tokens.push(normalized_assignment);
                        update_display_bounds(&mut display_start, &mut display_end, word);
                    }
                    NormalizedAssignment::Skipped => {}
                }
            }
        }
    }

    let command_word = simple_command.word_or_name.as_ref()?;
    let command_name = normalize_word(command_word)?;
    normalized_tokens.push(command_name.clone());
    update_display_bounds(&mut display_start, &mut display_end, command_word);

    let mut subcommand = None;
    if let Some(suffix) = &simple_command.suffix {
        for item in &suffix.0 {
            match item {
                ast::CommandPrefixOrSuffixItem::IoRedirect(_) => continue,
                ast::CommandPrefixOrSuffixItem::Word(word) => {
                    let normalized_word = normalize_word(word)?;
                    if !normalized_word.starts_with('-') {
                        subcommand = Some(normalized_word.clone());
                        normalized_tokens.push(normalized_word);
                        update_display_bounds(&mut display_start, &mut display_end, word);
                    }
                    break;
                }
                _ => break,
            }
        }
    }

    let start = display_start?;
    let end = display_end?;
    let display = command.get(start..end)?.to_string();

    Some(TerminalCommandPrefix {
        normalized: normalized_tokens.join(" "),
        display,
        tokens: normalized_tokens,
        command: command_name,
        subcommand,
    })
}

pub fn validate_terminal_command(command: &str) -> TerminalCommandValidation {
    let reader = BufReader::new(command.as_bytes());
    let options = ParserOptions::default();
    let source_info = SourceInfo::default();
    let mut parser = Parser::new(reader, &options, &source_info);

    let program = match parser.parse_program() {
        Ok(program) => program,
        Err(_) => return TerminalCommandValidation::Unsupported,
    };

    match program_validation(&program) {
        TerminalProgramValidation::Safe => TerminalCommandValidation::Safe,
        TerminalProgramValidation::Unsafe => TerminalCommandValidation::Unsafe,
        TerminalProgramValidation::Unsupported => TerminalCommandValidation::Unsupported,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalProgramValidation {
    Safe,
    Unsafe,
    Unsupported,
}

fn first_simple_command(program: &ast::Program) -> Option<&ast::SimpleCommand> {
    let complete_command = program.complete_commands.first()?;
    let compound_list_item = complete_command.0.first()?;
    let command = compound_list_item.0.first.seq.first()?;

    match command {
        ast::Command::Simple(simple_command) => Some(simple_command),
        _ => None,
    }
}

fn update_display_bounds(start: &mut Option<usize>, end: &mut Option<usize>, word: &ast::Word) {
    if let Some(location) = word.location() {
        let word_start = location.start.index;
        let word_end = location.end.index;
        *start = Some(start.map_or(word_start, |current| current.min(word_start)));
        *end = Some(end.map_or(word_end, |current| current.max(word_end)));
    }
}

enum NormalizedAssignment {
    Included(String),
    Skipped,
}

fn normalize_assignment_for_command_prefix(
    assignment: &ast::Assignment,
    word: &ast::Word,
) -> Option<NormalizedAssignment> {
    let operator = if assignment.append { "+=" } else { "=" };
    let assignment_prefix = format!("{}{}", assignment.name, operator);

    match &assignment.value {
        ast::AssignmentValue::Scalar(value) => {
            let normalized_value = normalize_word(value)?;
            let raw_value = word.value.strip_prefix(&assignment_prefix)?;
            let rendered_value = if shell_value_requires_quoting(&normalized_value) {
                raw_value.to_string()
            } else {
                normalized_value
            };

            Some(NormalizedAssignment::Included(format!(
                "{assignment_prefix}{rendered_value}"
            )))
        }
        ast::AssignmentValue::Array(_) => Some(NormalizedAssignment::Skipped),
    }
}

fn shell_value_requires_quoting(value: &str) -> bool {
    value.chars().any(|character| {
        character.is_whitespace()
            || !matches!(
                character,
                'a'..='z'
                    | 'A'..='Z'
                    | '0'..='9'
                    | '_'
                    | '@'
                    | '%'
                    | '+'
                    | '='
                    | ':'
                    | ','
                    | '.'
                    | '/'
                    | '-'
            )
    })
}

fn program_validation(program: &ast::Program) -> TerminalProgramValidation {
    combine_validations(
        program
            .complete_commands
            .iter()
            .map(compound_list_validation),
    )
}

fn compound_list_validation(compound_list: &ast::CompoundList) -> TerminalProgramValidation {
    combine_validations(
        compound_list
            .0
            .iter()
            .map(|item| and_or_list_validation(&item.0)),
    )
}

fn and_or_list_validation(and_or_list: &ast::AndOrList) -> TerminalProgramValidation {
    combine_validations(
        std::iter::once(pipeline_validation(&and_or_list.first)).chain(
            and_or_list.additional.iter().map(|and_or| match and_or {
                ast::AndOr::And(pipeline) | ast::AndOr::Or(pipeline) => {
                    pipeline_validation(pipeline)
                }
            }),
        ),
    )
}

fn pipeline_validation(pipeline: &ast::Pipeline) -> TerminalProgramValidation {
    combine_validations(pipeline.seq.iter().map(command_validation))
}

fn command_validation(command: &ast::Command) -> TerminalProgramValidation {
    match command {
        ast::Command::Simple(simple_command) => simple_command_validation(simple_command),
        ast::Command::Compound(compound_command, redirect_list) => combine_validations(
            std::iter::once(compound_command_validation(compound_command))
                .chain(redirect_list.iter().map(redirect_list_validation)),
        ),
        ast::Command::Function(function_definition) => {
            function_body_validation(&function_definition.body)
        }
        ast::Command::ExtendedTest(test_expr) => extended_test_expr_validation(test_expr),
    }
}

fn simple_command_validation(simple_command: &ast::SimpleCommand) -> TerminalProgramValidation {
    combine_validations(
        simple_command
            .prefix
            .iter()
            .map(command_prefix_validation)
            .chain(simple_command.word_or_name.iter().map(word_validation))
            .chain(simple_command.suffix.iter().map(command_suffix_validation)),
    )
}

fn command_prefix_validation(prefix: &ast::CommandPrefix) -> TerminalProgramValidation {
    combine_validations(prefix.0.iter().map(prefix_or_suffix_item_validation))
}

fn command_suffix_validation(suffix: &ast::CommandSuffix) -> TerminalProgramValidation {
    combine_validations(suffix.0.iter().map(prefix_or_suffix_item_validation))
}

fn prefix_or_suffix_item_validation(
    item: &ast::CommandPrefixOrSuffixItem,
) -> TerminalProgramValidation {
    match item {
        ast::CommandPrefixOrSuffixItem::IoRedirect(redirect) => io_redirect_validation(redirect),
        ast::CommandPrefixOrSuffixItem::Word(word) => word_validation(word),
        ast::CommandPrefixOrSuffixItem::AssignmentWord(assignment, word) => {
            combine_validations([assignment_validation(assignment), word_validation(word)])
        }
        ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, _) => {
            TerminalProgramValidation::Unsafe
        }
    }
}

fn io_redirect_validation(redirect: &ast::IoRedirect) -> TerminalProgramValidation {
    match redirect {
        ast::IoRedirect::File(_, _, target) => match target {
            ast::IoFileRedirectTarget::Filename(word) => word_validation(word),
            ast::IoFileRedirectTarget::ProcessSubstitution(_, _) => {
                TerminalProgramValidation::Unsafe
            }
            _ => TerminalProgramValidation::Safe,
        },
        ast::IoRedirect::HereDocument(_, here_doc) => {
            if here_doc.requires_expansion {
                word_validation(&here_doc.doc)
            } else {
                TerminalProgramValidation::Safe
            }
        }
        ast::IoRedirect::HereString(_, word) | ast::IoRedirect::OutputAndError(word, _) => {
            word_validation(word)
        }
    }
}

fn assignment_validation(assignment: &ast::Assignment) -> TerminalProgramValidation {
    match &assignment.value {
        ast::AssignmentValue::Scalar(word) => word_validation(word),
        ast::AssignmentValue::Array(words) => {
            combine_validations(words.iter().flat_map(|(key, value)| {
                key.iter()
                    .map(word_validation)
                    .chain(std::iter::once(word_validation(value)))
            }))
        }
    }
}

fn word_validation(word: &ast::Word) -> TerminalProgramValidation {
    let options = ParserOptions::default();
    let pieces = match brush_parser::word::parse(&word.value, &options) {
        Ok(pieces) => pieces,
        Err(_) => return TerminalProgramValidation::Unsupported,
    };

    combine_validations(
        pieces
            .iter()
            .map(|piece_with_source| word_piece_validation(&piece_with_source.piece)),
    )
}

fn word_piece_validation(piece: &WordPiece) -> TerminalProgramValidation {
    match piece {
        WordPiece::Text(_)
        | WordPiece::SingleQuotedText(_)
        | WordPiece::AnsiCQuotedText(_)
        | WordPiece::EscapeSequence(_)
        | WordPiece::TildePrefix(_) => TerminalProgramValidation::Safe,
        WordPiece::DoubleQuotedSequence(pieces)
        | WordPiece::GettextDoubleQuotedSequence(pieces) => combine_validations(
            pieces
                .iter()
                .map(|inner| word_piece_validation(&inner.piece)),
        ),
        WordPiece::ParameterExpansion(_) | WordPiece::ArithmeticExpression(_) => {
            TerminalProgramValidation::Unsafe
        }
        WordPiece::CommandSubstitution(command)
        | WordPiece::BackquotedCommandSubstitution(command) => {
            let reader = BufReader::new(command.as_bytes());
            let options = ParserOptions::default();
            let source_info = SourceInfo::default();
            let mut parser = Parser::new(reader, &options, &source_info);

            match parser.parse_program() {
                Ok(_) => TerminalProgramValidation::Unsafe,
                Err(_) => TerminalProgramValidation::Unsupported,
            }
        }
    }
}

fn compound_command_validation(
    compound_command: &ast::CompoundCommand,
) -> TerminalProgramValidation {
    match compound_command {
        ast::CompoundCommand::BraceGroup(brace_group) => {
            compound_list_validation(&brace_group.list)
        }
        ast::CompoundCommand::Subshell(subshell) => compound_list_validation(&subshell.list),
        ast::CompoundCommand::ForClause(for_clause) => combine_validations(
            for_clause
                .values
                .iter()
                .flat_map(|values| values.iter().map(word_validation))
                .chain(std::iter::once(do_group_validation(&for_clause.body))),
        ),
        ast::CompoundCommand::CaseClause(case_clause) => combine_validations(
            std::iter::once(word_validation(&case_clause.value))
                .chain(
                    case_clause
                        .cases
                        .iter()
                        .flat_map(|item| item.cmd.iter().map(compound_list_validation)),
                )
                .chain(
                    case_clause
                        .cases
                        .iter()
                        .flat_map(|item| item.patterns.iter().map(word_validation)),
                ),
        ),
        ast::CompoundCommand::IfClause(if_clause) => combine_validations(
            std::iter::once(compound_list_validation(&if_clause.condition))
                .chain(std::iter::once(compound_list_validation(&if_clause.then)))
                .chain(if_clause.elses.iter().flat_map(|elses| {
                    elses.iter().flat_map(|else_item| {
                        else_item
                            .condition
                            .iter()
                            .map(compound_list_validation)
                            .chain(std::iter::once(compound_list_validation(&else_item.body)))
                    })
                })),
        ),
        ast::CompoundCommand::WhileClause(while_clause)
        | ast::CompoundCommand::UntilClause(while_clause) => combine_validations([
            compound_list_validation(&while_clause.0),
            do_group_validation(&while_clause.1),
        ]),
        ast::CompoundCommand::ArithmeticForClause(_) => TerminalProgramValidation::Unsafe,
        ast::CompoundCommand::Arithmetic(_) => TerminalProgramValidation::Unsafe,
    }
}

fn do_group_validation(do_group: &ast::DoGroupCommand) -> TerminalProgramValidation {
    compound_list_validation(&do_group.list)
}

fn function_body_validation(function_body: &ast::FunctionBody) -> TerminalProgramValidation {
    combine_validations(
        std::iter::once(compound_command_validation(&function_body.0))
            .chain(function_body.1.iter().map(redirect_list_validation)),
    )
}

fn redirect_list_validation(redirect_list: &ast::RedirectList) -> TerminalProgramValidation {
    combine_validations(redirect_list.0.iter().map(io_redirect_validation))
}

fn extended_test_expr_validation(
    test_expr: &ast::ExtendedTestExprCommand,
) -> TerminalProgramValidation {
    extended_test_expr_inner_validation(&test_expr.expr)
}

fn extended_test_expr_inner_validation(expr: &ast::ExtendedTestExpr) -> TerminalProgramValidation {
    match expr {
        ast::ExtendedTestExpr::Not(inner) | ast::ExtendedTestExpr::Parenthesized(inner) => {
            extended_test_expr_inner_validation(inner)
        }
        ast::ExtendedTestExpr::And(left, right) | ast::ExtendedTestExpr::Or(left, right) => {
            combine_validations([
                extended_test_expr_inner_validation(left),
                extended_test_expr_inner_validation(right),
            ])
        }
        ast::ExtendedTestExpr::UnaryTest(_, word) => word_validation(word),
        ast::ExtendedTestExpr::BinaryTest(_, left, right) => {
            combine_validations([word_validation(left), word_validation(right)])
        }
    }
}

fn combine_validations(
    validations: impl IntoIterator<Item = TerminalProgramValidation>,
) -> TerminalProgramValidation {
    let mut saw_unsafe = false;
    let mut saw_unsupported = false;

    for validation in validations {
        match validation {
            TerminalProgramValidation::Unsupported => saw_unsupported = true,
            TerminalProgramValidation::Unsafe => saw_unsafe = true,
            TerminalProgramValidation::Safe => {}
        }
    }

    if saw_unsafe {
        TerminalProgramValidation::Unsafe
    } else if saw_unsupported {
        TerminalProgramValidation::Unsupported
    } else {
        TerminalProgramValidation::Safe
    }
}

fn extract_commands_from_program(program: &ast::Program, commands: &mut Vec<String>) -> Option<()> {
    for complete_command in &program.complete_commands {
        extract_commands_from_compound_list(complete_command, commands)?;
    }
    Some(())
}

fn extract_commands_from_compound_list(
    compound_list: &ast::CompoundList,
    commands: &mut Vec<String>,
) -> Option<()> {
    for item in &compound_list.0 {
        extract_commands_from_and_or_list(&item.0, commands)?;
    }
    Some(())
}

fn extract_commands_from_and_or_list(
    and_or_list: &ast::AndOrList,
    commands: &mut Vec<String>,
) -> Option<()> {
    extract_commands_from_pipeline(&and_or_list.first, commands)?;

    for and_or in &and_or_list.additional {
        match and_or {
            ast::AndOr::And(pipeline) | ast::AndOr::Or(pipeline) => {
                extract_commands_from_pipeline(pipeline, commands)?;
            }
        }
    }
    Some(())
}

fn extract_commands_from_pipeline(
    pipeline: &ast::Pipeline,
    commands: &mut Vec<String>,
) -> Option<()> {
    for command in &pipeline.seq {
        extract_commands_from_command(command, commands)?;
    }
    Some(())
}

fn extract_commands_from_command(command: &ast::Command, commands: &mut Vec<String>) -> Option<()> {
    match command {
        ast::Command::Simple(simple_command) => {
            extract_commands_from_simple_command(simple_command, commands)?;
        }
        ast::Command::Compound(compound_command, redirect_list) => {
            let body_start = extract_commands_from_compound_command(compound_command, commands)?;
            if let Some(redirect_list) = redirect_list {
                let mut normalized_redirects = Vec::new();
                for redirect in &redirect_list.0 {
                    match normalize_io_redirect(redirect)? {
                        RedirectNormalization::Normalized(s) => normalized_redirects.push(s),
                        RedirectNormalization::Skip => {}
                    }
                }
                if !normalized_redirects.is_empty() {
                    if body_start >= commands.len() {
                        return None;
                    }
                    commands.extend(normalized_redirects);
                }
                for redirect in &redirect_list.0 {
                    extract_commands_from_io_redirect(redirect, commands)?;
                }
            }
        }
        ast::Command::Function(func_def) => {
            extract_commands_from_function_body(&func_def.body, commands)?;
        }
        ast::Command::ExtendedTest(test_expr) => {
            extract_commands_from_extended_test_expr(test_expr, commands)?;
        }
    }
    Some(())
}

enum RedirectNormalization {
    Normalized(String),
    Skip,
}

fn extract_commands_from_simple_command(
    simple_command: &ast::SimpleCommand,
    commands: &mut Vec<String>,
) -> Option<()> {
    // Build a normalized command string from individual words, stripping shell
    // quotes so that security patterns match regardless of quoting style.
    // For example, both `rm -rf '/'` and `rm -rf /` normalize to "rm -rf /".
    //
    // If any word fails to normalize, we return None so that `extract_commands`
    // returns None — the same as a shell parse failure. The caller then falls
    // back to raw-input matching with always_allow disabled.
    let mut words = Vec::new();
    let mut redirects = Vec::new();

    if let Some(prefix) = &simple_command.prefix {
        for item in &prefix.0 {
            match item {
                ast::CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                    match normalize_io_redirect(redirect) {
                        Some(RedirectNormalization::Normalized(s)) => redirects.push(s),
                        Some(RedirectNormalization::Skip) => {}
                        None => return None,
                    }
                }
                ast::CommandPrefixOrSuffixItem::AssignmentWord(assignment, word) => {
                    match normalize_assignment_for_command_prefix(assignment, word)? {
                        NormalizedAssignment::Included(normalized_assignment) => {
                            words.push(normalized_assignment);
                        }
                        NormalizedAssignment::Skipped => {}
                    }
                }
                ast::CommandPrefixOrSuffixItem::Word(word) => {
                    words.push(normalize_word(word)?);
                }
                ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, _) => return None,
            }
        }
    }
    if let Some(word) = &simple_command.word_or_name {
        words.push(normalize_word(word)?);
    }
    if let Some(suffix) = &simple_command.suffix {
        for item in &suffix.0 {
            match item {
                ast::CommandPrefixOrSuffixItem::Word(word) => {
                    words.push(normalize_word(word)?);
                }
                ast::CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                    match normalize_io_redirect(redirect) {
                        Some(RedirectNormalization::Normalized(s)) => redirects.push(s),
                        Some(RedirectNormalization::Skip) => {}
                        None => return None,
                    }
                }
                ast::CommandPrefixOrSuffixItem::AssignmentWord(assignment, word) => {
                    match normalize_assignment_for_command_prefix(assignment, word)? {
                        NormalizedAssignment::Included(normalized_assignment) => {
                            words.push(normalized_assignment);
                        }
                        NormalizedAssignment::Skipped => {}
                    }
                }
                ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, _) => {}
            }
        }
    }

    if words.is_empty() && !redirects.is_empty() {
        return None;
    }

    let command_str = words.join(" ");
    if !command_str.is_empty() {
        commands.push(command_str);
    }
    commands.extend(redirects);

    // Extract nested commands from command substitutions, process substitutions, etc.
    if let Some(prefix) = &simple_command.prefix {
        extract_commands_from_command_prefix(prefix, commands)?;
    }
    if let Some(word) = &simple_command.word_or_name {
        extract_commands_from_word(word, commands)?;
    }
    if let Some(suffix) = &simple_command.suffix {
        extract_commands_from_command_suffix(suffix, commands)?;
    }
    Some(())
}

/// Normalizes a shell word by stripping quoting syntax and returning the
/// semantic (unquoted) value. Returns `None` if word parsing fails.
fn normalize_word(word: &ast::Word) -> Option<String> {
    let options = ParserOptions::default();
    let pieces = brush_parser::word::parse(&word.value, &options).ok()?;
    let mut result = String::new();
    for piece_with_source in &pieces {
        normalize_word_piece_into(
            &piece_with_source.piece,
            &word.value,
            piece_with_source.start_index,
            piece_with_source.end_index,
            &mut result,
        )?;
    }
    Some(result)
}

fn normalize_word_piece_into(
    piece: &WordPiece,
    raw_value: &str,
    start_index: usize,
    end_index: usize,
    result: &mut String,
) -> Option<()> {
    match piece {
        WordPiece::Text(text) => result.push_str(text),
        WordPiece::SingleQuotedText(text) => result.push_str(text),
        WordPiece::AnsiCQuotedText(text) => result.push_str(text),
        WordPiece::EscapeSequence(text) => {
            result.push_str(text.strip_prefix('\\').unwrap_or(text));
        }
        WordPiece::DoubleQuotedSequence(pieces)
        | WordPiece::GettextDoubleQuotedSequence(pieces) => {
            for inner in pieces {
                normalize_word_piece_into(
                    &inner.piece,
                    raw_value,
                    inner.start_index,
                    inner.end_index,
                    result,
                )?;
            }
        }
        WordPiece::TildePrefix(prefix) => {
            result.push('~');
            result.push_str(prefix);
        }
        // For parameter expansions, command substitutions, and arithmetic expressions,
        // preserve the original source text so that patterns like `\$HOME` continue
        // to match.
        WordPiece::ParameterExpansion(_)
        | WordPiece::CommandSubstitution(_)
        | WordPiece::BackquotedCommandSubstitution(_)
        | WordPiece::ArithmeticExpression(_) => {
            let source = raw_value.get(start_index..end_index)?;
            result.push_str(source);
        }
    }
    Some(())
}

fn is_known_safe_redirect_target(normalized_target: &str) -> bool {
    normalized_target == "/dev/null"
}

fn normalize_io_redirect(redirect: &ast::IoRedirect) -> Option<RedirectNormalization> {
    match redirect {
        ast::IoRedirect::File(fd, kind, target) => {
            let target_word = match target {
                ast::IoFileRedirectTarget::Filename(word) => word,
                _ => return Some(RedirectNormalization::Skip),
            };
            let operator = match kind {
                ast::IoFileRedirectKind::Read => "<",
                ast::IoFileRedirectKind::Write => ">",
                ast::IoFileRedirectKind::Append => ">>",
                ast::IoFileRedirectKind::ReadAndWrite => "<>",
                ast::IoFileRedirectKind::Clobber => ">|",
                // The parser pairs DuplicateInput/DuplicateOutput with
                // IoFileRedirectTarget::Duplicate (not Filename), so the
                // target match above will return Skip before we reach here.
                // These arms are kept for defensiveness.
                ast::IoFileRedirectKind::DuplicateInput => "<&",
                ast::IoFileRedirectKind::DuplicateOutput => ">&",
            };
            let fd_prefix = match fd {
                Some(fd) => fd.to_string(),
                None => String::new(),
            };
            let normalized = normalize_word(target_word)?;
            if is_known_safe_redirect_target(&normalized) {
                return Some(RedirectNormalization::Skip);
            }
            Some(RedirectNormalization::Normalized(format!(
                "{}{} {}",
                fd_prefix, operator, normalized
            )))
        }
        ast::IoRedirect::OutputAndError(word, append) => {
            let operator = if *append { "&>>" } else { "&>" };
            let normalized = normalize_word(word)?;
            if is_known_safe_redirect_target(&normalized) {
                return Some(RedirectNormalization::Skip);
            }
            Some(RedirectNormalization::Normalized(format!(
                "{} {}",
                operator, normalized
            )))
        }
        ast::IoRedirect::HereDocument(_, _) | ast::IoRedirect::HereString(_, _) => {
            Some(RedirectNormalization::Skip)
        }
    }
}

fn extract_commands_from_command_prefix(
    prefix: &ast::CommandPrefix,
    commands: &mut Vec<String>,
) -> Option<()> {
    for item in &prefix.0 {
        extract_commands_from_prefix_or_suffix_item(item, commands)?;
    }
    Some(())
}

fn extract_commands_from_command_suffix(
    suffix: &ast::CommandSuffix,
    commands: &mut Vec<String>,
) -> Option<()> {
    for item in &suffix.0 {
        extract_commands_from_prefix_or_suffix_item(item, commands)?;
    }
    Some(())
}

fn extract_commands_from_prefix_or_suffix_item(
    item: &ast::CommandPrefixOrSuffixItem,
    commands: &mut Vec<String>,
) -> Option<()> {
    match item {
        ast::CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
            extract_commands_from_io_redirect(redirect, commands)?;
        }
        ast::CommandPrefixOrSuffixItem::AssignmentWord(assignment, _word) => {
            extract_commands_from_assignment(assignment, commands)?;
        }
        ast::CommandPrefixOrSuffixItem::Word(word) => {
            extract_commands_from_word(word, commands)?;
        }
        ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_kind, subshell) => {
            extract_commands_from_compound_list(&subshell.list, commands)?;
        }
    }
    Some(())
}

fn extract_commands_from_io_redirect(
    redirect: &ast::IoRedirect,
    commands: &mut Vec<String>,
) -> Option<()> {
    match redirect {
        ast::IoRedirect::File(_fd, _kind, target) => match target {
            ast::IoFileRedirectTarget::ProcessSubstitution(_kind, subshell) => {
                extract_commands_from_compound_list(&subshell.list, commands)?;
            }
            ast::IoFileRedirectTarget::Filename(word) => {
                extract_commands_from_word(word, commands)?;
            }
            _ => {}
        },
        ast::IoRedirect::HereDocument(_fd, here_doc) => {
            if here_doc.requires_expansion {
                extract_commands_from_word(&here_doc.doc, commands)?;
            }
        }
        ast::IoRedirect::HereString(_fd, word) => {
            extract_commands_from_word(word, commands)?;
        }
        ast::IoRedirect::OutputAndError(word, _) => {
            extract_commands_from_word(word, commands)?;
        }
    }
    Some(())
}

fn extract_commands_from_assignment(
    assignment: &ast::Assignment,
    commands: &mut Vec<String>,
) -> Option<()> {
    match &assignment.value {
        ast::AssignmentValue::Scalar(word) => {
            extract_commands_from_word(word, commands)?;
        }
        ast::AssignmentValue::Array(words) => {
            for (opt_word, word) in words {
                if let Some(w) = opt_word {
                    extract_commands_from_word(w, commands)?;
                }
                extract_commands_from_word(word, commands)?;
            }
        }
    }
    Some(())
}

fn extract_commands_from_word(word: &ast::Word, commands: &mut Vec<String>) -> Option<()> {
    let options = ParserOptions::default();
    let pieces = brush_parser::word::parse(&word.value, &options).ok()?;
    for piece_with_source in pieces {
        extract_commands_from_word_piece(&piece_with_source.piece, commands)?;
    }
    Some(())
}

fn extract_commands_from_word_piece(piece: &WordPiece, commands: &mut Vec<String>) -> Option<()> {
    match piece {
        WordPiece::CommandSubstitution(cmd_str)
        | WordPiece::BackquotedCommandSubstitution(cmd_str) => {
            let nested_commands = extract_commands(cmd_str)?;
            commands.extend(nested_commands);
        }
        WordPiece::DoubleQuotedSequence(pieces)
        | WordPiece::GettextDoubleQuotedSequence(pieces) => {
            for inner_piece_with_source in pieces {
                extract_commands_from_word_piece(&inner_piece_with_source.piece, commands)?;
            }
        }
        WordPiece::ArithmeticExpression(expr) => {
            // The arithmetic body may contain `$(...)` or `${...}` that bash will
            // evaluate before doing arithmetic. Re-parse to extract those.
            // We propagate parse failures with `?` so that callers fail closed
            // (treating the whole input as a parse failure) rather than silently
            // dropping commands hidden inside content brush couldn't tokenize.
            extract_commands_from_word_string(&expr.value, commands)?;
        }
        WordPiece::ParameterExpansion(expr) => {
            extract_commands_from_parameter_expr(expr, commands)?;
        }
        WordPiece::EscapeSequence(_)
        | WordPiece::SingleQuotedText(_)
        | WordPiece::Text(_)
        | WordPiece::AnsiCQuotedText(_)
        | WordPiece::TildePrefix(_) => {}
    }
    Some(())
}

/// Re-parses a string as a bash word and recurses into its pieces to extract
/// any nested command substitutions. Returns `None` (failing closed) if brush
/// cannot tokenize the input, so callers treat allowlist decisions about this
/// input as untrusted.
fn extract_commands_from_word_string(s: &str, commands: &mut Vec<String>) -> Option<()> {
    let options = ParserOptions::default();
    let pieces = brush_parser::word::parse(s, &options).ok()?;
    for inner_piece in pieces {
        extract_commands_from_word_piece(&inner_piece.piece, commands)?;
    }
    Some(())
}

/// Recurses into the string-typed fields of a parameter expansion that bash
/// will subject to command substitution at expansion time, mirroring the
/// arithmetic expansion handling. Failing to extend this when adding new
/// `ParameterExpr` variants risks an allowlist bypass via e.g.
/// `${V:-$(curl evil)}`, `${V/pat/$(curl evil)}`, `${V:$(($(curl))):1}`.
fn extract_commands_from_parameter_expr(
    expr: &brush_parser::word::ParameterExpr,
    commands: &mut Vec<String>,
) -> Option<()> {
    use brush_parser::word::ParameterExpr;
    match expr {
        ParameterExpr::Parameter { .. }
        | ParameterExpr::ParameterLength { .. }
        | ParameterExpr::Transform { .. }
        | ParameterExpr::VariableNames { .. }
        | ParameterExpr::MemberKeys { .. } => {}
        ParameterExpr::UseDefaultValues { default_value, .. }
        | ParameterExpr::AssignDefaultValues { default_value, .. } => {
            if let Some(value) = default_value {
                extract_commands_from_word_string(value, commands)?;
            }
        }
        ParameterExpr::IndicateErrorIfNullOrUnset { error_message, .. } => {
            if let Some(value) = error_message {
                extract_commands_from_word_string(value, commands)?;
            }
        }
        ParameterExpr::UseAlternativeValue {
            alternative_value, ..
        } => {
            if let Some(value) = alternative_value {
                extract_commands_from_word_string(value, commands)?;
            }
        }
        ParameterExpr::RemoveSmallestSuffixPattern { pattern, .. }
        | ParameterExpr::RemoveLargestSuffixPattern { pattern, .. }
        | ParameterExpr::RemoveSmallestPrefixPattern { pattern, .. }
        | ParameterExpr::RemoveLargestPrefixPattern { pattern, .. }
        | ParameterExpr::UppercaseFirstChar { pattern, .. }
        | ParameterExpr::UppercasePattern { pattern, .. }
        | ParameterExpr::LowercaseFirstChar { pattern, .. }
        | ParameterExpr::LowercasePattern { pattern, .. } => {
            if let Some(pattern) = pattern {
                extract_commands_from_word_string(pattern, commands)?;
            }
        }
        ParameterExpr::Substring { offset, length, .. } => {
            extract_commands_from_word_string(&offset.value, commands)?;
            if let Some(length) = length {
                extract_commands_from_word_string(&length.value, commands)?;
            }
        }
        ParameterExpr::ReplaceSubstring {
            pattern,
            replacement,
            ..
        } => {
            extract_commands_from_word_string(pattern, commands)?;
            if let Some(replacement) = replacement {
                extract_commands_from_word_string(replacement, commands)?;
            }
        }
    }
    Some(())
}

fn extract_commands_from_compound_command(
    compound_command: &ast::CompoundCommand,
    commands: &mut Vec<String>,
) -> Option<usize> {
    match compound_command {
        ast::CompoundCommand::BraceGroup(brace_group) => {
            let body_start = commands.len();
            extract_commands_from_compound_list(&brace_group.list, commands)?;
            Some(body_start)
        }
        ast::CompoundCommand::Subshell(subshell) => {
            let body_start = commands.len();
            extract_commands_from_compound_list(&subshell.list, commands)?;
            Some(body_start)
        }
        ast::CompoundCommand::ForClause(for_clause) => {
            if let Some(words) = &for_clause.values {
                for word in words {
                    extract_commands_from_word(word, commands)?;
                }
            }
            let body_start = commands.len();
            extract_commands_from_do_group(&for_clause.body, commands)?;
            Some(body_start)
        }
        ast::CompoundCommand::CaseClause(case_clause) => {
            extract_commands_from_word(&case_clause.value, commands)?;
            let body_start = commands.len();
            for item in &case_clause.cases {
                if let Some(body) = &item.cmd {
                    extract_commands_from_compound_list(body, commands)?;
                }
            }
            Some(body_start)
        }
        ast::CompoundCommand::IfClause(if_clause) => {
            extract_commands_from_compound_list(&if_clause.condition, commands)?;
            let body_start = commands.len();
            extract_commands_from_compound_list(&if_clause.then, commands)?;
            if let Some(elses) = &if_clause.elses {
                for else_item in elses {
                    if let Some(condition) = &else_item.condition {
                        extract_commands_from_compound_list(condition, commands)?;
                    }
                    extract_commands_from_compound_list(&else_item.body, commands)?;
                }
            }
            Some(body_start)
        }
        ast::CompoundCommand::WhileClause(while_clause)
        | ast::CompoundCommand::UntilClause(while_clause) => {
            extract_commands_from_compound_list(&while_clause.0, commands)?;
            let body_start = commands.len();
            extract_commands_from_do_group(&while_clause.1, commands)?;
            Some(body_start)
        }
        ast::CompoundCommand::ArithmeticForClause(arith_for) => {
            let body_start = commands.len();
            extract_commands_from_do_group(&arith_for.body, commands)?;
            Some(body_start)
        }
        ast::CompoundCommand::Arithmetic(_arith_cmd) => Some(commands.len()),
    }
}

fn extract_commands_from_do_group(
    do_group: &ast::DoGroupCommand,
    commands: &mut Vec<String>,
) -> Option<()> {
    extract_commands_from_compound_list(&do_group.list, commands)
}

fn extract_commands_from_function_body(
    func_body: &ast::FunctionBody,
    commands: &mut Vec<String>,
) -> Option<()> {
    let body_start = extract_commands_from_compound_command(&func_body.0, commands)?;
    if let Some(redirect_list) = &func_body.1 {
        let mut normalized_redirects = Vec::new();
        for redirect in &redirect_list.0 {
            match normalize_io_redirect(redirect)? {
                RedirectNormalization::Normalized(s) => normalized_redirects.push(s),
                RedirectNormalization::Skip => {}
            }
        }
        if !normalized_redirects.is_empty() {
            if body_start >= commands.len() {
                return None;
            }
            commands.extend(normalized_redirects);
        }
        for redirect in &redirect_list.0 {
            extract_commands_from_io_redirect(redirect, commands)?;
        }
    }
    Some(())
}

fn extract_commands_from_extended_test_expr(
    test_expr: &ast::ExtendedTestExprCommand,
    commands: &mut Vec<String>,
) -> Option<()> {
    extract_commands_from_extended_test_expr_inner(&test_expr.expr, commands)
}

fn extract_commands_from_extended_test_expr_inner(
    expr: &ast::ExtendedTestExpr,
    commands: &mut Vec<String>,
) -> Option<()> {
    match expr {
        ast::ExtendedTestExpr::Not(inner) => {
            extract_commands_from_extended_test_expr_inner(inner, commands)?;
        }
        ast::ExtendedTestExpr::And(left, right) | ast::ExtendedTestExpr::Or(left, right) => {
            extract_commands_from_extended_test_expr_inner(left, commands)?;
            extract_commands_from_extended_test_expr_inner(right, commands)?;
        }
        ast::ExtendedTestExpr::Parenthesized(inner) => {
            extract_commands_from_extended_test_expr_inner(inner, commands)?;
        }
        ast::ExtendedTestExpr::UnaryTest(_, word) => {
            extract_commands_from_word(word, commands)?;
        }
        ast::ExtendedTestExpr::BinaryTest(_, word1, word2) => {
            extract_commands_from_word(word1, commands)?;
            extract_commands_from_word(word2, commands)?;
        }
    }
    Some(())
}

