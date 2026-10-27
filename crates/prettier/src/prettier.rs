use anyhow::Context as _;
use collections::{HashMap, HashSet};
use fs::Fs;
use gpui::{AsyncApp, Entity};
use language::language_settings::{LanguageSettings, PrettierSettings};
use language::{Buffer, Diff, Language, OffsetUtf16};
use lsp::{LanguageServer, LanguageServerId};
use node_runtime::NodeRuntime;
use paths::default_prettier_dir;
use serde::{Deserialize, Serialize};
use std::{
    ops::{ControlFlow, Range},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use util::{
    paths::{PathMatcher, PathStyle},
    rel_path::RelPath,
};

#[derive(Debug, Clone)]
pub enum Prettier {
    Real(RealPrettier),
    #[cfg(feature = "test-support")]
    Test(TestPrettier),
}

#[derive(Debug, Clone)]
pub struct RealPrettier {
    default: bool,
    prettier_dir: PathBuf,
    server: Arc<LanguageServer>,
}

#[cfg(feature = "test-support")]
#[derive(Debug, Clone)]
pub struct TestPrettier {
    prettier_dir: PathBuf,
    default: bool,
}

pub const FAIL_THRESHOLD: usize = 4;
pub const PRETTIER_SERVER_FILE: &str = "prettier_server.js";
pub const PRETTIER_SERVER_JS: &str = include_str!("./prettier_server.js");
const PRETTIER_PACKAGE_NAME: &str = "prettier";
const TAILWIND_PRETTIER_PLUGIN_PACKAGE_NAME: &str = "prettier-plugin-tailwindcss";

#[cfg(feature = "test-support")]
pub const FORMAT_SUFFIX: &str = "\nformatted by test prettier";
#[cfg(feature = "test-support")]
pub const RANGE_FORMAT_SUFFIX: &str = "\nrange formatted by test prettier";

impl Prettier {
    pub const CONFIG_FILE_NAMES: &'static [&'static str] = &[
        ".prettierrc",
        ".prettierrc.json",
        ".prettierrc.json5",
        ".prettierrc.yaml",
        ".prettierrc.yml",
        ".prettierrc.toml",
        ".prettierrc.js",
        ".prettierrc.cjs",
        ".prettierrc.mjs",
        ".prettierrc.ts",
        ".prettierrc.cts",
        ".prettierrc.mts",
        "package.json",
        "prettier.config.js",
        "prettier.config.cjs",
        "prettier.config.mjs",
        "prettier.config.ts",
        "prettier.config.cts",
        "prettier.config.mts",
        ".editorconfig",
        ".prettierignore",
    ];

    pub async fn locate_prettier_installation(
        fs: &dyn Fs,
        installed_prettiers: &HashSet<PathBuf>,
        locate_from: &Path,
    ) -> anyhow::Result<ControlFlow<(), Option<PathBuf>>> {
        let mut path_to_check = locate_from
            .components()
            .take_while(|component| component.as_os_str().to_string_lossy() != "node_modules")
            .collect::<PathBuf>();
        if path_to_check != locate_from {
            log::debug!(
                "Skipping prettier location for path {path_to_check:?} that is inside node_modules"
            );
            return Ok(ControlFlow::Break(()));
        }
        let path_to_check_metadata = fs
            .metadata(&path_to_check)
            .await
            .with_context(|| format!("failed to get metadata for initial path {path_to_check:?}"))?
            .with_context(|| format!("empty metadata for initial path {path_to_check:?}"))?;
        if !path_to_check_metadata.is_dir {
            path_to_check.pop();
        }

        let mut closest_package_json_path = None;
        loop {
            if installed_prettiers.contains(&path_to_check) {
                log::debug!("Found prettier path {path_to_check:?} in installed prettiers");
                return Ok(ControlFlow::Continue(Some(path_to_check)));
            } else if let Some(package_json_contents) =
                read_package_json(fs, &path_to_check).await?
            {
                if has_prettier_in_node_modules(fs, &path_to_check).await? {
                    log::debug!("Found prettier path {path_to_check:?} in the node_modules");
                    return Ok(ControlFlow::Continue(Some(path_to_check)));
                } else {
                    match &closest_package_json_path {
                        None => closest_package_json_path = Some(path_to_check.clone()),
                        Some(closest_package_json_path) => {
                            match package_json_contents.get("workspaces") {
                                Some(serde_json::Value::Array(workspaces)) => {
                                    let subproject_path = closest_package_json_path.strip_prefix(&path_to_check).expect("traversing path parents, should be able to strip prefix");
                                    if workspaces.iter().filter_map(|value| {
                                        if let serde_json::Value::String(s) = value {
                                            Some(s.clone())
                                        } else {
                                            log::warn!("Skipping non-string 'workspaces' value: {value:?}");
                                            None
                                        }
                                    }).any(|workspace_definition| {
                                        workspace_definition == subproject_path.to_string_lossy() || PathMatcher::new(&[workspace_definition], PathStyle::local()).ok().is_some_and(
                                            |path_matcher| RelPath::new(subproject_path, PathStyle::local()).is_ok_and(|path|  path_matcher.is_match(path)))
                                    }) {
                                        anyhow::ensure!(has_prettier_in_node_modules(fs, &path_to_check).await?,
                                            "Path {path_to_check:?} is the workspace root for project in \
                                            {closest_package_json_path:?}, but it has no prettier installed"
                                        );
                                        log::info!(
                                            "Found prettier path {path_to_check:?} in the workspace \
                                            root for project in {closest_package_json_path:?}"
                                        );
                                        return Ok(ControlFlow::Continue(Some(path_to_check)));
                                    } else {
                                        log::warn!(
                                            "Skipping path {path_to_check:?} workspace root with \
                                            workspaces {workspaces:?} that have no prettier installed"
                                        );
                                    }
                                }
                                Some(unknown) => log::error!(
                                    "Failed to parse workspaces for {path_to_check:?} from package.json, \
                                    got {unknown:?}. Skipping."
                                ),
                                None => log::warn!(
                                    "Skipping path {path_to_check:?} that has no prettier \
                                    dependency and no workspaces section in its package.json"
                                ),
                            }
                        }
                    }
                }
            }

            if !path_to_check.pop() {
                log::debug!("Found no prettier in ancestors of {locate_from:?}");
                return Ok(ControlFlow::Continue(None));
            }
        }
    }

    pub async fn locate_prettier_ignore(
        fs: &dyn Fs,
        prettier_ignores: &HashSet<PathBuf>,
        locate_from: &Path,
    ) -> anyhow::Result<ControlFlow<(), Option<PathBuf>>> {
        let mut path_to_check = locate_from
            .components()
            .take_while(|component| component.as_os_str().to_string_lossy() != "node_modules")
            .collect::<PathBuf>();
        if path_to_check != locate_from {
            log::debug!(
                "Skipping prettier ignore location for path {path_to_check:?} that is inside node_modules"
            );
            return Ok(ControlFlow::Break(()));
        }

        let path_to_check_metadata = fs
            .metadata(&path_to_check)
            .await
            .with_context(|| format!("failed to get metadata for initial path {path_to_check:?}"))?
            .with_context(|| format!("empty metadata for initial path {path_to_check:?}"))?;
        if !path_to_check_metadata.is_dir {
            path_to_check.pop();
        }

        let mut closest_package_json_path = None;
        loop {
            if prettier_ignores.contains(&path_to_check) {
                log::debug!("Found prettier ignore at {path_to_check:?}");
                return Ok(ControlFlow::Continue(Some(path_to_check)));
            } else if let Some(package_json_contents) =
                read_package_json(fs, &path_to_check).await?
            {
                let ignore_path = path_to_check.join(".prettierignore");
                if let Some(metadata) = fs
                    .metadata(&ignore_path)
                    .await
                    .with_context(|| format!("fetching metadata for {ignore_path:?}"))?
                    && !metadata.is_dir
                    && !metadata.is_symlink
                {
                    log::info!("Found prettier ignore at {ignore_path:?}");
                    return Ok(ControlFlow::Continue(Some(path_to_check)));
                }
                match &closest_package_json_path {
                    None => closest_package_json_path = Some(path_to_check.clone()),
                    Some(closest_package_json_path) => {
                        if let Some(serde_json::Value::Array(workspaces)) =
                            package_json_contents.get("workspaces")
                        {
                            let subproject_path = closest_package_json_path
                                .strip_prefix(&path_to_check)
                                .expect("traversing path parents, should be able to strip prefix");

                            if workspaces
                                .iter()
                                .filter_map(|value| {
                                    if let serde_json::Value::String(s) = value {
                                        Some(s.clone())
                                    } else {
                                        log::warn!(
                                            "Skipping non-string 'workspaces' value: {value:?}"
                                        );
                                        None
                                    }
                                })
                                .any(|workspace_definition| {
                                    workspace_definition == subproject_path.to_string_lossy()
                                        || PathMatcher::new(
                                            &[workspace_definition],
                                            PathStyle::local(),
                                        )
                                        .ok()
                                        .is_some_and(
                                            |path_matcher| {
                                                RelPath::new(subproject_path, PathStyle::local())
                                                    .is_ok_and(|rel_path| {
                                                        path_matcher.is_match(rel_path)
                                                    })
                                            },
                                        )
                                })
                            {
                                let workspace_ignore = path_to_check.join(".prettierignore");
                                if let Some(metadata) = fs.metadata(&workspace_ignore).await?
                                    && !metadata.is_dir
                                {
                                    log::info!(
                                        "Found prettier ignore at workspace root {workspace_ignore:?}"
                                    );
                                    return Ok(ControlFlow::Continue(Some(path_to_check)));
                                }
                            }
                        }
                    }
                }
            }

            if !path_to_check.pop() {
                log::debug!("Found no prettier ignore in ancestors of {locate_from:?}");
                return Ok(ControlFlow::Continue(None));
            }
        }
    }

    #[cfg(feature = "test-support")]
    pub async fn start(
        _: LanguageServerId,
        prettier_dir: PathBuf,
        _: NodeRuntime,
        _: Duration,
        _: AsyncApp,
    ) -> anyhow::Result<Self> {
        Ok(Self::Test(TestPrettier {
            default: prettier_dir == default_prettier_dir().as_path(),
            prettier_dir,
        }))
    }

    #[cfg(not(feature = "test-support"))]
    pub async fn start(
        server_id: LanguageServerId,
        prettier_dir: PathBuf,
        node: NodeRuntime,
        request_timeout: Duration,
        mut cx: AsyncApp,
    ) -> anyhow::Result<Self> {
        use lsp::{LanguageServerBinary, LanguageServerName};

        let executor = cx.background_executor().clone();
        anyhow::ensure!(
            prettier_dir.is_dir(),
            "Prettier dir {prettier_dir:?} is not a directory"
        );
        let prettier_server = default_prettier_dir().join(PRETTIER_SERVER_FILE);
        anyhow::ensure!(
            prettier_server.is_file(),
            "no prettier server package found at {prettier_server:?}"
        );

        let node_path = executor
            .spawn(async move { node.binary_path().await })
            .await?;
        let server_name = LanguageServerName("prettier".into());
        let server_binary = LanguageServerBinary {
            path: node_path,
            arguments: vec![prettier_server.into(), prettier_dir.as_path().into()],
            env: None,
        };

        let server = LanguageServer::new(
            Arc::new(parking_lot::Mutex::new(None)),
            server_id,
            server_name,
            server_binary,
            &prettier_dir,
            None,
            Default::default(),
            &mut cx,
        )
        .context("prettier server creation")?;

        let server = cx
            .update(|cx| {
                let params = server.default_initialize_params(false, false, cx);
                let configuration = lsp::DidChangeConfigurationParams {
                    settings: Default::default(),
                };
                executor.spawn(server.initialize(params, configuration.into(), request_timeout, cx))
            })
            .await
            .context("prettier server initialization")?;
        Ok(Self::Real(RealPrettier {
            server,
            default: prettier_dir == default_prettier_dir().as_path(),
            prettier_dir,
        }))
    }

    pub async fn format(
        &self,
        buffer: &Entity<Buffer>,
        buffer_path: Option<PathBuf>,
        ignore_dir: Option<PathBuf>,
        range_utf16: Option<Range<OffsetUtf16>>,
        request_timeout: Duration,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<Diff> {
        match self {
            Self::Real(local) => {
                let params = buffer
                    .update(cx, |buffer, cx| {
                        let buffer_language = buffer.language().map(|language| language.as_ref());
                        let language_settings = LanguageSettings::for_buffer(&buffer, cx);
                        let prettier_settings = &language_settings.prettier;
                        anyhow::ensure!(
                            prettier_settings.allowed,
                            "Cannot format: prettier is not allowed for language {buffer_language:?}"
                        );
                        let prettier_node_modules = self.prettier_dir().join("node_modules");
                        anyhow::ensure!(
                            prettier_node_modules.is_dir(),
                            "Prettier node_modules dir does not exist: {prettier_node_modules:?}"
                        );
                        let plugin_name_into_path = |plugin_name: &str| {
                            let prettier_plugin_dir = prettier_node_modules.join(plugin_name);
                            [
                                prettier_plugin_dir.join("dist").join("index.mjs"),
                                prettier_plugin_dir.join("dist").join("index.js"),
                                prettier_plugin_dir.join("dist").join("plugin.js"),
                                prettier_plugin_dir.join("src").join("plugin.js"),
                                prettier_plugin_dir.join("lib").join("index.js"),
                                prettier_plugin_dir.join("index.mjs"),
                                prettier_plugin_dir.join("index.js"),
                                prettier_plugin_dir.join("plugin.js"),
                                // this one is for @prettier/plugin-php
                                prettier_plugin_dir.join("standalone.js"),
                                // this one is for prettier-plugin-latex
                                prettier_plugin_dir.join("dist").join("prettier-plugin-latex.js"),
                                prettier_plugin_dir,
                            ]
                            .into_iter()
                            .find(|possible_plugin_path| possible_plugin_path.is_file())
                        };

                        // Tailwind plugin requires being added last
                        // https://github.com/tailwindlabs/prettier-plugin-tailwindcss#compatibility-with-other-prettier-plugins
                        let mut add_tailwind_back = false;

                        let mut located_plugins = prettier_settings.plugins.iter()
                            .filter(|plugin_name| {
                                if plugin_name.as_str() == TAILWIND_PRETTIER_PLUGIN_PACKAGE_NAME {
                                    add_tailwind_back = true;
                                    false
                                } else {
                                    true
                                }
                            })
                            .map(|plugin_name| {
                                let plugin_path = plugin_name_into_path(plugin_name);
                                (plugin_name.clone(), plugin_path)
                            })
                            .collect::<Vec<_>>();
                        if add_tailwind_back {
                            located_plugins.push((
                                TAILWIND_PRETTIER_PLUGIN_PACKAGE_NAME.to_owned(),
                                plugin_name_into_path(TAILWIND_PRETTIER_PLUGIN_PACKAGE_NAME),
                            ));
                        }

                        let prettier_options = if self.is_default() {
                            let mut options = prettier_settings.options.clone();
                            if !options.contains_key("tabWidth") {
                                options.insert(
                                    "tabWidth".to_string(),
                                    serde_json::Value::Number(serde_json::Number::from(
                                        language_settings.tab_size.get(),
                                    )),
                                );
                            }
                            if !options.contains_key("printWidth") {
                                options.insert(
                                    "printWidth".to_string(),
                                    serde_json::Value::Number(serde_json::Number::from(
                                        language_settings.preferred_line_length,
                                    )),
                                );
                            }
                            if !options.contains_key("useTabs") {
                                options.insert(
                                    "useTabs".to_string(),
                                    serde_json::Value::Bool(language_settings.hard_tabs),
                                );
                            }
                            Some(options)
                        } else {
                            None
                        };

                        let plugins = located_plugins
                            .into_iter()
                            .filter_map(|(plugin_name, located_plugin_path)| {
                                match located_plugin_path {
                                    Some(path) => Some(path),
                                    None => {
                                        log::error!("Have not found plugin path for {plugin_name:?} inside {prettier_node_modules:?}");
                                        None
                                    }
                                }
                            })
                            .collect();

                        let parser = prettier_parser_name(buffer_path.as_deref(), buffer_language, prettier_settings).context("getting prettier parser")?;

                        let ignore_path = ignore_dir.and_then(|dir| {
                            let ignore_file = dir.join(".prettierignore");
                            ignore_file.is_file().then_some(ignore_file)
                        });

                        log::debug!(
                            "Formatting file {:?} with prettier, plugins :{:?}, options: {:?}, ignore_path: {:?}",
                            buffer.file().map(|f| f.full_path(cx)),
                            plugins,
                            prettier_options,
                            ignore_path,
                        );

                        anyhow::Ok(FormatParams {
                            text: buffer.text(),
                            options: FormatOptions {
                                path: buffer_path,
                                parser,
                                plugins,
                                prettier_options,
                                ignore_path,
                                range_start: range_utf16.as_ref().map(|r| r.start.0),
                                range_end: range_utf16.as_ref().map(|r| r.end.0),
                            },
                        })
                })
                .context("building prettier request")?;

                let response = local
                    .server
                    .request::<Format>(params, request_timeout)
                    .await
                    .into_response()?;
                let diff_task = buffer.update(cx, |buffer, cx| buffer.diff(response.text, cx));
                Ok(diff_task.await)
            }
            #[cfg(feature = "test-support")]
            Self::Test(_) => Ok(buffer
                .update(cx, |buffer, cx| {
                    match buffer
                        .language()
                        .map(|language| language.lsp_id())
                        .as_deref()
                    {
                        Some("rust") => anyhow::bail!("prettier does not support Rust"),
                        Some(_other) => {
                            let buffer_language =
                                buffer.language().map(|language| language.as_ref());
                            let language_settings = LanguageSettings::for_buffer(buffer, cx);
                            let prettier_settings = &language_settings.prettier;
                            let parser = prettier_parser_name(
                                buffer_path.as_deref(),
                                buffer_language,
                                prettier_settings,
                            )?;

                            let formatted_text = if let Some(range) = &range_utf16 {
                                let text = buffer.text();
                                let start_byte = buffer.offset_utf16_to_offset(range.start);
                                let insert_at = text[start_byte..]
                                    .find('\n')
                                    .map(|pos| start_byte + pos)
                                    .unwrap_or(text.len());
                                let mut suffix = RANGE_FORMAT_SUFFIX.to_string();
                                if let Some(parser) = &parser {
                                    suffix = format!("{suffix}\n{parser}");
                                }
                                let mut result = String::new();
                                result.push_str(&text[..insert_at]);
                                result.push_str(&suffix);
                                result.push_str(&text[insert_at..]);
                                result
                            } else {
                                let mut text = buffer.text() + FORMAT_SUFFIX;
                                if let Some(parser) = &parser {
                                    text = format!("{text}\n{parser}");
                                }
                                text
                            };

                            Ok(buffer.diff(formatted_text, cx))
                        }
                        None => panic!("Should not format buffer without a language with prettier"),
                    }
                })?
                .await),
        }
    }

    pub async fn clear_cache(&self, request_timeout: Duration) -> anyhow::Result<()> {
        match self {
            Self::Real(local) => local
                .server
                .request::<ClearCache>((), request_timeout)
                .await
                .into_response()
                .context("prettier clear cache"),
            #[cfg(feature = "test-support")]
            Self::Test(_) => Ok(()),
        }
    }

    pub fn server(&self) -> Option<&Arc<LanguageServer>> {
        match self {
            Self::Real(local) => Some(&local.server),
            #[cfg(feature = "test-support")]
            Self::Test(_) => None,
        }
    }

    pub fn is_default(&self) -> bool {
        match self {
            Self::Real(local) => local.default,
            #[cfg(feature = "test-support")]
            Self::Test(test_prettier) => test_prettier.default,
        }
    }

    pub fn prettier_dir(&self) -> &Path {
        match self {
            Self::Real(local) => &local.prettier_dir,
            #[cfg(feature = "test-support")]
            Self::Test(test_prettier) => &test_prettier.prettier_dir,
        }
    }
}

fn prettier_parser_name(
    buffer_path: Option<&Path>,
    buffer_language: Option<&Language>,
    prettier_settings: &PrettierSettings,
) -> anyhow::Result<Option<String>> {
    let parser = if buffer_path.is_none() {
        let parser = prettier_settings
            .parser
            .as_deref()
            .or_else(|| buffer_language.and_then(|language| language.prettier_parser_name()));
        if parser.is_none() {
            log::error!(
                "Formatting unsaved file with prettier failed. No prettier parser configured for language {buffer_language:?}"
            );
            anyhow::bail!("Cannot determine prettier parser for unsaved file");
        }
        parser
    } else if let (Some(buffer_language), Some(buffer_path)) = (buffer_language, buffer_path)
        && buffer_path.extension().is_some_and(|extension| {
            let extension = extension.to_string_lossy();
            !buffer_language
                .config()
                .matcher
                .path_suffixes
                .iter()
                .any(|suffix| suffix.as_str() == extension)
        })
    {
        buffer_language.prettier_parser_name()
    } else {
        prettier_settings.parser.as_deref()
    };

    Ok(parser.map(ToOwned::to_owned))
}

async fn has_prettier_in_node_modules(fs: &dyn Fs, path: &Path) -> anyhow::Result<bool> {
    let possible_node_modules_location = path.join("node_modules").join(PRETTIER_PACKAGE_NAME);
    if let Some(node_modules_location_metadata) = fs
        .metadata(&possible_node_modules_location)
        .await
        .with_context(|| format!("fetching metadata for {possible_node_modules_location:?}"))?
    {
        return Ok(node_modules_location_metadata.is_dir);
    }
    Ok(false)
}

async fn read_package_json(
    fs: &dyn Fs,
    path: &Path,
) -> anyhow::Result<Option<HashMap<String, serde_json::Value>>> {
    let possible_package_json = path.join("package.json");
    if let Some(package_json_metadata) = fs
        .metadata(&possible_package_json)
        .await
        .with_context(|| format!("fetching metadata for package json {possible_package_json:?}"))?
        && !package_json_metadata.is_dir
        && !package_json_metadata.is_symlink
    {
        let package_json_contents = fs
            .load(&possible_package_json)
            .await
            .with_context(|| format!("reading {possible_package_json:?} file contents"))?;
        return serde_json::from_str::<HashMap<String, serde_json::Value>>(&package_json_contents)
            .map(Some)
            .with_context(|| format!("parsing {possible_package_json:?} file contents"));
    }
    Ok(None)
}

enum Format {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FormatParams {
    text: String,
    options: FormatOptions,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FormatOptions {
    plugins: Vec<PathBuf>,
    parser: Option<String>,
    #[serde(rename = "filepath")]
    path: Option<PathBuf>,
    prettier_options: Option<HashMap<String, serde_json::Value>>,
    ignore_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    range_start: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    range_end: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FormatResult {
    text: String,
}

impl lsp::request::Request for Format {
    type Params = FormatParams;
    type Result = FormatResult;
    const METHOD: &'static str = "prettier/format";
}

enum ClearCache {}

impl lsp::request::Request for ClearCache {
    type Params = ();
    type Result = ();
    const METHOD: &'static str = "prettier/clear_cache";
}

