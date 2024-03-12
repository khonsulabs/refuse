use musegc::{collected, CollectionGuard, Root};

#[test]
fn lifecycle() {
    collected(|| {
        let mut guard = CollectionGuard::acquire();
        let strong = Root::new(42_u32, &mut guard);
        let weak = strong.downgrade();

        assert_eq!(weak.load(&guard).unwrap(), Some(&42));

        // This collection should not remove anything.
        guard.collect();
        assert_eq!(weak.load(&guard).unwrap(), Some(&42));

        // Drop the strong reference
        drop(strong);

        // Now collection should remove the value.
        guard.collect();

        assert_eq!(weak.load(&guard).unwrap(), None);
    });
}
