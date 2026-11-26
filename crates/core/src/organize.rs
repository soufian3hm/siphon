//! Where files go and how individual sites are treated.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::types::Category;

/// Preferences for one file family.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryRule {
    pub category: Category,
    /// `None` uses a folder named after the family inside the download folder.
    #[serde(default)]
    pub folder: Option<PathBuf>,
    /// Extensions filed here in addition to the built-in ones: lower case,
    /// without the dot.
    #[serde(default)]
    pub extensions: Vec<String>,
}

impl CategoryRule {
    pub fn new(category: Category) -> Self {
        Self { category, folder: None, extensions: Vec::new() }
    }
}

/// One rule per family, in display order.
pub fn default_rules() -> Vec<CategoryRule> {
    Category::ALL.iter().copied().map(CategoryRule::new).collect()
}

/// Makes a rule list whole: every family exactly once, in display order,
/// with extensions normalised.
pub fn normalize_rules(rules: Vec<CategoryRule>) -> Vec<CategoryRule> {
    Category::ALL
        .iter()
        .map(|category| {
            let mut rule = rules.iter().find(|r| r.category == *category).cloned().unwrap_or_else(|| CategoryRule::new(*category));
            rule.extensions = parse_extensions(&rule.extensions.join(" "));
            if rule.folder.as_ref().is_some_and(|f| f.as_os_str().is_empty()) {
                rule.folder = None;
            }
            rule
        })
        .collect()
}

/// Splits what a person types into a list of extensions: `".iso, IMG  bin"`
/// becomes `["iso", "img", "bin"]`.
pub fn parse_extensions(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for token in text.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';')) {
        let ext = token.trim().trim_start_matches(['*', '.']).to_ascii_lowercase();
        let valid = !ext.is_empty() && ext.len() <= 16 && ext.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+'));
        if valid && !out.contains(&ext) {
            out.push(ext);
        }
    }
    out
}

/// The family of a file, honouring the user's extra extensions first.
pub fn category_of(rules: &[CategoryRule], file_name: &str) -> Category {
    if let Some((stem, ext)) = file_name.rsplit_once('.')
        && !stem.is_empty()
    {
        let ext = ext.to_ascii_lowercase();
        if let Some(rule) = rules.iter().find(|r| r.extensions.contains(&ext)) {
            return rule.category;
        }
    }
    Category::of(file_name)
}

/// How one site is treated.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SiteRule {
    /// `example.com` (the host and everything under it) or a pattern with
    /// `*` standing for any run of characters, e.g. `*.cdn.example.com`.
    pub pattern: String,
    /// Connections per download for this site. `None` follows the global
    /// setting. Servers that allow one or two connections per client need this.
    pub connections: Option<usize>,
    /// Account sent to this site (HTTP Basic, or the FTP login). The password
    /// lives in the system keychain, never in the settings file.
    pub username: String,
}

impl SiteRule {
    pub fn matches(&self, host: &str) -> bool {
        host_matches(&self.pattern, host)
    }

    /// A rule that would change nothing is not worth keeping.
    pub fn is_inert(&self) -> bool {
        self.pattern.trim().is_empty() || (self.connections.is_none() && self.username.trim().is_empty())
    }
}

/// Lower-cased host of an address, without credentials or port.
pub fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url.trim()).ok()?.host_str().map(|h| h.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase())
}

/// Tidies a pattern typed by a person: a pasted address becomes its host.
pub fn normalize_pattern(text: &str) -> String {
    let text = text.trim().to_ascii_lowercase();
    if text.contains("://")
        && let Some(host) = host_of(&text)
    {
        return host;
    }
    text.trim_end_matches('/').to_owned()
}

/// Matches a host against a site pattern, ignoring case.
pub fn host_matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.trim().to_ascii_lowercase();
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if pattern.is_empty() || host.is_empty() {
        return false;
    }
    if !pattern.contains('*') {
        return host == pattern || host.strip_suffix(pattern.as_str()).is_some_and(|rest| rest.ends_with('.'));
    }
    glob(pattern.as_bytes(), host.as_bytes())
}

/// Wildcard match where `*` is any run of bytes, in linear space.
fn glob(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && pattern[p] == b'*' {
            star = Some((p, t));
            p += 1;
        } else if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
        } else if let Some((sp, st)) = star {
            // Let the last star swallow one more byte and try again.
            p = sp + 1;
            t = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_patterns_cover_the_host_and_its_subdomains() {
        assert!(host_matches("example.com", "example.com"));
        assert!(host_matches("example.com", "dl.cdn.Example.com"));
        assert!(host_matches("Example.COM", "example.com."));
        assert!(!host_matches("example.com", "notexample.com"));
        assert!(!host_matches("example.com", "example.com.evil.net"));
        assert!(!host_matches("", "example.com"));
    }

    #[test]
    fn wildcards() {
        assert!(host_matches("*.example.com", "a.example.com"));
        assert!(!host_matches("*.example.com", "example.com"));
        assert!(host_matches("cdn*.example.*", "cdn42.example.org"));
        assert!(host_matches("*", "anything.at.all"));
        assert!(host_matches("192.168.*", "192.168.1.20"));
        assert!(!host_matches("cdn*.example.com", "static.example.com"));
        assert!(host_matches("*a*b*", "xxaxxbxx"));
        assert!(!host_matches("*a*b", "xxbxxa"));
    }

    #[test]
    fn hosts_and_patterns_from_addresses() {
        assert_eq!(host_of("https://User:pw@Files.Example.com:8443/a/b.zip?x=1").as_deref(), Some("files.example.com"));
        assert_eq!(host_of("ftp://[2001:db8::1]/pub/x").as_deref(), Some("2001:db8::1"));
        assert_eq!(host_of("not a url"), None);
        assert_eq!(normalize_pattern(" https://Files.Example.com/downloads/ "), "files.example.com");
        assert_eq!(normalize_pattern("*.Example.com/"), "*.example.com");
    }

    #[test]
    fn extra_extensions_win_over_the_built_in_table() {
        let mut rules = default_rules();
        rules.iter_mut().find(|r| r.category == Category::Archive).unwrap().extensions = parse_extensions(".safetensors, GGUF ;bin");
        assert_eq!(category_of(&rules, "model.safetensors"), Category::Archive);
        assert_eq!(category_of(&rules, "weights.GGUF"), Category::Archive);
        // `bin` is a program in the built-in table; the user's rule moves it.
        assert_eq!(category_of(&rules, "firmware.bin"), Category::Archive);
        assert_eq!(category_of(&rules, "movie.mkv"), Category::Video);
        assert_eq!(category_of(&rules, ".hidden"), Category::Other);
    }

    #[test]
    fn rule_lists_are_made_whole() {
        let partial = vec![
            CategoryRule { category: Category::Video, folder: Some(PathBuf::from("/Volumes/Media")), extensions: vec![".M2V".into(), "m2v".into()] },
            CategoryRule { category: Category::Audio, folder: Some(PathBuf::new()), extensions: vec![] },
        ];
        let rules = normalize_rules(partial);
        assert_eq!(rules.len(), Category::ALL.len());
        assert_eq!(rules[0].folder.as_deref(), Some(std::path::Path::new("/Volumes/Media")));
        assert_eq!(rules[0].extensions, vec!["m2v"]);
        assert_eq!(rules[1].folder, None);
        assert_eq!(parse_extensions("a/b, ../x, ok"), vec!["ok"]);
    }

    #[test]
    fn inert_rules() {
        assert!(SiteRule { pattern: "a.example".into(), ..Default::default() }.is_inert());
        assert!(!SiteRule { pattern: "a.example".into(), connections: Some(1), ..Default::default() }.is_inert());
        assert!(SiteRule { pattern: " ".into(), connections: Some(1), ..Default::default() }.is_inert());
    }
}
