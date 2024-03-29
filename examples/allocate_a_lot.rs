//! An example demonstrating multithreaded allocations.
//!
//! This exists an easy executable to tweak and run with profiling tools.
use refuse::{CollectionGuard, Ref};

fn main() {
    std::thread::scope(|s| {
        for _ in 0..16 {
            s.spawn(|| {
                let mut guard = CollectionGuard::acquire();
                for _ in 0..100 {
                    for _ in 0..100 {
                        Ref::new([0; 32], &guard);
                    }
                    guard.yield_to_collector();
                }
            });
        }
    });
}
