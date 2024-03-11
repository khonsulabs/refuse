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
comparing the time it takes to allocate each `Arc<[u8; 32]>`, `Strong<[u8;32]>`,
and `Weak<[u8; 32]>`. The measurements are the amount of time it takes for an
individual allocation. These results are from running on a Ryzen 3700X.

### 1 thread

| Label  | avg     | min     | max     | stddev  | out%   |
|--------|---------|---------|---------|---------|--------|
| Arc    | 47.87ns | 20.00ns | 11.27us | 151.4ns | 0.010% |
| Strong | 62.02ns | 30.00ns | 182.2us | 1.367us | 0.000% |
| Weak   | 65.89ns | 30.00ns | 439.2us | 1.556us | 0.001% |

### 4 threads

| Label  | avg     | min     | max     | stddev  | out%   |
|--------|---------|---------|---------|---------|--------|
| Arc    | 47.21ns | 20.00ns | 5.810us | 138.0ns | 0.010% |
| Strong | 147.5ns | 30.00ns | 314.4us | 3.849us | 0.001% |
| Weak   | 71.03ns | 30.00ns | 633.3us | 2.464us | 0.000% |

### 8 threads

| Label  | avg     | min     | max     | stddev  | out%   |
|--------|---------|---------|---------|---------|--------|
| Arc    | 48.91ns | 20.00ns | 65.56us | 172.4ns | 0.010% |
| Strong | 186.8ns | 30.00ns | 729.9us | 6.224us | 0.000% |
| Weak   | 109.4ns | 30.00ns | 1.136ms | 5.817us | 0.000% |

### 16 threads

| Label  | avg     | min     | max     | stddev  | out%   |
|--------|---------|---------|---------|---------|--------|
| Arc    | 55.47ns | 20.00ns | 248.9us | 354.6ns | 0.010% |
| Strong | 323.1ns | 30.00ns | 3.105ms | 18.29us | 0.000% |
| Weak   | 206.6ns | 30.00ns | 3.492ms | 16.00us | 0.000% |

### 32 threads

| Label  | avg     | min     | max     | stddev  | out%   |
|--------|---------|---------|---------|---------|--------|
| Arc    | 67.12ns | 20.00ns | 260.0us | 783.3ns | 0.001% |
| Strong | 616.7ns | 30.00ns | 11.91ms | 55.58us | 0.000% |
| Weak   | 432.5ns | 30.00ns | 13.66ms | 49.14us | 0.000% |

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
