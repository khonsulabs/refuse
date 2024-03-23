//! This example shows how the `Collectable` derive implements tracing
//! automatically.

use refuse::{collected, CollectionGuard, MapAs, Ref, Root, Trace};

#[derive(Trace, MapAs)]
struct Error {
    message: Ref<String>,
}

#[collected]
fn main() {
    let mut guard = CollectionGuard::acquire();

    let message = Ref::new(String::from("Hello!"), &guard);
    let error = Root::new(Error { message }, &guard);

    // Because `error` is a Root and refers to the message,
    guard.collect();

    assert_eq!(message.load(&guard).expect("still alive"), "Hello!");

    // After we drop the Root, the message will be able to be collected.
    drop(error);
    guard.collect();
    assert_eq!(message.load(&guard), None);
}
