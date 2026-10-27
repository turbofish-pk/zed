use anyhow::{Context as _, Result};
use async_trait::async_trait;
use collections::HashMap;
use futures::StreamExt;
use gpui::{App, AsyncApp, Entity, Task};
use http_client::github::latest_github_release;
pub use language::*;
use language::{
    LanguageName, LanguageToolchainStore, LspAdapterDelegate, LspInstaller,
    language_settings::LanguageSettings,
};
use lsp::{LanguageServerBinary, LanguageServerName};

use project::lsp_store::language_server_settings;
use regex::Regex;
use serde_json::{Value, json};
use settings::SemanticTokenRules;
use smol::fs;
use std::{
    borrow::Cow,
    ffi::{OsStr, OsString},
    future::Future,
    ops::Range,
    path::{Path, PathBuf},
    process::Output,
    str,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering::SeqCst},
    },
};
use task::{TaskTemplate, TaskTemplates, TaskVariables, VariableName};
use util::{
    ResultExt,
    fs::remove_matching,
    maybe, merge_json_value_into,
    paths::{PathStyle, UrlExt as _},
};

pub(crate) fn semantic_token_rules() -> SemanticTokenRules {
    let content = grammars::get_file("go/semantic_token_rules.json")
        .expect("missing go/semantic_token_rules.json");
    let json = std::str::from_utf8(&content.data).expect("invalid utf-8 in semantic_token_rules");
    settings::parse_json_with_comments::<SemanticTokenRules>(json)
        .expect("failed to parse go semantic_token_rules.json")
}

fn server_binary_arguments() -> Vec<OsString> {
    vec!["-mode=stdio".into()]
}

#[derive(Copy, Clone)]
pub struct GoLspAdapter;

impl GoLspAdapter {
    const SERVER_NAME: LanguageServerName = LanguageServerName::new_static("gopls");
}

static VERSION_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+\.\d+\.\d+").expect("Failed to create VERSION_REGEX"));

static GO_ESCAPE_SUBTEST_NAME_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"[.*+?^${}()|\[\]\\"']"#).expect("Failed to create GO_ESCAPE_SUBTEST_NAME_REGEX")
});

const BINARY: &str = if cfg!(target_os = "windows") {
    "gopls.exe"
} else {
    "gopls"
};

impl LspInstaller for GoLspAdapter {
    type BinaryVersion = Option<String>;

    async fn fetch_latest_server_version(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: bool,
        cx: &mut AsyncApp,
    ) -> Result<Option<String>> {
        static DID_SHOW_NOTIFICATION: AtomicBool = AtomicBool::new(false);

        const NOTIFICATION_MESSAGE: &str =
            "Could not install the Go language server `gopls`, because `go` was not found.";

        if delegate.which("go".as_ref()).await.is_none() {
            if DID_SHOW_NOTIFICATION
                .compare_exchange(false, true, SeqCst, SeqCst)
                .is_ok()
            {
                cx.update(|cx| {
                    delegate.show_notification(NOTIFICATION_MESSAGE, cx);
                });
            }
            anyhow::bail!(
                "Could not install the Go language server `gopls`, because `go` was not found."
            );
        }

        let release =
            latest_github_release("golang/tools", false, false, delegate.http_client()).await?;
        let version: Option<String> = release.tag_name.strip_prefix("gopls/v").map(str::to_string);
        if version.is_none() {
            log::warn!(
                "couldn't infer gopls version from GitHub release tag name '{}'",
                release.tag_name
            );
        }
        Ok(version)
    }

    async fn check_if_user_installed(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: &AsyncApp,
    ) -> Option<LanguageServerBinary> {
        let path = delegate.which(Self::SERVER_NAME.as_ref()).await?;
        Some(LanguageServerBinary {
            path,
            arguments: server_binary_arguments(),
            env: None,
        })
    }

    fn fetch_server_binary(
        &self,
        version: Option<String>,
        container_dir: PathBuf,
        delegate: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        let delegate = delegate.clone();

        async move {
            let go = delegate.which("go".as_ref()).await.unwrap_or("go".into());
            let go_version_output = util::command::new_command(&go)
                .args(["version"])
                .output()
                .await
                .context("failed to get go version via `go version` command`")?;
            let go_version = parse_version_output(&go_version_output)?;

            if let Some(version) = version {
                let binary_path = container_dir.join(format!("gopls_{version}_go_{go_version}"));
                if let Ok(metadata) = fs::metadata(&binary_path).await
                    && metadata.is_file()
                {
                    remove_matching(&container_dir, |entry| {
                        entry != binary_path && entry.file_name() != Some(OsStr::new("gobin"))
                    })
                    .await;

                    return Ok(LanguageServerBinary {
                        path: binary_path.to_path_buf(),
                        arguments: server_binary_arguments(),
                        env: None,
                    });
                }
            } else if let Some(path) = get_cached_server_binary(&container_dir).await {
                return Ok(path);
            }

            let gobin_dir = container_dir.join("gobin");
            fs::create_dir_all(&gobin_dir).await?;
            let install_output = util::command::new_command(go)
                .env("GO111MODULE", "on")
                .env("GOBIN", &gobin_dir)
                .args(["install", "golang.org/x/tools/gopls@latest"])
                .output()
                .await?;

            if !install_output.status.success() {
                log::error!(
                    "failed to install gopls via `go install`. stdout: {:?}, stderr: {:?}",
                    String::from_utf8_lossy(&install_output.stdout),
                    String::from_utf8_lossy(&install_output.stderr)
                );
                anyhow::bail!(
                    "failed to install gopls with `go install`. Is `go` installed and in the PATH? Check logs for more information."
                );
            }

            let installed_binary_path = gobin_dir.join(BINARY);
            let version_output = util::command::new_command(&installed_binary_path)
                .arg("version")
                .output()
                .await
                .context("failed to run installed gopls binary")?;
            let gopls_version = parse_version_output(&version_output)?;
            let binary_path = container_dir.join(format!("gopls_{gopls_version}_go_{go_version}"));
            fs::rename(&installed_binary_path, &binary_path).await?;

            Ok(LanguageServerBinary {
                path: binary_path.to_path_buf(),
                arguments: server_binary_arguments(),
                env: None,
            })
        }
    }

    async fn cached_server_binary(
        &self,
        container_dir: PathBuf,
        _: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        get_cached_server_binary(&container_dir).await
    }
}

#[async_trait(?Send)]
impl LspAdapter for GoLspAdapter {
    fn name(&self) -> LanguageServerName {
        Self::SERVER_NAME
    }

    async fn initialization_options(
        self: Arc<Self>,
        delegate: &Arc<dyn LspAdapterDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<Option<serde_json::Value>> {
        let semantic_tokens_enabled = cx.update(|cx| {
            LanguageSettings::resolve(None, Some(&LanguageName::new("Go")), cx)
                .semantic_tokens
                .enabled()
        });

        let mut default_config = json!({
            "usePlaceholders": false,
            "hints": {
                "assignVariableTypes": true,
                "compositeLiteralFields": true,
                "compositeLiteralTypes": true,
                "constantValues": true,
                "functionTypeParameters": true,
                "parameterNames": true,
                "rangeVariableTypes": true
            },
            "codelenses": {
                "test": true
            },
            "semanticTokens": semantic_tokens_enabled
        });

        let project_initialization_options = cx.update(|cx| {
            language_server_settings(delegate.as_ref(), &self.name(), cx)
                .and_then(|s| s.initialization_options.clone())
        });

        if let Some(override_options) = project_initialization_options {
            merge_json_value_into(override_options, &mut default_config);
        }

        Ok(Some(default_config))
    }

    async fn workspace_configuration(
        self: Arc<Self>,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: Option<lsp::Uri>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        Ok(cx
            .update(|cx| {
                language_server_settings(delegate.as_ref(), &self.name(), cx)
                    .and_then(|settings| settings.settings.clone())
            })
            .unwrap_or_default())
    }

    async fn label_for_completion(
        &self,
        completion: &lsp::CompletionItem,
        language: &Arc<Language>,
    ) -> Option<CodeLabel> {
        let label = &completion.label;

        // Gopls returns nested fields and methods as completions.
        // To syntax highlight these, combine their final component
        // with their detail.
        let name_offset = label.rfind('.').unwrap_or(0);

        match completion.kind.zip(completion.detail.as_ref()) {
            Some((lsp::CompletionItemKind::MODULE, detail)) => {
                let text = format!("{label} {detail}");
                let source = format!("import {text}");
                let runs = language
                    .highlight_text(source.as_str(), 7..7 + text[name_offset..].len())
                    .collect();
                let filter_range = completion
                    .filter_text
                    .as_deref()
                    .and_then(|filter_text| {
                        text.find(filter_text)
                            .map(|start| start..start + filter_text.len())
                    })
                    .unwrap_or(0..label.len());
                return Some(CodeLabel::new(text, filter_range, runs));
            }
            Some((
                lsp::CompletionItemKind::CONSTANT | lsp::CompletionItemKind::VARIABLE,
                detail,
            )) => {
                let text = format!("{label} {detail}");
                let source = format!("var {} {}", &text[name_offset..], detail);
                let runs = adjust_runs(
                    name_offset,
                    language.highlight_text(source.as_str(), 4..4 + text[name_offset..].len()),
                );
                let filter_range = completion
                    .filter_text
                    .as_deref()
                    .and_then(|filter_text| {
                        text.find(filter_text)
                            .map(|start| start..start + filter_text.len())
                    })
                    .unwrap_or(0..label.len());
                return Some(CodeLabel::new(text, filter_range, runs));
            }
            Some((lsp::CompletionItemKind::STRUCT, _)) => {
                let text = format!("{label} struct {{}}");
                let source = format!("type {}", &text[name_offset..]);
                let runs = adjust_runs(
                    name_offset,
                    language.highlight_text(source.as_str(), 5..5 + text[name_offset..].len()),
                );
                let filter_range = completion
                    .filter_text
                    .as_deref()
                    .and_then(|filter_text| {
                        text.find(filter_text)
                            .map(|start| start..start + filter_text.len())
                    })
                    .unwrap_or(0..label.len());
                return Some(CodeLabel::new(text, filter_range, runs));
            }
            Some((lsp::CompletionItemKind::INTERFACE, _)) => {
                let text = format!("{label} interface {{}}");
                let source = format!("type {}", &text[name_offset..]);
                let runs = adjust_runs(
                    name_offset,
                    language.highlight_text(source.as_str(), 5..5 + text[name_offset..].len()),
                );
                let filter_range = completion
                    .filter_text
                    .as_deref()
                    .and_then(|filter_text| {
                        text.find(filter_text)
                            .map(|start| start..start + filter_text.len())
                    })
                    .unwrap_or(0..label.len());
                return Some(CodeLabel::new(text, filter_range, runs));
            }
            Some((lsp::CompletionItemKind::FIELD, detail)) => {
                let text = format!("{label} {detail}");
                let source = format!("type T struct {{ {} }}", &text[name_offset..]);
                let runs = adjust_runs(
                    name_offset,
                    language.highlight_text(source.as_str(), 16..16 + text[name_offset..].len()),
                );
                let filter_range = completion
                    .filter_text
                    .as_deref()
                    .and_then(|filter_text| {
                        text.find(filter_text)
                            .map(|start| start..start + filter_text.len())
                    })
                    .unwrap_or(0..label.len());
                return Some(CodeLabel::new(text, filter_range, runs));
            }
            Some((lsp::CompletionItemKind::FUNCTION | lsp::CompletionItemKind::METHOD, detail)) => {
                if let Some(signature) = detail.strip_prefix("func") {
                    let text = format!("{label}{signature}");
                    let source = format!("func {} {{}}", &text[name_offset..]);
                    let runs = adjust_runs(
                        name_offset,
                        language.highlight_text(source.as_str(), 5..5 + text[name_offset..].len()),
                    );
                    let filter_range = completion
                        .filter_text
                        .as_deref()
                        .and_then(|filter_text| {
                            text.find(filter_text)
                                .map(|start| start..start + filter_text.len())
                        })
                        .unwrap_or(0..label.len());
                    return Some(CodeLabel::new(text, filter_range, runs));
                }
            }
            _ => {}
        }
        None
    }

    async fn label_for_symbol(
        &self,
        symbol: &language::Symbol,
        language: &Arc<Language>,
    ) -> Option<CodeLabel> {
        let name = &symbol.name;
        let (text, filter_range, display_range) = match symbol.kind {
            language::SymbolKind::Method | language::SymbolKind::Function => {
                let text = format!("func {} () {{}}", name);
                let filter_range = 5..5 + name.len();
                let display_range = 0..filter_range.end;
                (text, filter_range, display_range)
            }
            language::SymbolKind::Struct => {
                let text = format!("type {} struct {{}}", name);
                let filter_range = 5..5 + name.len();
                let display_range = 0..text.len();
                (text, filter_range, display_range)
            }
            language::SymbolKind::Interface => {
                let text = format!("type {} interface {{}}", name);
                let filter_range = 5..5 + name.len();
                let display_range = 0..text.len();
                (text, filter_range, display_range)
            }
            language::SymbolKind::Class => {
                let text = format!("type {} T", name);
                let filter_range = 5..5 + name.len();
                let display_range = 0..filter_range.end;
                (text, filter_range, display_range)
            }
            language::SymbolKind::Constant => {
                let text = format!("const {} = nil", name);
                let filter_range = 6..6 + name.len();
                let display_range = 0..filter_range.end;
                (text, filter_range, display_range)
            }
            language::SymbolKind::Variable => {
                let text = format!("var {} = nil", name);
                let filter_range = 4..4 + name.len();
                let display_range = 0..filter_range.end;
                (text, filter_range, display_range)
            }
            language::SymbolKind::Module => {
                let text = format!("package {}", name);
                let filter_range = 8..8 + name.len();
                let display_range = 0..filter_range.end;
                (text, filter_range, display_range)
            }
            _ => return None,
        };

        Some(CodeLabel::new(
            text[display_range.clone()].to_string(),
            filter_range,
            language
                .highlight_text(text.as_str(), display_range)
                .collect(),
        ))
    }

    fn client_command(
        &self,
        command_name: &str,
        arguments: &[serde_json::Value],
        path_style: PathStyle,
    ) -> Option<ClientCommand> {
        if let "gopls.run_tests" = command_name {
            let template = go_test_task_template(arguments.first()?, path_style)?;
            Some(ClientCommand::ScheduleTask(template))
        } else {
            None
        }
    }

    fn diagnostic_message_to_markdown(&self, message: &str) -> Option<String> {
        static REGEX: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?m)\n\s*").expect("Failed to create REGEX"));
        Some(REGEX.replace_all(message, "\n\n").to_string())
    }
}

fn json_string_array(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn go_test_task_template(
    arg: &serde_json::Value,
    path_style: PathStyle,
) -> Option<task::TaskTemplate> {
    let tests = json_string_array(arg, "Tests");
    let benchmarks = json_string_array(arg, "Benchmarks");
    if tests.is_empty() && benchmarks.is_empty() {
        return None;
    }

    let mut go_args = vec!["test".to_string(), "-test.fullpath=true".to_string()];

    if tests.is_empty() {
        go_args.push("-benchmem".to_string());
        go_args.push("-run=^$".to_string());
    } else {
        go_args.push("-timeout".to_string());
        go_args.push("30s".to_string());
        go_args.push("-run".to_string());
        if tests.len() == 1 {
            go_args.push(format!("^{}$", tests[0]));
        } else {
            go_args.push(format!("^({})$", tests.join("|")));
        }
    }

    if !benchmarks.is_empty() {
        go_args.push("-bench".to_string());
        if benchmarks.len() == 1 {
            go_args.push(format!("^{}$", benchmarks[0]));
        } else {
            go_args.push(format!("^({})$", benchmarks.join("|")));
        }
    }

    go_args.push(".".to_string());

    let label = if !tests.is_empty() {
        format!("go test {}", tests.join(", "))
    } else {
        format!("go bench {}", benchmarks.join(", "))
    };

    let cwd = arg
        .get("URI")
        .and_then(|v| v.as_str())
        .and_then(|uri| uri.parse::<lsp::Uri>().ok())
        .and_then(|uri| uri.to_file_path_ext(path_style).ok())
        .and_then(|path| path_style.parent(&path).map(|parent| parent.to_owned()))
        .map(|path| path.to_string_lossy().into_owned());

    Some(task::TaskTemplate {
        label,
        command: "go".to_string(),
        args: go_args,
        cwd,
        ..task::TaskTemplate::default()
    })
}

fn parse_version_output(output: &Output) -> Result<&str> {
    let version_stdout =
        str::from_utf8(&output.stdout).context("version command produced invalid utf8 output")?;

    let version = VERSION_REGEX
        .find(version_stdout)
        .with_context(|| format!("failed to parse version output '{version_stdout}'"))?
        .as_str();

    Ok(version)
}

async fn get_cached_server_binary(container_dir: &Path) -> Option<LanguageServerBinary> {
    maybe!(async {
        let mut last_binary_path = None;
        let mut entries = fs::read_dir(container_dir).await?;
        while let Some(entry) = entries.next().await {
            let entry = entry?;
            if entry.file_type().await?.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("gopls_"))
            {
                last_binary_path = Some(entry.path());
            }
        }

        let path = last_binary_path.context("no cached binary")?;
        anyhow::Ok(LanguageServerBinary {
            path,
            arguments: server_binary_arguments(),
            env: None,
        })
    })
    .await
    .log_err()
}

fn adjust_runs(
    delta: usize,
    runs: impl Iterator<Item = (Range<usize>, HighlightId)>,
) -> Vec<(Range<usize>, HighlightId)> {
    runs.map(|(range, highlight_id)| (range.start + delta..range.end + delta, highlight_id))
        .collect()
}

pub(crate) struct GoContextProvider;

pub(crate) struct GoRunnableResolver;

impl RunnableResolver for GoRunnableResolver {
    fn resolve(
        &self,
        local_captures: &[RunnableMatchCapture],
        shared_captures: &[RunnableMatchCapture],
        buffer: &BufferSnapshot,
    ) -> Option<ResolvedRunnable> {
        const FIELD_CHECK: &str = "_field_check";
        const FIELD_NAME: &str = "_field_name";
        const TABLE_TEST_CASE_NAME: &str = "_table_test_case_name";

        // A row may declare several string fields (e.g. `{ name: "x", label: "y" }`), so
        // the query emits one `@_field_name` + `@run` pair per field, in source order.
        //
        // When the loop body calls `t.Run(tc.<field>, ...)`, `@_field_check` names that
        // field; pick the pair whose `@_field_name` matches it. Without a `@_field_check`
        // (e.g. map-keyed tables) the first pair wins.
        let reference_text = shared_captures
            .iter()
            .find(|capture| capture.name() == Some(FIELD_CHECK))
            .map(|capture| buffer.text_for_range(capture.range()).collect::<String>());
        let pair_index = match &reference_text {
            Some(reference) => local_captures
                .iter()
                .filter(|capture| capture.name() == Some(FIELD_NAME))
                .position(|capture| buffer.text_for_range(capture.range()).equals_str(reference))?,
            None => 0,
        };

        // `@run` and `@_table_test_case_name` tag the same string literal, so the chosen
        // run's text is the case name.
        let run_capture = local_captures
            .iter()
            .filter(|capture| capture.is_run())
            .nth(pair_index)?;
        Some(ResolvedRunnable {
            run_range: run_capture.range(),
            extra_captures: smallvec::smallvec![(
                TABLE_TEST_CASE_NAME.to_string(),
                buffer.text_for_range(run_capture.range()).collect(),
            )],
        })
    }
}

const GO_PACKAGE_TASK_VARIABLE: VariableName = VariableName::Custom(Cow::Borrowed("GO_PACKAGE"));
const GO_MODULE_ROOT_TASK_VARIABLE: VariableName =
    VariableName::Custom(Cow::Borrowed("GO_MODULE_ROOT"));
const GO_SUBTEST_NAME_TASK_VARIABLE: VariableName =
    VariableName::Custom(Cow::Borrowed("GO_SUBTEST_NAME"));
const GO_TABLE_TEST_CASE_NAME_TASK_VARIABLE: VariableName =
    VariableName::Custom(Cow::Borrowed("GO_TABLE_TEST_CASE_NAME"));
const GO_SUITE_NAME_TASK_VARIABLE: VariableName =
    VariableName::Custom(Cow::Borrowed("GO_SUITE_NAME"));

impl ContextProvider for GoContextProvider {
    fn build_context(
        &self,
        variables: &TaskVariables,
        location: ContextLocation<'_>,
        _: Option<HashMap<String, String>>,
        _: Arc<dyn LanguageToolchainStore>,
        cx: &mut gpui::App,
    ) -> Task<Result<TaskVariables>> {
        let local_abs_path = location
            .file_location
            .buffer
            .read(cx)
            .file()
            .and_then(|file| Some(file.as_local()?.abs_path(cx)));

        let go_package_variable = local_abs_path
            .as_deref()
            .and_then(|local_abs_path| local_abs_path.parent())
            .map(|buffer_dir| {
                // Prefer the relative form `./my-nested-package/is-here` over
                // absolute path, because it's more readable in the modal, but
                // the absolute path also works.
                let package_name = variables
                    .get(&VariableName::WorktreeRoot)
                    .and_then(|worktree_abs_path| buffer_dir.strip_prefix(worktree_abs_path).ok())
                    .map(|relative_pkg_dir| {
                        if relative_pkg_dir.as_os_str().is_empty() {
                            ".".into()
                        } else {
                            format!("./{}", relative_pkg_dir.to_string_lossy())
                        }
                    })
                    .unwrap_or_else(|| format!("{}", buffer_dir.to_string_lossy()));

                (GO_PACKAGE_TASK_VARIABLE.clone(), package_name)
            });

        let go_module_root_variable = local_abs_path
            .as_deref()
            .and_then(|local_abs_path| local_abs_path.parent())
            .map(|buffer_dir| {
                // Walk dirtree up until getting the first go.mod file
                let module_dir = buffer_dir
                    .ancestors()
                    .find(|dir| dir.join("go.mod").is_file())
                    .map(|dir| dir.to_string_lossy().into_owned())
                    .unwrap_or_else(|| ".".to_string());

                (GO_MODULE_ROOT_TASK_VARIABLE.clone(), module_dir)
            });

        let _subtest_name = variables.get(&VariableName::Custom(Cow::Borrowed("_subtest_name")));

        let go_subtest_variable = extract_subtest_name(_subtest_name.unwrap_or(""))
            .map(|subtest_name| (GO_SUBTEST_NAME_TASK_VARIABLE.clone(), subtest_name));

        let _table_test_case_name = variables.get(&VariableName::Custom(Cow::Borrowed(
            "_table_test_case_name",
        )));

        let go_table_test_case_variable = _table_test_case_name
            .and_then(extract_subtest_name)
            .map(|case_name| (GO_TABLE_TEST_CASE_NAME_TASK_VARIABLE.clone(), case_name));

        let _suite_name = variables.get(&VariableName::Custom(Cow::Borrowed("_suite_name")));

        let go_suite_variable = _suite_name
            .and_then(extract_subtest_name)
            .map(|suite_name| (GO_SUITE_NAME_TASK_VARIABLE.clone(), suite_name));

        Task::ready(Ok(TaskVariables::from_iter(
            [
                go_package_variable,
                go_subtest_variable,
                go_table_test_case_variable,
                go_suite_variable,
                go_module_root_variable,
            ]
            .into_iter()
            .flatten(),
        )))
    }

    fn associated_tasks(&self, _: Option<Entity<Buffer>>, _: &App) -> Task<Option<TaskTemplates>> {
        let package_cwd = if GO_PACKAGE_TASK_VARIABLE.template_value() == "." {
            None
        } else {
            Some("$ZED_DIRNAME".to_string())
        };
        let module_cwd = Some(GO_MODULE_ROOT_TASK_VARIABLE.template_value());

        Task::ready(Some(TaskTemplates(vec![
            TaskTemplate {
                label: format!(
                    "go test {} -v -run Test{}/{}",
                    GO_PACKAGE_TASK_VARIABLE.template_value(),
                    GO_SUITE_NAME_TASK_VARIABLE.template_value(),
                    VariableName::Symbol.template_value(),
                ),
                command: "go".into(),
                args: vec![
                    "test".into(),
                    "-v".into(),
                    "-run".into(),
                    format!(
                        "\\^Test{}\\$/\\^{}\\$",
                        GO_SUITE_NAME_TASK_VARIABLE.template_value(),
                        VariableName::Symbol.template_value(),
                    ),
                ],
                cwd: package_cwd.clone(),
                tags: vec!["go-testify-suite".to_owned()],
                ..TaskTemplate::default()
            },
            TaskTemplate {
                label: format!(
                    "go test {} -v -run {}/{}",
                    GO_PACKAGE_TASK_VARIABLE.template_value(),
                    VariableName::Symbol.template_value(),
                    GO_TABLE_TEST_CASE_NAME_TASK_VARIABLE.template_value(),
                ),
                command: "go".into(),
                args: vec![
                    "test".into(),
                    "-v".into(),
                    "-run".into(),
                    format!(
                        "\\^{}\\$/\\^{}\\$",
                        VariableName::Symbol.template_value(),
                        GO_TABLE_TEST_CASE_NAME_TASK_VARIABLE.template_value(),
                    ),
                ],
                cwd: package_cwd.clone(),
                tags: vec![
                    "go-table-test-case".to_owned(),
                    "go-table-test-case-without-explicit-variable".to_owned(),
                ],
                ..TaskTemplate::default()
            },
            TaskTemplate {
                label: format!(
                    "go test {} -run {}",
                    GO_PACKAGE_TASK_VARIABLE.template_value(),
                    VariableName::Symbol.template_value(),
                ),
                command: "go".into(),
                args: vec![
                    "test".into(),
                    "-run".into(),
                    format!("\\^{}\\$", VariableName::Symbol.template_value(),),
                ],
                tags: vec!["go-test".to_owned()],
                cwd: package_cwd.clone(),
                ..TaskTemplate::default()
            },
            TaskTemplate {
                label: format!(
                    "go test {} -run {}",
                    GO_PACKAGE_TASK_VARIABLE.template_value(),
                    VariableName::Symbol.template_value(),
                ),
                command: "go".into(),
                args: vec![
                    "test".into(),
                    "-run".into(),
                    format!("\\^{}\\$", VariableName::Symbol.template_value(),),
                ],
                tags: vec!["go-example".to_owned()],
                cwd: package_cwd.clone(),
                ..TaskTemplate::default()
            },
            TaskTemplate {
                label: format!("go test {}", GO_PACKAGE_TASK_VARIABLE.template_value()),
                command: "go".into(),
                args: vec!["test".into()],
                cwd: package_cwd.clone(),
                ..TaskTemplate::default()
            },
            TaskTemplate {
                label: format!("go test {}", GO_PACKAGE_TASK_VARIABLE.template_value()),
                command: "go".into(),
                args: vec!["test".into()],
                cwd: package_cwd.clone(),
                tags: vec!["go-package-test".to_owned()],
                ..TaskTemplate::default()
            },
            TaskTemplate {
                label: "go test ./...".into(),
                command: "go".into(),
                args: vec!["test".into(), "./...".into()],
                cwd: module_cwd.clone(),
                ..TaskTemplate::default()
            },
            TaskTemplate {
                label: format!(
                    "go test {} -v -run {}/{}",
                    GO_PACKAGE_TASK_VARIABLE.template_value(),
                    VariableName::Symbol.template_value(),
                    GO_SUBTEST_NAME_TASK_VARIABLE.template_value(),
                ),
                command: "go".into(),
                args: vec![
                    "test".into(),
                    "-v".into(),
                    "-run".into(),
                    format!(
                        "\\^{}\\$/\\^{}\\$",
                        VariableName::Symbol.template_value(),
                        GO_SUBTEST_NAME_TASK_VARIABLE.template_value(),
                    ),
                ],
                cwd: package_cwd.clone(),
                tags: vec!["go-subtest".to_owned()],
                ..TaskTemplate::default()
            },
            TaskTemplate {
                label: format!(
                    "go test {} -bench {}",
                    GO_PACKAGE_TASK_VARIABLE.template_value(),
                    VariableName::Symbol.template_value()
                ),
                command: "go".into(),
                args: vec![
                    "test".into(),
                    "-benchmem".into(),
                    "-run='^$'".into(),
                    "-bench".into(),
                    format!("\\^{}\\$", VariableName::Symbol.template_value()),
                ],
                cwd: package_cwd.clone(),
                tags: vec!["go-benchmark".to_owned()],
                ..TaskTemplate::default()
            },
            TaskTemplate {
                label: format!(
                    "go test {} -fuzz=Fuzz -run {}",
                    GO_PACKAGE_TASK_VARIABLE.template_value(),
                    VariableName::Symbol.template_value(),
                ),
                command: "go".into(),
                args: vec![
                    "test".into(),
                    "-fuzz=Fuzz".into(),
                    "-run".into(),
                    format!("\\^{}\\$", VariableName::Symbol.template_value(),),
                ],
                tags: vec!["go-fuzz".to_owned()],
                cwd: package_cwd.clone(),
                ..TaskTemplate::default()
            },
            TaskTemplate {
                label: format!("go run {}", GO_PACKAGE_TASK_VARIABLE.template_value(),),
                command: "go".into(),
                args: vec!["run".into(), ".".into()],
                cwd: package_cwd.clone(),
                tags: vec!["go-main".to_owned()],
                ..TaskTemplate::default()
            },
            TaskTemplate {
                label: format!("go generate {}", GO_PACKAGE_TASK_VARIABLE.template_value()),
                command: "go".into(),
                args: vec!["generate".into()],
                cwd: package_cwd,
                tags: vec!["go-generate".to_owned()],
                ..TaskTemplate::default()
            },
            TaskTemplate {
                label: "go generate ./...".into(),
                command: "go".into(),
                args: vec!["generate".into(), "./...".into()],
                cwd: module_cwd,
                ..TaskTemplate::default()
            },
        ])))
    }

    fn runnable_resolver(&self) -> Option<Arc<dyn RunnableResolver>> {
        Some(Arc::new(GoRunnableResolver))
    }
}

fn extract_subtest_name(input: &str) -> Option<String> {
    let content = if input.starts_with('`') && input.ends_with('`') {
        input.trim_matches('`')
    } else {
        input.trim_matches('"')
    };

    let processed = content
        .chars()
        .map(|c| if c.is_whitespace() { '_' } else { c })
        .collect::<String>();

    Some(
        GO_ESCAPE_SUBTEST_NAME_REGEX
            .replace_all(&processed, |caps: &regex::Captures| {
                format!("\\{}", &caps[0])
            })
            .to_string(),
    )
}

