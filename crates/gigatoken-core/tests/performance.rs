use std::time::Instant;

use codexshim_gigatoken::{CountUpTo, CounterLimits, O200kPrototype};

fn replacement_corpus_seed() -> Vec<u8> {
    let source = include_bytes!("fixtures/replacement_large_corpus_seed.txt");
    let mut bytes = Vec::with_capacity(source.len() * 2);
    for &byte in source {
        if byte == b'\n' && bytes.last() != Some(&b'\r') {
            bytes.push(b'\r');
        }
        bytes.push(byte);
    }
    bytes
}

fn corpus() -> String {
    let mut bytes = replacement_corpus_seed();
    while bytes.len() < 246_384 {
        bytes.extend_from_slice(include_bytes!("../../../README.md"));
    }
    bytes.truncate(246_384);
    String::from_utf8(bytes).expect("fixture remains UTF-8")
}

fn overlap_corpus(overlap_percent: usize) -> String {
    const VOCABULARY: usize = 128;
    let shared = VOCABULARY * overlap_percent / 100;
    let mut text = String::with_capacity(246_384);
    let mut index = 0_usize;
    while text.len() < 246_384 {
        let vocabulary_index = index % VOCABULARY;
        let prefix = if vocabulary_index < shared {
            "alpha"
        } else {
            "omega"
        };
        let first = char::from(b'a' + u8::try_from(vocabulary_index / 26).expect("index fits"));
        let second = char::from(b'a' + u8::try_from(vocabulary_index % 26).expect("index fits"));
        text.push(' ');
        text.push_str(prefix);
        text.push(first);
        text.push(second);
        index += 1;
    }
    text.truncate(246_384);
    text
}

fn sized_fixture(pattern: &str, bytes: usize) -> String {
    let mut text = pattern.repeat(bytes.div_ceil(pattern.len()));
    let mut end = bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

#[test]
#[ignore = "release performance gate"]
fn release_load_and_partial_overlap_targets() {
    assert!(!cfg!(debug_assertions), "run this gate with --release");
    let mut load_samples = Vec::new();
    for _ in 0..7 {
        let started = Instant::now();
        let prototype = O200kPrototype::load_embedded().expect("load ranks");
        load_samples.push(started.elapsed());
        drop(prototype);
    }
    load_samples.sort_unstable();
    let load_p95 = load_samples[load_samples.len() - 1];

    let prototype = O200kPrototype::load_embedded().expect("load ranks");
    let replacement = corpus();
    let mut first_counter = prototype
        .fork_counter(CounterLimits::default())
        .expect("counter");
    let first_started = Instant::now();
    assert_eq!(
        first_counter.count_ordinary_up_to(&replacement, usize::MAX, || false),
        CountUpTo::Exact(57_475)
    );
    let first_count = first_started.elapsed();
    let base = overlap_corpus(100);
    let mut samples = Vec::new();
    for overlap_percent in [0_usize, 25, 50, 75, 100] {
        let candidate = overlap_corpus(overlap_percent);
        let mut timings = Vec::new();
        let mut resident_bytes = 0_usize;
        for _ in 0..7 {
            let mut counter = prototype
                .fork_counter(CounterLimits::default())
                .expect("counter");
            assert!(matches!(
                counter.count_ordinary_up_to(&base, usize::MAX, || false),
                CountUpTo::Exact(_)
            ));
            let started = Instant::now();
            assert!(matches!(
                counter.count_ordinary_up_to(&candidate, usize::MAX, || false),
                CountUpTo::Exact(_)
            ));
            timings.push(started.elapsed());
            resident_bytes = resident_bytes.max(counter.metrics().resident_bytes);
        }
        timings.sort_unstable();
        samples.push((overlap_percent, timings[timings.len() - 1], resident_bytes));
    }

    eprintln!("load_p95_ms={:.3}", load_p95.as_secs_f64() * 1_000.0);
    eprintln!(
        "replacement_first_count_ms={:.3}",
        first_count.as_secs_f64() * 1_000.0
    );
    for (overlap, elapsed, resident) in &samples {
        eprintln!(
            "overlap={overlap}% count_ms={:.3} resident_bytes={resident}",
            elapsed.as_secs_f64() * 1_000.0
        );
    }
    assert!(load_p95.as_millis() <= 300, "startup load p95 target");
    assert!(
        samples
            .iter()
            .filter(|(overlap, _, _)| *overlap != 100)
            .all(|(_, elapsed, _)| elapsed.as_millis() <= 10),
        "partial-overlap count target"
    );
}

#[test]
#[ignore = "30-minute unique-content resource soak"]
fn unique_content_soak_keeps_counter_resident_bytes_bounded() {
    let duration = std::env::var("CODEXSHIM_TOKEN_SOAK_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(
            std::time::Duration::from_secs(30 * 60),
            std::time::Duration::from_secs,
        );
    let prototype = O200kPrototype::load_embedded().expect("load ranks");
    let mut counter = prototype
        .fork_counter(CounterLimits::default())
        .expect("counter");
    let started = Instant::now();
    let mut iterations = 0_u64;
    let mut peak_resident = counter.metrics().resident_bytes;
    while started.elapsed() < duration {
        let mut text = String::with_capacity(8_192);
        for offset in 0..160_u64 {
            let value = iterations.wrapping_mul(163).wrapping_add(offset);
            text.push(' ');
            for shift in 0..12 {
                let letter = u8::try_from((value.rotate_left(shift) % 26) + u64::from(b'a'))
                    .expect("ASCII letter");
                text.push(char::from(letter));
            }
        }
        assert!(matches!(
            counter.count_ordinary_up_to(&text, usize::MAX, || false),
            CountUpTo::Exact(_)
        ));
        peak_resident = peak_resident.max(counter.metrics().resident_bytes);
        iterations = iterations.saturating_add(1);
    }
    eprintln!(
        "soak_seconds={:.3} iterations={iterations} peak_resident_bytes={peak_resident} final_resident_bytes={}",
        started.elapsed().as_secs_f64(),
        counter.metrics().resident_bytes
    );
    assert!(peak_resident <= 8 * 1024 * 1024);
}

#[test]
#[ignore = "release exact-interval matrix"]
fn exact_interval_matrix_is_bounded() {
    assert!(!cfg!(debug_assertions), "run this gate with --release");
    let prototype = O200kPrototype::load_embedded().expect("load ranks");
    for (name, pattern) in [
        ("ascii", "build output line "),
        ("code", "fn main() { println!(\"value\"); }\n"),
        ("json", r#"{"key":"value","items":[1,2,3]}"#),
        ("cjk", "繁體中文简体中文日本語한국어"),
        ("emoji", "👨‍👩‍👧‍👦👍🏽e\u{301}️"),
    ] {
        for bytes in [8 * 1024, 16 * 1024, 32 * 1024] {
            let text = sized_fixture(pattern, bytes);
            let mut timings = Vec::new();
            for _ in 0..7 {
                let mut counter = prototype
                    .fork_counter(CounterLimits::default())
                    .expect("counter");
                let started = Instant::now();
                assert_ne!(
                    counter.count_ordinary_up_to(&text, 9_872, || false),
                    CountUpTo::Cancelled
                );
                timings.push(started.elapsed());
            }
            timings.sort_unstable();
            let p95 = timings[timings.len() - 1];
            eprintln!(
                "exact_fixture={name} bytes={} p95_ms={:.3}",
                text.len(),
                p95.as_secs_f64() * 1_000.0
            );
        }
    }
}
