pub(super) fn percentile(sorted_values: &[f32], percentile: f32) -> f32 {
    if sorted_values.is_empty() {
        return 0.0;
    }

    if sorted_values.len() == 1 {
        return sorted_values[0];
    }

    let index = percentile * (sorted_values.len() - 1) as f32;
    let lower_index = index.floor() as usize;
    let upper_index = (lower_index + 1).min(sorted_values.len() - 1);

    if lower_index == upper_index {
        sorted_values[lower_index]
    } else {
        let fraction = index - lower_index as f32;
        sorted_values[lower_index] * (1.0 - fraction) + sorted_values[upper_index] * fraction
    }
}
