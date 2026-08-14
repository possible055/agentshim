use super::support::*;

pub(super) fn request_slope(samples: &[(usize, u64)]) -> f64 {
    let count = f64::from(u32::try_from(samples.len()).expect("bounded sample count"));
    let mean_x = samples
        .iter()
        .map(|(requests, _)| metric_as_f64(u64::try_from(*requests).expect("request count")))
        .sum::<f64>()
        / count;
    let mean_y = samples
        .iter()
        .map(|(_, memory)| metric_as_f64(*memory))
        .sum::<f64>()
        / count;
    let (numerator, denominator) = samples.iter().fold(
        (0.0, 0.0),
        |(numerator, denominator), (requests, memory)| {
            let x = metric_as_f64(u64::try_from(*requests).expect("request count")) - mean_x;
            (
                numerator + x * (metric_as_f64(*memory) - mean_y),
                denominator + x * x,
            )
        },
    );
    if denominator == 0.0 {
        0.0
    } else {
        numerator / denominator
    }
}
