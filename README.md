# Refuse

An easy-to-use, incremental, multi-threaded garbage collector for Rust.

```rust
use refuse::{CollectionGuard, Root, Ref};

// Execute a closure with access to a garbage collector.
refuse::collected(|| {
    let guard = CollectionGuard::acquire();
    // Allocate a vec![Ref(1), Ref(2), Ref(3)].
    let values: Vec<Ref<u32>> = (1..=3).map(|value| Ref::new(value, &guard)).collect();
    let values = Root::new(values, &guard);
    drop(guard);

    // Manually execute the garbage collector. Our data will not be freed,
    // since `values` is a "root" reference.
    refuse::collect();

    // Root references allow direct access to their data, even when a
    // `CollectionGuard` isn't held.
    let (one, two, three) = (values[0], values[1], values[2]);

    // Accessing the data contained in a `Ref` requires a guard, however.
    let mut guard = CollectionGuard::acquire();
    assert_eq!(one.load(&guard), Some(&1));
    assert_eq!(two.load(&guard), Some(&2));
    assert_eq!(three.load(&guard), Some(&3));

    // Dropping our root will allow the collector to free our `Ref`s.
    drop(values);
    guard.collect();
    assert_eq!(one.load(&guard), None);
});
```

As the version number indicates, this crate is in early development. No semver
compatibility will be provided until `0.1.0`.

## Motivation

While working on [Muse][muse], @Ecton recognized [the need for garbage
collection][gc-issue] to prevent untrusted scripts from uncontrollably leaking
memory. After surveying the landscape, he didn't find any that would easily
incorporate into his vision for the language. As far as he can tell, the design
choices of this collector are different than any existing collector in Rust.

## Design

This crate exposes a completely safe API for an incremental, multi-threaded,
tracing garbage collector in Rust.

Tracing garbage collectors can be implemented in various ways to identify the
"roots" of known memory so that they can trace from the roots through all
necessary references to determine which memory allocations can be freed.

This crate exposes a `Root<T>` type which behaves similarly to an `Arc<T>` but
automatically becomes a root for the collector. `Root<T>` implements
`Deref<Target = T>`, allowing access to the underlying data even while the
collector is running.

The `Ref<T>` type implements `Copy` and does not provide direct access to the
underlying data. To get a reference to the underlying data, a weak reference
must be upgraded using a `CollectionGuard`. The returned reference is tied to
the guard, which prevents collection from running while any guards are held.

A `CollectionGuard` is needed to:

- Allocate a new `Root<T>` or `Ref<T>`
- Load an `&T` from a `Ref<T>`

## Safety

This crate has safety comments next to each usage of unsafe, but and passes Miri
tests when provided the flags:

```sh
MIRIFLAGS="-Zmiri-permissive-provenance -Zmiri-ignore-leaks" cargo +nightly miri test
```

- `-Zmiri-permissive-provenance`: `parking_lot` internally casts a usize to a
  pointer, which breaks pointer provenance rules. Pointer provinence is
  currently only an experimental model, and nothing this collector is using
  from `parking_lot` couldn't be implemented in a fashion that honors pointer
  provinence. Thus, this library's author consider's this an implementation
  detail that can be ignored.
- `-Zmiri-ignore-leaks`: This crate spawns a global collector thread that never
  shuts down. Miri detects that the main thread does not wait for spawned
  threads to shut down and warns about this potential memory leak. When a thread
  is shut down and all of its data is no longer reachable, the thread storage
  will be cleaned up. However, the collector never shuts down and assumes that
  new threads could still be spawned at any given time.

  Additionally, on some platforms the main thread's thread-local storage may not
  be cleaned up when the main thread exits according to [`LocalKey`'s
  documentation][localkey]

This crate exposes a safe API that guarantees no undefined behavior can be
triggered by incorrectly using the API or implementing the `Collectable` trait
incorrectly. **Incorrect usage of this crate can lead to deadlocks and memory
leaks.** Specifically:

- Reference cycles between `Root<T>`'s will lead to leaks just as `Arc<T>`'s
  will.
- If a `Root<T>` uses locking for interior mutability, holding a lock without
  a collector guard can cause the garbage collector to block until the lock is
  released. This escalates from a pause to a deadlock if the lock can't be
  released without acquiring a collection guard. **All locks should acquired and
  dropped only while a `CollectorGuard` is acquired.**

## What's left

- **Finalizers**: Currently Drop is executed, but there's no way to attach
  behavior to run before the object is dropped.
- **More advanced algorithm**: The current algorithm employed is the naive
  mark-and-sweep. It performs well for smaller sets, but will become slower as
  the memory sets grow larger. Other algorithms may be considered, but the
  current naive algorithm is probably suitable for its application
  ([Muse][muse]).

## Benchmarks

Benchmarking is hard. These benchmarks aren't adequate. These numbers are from
executing `benches/timings.rs`, which compares allocating 100,000 32-byte values,
comparing the time it takes to allocate each `Arc<[u8; 32]>`, `Root<[u8;32]>`,
and `Ref<[u8; 32]>`. The measurements are the amount of time it takes for an
individual allocation. These results are from running on a Ryzen 3700X.

### 1 thread

| Label | avg     | min     | max     | stddev  | out%   |
|-------|---------|---------|---------|---------|--------|
| Arc   | 47.39ns | 20.00ns | 8.680us | 153.2ns | 0.010% |
| Ref   | 58.94ns | 30.00ns | 286.1us | 1.191us | 0.002% |
| Root  | 84.67ns | 40.00ns | 138.6us | 1.537us | 0.001% |

### 4 threads

| Label | avg     | min     | max     | stddev  | out%   |
|-------|---------|---------|---------|---------|--------|
| Arc   | 47.55ns | 20.00ns | 8.670us | 144.1ns | 0.010% |
| Ref   | 76.43ns | 30.00ns | 320.4us | 2.428us | 0.000% |
| Root  | 152.8ns | 40.00ns | 155.6us | 2.740us | 0.001% |

### 8 threads

| Label | avg     | min     | max     | stddev  | out%   |
|-------|---------|---------|---------|---------|--------|
| Arc   | 54.60ns | 20.00ns | 16.60us | 159.1ns | 0.010% |
| Ref   | 99.02ns | 30.00ns | 586.0us | 3.690us | 0.000% |
| Root  | 302.6ns | 40.00ns | 721.7us | 5.727us | 0.002% |

### 16 threads

| Label | avg     | min     | max     | stddev  | out%   |
|-------|---------|---------|---------|---------|--------|
| Arc   | 58.38ns | 20.00ns | 920.9us | 862.5ns | 0.000% |
| Ref   | 211.1ns | 30.00ns | 1.216ms | 10.41us | 0.000% |
| Root  | 675.6ns | 40.00ns | 1.565ms | 16.42us | 0.002% |

### 32 threads

| Label | avg     | min     | max     | stddev  | out%   |
|-------|---------|---------|---------|---------|--------|
| Arc   | 68.82ns | 20.00ns | 2.491ms | 1.680us | 0.000% |
| Ref   | 425.8ns | 30.00ns | 3.121ms | 21.73us | 0.000% |
| Root  | 1.538us | 40.00ns | 2.724ms | 33.31us | 0.002% |

### Author's Benchmark Summary

In these benchmarks, 100 allocations are collected into a pre-allocated `Vec`.
The `Vec` is cleared, and then the process is repeated 1,000 total times
yielding 100,000 total allocations.

In both the `Root` and `Ref` benchmarks, explicit calls to
`CollectorGuard::yield_to_collector()` are placed after the `Vec` is cleared.
The measurements include time waiting for the incremental garbage collector to
run during these yield points.

The CPU that is executing the benchmarks listed above has 16 cores. As the
numbers in the benchmarks show, the closer the CPU is to being fully saturated,
the more garbage collection impacts the performance.

There are plenty of opportunities to improve the performance, but incremental
garbage collection requires pausing all threads briefly to perform the
collection. These pauses are generally short when there are few active threads,
but when many threads are active, the pauses can be significant.

[muse]: https://github.com/khonsulabs/muse
[gc-issue]: https://github.com/khonsulabs/muse/issues/4
[localkey]: https://doc.rust-lang.org/std/thread/struct.LocalKey.html#platform-specific-behavior
