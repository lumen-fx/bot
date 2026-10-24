//! Every text the bot writes to GitHub. ASCII only, no emoji, no em-dashes,
//! and no `@` outside code spans except the one mention a nudge carries; a
//! test renders each template and checks those rules.

use crate::cla::{GroupMember, State, Verdict};
use crate::config::Config;

/// The hidden marker that identifies the status comment.
pub const STATUS_MARKER: &str = "<!-- lumenfx-bot:status v1 -->";

/// Make text from GitHub safe to embed: ASCII only, no mentions, no
/// backticks (it usually sits inside a code span), single line.
pub fn clean(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '@' => '_',
            '`' => '\'',
            '\n' | '\r' | '\t' => ' ',
            c if c.is_ascii() && !c.is_ascii_control() => c,
            _ => '?',
        })
        .collect()
}

/// A code span around cleaned text.
pub fn code(s: &str) -> String {
    format!("`{}`", clean(s))
}

/// One entry of the command list, shared by the help reply and `/help`.
pub struct CommandHelp {
    pub usage: &'static str,
    pub where_: &'static str,
    pub who: &'static str,
    pub effect: &'static str,
}

pub const COMMANDS: &[CommandHelp] = &[
    CommandHelp {
        usage: "help",
        where_: "pull request, issue",
        who: "anyone",
        effect: "Replies with this list.",
    },
    CommandHelp {
        usage: "ready",
        where_: "pull request",
        who: "author, maintainer",
        effect: "Marks requested changes as addressed and moves the pull request back to review. `review` is an alias.",
    },
    CommandHelp {
        usage: "author",
        where_: "pull request",
        who: "maintainer",
        effect: "Puts the pull request on the author until their next push or their `ready` command.",
    },
    CommandHelp {
        usage: "blocked [#N]",
        where_: "pull request, issue",
        who: "maintainer",
        effect: "Adds the blocked label. With #N, the bot removes it when #N closes.",
    },
    CommandHelp {
        usage: "unblock",
        where_: "pull request, issue",
        who: "maintainer",
        effect: "Removes the blocked label.",
    },
    CommandHelp {
        usage: "label +NAME -NAME ...",
        where_: "pull request, issue",
        who: "maintainer; anyone for the labels the repository opens to everyone",
        effect: "Adds and removes labels. Quote names with spaces: +\"good first issue\".",
    },
    CommandHelp {
        usage: "concern NAME",
        where_: "pull request",
        who: "maintainer",
        effect: "Raises a named concern; the pull request does not move to review while one is open.",
    },
    CommandHelp {
        usage: "resolve NAME",
        where_: "pull request",
        who: "maintainer",
        effect: "Resolves a concern.",
    },
    CommandHelp {
        usage: "sign SENTENCE",
        where_: "pull request",
        who: "a contributor to the pull request who has not signed the CLA",
        effect: "Signs the CLA in repositories that ask for one. SENTENCE is the repository's sentence, word for word, on the same line; the `cla` check shows it.",
    },
    CommandHelp {
        usage: "recheck",
        where_: "pull request",
        who: "anyone",
        effect: "Reads the CLA signatures again and updates the `cla` check.",
    },
    CommandHelp {
        usage: "no-docs-needed",
        where_: "pull request",
        who: "maintainer",
        effect: "Silences the docs advisory for this pull request.",
    },
];

/// Commands planned for later versions; they answer "not available yet".
pub const NOT_YET: &[&str] = &[
    "approve-ci",
    "rerun",
    "requeue",
    "flaky",
    "not-flaky",
    "breaking",
    "not-breaking",
    "perf",
    "try",
    "needs-info",
    "claim",
    "release",
    "assign",
    "unassign",
    "gfi",
    "not-gfi",
];

pub fn help_reply(slug: &str, public_url: &str) -> String {
    let mut s = format!(
        "Every command starts with the mention, one per line: `@{slug} <command>`. A bare word such as `ready` does nothing.\n\n"
    );
    for c in COMMANDS {
        s.push_str(&format!(
            "- `@{slug} {}` ({}; {}): {}\n",
            c.usage, c.where_, c.who, c.effect
        ));
    }
    s.push_str(&format!(
        "\nCommands inside code, quotes, or HTML are ignored. The full list is at {}/help.",
        public_url.trim_end_matches('/')
    ));
    s
}

/// Everything the status comment shows.
#[derive(Debug, Clone, Default)]
pub struct StatusView {
    pub welcome: Option<Welcome>,
    pub held_by_pullsift: bool,
    pub headline: String,
    pub label: Option<String>,
    /// (done, text)
    pub items: Vec<(bool, String)>,
    pub queued: bool,
    pub blocked_on: Option<u64>,
    pub concerns: Vec<(String, String)>,
    pub checks_line: String,
    pub held_runs: bool,
    pub coverage: Option<String>,
    pub pullsift: Option<String>,
    pub docs: Option<String>,
    pub base_ref: String,
    pub slug: String,
}

#[derive(Debug, Clone, Default)]
pub struct Welcome {
    /// The CLA is gated here and not signed yet.
    pub cla_needed: bool,
    pub contributing_url: Option<String>,
}

pub fn status_comment(v: &StatusView) -> String {
    let mut s = String::new();
    s.push_str(STATUS_MARKER);
    s.push('\n');
    if v.held_by_pullsift {
        s.push_str("Held by Pullsift; see its comment.\n");
        return s;
    }
    if let Some(w) = &v.welcome {
        s.push_str(
            "Welcome, and thank you for the pull request. This comment lists what is left \
             before a maintainer reviews it, and the bot keeps it up to date as things change.",
        );
        if w.cla_needed {
            s.push_str(
                " Start with the contributor license agreement; the CLA item below says how.",
            );
        }
        s.push_str(" A maintainer starts CI for a first pull request.");
        if let Some(url) = &w.contributing_url {
            s.push_str(&format!(
                " How the project works is in [CONTRIBUTING.md]({url})."
            ));
        }
        s.push_str("\n\n");
    }
    match &v.label {
        Some(l) => s.push_str(&format!("**Status: {}** ({})\n\n", v.headline, code(l))),
        None => s.push_str(&format!("**Status: {}**\n\n", v.headline)),
    }
    if v.queued {
        s.push_str("Queued in the merge queue.\n\n");
    }
    if let Some(n) = v.blocked_on {
        s.push_str(&format!(
            "Blocked on #{n}; the block lifts when it closes.\n\n"
        ));
    }
    let open: Vec<&(bool, String)> = v.items.iter().filter(|(done, _)| !done).collect();
    if open.is_empty() {
        s.push_str("Nothing is waiting on you.\n");
    } else {
        s.push_str("What is left:\n");
    }
    for (done, text) in &v.items {
        s.push_str(&format!("- [{}] {}\n", if *done { "x" } else { " " }, text));
    }
    s.push('\n');
    if !v.concerns.is_empty() {
        s.push_str(&format!(
            "Open concerns; a maintainer resolves each with `@{} resolve NAME`:\n",
            v.slug
        ));
        for (name, by) in &v.concerns {
            s.push_str(&format!("- {} (raised by {})\n", code(name), code(by)));
        }
        s.push('\n');
    }
    s.push_str(&v.checks_line);
    if v.held_runs {
        s.push_str(" CI for this push waits for a maintainer to approve the run.");
    }
    s.push('\n');
    for line in [&v.coverage, &v.pullsift, &v.docs].into_iter().flatten() {
        s.push_str(line);
        s.push('\n');
    }
    s.push_str(&format!(
        "\nCommands: `@{} help`. The bot follows `.github/lumenfx.toml` on {}.\n",
        v.slug,
        code(&v.base_ref)
    ));
    s
}

/// The line a contributor posts to sign, as a code span.
pub fn sign_line(slug: &str, sentence: &str) -> String {
    format!("`@{slug} sign {}`", clean(sentence))
}

fn logins(users: &[crate::snapshot::UserRef]) -> String {
    users
        .iter()
        .map(|u| code(&u.login))
        .collect::<Vec<_>>()
        .join(", ")
}

fn document(cfg: &Config) -> String {
    if cfg.raw.cla.document.is_empty() {
        "the CLA".to_string()
    } else {
        format!("the [CLA]({})", cfg.raw.cla.document)
    }
}

/// The CLA lines of the status comment: (done, text).
pub fn cla_items(cfg: &Config, v: &Verdict, slug: &str) -> Vec<(bool, String)> {
    let mut out = Vec::new();
    match &v.state {
        State::Signed => {
            let mut t = "CLA signed".to_string();
            if !v.unresolved.is_empty() {
                t.push_str(&format!(
                    ". Co-authors with no GitHub account are not asked to sign: {}",
                    names(&v.unresolved)
                ));
            }
            out.push((true, t));
        }
        State::Unknown(_) => out.push((
            false,
            format!(
                "CLA: the signature store could not be read. The bot reads it again on the next change, or on `@{slug} recheck`."
            ),
        )),
        State::Unsigned => {
            if !v.missing.is_empty() {
                let who = if v.missing.len() == 1 {
                    "sign"
                } else {
                    "each of them signs"
                };
                out.push((
                    false,
                    format!(
                        "CLA not signed by {}. Read {}, then {who} by commenting this line on this pull request: {}",
                        logins(&v.missing),
                        document(cfg),
                        sign_line(slug, &cfg.raw.cla.sentence)
                    ),
                ));
            }
            for (sha, name) in &v.unlinked {
                out.push((false, unlinked_text(sha, name)));
            }
        }
    }
    out
}

fn names(ns: &[String]) -> String {
    ns.iter().map(|n| code(n)).collect::<Vec<_>>().join(", ")
}

fn unlinked_text(sha: &str, name: &str) -> String {
    let by = if name.trim().is_empty() {
        String::new()
    } else {
        format!(" by {}", code(name))
    };
    format!(
        "CLA: commit {} is authored{by} with an email linked to no GitHub account. Add the email to your GitHub account, or re-author the commit with an email that is, and push.",
        code(sha)
    )
}

pub fn cla_check_title(v: &Verdict) -> String {
    if v.missing.is_empty() {
        "A commit email is linked to no GitHub account".to_string()
    } else {
        let who: Vec<String> = v.missing.iter().map(|u| clean(&u.login)).collect();
        format!("CLA not signed by {}", who.join(", "))
    }
}

pub fn cla_check_summary(cfg: &Config, v: &Verdict, slug: &str) -> String {
    let mut s = String::new();
    match &v.state {
        State::Signed => s.push_str(
            "Everyone whose commits are on this pull request has signed the CLA or does not need to.\n",
        ),
        State::Unknown(e) => s.push_str(&format!(
            "The signature store could not be read ({}). The bot reads it again on the next change, or on `@{slug} recheck`.\n",
            clean(e)
        )),
        State::Unsigned => {
            if !v.missing.is_empty() {
                s.push_str(&format!(
                    "Not signed yet: {}.\n\nRead {}. To sign, comment this line on this pull request, from the account that has not signed:\n\n{}\n\nSigning is once per GitHub account and covers every later pull request.\n",
                    logins(&v.missing),
                    document(cfg),
                    sign_line(slug, &cfg.raw.cla.sentence)
                ));
            }
            for (sha, name) in &v.unlinked {
                s.push_str(&format!("\n{}\n", unlinked_text(sha, name)));
            }
        }
    }
    if !v.unresolved.is_empty() {
        s.push_str(&format!(
            "\nCo-authors with no GitHub account are not asked to sign: {}.\n",
            names(&v.unresolved)
        ));
    }
    s.trim_start().to_string()
}

pub fn merge_group_summary(members: &[GroupMember], failing: &[u64]) -> String {
    if members.is_empty() {
        return "The bot could not tell which pull requests are in this merge group, so it cannot vouch for their CLA.\n".to_string();
    }
    let mut s =
        String::from("The `cla` check of each pull request on the head that was queued:\n\n");
    for m in members {
        let state = match m.cla {
            Some(crate::snapshot::CheckState::Success) => "signed",
            Some(crate::snapshot::CheckState::Failure) => "not signed",
            Some(crate::snapshot::CheckState::Pending) => "not reported yet",
            None => "no `cla` check",
        };
        let short: String = m.head_sha.chars().take(7).collect();
        s.push_str(&format!("- #{}: {} at {}\n", m.number, state, code(&short)));
    }
    if !failing.is_empty() {
        s.push_str("\nThe group fails until every pull request in it has signed.\n");
    }
    s
}

/// Reply to a `sign` whose words are not the sentence.
pub fn sign_wrong_sentence(slug: &str, sentence: &str) -> String {
    format!(
        "That is not the CLA sentence. To sign, comment exactly this line, with nothing before or after it: {}",
        sign_line(slug, sentence)
    )
}

/// Reply to a `sign` from someone this pull request does not need.
pub fn sign_not_needed() -> String {
    "`sign`: only someone whose commits are on this pull request, and who has not signed the CLA, can sign here. To sign for your own contributions, comment the same line on your pull request.".to_string()
}

pub fn sign_not_gated() -> String {
    "`sign`: this repository does not ask for a CLA.".to_string()
}

pub fn item_conflict(base: &str) -> String {
    format!(
        "Conflicts with {}. Rebase onto {} and force-push.",
        code(base),
        code(base)
    )
}

pub fn item_merge_commits(shas: &[String], base: &str) -> String {
    let list: Vec<String> = shas.iter().take(5).map(|s| code(s)).collect();
    let more = if shas.len() > 5 { ", ..." } else { "" };
    let (what, verb) = if shas.len() == 1 {
        ("Commit", "merges")
    } else {
        ("Commits", "merge")
    };
    format!(
        "Merge commits: {what} {}{more} {verb} another branch into this one. The merge queue rebases, so rebase onto {} instead of merging.",
        list.join(", "),
        code(base)
    )
}

pub fn item_red_check(name: &str, url: Option<&str>) -> String {
    match url {
        Some(u) => format!("{} failed ([details]({u})).", code(name)),
        None => format!("{} failed.", code(name)),
    }
}

pub fn item_changes_requested(reviewers: &[String], slug: &str) -> String {
    let who: Vec<String> = reviewers.iter().map(|r| code(r)).collect();
    format!(
        "Changes requested by {} are not addressed yet. Push the changes and reply to or resolve each review thread, or comment `@{slug} ready` once you have answered them.",
        who.join(", ")
    )
}

pub fn item_author_command(slug: &str) -> String {
    format!(
        "A maintainer has asked for changes. Push them, or comment `@{slug} ready` when you are done."
    )
}

pub fn conflict_comment(base: &str) -> String {
    format!(
        "This branch now conflicts with {}. Rebase it onto {} and force-push; the merge queue rebases, so merging {} in does not help.",
        code(base),
        code(base),
        code(base)
    )
}

/// The one comment that mentions someone: the author, after the pull request
/// waited on them for `days` days.
pub fn nudge_comment(mention: &str, days: u32, status_url: Option<&str>) -> String {
    let what = match status_url {
        Some(u) => format!("What is left is in the [status comment]({u})."),
        None => "What is left is listed above.".to_string(),
    };
    format!(
        "{mention}, this pull request has been waiting on you for {days} days. {what} It stays open; push or reply when you pick it up."
    )
}

pub fn docs_check_summary(slug: &str) -> String {
    format!(
        "Code under the repository's code paths changed and no documentation page did. \
         If users can observe this change, update its page in this pull request. \
         A maintainer silences this with `@{slug} no-docs-needed`."
    )
}

pub fn docs_line() -> String {
    "Docs: code changed and no doc page did. If users can observe this change, update its page."
        .to_string()
}

pub fn config_check_summary(errors: &[String]) -> String {
    if errors.is_empty() {
        "The edited `.github/lumenfx.toml` parses and validates.".to_string()
    } else {
        let mut s = "The edited `.github/lumenfx.toml` does not validate:\n\n".to_string();
        for e in errors {
            s.push_str(&format!("- {}\n", clean(e)));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The style rules every template follows. `allowed_mention` is the one
    /// `@login` a text may carry outside code spans.
    pub fn assert_style(text: &str, allowed_mention: Option<&str>) {
        assert!(text.is_ascii(), "non-ASCII in:\n{text}");
        assert!(
            !text.contains("--") || text.contains("<!--"),
            "double dash in:\n{text}"
        );
        let mut outside = String::new();
        let mut in_code = false;
        for c in text.chars() {
            if c == '`' {
                in_code = !in_code;
            } else if !in_code {
                outside.push(c);
            }
        }
        let outside = match allowed_mention {
            Some(m) => outside.replacen(m, "", 1),
            None => outside,
        };
        assert!(!outside.contains('@'), "stray @ in:\n{text}");
    }

    #[test]
    fn clean_strips_what_must_not_reach_github() {
        assert_eq!(clean("a@b `x`\nnew \u{2014} line"), "a_b 'x' new ? line");
    }

    #[test]
    fn help_follows_the_style() {
        let h = help_reply("lumenfx-bot", "https://bot.lumenfx.dev/");
        assert_style(&h, None);
        assert!(h.contains("https://bot.lumenfx.dev/help"));
        assert!(h.contains("A bare word"));
        for c in COMMANDS {
            assert!(h.contains(c.usage));
        }
    }

    #[test]
    fn every_template_follows_the_style() {
        let cfg = Config::parse(include_str!("../../../examples/lumenfx.toml")).unwrap();
        let v = Verdict {
            state: State::Unsigned,
            missing: vec![crate::snapshot::UserRef {
                id: 5,
                login: "newcomer".into(),
                is_bot: false,
            }],
            unlinked: vec![("abc1234".into(), "Some One".into())],
            unresolved: vec!["Claude".into()],
        };
        let mut cla = cla_items(&cfg, &v, "lumenfx-bot");
        assert_eq!(cla.len(), 2);
        let full = StatusView {
            held_by_pullsift: false,
            welcome: Some(Welcome {
                cla_needed: true,
                contributing_url: Some("https://github.com/o/r/blob/main/CONTRIBUTING.md".into()),
            }),
            headline: "waiting on author".into(),
            label: Some("S-Waiting-On-Author".into()),
            items: vec![
                cla.remove(0),
                cla.remove(0),
                (false, item_conflict("main")),
                (
                    false,
                    item_merge_commits(&vec!["abc1234".to_string(); 7], "main"),
                ),
                (
                    false,
                    item_red_check("test (ubuntu-latest)", Some("https://x/y")),
                ),
                (
                    false,
                    item_changes_requested(&["someone".into()], "lumenfx-bot"),
                ),
                (false, item_author_command("lumenfx-bot")),
                (true, "CLA signed".into()),
            ],
            queued: true,
            blocked_on: Some(4),
            concerns: vec![("naming".into(), "FizzWizZleDazzle".into())],
            checks_line: "Checks: 1 of 2 required green.".into(),
            held_runs: true,
            coverage: Some("Coverage: `codecov/patch` 86.00% of diff hit (target 80.00%).".into()),
            pullsift: Some("Pullsift: screened, no action.".into()),
            docs: Some(docs_line()),
            base_ref: "main".into(),
            slug: "lumenfx-bot".into(),
        };
        assert_style(&status_comment(&full), None);
        assert_style(
            &status_comment(&StatusView {
                held_by_pullsift: true,
                ..Default::default()
            }),
            None,
        );
        assert_style(&conflict_comment("main"), None);
        assert_style(&cla_check_summary(&cfg, &v, "lumenfx-bot"), None);
        assert_style(&cla_check_title(&v), None);
        for state in [State::Signed, State::Unknown("HTTP 503".into())] {
            let v = Verdict { state, ..v.clone() };
            assert_style(&cla_check_summary(&cfg, &v, "lumenfx-bot"), None);
            for (_, t) in cla_items(&cfg, &v, "lumenfx-bot") {
                assert_style(&t, None);
            }
        }
        let m = |n, cla| GroupMember {
            number: n,
            head_sha: "abcdef0123".into(),
            cla,
        };
        assert_style(
            &merge_group_summary(
                &[m(1, Some(crate::snapshot::CheckState::Success)), m(2, None)],
                &[2],
            ),
            None,
        );
        assert_style(&merge_group_summary(&[], &[]), None);
        assert_style(
            &sign_wrong_sentence("lumenfx-bot", &cfg.raw.cla.sentence),
            None,
        );
        assert_style(&sign_not_needed(), None);
        assert_style(&sign_not_gated(), None);
        assert_style(
            &nudge_comment("@newcomer", 14, Some("https://x")),
            Some("@newcomer"),
        );
        assert_style(&nudge_comment("`owner`", 14, None), None);
        assert_style(&docs_check_summary("lumenfx-bot"), None);
        assert_style(&docs_line(), None);
        assert_style(&config_check_summary(&["bad @key".into()]), None);
        assert_style(&config_check_summary(&[]), None);
    }
}
