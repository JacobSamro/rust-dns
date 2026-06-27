//! Blocked-domain matching.
//!
//! Two kinds of entries:
//!   * a plain domain (`facebook.com`) blocks the apex **and** every subdomain
//!     (`www.facebook.com`, `m.facebook.com`, …).
//!   * a wildcard (`*.example.com`) blocks **subdomains only** — the apex
//!     `example.com` keeps resolving.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;

#[derive(Default)]
pub struct Blocklist {
    /// Apex + all subdomains.
    exact: HashSet<String>,
    /// Strict subdomains only (apex excluded). Stored without the `*.` prefix.
    wildcard: HashSet<String>,
}

impl Blocklist {
    pub fn from_file(path: &Path) -> Result<Blocklist> {
        if !path.exists() {
            std::fs::write(path, DEFAULT_BLOCKLIST)
                .with_context(|| format!("creating blocklist {}", path.display()))?;
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading blocklist {}", path.display()))?;
        Ok(Blocklist::from_text(&text))
    }

    pub fn from_text(text: &str) -> Blocklist {
        let mut exact = HashSet::new();
        let mut wildcard = HashSet::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("*.") {
                let d = normalize(rest);
                if !d.is_empty() {
                    wildcard.insert(d);
                }
            } else {
                let d = normalize(line);
                if !d.is_empty() {
                    exact.insert(d);
                }
            }
        }
        Blocklist { exact, wildcard }
    }

    /// `name` must already be lowercase and without a trailing dot.
    pub fn is_blocked(&self, name: &str) -> bool {
        // Exact entries: match the name itself and every ancestor.
        let mut suffix = name;
        loop {
            if self.exact.contains(suffix) {
                return true;
            }
            match suffix.find('.') {
                Some(i) => suffix = &suffix[i + 1..],
                None => break,
            }
        }

        // Wildcard entries: match only strict ancestors (never the name itself),
        // so `*.example.com` blocks `www.example.com` but not `example.com`.
        if !self.wildcard.is_empty() {
            if let Some(i) = name.find('.') {
                let mut parent = &name[i + 1..];
                loop {
                    if self.wildcard.contains(parent) {
                        return true;
                    }
                    match parent.find('.') {
                        Some(j) => parent = &parent[j + 1..],
                        None => break,
                    }
                }
            }
        }

        false
    }

    pub fn len(&self) -> usize {
        self.exact.len() + self.wildcard.len()
    }

    /// Sorted list for display / serialization. Wildcard entries keep their
    /// `*.` prefix so they round-trip through the file and UI.
    pub fn to_sorted_vec(&self) -> Vec<String> {
        let mut v: Vec<String> = self.exact.iter().cloned().collect();
        v.extend(self.wildcard.iter().map(|d| format!("*.{d}")));
        v.sort();
        v
    }

    pub fn to_file_text(&self) -> String {
        let mut s = self.to_sorted_vec().join("\n");
        s.push('\n');
        s
    }
}

/// Lowercase, strip scheme/path/port/trailing dot if a user pastes a URL.
fn normalize(raw: &str) -> String {
    let mut d = raw.trim().to_lowercase();
    if let Some(idx) = d.find("://") {
        d = d[idx + 3..].to_string();
    }
    if let Some(idx) = d.find('/') {
        d = d[..idx].to_string();
    }
    if let Some(idx) = d.find(':') {
        d = d[..idx].to_string();
    }
    d.trim_end_matches('.').trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::Blocklist;

    #[test]
    fn plain_blocks_apex_and_subdomains() {
        let bl = Blocklist::from_text("facebook.com");
        assert!(bl.is_blocked("facebook.com"));
        assert!(bl.is_blocked("www.facebook.com"));
        assert!(bl.is_blocked("a.b.facebook.com"));
        assert!(!bl.is_blocked("notfacebook.com"));
        assert!(!bl.is_blocked("example.com"));
    }

    #[test]
    fn wildcard_blocks_subdomains_only() {
        let bl = Blocklist::from_text("*.example.com");
        assert!(!bl.is_blocked("example.com")); // apex still resolves
        assert!(bl.is_blocked("www.example.com"));
        assert!(bl.is_blocked("deep.cdn.example.com"));
        assert!(!bl.is_blocked("example.org"));
    }

    #[test]
    fn url_and_comments_are_normalized() {
        let bl = Blocklist::from_text("# comment\nhttps://www.youtube.com/watch?v=1\n\n");
        assert!(bl.is_blocked("www.youtube.com"));
        assert!(bl.is_blocked("x.www.youtube.com"));
        assert!(!bl.is_blocked("youtube.com"));
    }

    #[test]
    fn roundtrips_wildcard_prefix() {
        let bl = Blocklist::from_text("zzz.com\n*.example.com");
        let v = bl.to_sorted_vec();
        assert!(v.contains(&"*.example.com".to_string()));
        assert!(v.contains(&"zzz.com".to_string()));
        assert_eq!(bl.len(), 2);
    }
}

const DEFAULT_BLOCKLIST: &str = "\
# rust-dns blocklist — one domain per line.
# A plain domain blocks the apex and all subdomains:
#   facebook.com   -> blocks facebook.com, www.facebook.com, m.facebook.com, …
# A wildcard blocks subdomains only (apex still resolves):
#   *.example.com  -> blocks www.example.com but NOT example.com
# Lines starting with # are comments.
facebook.com
";
