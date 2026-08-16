use std::cmp::Ordering;

use tokio_util::sync::CancellationToken;

const CANCELLATION_CHECK_INTERVAL: usize = 1_024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SortCancelled;

/// Sort a slice while periodically observing cooperative cancellation.
///
/// # Errors
///
/// Returns [`SortCancelled`] if cancellation is observed before or during sorting.
pub fn sort_by<T>(
    values: &mut [T],
    cancellation: &CancellationToken,
    mut compare: impl FnMut(&T, &T) -> Ordering,
) -> Result<(), SortCancelled> {
    if cancellation.is_cancelled() {
        return Err(SortCancelled);
    }

    let mut comparisons = 0_usize;
    for root in (0..values.len() / 2).rev() {
        sift_down(
            values,
            root,
            values.len(),
            cancellation,
            &mut comparisons,
            &mut compare,
        )?;
    }
    for end in (1..values.len()).rev() {
        values.swap(0, end);
        sift_down(values, 0, end, cancellation, &mut comparisons, &mut compare)?;
    }

    if cancellation.is_cancelled() {
        Err(SortCancelled)
    } else {
        Ok(())
    }
}

pub fn sort_unstable_by<T>(
    values: &mut [T],
    cancellation: &CancellationToken,
    mut compare: impl FnMut(&T, &T) -> Ordering,
) -> Result<(), SortCancelled> {
    if cancellation.is_cancelled() {
        return Err(SortCancelled);
    }
    let comparisons = std::cell::Cell::new(0_usize);
    let cancelled = std::cell::Cell::new(false);
    values.sort_unstable_by(|left, right| {
        let count = comparisons.get().saturating_add(1);
        comparisons.set(count);
        if count.is_multiple_of(CANCELLATION_CHECK_INTERVAL) && cancellation.is_cancelled() {
            cancelled.set(true);
        }
        compare(left, right)
    });
    if cancelled.get() || cancellation.is_cancelled() {
        Err(SortCancelled)
    } else {
        Ok(())
    }
}

fn sift_down<T>(
    values: &mut [T],
    mut root: usize,
    end: usize,
    cancellation: &CancellationToken,
    comparisons: &mut usize,
    compare: &mut impl FnMut(&T, &T) -> Ordering,
) -> Result<(), SortCancelled> {
    loop {
        let left = root.saturating_mul(2).saturating_add(1);
        if left >= end {
            return Ok(());
        }

        let mut largest = left;
        let right = left.saturating_add(1);
        if right < end
            && compare_checked(
                &values[left],
                &values[right],
                cancellation,
                comparisons,
                compare,
            )? == Ordering::Less
        {
            largest = right;
        }
        if compare_checked(
            &values[root],
            &values[largest],
            cancellation,
            comparisons,
            compare,
        )? != Ordering::Less
        {
            return Ok(());
        }
        values.swap(root, largest);
        root = largest;
    }
}

fn compare_checked<T>(
    left: &T,
    right: &T,
    cancellation: &CancellationToken,
    comparisons: &mut usize,
    compare: &mut impl FnMut(&T, &T) -> Ordering,
) -> Result<Ordering, SortCancelled> {
    *comparisons = comparisons.saturating_add(1);
    if *comparisons % CANCELLATION_CHECK_INTERVAL == 0 && cancellation.is_cancelled() {
        return Err(SortCancelled);
    }
    Ok(compare(left, right))
}

#[cfg(test)]
mod tests {
    use tokio_util::sync::CancellationToken;

    #[cfg(feature = "bench-internals")]
    use super::sort_unstable_by;
    use super::{SortCancelled, sort_by};

    #[test]
    fn matches_standard_sort_with_duplicates() {
        let mut actual = (0..512)
            .rev()
            .map(|value| (value * 37) % 9_973)
            .collect::<Vec<_>>();
        let mut expected = actual.clone();
        expected.sort_unstable();

        sort_by(&mut actual, &CancellationToken::new(), Ord::cmp).expect("sort");

        assert_eq!(actual, expected);
    }

    #[test]
    fn rejects_pre_cancelled_sort() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let mut values = vec![3, 1, 2];

        assert_eq!(
            sort_by(&mut values, &cancellation, Ord::cmp),
            Err(SortCancelled)
        );
    }

    #[test]
    fn observes_cancellation_during_sort() {
        let cancellation = CancellationToken::new();
        let trigger = cancellation.clone();
        let mut comparisons = 0_usize;
        let mut values = (0..2_048).rev().collect::<Vec<_>>();

        let result = sort_by(&mut values, &cancellation, |left, right| {
            comparisons += 1;
            if comparisons == 1_024 {
                trigger.cancel();
            }
            left.cmp(right)
        });

        assert_eq!(result, Err(SortCancelled));
    }

    #[cfg(feature = "bench-internals")]
    #[test]
    fn unstable_sort_preserves_a_million_item_permutation() {
        let mut actual = (0..1_000_000_u64)
            .map(|value| value.wrapping_mul(6_364_136_223_846_793_005))
            .collect::<Vec<_>>();

        sort_unstable_by(&mut actual, &CancellationToken::new(), Ord::cmp).expect("sort");

        assert!(actual.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[cfg(feature = "bench-internals")]
    #[test]
    fn unstable_sort_million_item_cancellation_stays_bounded_in_release() {
        let source = (0..1_000_000_u64)
            .map(|value| value.wrapping_mul(6_364_136_223_846_793_005))
            .collect::<Vec<_>>();
        let mut samples = Vec::with_capacity(21);
        for _ in 0..21 {
            let cancellation = CancellationToken::new();
            let trigger = cancellation.clone();
            let mut comparisons = 0_usize;
            let mut values = source.clone();
            let started = std::time::Instant::now();
            let result = sort_unstable_by(&mut values, &cancellation, |left, right| {
                comparisons = comparisons.saturating_add(1);
                if comparisons == 1_024 {
                    trigger.cancel();
                }
                left.cmp(right)
            });
            samples.push(started.elapsed());
            assert_eq!(result, Err(SortCancelled));
        }
        samples.sort_unstable();
        let p95 = samples[19];
        if !cfg!(debug_assertions) {
            assert!(p95 <= std::time::Duration::from_millis(250));
        }
    }

    #[cfg(feature = "bench-internals")]
    #[test]
    fn unstable_sort_wins_paired_matrix_from_ten_thousand_items() {
        for len in [10_000_usize, 100_000, 1_000_000] {
            let source = (0..u64::try_from(len).expect("length fits u64"))
                .map(|value| value.wrapping_mul(6_364_136_223_846_793_005))
                .collect::<Vec<_>>();
            let mut heap_samples = Vec::with_capacity(21);
            let mut unstable_samples = Vec::with_capacity(21);
            for sample in 0..21 {
                let mut heap_values = source.clone();
                let mut unstable_values = source.clone();
                if sample % 2 == 0 {
                    heap_samples.push(measure_sort(&mut heap_values, sort_by));
                    unstable_samples.push(measure_sort(&mut unstable_values, sort_unstable_by));
                } else {
                    unstable_samples.push(measure_sort(&mut unstable_values, sort_unstable_by));
                    heap_samples.push(measure_sort(&mut heap_values, sort_by));
                }
                assert_eq!(heap_values, unstable_values);
            }
            heap_samples.sort_unstable();
            unstable_samples.sort_unstable();
            assert!(
                unstable_samples[10] < heap_samples[10],
                "unstable sort did not win at {len} items"
            );
        }
    }

    #[cfg(feature = "bench-internals")]
    fn measure_sort(
        values: &mut [u64],
        sort: impl FnOnce(
            &mut [u64],
            &CancellationToken,
            fn(&u64, &u64) -> std::cmp::Ordering,
        ) -> Result<(), SortCancelled>,
    ) -> std::time::Duration {
        let started = std::time::Instant::now();
        sort(values, &CancellationToken::new(), Ord::cmp).expect("sort");
        started.elapsed()
    }
}
