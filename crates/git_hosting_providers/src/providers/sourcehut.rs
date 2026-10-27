use std::str::FromStr;

use anyhow::{Result, bail};
use url::Url;

use git::{
    BuildCommitPermalinkParams, BuildPermalinkParams, GitHostingProvider, ParsedGitRemote,
    RemoteUrl,
};

use crate::get_host_from_git_remote_url;

pub struct SourceHut {
    name: String,
    base_url: Url,
}

impl SourceHut {
    pub fn new(name: &str, base_url: Url) -> Self {
        Self {
            name: name.to_string(),
            base_url,
        }
    }

    pub fn public_instance() -> Self {
        Self::new("SourceHut", Url::parse("https://git.sr.ht").unwrap())
    }

    pub fn from_remote_url(remote_url: &str) -> Result<Self> {
        let host = get_host_from_git_remote_url(remote_url)?;
        if host == "git.sr.ht" {
            bail!("the SourceHut instance is not self-hosted");
        }

        // TODO: detecting self hosted instances by checking whether "sourcehut" is in the url or not
        // is not very reliable. See https://github.com/zed-industries/zed/issues/26393 for more
        // information.
        if !host.contains("sourcehut") {
            bail!("not a SourceHut URL");
        }

        Ok(Self::new(
            "SourceHut Self-Hosted",
            Url::parse(&format!("https://{}", host))?,
        ))
    }
}

impl GitHostingProvider for SourceHut {
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
        format!("L{line}")
    }

    fn format_line_numbers(&self, start_line: u32, end_line: u32) -> String {
        format!("L{start_line}-{end_line}")
    }

    fn parse_remote_url(&self, url: &str) -> Option<ParsedGitRemote> {
        let url = RemoteUrl::from_str(url).ok()?;

        let host = url.host_str()?;
        if host != self.base_url.host_str()? {
            return None;
        }

        let mut path_segments = url.path_segments()?;
        let owner = path_segments.next()?.trim_start_matches('~');
        // We don't trim the `.git` suffix here like we do elsewhere, as
        // sourcehut treats a repo with `.git` suffix as a separate repo.
        //
        // For example, `git@git.sr.ht:~username/repo` and `git@git.sr.ht:~username/repo.git`
        // are two distinct repositories.
        let repo = path_segments.next()?;

        Some(ParsedGitRemote {
            owner: owner.into(),
            repo: repo.into(),
        })
    }

    fn build_commit_permalink(
        &self,
        remote: &ParsedGitRemote,
        params: BuildCommitPermalinkParams,
    ) -> Url {
        let BuildCommitPermalinkParams { sha } = params;
        let ParsedGitRemote { owner, repo } = remote;

        self.base_url()
            .join(&format!("~{owner}/{repo}/commit/{sha}"))
            .unwrap()
    }

    fn build_permalink(&self, remote: ParsedGitRemote, params: BuildPermalinkParams) -> Url {
        let ParsedGitRemote { owner, repo } = remote;
        let BuildPermalinkParams {
            sha,
            path,
            selection,
        } = params;

        let mut permalink = self
            .base_url()
            .join(&format!("~{owner}/{repo}/tree/{sha}/item/{path}"))
            .unwrap();
        permalink.set_fragment(
            selection
                .map(|selection| self.line_fragment(&selection))
                .as_deref(),
        );
        permalink
    }
}

