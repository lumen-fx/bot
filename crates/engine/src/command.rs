//! Command parsing: `@<bot> <command> [args]`, one command per line.
//!
//! Commands are found only in prose. Fenced and indented code, inline code,
//! HTML, and blockquotes are skipped, so quoting a command in a reply never
//! runs it again.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Arg {
    /// `+label` or `-label`; quoted names may contain spaces.
    Label {
        add: bool,
        name: String,
    },
    /// `#123`
    Issue(u64),
    /// `@login`
    User(String),
    Word(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    /// The command word, lowercased.
    pub name: String,
    pub args: Vec<Arg>,
    /// The line after the mention, whitespace-normalized. With the comment
    /// ID it keys "already ran", so an edit that adds a command runs only
    /// the new one.
    pub text: String,
}

impl Command {
    pub fn key(&self) -> String {
        hex::encode(Sha256::digest(self.text.as_bytes()))
    }
}

/// Every command in a comment body, in order.
pub fn parse(body: &str, bot_slug: &str) -> Vec<Command> {
    let mention = format!("@{}", bot_slug.to_ascii_lowercase());
    prose_lines(body)
        .iter()
        .filter_map(|line| parse_line(line, &mention))
        .collect()
}

fn parse_line(line: &str, mention: &str) -> Option<Command> {
    let lower = line.to_ascii_lowercase();
    let mut from = 0;
    while let Some(pos) = lower[from..].find(mention) {
        let start = from + pos;
        let end = start + mention.len();
        let before_ok = start == 0
            || !lower[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
        let after = &line[end..];
        let after_ok = after.starts_with(char::is_whitespace);
        if before_ok && after_ok {
            let rest = after.trim();
            let tokens = tokenize(rest);
            let (name, args) = tokens.split_first()?;
            if name.starts_with(['+', '-', '#', '@', '"']) {
                return None;
            }
            let text = tokens.join(" ");
            return Some(Command {
                name: name.to_ascii_lowercase(),
                args: args.iter().map(|t| arg(t)).collect(),
                text,
            });
        }
        from = end;
    }
    None
}

/// Split on whitespace, keeping `"quoted strings"` (also after `+`/`-`) whole.
fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn unquote(s: &str) -> String {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
        .to_string()
}

fn arg(t: &str) -> Arg {
    if let Some(rest) = t.strip_prefix('+') {
        return Arg::Label {
            add: true,
            name: unquote(rest),
        };
    }
    if let Some(rest) = t.strip_prefix('-')
        && !rest.is_empty()
    {
        return Arg::Label {
            add: false,
            name: unquote(rest),
        };
    }
    if let Some(n) = t.strip_prefix('#').and_then(|d| d.parse().ok()) {
        return Arg::Issue(n);
    }
    if let Some(u) = t.strip_prefix('@')
        && !u.is_empty()
    {
        return Arg::User(u.to_string());
    }
    Arg::Word(unquote(t))
}

/// The prose of a Markdown document as lines: text outside code, HTML and
/// blockquotes, split at line breaks and block ends.
fn prose_lines(body: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut skip_depth = 0usize;
    let flush = |cur: &mut String, lines: &mut Vec<String>| {
        if !cur.trim().is_empty() {
            lines.push(std::mem::take(cur));
        } else {
            cur.clear();
        }
    };
    for ev in Parser::new_ext(body, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH) {
        match ev {
            Event::Start(Tag::BlockQuote(_) | Tag::CodeBlock(_) | Tag::HtmlBlock) => {
                flush(&mut cur, &mut lines);
                skip_depth += 1;
            }
            Event::End(TagEnd::BlockQuote(_) | TagEnd::CodeBlock | TagEnd::HtmlBlock) => {
                skip_depth = skip_depth.saturating_sub(1);
                cur.clear();
            }
            _ if skip_depth > 0 => {}
            Event::Text(t) => cur.push_str(&t),
            Event::SoftBreak | Event::HardBreak => flush(&mut cur, &mut lines),
            Event::End(_) => flush(&mut cur, &mut lines),
            Event::Start(_) => {}
            // Inline code and inline HTML are never commands. Keep a space
            // so the words around them stay apart.
            Event::Code(_) | Event::InlineHtml(_) | Event::Html(_) => cur.push(' '),
            _ => {}
        }
    }
    flush(&mut cur, &mut lines);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(body: &str) -> Vec<String> {
        parse(body, "lumenfx-bot")
            .into_iter()
            .map(|c| c.name)
            .collect()
    }

    #[test]
    fn finds_a_command_anywhere_in_a_line() {
        assert_eq!(names("@lumenfx-bot ready"), ["ready"]);
        assert_eq!(names("Fixed it. @lumenfx-bot ready"), ["ready"]);
        assert_eq!(names("@LumenFX-Bot READY"), ["ready"]);
    }

    #[test]
    fn several_commands_one_per_line() {
        assert_eq!(
            names(
                "@lumenfx-bot label +A-Text\nthanks\n@lumenfx-bot blocked #4\n\n@lumenfx-bot help"
            ),
            ["label", "blocked", "help"]
        );
    }

    #[test]
    fn ignored_in_code_quotes_and_html() {
        assert!(names("`@lumenfx-bot ready`").is_empty());
        assert!(names("```\n@lumenfx-bot ready\n```").is_empty());
        assert!(names("    @lumenfx-bot ready").is_empty());
        assert!(names("> @lumenfx-bot ready").is_empty());
        assert!(names("> quoted\n> @lumenfx-bot ready\n\nmine").is_empty());
        assert!(names("<details>\n@lumenfx-bot ready\n</details>").is_empty());
        assert!(names("<!-- @lumenfx-bot ready -->").is_empty());
    }

    #[test]
    fn not_a_mention_of_something_else() {
        assert!(names("@lumenfx-botty ready").is_empty());
        assert!(names("me@lumenfx-bot ready").is_empty());
        assert!(names("@lumenfx-bot[bot] ready").is_empty());
        assert!(names("@lumenfx-bot").is_empty());
        assert!(names("@lumenfx-bot +A-Text").is_empty());
    }

    #[test]
    fn a_bare_word_is_never_a_command() {
        assert!(names("ready").is_empty());
        assert!(names("recheck").is_empty());
        assert!(names("I have read the CLA Document and I hereby sign the CLA").is_empty());
        assert!(names("lumenfx-bot ready").is_empty());
    }

    #[test]
    fn arguments() {
        let c = &parse(
            "@lumenfx-bot label +A-Text -\"good first issue\" +\"C-Bug\"",
            "lumenfx-bot",
        )[0];
        assert_eq!(
            c.args,
            vec![
                Arg::Label {
                    add: true,
                    name: "A-Text".into()
                },
                Arg::Label {
                    add: false,
                    name: "good first issue".into()
                },
                Arg::Label {
                    add: true,
                    name: "C-Bug".into()
                },
            ]
        );
        let c = &parse("@lumenfx-bot blocked #12 @someone word", "lumenfx-bot")[0];
        assert_eq!(
            c.args,
            vec![
                Arg::Issue(12),
                Arg::User("someone".into()),
                Arg::Word("word".into())
            ]
        );
    }

    #[test]
    fn the_key_ignores_spacing() {
        let a = &parse("@lumenfx-bot   concern   naming", "lumenfx-bot")[0];
        let b = &parse("@lumenfx-bot concern naming", "lumenfx-bot")[0];
        assert_eq!(a.key(), b.key());
        assert_eq!(a.text, "concern naming");
    }

    #[test]
    fn list_items_and_headings_are_prose() {
        assert_eq!(
            names("- @lumenfx-bot ready\n- @lumenfx-bot help"),
            ["ready", "help"]
        );
        assert_eq!(names("# @lumenfx-bot help"), ["help"]);
    }
}
