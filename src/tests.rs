//! This module contains tests that are written without relying on when the
//! garbage collector runs. Because tests are run in parallel, there's an edge
//! case that only impacts unit tests that prevents `collect()` from being
//! guaranteed to release values dropped directly before the call.
//!
//! This is an edge case would slow the garbage collector down slightly to
//! address, and it does not actually affect real-world use cases.
//!
//! # More info
//!
//! Consider the following series of events:
//!
//! | Test 1  | Test 2  | Collector  |
//! |---------|---------|------------|
//! | start   |         |            |
//! | guard   |         | start      |
//! | <work>  |         |            |
//! | collect | start   |            |
//! |         | guard   | locking    |
//! |         | <work>  |            |
//! |         | collect | collecting |
//!
//! When a thread first accesses the garbage collector, it registers itself by
//! sending a message over a channel to the global collector. Once the garbage
//! collector begins its locking process, it will not process new messages on
//! that channel until it is done collecting.
//!
//! If test 2's guard acquisition succeeds before the collector begins locking
//! and its registration message arrives after the collector begins locking, the
//! garbage collector will not trace test 2's thread during the run it is
//! beginning. Test 2's thread will be traced on the next collection.
//!
//! This subtle behavior can cause tests that rely on values being dropped after
//! invoking collect to fail spuriously, but only when running multiple tests at
//! the same time.
//!
//! For this reason, most unit tests are written in the tests/ directory. When
//! executing `cargo test`, each of these files is executed independently.

use crate::{CollectionGuard, Root};

#[test]
fn casting() {
    let guard = CollectionGuard::acquire();
    let value = Root::new(2_u32, &guard);
    let any = value.as_any();
    assert_eq!(any.downcast_ref().load(&guard), Some(&2_u32));
    assert_eq!(any.downcast_ref::<u16>().load(&guard), None);
}
