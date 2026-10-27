//! ## When to create a migration and why?
//! A migration is necessary when keymap actions or settings are renamed or transformed (e.g., from an array to a string, a string to an array, a boolean to an enum, etc.).
//!
//! This ensures that users with outdated settings are automatically updated to use the corresponding new settings internally.
//! It also provides a quick way to migrate their existing settings to the latest state using button in UI.
//!
//! ## How to create a migration?
//! Migrations use Tree-sitter to query commonly used patterns, such as actions with a string or actions with an array where the second argument is an object, etc.
//! Once queried, *you can filter out the modified items* and write the replacement logic.
//!
//! You *must not* modify previous migrations; always create new ones instead.
//! This is important because if a user is in an intermediate state, they can smoothly transition to the latest state.
//! Modifying existing migrations means they will only work for users upgrading from version x-1 to x, but not from x-2 to x, and so on, where x is the latest version.
//!
//! You only need to write replacement logic for x-1 to x because you can be certain that, internally, every user will be at x-1, regardless of their on disk state.

use anyhow::{Context as _, Result};
use settings_json::{infer_json_indent_size, parse_json_with_comments, update_value_in_json_text};
use std::{cmp::Reverse, ops::Range, sync::LazyLock};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Query, QueryMatch};

fn migrate(text: &str, patterns: MigrationPatterns, query: &Query) -> Result<Option<String>> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_json::LANGUAGE.into())?;
    let syntax_tree = parser
        .parse(text, None)
        .context("failed to parse settings")?;

    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(query, syntax_tree.root_node(), text.as_bytes());

    let mut edits = vec![];
    while let Some(mat) = matches.next() {
        if let Some((_, callback)) = patterns.get(mat.pattern_index) {
            edits.extend(callback(text, mat, query));
        }
    }

    edits.sort_unstable_by_key(|(range, _)| (range.start, Reverse(range.end)));
    edits.dedup_by(|(range_b, _), (range_a, _)| {
        range_a.contains(&range_b.start) || range_a.contains(&range_b.end)
    });

    if edits.is_empty() {
        Ok(None)
    } else {
        let mut new_text = text.to_string();
        for (range, replacement) in edits.iter().rev() {
            new_text.replace_range(range.clone(), replacement);
        }
        if new_text == text {
            log::error!(
                "Edits computed for configuration migration do not cause a change: {:?}",
                edits
            );
            Ok(None)
        } else {
            Ok(Some(new_text))
        }
    }
}

/// Runs the provided migrations on the given text.
/// Will automatically return `Ok(None)` if there's no content to migrate.
fn run_migrations(text: &str, migrations: &[MigrationType]) -> Result<Option<String>> {
    if text.is_empty() {
        return Ok(None);
    }

    let mut current_text = text.to_string();
    let mut result: Option<String> = None;
    let json_indent_size = infer_json_indent_size(&current_text);
    for migration in migrations.iter() {
        let migrated_text = match migration {
            MigrationType::TreeSitter(patterns, query) => migrate(&current_text, patterns, query)?,
            MigrationType::Json(callback) => {
                if current_text.trim().is_empty() {
                    return Ok(None);
                }
                let old_content: serde_json_lenient::Value =
                    parse_json_with_comments(&current_text)?;
                let old_value = serde_json::to_value(&old_content).unwrap();
                let mut new_value = old_value.clone();
                callback(&mut new_value)?;
                if new_value != old_value {
                    let mut current = current_text.clone();
                    let mut edits = vec![];
                    update_value_in_json_text(
                        &mut current,
                        &mut vec![],
                        json_indent_size,
                        &old_value,
                        &new_value,
                        &mut edits,
                    );
                    let mut migrated_text = current_text.clone();
                    for (range, replacement) in edits.into_iter() {
                        migrated_text.replace_range(range, &replacement);
                    }
                    Some(migrated_text)
                } else {
                    None
                }
            }
        };
        if let Some(migrated_text) = migrated_text {
            current_text = migrated_text.clone();
            result = Some(migrated_text);
        }
    }
    Ok(result.filter(|new_text| text != new_text))
}

pub fn migrate_keymap(text: &str) -> Result<Option<String>> {
    let migrations: &[MigrationType] = &[
        MigrationType::TreeSitter(
            migrations::m_2025_01_29::KEYMAP_PATTERNS,
            &KEYMAP_QUERY_2025_01_29,
        ),
        MigrationType::TreeSitter(
            migrations::m_2025_01_30::KEYMAP_PATTERNS,
            &KEYMAP_QUERY_2025_01_30,
        ),
        MigrationType::TreeSitter(
            migrations::m_2025_03_03::KEYMAP_PATTERNS,
            &KEYMAP_QUERY_2025_03_03,
        ),
        MigrationType::TreeSitter(
            migrations::m_2025_03_06::KEYMAP_PATTERNS,
            &KEYMAP_QUERY_2025_03_06,
        ),
        MigrationType::TreeSitter(
            migrations::m_2025_04_15::KEYMAP_PATTERNS,
            &KEYMAP_QUERY_2025_04_15,
        ),
    ];
    run_migrations(text, migrations)
}

enum MigrationType<'a> {
    TreeSitter(MigrationPatterns, &'a Query),
    Json(fn(&mut serde_json::Value) -> Result<()>),
}

pub fn migrate_settings(text: &str) -> Result<Option<String>> {
    let migrations: &[MigrationType] = &[
        MigrationType::TreeSitter(
            migrations::m_2025_01_02::SETTINGS_PATTERNS,
            &SETTINGS_QUERY_2025_01_02,
        ),
        MigrationType::TreeSitter(
            migrations::m_2025_01_30::SETTINGS_PATTERNS,
            &SETTINGS_QUERY_2025_01_30,
        ),
        MigrationType::TreeSitter(
            migrations::m_2025_03_29::SETTINGS_PATTERNS,
            &SETTINGS_QUERY_2025_03_29,
        ),
        MigrationType::TreeSitter(
            migrations::m_2025_07_08::SETTINGS_PATTERNS,
            &SETTINGS_QUERY_2025_07_08,
        ),
        MigrationType::Json(migrations::m_2025_10_01::flatten_code_actions_formatters),
        MigrationType::Json(migrations::m_2025_10_02::remove_formatters_on_save),
        MigrationType::Json(migrations::m_2025_10_16::restore_code_actions_on_format),
        MigrationType::Json(migrations::m_2025_10_17::make_file_finder_include_ignored_an_enum),
        MigrationType::Json(migrations::m_2025_10_21::make_relative_line_numbers_an_enum),
        MigrationType::TreeSitter(
            migrations::m_2025_11_12::SETTINGS_PATTERNS,
            &SETTINGS_QUERY_2025_11_12,
        ),
        MigrationType::TreeSitter(
            migrations::m_2025_12_01::SETTINGS_PATTERNS,
            &SETTINGS_QUERY_2025_12_01,
        ),
        MigrationType::TreeSitter(
            migrations::m_2025_12_15::SETTINGS_PATTERNS,
            &SETTINGS_QUERY_2025_12_15,
        ),
        MigrationType::Json(migrations::m_2025_01_27::make_auto_indent_an_enum),
        MigrationType::TreeSitter(
            migrations::m_2026_03_16::SETTINGS_PATTERNS,
            &SETTINGS_QUERY_2026_03_16,
        ),
        MigrationType::Json(migrations::m_2026_04_01::restructure_profiles_with_settings_key),
        MigrationType::Json(
            migrations::m_2026_04_17::promote_show_branch_icon_true_to_show_branch_status_icon,
        ),
        MigrationType::TreeSitter(
            migrations::m_2026_05_04::SETTINGS_PATTERNS,
            &SETTINGS_QUERY_2026_05_04,
        ),
        MigrationType::Json(migrations::m_2026_08_17::make_git_gutter_width_an_enum),
        MigrationType::Json(migrations::m_2026_08_26::rename_folder_icons_to_folder_indicator),
        MigrationType::Json(migrations::m_2026_08_30::nest_markdown_preview_settings),
    ];
    run_migrations(text, migrations)
}

pub type MigrationPatterns = &'static [(
    &'static str,
    fn(&str, &QueryMatch, &Query) -> Option<(Range<usize>, String)>,
)];

macro_rules! define_query {
    ($var_name:ident, $patterns_path:path) => {
        static $var_name: LazyLock<Query> = LazyLock::new(|| {
            Query::new(
                &tree_sitter_json::LANGUAGE.into(),
                &$patterns_path
                    .iter()
                    .map(|pattern| pattern.0)
                    .collect::<String>(),
            )
            .unwrap()
        });
    };
}

// keymap
define_query!(
    KEYMAP_QUERY_2025_01_29,
    migrations::m_2025_01_29::KEYMAP_PATTERNS
);
define_query!(
    KEYMAP_QUERY_2025_01_30,
    migrations::m_2025_01_30::KEYMAP_PATTERNS
);
define_query!(
    KEYMAP_QUERY_2025_03_03,
    migrations::m_2025_03_03::KEYMAP_PATTERNS
);
define_query!(
    KEYMAP_QUERY_2025_03_06,
    migrations::m_2025_03_06::KEYMAP_PATTERNS
);
define_query!(
    KEYMAP_QUERY_2025_04_15,
    migrations::m_2025_04_15::KEYMAP_PATTERNS
);

// settings
define_query!(
    SETTINGS_QUERY_2025_01_02,
    migrations::m_2025_01_02::SETTINGS_PATTERNS
);
define_query!(
    SETTINGS_QUERY_2025_01_30,
    migrations::m_2025_01_30::SETTINGS_PATTERNS
);
define_query!(
    SETTINGS_QUERY_2025_03_29,
    migrations::m_2025_03_29::SETTINGS_PATTERNS
);
define_query!(
    SETTINGS_QUERY_2025_07_08,
    migrations::m_2025_07_08::SETTINGS_PATTERNS
);
define_query!(
    SETTINGS_QUERY_2025_11_12,
    migrations::m_2025_11_12::SETTINGS_PATTERNS
);
define_query!(
    SETTINGS_QUERY_2025_12_01,
    migrations::m_2025_12_01::SETTINGS_PATTERNS
);
define_query!(
    SETTINGS_QUERY_2025_12_15,
    migrations::m_2025_12_15::SETTINGS_PATTERNS
);
define_query!(
    SETTINGS_QUERY_2026_03_16,
    migrations::m_2026_03_16::SETTINGS_PATTERNS
);
define_query!(
    SETTINGS_QUERY_2026_05_04,
    migrations::m_2026_05_04::SETTINGS_PATTERNS
);

mod patterns {
    mod keymap {
        pub const KEYMAP_ACTION_ARRAY_PATTERN: &str = r#"(document
            (array
                (object
                    (pair
                        key: (string (string_content) @name)
                        value: (
                            (object
                                (pair
                                    key: (string)
                                    value: ((array
                                        . (string (string_content) @action_name)
                                        . (string (string_content) @argument)
                                        .)) @array
                                )
                            )
                        )
                    )
                )
            )
            (#eq? @name "bindings")
        )"#;

        pub const KEYMAP_ACTION_STRING_PATTERN: &str = r#"(document
            (array
                (object
                    (pair
                        key: (string (string_content) @name)
                        value: (
                            (object
                                (pair
                                    key: (string)
                                    value: (string (string_content) @action_name)
                                )
                            )
                        )
                    )
                )
            )
            (#eq? @name "bindings")
        )"#;

        pub const KEYMAP_ACTION_ARRAY_ARGUMENT_AS_OBJECT_PATTERN: &str = r#"(document
            (array
                (object
                    (pair
                        key: (string (string_content) @name)
                        value: (
                            (object
                                (pair
                                    key: (string)
                                    value: ((array
                                        . (string (string_content) @action_name)
                                        . (object
                                            (pair
                                            key: (string (string_content) @argument_key)
                                            value: (_)  @argument_value))
                                        . ) @array
                                    ))
                                )
                            )
                        )
                    )
                )
            (#eq? @name "bindings")
        )"#;
    }

    mod settings {
        pub const SETTINGS_ROOT_KEY_VALUE_PATTERN: &str = r#"(document
            (object
                (pair
                    key: (string (string_content) @name)
                    value: (_)  @value
                )
            )
        )"#;

        pub const SETTINGS_NESTED_KEY_VALUE_PATTERN: &str = r#"(document
            (object
                (pair
                    key: (string (string_content) @parent_key)
                    value: (object
                        (pair
                            key: (string (string_content) @setting_name)
                            value: (_) @setting_value
                        )
                    )
                )
            )
        )"#;
    }

    pub(crate) use keymap::{
        KEYMAP_ACTION_ARRAY_ARGUMENT_AS_OBJECT_PATTERN, KEYMAP_ACTION_ARRAY_PATTERN,
        KEYMAP_ACTION_STRING_PATTERN,
    };

    pub(crate) use settings::{SETTINGS_NESTED_KEY_VALUE_PATTERN, SETTINGS_ROOT_KEY_VALUE_PATTERN};
}

mod migrations {
    use anyhow::Result;
    use serde_json::Value;
    /// Copied from settings_content (`ReleaseChannelOverrides` + `PlatformOverrides`)
    /// to avoid depending on that crate. Keep in sync.
    const OVERRIDE_KEYS: &[&str] = &[
        "dev", "nightly", "preview", "stable", // release channels
        "macos", "linux", "windows", // platforms
    ];

    /// Applies a migration callback to the root settings object as well as all
    /// nested platform, release-channel, and profile override objects.
    pub(crate) fn migrate_settings(
        value: &mut Value,
        migrate_one: &mut dyn FnMut(&mut serde_json::Map<String, Value>) -> Result<()>,
    ) -> Result<()> {
        let Some(root_object) = value.as_object_mut() else {
            return Ok(());
        };

        migrate_one(root_object)?;

        for &key in OVERRIDE_KEYS {
            if let Some(sub_object) = root_object.get_mut(key) {
                if let Some(sub_map) = sub_object.as_object_mut() {
                    migrate_one(sub_map)?;
                }
            }
        }

        if let Some(profiles) = root_object.get_mut("profiles") {
            if let Some(profiles_object) = profiles.as_object_mut() {
                for profile_value in profiles_object.values_mut() {
                    if let Some(profile_map) = profile_value.as_object_mut() {
                        if let Some(inner) = profile_map
                            .get_mut("settings")
                            .and_then(|v| v.as_object_mut())
                        {
                            migrate_one(inner)?;
                        } else {
                            migrate_one(profile_map)?;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Applies a migration callback to a value and its `languages` children,
    /// at the root level as well as all nested platform, release-channel, and
    /// profile override objects.
    pub(crate) fn migrate_language_setting(
        value: &mut Value,
        migrate_fn: fn(&mut Value, path: &[&str]) -> Result<()>,
    ) -> Result<()> {
        fn apply_to_value_and_languages(
            value: &mut Value,
            prefix: &[&str],
            migrate_fn: fn(&mut Value, path: &[&str]) -> Result<()>,
        ) -> Result<()> {
            migrate_fn(value, prefix)?;
            let languages = value
                .as_object_mut()
                .and_then(|obj| obj.get_mut("languages"))
                .and_then(|languages| languages.as_object_mut());
            if let Some(languages) = languages {
                for (language_name, language) in languages.iter_mut() {
                    let mut path: Vec<&str> = prefix.to_vec();
                    path.push("languages");
                    path.push(language_name);
                    migrate_fn(language, &path)?;
                }
            }
            Ok(())
        }

        if !value.is_object() {
            return Ok(());
        }

        apply_to_value_and_languages(value, &[], migrate_fn)?;

        let Some(root_object) = value.as_object_mut() else {
            return Ok(());
        };

        for &key in OVERRIDE_KEYS {
            if let Some(sub_value) = root_object.get_mut(key) {
                apply_to_value_and_languages(sub_value, &[key], migrate_fn)?;
            }
        }

        if let Some(profiles) = root_object.get_mut("profiles") {
            if let Some(profiles_object) = profiles.as_object_mut() {
                let profile_names: Vec<String> = profiles_object.keys().cloned().collect();
                for profile_name in &profile_names {
                    if let Some(profile_value) = profiles_object.get_mut(profile_name.as_str()) {
                        if let Some(settings_value) = profile_value
                            .as_object_mut()
                            .and_then(|m| m.get_mut("settings"))
                        {
                            apply_to_value_and_languages(
                                settings_value,
                                &["profiles", profile_name],
                                migrate_fn,
                            )?;
                        } else {
                            apply_to_value_and_languages(
                                profile_value,
                                &["profiles", profile_name],
                                migrate_fn,
                            )?;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    pub(crate) mod m_2025_01_02 {
        mod settings {
            use collections::HashMap;
            use std::{ops::Range, sync::LazyLock};
            use tree_sitter::{Query, QueryMatch};

            use crate::MigrationPatterns;
            use crate::patterns::SETTINGS_NESTED_KEY_VALUE_PATTERN;

            pub const SETTINGS_PATTERNS: MigrationPatterns = &[(
                SETTINGS_NESTED_KEY_VALUE_PATTERN,
                replace_deprecated_settings_values,
            )];

            fn replace_deprecated_settings_values(
                contents: &str,
                mat: &QueryMatch,
                query: &Query,
            ) -> Option<(Range<usize>, String)> {
                let parent_object_capture_ix = query.capture_index_for_name("parent_key")?;
                let parent_object_range = mat
                    .nodes_for_capture_index(parent_object_capture_ix)
                    .next()?
                    .byte_range();
                let parent_object_name = contents.get(parent_object_range)?;

                let setting_name_ix = query.capture_index_for_name("setting_name")?;
                let setting_name_range = mat
                    .nodes_for_capture_index(setting_name_ix)
                    .next()?
                    .byte_range();
                let setting_name = contents.get(setting_name_range)?;

                let setting_value_ix = query.capture_index_for_name("setting_value")?;
                let setting_value_range = mat
                    .nodes_for_capture_index(setting_value_ix)
                    .next()?
                    .byte_range();
                let setting_value = contents.get(setting_value_range.clone())?;

                UPDATED_SETTINGS
                    .get(&(parent_object_name, setting_name))
                    .and_then(|new_values| {
                        new_values
                            .iter()
                            .find_map(|(old_value, new_value)| {
                                (*old_value == setting_value).then(|| new_value.to_string())
                            })
                            .map(|new_value| (setting_value_range, new_value))
                    })
            }

            static UPDATED_SETTINGS: LazyLock<HashMap<(&str, &str), Vec<(&str, &str)>>> =
                LazyLock::new(|| {
                    HashMap::from_iter([
                        (
                            ("scrollbar", "diagnostics"),
                            vec![("true", "\"all\""), ("false", "\"none\"")],
                        ),
                    ])
                });
        }

        pub(crate) use settings::SETTINGS_PATTERNS;
    }

    pub(crate) mod m_2025_01_27 {
        mod settings {
            use anyhow::Result;
            use serde_json::Value;

            use crate::migrations::migrate_language_setting;

            pub fn make_auto_indent_an_enum(value: &mut Value) -> Result<()> {
                migrate_language_setting(value, migrate_auto_indent)
            }

            fn migrate_auto_indent(value: &mut Value, _path: &[&str]) -> Result<()> {
                let Some(auto_indent) = value
                    .as_object_mut()
                    .and_then(|obj| obj.get_mut("auto_indent"))
                else {
                    return Ok(());
                };

                *auto_indent = match auto_indent {
                    Value::Bool(true) => Value::String("syntax_aware".to_string()),
                    Value::Bool(false) => Value::String("none".to_string()),
                    Value::String(s)
                        if s == "syntax_aware" || s == "preserve_indent" || s == "none" =>
                    {
                        return Ok(());
                    }
                    _ => anyhow::bail!("Expected auto_indent to be a boolean or valid enum value"),
                };
                Ok(())
            }
        }

        pub(crate) use settings::make_auto_indent_an_enum;
    }

    pub(crate) mod m_2025_01_29 {
        mod keymap {
            use collections::HashMap;
            use std::{ops::Range, sync::LazyLock};
            use tree_sitter::{Query, QueryMatch};

            use crate::MigrationPatterns;
            use crate::patterns::{
                KEYMAP_ACTION_ARRAY_ARGUMENT_AS_OBJECT_PATTERN, KEYMAP_ACTION_ARRAY_PATTERN,
            };

            pub const KEYMAP_PATTERNS: MigrationPatterns = &[
                (
                    KEYMAP_ACTION_ARRAY_PATTERN,
                    replace_array_with_single_string,
                ),
                (
                    KEYMAP_ACTION_ARRAY_ARGUMENT_AS_OBJECT_PATTERN,
                    replace_action_argument_object_with_single_value,
                ),
            ];

            fn replace_array_with_single_string(
                contents: &str,
                mat: &QueryMatch,
                query: &Query,
            ) -> Option<(Range<usize>, String)> {
                let array_ix = query.capture_index_for_name("array")?;
                let action_name_ix = query.capture_index_for_name("action_name")?;
                let argument_ix = query.capture_index_for_name("argument")?;

                let action_name = contents.get(
                    mat.nodes_for_capture_index(action_name_ix)
                        .next()?
                        .byte_range(),
                )?;
                let argument = contents.get(
                    mat.nodes_for_capture_index(argument_ix)
                        .next()?
                        .byte_range(),
                )?;

                let replacement = TRANSFORM_ARRAY.get(&(action_name, argument))?;
                let replacement_as_string = format!("\"{replacement}\"");
                let range_to_replace = mat.nodes_for_capture_index(array_ix).next()?.byte_range();

                Some((range_to_replace, replacement_as_string))
            }

            static TRANSFORM_ARRAY: LazyLock<HashMap<(&str, &str), &str>> = LazyLock::new(|| {
                HashMap::from_iter([
                    // activate
                    (
                        ("workspace::ActivatePaneInDirection", "Up"),
                        "workspace::ActivatePaneUp",
                    ),
                    (
                        ("workspace::ActivatePaneInDirection", "Down"),
                        "workspace::ActivatePaneDown",
                    ),
                    (
                        ("workspace::ActivatePaneInDirection", "Left"),
                        "workspace::ActivatePaneLeft",
                    ),
                    (
                        ("workspace::ActivatePaneInDirection", "Right"),
                        "workspace::ActivatePaneRight",
                    ),
                    // swap
                    (
                        ("workspace::SwapPaneInDirection", "Up"),
                        "workspace::SwapPaneUp",
                    ),
                    (
                        ("workspace::SwapPaneInDirection", "Down"),
                        "workspace::SwapPaneDown",
                    ),
                    (
                        ("workspace::SwapPaneInDirection", "Left"),
                        "workspace::SwapPaneLeft",
                    ),
                    (
                        ("workspace::SwapPaneInDirection", "Right"),
                        "workspace::SwapPaneRight",
                    ),
                    // menu
                    (
                        ("app_menu::NavigateApplicationMenuInDirection", "Left"),
                        "app_menu::ActivateMenuLeft",
                    ),
                    (
                        ("app_menu::NavigateApplicationMenuInDirection", "Right"),
                        "app_menu::ActivateMenuRight",
                    ),
                    // vim push
                    (("vim::PushOperator", "Change"), "vim::PushChange"),
                    (("vim::PushOperator", "Delete"), "vim::PushDelete"),
                    (("vim::PushOperator", "Yank"), "vim::PushYank"),
                    (("vim::PushOperator", "Replace"), "vim::PushReplace"),
                    (
                        ("vim::PushOperator", "DeleteSurrounds"),
                        "vim::PushDeleteSurrounds",
                    ),
                    (("vim::PushOperator", "Mark"), "vim::PushMark"),
                    (("vim::PushOperator", "Indent"), "vim::PushIndent"),
                    (("vim::PushOperator", "Outdent"), "vim::PushOutdent"),
                    (("vim::PushOperator", "AutoIndent"), "vim::PushAutoIndent"),
                    (("vim::PushOperator", "Rewrap"), "vim::PushRewrap"),
                    (
                        ("vim::PushOperator", "ShellCommand"),
                        "vim::PushShellCommand",
                    ),
                    (("vim::PushOperator", "Lowercase"), "vim::PushLowercase"),
                    (("vim::PushOperator", "Uppercase"), "vim::PushUppercase"),
                    (
                        ("vim::PushOperator", "OppositeCase"),
                        "vim::PushOppositeCase",
                    ),
                    (("vim::PushOperator", "Register"), "vim::PushRegister"),
                    (
                        ("vim::PushOperator", "RecordRegister"),
                        "vim::PushRecordRegister",
                    ),
                    (
                        ("vim::PushOperator", "ReplayRegister"),
                        "vim::PushReplayRegister",
                    ),
                    (
                        ("vim::PushOperator", "ReplaceWithRegister"),
                        "vim::PushReplaceWithRegister",
                    ),
                    (
                        ("vim::PushOperator", "ToggleComments"),
                        "vim::PushToggleComments",
                    ),
                    // vim switch
                    (("vim::SwitchMode", "Normal"), "vim::SwitchToNormalMode"),
                    (("vim::SwitchMode", "Insert"), "vim::SwitchToInsertMode"),
                    (("vim::SwitchMode", "Replace"), "vim::SwitchToReplaceMode"),
                    (("vim::SwitchMode", "Visual"), "vim::SwitchToVisualMode"),
                    (
                        ("vim::SwitchMode", "VisualLine"),
                        "vim::SwitchToVisualLineMode",
                    ),
                    (
                        ("vim::SwitchMode", "VisualBlock"),
                        "vim::SwitchToVisualBlockMode",
                    ),
                    (
                        ("vim::SwitchMode", "HelixNormal"),
                        "vim::SwitchToHelixNormalMode",
                    ),
                    // vim resize
                    (("vim::ResizePane", "Widen"), "vim::ResizePaneRight"),
                    (("vim::ResizePane", "Narrow"), "vim::ResizePaneLeft"),
                    (("vim::ResizePane", "Shorten"), "vim::ResizePaneDown"),
                    (("vim::ResizePane", "Lengthen"), "vim::ResizePaneUp"),
                    // fold at level
                    (("editor::FoldAtLevel", "1"), "editor::FoldAtLevel1"),
                    (("editor::FoldAtLevel", "2"), "editor::FoldAtLevel2"),
                    (("editor::FoldAtLevel", "3"), "editor::FoldAtLevel3"),
                    (("editor::FoldAtLevel", "4"), "editor::FoldAtLevel4"),
                    (("editor::FoldAtLevel", "5"), "editor::FoldAtLevel5"),
                    (("editor::FoldAtLevel", "6"), "editor::FoldAtLevel6"),
                    (("editor::FoldAtLevel", "7"), "editor::FoldAtLevel7"),
                    (("editor::FoldAtLevel", "8"), "editor::FoldAtLevel8"),
                    (("editor::FoldAtLevel", "9"), "editor::FoldAtLevel9"),
                ])
            });

            /// [ "editor::FoldAtLevel", { "level": 1 } ] -> [ "editor::FoldAtLevel", 1 ]
            fn replace_action_argument_object_with_single_value(
                contents: &str,
                mat: &QueryMatch,
                query: &Query,
            ) -> Option<(Range<usize>, String)> {
                let array_ix = query.capture_index_for_name("array")?;
                let action_name_ix = query.capture_index_for_name("action_name")?;
                let argument_key_ix = query.capture_index_for_name("argument_key")?;
                let argument_value_ix = query.capture_index_for_name("argument_value")?;

                let action_name = contents.get(
                    mat.nodes_for_capture_index(action_name_ix)
                        .next()?
                        .byte_range(),
                )?;
                let argument_key = contents.get(
                    mat.nodes_for_capture_index(argument_key_ix)
                        .next()?
                        .byte_range(),
                )?;
                let argument_value = contents.get(
                    mat.nodes_for_capture_index(argument_value_ix)
                        .next()?
                        .byte_range(),
                )?;

                let new_action_name = UNWRAP_OBJECTS.get(&action_name)?.get(&argument_key)?;

                let range_to_replace = mat.nodes_for_capture_index(array_ix).next()?.byte_range();
                let replacement = format!("[\"{}\", {}]", new_action_name, argument_value);
                Some((range_to_replace, replacement))
            }

            /// "ctrl-k ctrl-1": [ "editor::PushOperator", { "Object": {} } ] -> [ "editor::vim::PushObject", {} ]
            static UNWRAP_OBJECTS: LazyLock<HashMap<&str, HashMap<&str, &str>>> =
                LazyLock::new(|| {
                    HashMap::from_iter([
                        (
                            "editor::FoldAtLevel",
                            HashMap::from_iter([("level", "editor::FoldAtLevel")]),
                        ),
                        (
                            "vim::PushOperator",
                            HashMap::from_iter([
                                ("Object", "vim::PushObject"),
                                ("FindForward", "vim::PushFindForward"),
                                ("FindBackward", "vim::PushFindBackward"),
                                ("Sneak", "vim::PushSneak"),
                                ("SneakBackward", "vim::PushSneakBackward"),
                                ("AddSurrounds", "vim::PushAddSurrounds"),
                                ("ChangeSurrounds", "vim::PushChangeSurrounds"),
                                ("Jump", "vim::PushJump"),
                                ("Digraph", "vim::PushDigraph"),
                                ("Literal", "vim::PushLiteral"),
                            ]),
                        ),
                    ])
                });
        }

        pub(crate) use keymap::KEYMAP_PATTERNS;
    }

    pub(crate) mod m_2025_01_30 {
        mod keymap {
            use collections::HashMap;
            use convert_case::{Case, Casing};
            use std::{ops::Range, sync::LazyLock};
            use tree_sitter::{Query, QueryMatch};

            use crate::MigrationPatterns;
            use crate::patterns::KEYMAP_ACTION_ARRAY_ARGUMENT_AS_OBJECT_PATTERN;

            pub const KEYMAP_PATTERNS: MigrationPatterns = &[(
                KEYMAP_ACTION_ARRAY_ARGUMENT_AS_OBJECT_PATTERN,
                action_argument_snake_case,
            )];

            fn to_snake_case(text: &str) -> String {
                text.to_case(Case::Snake)
            }

            fn action_argument_snake_case(
                contents: &str,
                mat: &QueryMatch,
                query: &Query,
            ) -> Option<(Range<usize>, String)> {
                let array_ix = query.capture_index_for_name("array")?;
                let action_name_ix = query.capture_index_for_name("action_name")?;
                let argument_key_ix = query.capture_index_for_name("argument_key")?;
                let argument_value_ix = query.capture_index_for_name("argument_value")?;
                let action_name = contents.get(
                    mat.nodes_for_capture_index(action_name_ix)
                        .next()?
                        .byte_range(),
                )?;

                let replacement_key = ACTION_ARGUMENT_SNAKE_CASE_REPLACE.get(action_name)?;
                let argument_key = contents.get(
                    mat.nodes_for_capture_index(argument_key_ix)
                        .next()?
                        .byte_range(),
                )?;

                if argument_key != *replacement_key {
                    return None;
                }

                let argument_value_node = mat.nodes_for_capture_index(argument_value_ix).next()?;
                let argument_value = contents.get(argument_value_node.byte_range())?;

                let new_key = to_snake_case(argument_key);
                let new_value = if argument_value_node.kind() == "string" {
                    format!("\"{}\"", to_snake_case(argument_value.trim_matches('"')))
                } else {
                    argument_value.to_string()
                };

                let range_to_replace = mat.nodes_for_capture_index(array_ix).next()?.byte_range();
                let replacement = format!(
                    "[\"{}\", {{ \"{}\": {} }}]",
                    action_name, new_key, new_value
                );

                Some((range_to_replace, replacement))
            }

            static ACTION_ARGUMENT_SNAKE_CASE_REPLACE: LazyLock<HashMap<&str, &str>> =
                LazyLock::new(|| {
                    HashMap::from_iter([
                        ("vim::NextWordStart", "ignorePunctuation"),
                        ("vim::NextWordEnd", "ignorePunctuation"),
                        ("vim::PreviousWordStart", "ignorePunctuation"),
                        ("vim::PreviousWordEnd", "ignorePunctuation"),
                        ("vim::MoveToNext", "partialWord"),
                        ("vim::MoveToPrev", "partialWord"),
                        ("vim::Down", "displayLines"),
                        ("vim::Up", "displayLines"),
                        ("vim::EndOfLine", "displayLines"),
                        ("vim::StartOfLine", "displayLines"),
                        ("vim::FirstNonWhitespace", "displayLines"),
                        ("pane::CloseActiveItem", "saveIntent"),
                        ("vim::Paste", "preserveClipboard"),
                        ("vim::Word", "ignorePunctuation"),
                        ("vim::Subword", "ignorePunctuation"),
                        ("vim::IndentObj", "includeBelow"),
                    ])
                });
        }

        mod settings {
            use std::ops::Range;
            use tree_sitter::{Query, QueryMatch};

            use crate::MigrationPatterns;
            use crate::patterns::SETTINGS_NESTED_KEY_VALUE_PATTERN;

            pub const SETTINGS_PATTERNS: MigrationPatterns = &[
                (
                    SETTINGS_NESTED_KEY_VALUE_PATTERN,
                    replace_tab_close_button_setting_key,
                ),
                (
                    SETTINGS_NESTED_KEY_VALUE_PATTERN,
                    replace_tab_close_button_setting_value,
                ),
            ];

            fn replace_tab_close_button_setting_key(
                contents: &str,
                mat: &QueryMatch,
                query: &Query,
            ) -> Option<(Range<usize>, String)> {
                let parent_object_capture_ix = query.capture_index_for_name("parent_key")?;
                let parent_object_range = mat
                    .nodes_for_capture_index(parent_object_capture_ix)
                    .next()?
                    .byte_range();
                let parent_object_name = contents.get(parent_object_range)?;

                let setting_name_ix = query.capture_index_for_name("setting_name")?;
                let setting_range = mat
                    .nodes_for_capture_index(setting_name_ix)
                    .next()?
                    .byte_range();
                let setting_name = contents.get(setting_range.clone())?;

                if parent_object_name == "tabs" && setting_name == "always_show_close_button" {
                    return Some((setting_range, "show_close_button".into()));
                }

                None
            }

            fn replace_tab_close_button_setting_value(
                contents: &str,
                mat: &QueryMatch,
                query: &Query,
            ) -> Option<(Range<usize>, String)> {
                let parent_object_capture_ix = query.capture_index_for_name("parent_key")?;
                let parent_object_range = mat
                    .nodes_for_capture_index(parent_object_capture_ix)
                    .next()?
                    .byte_range();
                let parent_object_name = contents.get(parent_object_range)?;

                let setting_name_ix = query.capture_index_for_name("setting_name")?;
                let setting_name_range = mat
                    .nodes_for_capture_index(setting_name_ix)
                    .next()?
                    .byte_range();
                let setting_name = contents.get(setting_name_range)?;

                let setting_value_ix = query.capture_index_for_name("setting_value")?;
                let setting_value_range = mat
                    .nodes_for_capture_index(setting_value_ix)
                    .next()?
                    .byte_range();
                let setting_value = contents.get(setting_value_range.clone())?;

                if parent_object_name == "tabs" && setting_name == "always_show_close_button" {
                    match setting_value {
                        "true" => {
                            return Some((setting_value_range, "\"always\"".to_string()));
                        }
                        "false" => {
                            return Some((setting_value_range, "\"hover\"".to_string()));
                        }
                        _ => {}
                    }
                }

                None
            }
        }

        pub(crate) use keymap::KEYMAP_PATTERNS;
        pub(crate) use settings::SETTINGS_PATTERNS;
    }

    pub(crate) mod m_2025_03_03 {
        mod keymap {
            use collections::HashMap;
            use std::{ops::Range, sync::LazyLock};
            use tree_sitter::{Query, QueryMatch};

            use crate::MigrationPatterns;
            use crate::patterns::KEYMAP_ACTION_STRING_PATTERN;

            pub const KEYMAP_PATTERNS: MigrationPatterns =
                &[(KEYMAP_ACTION_STRING_PATTERN, replace_string_action)];

            fn replace_string_action(
                contents: &str,
                mat: &QueryMatch,
                query: &Query,
            ) -> Option<(Range<usize>, String)> {
                let action_name_ix = query.capture_index_for_name("action_name")?;
                let action_name_node = mat.nodes_for_capture_index(action_name_ix).next()?;
                let action_name_range = action_name_node.byte_range();
                let action_name = contents.get(action_name_range.clone())?;

                if let Some(new_action_name) = STRING_REPLACE.get(&action_name) {
                    return Some((action_name_range, new_action_name.to_string()));
                }

                if let Some((new_action_name, options)) = STRING_TO_ARRAY_REPLACE.get(action_name) {
                    let full_string_range = action_name_node.parent()?.byte_range();
                    let mut options_parts = Vec::new();
                    for (key, value) in options.iter() {
                        options_parts.push(format!("\"{}\": {}", key, value));
                    }
                    let options_str = options_parts.join(", ");
                    let replacement = format!("[\"{}\", {{ {} }}]", new_action_name, options_str);
                    return Some((full_string_range, replacement));
                }

                None
            }

            static STRING_REPLACE: LazyLock<HashMap<&str, &str>> = LazyLock::new(|| {
                HashMap::from_iter([
                    (
                        "editor::GoToPrevDiagnostic",
                        "editor::GoToPreviousDiagnostic",
                    ),
                    ("editor::ContextMenuPrev", "editor::ContextMenuPrevious"),
                    ("search::SelectPrevMatch", "search::SelectPreviousMatch"),
                    ("file_finder::SelectPrev", "file_finder::SelectPrevious"),
                    ("menu::SelectPrev", "menu::SelectPrevious"),
                    ("editor::TabPrev", "editor::Backtab"),
                    ("pane::ActivatePrevItem", "pane::ActivatePreviousItem"),
                    ("vim::MoveToPrev", "vim::MoveToPrevious"),
                    ("vim::MoveToPrevMatch", "vim::MoveToPreviousMatch"),
                ])
            });

            /// "editor::GoToPrevHunk" -> ["editor::GoToPreviousHunk", { "center_cursor": true }]
            static STRING_TO_ARRAY_REPLACE: LazyLock<HashMap<&str, (&str, HashMap<&str, bool>)>> =
                LazyLock::new(|| {
                    HashMap::from_iter([
                        (
                            "editor::GoToHunk",
                            (
                                "editor::GoToHunk",
                                HashMap::from_iter([("center_cursor", true)]),
                            ),
                        ),
                        (
                            "editor::GoToPrevHunk",
                            (
                                "editor::GoToPreviousHunk",
                                HashMap::from_iter([("center_cursor", true)]),
                            ),
                        ),
                    ])
                });
        }

        pub(crate) use keymap::KEYMAP_PATTERNS;
    }

    pub(crate) mod m_2025_03_06 {
        mod keymap {
            use collections::HashSet;
            use std::{ops::Range, sync::LazyLock};
            use tree_sitter::{Query, QueryMatch};

            use crate::MigrationPatterns;
            use crate::patterns::KEYMAP_ACTION_ARRAY_ARGUMENT_AS_OBJECT_PATTERN;

            pub const KEYMAP_PATTERNS: MigrationPatterns = &[(
                KEYMAP_ACTION_ARRAY_ARGUMENT_AS_OBJECT_PATTERN,
                replace_array_with_single_string,
            )];

            fn replace_array_with_single_string(
                contents: &str,
                mat: &QueryMatch,
                query: &Query,
            ) -> Option<(Range<usize>, String)> {
                let array_ix = query.capture_index_for_name("array")?;
                let action_name_ix = query.capture_index_for_name("action_name")?;

                let action_name = contents.get(
                    mat.nodes_for_capture_index(action_name_ix)
                        .next()?
                        .byte_range(),
                )?;

                if TRANSFORM_ARRAY.contains(&action_name) {
                    let replacement_as_string = format!("\"{action_name}\"");
                    let range_to_replace =
                        mat.nodes_for_capture_index(array_ix).next()?.byte_range();
                    return Some((range_to_replace, replacement_as_string));
                }

                None
            }

            /// ["editor::GoToPreviousHunk", { "center_cursor": true }] -> "editor::GoToPreviousHunk"
            static TRANSFORM_ARRAY: LazyLock<HashSet<&str>> = LazyLock::new(|| {
                HashSet::from_iter(["editor::GoToHunk", "editor::GoToPreviousHunk"])
            });
        }

        pub(crate) use keymap::KEYMAP_PATTERNS;
    }

    pub(crate) mod m_2025_03_29 {
        mod settings {
            use std::ops::Range;
            use tree_sitter::{Query, QueryMatch};

            use crate::MigrationPatterns;
            use crate::patterns::SETTINGS_ROOT_KEY_VALUE_PATTERN;

            pub const SETTINGS_PATTERNS: MigrationPatterns = &[
                (SETTINGS_ROOT_KEY_VALUE_PATTERN, replace_setting_name),
                (SETTINGS_ROOT_KEY_VALUE_PATTERN, replace_setting_value),
            ];

            fn replace_setting_value(
                contents: &str,
                mat: &QueryMatch,
                query: &Query,
            ) -> Option<(Range<usize>, String)> {
                let setting_capture_ix = query.capture_index_for_name("name")?;
                let setting_name_range = mat
                    .nodes_for_capture_index(setting_capture_ix)
                    .next()?
                    .byte_range();
                let setting_name = contents.get(setting_name_range)?;

                if setting_name != "hide_mouse_while_typing" {
                    return None;
                }

                let value_capture_ix = query.capture_index_for_name("value")?;
                let value_range = mat
                    .nodes_for_capture_index(value_capture_ix)
                    .next()?
                    .byte_range();
                let value = contents.get(value_range.clone())?;

                let new_value = if value.trim() == "true" {
                    "\"on_typing_and_movement\""
                } else if value.trim() == "false" {
                    "\"never\""
                } else {
                    return None;
                };

                Some((value_range, new_value.to_string()))
            }

            fn replace_setting_name(
                contents: &str,
                mat: &QueryMatch,
                query: &Query,
            ) -> Option<(Range<usize>, String)> {
                let setting_capture_ix = query.capture_index_for_name("name")?;
                let setting_name_range = mat
                    .nodes_for_capture_index(setting_capture_ix)
                    .next()?
                    .byte_range();
                let setting_name = contents.get(setting_name_range.clone())?;

                let new_setting_name = if setting_name == "hide_mouse_while_typing" {
                    "hide_mouse"
                } else {
                    return None;
                };

                Some((setting_name_range, new_setting_name.to_string()))
            }
        }

        pub(crate) use settings::SETTINGS_PATTERNS;
    }

    pub(crate) mod m_2025_04_15 {
        mod keymap {
            use collections::HashMap;
            use std::{ops::Range, sync::LazyLock};
            use tree_sitter::{Query, QueryMatch};

            use crate::MigrationPatterns;
            use crate::patterns::KEYMAP_ACTION_STRING_PATTERN;

            pub const KEYMAP_PATTERNS: MigrationPatterns =
                &[(KEYMAP_ACTION_STRING_PATTERN, replace_string_action)];

            fn replace_string_action(
                contents: &str,
                mat: &QueryMatch,
                query: &Query,
            ) -> Option<(Range<usize>, String)> {
                let action_name_ix = query.capture_index_for_name("action_name")?;
                let action_name_node = mat.nodes_for_capture_index(action_name_ix).next()?;
                let action_name_range = action_name_node.byte_range();
                let action_name = contents.get(action_name_range.clone())?;

                if let Some(new_action_name) = STRING_REPLACE.get(&action_name) {
                    return Some((action_name_range, new_action_name.to_string()));
                }

                None
            }

            /// "space": "outline_panel::Open" -> "outline_panel::OpenSelectedEntry"
            static STRING_REPLACE: LazyLock<HashMap<&str, &str>> = LazyLock::new(|| {
                HashMap::from_iter([("outline_panel::Open", "outline_panel::OpenSelectedEntry")])
            });
        }

        pub(crate) use keymap::KEYMAP_PATTERNS;
    }

    pub(crate) mod m_2025_07_08 {
        mod settings {
            use std::ops::Range;
            use tree_sitter::{Query, QueryMatch};

            use crate::MigrationPatterns;
            use crate::patterns::SETTINGS_ROOT_KEY_VALUE_PATTERN;

            pub const SETTINGS_PATTERNS: MigrationPatterns = &[(
                SETTINGS_ROOT_KEY_VALUE_PATTERN,
                migrate_drag_and_drop_selection,
            )];

            fn migrate_drag_and_drop_selection(
                contents: &str,
                mat: &QueryMatch,
                query: &Query,
            ) -> Option<(Range<usize>, String)> {
                let name_ix = query.capture_index_for_name("name")?;
                let name_range = mat.nodes_for_capture_index(name_ix).next()?.byte_range();
                let name = contents.get(name_range)?;

                if name != "drag_and_drop_selection" {
                    return None;
                }

                let value_ix = query.capture_index_for_name("value")?;
                let value_node = mat.nodes_for_capture_index(value_ix).next()?;
                let value_range = value_node.byte_range();
                let value = contents.get(value_range.clone())?;

                match value {
                    "true" | "false" => {
                        let replacement = format!("{{\n    \"enabled\": {}\n  }}", value);
                        Some((value_range, replacement))
                    }
                    _ => None,
                }
            }
        }

        pub(crate) use settings::SETTINGS_PATTERNS;
    }

    pub(crate) mod m_2025_10_01 {
        mod settings {
            use crate::migrations::migrate_language_setting;
            use anyhow::Result;
            use serde_json::Value;

            pub fn flatten_code_actions_formatters(value: &mut Value) -> Result<()> {
                migrate_language_setting(value, |value, _path| {
                    let Some(obj) = value.as_object_mut() else {
                        return Ok(());
                    };
                    for key in ["formatter", "format_on_save"] {
                        let Some(formatter) = obj.get_mut(key) else {
                            continue;
                        };
                        let new_formatter = match formatter {
                            Value::Array(arr) => {
                                let mut new_arr = Vec::new();
                                let mut found_code_actions = false;
                                for item in arr {
                                    let Some(obj) = item.as_object() else {
                                        new_arr.push(item.clone());
                                        continue;
                                    };
                                    let code_actions_obj = obj
                                        .get("code_actions")
                                        .and_then(|code_actions| code_actions.as_object());
                                    let Some(code_actions) = code_actions_obj else {
                                        new_arr.push(item.clone());
                                        continue;
                                    };
                                    found_code_actions = true;
                                    for (name, enabled) in code_actions {
                                        if !enabled.as_bool().unwrap_or(true) {
                                            continue;
                                        }
                                        new_arr.push(serde_json::json!({
                                            "code_action": name
                                        }));
                                    }
                                }
                                if !found_code_actions {
                                    continue;
                                }
                                Value::Array(new_arr)
                            }
                            Value::Object(obj) => {
                                let mut new_arr = Vec::new();
                                let code_actions_obj = obj
                                    .get("code_actions")
                                    .and_then(|code_actions| code_actions.as_object());
                                let Some(code_actions) = code_actions_obj else {
                                    continue;
                                };
                                for (name, enabled) in code_actions {
                                    if !enabled.as_bool().unwrap_or(true) {
                                        continue;
                                    }
                                    new_arr.push(serde_json::json!({
                                        "code_action": name
                                    }));
                                }
                                if new_arr.len() == 1 {
                                    new_arr.pop().unwrap()
                                } else {
                                    Value::Array(new_arr)
                                }
                            }
                            _ => continue,
                        };

                        obj.insert(key.to_string(), new_formatter);
                    }
                    return Ok(());
                })
            }
        }

        pub(crate) use settings::flatten_code_actions_formatters;
    }

    pub(crate) mod m_2025_10_02 {
        mod settings {
            use anyhow::Result;
            use serde_json::Value;

            use crate::migrations::migrate_language_setting;

            pub fn remove_formatters_on_save(value: &mut Value) -> Result<()> {
                migrate_language_setting(value, remove_formatters_on_save_inner)
            }

            fn remove_formatters_on_save_inner(value: &mut Value, path: &[&str]) -> Result<()> {
                let Some(obj) = value.as_object_mut() else {
                    return Ok(());
                };
                let Some(format_on_save) = obj.get("format_on_save").cloned() else {
                    return Ok(());
                };
                let is_format_on_save_set_to_formatter =
                    format_on_save.as_str().map_or(true, |s| {
                        s != "on"
                            && s != "off"
                            && s != "modifications"
                            && s != "modifications_if_available"
                    });
                if !is_format_on_save_set_to_formatter {
                    return Ok(());
                }

                fn fmt_path(path: &[&str], key: &str) -> String {
                    let mut path = path.to_vec();
                    path.push(key);
                    path.join(".")
                }

                anyhow::ensure!(
                    obj.get("formatter").is_none(),
                    r#"Setting formatters in both "format_on_save" and "formatter" is deprecated. Please migrate the formatters from {} into {}"#,
                    fmt_path(path, "format_on_save"),
                    fmt_path(path, "formatter")
                );

                obj.insert("format_on_save".to_string(), serde_json::json!("on"));
                obj.insert("formatter".to_string(), format_on_save);

                Ok(())
            }
        }

        pub(crate) use settings::remove_formatters_on_save;
    }

    pub(crate) mod m_2025_10_16 {
        mod settings {
            use anyhow::Result;
            use serde_json::Value;

            use crate::migrations::migrate_language_setting;

            pub fn restore_code_actions_on_format(value: &mut Value) -> Result<()> {
                migrate_language_setting(value, restore_code_actions_on_format_inner)
            }

            fn restore_code_actions_on_format_inner(
                value: &mut Value,
                path: &[&str],
            ) -> Result<()> {
                let Some(obj) = value.as_object_mut() else {
                    return Ok(());
                };
                let code_actions_on_format = obj
                    .get("code_actions_on_format")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Default::default()));

                fn fmt_path(path: &[&str], key: &str) -> String {
                    let mut path = path.to_vec();
                    path.push(key);
                    path.join(".")
                }

                let Some(mut code_actions_map) = code_actions_on_format.as_object().cloned() else {
                    anyhow::bail!(
                        r#"The `code_actions_on_format` is in an invalid state and cannot be migrated at {}. Please ensure the code_actions_on_format setting is a Map<String, bool>"#,
                        fmt_path(path, "code_actions_on_format"),
                    );
                };

                let Some(formatter) = obj.get("formatter") else {
                    return Ok(());
                };
                let formatter_array = if let Some(array) = formatter.as_array() {
                    array.clone()
                } else {
                    vec![formatter.clone()]
                };
                if formatter_array.is_empty() {
                    return Ok(());
                }
                let mut code_action_formatters = Vec::new();
                for formatter in formatter_array {
                    let Some(code_action) = formatter.get("code_action") else {
                        return Ok(());
                    };
                    let Some(code_action_name) = code_action.as_str() else {
                        anyhow::bail!(
                            r#"The `code_action` is in an invalid state and cannot be migrated at {}. Please ensure the code_action setting is a String"#,
                            fmt_path(path, "formatter"),
                        );
                    };
                    code_action_formatters.push(code_action_name.to_string());
                }

                code_actions_map.extend(
                    code_action_formatters
                        .into_iter()
                        .rev()
                        .map(|code_action| (code_action, Value::Bool(true))),
                );

                obj.insert("formatter".to_string(), Value::Array(vec![]));
                obj.insert(
                    "code_actions_on_format".into(),
                    Value::Object(code_actions_map),
                );

                Ok(())
            }
        }

        pub(crate) use settings::restore_code_actions_on_format;
    }

    pub(crate) mod m_2025_10_17 {
        mod settings {
            use anyhow::Result;
            use serde_json::Value;

            use crate::migrations::migrate_settings;

            pub fn make_file_finder_include_ignored_an_enum(value: &mut Value) -> Result<()> {
                migrate_settings(value, &mut migrate_one)
            }

            fn migrate_one(obj: &mut serde_json::Map<String, Value>) -> Result<()> {
                let Some(file_finder) = obj.get_mut("file_finder") else {
                    return Ok(());
                };

                let Some(file_finder_obj) = file_finder.as_object_mut() else {
                    anyhow::bail!("Expected file_finder to be an object");
                };

                let Some(include_ignored) = file_finder_obj.get_mut("include_ignored") else {
                    return Ok(());
                };
                *include_ignored = match include_ignored {
                    Value::Bool(true) => Value::String("all".to_string()),
                    Value::Bool(false) => Value::String("indexed".to_string()),
                    Value::Null => Value::String("smart".to_string()),
                    Value::String(s) if s == "all" || s == "indexed" || s == "smart" => {
                        return Ok(());
                    }
                    _ => anyhow::bail!("Expected include_ignored to be a boolean or null"),
                };
                Ok(())
            }
        }

        pub(crate) use settings::make_file_finder_include_ignored_an_enum;
    }

    pub(crate) mod m_2025_10_21 {
        mod settings {
            use anyhow::Result;
            use serde_json::Value;

            use crate::migrations::migrate_settings;

            pub fn make_relative_line_numbers_an_enum(value: &mut Value) -> Result<()> {
                migrate_settings(value, &mut migrate_one)
            }

            fn migrate_one(obj: &mut serde_json::Map<String, Value>) -> Result<()> {
                let Some(relative_line_numbers) = obj.get_mut("relative_line_numbers") else {
                    return Ok(());
                };

                *relative_line_numbers = match relative_line_numbers {
                    Value::Bool(true) => Value::String("enabled".to_string()),
                    Value::Bool(false) => Value::String("disabled".to_string()),
                    Value::String(s) if s == "enabled" || s == "disabled" || s == "wrapped" => {
                        return Ok(());
                    }
                    _ => anyhow::bail!("Expected relative_line_numbers to be a boolean"),
                };
                Ok(())
            }
        }

        pub(crate) use settings::make_relative_line_numbers_an_enum;
    }

    pub(crate) mod m_2025_11_12 {
        mod settings {
            use std::ops::Range;
            use tree_sitter::{Query, QueryMatch};

            use crate::MigrationPatterns;
            use crate::patterns::SETTINGS_NESTED_KEY_VALUE_PATTERN;

            pub const SETTINGS_PATTERNS: MigrationPatterns = &[
                (
                    SETTINGS_NESTED_KEY_VALUE_PATTERN,
                    rename_open_file_on_paste_setting,
                ),
                (
                    SETTINGS_NESTED_KEY_VALUE_PATTERN,
                    replace_open_file_on_paste_setting_value,
                ),
            ];

            fn rename_open_file_on_paste_setting(
                contents: &str,
                mat: &QueryMatch,
                query: &Query,
            ) -> Option<(Range<usize>, String)> {
                if !is_project_panel_open_file_on_paste(contents, mat, query) {
                    return None;
                }

                let setting_name_ix = query.capture_index_for_name("setting_name")?;
                let setting_name_range = mat
                    .nodes_for_capture_index(setting_name_ix)
                    .next()?
                    .byte_range();

                Some((setting_name_range, "auto_open".to_string()))
            }

            fn replace_open_file_on_paste_setting_value(
                contents: &str,
                mat: &QueryMatch,
                query: &Query,
            ) -> Option<(Range<usize>, String)> {
                if !is_project_panel_open_file_on_paste(contents, mat, query) {
                    return None;
                }

                let value_ix = query.capture_index_for_name("setting_value")?;
                let value_node = mat.nodes_for_capture_index(value_ix).next()?;
                let value_range = value_node.byte_range();
                let value_text = contents.get(value_range.clone())?.trim();

                let normalized_value = match value_text {
                    "true" => "true",
                    "false" => "false",
                    _ => return None,
                };

                Some((
                    value_range,
                    format!("{{ \"on_paste\": {normalized_value} }}"),
                ))
            }

            fn is_project_panel_open_file_on_paste(
                contents: &str,
                mat: &QueryMatch,
                query: &Query,
            ) -> bool {
                let parent_key_ix = match query.capture_index_for_name("parent_key") {
                    Some(ix) => ix,
                    None => return false,
                };
                let parent_range = match mat.nodes_for_capture_index(parent_key_ix).next() {
                    Some(node) => node.byte_range(),
                    None => return false,
                };
                if contents.get(parent_range) != Some("project_panel") {
                    return false;
                }

                let setting_name_ix = match query.capture_index_for_name("setting_name") {
                    Some(ix) => ix,
                    None => return false,
                };
                let setting_name_range = match mat.nodes_for_capture_index(setting_name_ix).next() {
                    Some(node) => node.byte_range(),
                    None => return false,
                };
                contents.get(setting_name_range) == Some("open_file_on_paste")
            }
        }

        pub(crate) use settings::SETTINGS_PATTERNS;
    }

    pub(crate) mod m_2025_12_01 {
        mod settings {
            use std::ops::Range;
            use tree_sitter::{Query, QueryMatch};

            use crate::MigrationPatterns;
            use crate::patterns::SETTINGS_NESTED_KEY_VALUE_PATTERN;

            pub const SETTINGS_PATTERNS: MigrationPatterns = &[(
                SETTINGS_NESTED_KEY_VALUE_PATTERN,
                rename_enable_preview_from_code_navigation_setting,
            )];

            fn rename_enable_preview_from_code_navigation_setting(
                contents: &str,
                mat: &QueryMatch,
                query: &Query,
            ) -> Option<(Range<usize>, String)> {
                if !is_enable_preview_from_code_navigation(contents, mat, query) {
                    return None;
                }

                let setting_name_ix = query.capture_index_for_name("setting_name")?;
                let setting_name_range = mat
                    .nodes_for_capture_index(setting_name_ix)
                    .next()?
                    .byte_range();

                Some((
                    setting_name_range,
                    "enable_keep_preview_on_code_navigation".to_string(),
                ))
            }

            fn is_enable_preview_from_code_navigation(
                contents: &str,
                mat: &QueryMatch,
                query: &Query,
            ) -> bool {
                let parent_key_ix = match query.capture_index_for_name("parent_key") {
                    Some(ix) => ix,
                    None => return false,
                };
                let parent_range = match mat.nodes_for_capture_index(parent_key_ix).next() {
                    Some(node) => node.byte_range(),
                    None => return false,
                };
                if contents.get(parent_range) != Some("preview_tabs") {
                    return false;
                }

                let setting_name_ix = match query.capture_index_for_name("setting_name") {
                    Some(ix) => ix,
                    None => return false,
                };
                let setting_name_range = match mat.nodes_for_capture_index(setting_name_ix).next() {
                    Some(node) => node.byte_range(),
                    None => return false,
                };
                contents.get(setting_name_range) == Some("enable_preview_from_code_navigation")
            }
        }

        pub(crate) use settings::SETTINGS_PATTERNS;
    }

    pub(crate) mod m_2025_12_15 {
        mod settings {
            use std::ops::Range;
            use tree_sitter::{Query, QueryMatch};

            use crate::MigrationPatterns;
            use crate::patterns::SETTINGS_NESTED_KEY_VALUE_PATTERN;

            pub const SETTINGS_PATTERNS: MigrationPatterns = &[(
                SETTINGS_NESTED_KEY_VALUE_PATTERN,
                rename_restore_on_startup_values,
            )];

            fn rename_restore_on_startup_values(
                contents: &str,
                mat: &QueryMatch,
                query: &Query,
            ) -> Option<(Range<usize>, String)> {
                if !is_restore_on_startup_setting(contents, mat, query) {
                    return None;
                }

                let setting_value_ix = query.capture_index_for_name("setting_value")?;
                let setting_value_range = mat
                    .nodes_for_capture_index(setting_value_ix)
                    .next()?
                    .byte_range();
                let setting_value = contents.get(setting_value_range.clone())?;

                // The value includes quotes, so we check for the quoted string
                let new_value = match setting_value.trim() {
                    "\"none\"" => "\"empty_tab\"",
                    "\"welcome\"" => "\"launchpad\"",
                    _ => return None,
                };

                Some((setting_value_range, new_value.to_string()))
            }

            fn is_restore_on_startup_setting(
                contents: &str,
                mat: &QueryMatch,
                query: &Query,
            ) -> bool {
                // Check that the parent key is "workspace" (since restore_on_startup is under workspace settings)
                // Actually, restore_on_startup can be at the root level too, so we need to handle both cases
                // The SETTINGS_NESTED_KEY_VALUE_PATTERN captures parent_key and setting_name

                let setting_name_ix = match query.capture_index_for_name("setting_name") {
                    Some(ix) => ix,
                    None => return false,
                };
                let setting_name_range = match mat.nodes_for_capture_index(setting_name_ix).next() {
                    Some(node) => node.byte_range(),
                    None => return false,
                };
                contents.get(setting_name_range) == Some("restore_on_startup")
            }
        }

        pub(crate) use settings::SETTINGS_PATTERNS;
    }

    pub(crate) mod m_2026_03_16 {
        mod settings {
            use std::ops::Range;
            use tree_sitter::{Query, QueryMatch};

            use crate::MigrationPatterns;
            use crate::patterns::SETTINGS_NESTED_KEY_VALUE_PATTERN;

            pub const SETTINGS_PATTERNS: MigrationPatterns =
                &[(SETTINGS_NESTED_KEY_VALUE_PATTERN, rename_heex_settings)];

            fn rename_heex_settings(
                contents: &str,
                mat: &QueryMatch,
                query: &Query,
            ) -> Option<(Range<usize>, String)> {
                if !is_heex_settings(contents, mat, query) {
                    return None;
                }

                let setting_name_ix = query.capture_index_for_name("setting_name")?;
                let setting_name_range = mat
                    .nodes_for_capture_index(setting_name_ix)
                    .next()?
                    .byte_range();

                Some((setting_name_range, "HEEx".to_string()))
            }

            fn is_heex_settings(contents: &str, mat: &QueryMatch, query: &Query) -> bool {
                let parent_key_ix = match query.capture_index_for_name("parent_key") {
                    Some(ix) => ix,
                    None => return false,
                };
                let parent_range = match mat.nodes_for_capture_index(parent_key_ix).next() {
                    Some(node) => node.byte_range(),
                    None => return false,
                };
                if contents.get(parent_range) != Some("languages") {
                    return false;
                }

                let setting_name_ix = match query.capture_index_for_name("setting_name") {
                    Some(ix) => ix,
                    None => return false,
                };
                let setting_name_range = match mat.nodes_for_capture_index(setting_name_ix).next() {
                    Some(node) => node.byte_range(),
                    None => return false,
                };
                contents.get(setting_name_range) == Some("HEEX")
            }
        }

        pub(crate) use settings::SETTINGS_PATTERNS;
    }

    pub(crate) mod m_2026_04_01 {
        mod settings {
            use anyhow::Result;
            use serde_json::Value;

            pub fn restructure_profiles_with_settings_key(value: &mut Value) -> Result<()> {
                let Some(root_object) = value.as_object_mut() else {
                    return Ok(());
                };

                let Some(profiles) = root_object.get_mut("profiles") else {
                    return Ok(());
                };

                let Some(profiles_map) = profiles.as_object_mut() else {
                    return Ok(());
                };

                for profile_value in profiles_map.values_mut() {
                    if profile_value
                        .as_object()
                        .is_some_and(|m| m.contains_key("settings") || m.contains_key("base"))
                    {
                        continue;
                    }

                    *profile_value = serde_json::json!({ "settings": profile_value });
                }

                Ok(())
            }
        }

        pub(crate) use settings::restructure_profiles_with_settings_key;
    }

    pub(crate) mod m_2026_04_17 {
        mod settings {
            use anyhow::Result;
            use serde_json::Value;

            use crate::migrations::migrate_settings;

            const SETTINGS_KEY: &str = "settings";
            const TITLE_BAR_KEY: &str = "title_bar";
            const OLD_KEY: &str = "show_branch_icon";
            const NEW_KEY: &str = "show_branch_status_icon";

            pub fn promote_show_branch_icon_true_to_show_branch_status_icon(
                value: &mut Value,
            ) -> Result<()> {
                migrate_settings(value, &mut migrate_one)
            }

            fn migrate_one(object: &mut serde_json::Map<String, Value>) -> Result<()> {
                migrate_title_bar_value(object);

                if let Some(settings) = object
                    .get_mut(SETTINGS_KEY)
                    .and_then(|value| value.as_object_mut())
                {
                    migrate_title_bar_value(settings);
                }

                Ok(())
            }

            fn migrate_title_bar_value(object: &mut serde_json::Map<String, Value>) {
                let Some(title_bar) = object
                    .get_mut(TITLE_BAR_KEY)
                    .and_then(|value| value.as_object_mut())
                else {
                    return;
                };

                let Some(old_value) = title_bar.remove(OLD_KEY) else {
                    return;
                };

                if title_bar.contains_key(NEW_KEY) {
                    return;
                }

                if old_value == Value::Bool(true) {
                    title_bar.insert(NEW_KEY.to_string(), Value::Bool(true));
                }
            }
        }

        pub(crate) use settings::promote_show_branch_icon_true_to_show_branch_status_icon;
    }

    pub(crate) mod m_2026_05_04 {
        mod settings {
            use std::ops::Range;
            use tree_sitter::{Query, QueryMatch};

            use crate::MigrationPatterns;
            use crate::patterns::SETTINGS_ROOT_KEY_VALUE_PATTERN;

            pub const SETTINGS_PATTERNS: MigrationPatterns =
                &[(SETTINGS_ROOT_KEY_VALUE_PATTERN, replace_setting_value)];

            fn replace_setting_value(
                contents: &str,
                mat: &QueryMatch,
                query: &Query,
            ) -> Option<(Range<usize>, String)> {
                let setting_capture_ix = query.capture_index_for_name("name")?;
                let setting_name_range = mat
                    .nodes_for_capture_index(setting_capture_ix)
                    .next()?
                    .byte_range();
                let setting_name = contents.get(setting_name_range)?;

                if setting_name != "hide_mouse" {
                    return None;
                }

                let value_capture_ix = query.capture_index_for_name("value")?;
                let value_range = mat
                    .nodes_for_capture_index(value_capture_ix)
                    .next()?
                    .byte_range();
                let value = contents.get(value_range.clone())?;

                if value.trim() != "\"on_typing_and_movement\"" {
                    return None;
                }

                Some((value_range, "\"on_typing_and_action\"".to_string()))
            }
        }

        pub(crate) use settings::SETTINGS_PATTERNS;
    }

    pub(crate) mod m_2026_08_17 {
        mod settings {
            use anyhow::Result;
            use serde_json::Value;

            use crate::migrations::migrate_settings;

            pub fn make_git_gutter_width_an_enum(value: &mut Value) -> Result<()> {
                migrate_settings(value, &mut migrate_one)
            }

            fn migrate_one(obj: &mut serde_json::Map<String, Value>) -> Result<()> {
                let Some(gutter) = obj
                    .get_mut("gutter")
                    .and_then(|gutter| gutter.as_object_mut())
                else {
                    return Ok(());
                };

                let Some(git_gutter_width) = gutter.get_mut("git_gutter_width") else {
                    return Ok(());
                };

                *git_gutter_width = match git_gutter_width {
                    Value::Number(n) => {
                        serde_json::json!({
                            "custom": n
                        })
                    }
                    _ => return Ok(()),
                };

                Ok(())
            }
        }

        pub(crate) use settings::make_git_gutter_width_an_enum;
    }

    pub(crate) mod m_2026_08_26 {
        mod settings {
            use anyhow::Result;
            use serde_json::Value;

            use crate::migrations::migrate_settings;

            const PANEL_KEYS: &[&str] = &["project_panel", "outline_panel", "git_panel"];
            const OLD_KEY: &str = "folder_icons";
            const NEW_KEY: &str = "folder_indicator";

            pub fn rename_folder_icons_to_folder_indicator(value: &mut Value) -> Result<()> {
                migrate_settings(value, &mut migrate_one)
            }

            fn migrate_one(object: &mut serde_json::Map<String, Value>) -> Result<()> {
                for panel_key in PANEL_KEYS {
                    let Some(panel) = object.get_mut(*panel_key).and_then(Value::as_object_mut)
                    else {
                        continue;
                    };

                    // Anything other than a boolean never deserialized as `folder_icons`, so leave
                    // it in place rather than guessing which indicator was meant.
                    let indicator = match panel.get(OLD_KEY) {
                        Some(Value::Bool(true)) => "icon",
                        Some(Value::Bool(false)) => "chevron",
                        _ => continue,
                    };

                    panel.remove(OLD_KEY);
                    panel
                        .entry(NEW_KEY)
                        .or_insert_with(|| Value::String(indicator.to_string()));
                }

                Ok(())
            }
        }

        pub(crate) use settings::rename_folder_icons_to_folder_indicator;
    }

    pub(crate) mod m_2026_08_30 {
        mod settings {
            use anyhow::Result;
            use serde_json::Value;

            use crate::migrations::migrate_settings;

            const MARKDOWN_PREVIEW_KEY: &str = "markdown_preview";
            const KEY_MAPPINGS: &[(&str, &str)] = &[
                ("markdown_preview_font_size", "font_size"),
                ("markdown_preview_font_family", "font_family"),
                ("markdown_preview_code_font_family", "code_font_family"),
                ("markdown_preview_theme", "theme"),
            ];

            pub fn nest_markdown_preview_settings(value: &mut Value) -> Result<()> {
                migrate_settings(value, &mut migrate_one)
            }

            fn migrate_one(object: &mut serde_json::Map<String, Value>) -> Result<()> {
                let has_legacy_settings = KEY_MAPPINGS
                    .iter()
                    .any(|(old_key, _)| object.contains_key(*old_key));
                match object.get(MARKDOWN_PREVIEW_KEY) {
                    Some(Value::Null) if has_legacy_settings => {
                        object.remove(MARKDOWN_PREVIEW_KEY);
                    }
                    Some(markdown_preview) if !markdown_preview.is_object() => return Ok(()),
                    _ => {}
                }

                let migrated_settings = KEY_MAPPINGS
                    .iter()
                    .filter_map(|(old_key, new_key)| {
                        object
                            .remove(*old_key)
                            .map(|value| ((*new_key).to_string(), value))
                    })
                    .collect::<Vec<_>>();

                if migrated_settings.is_empty() {
                    return Ok(());
                }

                let Some(markdown_preview) = object
                    .entry(MARKDOWN_PREVIEW_KEY)
                    .or_insert_with(|| Value::Object(serde_json::Map::new()))
                    .as_object_mut()
                else {
                    return Ok(());
                };

                for (key, value) in migrated_settings {
                    markdown_preview.entry(key).or_insert(value);
                }

                Ok(())
            }
        }

        pub(crate) use settings::nest_markdown_preview_settings;
    }

}