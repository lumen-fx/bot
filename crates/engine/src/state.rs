//! The pull request state machine: which bot-owned S- role a pull request
//! carries, decided from its whole current state.
//!
//! Inputs are booleans and small enums so the whole input space can be
//! enumerated in tests. The first matching rule wins:
//!
//! ```text
//! 0. closed or merged                         -> no change
//! 1. a maintainer-owned S- label is present   -> no S- change
//! 2. Pullsift hold                            -> no S- change, nothing else either
//! 3. draft                                    -> author
//! 4. any author-side blocker                  -> author
//! 5. no blocker, required checks ok / known flake / infra / held, CLA ok,
//!    changes-requested addressed, no open concern
//!                                             -> review
//! 6. otherwise                                -> keep the current role; if none, triage
//! ```

use serde::{Deserialize, Serialize};

/// The S- roles the bot owns. Every other S- label is a maintainer's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    Triage,
    Author,
    Review,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Triage, Role::Author, Role::Review];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Lifecycle {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cla {
    /// Signed, exempt, or the repository is not gated.
    Ok,
    Unsigned,
    /// The verdict has not been reported yet.
    Pending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Conflict {
    Clean,
    Dirty,
    /// GitHub is still computing mergeability.
    Unknown,
}

/// The class of one required check on the head commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckClass {
    Ok,
    /// Failed, and not a known flake or infrastructure.
    RedOwn,
    RedKnownFlake,
    RedInfra,
    Pending,
    /// A fork run waits for a maintainer to approve it.
    Held,
    /// Waiting on CodeQL.
    Gated,
}

impl CheckClass {
    pub const ALL: [CheckClass; 7] = [
        CheckClass::Ok,
        CheckClass::RedOwn,
        CheckClass::RedKnownFlake,
        CheckClass::RedInfra,
        CheckClass::Pending,
        CheckClass::Held,
        CheckClass::Gated,
    ];

    fn bit(self) -> u8 {
        1 << (self as u8)
    }

    /// Classes that let a pull request reach review (rule 5).
    fn reviewable(self) -> bool {
        matches!(
            self,
            CheckClass::Ok | CheckClass::RedKnownFlake | CheckClass::RedInfra | CheckClass::Held
        )
    }
}

/// Which check classes are present among the required checks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckSet(u8);

impl CheckSet {
    pub fn from_bits(bits: u8) -> Self {
        CheckSet(bits & 0x7f)
    }
    pub fn insert(&mut self, c: CheckClass) {
        self.0 |= c.bit();
    }
    pub fn contains(self, c: CheckClass) -> bool {
        self.0 & c.bit() != 0
    }
    pub fn all_reviewable(self) -> bool {
        CheckClass::ALL
            .iter()
            .all(|c| !self.contains(*c) || c.reviewable())
    }
}

impl FromIterator<CheckClass> for CheckSet {
    fn from_iter<I: IntoIterator<Item = CheckClass>>(iter: I) -> Self {
        let mut s = CheckSet::default();
        for c in iter {
            s.insert(c);
        }
        s
    }
}

/// The set of bot-owned role labels currently on the pull request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleSet(u8);

impl RoleSet {
    pub fn from_bits(bits: u8) -> Self {
        RoleSet(bits & 0x7)
    }
    pub fn insert(&mut self, r: Role) {
        self.0 |= 1 << (r as u8);
    }
    pub fn contains(self, r: Role) -> bool {
        self.0 & (1 << (r as u8)) != 0
    }
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
    /// The role to keep under rule 6 when several are present: the first in
    /// triage, author, review order.
    pub fn first(self) -> Option<Role> {
        Role::ALL.into_iter().find(|r| self.contains(*r))
    }
}

/// An author-side blocker (9.3): the author is the one who must act.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Blocker {
    ClaUnsigned,
    Conflict,
    MergeCommits,
    RedCheck,
    MigrationNote,
    ChangesRequested,
    Ejected,
    AuthorCommand,
}

/// Everything rule 0 to 6 look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inputs {
    pub lifecycle: Lifecycle,
    pub draft: bool,
    pub maintainer_label: bool,
    pub pullsift_hold: bool,
    pub cla: Cla,
    pub conflict: Conflict,
    pub merge_commits: bool,
    pub checks: CheckSet,
    pub migration_mismatch: bool,
    pub changes_requested: bool,
    pub ejected: bool,
    pub author_command: bool,
    pub open_concern: bool,
    pub current: RoleSet,
}

impl Inputs {
    pub fn blockers(&self) -> Vec<Blocker> {
        let mut b = Vec::new();
        if self.cla == Cla::Unsigned {
            b.push(Blocker::ClaUnsigned);
        }
        if self.conflict == Conflict::Dirty {
            b.push(Blocker::Conflict);
        }
        if self.merge_commits {
            b.push(Blocker::MergeCommits);
        }
        if self.checks.contains(CheckClass::RedOwn) {
            b.push(Blocker::RedCheck);
        }
        if self.migration_mismatch {
            b.push(Blocker::MigrationNote);
        }
        if self.changes_requested {
            b.push(Blocker::ChangesRequested);
        }
        if self.ejected {
            b.push(Blocker::Ejected);
        }
        if self.author_command {
            b.push(Blocker::AuthorCommand);
        }
        b
    }
}

/// What to do with the bot-owned S- labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "action", content = "role")]
pub enum RoleAction {
    /// Leave every S- label as it is.
    Unchanged,
    /// Exactly this role, and no other bot-owned role.
    Set(Role),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    /// The rule (0 to 6) that matched.
    pub rule: u8,
    pub action: RoleAction,
    pub blockers: Vec<Blocker>,
}

pub fn decide(i: &Inputs) -> Decision {
    let blockers = i.blockers();
    let (rule, action) = if i.lifecycle != Lifecycle::Open {
        (0, RoleAction::Unchanged)
    } else if i.maintainer_label {
        (1, RoleAction::Unchanged)
    } else if i.pullsift_hold {
        (2, RoleAction::Unchanged)
    } else if i.draft {
        (3, RoleAction::Set(Role::Author))
    } else if !blockers.is_empty() {
        (4, RoleAction::Set(Role::Author))
    } else if i.checks.all_reviewable() && i.cla == Cla::Ok && !i.open_concern {
        // No blocker means changes requested are addressed and the CLA is
        // not unsigned; Pending is ruled out here.
        (5, RoleAction::Set(Role::Review))
    } else {
        (
            6,
            RoleAction::Set(i.current.first().unwrap_or(Role::Triage)),
        )
    };
    Decision {
        rule,
        action,
        blockers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Inputs {
        Inputs {
            lifecycle: Lifecycle::Open,
            draft: false,
            maintainer_label: false,
            pullsift_hold: false,
            cla: Cla::Ok,
            conflict: Conflict::Clean,
            merge_commits: false,
            checks: [CheckClass::Ok].into_iter().collect(),
            migration_mismatch: false,
            changes_requested: false,
            ejected: false,
            author_command: false,
            open_concern: false,
            current: RoleSet::default(),
        }
    }

    fn roles(rs: &[Role]) -> RoleSet {
        let mut s = RoleSet::default();
        for r in rs {
            s.insert(*r);
        }
        s
    }

    fn checks(cs: &[CheckClass]) -> CheckSet {
        cs.iter().copied().collect()
    }

    /// Named rows: one per rule and per notable case in section 9.
    #[test]
    fn table() {
        use CheckClass::*;
        use RoleAction::*;
        let rows: Vec<(&str, Inputs, u8, RoleAction)> = vec![
            ("all green", base(), 5, Set(Role::Review)),
            (
                "merged",
                Inputs {
                    lifecycle: Lifecycle::Merged,
                    ..base()
                },
                0,
                Unchanged,
            ),
            (
                "closed with a red check",
                Inputs {
                    lifecycle: Lifecycle::Closed,
                    checks: checks(&[RedOwn]),
                    ..base()
                },
                0,
                Unchanged,
            ),
            (
                "maintainer label beats red",
                Inputs {
                    maintainer_label: true,
                    checks: checks(&[RedOwn]),
                    ..base()
                },
                1,
                Unchanged,
            ),
            (
                "maintainer label beats pullsift",
                Inputs {
                    maintainer_label: true,
                    pullsift_hold: true,
                    ..base()
                },
                1,
                Unchanged,
            ),
            (
                "pullsift hold beats draft",
                Inputs {
                    pullsift_hold: true,
                    draft: true,
                    ..base()
                },
                2,
                Unchanged,
            ),
            (
                "draft, all green",
                Inputs {
                    draft: true,
                    ..base()
                },
                3,
                Set(Role::Author),
            ),
            (
                "draft, checks pending",
                Inputs {
                    draft: true,
                    checks: checks(&[Pending]),
                    ..base()
                },
                3,
                Set(Role::Author),
            ),
            (
                "red own check",
                Inputs {
                    checks: checks(&[Ok, RedOwn]),
                    ..base()
                },
                4,
                Set(Role::Author),
            ),
            (
                "red own while others pending",
                Inputs {
                    checks: checks(&[RedOwn, Pending]),
                    ..base()
                },
                4,
                Set(Role::Author),
            ),
            (
                "cla unsigned",
                Inputs {
                    cla: Cla::Unsigned,
                    ..base()
                },
                4,
                Set(Role::Author),
            ),
            (
                "conflict",
                Inputs {
                    conflict: Conflict::Dirty,
                    ..base()
                },
                4,
                Set(Role::Author),
            ),
            (
                "merge commits",
                Inputs {
                    merge_commits: true,
                    ..base()
                },
                4,
                Set(Role::Author),
            ),
            (
                "migration mismatch",
                Inputs {
                    migration_mismatch: true,
                    ..base()
                },
                4,
                Set(Role::Author),
            ),
            (
                "changes requested",
                Inputs {
                    changes_requested: true,
                    ..base()
                },
                4,
                Set(Role::Author),
            ),
            (
                "ejected",
                Inputs {
                    ejected: true,
                    ..base()
                },
                4,
                Set(Role::Author),
            ),
            (
                "author command",
                Inputs {
                    author_command: true,
                    ..base()
                },
                4,
                Set(Role::Author),
            ),
            (
                "known flake is not the author's",
                Inputs {
                    checks: checks(&[Ok, RedKnownFlake]),
                    ..base()
                },
                5,
                Set(Role::Review),
            ),
            (
                "infra is not the author's",
                Inputs {
                    checks: checks(&[RedInfra]),
                    ..base()
                },
                5,
                Set(Role::Review),
            ),
            (
                "held run waits on a maintainer",
                Inputs {
                    checks: checks(&[Ok, Held]),
                    ..base()
                },
                5,
                Set(Role::Review),
            ),
            (
                "no required checks",
                Inputs {
                    checks: CheckSet::default(),
                    ..base()
                },
                5,
                Set(Role::Review),
            ),
            (
                "pending, no label yet",
                Inputs {
                    checks: checks(&[Ok, Pending]),
                    ..base()
                },
                6,
                Set(Role::Triage),
            ),
            (
                "pending keeps review",
                Inputs {
                    checks: checks(&[Pending]),
                    current: roles(&[Role::Review]),
                    ..base()
                },
                6,
                Set(Role::Review),
            ),
            (
                "pending keeps author",
                Inputs {
                    checks: checks(&[Pending]),
                    current: roles(&[Role::Author]),
                    ..base()
                },
                6,
                Set(Role::Author),
            ),
            (
                "gated keeps label",
                Inputs {
                    checks: checks(&[Gated]),
                    current: roles(&[Role::Author]),
                    ..base()
                },
                6,
                Set(Role::Author),
            ),
            (
                "cla pending",
                Inputs {
                    cla: Cla::Pending,
                    ..base()
                },
                6,
                Set(Role::Triage),
            ),
            (
                "open concern keeps the label",
                Inputs {
                    open_concern: true,
                    current: roles(&[Role::Triage]),
                    ..base()
                },
                6,
                Set(Role::Triage),
            ),
            (
                "open concern with no label",
                Inputs {
                    open_concern: true,
                    ..base()
                },
                6,
                Set(Role::Triage),
            ),
            (
                "open concern is not an author blocker",
                Inputs {
                    open_concern: true,
                    current: roles(&[Role::Review]),
                    ..base()
                },
                6,
                Set(Role::Review),
            ),
            (
                "two roles, pending: first kept",
                Inputs {
                    checks: checks(&[Pending]),
                    current: roles(&[Role::Author, Role::Review]),
                    ..base()
                },
                6,
                Set(Role::Author),
            ),
        ];
        for (name, inputs, rule, action) in rows {
            let d = decide(&inputs);
            assert_eq!((d.rule, d.action), (rule, action), "{name}");
        }
    }

    /// The whole input space. Checks each decision against the rules written
    /// out a second time, and against the invariants of section 20.
    #[test]
    fn exhaustive() {
        let lifecycles = [Lifecycle::Open, Lifecycle::Closed, Lifecycle::Merged];
        let clas = [Cla::Ok, Cla::Unsigned, Cla::Pending];
        let conflicts = [Conflict::Clean, Conflict::Dirty, Conflict::Unknown];
        let mut count = 0u64;
        for lifecycle in lifecycles {
            for cla in clas {
                for conflict in conflicts {
                    for checks in 0u8..128 {
                        for current in 0u8..8 {
                            for flags in 0u16..(1 << 9) {
                                let f = |n: u16| flags & (1 << n) != 0;
                                let i = Inputs {
                                    lifecycle,
                                    draft: f(0),
                                    maintainer_label: f(1),
                                    pullsift_hold: f(2),
                                    cla,
                                    conflict,
                                    merge_commits: f(3),
                                    checks: CheckSet::from_bits(checks),
                                    migration_mismatch: f(4),
                                    changes_requested: f(5),
                                    ejected: f(6),
                                    author_command: f(7),
                                    open_concern: f(8),
                                    current: RoleSet::from_bits(current),
                                };
                                check_one(&i);
                                count += 1;
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(count, 3 * 3 * 3 * 128 * 8 * 512);
    }

    fn check_one(i: &Inputs) {
        let d = decide(i);
        let blocked = i.cla == Cla::Unsigned
            || i.conflict == Conflict::Dirty
            || i.merge_commits
            || i.checks.contains(CheckClass::RedOwn)
            || i.migration_mismatch
            || i.changes_requested
            || i.ejected
            || i.author_command;
        let expected = if i.lifecycle != Lifecycle::Open || i.maintainer_label || i.pullsift_hold {
            RoleAction::Unchanged
        } else if i.draft || blocked {
            RoleAction::Set(Role::Author)
        } else if !i.checks.contains(CheckClass::Pending)
            && !i.checks.contains(CheckClass::Gated)
            && i.cla == Cla::Ok
            && !i.open_concern
        {
            RoleAction::Set(Role::Review)
        } else if i.current.contains(Role::Triage) {
            RoleAction::Set(Role::Triage)
        } else if i.current.contains(Role::Author) {
            RoleAction::Set(Role::Author)
        } else if i.current.contains(Role::Review) {
            RoleAction::Set(Role::Review)
        } else {
            RoleAction::Set(Role::Triage)
        };
        assert_eq!(d.action, expected, "{i:?}");

        // Maintainer-owned labels are never overridden.
        if i.maintainer_label {
            assert_eq!(d.action, RoleAction::Unchanged, "{i:?}");
        }
        // A draft never sits in review.
        if i.draft {
            assert_ne!(d.action, RoleAction::Set(Role::Review), "{i:?}");
        }
        // A known flake or infra failure is never charged to the author: with
        // no blocker and no draft, an author role can only be one rule 6 kept
        // while checks are pending.
        if !blocked && !i.draft && d.action == RoleAction::Set(Role::Author) {
            assert!(
                d.rule == 6 && i.current.first() == Some(Role::Author),
                "{i:?}"
            );
        }
        // The blockers are exactly the author-side conditions.
        assert_eq!(!d.blockers.is_empty(), blocked, "{i:?}");
    }
}
