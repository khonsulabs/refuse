use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use musegc::{collected, CollectionGuard, Strong, Weak};

fn arc_string() -> Arc<[u8; 32]> {
    Arc::new([0; 32])
}

fn strong_string(guard: &mut CollectionGuard) -> Strong<[u8; 32]> {
    Strong::new([0; 32], guard)
}
fn weak_string(guard: &mut CollectionGuard) -> Weak<[u8; 32]> {
    Weak::new([0; 32], guard)
}

fn criterion_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("alloc-drop");
    group.bench_function("arc", |b| b.iter(|| black_box(arc_string())));
    collected(|| {
        group.bench_function("weak", |b| {
            let mut guard = CollectionGuard::acquire();
            b.iter(|| {
                let result = black_box(weak_string(&mut guard));
                guard.yield_to_collector();
                result
            })
        });
        group.bench_function("strong", |b| {
            let mut guard = CollectionGuard::acquire();
            b.iter(|| {
                let result = black_box(strong_string(&mut guard));
                guard.yield_to_collector();
                result
            })
        });
    });
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
