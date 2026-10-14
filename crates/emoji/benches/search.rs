use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use signal_emoji::{SearchOptions, completion_at, search};

fn bench(c: &mut Criterion) {
    signal_emoji::warm_up();
    let opts = SearchOptions::default();
    // Each keystroke of type-ahead is a fresh search, so measure the whole ramp.
    for query in ["s", "sm", "smi", "smile", "slightly_sm", "face smiling", "smlie", "+1"] {
        c.bench_function(&format!("search {query:?}"), |b| {
            b.iter(|| search(black_box(query), &opts))
        });
    }
    c.bench_function("completion_at", |b| {
        let text = "this is a fairly long message with an emoji at the end :thu";
        b.iter(|| completion_at(black_box(text), text.len()))
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
