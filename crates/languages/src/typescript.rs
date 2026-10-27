use anyhow::{Context as _, Result};
use async_trait::async_trait;
use chrono::{DateTime, Local};
use collections::HashMap;
use futures::future::join_all;
use gpui::{App, AppContext, AsyncApp, Entity, Task};
use itertools::Itertools as _;
use language::{
    Buffer, ContextLocation, ContextProvider, File, LanguageName, LanguageToolchainStore,
    LspAdapter, LspAdapterDelegate, LspInstaller, Toolchain,
};
use lsp::{CodeActionKind, LanguageServerBinary, LanguageServerName, Uri};
use node_runtime::{NodeRuntime, VersionStrategy};
use project::{Fs, lsp_store::language_server_settings};
use semver::{Version, VersionReq};
use serde_json::{Value, json};
use smol::lock::RwLock;
use std::{
    borrow::Cow,
    ffi::OsString,
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};
use task::{TaskTemplate, TaskTemplates, VariableName};
use util::rel_path::RelPath;
use util::{ResultExt, maybe};

use crate::{PackageJson, PackageJsonData};

pub(crate) struct TypeScriptContextProvider {
    fs: Arc<dyn Fs>,
    last_package_json: PackageJsonContents,
}

const TYPESCRIPT_RUNNER_VARIABLE: VariableName =
    VariableName::Custom(Cow::Borrowed("TYPESCRIPT_RUNNER"));

const TYPESCRIPT_JEST_TEST_NAME_VARIABLE: VariableName =
    VariableName::Custom(Cow::Borrowed("TYPESCRIPT_JEST_TEST_NAME"));

const TYPESCRIPT_VITEST_TEST_NAME_VARIABLE: VariableName =
    VariableName::Custom(Cow::Borrowed("TYPESCRIPT_VITEST_TEST_NAME"));

const TYPESCRIPT_JEST_PACKAGE_PATH_VARIABLE: VariableName =
    VariableName::Custom(Cow::Borrowed("TYPESCRIPT_JEST_PACKAGE_PATH"));

const TYPESCRIPT_MOCHA_PACKAGE_PATH_VARIABLE: VariableName =
    VariableName::Custom(Cow::Borrowed("TYPESCRIPT_MOCHA_PACKAGE_PATH"));

const TYPESCRIPT_VITEST_PACKAGE_PATH_VARIABLE: VariableName =
    VariableName::Custom(Cow::Borrowed("TYPESCRIPT_VITEST_PACKAGE_PATH"));

const TYPESCRIPT_JASMINE_PACKAGE_PATH_VARIABLE: VariableName =
    VariableName::Custom(Cow::Borrowed("TYPESCRIPT_JASMINE_PACKAGE_PATH"));

const TYPESCRIPT_BUN_PACKAGE_PATH_VARIABLE: VariableName =
    VariableName::Custom(Cow::Borrowed("TYPESCRIPT_BUN_PACKAGE_PATH"));

const TYPESCRIPT_BUN_TEST_NAME_VARIABLE: VariableName =
    VariableName::Custom(Cow::Borrowed("TYPESCRIPT_BUN_TEST_NAME"));

const TYPESCRIPT_NODE_PACKAGE_PATH_VARIABLE: VariableName =
    VariableName::Custom(Cow::Borrowed("TYPESCRIPT_NODE_PACKAGE_PATH"));

#[derive(Clone, Debug, Default)]
struct PackageJsonContents(Arc<RwLock<HashMap<PathBuf, PackageJson>>>);

impl PackageJsonData {
    fn fill_task_templates(&self, task_templates: &mut TaskTemplates) {
        if self.jest_package_path.is_some() {
            task_templates.0.push(TaskTemplate {
                label: "jest file test".to_owned(),
                command: TYPESCRIPT_RUNNER_VARIABLE.template_value(),
                args: vec![
                    "exec".to_owned(),
                    "--".to_owned(),
                    "jest".to_owned(),
                    "--runInBand".to_owned(),
                    VariableName::File.template_value(),
                ],
                cwd: Some(TYPESCRIPT_JEST_PACKAGE_PATH_VARIABLE.template_value()),
                ..TaskTemplate::default()
            });
            task_templates.0.push(TaskTemplate {
                label: format!("jest test {}", VariableName::Symbol.template_value()),
                command: TYPESCRIPT_RUNNER_VARIABLE.template_value(),
                args: vec![
                    "exec".to_owned(),
                    "--".to_owned(),
                    "jest".to_owned(),
                    "--runInBand".to_owned(),
                    "--testNamePattern".to_owned(),
                    format!(
                        "\"{}\"",
                        TYPESCRIPT_JEST_TEST_NAME_VARIABLE.template_value()
                    ),
                    VariableName::File.template_value(),
                ],
                tags: vec![
                    "ts-test".to_owned(),
                    "js-test".to_owned(),
                    "tsx-test".to_owned(),
                ],
                cwd: Some(TYPESCRIPT_JEST_PACKAGE_PATH_VARIABLE.template_value()),
                ..TaskTemplate::default()
            });
        }

        if self.vitest_package_path.is_some() {
            task_templates.0.push(TaskTemplate {
                label: format!("{} file test", "vitest".to_owned()),
                command: TYPESCRIPT_RUNNER_VARIABLE.template_value(),
                args: vec![
                    "exec".to_owned(),
                    "--".to_owned(),
                    "vitest".to_owned(),
                    "run".to_owned(),
                    "--no-file-parallelism".to_owned(),
                    VariableName::File.template_value(),
                ],
                cwd: Some(TYPESCRIPT_VITEST_PACKAGE_PATH_VARIABLE.template_value()),
                ..TaskTemplate::default()
            });
            task_templates.0.push(TaskTemplate {
                label: format!(
                    "{} test {}",
                    "vitest".to_owned(),
                    VariableName::Symbol.template_value(),
                ),
                command: TYPESCRIPT_RUNNER_VARIABLE.template_value(),
                args: vec![
                    "exec".to_owned(),
                    "--".to_owned(),
                    "vitest".to_owned(),
                    "run".to_owned(),
                    "--no-file-parallelism".to_owned(),
                    "--testNamePattern".to_owned(),
                    format!(
                        "\"{}\"",
                        TYPESCRIPT_VITEST_TEST_NAME_VARIABLE.template_value()
                    ),
                    VariableName::File.template_value(),
                ],
                tags: vec![
                    "ts-test".to_owned(),
                    "js-test".to_owned(),
                    "tsx-test".to_owned(),
                ],
                cwd: Some(TYPESCRIPT_VITEST_PACKAGE_PATH_VARIABLE.template_value()),
                ..TaskTemplate::default()
            });
        }

        if self.mocha_package_path.is_some() {
            task_templates.0.push(TaskTemplate {
                label: format!("{} file test", "mocha".to_owned()),
                command: TYPESCRIPT_RUNNER_VARIABLE.template_value(),
                args: vec![
                    "exec".to_owned(),
                    "--".to_owned(),
                    "mocha".to_owned(),
                    VariableName::File.template_value(),
                ],
                cwd: Some(TYPESCRIPT_MOCHA_PACKAGE_PATH_VARIABLE.template_value()),
                ..TaskTemplate::default()
            });
            task_templates.0.push(TaskTemplate {
                label: format!(
                    "{} test {}",
                    "mocha".to_owned(),
                    VariableName::Symbol.template_value(),
                ),
                command: TYPESCRIPT_RUNNER_VARIABLE.template_value(),
                args: vec![
                    "exec".to_owned(),
                    "--".to_owned(),
                    "mocha".to_owned(),
                    "--grep".to_owned(),
                    format!("\"{}\"", VariableName::Symbol.template_value()),
                    VariableName::File.template_value(),
                ],
                tags: vec![
                    "ts-test".to_owned(),
                    "js-test".to_owned(),
                    "tsx-test".to_owned(),
                ],
                cwd: Some(TYPESCRIPT_MOCHA_PACKAGE_PATH_VARIABLE.template_value()),
                ..TaskTemplate::default()
            });
        }

        if self.jasmine_package_path.is_some() {
            task_templates.0.push(TaskTemplate {
                label: format!("{} file test", "jasmine".to_owned()),
                command: TYPESCRIPT_RUNNER_VARIABLE.template_value(),
                args: vec![
                    "exec".to_owned(),
                    "--".to_owned(),
                    "jasmine".to_owned(),
                    VariableName::File.template_value(),
                ],
                cwd: Some(TYPESCRIPT_JASMINE_PACKAGE_PATH_VARIABLE.template_value()),
                ..TaskTemplate::default()
            });
            task_templates.0.push(TaskTemplate {
                label: format!(
                    "{} test {}",
                    "jasmine".to_owned(),
                    VariableName::Symbol.template_value(),
                ),
                command: TYPESCRIPT_RUNNER_VARIABLE.template_value(),
                args: vec![
                    "exec".to_owned(),
                    "--".to_owned(),
                    "jasmine".to_owned(),
                    format!("--filter={}", VariableName::Symbol.template_value()),
                    VariableName::File.template_value(),
                ],
                tags: vec![
                    "ts-test".to_owned(),
                    "js-test".to_owned(),
                    "tsx-test".to_owned(),
                ],
                cwd: Some(TYPESCRIPT_JASMINE_PACKAGE_PATH_VARIABLE.template_value()),
                ..TaskTemplate::default()
            });
        }

        if self.bun_package_path.is_some() {
            task_templates.0.push(TaskTemplate {
                label: format!("{} file test", "bun test".to_owned()),
                command: "bun".to_owned(),
                args: vec!["test".to_owned(), VariableName::File.template_value()],
                cwd: Some(TYPESCRIPT_BUN_PACKAGE_PATH_VARIABLE.template_value()),
                ..TaskTemplate::default()
            });
            task_templates.0.push(TaskTemplate {
                label: format!("bun test {}", VariableName::Symbol.template_value(),),
                command: "bun".to_owned(),
                args: vec![
                    "test".to_owned(),
                    "--test-name-pattern".to_owned(),
                    format!("\"{}\"", TYPESCRIPT_BUN_TEST_NAME_VARIABLE.template_value()),
                    VariableName::File.template_value(),
                ],
                tags: vec![
                    "ts-test".to_owned(),
                    "js-test".to_owned(),
                    "tsx-test".to_owned(),
                ],
                cwd: Some(TYPESCRIPT_BUN_PACKAGE_PATH_VARIABLE.template_value()),
                ..TaskTemplate::default()
            });
        }

        if self.node_package_path.is_some() {
            task_templates.0.push(TaskTemplate {
                label: format!("{} file test", "node test".to_owned()),
                command: "node".to_owned(),
                args: vec!["--test".to_owned(), VariableName::File.template_value()],
                tags: vec![
                    "ts-test".to_owned(),
                    "js-test".to_owned(),
                    "tsx-test".to_owned(),
                ],
                cwd: Some(TYPESCRIPT_NODE_PACKAGE_PATH_VARIABLE.template_value()),
                ..TaskTemplate::default()
            });
            task_templates.0.push(TaskTemplate {
                label: format!("node test {}", VariableName::Symbol.template_value()),
                command: "node".to_owned(),
                args: vec![
                    "--test".to_owned(),
                    "--test-name-pattern".to_owned(),
                    format!("\"{}\"", VariableName::Symbol.template_value()),
                    VariableName::File.template_value(),
                ],
                tags: vec![
                    "ts-test".to_owned(),
                    "js-test".to_owned(),
                    "tsx-test".to_owned(),
                ],
                cwd: Some(TYPESCRIPT_NODE_PACKAGE_PATH_VARIABLE.template_value()),
                ..TaskTemplate::default()
            });
        }

        let script_name_counts: HashMap<_, usize> =
            self.scripts
                .iter()
                .fold(HashMap::default(), |mut acc, (_, script)| {
                    *acc.entry(script).or_default() += 1;
                    acc
                });
        for (path, script) in &self.scripts {
            let label = if script_name_counts.get(script).copied().unwrap_or_default() > 1
                && let Some(parent) = path.parent().and_then(|parent| parent.file_name())
            {
                let parent = parent.to_string_lossy();
                format!("{parent}/package.json > {script}")
            } else {
                format!("package.json > {script}")
            };
            task_templates.0.push(TaskTemplate {
                label,
                command: TYPESCRIPT_RUNNER_VARIABLE.template_value(),
                args: vec!["run".to_owned(), script.to_owned()],
                tags: vec!["package-script".into()],
                cwd: Some(
                    path.parent()
                        .unwrap_or(Path::new("/"))
                        .to_string_lossy()
                        .to_string(),
                ),
                ..TaskTemplate::default()
            });
        }
    }
}

impl TypeScriptContextProvider {
    pub fn new(fs: Arc<dyn Fs>) -> Self {
        Self {
            fs,
            last_package_json: PackageJsonContents::default(),
        }
    }

    fn combined_package_json_data(
        &self,
        fs: Arc<dyn Fs>,
        worktree_root: &Path,
        file_relative_path: &RelPath,
        cx: &App,
    ) -> Task<anyhow::Result<PackageJsonData>> {
        let new_json_data = file_relative_path
            .ancestors()
            .map(|path| worktree_root.join(path.as_std_path()))
            .map(|parent_path| {
                self.package_json_data(&parent_path, self.last_package_json.clone(), fs.clone(), cx)
            })
            .collect::<Vec<_>>();

        cx.background_spawn(async move {
            let mut package_json_data = PackageJsonData::default();
            for new_data in join_all(new_json_data).await.into_iter().flatten() {
                package_json_data.merge(new_data);
            }
            Ok(package_json_data)
        })
    }

    fn package_json_data(
        &self,
        directory_path: &Path,
        existing_package_json: PackageJsonContents,
        fs: Arc<dyn Fs>,
        cx: &App,
    ) -> Task<anyhow::Result<PackageJsonData>> {
        let package_json_path = directory_path.join("package.json");
        let metadata_check_fs = fs.clone();
        cx.background_spawn(async move {
            let metadata = metadata_check_fs
                .metadata(&package_json_path)
                .await
                .with_context(|| format!("getting metadata for {package_json_path:?}"))?
                .with_context(|| format!("missing FS metadata for {package_json_path:?}"))?;
            let mtime = DateTime::<Local>::from(metadata.mtime.timestamp_for_user());
            let existing_data = {
                let contents = existing_package_json.0.read().await;
                contents
                    .get(&package_json_path)
                    .filter(|package_json| package_json.mtime == mtime)
                    .map(|package_json| package_json.data.clone())
            };
            match existing_data {
                Some(existing_data) => Ok(existing_data),
                None => {
                    let package_json_string =
                        fs.load(&package_json_path).await.with_context(|| {
                            format!("loading package.json from {package_json_path:?}")
                        })?;
                    let package_json: HashMap<String, serde_json_lenient::Value> =
                        serde_json_lenient::from_str(&package_json_string).with_context(|| {
                            format!("parsing package.json from {package_json_path:?}")
                        })?;
                    let new_data =
                        PackageJsonData::new(package_json_path.as_path().into(), package_json);
                    {
                        let mut contents = existing_package_json.0.write().await;
                        contents.insert(
                            package_json_path,
                            PackageJson {
                                mtime,
                                data: new_data.clone(),
                            },
                        );
                    }
                    Ok(new_data)
                }
            }
        })
    }
}

async fn detect_package_manager(
    worktree_root: PathBuf,
    fs: Arc<dyn Fs>,
    package_json_data: Option<PackageJsonData>,
) -> &'static str {
    if let Some(package_json_data) = package_json_data
        && let Some(package_manager) = package_json_data.package_manager
    {
        return package_manager;
    }
    if fs.is_file(&worktree_root.join("pnpm-lock.yaml")).await {
        return "pnpm";
    }
    if fs.is_file(&worktree_root.join("yarn.lock")).await {
        return "yarn";
    }
    "npm"
}

impl ContextProvider for TypeScriptContextProvider {
    fn associated_tasks(
        &self,
        buffer: Option<Entity<Buffer>>,
        cx: &App,
    ) -> Task<Option<TaskTemplates>> {
        let file = buffer.and_then(|buffer| buffer.read(cx).file());
        let Some(file) = project::File::from_dyn(file).cloned() else {
            return Task::ready(None);
        };
        let Some(worktree_root) = file.worktree.read(cx).root_dir() else {
            return Task::ready(None);
        };
        let file_relative_path = file.path().clone();
        let package_json_data = self.combined_package_json_data(
            self.fs.clone(),
            &worktree_root,
            &file_relative_path,
            cx,
        );

        cx.background_spawn(async move {
            let mut task_templates = TaskTemplates(Vec::new());
            task_templates.0.push(TaskTemplate {
                label: format!(
                    "execute selection {}",
                    VariableName::SelectedText.template_value()
                ),
                command: "node".to_owned(),
                args: vec![
                    "-e".to_owned(),
                    format!("\"{}\"", VariableName::SelectedText.template_value()),
                ],
                ..TaskTemplate::default()
            });

            match package_json_data.await {
                Ok(package_json) => {
                    package_json.fill_task_templates(&mut task_templates);
                }
                Err(e) => {
                    log::error!(
                        "Failed to read package.json for worktree {file_relative_path:?}: {e:#}"
                    );
                }
            }

            Some(task_templates)
        })
    }

    fn build_context(
        &self,
        current_vars: &task::TaskVariables,
        location: ContextLocation<'_>,
        _project_env: Option<HashMap<String, String>>,
        _toolchains: Arc<dyn LanguageToolchainStore>,
        cx: &mut App,
    ) -> Task<Result<task::TaskVariables>> {
        let mut vars = task::TaskVariables::default();

        if let Some(symbol) = current_vars.get(&VariableName::Symbol) {
            vars.insert(
                TYPESCRIPT_JEST_TEST_NAME_VARIABLE,
                replace_test_name_parameters(symbol),
            );
            vars.insert(
                TYPESCRIPT_VITEST_TEST_NAME_VARIABLE,
                replace_test_name_parameters(symbol),
            );
            vars.insert(
                TYPESCRIPT_BUN_TEST_NAME_VARIABLE,
                replace_test_name_parameters(symbol),
            );
        }
        let file_path = location
            .file_location
            .buffer
            .read(cx)
            .file()
            .map(|file| file.path());

        let args = location.worktree_root.zip(location.fs).zip(file_path).map(
            |((worktree_root, fs), file_path)| {
                (
                    self.combined_package_json_data(fs.clone(), &worktree_root, file_path, cx),
                    worktree_root,
                    fs,
                )
            },
        );
        cx.background_spawn(async move {
            if let Some((task, worktree_root, fs)) = args {
                let package_json_data = task.await.log_err();
                vars.insert(
                    TYPESCRIPT_RUNNER_VARIABLE,
                    detect_package_manager(worktree_root, fs, package_json_data.clone())
                        .await
                        .to_owned(),
                );

                if let Some(package_json_data) = package_json_data {
                    if let Some(path) = package_json_data.jest_package_path {
                        vars.insert(
                            TYPESCRIPT_JEST_PACKAGE_PATH_VARIABLE,
                            path.parent()
                                .unwrap_or(Path::new(""))
                                .to_string_lossy()
                                .to_string(),
                        );
                    }

                    if let Some(path) = package_json_data.mocha_package_path {
                        vars.insert(
                            TYPESCRIPT_MOCHA_PACKAGE_PATH_VARIABLE,
                            path.parent()
                                .unwrap_or(Path::new(""))
                                .to_string_lossy()
                                .to_string(),
                        );
                    }

                    if let Some(path) = package_json_data.vitest_package_path {
                        vars.insert(
                            TYPESCRIPT_VITEST_PACKAGE_PATH_VARIABLE,
                            path.parent()
                                .unwrap_or(Path::new(""))
                                .to_string_lossy()
                                .to_string(),
                        );
                    }

                    if let Some(path) = package_json_data.jasmine_package_path {
                        vars.insert(
                            TYPESCRIPT_JASMINE_PACKAGE_PATH_VARIABLE,
                            path.parent()
                                .unwrap_or(Path::new(""))
                                .to_string_lossy()
                                .to_string(),
                        );
                    }

                    if let Some(path) = package_json_data.bun_package_path {
                        vars.insert(
                            TYPESCRIPT_BUN_PACKAGE_PATH_VARIABLE,
                            path.parent()
                                .unwrap_or(Path::new(""))
                                .to_string_lossy()
                                .to_string(),
                        );
                    }

                    if let Some(path) = package_json_data.node_package_path {
                        vars.insert(
                            TYPESCRIPT_NODE_PACKAGE_PATH_VARIABLE,
                            path.parent()
                                .unwrap_or(Path::new(""))
                                .to_string_lossy()
                                .to_string(),
                        );
                    }
                }
            }
            Ok(vars)
        })
    }
}

fn typescript_server_binary_arguments(server_path: &Path) -> Vec<OsString> {
    vec![server_path.into(), "--stdio".into()]
}

fn replace_test_name_parameters(test_name: &str) -> String {
    static PATTERN: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(\$([A-Za-z0-9_\.]+|[\#])|%[psdifjo#\$%])").unwrap());
    PATTERN.split(test_name).map(regex::escape).join("(.+?)")
}

static TYPESCRIPT_VERSION_REQ: LazyLock<VersionReq> =
    LazyLock::new(|| VersionReq::parse("^6").expect("Failed to parse TypeScript version req"));

pub struct TypeScriptLspAdapter {
    fs: Arc<dyn Fs>,
    node: NodeRuntime,
}

impl TypeScriptLspAdapter {
    const OLD_SERVER_PATH: &str = "node_modules/typescript-language-server/lib/cli.js";
    const NEW_SERVER_PATH: &str = "node_modules/typescript-language-server/lib/cli.mjs";

    const PACKAGE_NAME: &str = "typescript";
    const SERVER_PACKAGE_NAME: &str = "typescript-language-server";

    const SERVER_NAME: LanguageServerName =
        LanguageServerName::new_static(Self::SERVER_PACKAGE_NAME);

    pub fn new(node: NodeRuntime, fs: Arc<dyn Fs>) -> Self {
        TypeScriptLspAdapter { fs, node }
    }

    async fn tsdk_path(&self, adapter: &Arc<dyn LspAdapterDelegate>) -> Option<&'static str> {
        let is_yarn = adapter
            .read_text_file(
                RelPath::from_unix_str(".yarn/sdks/typescript/lib/typescript.js").unwrap(),
            )
            .await
            .is_ok();

        let tsdk_path = if is_yarn {
            ".yarn/sdks/typescript/lib"
        } else {
            "node_modules/typescript/lib"
        };

        // typescript-language-server doesn't support TypeScript 7+, which no longer
        // ships `tsserver.js`.
        if self
            .fs
            .is_file(
                &adapter
                    .worktree_root_path()
                    .join(tsdk_path)
                    .join("tsserver.js"),
            )
            .await
        {
            Some(tsdk_path)
        } else {
            None
        }
    }
}

pub struct TypeScriptVersions {
    typescript_version: Version,
    server_version: Version,
}

impl LspInstaller for TypeScriptLspAdapter {
    type BinaryVersion = TypeScriptVersions;

    async fn fetch_latest_server_version(
        &self,
        _: &Arc<dyn LspAdapterDelegate>,
        _: bool,
        _: &mut AsyncApp,
    ) -> Result<Self::BinaryVersion> {
        Ok(TypeScriptVersions {
            typescript_version: self
                .node
                .npm_package_latest_version_with_requirement(
                    Self::PACKAGE_NAME,
                    Some(&TYPESCRIPT_VERSION_REQ),
                )
                .await?,
            server_version: self
                .node
                .npm_package_latest_version(Self::SERVER_PACKAGE_NAME)
                .await?,
        })
    }

    fn check_if_version_installed(
        &self,
        version: &Self::BinaryVersion,
        container_dir: &PathBuf,
        _: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Option<LanguageServerBinary>> + use<> {
        let node = self.node.clone();
        let typescript_version = version.typescript_version.clone();
        let server_version = version.server_version.clone();
        let container_dir = container_dir.clone();

        async move {
            let server_path = container_dir.join(Self::NEW_SERVER_PATH);

            // Pin rather than Latest so an unusable TypeScript 7.x install gets downgraded.
            if node
                .should_install_npm_package(
                    Self::PACKAGE_NAME,
                    &server_path,
                    &container_dir,
                    VersionStrategy::Pin(&typescript_version),
                )
                .await
            {
                return None;
            }

            if node
                .should_install_npm_package(
                    Self::SERVER_PACKAGE_NAME,
                    &server_path,
                    &container_dir,
                    VersionStrategy::Latest(&server_version),
                )
                .await
            {
                return None;
            }

            Some(LanguageServerBinary {
                path: node.binary_path().await.ok()?,
                env: None,
                arguments: typescript_server_binary_arguments(&server_path),
            })
        }
    }

    fn fetch_server_binary(
        &self,
        latest_version: Self::BinaryVersion,
        container_dir: PathBuf,
        _: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        let node = self.node.clone();

        async move {
            let server_path = container_dir.join(Self::NEW_SERVER_PATH);
            let typescript_version = latest_version.typescript_version.to_string();

            node.npm_install_packages(
                &container_dir,
                &[
                    (Self::PACKAGE_NAME, typescript_version.as_str()),
                    (Self::SERVER_PACKAGE_NAME, "latest"),
                ],
            )
            .await?;

            Ok(LanguageServerBinary {
                path: node.binary_path().await?,
                env: None,
                arguments: typescript_server_binary_arguments(&server_path),
            })
        }
    }

    async fn cached_server_binary(
        &self,
        container_dir: PathBuf,
        _: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        get_cached_ts_server_binary(container_dir, &self.node).await
    }
}

#[async_trait(?Send)]
impl LspAdapter for TypeScriptLspAdapter {
    fn name(&self) -> LanguageServerName {
        Self::SERVER_NAME
    }

    fn code_action_kinds(&self) -> Option<Vec<CodeActionKind>> {
        Some(vec![
            CodeActionKind::QUICKFIX,
            CodeActionKind::REFACTOR,
            CodeActionKind::REFACTOR_EXTRACT,
            CodeActionKind::SOURCE,
        ])
    }

    async fn label_for_completion(
        &self,
        item: &lsp::CompletionItem,
        language: &Arc<language::Language>,
    ) -> Option<language::CodeLabel> {
        use lsp::CompletionItemKind as Kind;
        let label_len = item.label.len();
        let grammar = language.grammar()?;
        let highlight_id = match item.kind? {
            Kind::CLASS | Kind::INTERFACE | Kind::ENUM => grammar.highlight_id_for_name("type"),
            Kind::CONSTRUCTOR => grammar.highlight_id_for_name("type"),
            Kind::CONSTANT => grammar.highlight_id_for_name("constant"),
            Kind::FUNCTION | Kind::METHOD => grammar.highlight_id_for_name("function"),
            Kind::PROPERTY | Kind::FIELD => grammar.highlight_id_for_name("property"),
            Kind::VARIABLE => grammar.highlight_id_for_name("variable"),
            _ => None,
        }?;

        let text = if let Some(description) = item
            .label_details
            .as_ref()
            .and_then(|label_details| label_details.description.as_ref())
        {
            format!("{} {}", item.label, description)
        } else if let Some(detail) = &item.detail {
            format!("{} {}", item.label, detail)
        } else {
            item.label.clone()
        };
        Some(language::CodeLabel::filtered(
            text,
            label_len,
            item.filter_text.as_deref(),
            vec![(0..label_len, highlight_id)],
        ))
    }

    async fn initialization_options(
        self: Arc<Self>,
        adapter: &Arc<dyn LspAdapterDelegate>,
        _: &mut AsyncApp,
    ) -> Result<Option<serde_json::Value>> {
        let tsdk_path = self.tsdk_path(adapter).await;
        Ok(Some(json!({
            "provideFormatter": true,
            "hostInfo": "zed",
            "tsserver": {
                "path": tsdk_path,
            },
            "preferences": {
                "includeInlayParameterNameHints": "all",
                "includeInlayParameterNameHintsWhenArgumentMatchesName": true,
                "includeInlayFunctionParameterTypeHints": true,
                "includeInlayVariableTypeHints": true,
                "includeInlayVariableTypeHintsWhenTypeMatchesName": true,
                "includeInlayPropertyDeclarationTypeHints": true,
                "includeInlayFunctionLikeReturnTypeHints": true,
                "includeInlayEnumMemberValueHints": true,
            }
        })))
    }

    async fn workspace_configuration(
        self: Arc<Self>,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: Option<Uri>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let override_options = cx.update(|cx| {
            language_server_settings(delegate.as_ref(), &Self::SERVER_NAME, cx)
                .and_then(|s| s.settings.clone())
        });
        if let Some(options) = override_options {
            return Ok(options);
        }
        Ok(json!({
            "completions": {
              "completeFunctionCalls": true
            }
        }))
    }

    fn language_ids(&self) -> HashMap<LanguageName, String> {
        HashMap::from_iter([
            (LanguageName::new_static("TypeScript"), "typescript".into()),
            (LanguageName::new_static("JavaScript"), "javascript".into()),
            (LanguageName::new_static("TSX"), "typescriptreact".into()),
        ])
    }

    fn is_opt_in_for(&self, language: &LanguageName) -> bool {
        matches!(language.as_ref(), "TypeScript" | "JavaScript" | "TSX")
    }
}

async fn get_cached_ts_server_binary(
    container_dir: PathBuf,
    node: &NodeRuntime,
) -> Option<LanguageServerBinary> {
    maybe!(async {
        let old_server_path = container_dir.join(TypeScriptLspAdapter::OLD_SERVER_PATH);
        let new_server_path = container_dir.join(TypeScriptLspAdapter::NEW_SERVER_PATH);
        if new_server_path.exists() {
            Ok(LanguageServerBinary {
                path: node.binary_path().await?,
                env: None,
                arguments: typescript_server_binary_arguments(&new_server_path),
            })
        } else if old_server_path.exists() {
            Ok(LanguageServerBinary {
                path: node.binary_path().await?,
                env: None,
                arguments: typescript_server_binary_arguments(&old_server_path),
            })
        } else {
            anyhow::bail!("missing executable in directory {container_dir:?}")
        }
    })
    .await
    .log_err()
}

