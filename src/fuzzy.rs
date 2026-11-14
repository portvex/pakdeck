//! A small fuzzy matcher for filtering and highlighting package names.
//!
//! The pattern's characters must appear in order in the text (not
//! necessarily adjacent). Matching is case-insensitive unless the pattern
//! has an uppercase letter (smart case). Scores favour consecutive runs,
//! matches at word starts (after `-`, `_`, `.`, `/`, `@`, `+` or a space)
//! and at the very start; gaps cost a little. Every possible start of the
//! first character is tried, keeping the best result.

/// A successful match: higher `score` is better; `positions` are the
/// matched character indices in the text (not byte offsets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub score: i64,
    pub positions: Vec<usize>,
}

const CONSECUTIVE: i64 = 15;
const WORD_START: i64 = 10;
const FIRST_CHAR: i64 = 20;
const MATCH: i64 = 1;
const GAP: i64 = 1;
/// Longer texts are cut off; package names and summaries are short.
const MAX_TEXT: usize = 512;

fn is_separator(c: char) -> bool {
    matches!(c, '-' | '_' | '.' | '/' | '@' | '+' | ' ' | ':')
}

/// Match `pattern` against `text`. An empty pattern matches everything
/// with score 0.
pub fn fuzzy_match(pattern: &str, text: &str) -> Option<Match> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return Some(Match {
            score: 0,
            positions: Vec::new(),
        });
    }
    let case_sensitive = pattern.chars().any(char::is_uppercase);
    let norm = |c: char| if case_sensitive { c } else { c.to_ascii_lowercase() };
    let pat: Vec<char> = pattern.chars().map(norm).collect();
    let txt: Vec<char> = text.chars().take(MAX_TEXT).map(norm).collect();
    let raw: Vec<char> = text.chars().take(MAX_TEXT).collect();
    let first = *pat.first()?;

    let mut best: Option<Match> = None;
    for (start, c) in txt.iter().enumerate() {
        if *c != first {
            continue;
        }
        let Some(m) = greedy_from(&pat, &txt, &raw, start) else {
            // No complete match from here, so none from any later start either.
            break;
        };
        if best.as_ref().is_none_or(|b| m.score > b.score) {
            best = Some(m);
        }
    }
    best
}

/// Match the pattern greedily, starting with its first character at `start`.
fn greedy_from(pat: &[char], txt: &[char], raw: &[char], start: usize) -> Option<Match> {
    let mut positions = Vec::with_capacity(pat.len());
    let mut score: i64 = 0;
    let mut ti = start;
    let mut prev: Option<usize> = None;
    for pc in pat {
        let found = txt.iter().skip(ti).position(|c| c == pc)?;
        let at = ti.saturating_add(found);
        score = score.saturating_add(MATCH);
        match prev {
            Some(p) if at == p.saturating_add(1) => score = score.saturating_add(CONSECUTIVE),
            Some(p) => {
                let gap = i64::try_from(at.saturating_sub(p)).unwrap_or(i64::MAX);
                score = score.saturating_sub(gap.saturating_mul(GAP));
            }
            None => {}
        }
        let word_start = at == 0
            || at
                .checked_sub(1)
                .and_then(|i| raw.get(i))
                .is_some_and(|c| is_separator(*c));
        if at == 0 {
            score = score.saturating_add(FIRST_CHAR);
        } else if word_start {
            score = score.saturating_add(WORD_START);
        }
        positions.push(at);
        prev = Some(at);
        ti = at.saturating_add(1);
    }
    // Prefer shorter texts slightly (an exact name beats a longer one).
    let len = i64::try_from(txt.len()).unwrap_or(i64::MAX);
    Some(Match {
        score: score.saturating_sub(len / 8),
        positions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(p: &str, t: &str) -> i64 {
        fuzzy_match(p, t).map_or(i64::MIN, |m| m.score)
    }

    #[test]
    fn matches_in_order() {
        assert_eq!(fuzzy_match("rg", "ripgrep").map(|m| m.positions), Some(vec![0, 3]));
        assert!(fuzzy_match("gr", "rg").is_none());
        assert!(fuzzy_match("xyz", "ripgrep").is_none());
        assert_eq!(fuzzy_match("", "anything").map(|m| m.score), Some(0));
    }

    #[test]
    fn ranking() {
        // Exact and prefix matches beat scattered ones.
        assert!(score("tree", "tree") > score("tree", "tree-sitter"));
        assert!(score("tree", "tree-sitter") > score("tree", "the-real-ecosystem"));
        // Word starts count: "nc" in "netcat" vs "n...c" across words.
        assert!(score("ps", "py-setuptools") > score("ps", "apsw"));
        // Best start is chosen, not the first occurrence.
        assert_eq!(fuzzy_match("abc", "a-x-abc").map(|m| m.positions), Some(vec![4, 5, 6]));
    }

    #[test]
    fn smart_case() {
        assert!(fuzzy_match("ff", "Firefox").is_some());
        assert!(fuzzy_match("FF", "firefox").is_none());
        assert!(fuzzy_match("Fire", "Firefox").is_some());
    }

    #[test]
    fn unicode_and_long_text_do_not_panic() {
        assert!(fuzzy_match("é", "café").is_some());
        let long = "a".repeat(10_000);
        assert!(fuzzy_match("aaa", &long).is_some());
        assert!(fuzzy_match("ab", &long).is_none());
    }
}
