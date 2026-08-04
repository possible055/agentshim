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

    use super::{SortCancelled, sort_by};

    #[test]
    fn matches_standard_sort_for_large_duplicate_corpus() {
        let mut actual = (0..100_000)
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
        let mut values = (0..100_000).rev().collect::<Vec<_>>();

        let result = sort_by(&mut values, &cancellation, |left, right| {
            comparisons += 1;
            if comparisons == 1_024 {
                trigger.cancel();
            }
            left.cmp(right)
        });

        assert_eq!(result, Err(SortCancelled));
    }
}
