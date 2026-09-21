use std::{
    ffi::OsStr,
    path::{Component, Path, PathBuf},
};

use globset::{GlobBuilder, GlobMatcher};
use ignore::types::{Types, TypesBuilder};
use serde::{Deserialize, Serialize};

use crate::path::ResolvedPath;

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(untagged)]
pub enum GlobPatterns {
    Single(String),
    List(Vec<String>),
}

impl GlobPatterns {
    #[must_use]
    pub fn as_slice(&self) -> &[String] {
        match self {
            Self::Single(pattern) => std::slice::from_ref(pattern),
            Self::List(patterns) => patterns,
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }

    #[must_use]
    pub fn first(&self) -> Option<&str> {
        self.as_slice().first().map(String::as_str)
    }

    /// Validate pattern syntax across single or multiple patterns.
    ///
    /// # Errors
    ///
    /// Returns an error if any pattern is empty, contains NUL, or is a bare `!`.
    pub fn validate(&self) -> Result<(), String> {
        let slice = self.as_slice();
        if slice.is_empty() {
            return Err("pattern must not be empty".to_owned());
        }
        for pattern in slice {
            if pattern.is_empty() {
                return Err("pattern must not be empty".to_owned());
            }
            if pattern.contains('\0') {
                return Err("pattern must not contain NUL".to_owned());
            }
            if pattern == "!" {
                return Err("`!` must be followed by a glob pattern".to_owned());
            }
        }
        Ok(())
    }
}

impl From<&str> for GlobPatterns {
    fn from(value: &str) -> Self {
        Self::Single(value.to_owned())
    }
}

impl From<String> for GlobPatterns {
    fn from(value: String) -> Self {
        Self::Single(value)
    }
}

impl From<Vec<String>> for GlobPatterns {
    fn from(value: Vec<String>) -> Self {
        Self::List(value)
    }
}

#[derive(Clone, Debug)]
pub struct CompiledGlobRule {
    matcher: GlobMatcher,
    is_basename: bool,
}

#[derive(Clone, Debug)]
pub struct CompiledGlobFilter {
    positive_rules: Vec<CompiledGlobRule>,
    negative_rules: Vec<CompiledGlobRule>,
}

impl CompiledGlobFilter {
    /// Compile scalar or array glob patterns into a multi-rule filter.
    ///
    /// # Errors
    ///
    /// Returns an error if validation fails or any pattern syntax is invalid.
    pub fn compile(patterns: &GlobPatterns) -> Result<Self, String> {
        patterns.validate()?;
        let mut positive_rules = Vec::new();
        let mut negative_rules = Vec::new();

        for raw in patterns.as_slice() {
            let (is_negative, pat) = if let Some(stripped) = raw.strip_prefix('!') {
                (true, stripped)
            } else {
                (false, raw.as_str())
            };
            if pat.is_empty() {
                return Err("`!` must be followed by a glob pattern".to_owned());
            }
            let is_basename = !pat.contains('/');
            let matcher = GlobBuilder::new(pat)
                .literal_separator(true)
                .backslash_escape(false)
                .build()
                .map_err(|error| error.to_string())?
                .compile_matcher();

            let rule = CompiledGlobRule {
                matcher,
                is_basename,
            };
            if is_negative {
                negative_rules.push(rule);
            } else {
                positive_rules.push(rule);
            }
        }

        Ok(Self {
            positive_rules,
            negative_rules,
        })
    }

    #[must_use]
    pub fn is_match(&self, file_name: &OsStr, match_path: &Path, repo_key: Option<&Path>) -> bool {
        for rule in &self.negative_rules {
            if rule.is_basename {
                if rule.matcher.is_match(file_name) {
                    return false;
                }
            } else {
                if rule.matcher.is_match(match_path) {
                    return false;
                }
                if let Some(key) = repo_key {
                    if rule.matcher.is_match(key) {
                        return false;
                    }
                }
            }
        }

        if self.positive_rules.is_empty() {
            return true;
        }

        for rule in &self.positive_rules {
            if rule.is_basename {
                if rule.matcher.is_match(file_name) {
                    return true;
                }
            } else {
                if rule.matcher.is_match(match_path) {
                    return true;
                }
                if let Some(key) = repo_key {
                    if rule.matcher.is_match(key) {
                        return true;
                    }
                }
            }
        }

        false
    }
}

#[must_use]
pub fn resolve_literal_prefix(patterns: &GlobPatterns, base: &ResolvedPath) -> Option<PathBuf> {
    let slice = patterns.as_slice();
    let mut positive_patterns = slice.iter().filter(|pat| !pat.starts_with('!'));
    let first_positive = positive_patterns.next()?;
    if positive_patterns.next().is_some() {
        return None;
    }
    if !first_positive.contains('/') {
        return None;
    }
    let raw_prefix = literal_path_prefix(first_positive)?;
    let base_key = base.key();
    if let Ok(stripped) = raw_prefix.strip_prefix(base_key) {
        if !stripped.as_os_str().is_empty() {
            return Some(stripped.to_path_buf());
        }
    }
    Some(raw_prefix)
}

#[must_use]
pub fn literal_path_prefix(pattern: &str) -> Option<PathBuf> {
    let mut prefix = PathBuf::new();
    for component in Path::new(pattern).components() {
        let Component::Normal(component) = component else {
            return None;
        };
        if component
            .to_string_lossy()
            .chars()
            .any(|character| matches!(character, '*' | '?' | '[' | ']' | '{' | '}'))
        {
            break;
        }
        prefix.push(component);
    }
    (!prefix.as_os_str().is_empty()).then_some(prefix)
}

/// Compile an optional file type filter name (e.g. `rust`, `python`, `ts`).
///
/// # Errors
///
/// Returns an error if the type name is not recognized by the `ignore` type catalog.
pub fn build_type_filter(file_type: Option<&str>) -> Result<Option<Types>, String> {
    let Some(file_type) = file_type else {
        return Ok(None);
    };
    let mut builder = TypesBuilder::new();
    builder.add_defaults();
    builder.select(file_type);
    builder.build().map(Some).map_err(|_| {
        format!("unknown file type: \"{file_type}\"; use standard types like rust, python, js, ts, go, java")
    })
}
