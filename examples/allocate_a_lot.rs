use musegc::{collected, CollectionGuard, Ref};

fn main() {
    // collected(|| {
    //     let mut guard = CollectionGuard::acquire();
    //     for _ in 0..1000 {
    //         Ref::new([0; 32], &mut guard);
    //     }
    //     guard.collect();
    //     for _ in 0..1000 {
    //         Ref::new([0; 32], &mut guard);
    //     }
    //     guard.collect();
    // });
    std::thread::scope(|s| {
        for _ in 0..16 {
            s.spawn(|| {
                collected(|| {
                    let mut guard = CollectionGuard::acquire();
                    for _ in 0..100 {
                        for _ in 0..100 {
                            Ref::new([0; 32], &mut guard);
                        }
                        guard.yield_to_collector();
                    }
                });
            });
        }
    });
}
