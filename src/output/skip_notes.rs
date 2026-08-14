//! Shared skip notes for grep, glob, and traversal.
//!
//! Body lines are selected first. Remaining budget then samples
//! `path — reason` rows. The terminal line always reports the true total.

const MAX_SAMPLED_SKIP_ENTRIES: usize = 32;

/// Emitted when a search returns nothing and gitignore filtering was in force. The
/// filtered paths are dropped inside the walker and never reach this crate, so the
/// caller cannot be told which ones were lost — only that the flag is worth retrying.
pub(crate) const GITIGNORE_RETRY_HINT: &str =
    "Retry with include_ignored=true if the path you expect is gitignored.";

/// Emitted when a search skipped files whose encoding could not be determined. Without
/// this the caller sees only that the files were skipped, with no way to know that naming
/// their encoding would let the search reach them.
pub(crate) const UNDECODABLE_RETRY_HINT: &str =
    "Retry with fallback_encoding set to their WHATWG label to search the undecodable files.";

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub enum SkipReason {
    Binary,
    ChangedWhileSearched,
    Undecodable,
    Io,
    LineExceedsSearchHeap,
    CaptureBudget,
    Escaped,
    NonUnicodePath,
    TranscodeMemory,
}

impl SkipReason {
    #[must_use]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::ChangedWhileSearched => "changed while being searched",
            Self::Undecodable => "undecodable",
            Self::Io => "io",
            Self::LineExceedsSearchHeap => "line exceeds search heap",
            Self::CaptureBudget => "matching content exceeds capture budget",
            Self::Escaped => "escaped",
            Self::NonUnicodePath => "non-unicode path",
            Self::TranscodeMemory => "legacy encoding too large to decode for search",
        }
    }

    #[must_use]
    pub(crate) const fn single_file_message(self) -> &'static str {
        match self {
            Self::Binary => "single grep target is binary",
            Self::ChangedWhileSearched => "single grep target changed while being searched",
            Self::Undecodable => "single grep target is undecodable",
            Self::Io => "single grep target could not be searched",
            Self::LineExceedsSearchHeap => {
                "single grep target has a line that exceeds the search heap"
            }
            Self::CaptureBudget => "single grep target matching content exceeds the capture budget",
            Self::Escaped => "single grep target escaped the search root",
            Self::NonUnicodePath => "single grep target path is not valid Unicode",
            Self::TranscodeMemory => {
                "single grep target is too large to decode from its legacy encoding"
            }
        }
    }

    #[must_use]
    pub(crate) const fn retryable(self) -> bool {
        matches!(self, Self::ChangedWhileSearched | Self::Io)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub(crate) struct SkipEntry {
    pub path: String,
    pub reason: SkipReason,
}

impl SkipEntry {
    #[must_use]
    pub(crate) fn line(&self) -> String {
        format!("{} — {}", self.path, self.reason.as_str())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub(crate) struct SkipNotes {
    entries: Vec<SkipEntry>,
    total: usize,
    /// Tracked separately from `entries` because sampling stops at 32: an undecodable file
    /// beyond that cut would otherwise lose the one hint that makes it reachable.
    undecodable: bool,
}

impl SkipNotes {
    pub(crate) fn record(&mut self, path: impl Into<String>, reason: SkipReason) {
        self.total = self.total.saturating_add(1);
        self.undecodable |= reason == SkipReason::Undecodable;
        if self.entries.len() < MAX_SAMPLED_SKIP_ENTRIES {
            self.entries.push(SkipEntry {
                path: path.into(),
                reason,
            });
        }
    }

    pub(crate) fn merge(&mut self, other: &Self) {
        for entry in &other.entries {
            if self.entries.len() >= MAX_SAMPLED_SKIP_ENTRIES {
                break;
            }
            self.entries.push(entry.clone());
        }
        self.total = self.total.saturating_add(other.total);
        self.undecodable |= other.undecodable;
    }

    #[must_use]
    pub(crate) fn has_undecodable(&self) -> bool {
        self.undecodable
    }

    #[must_use]
    pub(crate) fn total(&self) -> usize {
        self.total
    }

    #[must_use]
    pub(crate) fn sampled(&self) -> &[SkipEntry] {
        &self.entries
    }

    #[must_use]
    pub(crate) fn terminal(&self, scan_complete: bool, noun: &str) -> Option<String> {
        if self.total == 0 {
            return None;
        }
        let shown = self.entries.len();
        let head = if scan_complete {
            format!("Skipped: {} {noun}", self.total)
        } else {
            format!("Skipped while producing this page: {} {noun}", self.total)
        };
        if shown < self.total {
            Some(format!(
                "{head}, showing {shown} — narrow path/glob to inspect the rest."
            ))
        } else {
            Some(format!("{head}."))
        }
    }
}

#[must_use]
pub(crate) fn search_tail(
    notes: &SkipNotes,
    scan_complete: bool,
    noun: &str,
    extras: impl IntoIterator<Item = String>,
    next_offset: Option<usize>,
) -> Vec<String> {
    let mut tail: Vec<String> = notes.sampled().iter().map(SkipEntry::line).collect();
    if let Some(terminal) = notes.terminal(scan_complete, noun) {
        tail.push(terminal);
    }
    tail.extend(extras);
    if let Some(next) = next_offset {
        tail.push(format!(
            "{} {}={next}.",
            super::PARTIAL_MARKER,
            super::NEXT_OFFSET_FIELD
        ));
    }
    tail
}

#[cfg(test)]
mod tests {
    use super::{SkipNotes, SkipReason, search_tail};

    #[test]
    fn terminal_reports_the_true_total_when_samples_are_capped() {
        let mut notes = SkipNotes::default();
        for index in 0..40 {
            notes.record(format!("f{index}"), SkipReason::Binary);
        }
        assert_eq!(notes.total(), 40);
        assert_eq!(notes.sampled().len(), 32);
        assert_eq!(
            notes.terminal(true, "files").as_deref(),
            Some("Skipped: 40 files, showing 32 — narrow path/glob to inspect the rest.")
        );
        let tail = search_tail(&notes, true, "files", [], Some(3));
        assert!(tail[0].ends_with(" — binary"));
        assert_eq!(
            tail.last().map(String::as_str),
            Some("Partial: next_offset=3.")
        );
    }
}
