//! `evaluate(snapshot) -> Plan`: the whole decision, with no I/O.
//!
//! The event that woke the bot only says which thread to look at; this
//! function decides from the thread's entire current state, so a missed or
//! reordered webhook heals on the next one.

use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Utc};

use crate::command::{self, Arg, Command};
use crate::config::Config;
use crate::plan::{CheckRun, Job, LabelChange, NewComment, Plan, Reaction, StatusComment};
use crate::snapshot::{Comment, EntityState, Item, PullRequest, ReviewState, Snapshot};
use crate::state::{
    self, Blocker, CheckClass, CheckSet, Cla, Conflict, Decision, Inputs, Lifecycle, Role,
    RoleAction, RoleSet,
};
use crate::templates::{self, STATUS_MARKER, StatusView, Welcome, code};

/// New comments the bot may post on one thread in 24 hours.
pub const DAILY_COMMENT_CAP: u32 = 3;
/// Delay before looking again while GitHub computes mergeability.
pub const MERGEABLE_RESCAN_SECS: i64 = 60;

pub const DOCS_CHECK: &str = "docs";
pub const CONFIG_CHECK: &str = "lumenfx config";

pub fn evaluate(s: &Snapshot) -> Plan {
    let mut ev = Eval {
        s,
        cfg: s.config.effective(),
        plan: Plan {
            state: s.store.entity.clone(),
            ..Plan::default()
        },
        labels: current_labels(s),
        replies: Vec::new(),
        notices: Vec::new(),
        new_comments_budget: DAILY_COMMENT_CAP.saturating_sub(s.store.comments_last_day),
    };
    if let crate::snapshot::ConfigState::Invalid { errors, last_good } = &s.config {
        ev.plan.notes.push(format!(
            "config on the default branch does not validate ({} errors); {}",
            errors.len(),
            if last_good.is_some() {
                "following the last valid version"
            } else {
                "no valid version known, answering help only"
            }
        ));
    }
    ev.run_commands();
    if ev.cfg.is_some() {
        match &s.item {
            Item::Pr(pr) => ev.pull_request(pr),
            Item::Issue(_) => ev.issue(),
        }
    } else {
        ev.plan
            .notes
            .push("no usable config: only help is answered".into());
    }
    ev.finish()
}

fn current_labels(s: &Snapshot) -> BTreeSet<String> {
    match &s.item {
        Item::Pr(pr) => pr.labels.clone(),
        Item::Issue(i) => i.labels.clone(),
    }
}

fn is_open(s: &Snapshot) -> bool {
    match &s.item {
        Item::Pr(pr) => pr.lifecycle == Lifecycle::Open,
        Item::Issue(i) => i.open,
    }
}

struct Eval<'a> {
    s: &'a Snapshot,
    cfg: Option<&'a Config>,
    plan: Plan,
    /// The labels as they will be once the plan is applied.
    labels: BTreeSet<String>,
    /// Replies to command comments, posted after the status comment.
    replies: Vec<NewComment>,
    /// Conflict and nudge comments, posted after replies.
    notices: Vec<NewComment>,
    new_comments_budget: u32,
}

enum Outcome {
    /// Done; acknowledged with a reaction.
    Ack,
    /// Output or an error, sent as a reply.
    Reply(String),
}

impl<'a> Eval<'a> {
    fn is_bot_comment(&self, c: &Comment) -> bool {
        c.user.is_bot || c.user.login.eq_ignore_ascii_case(&self.s.bot.login)
    }

    fn is_maintainer(&self, user_id: u64) -> bool {
        let Some(cfg) = self.cfg else { return false };
        let facts = self.s.users.get(&user_id);
        if facts.is_some_and(|f| f.is_bot) {
            return false;
        }
        cfg.raw
            .permissions
            .maintainers
            .iter()
            .any(|m| m.id == user_id)
            || (cfg.raw.permissions.write_access_is_maintainer
                && facts.is_some_and(|f| f.write_access))
    }

    fn author_id(&self) -> u64 {
        match &self.s.item {
            Item::Pr(pr) => pr.author.id,
            Item::Issue(i) => i.author.id,
        }
    }

    fn is_pr(&self) -> bool {
        matches!(self.s.item, Item::Pr(_))
    }

    fn set_label(&mut self, label: &str, present: bool, rule: &str) {
        let changed = if present {
            self.labels.insert(label.to_string())
        } else {
            self.labels.remove(label)
        };
        if changed {
            self.plan.labels.retain(|c| c.label != label);
            let was = current_labels(self.s).contains(label);
            if was != present {
                self.plan.labels.push(LabelChange {
                    label: label.to_string(),
                    present,
                    rule: rule.to_string(),
                });
            }
        }
    }

    // ---- commands ---------------------------------------------------------

    fn run_commands(&mut self) {
        let mut comments: Vec<&Comment> = self.s.comments.iter().collect();
        comments.sort_by_key(|c| (c.created_at, c.id));
        for c in comments {
            if self.is_bot_comment(c) {
                continue;
            }
            let cmds = command::parse(&c.body, &self.s.bot.slug);
            let mut outputs = Vec::new();
            let mut acked = false;
            let mut new_keys = Vec::new();
            for cmd in cmds {
                let key = (c.id, cmd.key());
                if self.plan.state.processed.contains(&key) {
                    continue;
                }
                self.plan.state.processed.insert(key.clone());
                new_keys.push(key.1);
                if self.cfg.is_none() && cmd.name != "help" {
                    self.plan.notes.push(format!(
                        "ignored `{}` from comment {}: no config",
                        cmd.name, c.id
                    ));
                    continue;
                }
                if !is_open(self.s) {
                    self.plan.notes.push(format!(
                        "ignored `{}` from comment {}: closed",
                        cmd.name, c.id
                    ));
                    continue;
                }
                match self.command(c, &cmd) {
                    Outcome::Ack => acked = true,
                    Outcome::Reply(text) => outputs.push(text),
                }
                self.plan.notes.push(format!(
                    "ran `{}` from comment {} by {}",
                    cmd.name, c.id, c.user.login
                ));
            }
            if acked && outputs.is_empty() {
                self.plan.reactions.push(Reaction {
                    comment_id: c.id,
                    content: "+1".into(),
                });
            }
            if !outputs.is_empty() {
                self.replies.push(NewComment {
                    purpose: "reply".into(),
                    key: format!("{}:{}", c.id, short(&new_keys.join(","))),
                    body: outputs.join("\n\n"),
                });
            }
        }
    }

    fn command(&mut self, c: &Comment, cmd: &Command) -> Outcome {
        let slug = self.s.bot.slug.clone();
        let maintainer = self.is_maintainer(c.user.id);
        let author = c.user.id == self.author_id();
        let deny = |who: &str| {
            Outcome::Reply(format!(
                "`{}`: only {who} can run this.",
                templates::clean(&cmd.text)
            ))
        };
        let pr_only = || {
            Outcome::Reply(format!(
                "`{}` works on pull requests only.",
                templates::clean(&cmd.name)
            ))
        };
        match cmd.name.as_str() {
            "help" => Outcome::Reply(templates::help_reply(&slug, &self.s.bot.public_url)),
            "ready" | "review" => {
                if !self.is_pr() {
                    return pr_only();
                }
                if !(author || maintainer) {
                    return deny("the author or a maintainer");
                }
                self.plan.state.ready_at = Some(c.created_at);
                Outcome::Ack
            }
            "author" => {
                if !self.is_pr() {
                    return pr_only();
                }
                if !maintainer {
                    return deny("a maintainer");
                }
                self.plan.state.author_forced_at = Some(c.created_at);
                Outcome::Ack
            }
            "blocked" => {
                if !maintainer {
                    return deny("a maintainer");
                }
                let by = cmd.args.iter().find_map(|a| match a {
                    Arg::Issue(n) => Some(*n),
                    _ => None,
                });
                if by == Some(self.s.number) {
                    return Outcome::Reply("`blocked`: a thread cannot block itself.".into());
                }
                let label = self
                    .cfg
                    .map(|c| c.labels().blocked.clone())
                    .unwrap_or_default();
                self.set_label(&label, true, "command blocked");
                self.plan.state.blocked = true;
                self.plan.state.blocked_by = by;
                Outcome::Ack
            }
            "unblock" => {
                if !maintainer {
                    return deny("a maintainer");
                }
                let label = self
                    .cfg
                    .map(|c| c.labels().blocked.clone())
                    .unwrap_or_default();
                self.set_label(&label, false, "command unblock");
                self.plan.state.blocked = false;
                self.plan.state.blocked_by = None;
                Outcome::Ack
            }
            "label" => self.label_command(cmd, maintainer),
            "concern" => {
                if !self.is_pr() {
                    return pr_only();
                }
                if !maintainer {
                    return deny("a maintainer");
                }
                let name = words(cmd);
                if name.is_empty() {
                    return Outcome::Reply("`concern` needs a name: `concern NAME`.".into());
                }
                self.plan.state.concerns.insert(
                    name,
                    crate::snapshot::Concern {
                        by: c.user.login.clone(),
                        at: c.created_at,
                    },
                );
                Outcome::Ack
            }
            "resolve" => {
                if !self.is_pr() {
                    return pr_only();
                }
                if !maintainer {
                    return deny("a maintainer");
                }
                let name = words(cmd);
                if self.plan.state.concerns.remove(&name).is_none() {
                    return Outcome::Reply(format!(
                        "`resolve`: there is no open concern {}.",
                        code(&name)
                    ));
                }
                Outcome::Ack
            }
            "no-docs-needed" => {
                if !self.is_pr() {
                    return pr_only();
                }
                if !maintainer {
                    return deny("a maintainer");
                }
                self.plan.state.no_docs_needed = true;
                Outcome::Ack
            }
            other if templates::NOT_YET.contains(&other) => Outcome::Reply(format!(
                "{} is not available yet. `@{slug} help` lists the commands that are.",
                code(other)
            )),
            other => Outcome::Reply(format!(
                "Unknown command {}. `@{slug} help` lists the commands.",
                code(other)
            )),
        }
    }

    fn label_command(&mut self, cmd: &Command, maintainer: bool) -> Outcome {
        let Some(cfg) = self.cfg else {
            return Outcome::Ack;
        };
        let ops: Vec<(bool, String)> = cmd
            .args
            .iter()
            .filter_map(|a| match a {
                Arg::Label { add, name } => Some((*add, name.clone())),
                _ => None,
            })
            .collect();
        if ops.is_empty() {
            return Outcome::Reply("`label` needs at least one `+NAME` or `-NAME`.".into());
        }
        let mut errors = Vec::new();
        for (add, name) in ops {
            if !maintainer && !cfg.is_self_service_label(&name) {
                errors.push(format!("only a maintainer can change {}", code(&name)));
                continue;
            }
            if add
                && let Some(existing) = &self.s.repo_labels
                && !existing.contains(&name)
            {
                errors.push(format!(
                    "there is no label {} in this repository",
                    code(&name)
                ));
                continue;
            }
            self.set_label(&name, add, "command label");
        }
        if errors.is_empty() {
            Outcome::Ack
        } else {
            Outcome::Reply(format!("`label`: {}.", errors.join("; ")))
        }
    }

    // ---- issues -----------------------------------------------------------

    fn issue(&mut self) {
        // Issue triage is a later milestone; issues get commands only.
        self.clear_finished_block();
    }

    fn clear_finished_block(&mut self) {
        if let (Some(n), Some(true)) = (self.plan.state.blocked_by, self.s.blocked_target_closed) {
            if self.plan.state.blocked
                && let Some(cfg) = self.cfg
            {
                let label = cfg.labels().blocked.clone();
                self.set_label(&label, false, "blocking issue closed");
            }
            self.plan.state.blocked = false;
            self.plan.state.blocked_by = None;
            self.plan.notes.push(format!("#{n} closed; block lifted"));
        }
    }

    // ---- pull requests ----------------------------------------------------

    fn pull_request(&mut self, pr: &PullRequest) {
        let cfg = self.cfg.expect("checked by caller");
        if pr.lifecycle != Lifecycle::Open {
            self.plan.notes.push("closed: nothing to do".into());
            return;
        }
        self.clear_finished_block();

        let roles = cfg.labels();
        let mut current = RoleSet::default();
        for r in Role::ALL {
            if self.labels.contains(role_label(cfg, r)) {
                current.insert(r);
            }
        }
        let cla = if !cfg.raw.cla.enabled || pr.author.is_bot {
            Cla::Ok
        } else {
            match pr.cla_check {
                Some(crate::snapshot::CheckState::Success) => Cla::Ok,
                Some(crate::snapshot::CheckState::Failure) => Cla::Unsigned,
                _ => Cla::Pending,
            }
        };
        let conflict = match pr.mergeable_state.as_deref() {
            Some("dirty") => Conflict::Dirty,
            None | Some("unknown") => Conflict::Unknown,
            Some(_) => Conflict::Clean,
        };
        let checks: CheckSet = pr.checks.iter().map(|c| c.class).collect();
        let (changes_requested, cr_by) = self.changes_requested(pr);
        let st = &self.plan.state;
        let author_command = st.author_forced_at.is_some_and(|t| {
            !(pr.last_push_at.is_some_and(|p| p > t) || st.ready_at.is_some_and(|r| r > t))
        });
        let inputs = Inputs {
            lifecycle: pr.lifecycle,
            draft: pr.draft,
            maintainer_label: roles
                .maintainer_owned
                .iter()
                .any(|l| self.labels.contains(l)),
            pullsift_hold: pr.pullsift.holds(),
            cla,
            conflict,
            merge_commits: cfg.raw.no_merges.enabled && !pr.merge_commits.is_empty(),
            checks,
            // Migration notes and queue ejections are later milestones.
            migration_mismatch: false,
            changes_requested,
            ejected: false,
            author_command,
            open_concern: !st.concerns.is_empty(),
            current,
        };
        let decision = state::decide(&inputs);
        self.plan.notes.push(format!(
            "state machine rule {}: {:?}, blockers {:?}",
            decision.rule, decision.action, decision.blockers
        ));
        if let RoleAction::Set(role) = decision.action {
            for r in Role::ALL {
                let rule = format!("9.2 rule {}", decision.rule);
                self.set_label(role_label(cfg, r), r == role, &rule);
            }
        }
        let held = decision.rule == 2;

        if !held {
            self.autolabel(cfg, pr);
        }

        let actionable =
            pr.first_time || !decision.blockers.is_empty() || checks.contains(CheckClass::Held);
        let creating_status = self.status_comment(cfg, pr, &decision, &inputs, &cr_by, actionable);
        self.plan.state.last_actionable = actionable;

        if !held {
            self.conflict_notice(pr, conflict, creating_status);
            self.nudge(cfg, pr, &decision);
            self.check_runs(cfg, pr);
        }
        self.plan.decision = Some(decision);
    }

    /// Changes requested and not addressed (9.3.6), with the reviewers who
    /// asked.
    fn changes_requested(&self, pr: &PullRequest) -> (bool, Vec<String>) {
        let mut latest: std::collections::BTreeMap<u64, (&ReviewState, DateTime<Utc>, &str)> =
            Default::default();
        for r in &pr.reviews {
            if r.user.id == pr.author.id || r.user.is_bot || !self.is_maintainer(r.user.id) {
                continue;
            }
            if !matches!(
                r.state,
                ReviewState::Approved | ReviewState::ChangesRequested | ReviewState::Dismissed
            ) {
                continue;
            }
            let e = latest
                .entry(r.user.id)
                .or_insert((&r.state, r.submitted_at, &r.user.login));
            if r.submitted_at >= e.1 {
                *e = (&r.state, r.submitted_at, &r.user.login);
            }
        }
        let threads_answered = pr.threads.iter().all(|t| {
            let Some(first) = t.comments.first() else {
                return true;
            };
            if t.resolved || !self.is_maintainer(first.user.id) {
                return true;
            }
            let last_maintainer = t
                .comments
                .iter()
                .filter(|c| self.is_maintainer(c.user.id))
                .map(|c| c.created_at)
                .max();
            t.comments.iter().any(|c| {
                c.user.id == pr.author.id && last_maintainer.is_none_or(|m| c.created_at > m)
            })
        });
        let mut by = Vec::new();
        for (state, at, login) in latest.values() {
            if **state != ReviewState::ChangesRequested {
                continue;
            }
            let pushed = pr.last_push_at.is_some_and(|p| p > *at);
            let ready = self.plan.state.ready_at.is_some_and(|r| r > *at);
            if !(ready || (pushed && threads_answered)) {
                by.push(login.to_string());
            }
        }
        (!by.is_empty(), by)
    }

    fn autolabel(&mut self, cfg: &Config, pr: &PullRequest) {
        let exists = |l: &str| self.s.repo_labels.as_ref().is_none_or(|r| r.contains(l));
        if !self.labels.iter().any(|l| l.starts_with("A-")) {
            for l in cfg.area_labels(pr.files.iter().map(String::as_str)) {
                if exists(&l) {
                    self.set_label(&l, true, "autolabel path");
                } else {
                    self.plan
                        .notes
                        .push(format!("autolabel {l} skipped: no such label"));
                }
            }
        }
        if !self.labels.iter().any(|l| l.starts_with("C-"))
            && let Some(l) = cfg.class_label(&pr.title)
        {
            if exists(&l) {
                self.set_label(&l, true, "autolabel title");
            } else {
                self.plan
                    .notes
                    .push(format!("autolabel {l} skipped: no such label"));
            }
        }
    }

    fn existing_status(&self) -> Option<&'a Comment> {
        let mine = |c: &&Comment| c.user.login.eq_ignore_ascii_case(&self.s.bot.login);
        if let Some(id) = self.s.store.status_comment_id
            && let Some(c) = self.s.comments.iter().find(|c| c.id == id)
        {
            return Some(c);
        }
        self.s
            .comments
            .iter()
            .filter(mine)
            .find(|c| c.body.starts_with(STATUS_MARKER))
    }

    /// Plans the status comment; returns whether it is being created now.
    fn status_comment(
        &mut self,
        cfg: &Config,
        pr: &PullRequest,
        d: &Decision,
        i: &Inputs,
        cr_by: &[String],
        actionable: bool,
    ) -> bool {
        use crate::config::StatusCommentMode as Mode;
        let mode = cfg.raw.status_comment.mode;
        let existing = self.existing_status();
        let view = self.status_view(cfg, pr, d, i, cr_by);
        let body = templates::status_comment(&view);
        match existing {
            Some(c) => {
                if mode != Mode::Never && c.body.trim_end() != body.trim_end() {
                    self.plan.status_comment = Some(StatusComment {
                        body,
                        create: false,
                    });
                }
                false
            }
            None => {
                let deleted = self.s.store.status_comment_id.is_some();
                let create = d.rule != 2
                    && match mode {
                        Mode::Never => false,
                        Mode::Always => {
                            !deleted || (actionable && !self.s.store.entity.last_actionable)
                        }
                        Mode::Actionable => {
                            actionable && (!deleted || !self.s.store.entity.last_actionable)
                        }
                    };
                if !create {
                    return false;
                }
                if self.new_comments_budget == 0 {
                    self.plan
                        .notes
                        .push("status comment held back: daily comment cap reached".into());
                    return false;
                }
                self.new_comments_budget -= 1;
                self.plan.status_comment = Some(StatusComment { body, create: true });
                true
            }
        }
    }

    fn status_view(
        &self,
        cfg: &Config,
        pr: &PullRequest,
        d: &Decision,
        i: &Inputs,
        cr_by: &[String],
    ) -> StatusView {
        let slug = self.s.bot.slug.clone();
        let mut v = StatusView {
            slug: slug.clone(),
            base_ref: pr.default_branch.clone(),
            held_by_pullsift: d.rule == 2,
            queued: pr.queued,
            blocked_on: self.plan.state.blocked_by,
            ..Default::default()
        };
        if v.held_by_pullsift {
            return v;
        }
        let shown_role = match d.action {
            RoleAction::Set(r) => Some(r),
            RoleAction::Unchanged => None,
        };
        v.label = match d.action {
            RoleAction::Set(r) => Some(role_label(cfg, r).to_string()),
            RoleAction::Unchanged => cfg
                .labels()
                .maintainer_owned
                .iter()
                .find(|l| self.labels.contains(*l))
                .cloned(),
        };
        v.headline = match (d.rule, shown_role) {
            (1, _) => "set by a maintainer".into(),
            (3, _) => "draft".into(),
            (_, Some(Role::Author)) => "waiting on author".into(),
            (_, Some(Role::Review)) => "waiting on review".into(),
            _ if i.open_concern => "waiting on a maintainer".into(),
            _ => "waiting for checks".into(),
        };
        if pr.first_time {
            v.welcome = Some(Welcome {
                cla_needed: cfg.raw.cla.enabled && i.cla == Cla::Unsigned,
                contributing_url: cfg.raw.status_comment.contributing.as_ref().map(|p| {
                    format!(
                        "https://github.com/{}/blob/{}/{}",
                        self.s.repo, pr.default_branch, p
                    )
                }),
            });
        }
        let base = pr.base_ref.as_str();
        if cfg.raw.cla.enabled {
            match i.cla {
                Cla::Ok => v.items.push((true, "CLA signed".into())),
                Cla::Unsigned => v.items.push((false, templates::item_cla_unsigned(cfg))),
                Cla::Pending => v
                    .items
                    .push((false, "CLA: the check has not reported yet.".into())),
            }
        }
        for b in &d.blockers {
            match b {
                Blocker::ClaUnsigned => {}
                Blocker::Conflict => v.items.push((false, templates::item_conflict(base))),
                Blocker::MergeCommits => v.items.push((
                    false,
                    templates::item_merge_commits(&pr.merge_commits, base),
                )),
                Blocker::RedCheck => {
                    for c in pr.checks.iter().filter(|c| c.class == CheckClass::RedOwn) {
                        v.items
                            .push((false, templates::item_red_check(&c.name, c.url.as_deref())));
                    }
                }
                Blocker::ChangesRequested => v
                    .items
                    .push((false, templates::item_changes_requested(cr_by, &slug))),
                Blocker::AuthorCommand => {
                    v.items.push((false, templates::item_author_command(&slug)))
                }
                Blocker::MigrationNote | Blocker::Ejected => {}
            }
        }
        if i.conflict == Conflict::Clean && !d.blockers.contains(&Blocker::Conflict) {
            v.items
                .push((true, format!("No conflicts with {}", code(base))));
        }
        if cfg.raw.no_merges.enabled && pr.merge_commits.is_empty() {
            v.items.push((true, "No merge commits".into()));
        }
        v.concerns = self
            .plan
            .state
            .concerns
            .iter()
            .map(|(n, c)| (n.clone(), c.by.clone()))
            .collect();
        let total = pr.checks.len();
        let green = pr
            .checks
            .iter()
            .filter(|c| c.class == CheckClass::Ok)
            .count();
        v.checks_line = if total == 0 {
            "Checks: no required checks are configured for this branch.".into()
        } else {
            format!("Checks: {green} of {total} required green.")
        };
        v.held_runs = pr.checks.iter().any(|c| c.class == CheckClass::Held);
        v.coverage = pr.coverage.as_ref().map(|c| {
            let desc = templates::clean(&c.description);
            match &c.url {
                Some(u) => format!(
                    "Coverage: `codecov/patch` {}: [{desc}]({u}).",
                    templates::clean(&c.state)
                ),
                None => format!(
                    "Coverage: `codecov/patch` {}: {desc}.",
                    templates::clean(&c.state)
                ),
            }
        });
        v.pullsift = match (&pr.pullsift.comment, pr.pullsift.flagged) {
            (_, true) => Some("Pullsift: flagged; see its comment.".into()),
            (Some(c), false) if c.passed => {
                Some(format!("Pullsift: [screened]({}), no action.", c.url))
            }
            (Some(c), false) => Some(format!("Pullsift: [scored]({}); see its comment.", c.url)),
            (None, false) => None,
        };
        if self.docs_advisory(cfg, pr) {
            v.docs = Some(templates::docs_line());
        }
        v
    }

    fn docs_advisory(&self, cfg: &Config, pr: &PullRequest) -> bool {
        if self.plan.state.no_docs_needed {
            return false;
        }
        if cfg
            .raw
            .docs
            .quiet_classes
            .iter()
            .any(|q| self.labels.contains(q))
        {
            return false;
        }
        let code_changed = pr.files.iter().any(|f| cfg.is_code_path(f));
        let docs_changed = pr.files.iter().any(|f| cfg.is_docs_path(f));
        code_changed && !docs_changed
    }

    fn conflict_notice(&mut self, pr: &PullRequest, conflict: Conflict, creating_status: bool) {
        match conflict {
            Conflict::Clean => self.plan.state.conflict_notified = false,
            Conflict::Unknown => self.plan.jobs.push(Job {
                reason: "mergeable-rescan".into(),
                at: self.s.now + Duration::seconds(MERGEABLE_RESCAN_SECS),
            }),
            Conflict::Dirty if !self.plan.state.conflict_notified => {
                self.plan.state.conflict_notified = true;
                if creating_status {
                    self.plan
                        .notes
                        .push("conflict: announced by the new status comment".into());
                } else {
                    self.post(NewComment {
                        purpose: "conflict".into(),
                        key: format!("{}", self.s.now.timestamp()),
                        body: templates::conflict_comment(&pr.base_ref),
                    });
                }
            }
            Conflict::Dirty => {}
        }
    }

    /// One nudge per waiting period, once the author has been quiet for
    /// `author_nudge_days` while a blocker is theirs. Drafts and pull
    /// requests by maintainers are not nudged.
    fn nudge(&mut self, cfg: &Config, pr: &PullRequest, d: &Decision) {
        if d.rule != 4 || pr.author.is_bot || self.is_maintainer(pr.author.id) {
            return;
        }
        let mut last = pr.created_at;
        let mut bump = |t: DateTime<Utc>| {
            if t > last {
                last = t
            }
        };
        if let Some(p) = pr.last_push_at {
            bump(p);
        }
        for c in self.s.comments.iter().filter(|c| c.user.id == pr.author.id) {
            bump(c.created_at);
        }
        for t in &pr.threads {
            for c in t.comments.iter().filter(|c| c.user.id == pr.author.id) {
                bump(c.created_at);
            }
        }
        if self.plan.state.nudged_for == Some(last) {
            return;
        }
        let due = last + Duration::days(i64::from(cfg.raw.stale.author_nudge_days));
        if self.s.now < due {
            self.plan.jobs.push(Job {
                reason: "author-nudge".into(),
                at: due,
            });
            return;
        }
        let status_url = self
            .existing_status()
            .map(|c| c.url.clone())
            .filter(|u| !u.is_empty());
        let mention = format!("@{}", pr.author.login);
        self.post(NewComment {
            purpose: "nudge".into(),
            key: last.timestamp().to_string(),
            body: templates::nudge_comment(
                &mention,
                (self.s.now - last).num_days().max(1) as u32,
                status_url.as_deref(),
            ),
        });
        self.plan.state.nudged_for = Some(last);
    }

    fn check_runs(&mut self, cfg: &Config, pr: &PullRequest) {
        let mut want = Vec::new();
        if self.docs_advisory(cfg, pr) {
            want.push(CheckRun {
                name: DOCS_CHECK.into(),
                head_sha: pr.head_sha.clone(),
                conclusion: "neutral".into(),
                title: "Code changed and no doc did".into(),
                summary: templates::docs_check_summary(&self.s.bot.slug),
            });
        } else if pr
            .own_checks
            .get(DOCS_CHECK)
            .is_some_and(|c| c.conclusion.as_deref() == Some("neutral"))
        {
            want.push(CheckRun {
                name: DOCS_CHECK.into(),
                head_sha: pr.head_sha.clone(),
                conclusion: "success".into(),
                title: "No docs advisory".into(),
                summary: "The docs advisory no longer applies to this pull request.".into(),
            });
        }
        if let Some(errors) = &pr.config_edit {
            want.push(CheckRun {
                name: CONFIG_CHECK.into(),
                head_sha: pr.head_sha.clone(),
                conclusion: if errors.is_empty() {
                    "success"
                } else {
                    "failure"
                }
                .into(),
                title: if errors.is_empty() {
                    "Config is valid".into()
                } else {
                    format!("Config has {} error(s)", errors.len())
                },
                summary: templates::config_check_summary(errors),
            });
        }
        for w in want {
            let same = pr.own_checks.get(&w.name).is_some_and(|c| {
                c.conclusion.as_deref() == Some(w.conclusion.as_str())
                    && c.title == w.title
                    && c.summary == w.summary
            });
            if !same {
                self.plan.check_runs.push(w);
            }
        }
    }

    /// Queue a notice (a conflict or a nudge). The daily cap is applied in
    /// `finish`, after command replies.
    fn post(&mut self, c: NewComment) {
        if !self
            .s
            .store
            .posted
            .contains(&(c.purpose.clone(), c.key.clone()))
        {
            self.notices.push(c);
        }
    }

    fn finish(mut self) -> Plan {
        // The status comment took its share of the cap when it was planned.
        // Replies come next, so a command answer is not what the cap drops
        // first; notices last.
        let queued: Vec<NewComment> = std::mem::take(&mut self.replies)
            .into_iter()
            .chain(std::mem::take(&mut self.notices))
            .collect();
        for c in queued {
            if self
                .s
                .store
                .posted
                .contains(&(c.purpose.clone(), c.key.clone()))
            {
                continue;
            }
            if self.new_comments_budget == 0 {
                self.plan.notes.push(format!(
                    "{} comment held back: daily comment cap reached",
                    c.purpose
                ));
                // Not sent, so not done: the next evaluation tries again.
                match c.purpose.as_str() {
                    "nudge" => self.plan.state.nudged_for = self.s.store.entity.nudged_for,
                    "conflict" => self.plan.state.conflict_notified = false,
                    _ => {}
                }
                continue;
            }
            self.new_comments_budget -= 1;
            self.plan.comments.push(c);
        }
        self.plan.state.processed = trim_processed(&self.plan.state, self.s);
        self.plan
    }
}

/// Keys of comments that no longer exist are dropped, so the stored set
/// does not grow without bound.
fn trim_processed(st: &EntityState, s: &Snapshot) -> BTreeSet<(u64, String)> {
    let ids: BTreeSet<u64> = s.comments.iter().map(|c| c.id).collect();
    st.processed
        .iter()
        .filter(|(id, _)| ids.contains(id))
        .cloned()
        .collect()
}

fn role_label(cfg: &Config, r: Role) -> &str {
    let l = cfg.labels();
    match r {
        Role::Triage => &l.triage,
        Role::Author => &l.author,
        Role::Review => &l.review,
    }
}

fn words(cmd: &Command) -> String {
    cmd.args
        .iter()
        .map(|a| match a {
            Arg::Word(w) => w.clone(),
            Arg::Issue(n) => format!("#{n}"),
            Arg::User(u) => u.clone(),
            Arg::Label { add, name } => format!("{}{name}", if *add { "+" } else { "-" }),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn short(s: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(&Sha256::digest(s.as_bytes())[..6])
}
