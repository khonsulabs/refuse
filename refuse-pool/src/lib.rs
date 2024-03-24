//! Garbage-collected "interned" strings.
//!
//! Interning is a process of making many equal things share the same underlying
//! resource. This crate introduces two types that are powered by the
//! [Refuse][refuse] garbage collector:
//!
//! - [`RootString`]: A `Root<String>`-like type that ensures all instances of
//!       the same exact byte sequence refer to the same allocation.
//! - [`RefString`]: A `Ref<String>` type that is a reference to a
//!       [`RootString`].
//!
//! ```rust
//! use refuse::CollectionGuard;
//! use refuse_pool::{RefString, RootString};
//!
//! refuse::collected(|| {
//!     let a = RootString::from("a");
//!     let a_again = RootString::from(String::from("a"));
//!
//!     // Both a and a_again point to the same underlying storage.
//!     assert_eq!(a.root_count(), 2);
//!     // Comparing two RootStrings is cheap.
//!     assert_eq!(a, a_again);
//!
//!     // a_ref can be used to gain a reference to a string,
//!     // but only until the string is unreachable.
//!     let a_ref = a.downgrade();
//!
//!     let mut guard = CollectionGuard::acquire();
//!     assert_eq!(a_ref.load(&guard), Some("a"));
//!
//!     drop(a);
//!     drop(a_again);
//!     guard.collect();
//!     assert_eq!(a_ref.load(&guard), None);
//! });
//! ```
//!
//! [refuse]: https://github.com/khonsulabs/refuse

use std::borrow::Cow;
use std::fmt::{Debug, Display};
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::{Mutex, OnceLock};

use ahash::AHasher;
use hashbrown::HashTable;
use refuse::{AnyRef, CollectionGuard, LocalPool, Ref, Root, SimpleType};

#[derive(Default)]
struct StringPool {
    allocator: LocalPool,
    strings: HashTable<RootString>,
}

impl StringPool {
    fn global() -> &'static Mutex<StringPool> {
        static POOL: OnceLock<Mutex<StringPool>> = OnceLock::new();
        POOL.get_or_init(Mutex::default)
    }

    fn intern(&mut self, key: Cow<'_, str>) -> &RootString {
        let hash = hash_str(key.as_ref());
        self.strings
            .entry(hash, |a| &*a.0.string == key.as_ref(), |e| e.0.hash)
            .or_insert_with(|| {
                RootString(Root::new(
                    PooledString {
                        hash,
                        string: match key {
                            Cow::Borrowed(str) => Box::from(str),
                            Cow::Owned(str) => str.into_boxed_str(),
                        },
                    },
                    &self.allocator.enter(),
                ))
            })
            .into_mut()
    }
}

fn hash_str(str: &str) -> u64 {
    let mut hasher = AHasher::default();
    str.hash(&mut hasher);
    hasher.finish()
}

struct StoredString(Root<PooledString>);

impl Hash for StoredString {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash.hash(state);
    }
}

impl Eq for StoredString {}

impl PartialEq for StoredString {
    fn eq(&self, other: &Self) -> bool {
        self.0.hash == other.0.hash && self.0.string == other.0.string
    }
}

impl PartialEq<&'_ str> for StoredString {
    fn eq(&self, other: &&'_ str) -> bool {
        &*self.0.string == *other
    }
}

/// A "root" reference to a garbage collected, interned string.
///
/// This type is cheap to check equality because it ensures each unique string
/// is allocated only once, and references are reused automatically.
#[derive(Clone)]
pub struct RootString(Root<PooledString>);

impl RootString {
    /// Returns a root reference to a garabge collected string that contains
    /// `s`.
    ///
    /// If another `RootString` exists already with the same contents as `s`, it
    /// will be returned and `s` will be dropped.
    pub fn new<'a>(s: impl Into<Cow<'a, str>>) -> Self {
        let mut pool = StringPool::global().lock().expect("poisoned");
        pool.intern(s.into()).clone()
    }

    /// Returns a reference to this root string.
    pub fn downgrade(&self) -> RefString {
        RefString(self.0.downgrade())
    }

    /// Returns a typeless reference to this string.
    pub fn as_any(&self) -> AnyRef {
        self.0.as_any()
    }

    /// Returns the number of root references to this string, `self` included.
    pub fn root_count(&self) -> u64 {
        // We subtract one because the string pool always contains a reference.
        // Including it in the publicly viewable count would just lead to
        // confusion, as it did for @ecton when writing the first doctest for
        // this crate.
        self.0.root_count() - 1
    }
}

impl Drop for RootString {
    fn drop(&mut self) {
        if self.0.root_count() == 2 {
            // This is the last `RootString` aside from the one stored in the
            // pool, so we should remove the pool entry.
            let mut pool = StringPool::global().lock().expect("poisoned");
            let Ok(entry) = pool
                .strings
                .find_entry(self.0.hash, |s| Root::ptr_eq(&s.0, &self.0))
                .map(|entry| entry.remove().0)
            else {
                return;
            };
            drop(pool);
            // We delay dropping the removed entry to ensure that we don't
            // re-enter this block and cause a deadlock.
            drop(entry);
        }
    }
}

impl Debug for RootString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&self.0.string, f)
    }
}

impl Display for RootString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.0.string, f)
    }
}

impl From<&'_ str> for RootString {
    fn from(value: &'_ str) -> Self {
        Self::new(value)
    }
}

impl From<&'_ String> for RootString {
    fn from(value: &'_ String) -> Self {
        Self::new(value)
    }
}

impl From<String> for RootString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl Hash for RootString {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.downgrade().hash(state);
    }
}

impl Eq for RootString {}

impl PartialEq for RootString {
    fn eq(&self, other: &Self) -> bool {
        Root::ptr_eq(&self.0, &other.0)
    }
}

impl PartialEq<str> for RootString {
    fn eq(&self, other: &str) -> bool {
        &*self.0.string == other
    }
}

impl PartialEq<&'_ str> for RootString {
    fn eq(&self, other: &&'_ str) -> bool {
        self == *other
    }
}

impl PartialEq<String> for RootString {
    fn eq(&self, other: &String) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<&'_ String> for RootString {
    fn eq(&self, other: &&'_ String) -> bool {
        self == *other
    }
}

impl Ord for RootString {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.string.cmp(&other.0.string)
    }
}

impl PartialOrd for RootString {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Deref for RootString {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

struct PooledString {
    hash: u64,
    string: Box<str>,
}

impl SimpleType for PooledString {}

impl Deref for PooledString {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.string
    }
}

/// A weak reference to a garbage collected, interned string.
#[derive(Copy, Clone, Hash, Eq, PartialEq)]
pub struct RefString(Ref<PooledString>);

impl RefString {
    /// Loads a reference to the underlying string, if the string hasn't been
    /// freed.
    pub fn load<'guard>(&self, guard: &'guard CollectionGuard) -> Option<&'guard str> {
        self.0.load(guard).map(|pooled| &*pooled.string)
    }

    /// Loads this string as a root, if the string hasn't been freed.
    pub fn as_root(&self, guard: &CollectionGuard) -> Option<RootString> {
        self.0.as_root(guard).map(RootString)
    }

    /// Returns a typeless reference to this string.
    pub fn as_any(&self) -> AnyRef {
        self.0.as_any()
    }
}

impl PartialEq<RootString> for RefString {
    fn eq(&self, other: &RootString) -> bool {
        self.0 == other.0.downgrade()
    }
}

impl PartialEq<RefString> for RootString {
    fn eq(&self, other: &RefString) -> bool {
        *other == *self
    }
}

#[test]
fn intern() {
    let mut guard = CollectionGuard::acquire();
    let a = RootString::from("a");
    let b = RootString::from("a");
    assert!(Root::ptr_eq(&a.0, &b.0));

    let as_ref = a.downgrade();
    drop(a);
    drop(b);
    assert_eq!(as_ref.load(&guard), Some("a"));

    guard.collect();

    let _a = RootString::from("a");
    assert!(as_ref.load(&guard).is_none());
}
