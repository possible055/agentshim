use std::sync::OnceLock;

use codexshim_gigatoken::{CountUpTo, CounterLimits, O200kCounter, O200kPrototype};

fn counter() -> O200kCounter {
    static PROTOTYPE: OnceLock<O200kPrototype> = OnceLock::new();
    PROTOTYPE
        .get_or_init(|| O200kPrototype::load_embedded().expect("load pinned o200k ranks"))
        .fork_counter(CounterLimits::default())
        .expect("fork counter")
}

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

#[test]
fn pinned_tiktoken_ordinary_counts_match() {
    let fixtures = [
        ("", 0),
        ("hello", 1),
        ("hello world", 2),
        ("\r\n\t   ", 2),
        (&format!("long {}", " ".repeat(100)), 3),
        ("fn main() { println!(\"hello\"); }", 9),
        (r#"{"key":"value","escaped":"\\\""}"#, 10),
        (
            r#"<root attr="value"><item>text &amp; more</item></root>"#,
            18,
        ),
        ("繁體中文與简体中文、日本語、한국어", 12),
        ("👨‍👩‍👧‍👦👍🏽e\u{301}️", 17),
        (r"C:\Projects\codexshim\src\main.rs", 12),
        ("/usr/local/bin/codexshim", 7),
        ("Error: stack\n  at foo.rs:42:7", 12),
        ("Wall time: 18446744073709551615.9999 seconds\nOutput:", 18),
    ];
    let mut counter = counter();
    for (text, expected) in fixtures {
        assert_eq!(
            counter.count_ordinary_up_to(text, usize::MAX, || false),
            CountUpTo::Exact(expected),
            "fixture {text:?}"
        );
    }
}

#[test]
fn exact_boundaries_and_ordinary_special_literals_match() {
    let mut counter = counter();
    for expected in [9_871, 9_872, 9_873, 9_999, 10_000, 10_001] {
        let text = " x".repeat(expected);
        assert_eq!(
            counter.count_ordinary_up_to(&text, expected, || false),
            CountUpTo::Exact(expected)
        );
        assert_eq!(
            counter.count_ordinary_up_to(&text, expected - 1, || false),
            CountUpTo::Exceeded
        );
    }
}

#[test]
fn replacement_large_corpus_matches_pinned_oracle() {
    let mut bytes = replacement_corpus_seed();
    while bytes.len() < 246_384 {
        bytes.extend_from_slice(include_bytes!("../../../README.md"));
    }
    bytes.truncate(246_384);
    let text = std::str::from_utf8(&bytes).expect("fixture remains UTF-8");
    assert_eq!(
        counter().count_ordinary_up_to(text, usize::MAX, || false),
        CountUpTo::Exact(57_475)
    );
}

#[test]
fn cancellation_and_cache_bounds_are_observable() {
    let limits = CounterLimits {
        short_cache_slots: 64,
        long_cache_entries: 2,
        long_cache_bytes: 80,
        retained_scratch_tokens: 64,
        cancellation_stride: 1,
    };
    let prototype = O200kPrototype::load_embedded().expect("load pinned o200k ranks");
    let mut counter = prototype.fork_counter(limits).expect("fork counter");
    let resident_before = counter.metrics().resident_bytes;

    for index in 0..20 {
        let suffix = char::from(b'a' + u8::try_from(index).expect("index fits u8"));
        let text = format!(" abcdefghijklmnopqrstuvwxyz{suffix}{}", "x".repeat(30));
        assert!(matches!(
            counter.count_ordinary_up_to(&text, usize::MAX, || false),
            CountUpTo::Exact(_)
        ));
    }
    let metrics = counter.metrics();
    assert!(metrics.resets > 0 || metrics.uncached > 0);
    assert!(metrics.resident_bytes <= resident_before + 16 * 1024);

    let mut checks = 0;
    assert_eq!(
        counter.count_ordinary_up_to(&"abcdef".repeat(2_000), usize::MAX, || {
            checks += 1;
            checks > 4
        }),
        CountUpTo::Cancelled
    );
}

#[test]
fn cached_pretokens_use_strided_cancellation_polling() {
    let mut counter = counter();
    let text = " x".repeat(2_000);
    assert!(matches!(
        counter.count_ordinary_up_to(&text, usize::MAX, || false),
        CountUpTo::Exact(_)
    ));

    let mut checks = 0_usize;
    assert_eq!(
        counter.count_ordinary_up_to(&text, usize::MAX, || {
            checks += 1;
            checks == 3
        }),
        CountUpTo::Cancelled
    );
    assert_eq!(checks, 3);
}
