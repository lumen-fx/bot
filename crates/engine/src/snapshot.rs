//! What the bot sees when it evaluates one pull request or issue: GitHub's
//! current state, already read and reduced to facts, plus the little history
//! the bot keeps that GitHub cannot give back.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::state::{CheckClass, Lifecycle};

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub repo: String,
    pub number: u64,
    pub now: DateTime<Utc>,
    pub bot: BotIdentity,
    pub config: ConfigState,
    pub item: Item,
    /// Every issue comment on the thread, oldest first.
    pub comments: Vec<Comment>,
    /// Facts about the people involved, by user ID.
    pub users: BTreeMap<u64, UserFacts>,
    /// The labels that exist in the repository. Read only when a `label`
    /// command needs it.
    pub repo_labels: Option<BTreeSet<String>>,
    /// Whether the issue named by `blocked #N` is closed, when one is named.
    pub blocked_target_closed: Option<bool>,
    pub store: Stored,
}

#[derive(Debug, Clone)]
pub struct BotIdentity {
    /// The App's slug: the name in `@<slug> <command>`.
    pub slug: String,
    /// The App's account login, `<slug>[bot]`.
    pub login: String,
    /// Base URL of the bot's read-only pages, for links.
    pub public_url: String,
}

#[derive(Debug, Clone)]
pub enum ConfigState {
    /// No `.github/lumenfx.toml` on the default branch: answer `help`, do
    /// nothing else.
    Missing,
    Valid(Box<Config>),
    /// The file on main does not validate. `last_good` is the last version
    /// that did, which the bot keeps following.
    Invalid {
        errors: Vec<String>,
        last_good: Option<Box<Config>>,
    },
}

impl ConfigState {
    pub fn effective(&self) -> Option<&Config> {
        match self {
            ConfigState::Valid(c) => Some(c),
            ConfigState::Invalid { last_good, .. } => last_good.as_deref(),
            ConfigState::Missing => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Item {
    Pr(Box<PullRequest>),
    Issue(Issue),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserRef {
    pub id: u64,
    pub login: String,
    #[serde(default)]
    pub is_bot: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserFacts {
    pub login: String,
    pub is_bot: bool,
    /// write, maintain, or admin on the repository.
    pub write_access: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub id: u64,
    pub user: UserRef,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct Issue {
    pub title: String,
    pub open: bool,
    pub author: UserRef,
    pub labels: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub struct PullRequest {
    pub title: String,
    pub lifecycle: Lifecycle,
    pub draft: bool,
    pub author: UserRef,
    pub created_at: DateTime<Utc>,
    /// No merged pull request by this author in the repository.
    pub first_time: bool,
    pub head_sha: String,
    pub base_ref: String,
    pub default_branch: String,
    /// `mergeable_state`: "dirty" is a conflict; `None` while GitHub computes.
    pub mergeable_state: Option<String>,
    pub labels: BTreeSet<String>,
    pub files: Vec<String>,
    /// Short SHAs of commits with more than one parent.
    pub merge_commits: Vec<String>,
    pub checks: Vec<RequiredCheck>,
    /// The `cla` check on the head, as published by whatever posts it.
    pub cla_check: Option<CheckState>,
    /// The `codecov/patch` commit status on the head.
    pub coverage: Option<StatusFacts>,
    pub reviews: Vec<Review>,
    pub threads: Vec<Thread>,
    /// The newest push to the head branch the bot can see.
    pub last_push_at: Option<DateTime<Utc>>,
    pub pullsift: Pullsift,
    pub queued: bool,
    /// Check runs this App already published on the head, by name.
    pub own_checks: BTreeMap<String, OwnCheck>,
    /// Set when the pull request edits the config file: the errors of the
    /// edited version (empty when it is valid).
    pub config_edit: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredCheck {
    pub name: String,
    pub class: CheckClass,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckState {
    Success,
    Failure,
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusFacts {
    pub state: String,
    pub description: String,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub user: UserRef,
    pub state: ReviewState,
    pub submitted_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub resolved: bool,
    /// Oldest first.
    pub comments: Vec<ThreadComment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadComment {
    pub user: UserRef,
    pub created_at: DateTime<Utc>,
}

/// Pullsift's verdict as visible on the pull request. The bot reads it and
/// never changes it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pullsift {
    /// The `slop-suspect` label is set.
    pub flagged: bool,
    /// Pullsift converted the pull request to draft and it is still draft.
    pub holds_draft: bool,
    /// Pullsift's score comment, when it has posted one.
    pub comment: Option<PullsiftComment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullsiftComment {
    /// The comment reads as a pass (below every action threshold).
    pub passed: bool,
    pub url: String,
}

impl Pullsift {
    pub fn holds(&self) -> bool {
        self.flagged || self.holds_draft
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnCheck {
    pub id: u64,
    pub conclusion: Option<String>,
    pub title: String,
    pub summary: String,
}

/// What the bot stored about this thread.
#[derive(Debug, Clone, Default)]
pub struct Stored {
    pub entity: EntityState,
    /// The status comment the bot made, if any.
    pub status_comment_id: Option<u64>,
    /// New comments the bot posted on this thread in the last 24 hours.
    pub comments_last_day: u32,
    /// (purpose, key) of every comment the bot posted here.
    pub posted: BTreeSet<(String, String)>,
}

/// The per-thread state the bot keeps, stored as one JSON document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EntityState {
    /// (comment ID, command key) of every command already handled.
    pub processed: BTreeSet<(u64, String)>,
    pub concerns: BTreeMap<String, Concern>,
    /// Last `ready` by the author or a maintainer.
    pub ready_at: Option<DateTime<Utc>>,
    /// Last `author` by a maintainer.
    pub author_forced_at: Option<DateTime<Utc>>,
    /// The blocked label was set by the `blocked` command.
    pub blocked: bool,
    pub blocked_by: Option<u64>,
    pub no_docs_needed: bool,
    /// A conflict comment went out for the current conflict episode.
    pub conflict_notified: bool,
    /// The author activity time the last nudge was for.
    pub nudged_for: Option<DateTime<Utc>>,
    /// Whether the last evaluation found something actionable, so a deleted
    /// status comment comes back only on the next transition into it.
    pub last_actionable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Concern {
    pub by: String,
    pub at: DateTime<Utc>,
}
