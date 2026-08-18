use agentshim_core::output::{CallBudget, TokenGate};

#[derive(Clone)]
pub(crate) struct NativeOutputLimits {
    page: usize,
    wire: usize,
}

impl NativeOutputLimits {
    pub(crate) fn new(page_override: Option<u32>) -> Self {
        let page = page_override.map_or(DEFAULT_PAGE_BUDGET_BYTES, |bytes| bytes as usize);
        Self {
            page: page.max(MIN_PAGE_BYTES),
            wire: NATIVE_WIRE_BYTE_LIMIT,
        }
    }

    pub(crate) fn capture_publish_bytes(&self) -> u64 {
        self.page as u64
    }
}

const MIN_PAGE_BYTES: usize = 4_096;
const NATIVE_WIRE_BYTE_LIMIT: usize = 1024 * 1024;
const DEFAULT_PAGE_BUDGET_BYTES: usize = 50_000;

impl CallBudget for NativeOutputLimits {
    fn page_bytes(&self) -> usize {
        self.page
    }

    fn wire_bytes(&self) -> usize {
        self.wire
    }

    fn token_gate(&self) -> Option<&dyn TokenGate> {
        None
    }
}
