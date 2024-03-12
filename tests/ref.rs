use musegc::{collected, CollectionGuard, Ref};

#[test]
fn lifecycle() {
    collected(|| {
        let mut guard = CollectionGuard::acquire();
        let collected = Ref::new(42_u32, &mut guard);

        assert_eq!(collected.load(&guard).unwrap(), Some(&42));

        guard.collect();

        assert_eq!(collected.load(&guard).unwrap(), None);
    });
}
