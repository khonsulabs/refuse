use refuse::{collected, CollectionGuard, Root};

#[test]
#[collected]
fn lifecycle() {
    let mut guard = CollectionGuard::acquire();
    let root = Root::new(42_u32, &guard);
    let reference = root.downgrade();

    assert_eq!(reference.load(&guard), Some(&42));

    // This collection should not remove anything.
    guard.collect();
    assert_eq!(reference.load(&guard), Some(&42));

    // Drop the root reference.
    drop(root);

    // Now collection should remove the value.
    guard.collect();

    assert_eq!(reference.load(&guard), None);
}
