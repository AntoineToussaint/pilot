//! Parse "Fixes #123" / "Closes ENG-45" / "Resolves owner/repo#7"
//! style links out of a PR body. Used to auto-link issues to a
//! workspace at attach time.
//!
//! ## What's recognized
//!
//! GitHub's documented closing-keyword set + a few common synonyms:
//! `close`, `closes`, `closed`, `fix`, `fixes`, `fixed`, `resolve`,
//! `resolves`, `resolved`. Case-insensitive. Must be followed by
//! whitespace and one of:
//!
//! - `#123` — same-repo GitHub issue
//! - `owner/repo#123` — cross-repo GitHub issue
//! - `ENG-45`, `ENG-456`, `ABC-1` — Linear-style ticket key
//!
//! Markdown links like `[ENG-45](https://linear.app/.../ENG-45)` are
//! parsed via the linear-key path: we only need the key to find the
//! task in the polled set.
//!
//! ## What's NOT recognized
//!
//! Plain bare `#123` without a closing keyword (too noisy — many
//! PRs reference issues without intending to close them). The user
//! can always attach manually if they want a non-closing link.

use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IssueLink {
    /// `#123` or `owner/repo#123`. `repo` is the optional explicit
    /// owner/repo; `None` means same-repo as the PR.
    GitHub { repo: Option<String>, number: u64 },
    /// `ENG-45`. Caller maps the prefix to a Linear team if needed.
    Linear { key: String },
}

const KEYWORDS: &[&str] = &[
    "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
];

/// Pull every `IssueLink` mention out of `body`. Order is preserved
/// in a sorted set for deterministic output (lets tests rely on the
/// shape and means re-parsing the same body returns the same value).
pub fn extract(body: &str) -> Vec<IssueLink> {
    let mut out = BTreeSet::new();
    for token in tokenize(body) {
        if let Some(link) = parse_link_after_keyword(&token) {
            out.insert(link);
        }
    }
    out.into_iter().collect()
}

/// Split the body into "after-keyword" candidate strings. We look at
/// every position after a closing keyword and grab the next ~80 chars
/// to scan, since "Fixes #1, #2, #3" should hit all three.
fn tokenize(body: &str) -> Vec<String> {
    let lower = body.to_lowercase();
    let mut tokens = Vec::new();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        for kw in KEYWORDS {
            if lower[i..].starts_with(kw) {
                let after = i + kw.len();
                if after >= bytes.len() {
                    break;
                }
                let next = bytes[after];
                // Need a separator after the keyword so we don't match
                // inside other words ("foreclosed" shouldn't match).
                if !next.is_ascii_whitespace() && next != b':' {
                    continue;
                }
                let end = (after + 80).min(body.len());
                tokens.push(body[after..end].to_string());
            }
        }
        i += 1;
    }
    tokens
}

fn parse_link_after_keyword(s: &str) -> Option<IssueLink> {
    // Strip leading whitespace + colon.
    let s = s.trim_start_matches(|c: char| c.is_whitespace() || c == ':');

    // GitHub same-repo: `#123`
    if let Some(rest) = s.strip_prefix('#')
        && let Some(num) = take_digits(rest)
    {
        return Some(IssueLink::GitHub {
            repo: None,
            number: num,
        });
    }

    // GitHub cross-repo: `owner/repo#123`
    if let Some((repo, rest)) = split_repo_hash(s)
        && let Some(num) = take_digits(rest)
    {
        return Some(IssueLink::GitHub {
            repo: Some(repo),
            number: num,
        });
    }

    // Linear: `ABC-123`. ABC must be 2-10 alpha chars.
    if let Some(key) = take_linear_key(s) {
        return Some(IssueLink::Linear { key });
    }

    None
}

fn take_digits(s: &str) -> Option<u64> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

fn split_repo_hash(s: &str) -> Option<(String, &str)> {
    let hash_at = s.find('#')?;
    let repo = &s[..hash_at];
    if !repo
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '-' || c == '_' || c == '.')
        || !repo.contains('/')
    {
        return None;
    }
    Some((repo.to_string(), &s[hash_at + 1..]))
}

fn take_linear_key(s: &str) -> Option<String> {
    let mut chars = s.chars();
    let prefix: String = chars
        .by_ref()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    if !(2..=10).contains(&prefix.len()) {
        return None;
    }
    // Need a hyphen separator.
    let after_prefix = &s[prefix.len()..];
    let rest = after_prefix.strip_prefix('-')?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let prefix_upper = prefix.to_uppercase();
    Some(format!("{prefix_upper}-{digits}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_simple_same_repo_issue() {
        let body = "This PR fixes #42.";
        assert_eq!(
            extract(body),
            vec![IssueLink::GitHub {
                repo: None,
                number: 42
            }]
        );
    }

    #[test]
    fn extracts_cross_repo_issue() {
        let body = "Closes acme/widgets#7";
        assert_eq!(
            extract(body),
            vec![IssueLink::GitHub {
                repo: Some("acme/widgets".into()),
                number: 7
            }]
        );
    }

    #[test]
    fn extracts_linear_ticket() {
        let body = "Resolves ENG-456 with the new flow.";
        assert_eq!(
            extract(body),
            vec![IssueLink::Linear {
                key: "ENG-456".into()
            }]
        );
    }

    #[test]
    fn extracts_multiple_links_in_one_body() {
        let body = "Fixes #1\n\nAlso closes ENG-2 and resolves acme/foo#99.";
        let links = extract(body);
        assert!(links.contains(&IssueLink::GitHub {
            repo: None,
            number: 1
        }));
        assert!(links.contains(&IssueLink::Linear {
            key: "ENG-2".into()
        }));
        assert!(links.contains(&IssueLink::GitHub {
            repo: Some("acme/foo".into()),
            number: 99
        }));
    }

    #[test]
    fn ignores_bare_hash_without_keyword() {
        // "PR #5" alone shouldn't auto-link — only a closing keyword
        // is intentional enough to mean "this workspace is for that
        // issue."
        assert!(extract("Built on top of #5").is_empty());
    }

    #[test]
    fn ignores_keyword_inside_other_words() {
        // "foreclosed" contains "closed" but isn't a verb form.
        assert!(extract("This branch was foreclosed#42 incorrectly").is_empty());
    }

    #[test]
    fn handles_colon_separator() {
        let body = "Fixes: #99";
        assert_eq!(
            extract(body),
            vec![IssueLink::GitHub {
                repo: None,
                number: 99
            }]
        );
    }

    #[test]
    fn case_insensitive_keyword_matching() {
        let body = "FIXES #1\nFixed #2\nfix #3";
        let links = extract(body);
        for n in [1u64, 2, 3] {
            assert!(links.contains(&IssueLink::GitHub {
                repo: None,
                number: n
            }));
        }
    }

    #[test]
    fn deduplicates_repeated_mentions() {
        let body = "Fixes #1. Also fixes #1 because we mean it.";
        assert_eq!(
            extract(body),
            vec![IssueLink::GitHub {
                repo: None,
                number: 1
            }]
        );
    }

    #[test]
    fn empty_body_returns_empty() {
        assert!(extract("").is_empty());
    }

    #[test]
    fn linear_prefix_too_short_or_long_rejected() {
        assert!(extract("Fixes A-1").is_empty());
        assert!(extract("Fixes ABCDEFGHIJK-1").is_empty());
    }
}
