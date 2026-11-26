//! Many addresses at once: links pulled out of free text, and patterns that
//! stand for a numbered series.
//!
//! The pattern syntax is the one `curl` made familiar:
//!
//! * `[1-100]` counts from 1 to 100
//! * `[001-100]` does the same with leading zeros (`001`, `002`, ...)
//! * `[0-100:10]` counts in steps of ten
//! * `[a-z]` walks letters
//! * `{mon,tue,wed}` picks from a list
//!
//! Several patterns in one address multiply.

/// More than this from one line is almost certainly a typo.
pub const MAX_EXPANSION: usize = 10_000;

const SCHEMES: [&str; 7] = ["http://", "https://", "ftp://", "ftps://", "mms://", "mmsh://", "mmst://"];

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BatchError {
    #[error("this pattern stands for {0} addresses; the most allowed at once is {MAX_EXPANSION}")]
    TooMany(u128),
}

/// One piece of a parsed pattern.
#[derive(Debug, PartialEq, Eq)]
enum Part {
    Literal(String),
    Choices(Vec<String>),
}

impl Part {
    fn len(&self) -> u128 {
        match self {
            Part::Literal(_) => 1,
            Part::Choices(c) => c.len() as u128,
        }
    }
}

/// True when the address contains something [`expand`] would multiply.
pub fn is_pattern(address: &str) -> bool {
    parse(address).iter().any(|p| matches!(p, Part::Choices(_)))
}

/// Expands one address into the series it stands for. An address without a
/// pattern comes back as itself.
pub fn expand(address: &str) -> Result<Vec<String>, BatchError> {
    let parts = parse(address.trim());
    let count = parts.iter().map(Part::len).fold(1u128, |a, b| a.saturating_mul(b));
    if count > MAX_EXPANSION as u128 {
        return Err(BatchError::TooMany(count));
    }
    let mut out = vec![String::new()];
    for part in &parts {
        match part {
            Part::Literal(text) => out.iter_mut().for_each(|s| s.push_str(text)),
            Part::Choices(choices) => {
                let mut next = Vec::with_capacity(out.len() * choices.len());
                for prefix in &out {
                    for choice in choices {
                        next.push(format!("{prefix}{choice}"));
                    }
                }
                out = next;
            }
        }
    }
    Ok(out)
}

fn parse(address: &str) -> Vec<Part> {
    let mut parts: Vec<Part> = Vec::new();
    let mut literal = String::new();
    let mut rest = address;
    while let Some(open) = rest.find(['[', '{']) {
        let closer = if rest.as_bytes()[open] == b'[' { ']' } else { '}' };
        let Some(len) = rest[open + 1..].find(closer) else { break };
        let inner = &rest[open + 1..open + 1 + len];
        let choices = if closer == ']' { range(inner) } else { list(inner) };
        match choices {
            Some(choices) => {
                literal.push_str(&rest[..open]);
                if !literal.is_empty() {
                    parts.push(Part::Literal(std::mem::take(&mut literal)));
                }
                parts.push(Part::Choices(choices));
            }
            // Not a pattern (an IPv6 literal, a query that uses brackets):
            // keep the text exactly as written.
            None => literal.push_str(&rest[..open + 1 + len + 1]),
        }
        rest = &rest[open + 1 + len + 1..];
    }
    literal.push_str(rest);
    if !literal.is_empty() {
        parts.push(Part::Literal(literal));
    }
    parts
}

/// `1-100`, `001-100`, `0-100:10`, `a-z`, `a-z:2`.
fn range(inner: &str) -> Option<Vec<String>> {
    let (span, step) = match inner.split_once(':') {
        Some((span, step)) => (span, step.parse::<u64>().ok().filter(|s| *s > 0)?),
        None => (inner, 1),
    };
    let (first, last) = span.split_once('-')?;
    if first.is_empty() || last.is_empty() {
        return None;
    }

    if first.bytes().all(|b| b.is_ascii_digit()) && last.bytes().all(|b| b.is_ascii_digit()) && first.len() <= 18 && last.len() <= 18 {
        let (from, to) = (first.parse::<u64>().ok()?, last.parse::<u64>().ok()?);
        // Leading zeros on the first number ask for that width throughout.
        let width = if first.len() > 1 && first.starts_with('0') { first.len() } else { 0 };
        let count = from.abs_diff(to) / step + 1;
        if count > MAX_EXPANSION as u64 * 100 {
            // Too many to materialise; the caller reports the total.
            return Some(vec![String::new(); MAX_EXPANSION + 1]);
        }
        return Some(
            (0..count)
                .map(|i| if from <= to { from + i * step } else { from - i * step })
                .map(|n| format!("{n:0width$}"))
                .collect(),
        );
    }

    let (mut a, mut b) = (first.chars(), last.chars());
    let (from, to) = (a.next()?, b.next()?);
    let same_case = (from.is_ascii_lowercase() && to.is_ascii_lowercase()) || (from.is_ascii_uppercase() && to.is_ascii_uppercase());
    if a.next().is_some() || b.next().is_some() || !same_case {
        return None;
    }
    let (from, to) = (from as u8, to as u8);
    let count = u64::from(from.abs_diff(to)) / step + 1;
    Some((0..count).map(|i| if from <= to { from + (i * step) as u8 } else { from - (i * step) as u8 }).map(|c| (c as char).to_string()).collect())
}

/// `a,b,c` with at least two alternatives.
fn list(inner: &str) -> Option<Vec<String>> {
    let items: Vec<String> = inner.split(',').map(str::to_owned).collect();
    (items.len() >= 2).then_some(items)
}

/// Pulls every address out of free text: a pasted paragraph, a text file, the
/// source of a page.
pub fn extract_links(text: &str) -> Vec<String> {
    let mut links: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for token in text.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`')) {
        let lower = token.to_ascii_lowercase();
        if let Some(start) = SCHEMES.iter().filter_map(|s| lower.find(s)).min() {
            let link = token[start..].trim_end_matches([',', ';', ')', '.', '\\']);
            // A closing bracket only belongs to the address when it has a partner.
            let mut link = link;
            while link.ends_with(']') && link.matches(']').count() > link.matches('[').count() {
                link = link[..link.len() - 1].trim_end_matches([',', ';', ')', '.']);
            }
            if link.len() > 10 && seen.insert(link.to_owned()) {
                links.push(link.to_owned());
            }
        }
    }
    links
}

/// Everything a block of text asks for: addresses found anywhere in it, with
/// patterns expanded. Returns the addresses and the first problem met.
pub fn collect(text: &str) -> (Vec<String>, Option<BatchError>) {
    let mut out: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut problem = None;
    for link in extract_links(text) {
        match expand(&link) {
            Ok(series) => {
                for address in series {
                    if out.len() >= MAX_EXPANSION {
                        problem.get_or_insert(BatchError::TooMany(out.len() as u128 + 1));
                        return (out, problem);
                    }
                    if seen.insert(address.clone()) {
                        out.push(address);
                    }
                }
            }
            Err(e) => {
                problem.get_or_insert(e);
            }
        }
    }
    (out, problem)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_ranges() {
        assert_eq!(expand("https://a.example/p[1-3].jpg").unwrap(), vec!["https://a.example/p1.jpg", "https://a.example/p2.jpg", "https://a.example/p3.jpg"]);
        assert_eq!(expand("x[08-11]").unwrap(), vec!["x08", "x09", "x10", "x11"]);
        assert_eq!(expand("x[001-003]y").unwrap(), vec!["x001y", "x002y", "x003y"]);
        assert_eq!(expand("x[0-20:10]").unwrap(), vec!["x0", "x10", "x20"]);
        assert_eq!(expand("x[3-1]").unwrap(), vec!["x3", "x2", "x1"]);
        assert_eq!(expand("x[5-5]").unwrap(), vec!["x5"]);
        // A single zero is a number, not a width.
        assert_eq!(expand("x[0-2]").unwrap(), vec!["x0", "x1", "x2"]);
    }

    #[test]
    fn letters_and_lists() {
        assert_eq!(expand("vol-[a-c].zip").unwrap(), vec!["vol-a.zip", "vol-b.zip", "vol-c.zip"]);
        assert_eq!(expand("[A-E:2]").unwrap(), vec!["A", "C", "E"]);
        assert_eq!(expand("day-{mon,tue}.mp3").unwrap(), vec!["day-mon.mp3", "day-tue.mp3"]);
        assert_eq!(expand("s[1-2]e{01,02}").unwrap(), vec!["s1e01", "s1e02", "s2e01", "s2e02"]);
    }

    #[test]
    fn things_that_only_look_like_patterns_are_left_alone() {
        for plain in [
            "http://[2001:db8::1]/file.bin",
            "https://a.example/list?ids[]=4&tag={x}",
            "https://a.example/a[b-]c",
            "https://a.example/[a-Z]",
            "https://a.example/[1-9:0]",
            "https://a.example/[abc]",
            "https://a.example/open[1-3",
        ] {
            assert_eq!(expand(plain).unwrap(), vec![plain], "{plain}");
            assert!(!is_pattern(plain), "{plain}");
        }
        assert!(is_pattern("https://a.example/[1-3]"));
    }

    #[test]
    fn runaway_patterns_are_refused_not_expanded() {
        assert_eq!(expand("x[1-10000]").unwrap().len(), 10_000);
        assert_eq!(expand("x[1-10001]"), Err(BatchError::TooMany(10_001)));
        assert_eq!(expand("x[1-1000][1-1000]"), Err(BatchError::TooMany(1_000_000)));
        assert!(matches!(expand("x[0-999999999999999999]"), Err(BatchError::TooMany(_))));
    }

    #[test]
    fn links_are_found_in_messy_text() {
        assert_eq!(extract_links("https://a.example/file.zip"), vec!["https://a.example/file.zip"]);
        assert_eq!(
            extract_links("Get it here: <https://a.example/x.iso>, or ftp://b.example/y.bin.\nthanks"),
            vec!["https://a.example/x.iso", "ftp://b.example/y.bin"]
        );
        assert_eq!(extract_links("href=\"https://a.example/z\" https://a.example/z"), vec!["https://a.example/z"]);
        assert!(extract_links("no links here, just http:// and words").is_empty());
        // Patterns survive extraction; a stray bracket does not.
        assert_eq!(extract_links("see [https://a.example/p[1-3].jpg]"), vec!["https://a.example/p[1-3].jpg"]);
        assert_eq!(extract_links("(see https://a.example/doc.pdf)"), vec!["https://a.example/doc.pdf"]);
        assert_eq!(extract_links("[https://a.example/doc.pdf]"), vec!["https://a.example/doc.pdf"]);
    }

    #[test]
    fn collecting_expands_and_removes_repeats() {
        let (links, problem) = collect("https://a.example/p[1-3].jpg\nhttps://a.example/p2.jpg\nhttps://b.example/x");
        assert_eq!(problem, None);
        assert_eq!(links, vec!["https://a.example/p1.jpg", "https://a.example/p2.jpg", "https://a.example/p3.jpg", "https://b.example/x"]);

        let (links, problem) = collect("https://a.example/ok.zip https://a.example/x[1-99999]");
        assert_eq!(links, vec!["https://a.example/ok.zip"]);
        assert_eq!(problem, Some(BatchError::TooMany(99_999)));
    }
}
