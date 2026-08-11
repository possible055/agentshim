use super::*;

/// An optional bound on structure-tree parsing.
///
/// A parsing library returns the *complete* tree by default, so the unbounded
/// guard (`ParseBudget::unbounded`) applies no time limit and no element cap.
/// Callers that deliberately want to bound the work — e.g. the extraction
/// reading-order strategy, which can fall back to content-stream order — build a
/// bounded guard from a wall-clock `Duration`, which also caps the element count
/// at [`MAX_STRUCT_ELEMENTS`]. On `wasm32-unknown-unknown` `std::time::Instant`
/// panics, so the time limit is a no-op there and only the element cap applies.
#[derive(Clone, Copy, Default)]
pub(super) struct ParseBudget {
    #[cfg(not(target_arch = "wasm32"))]
    deadline: Option<std::time::Instant>,
    max_elements: Option<usize>,
}

impl ParseBudget {
    /// No limit — parse the entire tree (the default for a general-purpose API).
    pub(super) fn unbounded() -> Self {
        Self::default()
    }

    /// Bound parsing by `budget` wall-clock time, and cap the element count at
    /// [`MAX_STRUCT_ELEMENTS`] as a companion guard against pathological trees.
    pub(super) fn from_option(budget: Option<Duration>) -> Self {
        match budget {
            None => Self::unbounded(),
            Some(_budget) => Self {
                #[cfg(not(target_arch = "wasm32"))]
                deadline: Some(std::time::Instant::now() + _budget),
                max_elements: Some(MAX_STRUCT_ELEMENTS),
            },
        }
    }

    /// Whether either bound has been exceeded at the current element count.
    /// Always `false` for an unbounded guard.
    #[inline]
    pub(super) fn exceeded(&self, element_count: usize) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(deadline) = self.deadline {
            if std::time::Instant::now() > deadline {
                return true;
            }
        }
        self.max_elements.is_some_and(|max| element_count > max)
    }
}

/// A timer for measuring elapsed time, WASM-safe.
#[derive(Clone, Copy)]
pub(super) struct Timer {
    #[cfg(not(target_arch = "wasm32"))]
    start: std::time::Instant,
}

impl Timer {
    pub(super) fn now() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            Self {
                start: std::time::Instant::now(),
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            Self {}
        }
    }

    pub(super) fn elapsed_debug(&self) -> String {
        #[cfg(not(target_arch = "wasm32"))]
        {
            format!("{:?}", self.start.elapsed())
        }
        #[cfg(target_arch = "wasm32")]
        {
            "(time unavailable on wasm)".to_string()
        }
    }
}

/// Maximum number of structure elements to parse.
/// Trees larger than this cause expensive traversal (seconds for 50K+ elements).
/// 10K elements is sufficient for any normal document; larger trees indicate
/// deeply structured books where content-stream order works equally well.
pub(super) const MAX_STRUCT_ELEMENTS: usize = 10_000;
