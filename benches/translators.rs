//! Translator parsing benchmarks: serde_json vs simd-json on real-shape bodies.
//!
//! Decision-driving harness — see BENCHMARKS.md for the resulting numbers and
//! the adopt/skip choice. The fixtures live under `benches/fixtures/` and are
//! embedded at compile time so the bench is hermetic.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use amp_proxy::customproxy::responses_translator::translate_responses_request_to_chat;

/// Synthetic Anthropic Messages body (~4 KB).
const SMALL: &[u8] = include_bytes!("fixtures/small.json");
/// Synthetic Gemini generateContent body (~48 KB).
const MEDIUM: &[u8] = include_bytes!("fixtures/medium.json");
/// Synthetic OpenAI Responses body (~140 KB, 25+ input items, 5+ tool calls).
const LARGE: &[u8] = include_bytes!("fixtures/large.json");

fn parse_serde(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice::<serde_json::Value>(body).expect("serde parse")
}

fn parse_simd_owned(body: &[u8]) -> serde_json::Value {
    let mut buf = body.to_vec();
    simd_json::serde::from_slice::<serde_json::Value>(&mut buf).expect("simd owned parse")
}

fn parse_simd_borrowed(body: &[u8]) {
    let mut buf = body.to_vec();
    let v = simd_json::to_borrowed_value(&mut buf).expect("simd borrowed parse");
    // Drop without traversal — pure parse cost.
    drop(v);
}

fn translate_full(body: &[u8]) -> Vec<u8> {
    let (out, _ctx) = translate_responses_request_to_chat(body).expect("translate");
    out
}

fn bench_parsing(c: &mut Criterion) {
    let cases: [(&str, &[u8]); 3] = [("small", SMALL), ("medium", MEDIUM), ("large", LARGE)];

    let mut g = c.benchmark_group("parse");
    for (label, body) in cases {
        g.throughput(Throughput::Bytes(body.len() as u64));

        g.bench_with_input(BenchmarkId::new("serde_json", label), &body, |b, &body| {
            b.iter(|| black_box(parse_serde(black_box(body))))
        });
        g.bench_with_input(
            BenchmarkId::new("simd_json_owned", label),
            &body,
            |b, &body| b.iter(|| black_box(parse_simd_owned(black_box(body)))),
        );
        g.bench_with_input(
            BenchmarkId::new("simd_json_borrowed", label),
            &body,
            |b, &body| b.iter(|| parse_simd_borrowed(black_box(body))),
        );
    }
    g.finish();
}

fn bench_translate(c: &mut Criterion) {
    // Only the LARGE body actually fits the OpenAI Responses input shape that
    // translate_responses_request_to_chat consumes; small/medium are different
    // upstream shapes and would no-op the input walk. Run translate on LARGE
    // so the parse-vs-walk ratio is meaningful for the decision.
    let mut g = c.benchmark_group("translate_responses");
    g.throughput(Throughput::Bytes(LARGE.len() as u64));
    g.bench_function(BenchmarkId::new("serde_full", "large"), |b| {
        b.iter(|| black_box(translate_full(black_box(LARGE))))
    });
    // Isolate the parse step on the same body so callers can compute
    // (translate - parse) ≈ graph rebuild + serialize cost.
    g.bench_function(BenchmarkId::new("parse_only_serde", "large"), |b| {
        b.iter(|| black_box(parse_serde(black_box(LARGE))))
    });
    g.bench_function(BenchmarkId::new("parse_only_simd_owned", "large"), |b| {
        b.iter(|| black_box(parse_simd_owned(black_box(LARGE))))
    });
    g.finish();
}

criterion_group!(benches, bench_parsing, bench_translate);
criterion_main!(benches);
