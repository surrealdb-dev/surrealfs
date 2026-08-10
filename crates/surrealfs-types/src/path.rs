//! Repository-relative paths.
//!
//! Canonical form: absolute, `/`-separated, UTF-8, no `.`/`..`/empty components, no NUL,
//! no trailing slash except the root `/`. Byte-case-sensitive comparison.

use serde::{Deserialize, Serialize};

use crate::error::SfsError;

pub const MAX_COMPONENT_BYTES: usize = 255;
pub const MAX_PATH_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoPath(String);

impl RepoPath {
    pub fn root() -> Self {
        RepoPath("/".to_string())
    }

    /// Parse and normalize. Accepts paths with or without a leading `/`.
    pub fn parse(input: &str) -> Result<Self, SfsError> {
        if input.contains('\0') {
            return Err(SfsError::InvalidPath("path contains NUL".into()));
        }
        let trimmed = input.trim_start_matches('/');
        if trimmed.is_empty() {
            return Ok(Self::root());
        }
        let mut parts: Vec<&str> = Vec::new();
        for comp in trimmed.split('/') {
            match comp {
                "" | "." => {
                    return Err(SfsError::InvalidPath(format!(
                        "empty or dot component in {input:?}"
                    )))
                }
                ".." => {
                    return Err(SfsError::InvalidPath(format!(
                        "parent traversal in {input:?}"
                    )))
                }
                c if c.len() > MAX_COMPONENT_BYTES => {
                    return Err(SfsError::InvalidPath(format!(
                        "component over {MAX_COMPONENT_BYTES} bytes in {input:?}"
                    )))
                }
                c => parts.push(c),
            }
        }
        let normalized = format!("/{}", parts.join("/"));
        if normalized.len() > MAX_PATH_BYTES {
            return Err(SfsError::InvalidPath(format!(
                "path over {MAX_PATH_BYTES} bytes"
            )));
        }
        Ok(RepoPath(normalized))
    }

    pub fn is_root(&self) -> bool {
        self.0 == "/"
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parent path; `None` for the root.
    pub fn parent(&self) -> Option<RepoPath> {
        if self.is_root() {
            return None;
        }
        match self.0.rfind('/') {
            Some(0) => Some(RepoPath::root()),
            Some(i) => Some(RepoPath(self.0[..i].to_string())),
            None => None,
        }
    }

    /// Final component; `None` for the root.
    pub fn file_name(&self) -> Option<&str> {
        if self.is_root() {
            None
        } else {
            self.0.rsplit('/').next()
        }
    }

    /// All ancestor directories from `/` down to the immediate parent.
    pub fn ancestors(&self) -> Vec<RepoPath> {
        let mut out = Vec::new();
        let mut cur = self.parent();
        while let Some(p) = cur {
            cur = p.parent();
            out.push(p);
        }
        out.reverse();
        out
    }

    /// True if `self` is `other` or lies underneath it.
    pub fn starts_with(&self, other: &RepoPath) -> bool {
        if other.is_root() {
            return true;
        }
        self.0 == other.0 || self.0.starts_with(&format!("{}/", other.0))
    }

    pub fn join(&self, name: &str) -> Result<RepoPath, SfsError> {
        RepoPath::parse(&format!("{}/{}", self.0, name))
    }
}

impl std::fmt::Display for RepoPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_and_validates() {
        assert_eq!(RepoPath::parse("a/b").unwrap().as_str(), "/a/b");
        assert_eq!(RepoPath::parse("/a/b").unwrap().as_str(), "/a/b");
        assert_eq!(RepoPath::parse("/").unwrap().as_str(), "/");
        assert_eq!(RepoPath::parse("").unwrap().as_str(), "/");
        assert!(RepoPath::parse("a//b").is_err());
        assert!(RepoPath::parse("a/./b").is_err());
        assert!(RepoPath::parse("a/../b").is_err());
        assert!(RepoPath::parse("a\0b").is_err());
    }

    #[test]
    fn parent_and_name() {
        let p = RepoPath::parse("/a/b/c").unwrap();
        assert_eq!(p.parent().unwrap().as_str(), "/a/b");
        assert_eq!(p.file_name().unwrap(), "c");
        assert_eq!(
            p.ancestors()
                .iter()
                .map(|a| a.as_str().to_string())
                .collect::<Vec<_>>(),
            vec!["/", "/a", "/a/b"]
        );
        assert!(RepoPath::root().parent().is_none());
    }
}
