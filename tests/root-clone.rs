use refuse::{collected, CollectionGuard, Root};

#[test]
fn clone() {
    collected(|| {
        let mut guard = CollectionGuard::acquire();
        let root = Root::new(42_u32, &guard);
        let clone = root.clone();
        let reference = root.downgrade();

        assert_eq!(reference.load(&guard), Some(&42));

        // This collection should not remove anything.
        guard.collect();
        assert_eq!(reference.load(&guard), Some(&42));

        // Drop the root reference.
        drop(root);

        // We still have `clone`, this should still not free our data.
        guard.collect();
        assert_eq!(reference.load(&guard), Some(&42));

        // Drop the clone, and now the data should be freed.
        drop(clone);
        guard.collect();
        assert_eq!(reference.load(&guard), None);
    });
}
