//! A basic usage example demonstrating the garbage collector.
use musegc::{CollectionGuard, Ref, Root};

fn main() {
    // Execute a closure with access to a garbage collector.
    musegc::collected(|| {
        let mut guard = CollectionGuard::acquire();
        // Allocate a vec![Ref(1), Ref(2), Ref(3)].
        let values: Vec<Ref<u32>> = (1..=3).map(|value| Ref::new(value, &mut guard)).collect();
        let values = Root::new(values, &mut guard);
        drop(guard);

        // Manually execute the garbage collector. Our data will not be freed,
        // since `values` is a "root" reference.
        musegc::collect();

        // Root references allow direct access to their data, even when a
        // `CollectionGuard` isn't held.
        let (one, two, three) = (values[0], values[1], values[2]);

        // Accessing the data contained in a `Ref` requires a guard, however.
        let mut guard = CollectionGuard::acquire();
        assert_eq!(one.load(&guard).unwrap(), Some(&1));
        assert_eq!(two.load(&guard).unwrap(), Some(&2));
        assert_eq!(three.load(&guard).unwrap(), Some(&3));

        // Dropping our root will allow the collector to free our `Ref`s.
        drop(values);
        guard.collect();
        assert_eq!(one.load(&guard).unwrap(), None);
    });
}
