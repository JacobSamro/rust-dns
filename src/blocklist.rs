//! Blocked-domain matching. A query is blocked if the domain or any of its
//! parent domains is in the set, so blocking `facebook.com` also blocks
//! `www.facebook.com`, `m.facebook.com`, etc.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;

#[derive(Default)]
pub struct Blocklist {
    set: HashSet<String>,
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
        let set = text
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(normalize)
            .filter(|d| !d.is_empty())
            .collect();
        Blocklist { set }
    }

    /// `name` must already be lowercase and without a trailing dot.
    pub fn is_blocked(&self, name: &str) -> bool {
        if self.set.is_empty() {
            return false;
        }
        let mut suffix = name;
        loop {
            if self.set.contains(suffix) {
                return true;
            }
            match suffix.find('.') {
                Some(i) => suffix = &suffix[i + 1..],
                None => return false,
            }
        }
    }

    pub fn len(&self) -> usize {
        self.set.len()
    }

    /// Sorted list for display / serialization back to the file.
    pub fn to_sorted_vec(&self) -> Vec<String> {
        let mut v: Vec<String> = self.set.iter().cloned().collect();
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

const DEFAULT_BLOCKLIST: &str = "\
# rust-dns blocklist — one domain per line.
# Blocking a domain also blocks all of its subdomains.
# Lines starting with # are comments.
facebook.com
";
