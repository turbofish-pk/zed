use std::{fmt, ops::Not as _, rc::Rc};

use futures::StreamExt;
use itertools::Itertools as _;

use crate::{
    git::{AutomatedChangeKind, CommitDetails, CommitList, ZED_ZIPPY_LOGIN},
    github::{
        Approvable, CommitAuthor, CommitFileChange, CommitMetadata, GithubApiClient, GithubLogin,
        PullRequestComment, PullRequestData, PullRequestReview, Repository, ReviewState,
    },
    report::{Report, ReportEntry},
};

const ZED_ZIPPY_COMMENT_APPROVAL_PATTERN: &str = "@zed-zippy approve";
const ZED_ZIPPY_GROUP_APPROVAL: &str = "@zed-industries/approved";

#[derive(Debug)]
pub enum ReviewSuccess {
    ApprovingComment(Vec<PullRequestComment>),
    CoAuthored(Vec<CommitAuthor>),
    PullRequestReviewed(Vec<PullRequestReview>),
    ZedZippyCommit(AutomatedChangeKind, GithubLogin),
}

impl ReviewSuccess {
    pub(crate) fn reviewers(&self) -> anyhow::Result<String> {
        let reviewers = match self {
            Self::CoAuthored(authors) => authors.iter().map(ToString::to_string).collect_vec(),
            Self::PullRequestReviewed(reviews) => reviews
                .iter()
                .filter_map(|review| review.user.as_ref())
                .map(|user| format!("@{}", user.login))
                .collect_vec(),
            Self::ApprovingComment(comments) => comments
                .iter()
                .map(|comment| format!("@{}", comment.user.login))
                .collect_vec(),
            Self::ZedZippyCommit(_, login) => vec![login.to_string()],
        };

        let reviewers = reviewers.into_iter().unique().collect_vec();

        reviewers
            .is_empty()
            .not()
            .then(|| reviewers.join(", "))
            .ok_or_else(|| anyhow::anyhow!("Expected at least one reviewer"))
    }
}

impl fmt::Display for ReviewSuccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CoAuthored(_) => formatter.write_str("Co-authored by an organization member"),
            Self::PullRequestReviewed(_) => {
                formatter.write_str("Approved by an organization review")
            }
            Self::ApprovingComment(_) => {
                formatter.write_str("Approved by an organization approval comment")
            }
            Self::ZedZippyCommit(kind, _) => {
                write!(formatter, "Fully untampered automated {kind}")
            }
        }
    }
}

#[derive(Debug)]
pub enum ReviewFailure {
    // todo: We could still query the GitHub API here to search for one
    NoPullRequestFound,
    Unreviewed,
    UnexpectedZippyAction(AutomatedChangeFailure),
    Other(anyhow::Error),
}

impl fmt::Display for ReviewFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoPullRequestFound => formatter.write_str("No pull request found"),
            Self::Unreviewed => formatter
                .write_str("No qualifying organization approval found for the pull request"),
            Self::UnexpectedZippyAction(failure) => {
                write!(formatter, "Validating Zed Zippy change failed: {failure}")
            }
            Self::Other(error) => write!(formatter, "Failed to inspect review state: {error}"),
        }
    }
}

#[derive(Debug)]
pub enum AutomatedChangeFailure {
    NoMentionInTitle,
    MissingCommitData,
    AuthorMismatch,
    UnexpectedCoAuthors,
    NotSigned,
    InvalidSignature,
    UnexpectedLineChanges {
        kind: AutomatedChangeKind,
        additions: u64,
        deletions: u64,
    },
    UnexpectedFiles {
        kind: AutomatedChangeKind,
        found: Vec<String>,
    },
}

impl fmt::Display for AutomatedChangeFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMentionInTitle => formatter.write_str("No @-mention found in commit title"),
            Self::MissingCommitData => formatter.write_str("No commit data found on GitHub"),
            Self::AuthorMismatch => {
                formatter.write_str("GitHub author does not match bot identity")
            }
            Self::UnexpectedCoAuthors => formatter.write_str("Commit has unexpected co-authors"),
            Self::NotSigned => formatter.write_str("Commit is not signed"),
            Self::InvalidSignature => formatter.write_str("Commit signature is invalid"),
            Self::UnexpectedLineChanges {
                kind,
                additions,
                deletions,
            } => {
                write!(
                    formatter,
                    "Unexpected line changes for {kind} \
                     ({additions} additions, {deletions} deletions, \
                     expected {} each)",
                    kind.expected_loc()
                )
            }
            Self::UnexpectedFiles { kind, found } => {
                let expected = kind.expected_files().join(", ");
                let actual = found.join(", ");
                write!(
                    formatter,
                    "Unexpected files changed for {kind} \
                     (expected [{expected}], found [{actual}])"
                )
            }
        }
    }
}

impl AutomatedChangeKind {
    fn validate_changes(
        self,
        metadata: &CommitMetadata,
        files: &[CommitFileChange],
    ) -> Result<(), AutomatedChangeFailure> {
        let expected_loc = self.expected_loc();
        if metadata.additions() != expected_loc || metadata.deletions() != expected_loc {
            return Err(AutomatedChangeFailure::UnexpectedLineChanges {
                kind: self,
                additions: metadata.additions(),
                deletions: metadata.deletions(),
            });
        }

        let files_differ = files.len() != self.expected_files().len()
            || files
                .iter()
                .any(|f| self.expected_files().contains(&f.filename.as_str()).not());

        if files_differ {
            return Err(AutomatedChangeFailure::UnexpectedFiles {
                kind: self,
                found: files.into_iter().map(|f| f.filename.clone()).collect(),
            });
        }

        Ok(())
    }
}

pub(crate) type ReviewResult = Result<ReviewSuccess, ReviewFailure>;

impl<E: Into<anyhow::Error>> From<E> for ReviewFailure {
    fn from(err: E) -> Self {
        Self::Other(anyhow::anyhow!(err))
    }
}

pub struct Reporter {
    commits: CommitList,
    github_client: Rc<dyn GithubApiClient>,
}

impl Reporter {
    pub fn new(commits: CommitList, github_client: Rc<dyn GithubApiClient>) -> Self {
        Self {
            commits,
            github_client,
        }
    }

    pub async fn result_for_commit(
        commit: CommitDetails,
        github_client: Rc<dyn GithubApiClient>,
    ) -> ReviewResult {
        Self::new(Default::default(), github_client)
            .check_commit(&commit)
            .await
    }

    /// Method that checks every commit for compliance
    pub async fn check_commit(
        &self,
        commit: &CommitDetails,
    ) -> Result<ReviewSuccess, ReviewFailure> {
        let Some(pr_number) = commit.pr_number() else {
            if commit.author().is_zed_zippy() {
                return self.check_zippy_automated_change(commit).await;
            } else {
                return Err(ReviewFailure::NoPullRequestFound);
            }
        };

        let pull_request = self
            .github_client
            .get_pull_request(&Repository::ZED, pr_number)
            .await?;

        if let Some(approval) = self
            .check_approving_pull_request_review(&pull_request)
            .await?
        {
            return Ok(approval);
        }

        if let Some(approval) = self
            .check_approving_pull_request_comment(&pull_request)
            .await?
        {
            return Ok(approval);
        }

        if let Some(approval) = self.check_commit_co_authors(commit).await? {
            return Ok(approval);
        }

        Err(ReviewFailure::Unreviewed)
    }

    async fn check_zippy_automated_change(
        &self,
        commit: &CommitDetails,
    ) -> Result<ReviewSuccess, ReviewFailure> {
        let (change_kind, responsible_actor) =
            commit
                .detect_automated_change()
                .ok_or(ReviewFailure::UnexpectedZippyAction(
                    AutomatedChangeFailure::NoMentionInTitle,
                ))?;

        let commit_data = self
            .github_client
            .get_commit_metadata(&Repository::ZED, &[commit.sha()])
            .await?;

        let metadata =
            commit_data
                .get(commit.sha())
                .ok_or(ReviewFailure::UnexpectedZippyAction(
                    AutomatedChangeFailure::MissingCommitData,
                ))?;

        if !metadata
            .primary_author()
            .user()
            .is_some_and(|login| login.as_str() == ZED_ZIPPY_LOGIN)
        {
            return Err(ReviewFailure::UnexpectedZippyAction(
                AutomatedChangeFailure::AuthorMismatch,
            ));
        }

        if metadata.co_authors().is_some() {
            return Err(ReviewFailure::UnexpectedZippyAction(
                AutomatedChangeFailure::UnexpectedCoAuthors,
            ));
        }

        let signature = metadata
            .signature()
            .ok_or(ReviewFailure::UnexpectedZippyAction(
                AutomatedChangeFailure::NotSigned,
            ))?;

        if !signature.is_valid() {
            return Err(ReviewFailure::UnexpectedZippyAction(
                AutomatedChangeFailure::InvalidSignature,
            ));
        }

        let files = self
            .github_client
            .get_commit_files(&Repository::ZED, commit.sha())
            .await?;

        change_kind
            .validate_changes(metadata, &files)
            .map_err(ReviewFailure::UnexpectedZippyAction)?;

        Ok(ReviewSuccess::ZedZippyCommit(
            change_kind,
            GithubLogin::new(responsible_actor.to_owned()),
        ))
    }

    async fn check_commit_co_authors(
        &self,
        commit: &CommitDetails,
    ) -> Result<Option<ReviewSuccess>, ReviewFailure> {
        if commit.co_authors().is_some()
            && let Some(commit_authors) = self
                .github_client
                .get_commit_metadata(&Repository::ZED, &[commit.sha()])
                .await?
                .get(commit.sha())
                .and_then(|authors| authors.co_authors())
        {
            let mut org_co_authors = Vec::new();
            for co_author in commit_authors {
                if let Some(github_login) = co_author.user()
                    && self
                        .github_client
                        .check_repo_write_permission(&Repository::ZED, github_login)
                        .await?
                {
                    org_co_authors.push(co_author.clone());
                }
            }

            Ok(org_co_authors
                .is_empty()
                .not()
                .then_some(ReviewSuccess::CoAuthored(org_co_authors)))
        } else {
            Ok(None)
        }
    }

    async fn check_approving_pull_request_review(
        &self,
        pull_request: &PullRequestData,
    ) -> Result<Option<ReviewSuccess>, ReviewFailure> {
        let reviews = self
            .github_client
            .get_pull_request_reviews(&Repository::ZED, pull_request.number)
            .await?;

        let qualifying_reviews = reviews
            .into_iter()
            .filter(|review| Self::is_qualifying_approval(review, pull_request))
            .collect_vec();

        Ok(qualifying_reviews
            .is_empty()
            .not()
            .then_some(ReviewSuccess::PullRequestReviewed(qualifying_reviews)))
    }

    async fn check_approving_pull_request_comment(
        &self,
        pull_request: &PullRequestData,
    ) -> Result<Option<ReviewSuccess>, ReviewFailure> {
        let comments = self
            .github_client
            .get_pull_request_comments(&Repository::ZED, pull_request.number)
            .await?;

        let qualifying_comments = comments
            .into_iter()
            .filter(|comment| Self::is_qualifying_approval(comment, pull_request))
            .collect_vec();

        Ok(qualifying_comments
            .is_empty()
            .not()
            .then_some(ReviewSuccess::ApprovingComment(qualifying_comments)))
    }

    pub fn is_qualifying_approval(item: &impl Approvable, pull_request: &PullRequestData) -> bool {
        let Some(author_login) = item.author_login() else {
            return false;
        };

        let distinct_actor = pull_request
            .user
            .as_ref()
            .is_none_or(|pr_user| pr_user.login != author_login);

        let approving_pattern = item
            .review_state()
            .is_some_and(|state| state == ReviewState::Approved)
            || item.body().is_some_and(Self::contains_approving_pattern);

        let actor_is_authorized = item
            .author_association()
            .is_some_and(|association| association.has_write_access());

        distinct_actor && approving_pattern && actor_is_authorized
    }

    fn contains_approving_pattern(body: &str) -> bool {
        body.contains(ZED_ZIPPY_COMMENT_APPROVAL_PATTERN) || body.contains(ZED_ZIPPY_GROUP_APPROVAL)
    }

    pub async fn generate_report(mut self, max_concurrent_checks: usize) -> Report {
        let commits_to_check = std::mem::take(&mut self.commits);
        let total_commits = commits_to_check.len();

        let reports = futures::stream::iter(commits_to_check.into_iter().enumerate().map(
            async |(i, commit)| {
                println!(
                    "Checking commit {:?} ({current}/{total})",
                    commit.sha().short(),
                    current = i + 1,
                    total = total_commits
                );

                let review_result = self.check_commit(&commit).await;

                if let Err(err) = &review_result {
                    println!("Commit {:?} failed review: {:?}", commit.sha().short(), err);
                }

                (commit, review_result)
            },
        ))
        .buffered(max_concurrent_checks)
        .collect::<Vec<_>>()
        .await;

        Report::from_entries(
            reports
                .into_iter()
                .map(|(commit, result)| ReportEntry::new(commit, result)),
        )
    }
}

