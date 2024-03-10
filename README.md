# musegc

> This name is a placeholder. The design of this crate has nothing to do with
> [Muse][muse], so naming it `muse-gc` seems misleading.

An easy-to-use, incremental, multi-threaded garbage collector for Rust.

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

This crate exposes a `Strong<T>` type which behaves similarly to an `Arc<T>` but
automatically becomes a root for the collector. `Strong<T>` implements
`Deref<Target = T>`, allowing access to the underlying data even while the
collector is running.

The `Weak<T>` type implements `Copy` and does not provide direct access to the
underlying data. To get a reference to the underlying data, a weak reference
must be upgraded using a `CollectionGuard`. The returned reference is tied to
the guard, which prevents collection from running while any guards are held.

A `CollectionGuard` is needed to:

- Allocate a new `Strong<T>` or `Weak<T>`
- Upgrade a `Weak<T>` to an `&T`

## Safety

**Here Be Dragons**. This crate is missing all safety comments, but passes
miri tests when provided the flags:

```sh
MIRIFLAGS="-Zmiri-permissive-provenance -Zmiri-ignore-leaks" cargo +nightly miri test
```

- `-Zmiri-permissive-provenance`: `parking_lot` internally casts a usize to a
  pointer, which breaks pointer provenance rules. Pointer provinence is
  currently only an experimental model, and nothing this collector is using
  from `parking_lot` couldn't be implemented in a fashion that honors pointer
  provinence. Thus, this library's author consider's this an implementation
  detail that can be ignored.
- `-Zmiri-ignore-leaks`: This crate uses thread local storage which is
  documented to not always run destructors for local keys on the main thread, as
  some platforms abort rather than performing cleanup code.

This crate exposes a safe API that guarantees no undefined behavior can be
triggered by incorrectly using the API or implementing the `Collectable` trait
incorrectly. **Incorrect usage of this crate can lead to deadlocks and memory
leaks.** Specifically:

- Reference cycles between `Strong<T>`'s will lead to leaks just as `Arc<T>`'s
  will.
- If a `Strong<T>` uses locking for interior mutability, holding a lock without
  a collector guard can cause the garbage collector to block until the lock is
  released. This escalates from a pause to a deadlock if the lock can't be
  released without acquiring a collection guard. **All locks should acquired and
  dropped only while a `CollectorGuard` is acquired.**

## What's left

- **Safety Documentation**: While Miri passes running examples and tests, this
  crate's safety model needs to be documented with an overview somewhere that
  inline safety comments can reference.
- **Better automatic collector heuristics**: The collector should measure how
  long it takes to run and use that measurement to scale scheduled collections.
- **More advanced algorithm**: The current algorithm employed is the naive
  mark-and-sweep. It performs well for smaller sets, but will become slower as
  the memory sets grow larger. Other algorithms may be considered, but the
  current naive algorithm is probably suitable for its application
  ([Muse][muse]).

## Benchmarks

Benchmarking is hard. These benchmarks aren't adequate. These numbers are from
executing `benches/timings.rs`, which compares allocating 100,000 32-byte values,
comparing the time it takes to allocate each `Arc<[u8; 32]>`, `Strong<[u8;32]>`,
and `Weak<[u8; 32]>`. The measurements are the amount of time it takes for an
individual allocation. These results are from running on a Ryzen 3700X.

#### 1 thread

| Label  | avg     | min     | max     | stddev  | out%   |
|--------|---------|---------|---------|---------|--------|
| Arc    | 44.53ns | 20.00ns | 33.82us | 182.9ns | 0.010% |
| Strong | 63.27ns | 30.00ns | 442.7us | 1.422us | 0.002% |
| Weak   | 67.05ns | 30.00ns | 183.1us | 790.9ns | 0.003% |

#### 4 threads

| Label  | avg     | min     | max     | stddev  | out%   |
|--------|---------|---------|---------|---------|--------|
| Arc    | 45.08ns | 20.00ns | 11.30us | 135.8ns | 0.010% |
| Strong | 75.56ns | 30.00ns | 387.7us | 2.330us | 0.000% |
| Weak   | 71.03ns | 30.00ns | 854.3us | 2.452us | 0.000% |

#### 8 threads

| Label  | avg     | min     | max     | stddev  | out%   |
|--------|---------|---------|---------|---------|--------|
| Arc    | 49.55ns | 20.00ns | 16.61us | 145.7ns | 0.010% |
| Strong | 138.5ns | 30.00ns | 854.6us | 5.905us | 0.000% |
| Weak   | 92.53ns | 30.00ns | 1.418ms | 4.925us | 0.000% |

#### 16 threads

| Label  | avg     | min     | max     | stddev  | out%   |
|--------|---------|---------|---------|---------|--------|
| Arc    | 57.23ns | 20.00ns | 255.1us | 352.8ns | 0.010% |
| Strong | 420.8ns | 30.00ns | 3.133ms | 22.28us | 0.000% |
| Weak   | 222.6ns | 30.00ns | 3.320ms | 17.23us | 0.000% |

#### 32 threads

| Label  | avg     | min     | max     | stddev  | out%   |
|--------|---------|---------|---------|---------|--------|
| Arc    | 69.91ns | 20.00ns | 352.2us | 888.3ns | 0.001% |
| Strong | 1.673us | 30.00ns | 14.64ms | 91.80us | 0.000% |
| Weak   | 1.084us | 30.00ns | 12.59ms | 79.43us | 0.000% |

### Author's Benchmark Summary

In these benchmarks, 100 allocations are collected into a pre-allocated `Vec`.
The `Vec` is cleared, and then the process is repeated 1,000 total times
yielding 100,000 total allocations.

In both the `Strong` and `Weak` benchmarks, explicit calls to
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
