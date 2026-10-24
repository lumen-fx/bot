//! A Plan: what the bot wants GitHub and its own store to look like after one
//! evaluation. The executor writes only the part that differs from what is
//! already there, so applying a Plan twice writes nothing the second time.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::snapshot::EntityState;
use crate::state::Decision;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// The state machine's decision, for the log; `None` for issues and for
    /// repositories without a config.
    pub decision: Option<Decision>,
    pub labels: Vec<LabelChange>,
    pub status_comment: Option<StatusComment>,
    pub comments: Vec<NewComment>,
    pub reactions: Vec<Reaction>,
    pub check_runs: Vec<CheckRun>,
    /// Signatures to append to the CLA store before anything else is
    /// written.
    pub cla_signatures: Vec<ClaSignature>,
    /// Lock the conversation.
    pub lock: bool,
    /// The thread's state after this evaluation; stored as is.
    pub state: EntityState,
    pub jobs: Vec<Job>,
    /// Why, in plain words: one line per rule that fired or write held back.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelChange {
    pub label: String,
    /// true: the label must be present; false: absent.
    pub present: bool,
    pub rule: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusComment {
    pub body: String,
    /// Whether to create it when it does not exist. Edits of an existing
    /// status comment always go through.
    pub create: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewComment {
    /// What the comment is for; with `key`, deduplicates it.
    pub purpose: String,
    pub key: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    pub comment_id: u64,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRun {
    pub name: String,
    pub head_sha: String,
    /// `None` publishes the run as in progress.
    pub conclusion: Option<String>,
    pub title: String,
    pub summary: String,
}

/// One entry of the CLA store, in CLA Assistant's format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaSignature {
    /// `owner/name` of the store, and the file in it.
    pub store: String,
    pub path: String,
    /// The login at signing; matching is always by `id`.
    pub name: String,
    pub id: u64,
    /// The signing comment: the signature's record.
    pub comment_id: u64,
    pub created_at: DateTime<Utc>,
    pub repo: String,
    pub repo_id: u64,
    pub pull_request: u64,
}

/// A re-evaluation of this thread at a later time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub reason: String,
    pub at: DateTime<Utc>,
}

impl Plan {
    /// True when applying the plan would write nothing to GitHub (the stored
    /// state may still change).
    pub fn writes_nothing(&self) -> bool {
        self.labels.is_empty()
            && self.status_comment.is_none()
            && self.comments.is_empty()
            && self.reactions.is_empty()
            && self.check_runs.is_empty()
            && self.cla_signatures.is_empty()
            && !self.lock
    }
}
