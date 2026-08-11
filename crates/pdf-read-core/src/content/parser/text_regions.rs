use super::*;

/// SIMD-accelerated pre-scan to identify text-bearing regions in a content stream.
///
/// Finds BT/Do operators via memchr, then for each one determines the region
/// boundaries and required graphics state. When the backward scan can capture
/// all enclosing `q`/`cm` context within 4KB, returns [`PrescanResult::Regions`].
/// Otherwise, runs a lightweight forward CTM scan to capture the full graphics
/// state and returns [`PrescanResult::RegionsWithCtm`].
///
/// # Arguments
///
/// * `data` - Raw content stream bytes
///
/// # Returns
///
/// Returns `None` if the forward scan fails, signaling the caller to fall back
/// to full stream parsing.
pub(super) fn prescan_text_regions(data: &[u8]) -> Option<PrescanResult> {
    fn is_boundary(b: u8) -> bool {
        b.is_ascii_whitespace()
            || matches!(
                b,
                b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
            )
    }

    let len = data.len();
    // Collect positions of BT and Do operators (text-bearing operators)
    let mut text_positions: Vec<usize> = Vec::new();
    let mut offset = 0;

    // Use memchr to find 'B' and 'D' candidates (SIMD-accelerated)
    loop {
        match memchr::memchr2(b'B', b'D', &data[offset..]) {
            None => break,
            Some(rel_pos) => {
                let pos = offset + rel_pos;
                offset = pos + 1;

                // Check for "BT" at boundary
                #[allow(clippy::if_same_then_else)]
                if data[pos] == b'B' && pos + 1 < len && data[pos + 1] == b'T' {
                    let before_ok = pos == 0 || is_boundary(data[pos - 1]);
                    let after_ok = pos + 2 >= len || is_boundary(data[pos + 2]);
                    if before_ok && after_ok {
                        text_positions.push(pos);
                    }
                }
                // Check for "Do" at boundary
                else if data[pos] == b'D' && pos + 1 < len && data[pos + 1] == b'o' {
                    let before_ok = pos == 0 || is_boundary(data[pos - 1]);
                    let after_ok = pos + 2 >= len || is_boundary(data[pos + 2]);
                    if before_ok && after_ok {
                        text_positions.push(pos);
                    }
                }
            }
        }
    }

    if text_positions.is_empty() {
        return Some(PrescanResult::Empty);
    }

    // Drop Do positions when Do dominates BT (chart/figure graphics that
    // would merge prescan regions across the entire stream).
    // Everything below materialises one region per text position, and on the CTM path a
    // graphics state with an owned font name beside it, so the prescan's own footprint is
    // proportional to attacker-controlled input. Declining leaves the caller on the full
    // parser, which is the bounded path: it refuses on the operator budget instead.
    if text_positions.len() > crate::budget::prescan_region_ceiling() {
        return None;
    }

    let bt_count = text_positions
        .iter()
        .filter(|&&p| p + 1 < len && data[p] == b'B')
        .count();
    let do_count = text_positions.len() - bt_count;
    if do_count > 50 && do_count > bt_count * 10 {
        text_positions.retain(|&p| p + 1 < len && data[p] == b'B');
        if text_positions.is_empty() {
            return Some(PrescanResult::Empty);
        }
    }

    // For each text position, scan backwards to find the nearest unmatched 'q'
    // to capture CTM state (cm operators between q and BT/Do).
    let mut regions: Vec<(usize, usize)> = Vec::new();
    let mut needs_forward_ctm = false;

    for &tp in &text_positions {
        // Find region start: scan backwards for unmatched q
        let (region_start, hit_limit) = find_region_start(data, tp);

        if hit_limit {
            needs_forward_ctm = true;
        }

        // Find region end: for BT, find matching ET; for Do, end after "Do"
        let region_end = if data[tp] == b'B' {
            // Find matching ET
            find_matching_et(data, tp + 2).unwrap_or(len)
        } else {
            // Do operator: include operands before and the operator itself
            tp + 2
        };

        let end = region_end.min(len);
        regions.push((region_start, end));
    }

    // Merge overlapping/adjacent regions
    if regions.is_empty() {
        return Some(PrescanResult::Empty);
    }

    if needs_forward_ctm {
        // At least one BT was too far from the start of the stream for the
        // backward scan to capture all enclosing CTM context. Run a lightweight
        // forward scan to get the full graphics state at each BT/Do position.
        //
        // Regions start at the BT/Do position itself (not the backward-scanned
        // q) to avoid q/Q nesting issues with the SaveState/RestoreState
        // wrapping. The forward scan also tracks font state so BT blocks that
        // inherit fonts from prior state get the correct Tf injected.
        let states = forward_scan_ctm(data, &text_positions)?;

        // Build BT-based regions with their graphics state.
        // Extend each region to include preceding BDC/BMC and following EMC
        // so that marked-content operators are preserved in tagged PDFs.
        let mut ctm_regions: Vec<(usize, usize)> = Vec::new();
        for &tp in &text_positions {
            let region_start = find_preceding_marked_content(data, tp);
            let region_end = if data[tp] == b'B' {
                let et_end = find_matching_et(data, tp + 2).unwrap_or(len);
                find_following_emc(data, et_end)
            } else {
                tp + 2
            };
            ctm_regions.push((region_start, region_end.min(len)));
        }

        // Merge overlapping regions and track which state goes with each.
        let mut indexed: Vec<((usize, usize), PrescanState)> =
            ctm_regions.into_iter().zip(states).collect();
        indexed.sort_by_key(|&(r, _)| r.0);

        let mut merged: Vec<(usize, usize)> = Vec::new();
        let mut merged_states: Vec<PrescanState> = Vec::new();

        for (r, state) in indexed {
            if let Some(last) = merged.last_mut() {
                if r.0 <= last.1 {
                    last.1 = last.1.max(r.1);
                    continue; // Merged — keep the state from the first region
                }
            }
            merged.push(r);
            merged_states.push(state);
        }

        return Some(PrescanResult::RegionsWithCtm {
            regions: merged,
            region_states: merged_states,
        });
    }

    regions.sort_unstable_by_key(|r| r.0);
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for r in regions {
        if let Some(last) = merged.last_mut() {
            if r.0 <= last.1 {
                last.1 = last.1.max(r.1);
                continue;
            }
        }
        merged.push(r);
    }

    Some(PrescanResult::Regions(merged))
}

/// Scan backwards from `pos` to find the start of the graphics state context.
///
/// Looks for the nearest unmatched `q` operator within a 4KB window,
/// handling nested `q`/`Q` pairs.
///
/// # Arguments
///
/// * `data` - Full content stream bytes
/// * `pos` - Byte offset to scan backwards from (typically a BT/Do position)
///
/// # Returns
///
/// `(offset, hit_limit)` where `offset` is the position of the nearest
/// unmatched `q` (or `pos` if none found), and `hit_limit` is true if the
/// 4KB scan window didn't reach the beginning of the data. When `hit_limit`
/// is true, there may be additional enclosing `q`/`cm` operators beyond
/// the window that affect the CTM.
pub(super) fn find_region_start(data: &[u8], pos: usize) -> (usize, bool) {
    // Simple backward scan: find the nearest line that starts with 'q' or
    // the beginning of data. We limit backward scan to 4KB for performance.
    let scan_start = pos.saturating_sub(4096);
    let region = &data[scan_start..pos];

    // Find the last unmatched q by tracking Q/q balance backwards
    let mut q_depth: i32 = 0;
    let mut best_q_pos = pos; // Default: start from text position itself
    let mut i = region.len();

    while i > 0 {
        i -= 1;
        let b = region[i];

        // Look for 'q' or 'Q' at operator boundaries
        if b == b'q' || b == b'Q' {
            let abs_pos = scan_start + i;
            // Verify it's a standalone operator (boundary check)
            let before_ok = i == 0 || {
                let prev = region[i - 1];
                prev.is_ascii_whitespace() || matches!(prev, b')' | b'>' | b']')
            };
            let after_ok = i + 1 >= region.len() || {
                let next = region[i + 1];
                next.is_ascii_whitespace()
                    || matches!(next, b'(' | b'<' | b'[' | b'/' | b'%')
                    || next.is_ascii_digit()
                    || next == b'-'
                    || next == b'.'
            };

            if before_ok && after_ok {
                if b == b'Q' {
                    q_depth += 1;
                } else {
                    // 'q'
                    if q_depth > 0 {
                        q_depth -= 1;
                    } else {
                        // Unmatched q — this is our region start
                        best_q_pos = abs_pos;
                        break;
                    }
                }
            }
        }
    }

    // We can only guarantee complete CTM context if we scanned all the way
    // to the beginning of the data. Even if we found an unmatched 'q' within
    // 4KB, there may be additional enclosing q/cm operators before the scan
    // window that establish scaling transforms we're missing.
    let hit_limit = scan_start > 0;
    (best_q_pos, hit_limit)
}

/// Scan backward from `pos` to find any immediately preceding BDC/BMC operator.
/// Returns the position of the BDC/BMC if found within 256 bytes, otherwise `pos`.
pub(super) fn find_preceding_marked_content(data: &[u8], pos: usize) -> usize {
    let scan_start = pos.saturating_sub(256);
    let mut i = pos;
    while i > scan_start {
        i -= 1;
        // Look for 'C' which ends BDC or BMC
        if data[i] == b'C'
            && i >= 2
            && data[i - 2] == b'B'
            && (data[i - 1] == b'D' || data[i - 1] == b'M')
        {
            let op_start = i - 2;
            // Verify operator boundary
            let before_ok = op_start == 0 || !data[op_start - 1].is_ascii_alphanumeric();
            let after_ok = i + 1 >= data.len() || !data[i + 1].is_ascii_alphanumeric();
            if before_ok && after_ok {
                // For BDC, scan further back to include the tag and properties dict
                // e.g., "/Span << /MCID 0 >> BDC"
                // Find the start of the line/command
                let mut line_start = op_start;
                while line_start > scan_start
                    && data[line_start - 1] != b'\n'
                    && data[line_start - 1] != b'\r'
                {
                    line_start -= 1;
                }
                return line_start;
            }
        }
    }
    pos
}

/// Scan forward from `pos` to find any immediately following EMC operator.
/// Returns the position after the EMC if found within 256 bytes, otherwise `pos`.
pub(super) fn find_following_emc(data: &[u8], pos: usize) -> usize {
    let scan_end = (pos + 256).min(data.len());
    let mut i = pos;
    while i + 2 < scan_end {
        if data[i] == b'E' && data[i + 1] == b'M' && data[i + 2] == b'C' {
            let before_ok = i == 0 || data[i - 1].is_ascii_whitespace();
            let after_ok = i + 3 >= data.len() || data[i + 3].is_ascii_whitespace();
            if before_ok && after_ok {
                return i + 3;
            }
        }
        i += 1;
    }
    pos
}

/// Find the position after matching "ET" for a BT starting at `start`.
pub(super) fn find_matching_et(data: &[u8], start: usize) -> Option<usize> {
    let mut offset = start;
    let len = data.len();
    // Use memchr to find 'E' candidates
    loop {
        let rel = memchr::memchr(b'E', &data[offset..])?;
        let pos = offset + rel;
        offset = pos + 1;
        if pos + 1 < len && data[pos + 1] == b'T' {
            let before_ok = pos == 0
                || data[pos - 1].is_ascii_whitespace()
                || matches!(data[pos - 1], b')' | b'>' | b']' | b'}' | b'/' | b'%');
            let after_ok = pos + 2 >= len || {
                let next = data[pos + 2];
                next.is_ascii_whitespace() || matches!(next, b'(' | b'<' | b'[' | b'/' | b'%')
            };
            if before_ok && after_ok {
                return Some(pos + 2);
            }
        }
    }
}
