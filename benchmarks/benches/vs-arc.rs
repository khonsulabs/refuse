//! A single-threaded benchmark run by Criterion.

use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use refuse::{CollectionGuard, Ref, Root};

fn arc_string() -> Arc<[u8; 32]> {
    Arc::new([0; 32])
}

fn root_string(guard: &mut CollectionGuard<'_>) -> Root<[u8; 32]> {
    Root::new([0; 32], guard)
}
fn ref_string(guard: &mut CollectionGuard<'_>) -> Ref<[u8; 32]> {
    Ref::new([0; 32], guard)
}

fn criterion_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("alloc-drop");
    group.bench_function("arc", |b| b.iter(|| black_box(arc_string())));

    group.bench_function("ref", |b| {
        b.iter(move || {
            let mut guard = CollectionGuard::acquire();
            black_box(ref_string(&mut guard))
        });
    });

    group.bench_function("root", |b| {
        b.iter(move || {
            let mut guard = CollectionGuard::acquire();
            black_box(root_string(&mut guard))
        });
    });
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
