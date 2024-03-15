//! Shows how to implement `MapAs` to allow type-erased access to a mapped
//! type.
//!
//! Because ["custom DSTs are a largely half-baked feature for
//! now"][rustnomicon], this crate cannot directly wrap dynamically sized types
//! such as dynamic trait objects due to how its data structures work and the
//! required features being unstable.
//!
//! The `MapAs` trait allows a DST to be used as the `MapAs::Target` type,
//! and this enables `AnyRef` to load a reference to the `MapAs::Target` type
//! of any stored type.
//!
//! [rustnomicon]:
//!     https://doc.rust-lang.org/nomicon/exotic-sizes.html#dynamically-sized-types-dsts

use musegc::{collected, AnyRef, CollectionGuard, ContainsNoRefs, MapAs, Ref};

trait SomeTrait {
    fn do_something(&self);
}

struct SomeType;

impl MapAs for SomeType {
    type Target = dyn SomeTrait;

    fn map_as(&self) -> &Self::Target {
        self
    }
}

impl ContainsNoRefs for SomeType {}

impl SomeTrait for SomeType {
    fn do_something(&self) {
        println!("Did something!");
    }
}

fn main() {
    collected(|| {
        let guard = CollectionGuard::acquire();
        let gced: Ref<SomeType> = Ref::new(SomeType, &guard);
        let type_erased: AnyRef = gced.as_any();
        type_erased
            .load_mapped::<dyn SomeTrait>(&guard)
            .unwrap()
            .do_something();
    });
}
