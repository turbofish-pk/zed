use crate::{
    BuildCommitPermalinkParams, GitHostingProviderRegistry, GitRemote, Oid, parse_git_remote_url,
    repository::GitBinary, status::StatusCode,
};
use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use gpui::SharedString;
use std::{str::FromStr, sync::Arc};

#[derive(Clone, Debug, Default)]
pub struct ParsedCommitMessage {
    pub message: SharedString,
    pub permalink: Option<url::Url>,
    pub pull_request: Option<crate::hosting_provider::PullRequest>,
    pub remote: Option<GitRemote>,
}

impl ParsedCommitMessage {
    pub fn parse(
        sha: String,
        message: String,
        remote_url: Option<&str>,
        provider_registry: Option<Arc<GitHostingProviderRegistry>>,
    ) -> Self {
        if let Some((hosting_provider, remote)) = provider_registry
            .and_then(|reg| remote_url.and_then(|url| parse_git_remote_url(reg, url)))
        {
            let pull_request = hosting_provider.extract_pull_request(&remote, &message);
            Self {
                message: message.into(),
                permalink: Some(
                    hosting_provider
                        .build_commit_permalink(&remote, BuildCommitPermalinkParams { sha: &sha }),
                ),
                pull_request,
                remote: Some(GitRemote {
                    host: hosting_provider,
                    owner: remote.owner.into(),
                    repo: remote.repo.into(),
                }),
            }
        } else {
            Self {
                message: message.into(),
                ..Default::default()
            }
        }
    }
}

pub(crate) async fn get_messages(git: &GitBinary, shas: &[Oid]) -> Result<HashMap<Oid, String>> {
    if shas.is_empty() {
        return Ok(HashMap::default());
    }

    let output = if cfg!(windows) {
        // Windows has a maximum invocable command length, so we chunk the input.
        // Actual max is 32767, but we leave some room for the rest of the command as we aren't in precise control of what std might do here
        const MAX_CMD_LENGTH: usize = 30000;
        // 40 bytes of hash, 2 quotes and a separating space
        const SHA_LENGTH: usize = 40 + 2 + 1;
        const MAX_ENTRIES_PER_INVOCATION: usize = MAX_CMD_LENGTH / SHA_LENGTH;

        let mut result = vec![];
        for shas in shas.chunks(MAX_ENTRIES_PER_INVOCATION) {
            let partial = get_messages_impl(git, shas).await?;
            result.extend(partial);
        }
        result
    } else {
        get_messages_impl(git, shas).await?
    };

    Ok(shas
        .iter()
        .cloned()
        .zip(output)
        .collect::<HashMap<Oid, String>>())
}

pub(crate) async fn get_tag_names(
    git: &GitBinary,
    shas: &[Oid],
) -> Result<HashMap<Oid, Vec<String>>> {
    if shas.is_empty() {
        return Ok(HashMap::default());
    }

    let output = git
        .build_command(&[
            "for-each-ref",
            "refs/tags",
            "--sort=-creatordate",
            "--format=%(objectname)%00%(*objectname)%00%(refname:short)",
        ])
        .output()
        .await
        .context("starting git for-each-ref process")?;
    anyhow::ensure!(
        output.status.success(),
        "'git for-each-ref' failed with error {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    Ok(parse_tag_names(
        &String::from_utf8_lossy(&output.stdout),
        shas,
    ))
}

fn parse_tag_names(output: &str, shas: &[Oid]) -> HashMap<Oid, Vec<String>> {
    let shas = shas.iter().copied().collect::<HashSet<_>>();
    let mut result = HashMap::<Oid, Vec<String>>::default();

    for line in output.lines() {
        let mut fields = line.split('\0');
        let object_sha = fields.next();
        let peeled_sha = fields.next().filter(|sha| !sha.is_empty());
        let Some(sha) = peeled_sha
            .or(object_sha)
            .and_then(|sha| Oid::from_str(sha).ok())
        else {
            continue;
        };
        let Some(tag_name) = fields.next().filter(|tag_name| !tag_name.is_empty()) else {
            continue;
        };
        result.entry(sha).or_default().push(tag_name.to_string());
    }

    result.retain(|sha, _| shas.contains(sha));
    result
}

async fn get_messages_impl(git: &GitBinary, shas: &[Oid]) -> Result<Vec<String>> {
    const MARKER: &str = "<MARKER>";
    let output = git
        .build_command(&["show"])
        .arg("-s")
        .arg(format!("--format=%B{}", MARKER))
        .args(shas.iter().map(ToString::to_string))
        .output()
        .await
        .context("starting git show process")?;
    anyhow::ensure!(
        output.status.success(),
        "'git show' failed with error {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim()
        .split_terminator(MARKER)
        .map(|str| str.trim().replace("<", "&lt;").replace(">", "&gt;"))
        .collect::<Vec<_>>())
}

pub(crate) const GITLINK_MODE: &str = "160000";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitDiffObjectKind {
    Blob,
    Gitlink,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CommitDiffObject<'a> {
    pub oid: &'a str,
    pub kind: CommitDiffObjectKind,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CommitDiffEntry<'a> {
    pub path: &'a str,
    pub status: StatusCode,
    pub old_object: Option<CommitDiffObject<'a>>,
    pub new_object: Option<CommitDiffObject<'a>>,
}

/// Parses the output of `git diff --raw --no-abbrev -z`.
pub(crate) fn parse_git_diff_raw(
    content: &str,
) -> impl Iterator<Item = Result<CommitDiffEntry<'_>>> {
    let mut parts = content.split('\0');
    std::iter::from_fn(move || {
        let metadata = parts.next()?;
        if metadata.is_empty() {
            return None;
        }

        let path = match parts.next() {
            Some(path) => path,
            None => return Some(Err(anyhow::anyhow!("raw diff is missing the path"))),
        };
        Some(parse_git_diff_raw_entry(metadata, path))
    })
}

fn parse_git_diff_raw_entry<'a>(metadata: &'a str, path: &'a str) -> Result<CommitDiffEntry<'a>> {
    let mut fields = metadata
        .strip_prefix(':')
        .context("raw diff metadata is missing its ':' prefix")?
        .split_ascii_whitespace();
    let old_mode = fields.next().context("raw diff is missing the old mode")?;
    let new_mode = fields.next().context("raw diff is missing the new mode")?;
    let old_oid = fields
        .next()
        .context("raw diff is missing the old object ID")?;
    let new_oid = fields
        .next()
        .context("raw diff is missing the new object ID")?;
    let status = match fields.next() {
        Some("M") => StatusCode::Modified,
        Some("T") => StatusCode::TypeChanged,
        Some("A") => StatusCode::Added,
        Some("D") => StatusCode::Deleted,
        Some(status) => anyhow::bail!("unsupported raw diff status {status}"),
        None => anyhow::bail!("raw diff is missing the status"),
    };

    Ok(CommitDiffEntry {
        path,
        status,
        old_object: (!old_oid.bytes().all(|byte| byte == b'0')).then(|| CommitDiffObject {
            oid: old_oid,
            kind: if old_mode == GITLINK_MODE {
                CommitDiffObjectKind::Gitlink
            } else {
                CommitDiffObjectKind::Blob
            },
        }),
        new_object: (!new_oid.bytes().all(|byte| byte == b'0')).then(|| CommitDiffObject {
            oid: new_oid,
            kind: if new_mode == GITLINK_MODE {
                CommitDiffObjectKind::Gitlink
            } else {
                CommitDiffObjectKind::Blob
            },
        }),
    })
}

