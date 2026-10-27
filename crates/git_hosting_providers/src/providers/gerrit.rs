use std::str::FromStr;
use std::sync::Arc;

use url::Url;

use git::{
    BuildCommitPermalinkParams, BuildPermalinkParams, GitHostingProvider, ParsedGitRemote,
    RemoteUrl,
};

pub struct Gerrit {
    name: String,
    base_url: Url,
}

impl Gerrit {
    pub fn new(name: impl Into<String>, base_url: Url) -> Self {
        Self {
            name: name.into(),
            base_url,
        }
    }
}

#[async_trait::async_trait]
impl GitHostingProvider for Gerrit {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn base_url(&self) -> Url {
        self.base_url.clone()
    }

    fn supports_avatars(&self) -> bool {
        false
    }

    fn format_line_number(&self, line: u32) -> String {
        format!("{line}")
    }

    fn format_line_numbers(&self, start_line: u32, _end_line: u32) -> String {
        format!("{start_line}")
    }

    fn parse_remote_url(&self, url: &str) -> Option<ParsedGitRemote> {
        let url = RemoteUrl::from_str(url).ok()?;

        let host = url.host_str()?;
        if host != self.base_url.host_str()? {
            return None;
        }

        let path_segments = url.path_segments()?.collect::<Vec<_>>();
        let joined_path = path_segments.join("/");
        let repo = joined_path
            .trim_start_matches("a/")
            .trim_end_matches(".git");

        Some(ParsedGitRemote {
            owner: Arc::from(""),
            repo: repo.into(),
        })
    }

    fn build_commit_permalink(
        &self,
        remote: &ParsedGitRemote,
        params: BuildCommitPermalinkParams,
    ) -> Url {
        let BuildCommitPermalinkParams { sha } = params;
        let ParsedGitRemote { owner: _, repo } = remote;

        self.base_url().join(&format!("{repo}/+/{sha}")).unwrap()
    }

    fn build_permalink(&self, remote: ParsedGitRemote, params: BuildPermalinkParams) -> Url {
        let ParsedGitRemote { owner: _, repo } = remote;
        let BuildPermalinkParams {
            sha,
            path,
            selection,
        } = params;

        let mut permalink = self
            .base_url()
            .join(&format!("{repo}/+/{sha}/{path}"))
            .unwrap();
        permalink.set_fragment(
            selection
                .map(|selection| self.line_fragment(&selection))
                .as_deref(),
        );
        permalink
    }
}

