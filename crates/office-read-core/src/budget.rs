use std::{cell::RefCell, sync::Arc};

use crate::OfficeReadError;

#[derive(Clone)]
pub struct CancelSignal(Arc<dyn Fn() -> bool + Send + Sync>);

impl CancelSignal {
    pub fn new(check: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(check))
    }

    pub fn never() -> Self {
        Self::new(|| false)
    }

    pub fn is_cancelled(&self) -> bool {
        (self.0)()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct OfficeReadLimits {
    file_bytes: u64,
    zip_entries: usize,
    zip_compression_ratio: u64,
    part_bytes: u64,
    total_part_bytes: u64,
    cfb_entries: usize,
    cfb_stream_bytes: u64,
    total_cfb_bytes: u64,
    xml_events: usize,
    xml_depth: usize,
    xml_attributes: usize,
    xml_text_node_bytes: usize,
    total_xml_text_bytes: u64,
    model_items: usize,
    model_text_bytes: u64,
    markdown_bytes: usize,
}

impl OfficeReadLimits {
    pub const fn within(call_bytes: usize) -> Self {
        let call = call_bytes as u64;
        Self {
            file_bytes: call / 2,
            zip_entries: 4_096,
            zip_compression_ratio: 1_000,
            part_bytes: call / 4,
            total_part_bytes: call / 2,
            cfb_entries: 16_384,
            cfb_stream_bytes: call / 3,
            total_cfb_bytes: call / 2,
            xml_events: 2_000_000,
            xml_depth: 256,
            xml_attributes: 2_000_000,
            xml_text_node_bytes: call_bytes / 16,
            total_xml_text_bytes: call / 3,
            model_items: 1_000_000,
            model_text_bytes: call / 3,
            markdown_bytes: call_bytes / 4,
        }
    }

    pub(crate) fn check_file_bytes(self, observed: u64) -> Result<(), OfficeReadError> {
        check_limit("office_input_bytes", self.file_bytes, observed)
    }
}

struct BudgetState {
    limits: OfficeReadLimits,
    cancelled: CancelSignal,
    total_part_bytes: u64,
    total_cfb_bytes: u64,
    total_xml_text_bytes: u64,
    ppt_records: u64,
    ppt_model_items: u64,
    ppt_text_bytes: u64,
    opc_model_items: u64,
    opc_text_bytes: u64,
    model_items: u64,
    model_text_bytes: u64,
    failure: Option<LimitExceeded>,
}

#[derive(Clone, Copy)]
struct LimitExceeded {
    resource: &'static str,
    limit: u64,
    observed: u64,
}

thread_local! {
    static ACTIVE: RefCell<Option<BudgetState>> = const { RefCell::new(None) };
}

pub(crate) struct BudgetScope;

pub(crate) fn enter(limits: OfficeReadLimits, cancelled: CancelSignal) -> BudgetScope {
    ACTIVE.with(|active| {
        let previous = active.replace(Some(BudgetState {
            limits,
            cancelled,
            total_part_bytes: 0,
            total_cfb_bytes: 0,
            total_xml_text_bytes: 0,
            ppt_records: 0,
            ppt_model_items: 0,
            ppt_text_bytes: 0,
            opc_model_items: 0,
            opc_text_bytes: 0,
            model_items: 0,
            model_text_bytes: 0,
            failure: None,
        }));
        assert!(previous.is_none(), "Office budget scopes must not nest");
    });
    BudgetScope
}

impl Drop for BudgetScope {
    fn drop(&mut self) {
        ACTIVE.with(|active| {
            active.borrow_mut().take();
        });
    }
}

pub(crate) fn check_cancelled() -> Result<(), OfficeReadError> {
    ACTIVE.with(|active| {
        let active = active.borrow();
        let Some(state) = active.as_ref() else {
            return Ok(());
        };
        if let Some(failure) = state.failure {
            return Err(OfficeReadError::ResourceLimit {
                resource: failure.resource,
                limit: failure.limit,
                observed: failure.observed,
            });
        }
        if state.cancelled.is_cancelled() {
            return Err(OfficeReadError::Cancelled);
        }
        Ok(())
    })
}

pub(crate) fn is_cancelled() -> bool {
    ACTIVE.with(|active| {
        active
            .borrow()
            .as_ref()
            .is_some_and(|state| state.cancelled.is_cancelled())
    })
}

pub(crate) fn check_zip_entries(observed: usize) -> Result<(), OfficeReadError> {
    with_state(|state| {
        check_limit(
            "office_zip_entries",
            state.limits.zip_entries as u64,
            observed as u64,
        )
    })
}

pub(crate) fn charge_zip_part(observed: u64) -> Result<(), OfficeReadError> {
    with_state(|state| {
        check_limit("office_zip_part_bytes", state.limits.part_bytes, observed)?;
        state.total_part_bytes = state.total_part_bytes.saturating_add(observed);
        check_limit(
            "office_zip_total_bytes",
            state.limits.total_part_bytes,
            state.total_part_bytes,
        )
    })
}

pub(crate) fn check_zip_part(observed: u64) -> Result<(), OfficeReadError> {
    with_state(|state| check_limit("office_zip_part_bytes", state.limits.part_bytes, observed))
}

pub(crate) fn check_zip_compression_ratio(
    compressed: u64,
    uncompressed: u64,
) -> Result<(), OfficeReadError> {
    with_state(|state| {
        if uncompressed == 0 {
            return Ok(());
        }
        if compressed == 0 {
            return check_limit("office_zip_compression_ratio", 0, uncompressed);
        }
        let ratio = uncompressed.saturating_add(compressed - 1) / compressed;
        check_limit(
            "office_zip_compression_ratio",
            state.limits.zip_compression_ratio,
            ratio,
        )
    })
}

pub(crate) fn charge_zip_growth(declared: u64, observed: u64) -> Result<(), OfficeReadError> {
    with_state(|state| {
        check_limit("office_zip_part_bytes", state.limits.part_bytes, observed)?;
        let _ = declared;
        Ok(())
    })
}

pub(crate) fn check_cfb_entries(observed: usize) -> Result<(), OfficeReadError> {
    with_state(|state| {
        check_limit(
            "office_cfb_entries",
            state.limits.cfb_entries as u64,
            observed as u64,
        )
    })
}

pub(crate) fn charge_cfb_stream(observed: u64) -> Result<(), OfficeReadError> {
    with_state(|state| {
        check_limit(
            "office_cfb_stream_bytes",
            state.limits.cfb_stream_bytes,
            observed,
        )?;
        state.total_cfb_bytes = state.total_cfb_bytes.saturating_add(observed);
        check_limit(
            "office_cfb_total_bytes",
            state.limits.total_cfb_bytes,
            state.total_cfb_bytes,
        )
    })
}

pub(crate) fn charge_cfb_growth(observed: u64) -> Result<(), OfficeReadError> {
    with_state(|state| {
        check_limit(
            "office_cfb_stream_bytes",
            state.limits.cfb_stream_bytes,
            observed,
        )
    })
}

pub(crate) fn charge_cfb_internal(observed: u64) -> Result<(), OfficeReadError> {
    with_state(|state| {
        check_limit(
            "office_cfb_stream_bytes",
            state.limits.cfb_stream_bytes,
            observed,
        )?;
        state.total_cfb_bytes = state.total_cfb_bytes.saturating_add(observed);
        check_limit(
            "office_cfb_total_bytes",
            state.limits.total_cfb_bytes,
            state.total_cfb_bytes,
        )
    })
}

pub(crate) fn check_markdown_bytes(observed: usize) -> Result<(), OfficeReadError> {
    with_state(|state| {
        check_limit(
            "office_markdown_bytes",
            state.limits.markdown_bytes as u64,
            observed as u64,
        )
    })
}

pub(crate) fn markdown_within_limit(observed: usize) -> bool {
    ACTIVE.with(|active| {
        let mut active = active.borrow_mut();
        let Some(state) = active.as_mut() else {
            return true;
        };
        if observed <= state.limits.markdown_bytes {
            return true;
        }
        state.failure.get_or_insert(LimitExceeded {
            resource: "office_markdown_bytes",
            limit: state.limits.markdown_bytes as u64,
            observed: observed as u64,
        });
        false
    })
}

pub(crate) fn check_model_items(
    resource: &'static str,
    observed: usize,
) -> Result<(), OfficeReadError> {
    with_state(|state| check_limit(resource, state.limits.model_items as u64, observed as u64))
}

pub(crate) fn check_model_text_bytes(
    resource: &'static str,
    observed: usize,
) -> Result<(), OfficeReadError> {
    with_state(|state| check_limit(resource, state.limits.model_text_bytes, observed as u64))
}

pub(crate) fn charge_model_items(
    resource: &'static str,
    observed: usize,
) -> Result<(), OfficeReadError> {
    with_state(|state| {
        state.model_items = state.model_items.saturating_add(observed as u64);
        check_limit(resource, state.limits.model_items as u64, state.model_items)
    })
}

pub(crate) fn charge_model_text(
    resource: &'static str,
    observed: usize,
) -> Result<(), OfficeReadError> {
    with_state(|state| {
        state.model_text_bytes = state.model_text_bytes.saturating_add(observed as u64);
        check_limit(
            resource,
            state.limits.model_text_bytes,
            state.model_text_bytes,
        )
    })
}

pub(crate) fn charge_ppt_record() -> Result<(), OfficeReadError> {
    with_state(|state| {
        state.ppt_records = state.ppt_records.saturating_add(1);
        check_limit(
            "office_ppt_records",
            state.limits.model_items as u64,
            state.ppt_records,
        )
    })
}

pub(crate) fn charge_ppt_item(resource: &'static str) -> Result<(), OfficeReadError> {
    with_state(|state| {
        state.ppt_model_items = state.ppt_model_items.saturating_add(1);
        check_limit(
            resource,
            state.limits.model_items as u64,
            state.ppt_model_items,
        )
    })
}

pub(crate) fn check_ppt_text_allocation(observed: usize) -> Result<(), OfficeReadError> {
    with_state(|state| {
        let projected = state.ppt_text_bytes.saturating_add(observed as u64);
        check_limit(
            "office_ppt_text_bytes",
            state.limits.model_text_bytes,
            projected,
        )
    })
}

pub(crate) fn charge_ppt_text(observed: usize) -> Result<(), OfficeReadError> {
    with_state(|state| {
        state.ppt_text_bytes = state.ppt_text_bytes.saturating_add(observed as u64);
        check_limit(
            "office_ppt_text_bytes",
            state.limits.model_text_bytes,
            state.ppt_text_bytes,
        )
    })
}

pub(crate) fn charge_opc_item(resource: &'static str) -> Result<(), OfficeReadError> {
    with_state(|state| {
        state.opc_model_items = state.opc_model_items.saturating_add(1);
        check_limit(
            resource,
            state.limits.model_items as u64,
            state.opc_model_items,
        )
    })
}

pub(crate) fn charge_opc_text(
    resource: &'static str,
    observed: usize,
) -> Result<(), OfficeReadError> {
    with_state(|state| {
        state.opc_text_bytes = state.opc_text_bytes.saturating_add(observed as u64);
        check_limit(
            resource,
            state.limits.model_text_bytes,
            state.opc_text_bytes,
        )
    })
}

pub(crate) fn validate_xml(xml: &[u8]) -> Result<(), OfficeReadError> {
    use quick_xml::events::Event;

    with_state(|state| {
        let mut reader = quick_xml::Reader::from_reader(xml);
        let mut events = 0_usize;
        let mut attributes = 0_usize;
        let mut depth = 0_usize;
        loop {
            if state.cancelled.is_cancelled() {
                return Err(OfficeReadError::Cancelled);
            }
            let event = reader
                .read_event()
                .map_err(|_| OfficeReadError::Invalid { stage: "xml" })?;
            events = events.saturating_add(1);
            check_limit(
                "office_xml_events",
                state.limits.xml_events as u64,
                events as u64,
            )?;
            match event {
                Event::Start(element) => {
                    depth = depth.saturating_add(1);
                    check_limit(
                        "office_xml_depth",
                        state.limits.xml_depth as u64,
                        depth as u64,
                    )?;
                    for attribute in element.attributes() {
                        attribute.map_err(|_| OfficeReadError::Invalid { stage: "xml" })?;
                        attributes = attributes.saturating_add(1);
                    }
                }
                Event::Empty(element) => {
                    for attribute in element.attributes() {
                        attribute.map_err(|_| OfficeReadError::Invalid { stage: "xml" })?;
                        attributes = attributes.saturating_add(1);
                    }
                }
                Event::End(_) => {
                    depth = depth
                        .checked_sub(1)
                        .ok_or(OfficeReadError::Invalid { stage: "xml" })?;
                }
                Event::Text(text) => {
                    let bytes = text.len();
                    check_limit(
                        "office_xml_text_node_bytes",
                        state.limits.xml_text_node_bytes as u64,
                        bytes as u64,
                    )?;
                    state.total_xml_text_bytes =
                        state.total_xml_text_bytes.saturating_add(bytes as u64);
                    check_limit(
                        "office_xml_text_bytes",
                        state.limits.total_xml_text_bytes,
                        state.total_xml_text_bytes,
                    )?;
                }
                Event::CData(text) => {
                    let bytes = text.len();
                    check_limit(
                        "office_xml_text_node_bytes",
                        state.limits.xml_text_node_bytes as u64,
                        bytes as u64,
                    )?;
                    state.total_xml_text_bytes =
                        state.total_xml_text_bytes.saturating_add(bytes as u64);
                    check_limit(
                        "office_xml_text_bytes",
                        state.limits.total_xml_text_bytes,
                        state.total_xml_text_bytes,
                    )?;
                }
                Event::DocType(_) => {
                    return Err(OfficeReadError::Unsupported {
                        stage: "xml_doctype",
                    });
                }
                Event::Eof => {
                    if depth != 0 {
                        return Err(OfficeReadError::Invalid { stage: "xml" });
                    }
                    break;
                }
                _ => {}
            }
            check_limit(
                "office_xml_attributes",
                state.limits.xml_attributes as u64,
                attributes as u64,
            )?;
        }
        Ok(())
    })
}

fn with_state<T>(
    run: impl FnOnce(&mut BudgetState) -> Result<T, OfficeReadError>,
) -> Result<T, OfficeReadError> {
    ACTIVE.with(|active| {
        let mut active = active.borrow_mut();
        if let Some(state) = active.as_mut() {
            if state.cancelled.is_cancelled() {
                Err(OfficeReadError::Cancelled)
            } else {
                run(state)
            }
        } else {
            let mut state = BudgetState {
                limits: OfficeReadLimits {
                    file_bytes: u64::MAX,
                    zip_entries: usize::MAX,
                    zip_compression_ratio: u64::MAX,
                    part_bytes: u64::MAX,
                    total_part_bytes: u64::MAX,
                    cfb_entries: usize::MAX,
                    cfb_stream_bytes: u64::MAX,
                    total_cfb_bytes: u64::MAX,
                    xml_events: usize::MAX,
                    xml_depth: usize::MAX,
                    xml_attributes: usize::MAX,
                    xml_text_node_bytes: usize::MAX,
                    total_xml_text_bytes: u64::MAX,
                    model_items: usize::MAX,
                    model_text_bytes: u64::MAX,
                    markdown_bytes: usize::MAX,
                },
                cancelled: CancelSignal::never(),
                total_part_bytes: 0,
                total_cfb_bytes: 0,
                total_xml_text_bytes: 0,
                ppt_records: 0,
                ppt_model_items: 0,
                ppt_text_bytes: 0,
                opc_model_items: 0,
                opc_text_bytes: 0,
                model_items: 0,
                model_text_bytes: 0,
                failure: None,
            };
            run(&mut state)
        }
    })
}

fn check_limit(resource: &'static str, limit: u64, observed: u64) -> Result<(), OfficeReadError> {
    if observed > limit {
        Err(OfficeReadError::ResourceLimit {
            resource,
            limit,
            observed,
        })
    } else {
        Ok(())
    }
}
