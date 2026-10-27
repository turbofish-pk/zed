use anyhow::Result;
use async_trait::async_trait;
use collections::HashMap;
use gpui::AsyncApp;
use language::{LanguageServerName, LspAdapter, LspAdapterDelegate, LspInstaller, Toolchain};
use lsp::{LanguageServerBinary, Uri};
use node_runtime::{NodeRuntime, VersionStrategy};
use project::{ContextProviderWithTasks, lsp_store::language_server_settings};
use semver::Version;
use serde_json::Value;
use std::{future::Future, path::PathBuf, sync::Arc, vec};
use task::{TaskTemplate, TaskTemplates, VariableName};
use util::{ResultExt, maybe};

pub(super) fn bash_task_context() -> ContextProviderWithTasks {
    ContextProviderWithTasks::new(TaskTemplates(vec![
        TaskTemplate {
            label: "execute selection".to_owned(),
            command: VariableName::SelectedText.template_value(),
            ..TaskTemplate::default()
        },
        TaskTemplate {
            label: format!("run '{}'", VariableName::File.template_value()),
            command: VariableName::File.template_value(),
            tags: vec!["bash-script".to_owned()],
            ..TaskTemplate::default()
        },
    ]))
}

pub struct BashLspAdapter {
    node: NodeRuntime,
}

impl BashLspAdapter {
    const PACKAGE_NAME: &str = "bash-language-server";
    const NODE_MODULE_RELATIVE_SERVER_PATH: &str = "bash-language-server/out/cli.js";

    pub fn new(node: NodeRuntime) -> Self {
        Self { node }
    }

    async fn get_cached_server_binary(
        container_dir: PathBuf,
        env: HashMap<String, String>,
        node: &NodeRuntime,
    ) -> Option<lsp::LanguageServerBinary> {
        maybe!(async {
            let server_path = container_dir
                .join("node_modules")
                .join(Self::NODE_MODULE_RELATIVE_SERVER_PATH);
            anyhow::ensure!(
                server_path.exists(),
                "missing executable in directory {server_path:?}"
            );
            Ok(LanguageServerBinary {
                path: node.binary_path().await?,
                env: Some(env),
                arguments: vec![server_path.into(), "start".into()],
            })
        })
        .await
        .log_err()
    }
}

impl LspInstaller for BashLspAdapter {
    type BinaryVersion = Version;

    async fn cached_server_binary(
        &self,
        container_dir: std::path::PathBuf,
        delegate: &dyn LspAdapterDelegate,
    ) -> Option<lsp::LanguageServerBinary> {
        let env = delegate.shell_env().await;
        Self::get_cached_server_binary(container_dir, env, &self.node).await
    }

    async fn check_if_user_installed(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: &gpui::AsyncApp,
    ) -> Option<lsp::LanguageServerBinary> {
        let path = delegate.which(Self::PACKAGE_NAME.as_ref()).await?;
        let env = delegate.shell_env().await;

        Some(LanguageServerBinary {
            path,
            env: Some(env),
            arguments: vec!["start".into()],
        })
    }

    fn check_if_version_installed(
        &self,
        version: &Self::BinaryVersion,
        container_dir: &PathBuf,
        delegate: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Option<lsp::LanguageServerBinary>> + use<> {
        let node = self.node.clone();
        let version = version.clone();
        let container_dir = container_dir.clone();
        let delegate = delegate.clone();

        async move {
            let server_path = container_dir
                .join("node_modules")
                .join(Self::NODE_MODULE_RELATIVE_SERVER_PATH);

            let should_install_language_server = node
                .should_install_npm_package(
                    Self::PACKAGE_NAME,
                    &server_path,
                    &container_dir,
                    VersionStrategy::Latest(&version),
                )
                .await;

            if should_install_language_server {
                None
            } else {
                let env = delegate.shell_env().await;
                Some(LanguageServerBinary {
                    path: node.binary_path().await.ok()?,
                    env: Some(env),
                    arguments: vec![server_path.into(), "start".into()],
                })
            }
        }
    }

    async fn fetch_latest_server_version(
        &self,
        _: &Arc<dyn LspAdapterDelegate>,
        _: bool,
        _: &mut gpui::AsyncApp,
    ) -> Result<Self::BinaryVersion> {
        self.node
            .npm_package_latest_version(Self::PACKAGE_NAME)
            .await
    }

    fn fetch_server_binary(
        &self,
        _latest_version: Self::BinaryVersion,
        container_dir: std::path::PathBuf,
        delegate: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<lsp::LanguageServerBinary>> + use<> {
        let node = self.node.clone();
        let delegate = delegate.clone();

        async move {
            let server_path = container_dir
                .join("node_modules")
                .join(Self::NODE_MODULE_RELATIVE_SERVER_PATH);

            node.npm_install_latest_packages(&container_dir, &[Self::PACKAGE_NAME])
                .await?;

            let env = delegate.shell_env().await;
            Ok(LanguageServerBinary {
                path: node.binary_path().await?,
                env: Some(env),
                arguments: vec![server_path.into(), "start".into()],
            })
        }
    }
}

#[async_trait(?Send)]
impl LspAdapter for BashLspAdapter {
    fn name(&self) -> LanguageServerName {
        LanguageServerName::new_static(Self::PACKAGE_NAME)
    }

    async fn workspace_configuration(
        self: Arc<Self>,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: Option<Uri>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let settings = cx.update(|cx| {
            language_server_settings(delegate.as_ref(), &self.name(), cx)
                .and_then(|s| s.settings.clone())
        });

        Ok(settings.unwrap_or_default())
    }
}

