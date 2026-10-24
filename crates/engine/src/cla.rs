//! The CLA gate: who must sign a pull request, whether they have, the
//! `sign` command's sentence, and the merge-group verdict. The server
//! resolves commit emails to accounts and reads the signature store; this
//! module only decides.

use std::collections::BTreeSet;

use crate::command::Command;
use crate::config::Config;
use crate::plan::CheckRun;
use crate::snapshot::{CheckState, ClaStore, CommitFacts, UserRef};
use crate::templates;

/// The check run the bot publishes on pull request heads and merge groups.
pub const CLA_CHECK: &str = "cla";

/// GitHub's own committer for changes made in its web interface. It commits
/// on someone else's behalf and never signs.
pub const WEB_FLOW_ID: u64 = 19_864_447;

/// What a pull request's commits ask for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Requirement {
    /// Accounts that must have signed, in the order the commits name them.
    pub signers: Vec<UserRef>,
    /// Commits whose author email resolves to no account: (short SHA,
    /// author name). Each one blocks.
    pub unlinked: Vec<(String, String)>,
    /// Co-authored-by names that resolve to no account. Listed, never
    /// blocking.
    pub unresolved: Vec<String>,
}

impl Requirement {
    pub fn needs(&self, id: u64) -> bool {
        self.signers.iter().any(|u| u.id == id)
    }
}

/// Accounts that never sign: bots, GitHub's web committer, `allow_ids`, and
/// the maintainers named in `[permissions]`.
pub fn exempt(cfg: &Config, u: &UserRef) -> bool {
    u.is_bot
        || u.id == WEB_FLOW_ID
        || cfg.raw.cla.allow_ids.contains(&u.id)
        || cfg.raw.permissions.maintainers.iter().any(|m| m.id == u.id)
}

pub fn requirement(cfg: &Config, commits: &[CommitFacts]) -> Requirement {
    let mut r = Requirement::default();
    let mut seen = BTreeSet::new();
    let mut need = |r: &mut Requirement, u: &UserRef| {
        if !exempt(cfg, u) && seen.insert(u.id) {
            r.signers.push(u.clone());
        }
    };
    for c in commits {
        match &c.author.account {
            Some(u) => need(&mut r, u),
            None => {
                let short: String = c.sha.chars().take(7).collect();
                r.unlinked.push((short, c.author.name.clone()));
            }
        }
        // A committer whose email is linked to no account does not block:
        // the author is who the commit is attributed to.
        if let Some(u) = &c.committer.account {
            need(&mut r, u);
        }
        for p in &c.co_authors {
            match &p.account {
                Some(u) => need(&mut r, u),
                None => {
                    if !r.unresolved.contains(&p.name) {
                        r.unresolved.push(p.name.clone());
                    }
                }
            }
        }
    }
    r
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Signed,
    Unsigned,
    /// The store could not be read, so nothing can be said yet.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub state: State,
    /// Accounts that must sign and have not.
    pub missing: Vec<UserRef>,
    pub unlinked: Vec<(String, String)>,
    pub unresolved: Vec<String>,
}

/// The verdict for a requirement, given the store and the signatures this
/// evaluation is about to add.
pub fn verdict(req: &Requirement, store: &ClaStore, signing_now: &BTreeSet<u64>) -> Verdict {
    let mut v = Verdict {
        state: State::Signed,
        missing: Vec::new(),
        unlinked: req.unlinked.clone(),
        unresolved: req.unresolved.clone(),
    };
    if !req.signers.is_empty() {
        match store {
            ClaStore::Read(ids) => {
                v.missing = req
                    .signers
                    .iter()
                    .filter(|u| !ids.contains(&u.id) && !signing_now.contains(&u.id))
                    .cloned()
                    .collect();
            }
            ClaStore::Unreadable(e) => {
                v.state = State::Unknown(e.clone());
                return v;
            }
            ClaStore::NotRead => {
                v.state = State::Unknown("the signature store was not read".into());
                return v;
            }
        }
    }
    if !v.missing.is_empty() || !v.unlinked.is_empty() {
        v.state = State::Unsigned;
    }
    v
}

/// Whether a `sign` command carries the configured sentence. The words
/// after `sign` must be the sentence exactly: same case, same punctuation,
/// nothing before or after it. Runs of spaces count as one.
pub fn carries_sentence(cfg: &Config, cmd: &Command) -> bool {
    let want = cfg
        .raw
        .cla
        .sentence
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let got = cmd.text.split_once(' ').map(|(_, rest)| rest).unwrap_or("");
    !want.is_empty() && got == want
}

/// The `cla` check run for a pull request head.
pub fn check_run(cfg: &Config, v: &Verdict, head_sha: &str, slug: &str) -> CheckRun {
    let (conclusion, title) = match &v.state {
        State::Signed => (Some("success"), "CLA signed".to_string()),
        State::Unsigned => (Some("failure"), templates::cla_check_title(v)),
        State::Unknown(_) => (None, "Waiting for the signature store".to_string()),
    };
    CheckRun {
        name: CLA_CHECK.into(),
        head_sha: head_sha.into(),
        conclusion: conclusion.map(String::from),
        title,
        summary: templates::cla_check_summary(cfg, v, slug),
    }
}

/// One pull request of a merge group, with the `cla` conclusion the bot
/// published on the head that was queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMember {
    pub number: u64,
    pub head_sha: String,
    pub cla: Option<CheckState>,
}

/// The `cla` check run for a merge group: success only when every pull
/// request in it has a passing `cla` on its queued head.
pub fn merge_group_check(group_head: &str, members: &[GroupMember]) -> CheckRun {
    let failing: Vec<u64> = members
        .iter()
        .filter(|m| m.cla != Some(CheckState::Success))
        .map(|m| m.number)
        .collect();
    let ok = !members.is_empty() && failing.is_empty();
    CheckRun {
        name: CLA_CHECK.into(),
        head_sha: group_head.into(),
        conclusion: Some(if ok { "success" } else { "failure" }.into()),
        title: if ok {
            "CLA signed for every pull request in the group".into()
        } else if members.is_empty() {
            "Pull requests in the group unknown".into()
        } else {
            "CLA not signed for a pull request in the group".into()
        },
        summary: templates::merge_group_summary(members, &failing),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command;
    use crate::snapshot::Person;

    fn cfg() -> Config {
        Config::parse(
            "[permissions]\nmaintainers = [{ id = 1, login = \"owner\" }]\n\
             [cla]\nenabled = true\nstore = \"o/s\"\npath = \"cla.json\"\n\
             sentence = \"I have read the CLA Document and I hereby sign the CLA\"\nallow_ids = [7]\n",
        )
        .unwrap()
    }

    fn user(id: u64, login: &str) -> UserRef {
        UserRef {
            id,
            login: login.into(),
            is_bot: login.ends_with("[bot]"),
        }
    }

    fn person(name: &str, account: Option<UserRef>) -> Person {
        Person {
            name: name.into(),
            account,
        }
    }

    fn commit(sha: &str, author: Option<UserRef>, committer: Option<UserRef>) -> CommitFacts {
        CommitFacts {
            sha: sha.into(),
            author: person("A", author),
            committer: person("C", committer),
            co_authors: Vec::new(),
        }
    }

    #[test]
    fn authors_and_committers_sign_once_each() {
        let c = cfg();
        let r = requirement(
            &c,
            &[
                commit("aaaaaaaaa", Some(user(10, "ann")), Some(user(11, "cal"))),
                commit("bbbbbbbbb", Some(user(10, "ann")), Some(user(10, "ann"))),
            ],
        );
        assert_eq!(
            r.signers.iter().map(|u| u.id).collect::<Vec<_>>(),
            vec![10, 11]
        );
        assert!(r.unlinked.is_empty());
    }

    #[test]
    fn bots_web_flow_allow_ids_and_maintainers_are_exempt() {
        let c = cfg();
        let r = requirement(
            &c,
            &[
                commit(
                    "a1",
                    Some(user(2, "dependabot[bot]")),
                    Some(user(WEB_FLOW_ID, "web-flow")),
                ),
                commit("a2", Some(user(1, "owner")), Some(user(7, "allowed"))),
            ],
        );
        assert!(r.signers.is_empty(), "{r:?}");
    }

    #[test]
    fn an_unlinked_author_blocks_and_an_unlinked_committer_does_not() {
        let c = cfg();
        let r = requirement(&c, &[commit("abcdef0123", None, None)]);
        assert_eq!(r.unlinked, vec![("abcdef0".into(), "A".into())]);
        let v = verdict(&r, &ClaStore::Read(BTreeSet::new()), &BTreeSet::new());
        assert_eq!(v.state, State::Unsigned);

        let r = requirement(&c, &[commit("abcdef0123", Some(user(10, "ann")), None)]);
        assert!(r.unlinked.is_empty());
        let v = verdict(&r, &ClaStore::Read([10].into()), &BTreeSet::new());
        assert_eq!(v.state, State::Signed);
    }

    #[test]
    fn linked_trailers_sign_and_unlinked_ones_are_listed() {
        let c = cfg();
        let mut k = commit("a1", Some(user(10, "ann")), None);
        k.co_authors = vec![
            person("Bea", Some(user(12, "bea"))),
            person("Claude", None),
            person("Claude", None),
        ];
        let r = requirement(&c, &[k]);
        assert_eq!(
            r.signers.iter().map(|u| u.id).collect::<Vec<_>>(),
            vec![10, 12]
        );
        assert_eq!(r.unresolved, vec!["Claude".to_string()]);
        let v = verdict(&r, &ClaStore::Read([10].into()), &BTreeSet::new());
        assert_eq!(v.state, State::Unsigned);
        assert_eq!(v.missing, vec![user(12, "bea")]);
        let v = verdict(&r, &ClaStore::Read([10].into()), &[12].into());
        assert_eq!(v.state, State::Signed);
        assert_eq!(v.unresolved, vec!["Claude".to_string()]);
    }

    #[test]
    fn an_unreadable_store_waits_unless_nobody_must_sign() {
        let c = cfg();
        let r = requirement(&c, &[commit("a1", Some(user(10, "ann")), None)]);
        let v = verdict(&r, &ClaStore::Unreadable("503".into()), &BTreeSet::new());
        assert_eq!(v.state, State::Unknown("503".into()));
        assert_eq!(check_run(&c, &v, "h", "bot").conclusion, None);

        let r = requirement(&c, &[commit("a1", Some(user(1, "owner")), None)]);
        let v = verdict(&r, &ClaStore::Unreadable("503".into()), &BTreeSet::new());
        assert_eq!(v.state, State::Signed);
    }

    #[test]
    fn the_sentence_must_match_exactly() {
        let c = cfg();
        let ok = |body: &str| {
            let cmds = command::parse(body, "lumenfx-bot");
            carries_sentence(&c, &cmds[0])
        };
        assert!(ok(
            "@lumenfx-bot sign I have read the CLA Document and I hereby sign the CLA"
        ));
        assert!(ok(
            "@lumenfx-bot SIGN   I have read the CLA Document and  I hereby sign the CLA  "
        ));
        assert!(!ok(
            "@lumenfx-bot sign I have read the CLA Document and I hereby sign the CLA."
        ));
        assert!(!ok(
            "@lumenfx-bot sign i have read the cla document and i hereby sign the cla"
        ));
        assert!(!ok(
            "@lumenfx-bot sign \"I have read the CLA Document and I hereby sign the CLA\""
        ));
        assert!(!ok("@lumenfx-bot sign"));
        assert!(!ok(
            "@lumenfx-bot sign I have read the CLA Document and I hereby sign the CLA for ann"
        ));
        // The sentence on the next line is a separate line, not the command's.
        assert!(!ok(
            "@lumenfx-bot sign\nI have read the CLA Document and I hereby sign the CLA"
        ));
    }

    #[test]
    fn check_runs_follow_the_verdict() {
        let c = cfg();
        let r = requirement(&c, &[commit("a1", Some(user(10, "ann")), None)]);
        let v = verdict(&r, &ClaStore::Read(BTreeSet::new()), &BTreeSet::new());
        let run = check_run(&c, &v, "h", "lumenfx-bot");
        assert_eq!(run.conclusion.as_deref(), Some("failure"));
        assert!(run.summary.contains(
            "`@lumenfx-bot sign I have read the CLA Document and I hereby sign the CLA`"
        ));
        assert!(run.summary.contains("`ann`"));
        let v = verdict(&r, &ClaStore::Read([10].into()), &BTreeSet::new());
        assert_eq!(
            check_run(&c, &v, "h", "lumenfx-bot").conclusion.as_deref(),
            Some("success")
        );
    }

    #[test]
    fn a_merge_group_passes_only_when_every_member_passes() {
        let m = |n: u64, cla: Option<CheckState>| GroupMember {
            number: n,
            head_sha: format!("h{n}"),
            cla,
        };
        let pass = merge_group_check("g", &[m(1, Some(CheckState::Success))]);
        assert_eq!(pass.conclusion.as_deref(), Some("success"));
        assert_eq!(pass.head_sha, "g");
        let fail = merge_group_check(
            "g",
            &[
                m(1, Some(CheckState::Success)),
                m(2, Some(CheckState::Failure)),
                m(3, None),
            ],
        );
        assert_eq!(fail.conclusion.as_deref(), Some("failure"));
        assert!(fail.summary.contains("#1: signed"));
        assert!(fail.summary.contains("#2: not signed"));
        assert!(fail.summary.contains("#3: no `cla` check"));
        let empty = merge_group_check("g", &[]);
        assert_eq!(empty.conclusion.as_deref(), Some("failure"));
    }
}
