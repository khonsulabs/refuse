use std::borrow::Cow;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::{Mutex, OnceLock};

use ahash::AHasher;
use hashbrown::HashTable;
use refuse::{CollectionGuard, LocalPool, Ref, Root, SimpleType};

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

#[derive(Clone)]
pub struct RootString(Root<PooledString>);

impl RootString {
    pub fn new<'a>(s: impl Into<Cow<'a, str>>) -> Self {
        let mut pool = StringPool::global().lock().expect("poisoned");
        pool.intern(s.into()).clone()
    }

    pub fn to_ref(&self) -> StringRef {
        StringRef(self.0.downgrade())
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
        self.0.hash.hash(state);
    }
}

impl Eq for RootString {}

impl PartialEq for RootString {
    fn eq(&self, other: &Self) -> bool {
        Root::ptr_eq(&self.0, &other.0)
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

#[derive(Copy, Clone)]
pub struct StringRef(Ref<PooledString>);

impl StringRef {
    pub fn load<'guard>(&self, guard: &'guard CollectionGuard) -> Option<&'guard str> {
        self.0.load(guard).map(|pooled| &*pooled.string)
    }
}

#[test]
fn intern() {
    refuse::collected(|| {
        let mut guard = CollectionGuard::acquire();
        let a = RootString::from("a");
        let b = RootString::from("a");
        assert!(Root::ptr_eq(&a.0, &b.0));

        let as_ref = a.to_ref();
        drop(a);
        drop(b);
        assert_eq!(as_ref.load(&guard), Some("a"));

        guard.collect();

        let _a = RootString::from("a");
        assert!(as_ref.load(&guard).is_none());
    });
}
