use refuse::{CollectionGuard, Ref};

#[test]
fn lifecycle() {
    let mut guard = CollectionGuard::acquire();
    let collected = Ref::new(42_u32, &guard);

    assert_eq!(collected.load(&guard), Some(&42));

    guard.collect();

    assert_eq!(collected.load(&guard), None);
}
