//! `.github/lumenfx.toml`: the per-repository rules the bot follows.
//!
//! The file is read from the default branch only, so a pull request can never
//! change the rules it is judged by. Every section has defaults, so a repository
//! opts into a feature by writing its section. Unknown keys are errors: a typo
//! in a gate's config must not silently turn the gate off.

use std::collections::BTreeMap;

use globset::{Glob, GlobBuilder, GlobMatcher, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

/// Where the file lives in a repository.
pub const CONFIG_PATH: &str = ".github/lumenfx.toml";

/// The parsed file, before validation.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub labels: Labels,
    /// Path globs to labels, plus a `title_prefix` sub-table. Kept as a raw
    /// table so the file's order survives: the first matching path wins.
    #[serde(default)]
    pub autolabel: toml::Table,
    #[serde(default)]
    pub status_comment: StatusComment,
    #[serde(default)]
    pub no_merges: NoMerges,
    #[serde(default)]
    pub stale: Stale,
    #[serde(default)]
    pub cla: Cla,
    #[serde(default)]
    pub migration: Migration,
    #[serde(default)]
    pub ci_approval: CiApproval,
    #[serde(default)]
    pub codeql: Codeql,
    #[serde(default)]
    pub tests: Tests,
    #[serde(default)]
    pub flaky: Flaky,
    #[serde(default)]
    pub requeue: Requeue,
    #[serde(default)]
    pub docs: Docs,
    #[serde(default)]
    pub sizes: Sizes,
    #[serde(default)]
    pub perf: Perf,
    #[serde(default)]
    pub good_first_issue: GoodFirstIssue,
    #[serde(default)]
    pub claims: Claims,
    #[serde(default)]
    pub repins: BTreeMap<String, Repin>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    #[serde(default)]
    pub maintainers: Vec<Maintainer>,
    /// Repository write access (write, maintain, admin) also counts.
    #[serde(default = "yes")]
    pub write_access_is_maintainer: bool,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            maintainers: Vec::new(),
            write_access_is_maintainer: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Maintainer {
    /// The numeric GitHub user ID. Logins change; IDs do not.
    pub id: u64,
    /// For reading the file; never used to match.
    #[serde(default)]
    pub login: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Labels {
    #[serde(default)]
    pub status: StatusLabels,
    #[serde(default)]
    pub self_service: SelfService,
}

/// Role -> label name in this repository.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StatusLabels {
    #[serde(default = "d_triage")]
    pub triage: String,
    #[serde(default = "d_author")]
    pub author: String,
    #[serde(default = "d_review")]
    pub review: String,
    #[serde(default = "d_info")]
    pub info: String,
    #[serde(default = "d_ready")]
    pub ready: String,
    #[serde(default = "d_blocked")]
    pub blocked: String,
    /// S- labels the bot never adds or removes on its own; their presence
    /// freezes the bot's S- handling.
    #[serde(default = "d_maintainer_owned")]
    pub maintainer_owned: Vec<String>,
}

impl Default for StatusLabels {
    fn default() -> Self {
        Self {
            triage: d_triage(),
            author: d_author(),
            review: d_review(),
            info: d_info(),
            ready: d_ready(),
            blocked: d_blocked(),
            maintainer_owned: d_maintainer_owned(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SelfService {
    /// Label patterns (`*` wildcard) anyone may set with the `label` command.
    #[serde(default)]
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StatusCommentMode {
    /// Created once the author has something to do, a run is held, or the
    /// author is new; edited in place after that.
    #[default]
    Actionable,
    /// Every pull request.
    Always,
    /// Labels and checks only.
    Never,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StatusComment {
    #[serde(default)]
    pub mode: StatusCommentMode,
    /// Path of the contributing guide on the default branch, linked from the
    /// welcome a first-time contributor reads.
    #[serde(default)]
    pub contributing: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NoMerges {
    /// Merge commits in a branch block it (the merge queue rebases).
    #[serde(default = "yes")]
    pub enabled: bool,
}

impl Default for NoMerges {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Stale {
    #[serde(default = "d_14")]
    pub author_nudge_days: u32,
    #[serde(default = "d_14")]
    pub issue_info_nudge_days: u32,
    /// Must be 0: the bot never closes anything by timer.
    #[serde(default)]
    pub close_after_days: u32,
}

impl Default for Stale {
    fn default() -> Self {
        Self {
            author_nudge_days: 14,
            issue_info_nudge_days: 14,
            close_after_days: 0,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Cla {
    /// Publish the `cla` check on pull requests and merge groups.
    #[serde(default)]
    pub enabled: bool,
    /// The repository that holds the signatures, `owner/name`.
    #[serde(default)]
    pub store: String,
    /// The signature file in the store, in CLA Assistant's JSON format.
    #[serde(default)]
    pub path: String,
    /// Link to the agreement.
    #[serde(default)]
    pub document: String,
    /// What a contributor writes after `@<bot> sign`, exactly.
    #[serde(default)]
    pub sentence: String,
    /// Accounts that never sign, by numeric ID. Bot accounts and the
    /// maintainers in `[permissions]` are exempt without being listed.
    #[serde(default)]
    pub allow_ids: Vec<u64>,
    /// Lock a pull request's conversation once it merges, so the signing
    /// comments on it cannot be edited afterwards.
    #[serde(default)]
    pub lock_after_merge: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Migration {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub dir: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CiApproval {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub require: Vec<String>,
    #[serde(default)]
    pub deny_paths: Vec<String>,
    #[serde(default)]
    pub max_runs_per_author_per_day: u32,
    #[serde(default)]
    pub min_account_age_days: u32,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Codeql {
    #[serde(default)]
    pub gate: bool,
    #[serde(default)]
    pub environment: String,
    #[serde(default)]
    pub verdict_check: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Tests {
    #[serde(default)]
    pub trusted_events: Vec<String>,
    #[serde(default)]
    pub artifacts: String,
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub infra_patterns: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Flaky {
    #[serde(default)]
    pub confirm: String,
    #[serde(default)]
    pub suspect_after_distinct_trees: u32,
    #[serde(default)]
    pub window_days: u32,
    #[serde(default)]
    pub issue_labels: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Requeue {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub max_per_pr_head: u32,
    #[serde(default)]
    pub max_per_day: u32,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Docs {
    /// Paths whose change may be user-observable.
    #[serde(default)]
    pub code: Vec<String>,
    /// Paths that are documentation.
    #[serde(default)]
    pub docs: Vec<String>,
    /// C- labels for which the advisory stays quiet.
    #[serde(default)]
    pub quiet_classes: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Sizes {
    #[serde(default)]
    pub artifact: String,
    #[serde(default)]
    pub report_growth_percent: f64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Perf {
    #[serde(default)]
    pub workflow: String,
    #[serde(default)]
    pub benches: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GoodFirstIssue {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub areas: Vec<String>,
    #[serde(default)]
    pub classes: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Claims {
    #[serde(default)]
    pub nudge_days: u32,
    #[serde(default)]
    pub release_days: u32,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Repin {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub workflow: String,
}

fn yes() -> bool {
    true
}
fn d_14() -> u32 {
    14
}
fn d_triage() -> String {
    "S-Needs-Triage".into()
}
fn d_author() -> String {
    "S-Waiting-On-Author".into()
}
fn d_review() -> String {
    "S-Needs-Review".into()
}
fn d_info() -> String {
    "S-Needs-Info".into()
}
fn d_ready() -> String {
    "S-Ready".into()
}
fn d_blocked() -> String {
    "S-Blocked".into()
}
fn d_maintainer_owned() -> Vec<String> {
    [
        "S-Needs-Design",
        "S-Blocked",
        "S-Duplicate",
        "S-Wontfix",
        "S-Needs-Info",
    ]
    .map(String::from)
    .to_vec()
}

/// One path rule of `[autolabel]`.
#[derive(Debug, Clone)]
pub struct PathRule {
    pub pattern: String,
    pub label: String,
    matcher: GlobMatcher,
}

/// A validated config: the raw file plus everything compiled from it.
#[derive(Debug, Clone)]
pub struct Config {
    pub raw: RawConfig,
    pub path_rules: Vec<PathRule>,
    /// Conventional-commit type -> C- label.
    pub title_prefix: Vec<(String, String)>,
    docs_code: GlobSet,
    docs_docs: GlobSet,
    self_service: Vec<GlobMatcher>,
}

impl PartialEq for Config {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl Config {
    /// Parse and validate. Every problem is reported, not just the first.
    pub fn parse(text: &str) -> Result<Config, Vec<String>> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| vec![e.to_string()])?;
        Self::from_raw(raw)
    }

    pub fn from_raw(raw: RawConfig) -> Result<Config, Vec<String>> {
        let mut errors = Vec::new();

        for m in &raw.permissions.maintainers {
            if m.id == 0 {
                errors.push(format!(
                    "permissions.maintainers: `{}` has id 0; use the numeric GitHub user ID",
                    m.login
                ));
            }
        }

        let s = &raw.labels.status;
        for (role, name) in [
            ("triage", &s.triage),
            ("author", &s.author),
            ("review", &s.review),
            ("info", &s.info),
            ("ready", &s.ready),
            ("blocked", &s.blocked),
        ] {
            if name.trim().is_empty() {
                errors.push(format!("labels.status.{role} is empty"));
            }
        }
        let bot_roles = [&s.triage, &s.author, &s.review];
        if bot_roles[0] == bot_roles[1]
            || bot_roles[1] == bot_roles[2]
            || bot_roles[0] == bot_roles[2]
        {
            errors.push(
                "labels.status: triage, author and review must be three different labels".into(),
            );
        }
        for owned in &s.maintainer_owned {
            if bot_roles.contains(&owned) {
                errors.push(format!(
                    "labels.status.maintainer_owned lists `{owned}`, which is a role the bot sets"
                ));
            }
        }

        let mut self_service = Vec::new();
        for p in &raw.labels.self_service.allow {
            match label_glob(p) {
                Ok(m) => self_service.push(m),
                Err(e) => errors.push(format!("labels.self_service.allow `{p}`: {e}")),
            }
        }

        let mut path_rules = Vec::new();
        let mut title_prefix = Vec::new();
        for (key, value) in &raw.autolabel {
            if key == "title_prefix" {
                match value {
                    toml::Value::Table(t) => {
                        for (prefix, label) in t {
                            match label.as_str() {
                                Some(l) if !l.is_empty() => {
                                    title_prefix.push((prefix.to_ascii_lowercase(), l.to_string()))
                                }
                                _ => errors.push(format!(
                                    "autolabel.title_prefix.{prefix} must be a label name"
                                )),
                            }
                        }
                    }
                    _ => errors.push("autolabel.title_prefix must be a table".into()),
                }
                continue;
            }
            let Some(label) = value.as_str().filter(|l| !l.is_empty()) else {
                errors.push(format!("autolabel `{key}` must map to a label name"));
                continue;
            };
            match path_glob(key) {
                Ok(matcher) => path_rules.push(PathRule {
                    pattern: key.clone(),
                    label: label.to_string(),
                    matcher,
                }),
                Err(e) => errors.push(format!("autolabel `{key}`: {e}")),
            }
        }

        let docs_code = globset(&raw.docs.code, "docs.code", &mut errors);
        let docs_docs = globset(&raw.docs.docs, "docs.docs", &mut errors);
        // Compiled for validation only; the features that use them are later.
        globset(
            &raw.ci_approval.deny_paths,
            "ci_approval.deny_paths",
            &mut errors,
        );

        if raw.stale.close_after_days != 0 {
            errors.push(
                "stale.close_after_days must be 0: the bot never closes pull requests or issues by timer"
                    .into(),
            );
        }
        if raw.stale.author_nudge_days == 0 {
            errors.push("stale.author_nudge_days must be at least 1".into());
        }
        if raw.cla.enabled {
            let c = &raw.cla;
            if c.sentence.trim().is_empty() {
                errors.push("cla.sentence is required when cla.enabled = true".into());
            } else if c.sentence.contains(['\n', '\r', '`']) || !c.sentence.is_ascii() {
                errors.push("cla.sentence must be one line of ASCII text without backticks".into());
            }
            let parts: Vec<&str> = c.store.split('/').collect();
            if parts.len() != 2 || parts.iter().any(|p| p.is_empty()) {
                errors.push("cla.store must be `owner/name` when cla.enabled = true".into());
            }
            if c.path.is_empty() || c.path.starts_with('/') || c.path.contains("..") {
                errors.push("cla.path must be a relative path in the store".into());
            }
        }
        if raw.migration.enabled && (raw.migration.label.is_empty() || raw.migration.dir.is_empty())
        {
            errors.push(
                "migration.label and migration.dir are required when migration.enabled = true"
                    .into(),
            );
        }
        if let Some(c) = &raw.status_comment.contributing
            && (c.is_empty() || c.starts_with('/') || c.contains(".."))
        {
            errors.push(
                "status_comment.contributing must be a relative path in the repository".into(),
            );
        }

        if !errors.is_empty() {
            return Err(errors);
        }
        Ok(Config {
            raw,
            path_rules,
            title_prefix,
            docs_code,
            docs_docs,
            self_service,
        })
    }

    pub fn labels(&self) -> &StatusLabels {
        &self.raw.labels.status
    }

    /// The A- labels (or whatever the map names) for a set of changed paths:
    /// for each file, the first matching rule.
    pub fn area_labels<'a>(&self, files: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for f in files {
            if let Some(rule) = self.path_rules.iter().find(|r| r.matcher.is_match(f))
                && !out.contains(&rule.label)
            {
                out.push(rule.label.clone());
            }
        }
        out
    }

    /// The C- label for a title, from its conventional-commit type
    /// (`fix(scope)!: ...` has type `fix`).
    pub fn class_label(&self, title: &str) -> Option<String> {
        let t = title.trim_start().to_ascii_lowercase();
        let end = t
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
            .unwrap_or(t.len());
        let (kind, rest) = t.split_at(end);
        if kind.is_empty()
            || !(rest.starts_with('(') || rest.starts_with(':') || rest.starts_with('!'))
        {
            return None;
        }
        self.title_prefix
            .iter()
            .find(|(p, _)| p == kind)
            .map(|(_, l)| l.clone())
    }

    pub fn is_code_path(&self, path: &str) -> bool {
        self.docs_code.is_match(path)
    }

    pub fn is_docs_path(&self, path: &str) -> bool {
        self.docs_docs.is_match(path)
    }

    pub fn is_self_service_label(&self, label: &str) -> bool {
        self.self_service.iter().any(|m| m.is_match(label))
    }
}

fn path_glob(p: &str) -> Result<GlobMatcher, globset::Error> {
    Ok(GlobBuilder::new(p)
        .literal_separator(true)
        .build()?
        .compile_matcher())
}

fn label_glob(p: &str) -> Result<GlobMatcher, globset::Error> {
    Ok(Glob::new(p)?.compile_matcher())
}

fn globset(patterns: &[String], what: &str, errors: &mut Vec<String>) -> GlobSet {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        match GlobBuilder::new(p).literal_separator(true).build() {
            Ok(g) => {
                b.add(g);
            }
            Err(e) => errors.push(format!("{what} `{p}`: {e}")),
        }
    }
    b.build().unwrap_or_else(|_| GlobSet::empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example config shipped in examples/.
    pub const LUMEN: &str = include_str!("../../../examples/lumenfx.toml");

    #[test]
    fn the_example_config_validates() {
        let c = Config::parse(LUMEN).expect("example config is valid");
        assert_eq!(c.labels().author, "S-Waiting-On-Author");
        assert!(c.raw.cla.enabled);
        assert!(c.raw.cla.lock_after_merge);
        assert_eq!(c.raw.repins["candela"].workflow, "candela-pin.yml");
    }

    #[test]
    fn an_empty_file_is_valid_and_turns_everything_off() {
        let c = Config::parse("").unwrap();
        assert!(!c.raw.cla.enabled);
        assert!(c.path_rules.is_empty());
        assert!(!c.is_code_path("core/x.rs"));
    }

    #[test]
    fn unknown_keys_are_errors() {
        let e = Config::parse("[cla]\nenabeld = true\n").unwrap_err();
        assert!(e[0].contains("enabeld"), "{e:?}");
        let e = Config::parse("[nonsense]\n").unwrap_err();
        assert!(e[0].contains("nonsense"), "{e:?}");
    }

    #[test]
    fn every_problem_is_reported() {
        let e = Config::parse(
            "[permissions]\nmaintainers=[{id=0, login=\"x\"}]\n[stale]\nclose_after_days=3\n[autolabel]\n\"a/[\"=\"A-X\"\n",
        )
        .unwrap_err();
        assert_eq!(e.len(), 3, "{e:?}");
    }

    #[test]
    fn first_matching_path_rule_wins_per_file() {
        let c = Config::parse(
            "[autolabel]\n\"core/script/**\" = \"A-Scripting\"\n\"core/**\" = \"A-Core\"\n",
        )
        .unwrap();
        assert_eq!(c.area_labels(["core/script/a.rs"]), vec!["A-Scripting"]);
        assert_eq!(
            c.area_labels(["core/x/a.rs", "core/script/b.rs", "core/y.rs"]),
            vec!["A-Core", "A-Scripting"]
        );
        assert!(c.area_labels(["README.md"]).is_empty());
    }

    #[test]
    fn title_prefix_matches_the_conventional_type_only() {
        let c = Config::parse("[autolabel.title_prefix]\nfix = \"C-Bug\"\nfeat = \"C-Feature\"\n")
            .unwrap();
        assert_eq!(c.class_label("fix(devtools): x").as_deref(), Some("C-Bug"));
        assert_eq!(c.class_label("feat!: x").as_deref(), Some("C-Feature"));
        assert_eq!(c.class_label("Fix: x").as_deref(), Some("C-Bug"));
        assert_eq!(c.class_label("fixture: x"), None);
        assert_eq!(c.class_label("fix the thing"), None);
    }

    #[test]
    fn self_service_patterns() {
        let c = Config::parse("[labels.self_service]\nallow=[\"A-*\"]\n").unwrap();
        assert!(c.is_self_service_label("A-Text"));
        assert!(!c.is_self_service_label("S-Needs-Review"));
    }

    #[test]
    fn an_enabled_cla_needs_its_store_and_sentence() {
        let e = Config::parse("[cla]\nenabled = true\n").unwrap_err();
        assert_eq!(e.len(), 3, "{e:?}");
        let e = Config::parse(
            "[cla]\nenabled = true\nstore = \"o/r\"\npath = \"a.json\"\nsentence = \"I `sign`\"\n",
        )
        .unwrap_err();
        assert!(e[0].contains("one line"), "{e:?}");
        assert!(Config::parse("[cla]\nenabled = false\n").is_ok());
    }

    #[test]
    fn a_role_cannot_be_maintainer_owned() {
        let e =
            Config::parse("[labels.status]\nmaintainer_owned=[\"S-Needs-Review\"]\n").unwrap_err();
        assert!(e[0].contains("S-Needs-Review"));
    }
}
