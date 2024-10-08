#![doc = include_str!("../README.md")]

use core::slice;
use std::alloc::{alloc_zeroed, Layout};
use std::any::{Any, TypeId};
use std::cell::{Cell, OnceCell, RefCell, UnsafeCell};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet, LinkedList, VecDeque};
use std::fmt::{Debug, Display};
use std::hash::Hash;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::num::{
    NonZeroI128, NonZeroI16, NonZeroI32, NonZeroI64, NonZeroI8, NonZeroIsize, NonZeroU128,
    NonZeroU16, NonZeroU32, NonZeroU64, NonZeroU8, NonZeroUsize,
};
use std::ops::Deref;
use std::rc::Rc;
use std::sync::atomic::{
    self, AtomicBool, AtomicI16, AtomicI32, AtomicI64, AtomicI8, AtomicIsize, AtomicU16, AtomicU32,
    AtomicU64, AtomicU8, AtomicUsize, Ordering,
};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};
use std::{array, thread};

use crossbeam_utils::sync::{Parker, Unparker};
use flume::{Receiver, RecvError, RecvTimeoutError, Sender};
use intentional::{Assert, Cast};
use kempt::map::Field;
use kempt::{Map, Set};
use parking_lot::{Condvar, Mutex, RwLock};
pub use refuse_macros::{MapAs, Trace};

/// Architecture overview of the underlying design of Refuse.
///
/// # Overview
///
/// *Refuse* is an incremental, tracing garbage collector. Incremental garbage
/// collectors can only run when it knows no threads are currently accessing
/// collectable memory. This fits the access pattern of an `RwLock`: the
/// collector can acquire a "write" lock to ensure that all other threads can't
/// read while it is running.
///
/// Originally, Refuse used an `RwLock` and a shared allocation arena. This did
/// not perform well in multi-threaded benchmarks. So the global `RwLock` was
/// replaced with atomic tracking the number of currently acquired
/// [`CollectionGuard`] and whether the collector is currently trying to start
/// collection.
///
/// Each thread allocates its own independent allocation arena and stores a copy
/// of it in thread-local storage. It also registers a copy with the global
/// collector. Refuse's public API ensures that no access is provided to the
/// local thread's data without first having acquired a [`CollectionGuard`].
/// This ensures that the collector can guarantee exclusive access to the
/// underlying data.
///
/// # Allocation Arenas
///
/// Each thread is given its own allocation arena, which is a data structure
/// designed for concurrently reading portions of its data while still being
/// able to perform new allocations from the owning thread.
///
/// At the root of each arena is a map of types to type-erased `Bin<T>`s. A
/// `Bin<T>` is the root of a linked-list of `Slabs<T>`. Each `Slabs<T>`
/// contains a list of `Slab<T>`s and an optional next `Slabs<T>`. Each
/// `Slab<T>` holds 256 `Slot<T>`s. Each slot is a combination of the slot's
/// state, and the slot's data.
///
/// The slot's state is stored in an atomic and keeps track of:
///
/// - A 32-bit generation. When loading a [`Ref`], its generation is validated
///   to ensure it is the same allocation.
/// - Whether the slot is allocated or not
/// - Garbage collector marking bits
///
/// The owning thread or and the collector are the only types that can modify
/// non-atomic data in a `Bin<T>`. Other threads may need to load a reference to
/// a `Ref<T>`'s underlying data while the owning thread is allocating. This is
/// made safe by:
///
/// - Because allocations for a new slab can't be referred to until after the
///   allocating function returns, we can update `Slabs::next` safely while
///   other threads read the data structure.
/// - Each slot's state is controlled with atomic operations. This ensures
///   consistent access for both the reading thread and the allocating thread.
/// - The slot's state is generational, minimizing the chance of an invalid
///   reference being promoted. Even if a "stale" ref contains a reused
///   generation, the load will still point to valid data because of the order
///   of initialization.
///
/// # Collection
///
/// Refuse is a naive, mark-and-sweep collector. Each collection phase is
/// divided into three portions:
///
/// - Exclusive Access Acquisition
/// - Tracing and marking
/// - Sweeping
///
/// Refuse keeps track of two metrics:
///
/// - `average_collection_locking`: The average duration to acquire exclusive
///   access.
/// - `average_collection`: The average duration of a total collection process,
///   including exclusive access acquisition.
///
/// ## Exclusive Access Acquisition
///
/// Refuse's goal is to be able to be used in nearly any application, including
/// games. Games typically do not want to dip below 60 frames-per-second (FPS),
/// which means that if a garbage collection pause is longer than 16ms, it will
/// cause FPS drops.
///
/// Refuse tries to minimize pauses by waiting for exclusive access only for a
/// multiple of `average_collection_locking`. If access isn't acquired by the
/// deadline, collection is rescheduled again in the near future with an
/// increased multiple. If this process fails several times consecutively,
/// garbage collection will be forced by waiting indefinitely.
///
/// Access is controlled by a single [`AtomicUsize`]. A single bit keeps track
/// of whether the collector is trying to collect or not. The remaining bits
/// keep track of how many [`CollectionGuard`]s are acquired and not yielding.
///
/// [`CollectionGuard::acquire()`] checks if the collection bit is set. If it
/// is, it waits until the current collection finishes and checks again. If the
/// bit is not set, the count is atomically incremented.
///
/// When the final [`CollectionGuard`] drops or yields, it notifies the
/// collector thread so that it can begin collecting.
///
/// ## Tracing and marking
///
/// The goal of this phase is to identify all allocations that can currently be
/// reached by any [`Root<T>`]. When a slot is initially allocated, the marking
/// bits are 0. Each time the collector runs, a new non-zero marking bits is
/// selected by incrementing the previous marking bits and skipping 0 on wrap.
///
/// All `Bin<T>`s of all threads are scanned for any `Slot<T>` that is allocated
/// and has a non-zero root count. Each allocated slot is then marked. If the
/// slot didn't already contain the current marking bits, it is [`Trace`]d,
/// which allows any references found to be marked.
///
/// This process continues until all found references are marked and traced.
///
/// ## Sweeping
///
/// The goal of this phase is to free allocations that are no longer reachable.
/// This is done by scanning all `Bin<T>`s of all threads looking for any
/// allocated `Slot<T>`s that do not contain the current marking bits. When
/// found, the slot is deallocated and contained data has its `Drop`
/// implementation invoked.
pub mod architecture {}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Ord, PartialOrd)]
struct CollectorThreadId(u32);

impl CollectorThreadId {
    fn unique() -> Self {
        static NEXT_ID: AtomicU32 = AtomicU32::new(0);
        Self(NEXT_ID.fetch_add(1, Ordering::Release))
    }
}

enum CollectorCommand {
    NewThread(CollectorThreadId, Arc<UnsafeBins>),
    ThreadShutdown(CollectorThreadId),
    Collect(Instant),
    ScheduleCollect,
}

impl CollectorCommand {
    fn send(self) {
        GlobalCollector::get()
            .sender
            .send(self)
            .expect("collector not running");
    }

    fn schedule_collect_if_needed() {
        let collector = GlobalCollector::get();
        if collector
            .info
            .signalled_collector
            .compare_exchange(false, true, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            collector
                .sender
                .send(Self::ScheduleCollect)
                .expect("collector not running");
        }
    }
}

type AllThreadBins = Arc<RwLock<Map<CollectorThreadId, Weak<UnsafeBins>>>>;

struct ThreadBins {
    alive: bool,
    bins: Arc<UnsafeBins>,
}

struct CollectorInfo {
    info: Mutex<CollectorInfoData>,
    collection_sync: Condvar,
    collector_unparker: Unparker,
    signalled_collector: AtomicBool,
    reader_state: ReaderState,
    all_threads: AllThreadBins,
    type_indexes: RwLock<Map<TypeId, TypeIndex>>,
}

impl CollectorInfo {
    fn wait_for_collection(&self, requested_at: Instant) {
        let mut info = self.info.lock();
        while info.last_run < requested_at {
            self.collection_sync.wait(&mut info);
        }
    }
}

struct CollectorInfoData {
    last_run: Instant,
}

struct CollectorThreadChannels {
    tracer: Sender<TraceRequest>,
}

struct TraceRequest {
    thread: CollectorThreadId,
    bins: Arc<dyn AnyBin>,
    mark_one_sender: Arc<Sender<MarkRequest>>,
}

struct MarkRequest {
    thread: CollectorThreadId,
    type_index: TypeIndex,
    slot_generation: u32,
    bin_id: BinId,
    mark_one_sender: Arc<Sender<MarkRequest>>,
}

struct Collector {
    shared: Arc<CollectorInfo>,
    receiver: Receiver<CollectorCommand>,
    thread_bins: Map<CollectorThreadId, ThreadBins>,
    active_threads: usize,
    mark_bits: u8,
    next_gc: Option<Instant>,
    pause_failures: u8,
    average_collection: Duration,
    average_collection_locking: Duration,
}

impl Collector {
    fn new(receiver: Receiver<CollectorCommand>, shared: Arc<CollectorInfo>) -> Self {
        Self {
            shared,
            receiver,
            thread_bins: Map::new(),
            active_threads: 0,
            mark_bits: 0,
            next_gc: None,
            pause_failures: 0,
            average_collection: Duration::from_millis(1),
            average_collection_locking: Duration::from_millis(1),
        }
    }

    fn next_command(&self) -> Result<Option<CollectorCommand>, RecvError> {
        if let Some(next_gc) = self.next_gc {
            match self.receiver.recv_deadline(next_gc) {
                Ok(value) => Ok(Some(value)),
                Err(RecvTimeoutError::Timeout) => Ok(None),
                Err(RecvTimeoutError::Disconnected) => Err(RecvError::Disconnected),
            }
        } else {
            self.receiver.recv().map(Some)
        }
    }

    fn schedule_gc(&mut self, target: Instant) {
        if self.next_gc.map_or(true, |next_gc| target < next_gc) {
            self.next_gc = Some(target);
        }
    }

    fn run(mut self, parker: &Parker) {
        thread::scope(|scope| {
            let (tracer, trace_receiver) = flume::bounded(128);

            let channels = CollectorThreadChannels { tracer };

            let available_parallelism = thread::available_parallelism()
                .ok()
                .map_or(1, NonZeroUsize::get);
            // We always need at least one trace thread
            let trace_threads = 1.max(available_parallelism - 1);
            for _ in 0..trace_threads {
                scope.spawn({
                    let trace_receiver = trace_receiver.clone();
                    move || {
                        while let Ok(request) = trace_receiver.recv() {
                            request
                                .bins
                                .trace(&mut Tracer::new(request.thread, &request.mark_one_sender));
                        }
                    }
                });
            }

            loop {
                let command = match self.next_command() {
                    Ok(Some(command)) => command,
                    Ok(None) => {
                        self.collect_and_notify(&channels, parker);
                        continue;
                    }
                    Err(_) => break,
                };
                match command {
                    CollectorCommand::NewThread(id, bins) => {
                        let new_thread = self
                            .thread_bins
                            .insert(id, ThreadBins { alive: true, bins })
                            .is_none();
                        assert!(new_thread);
                        self.active_threads += 1;
                    }
                    CollectorCommand::ThreadShutdown(id) => {
                        self.thread_bins.get_mut(&id).expect("unknown thread").alive = false;
                        self.active_threads -= 1;

                        // If we have no active threads, we can stop worrying about
                        // garbage collection.
                        if self.active_threads == 0 {
                            self.thread_bins.clear();
                        }
                    }
                    CollectorCommand::Collect(requested_at) => {
                        let info = self.shared.info.lock();
                        if info.last_run < requested_at {
                            drop(info);
                            self.collect_and_notify(&channels, parker);
                        }
                    }
                    CollectorCommand::ScheduleCollect => {
                        self.schedule_gc(Instant::now() + self.average_collection * 5);
                    }
                }
            }

            // Dropping channels ensures the worker threads shut down, allowing
            // full cleanup. Because of thread local storage limitations, this
            // may never execute on some platforms.
            drop(channels);
        });
    }

    fn collect_and_notify(&mut self, channels: &CollectorThreadChannels, parker: &Parker) {
        self.next_gc = None;
        let gc_start = Instant::now();
        let collect_result = self.collect(channels, parker);
        let gc_finish = Instant::now();

        let gc_pause = match collect_result {
            CollectResult::Ok => {
                let elapsed = gc_finish - gc_start;
                // Keep track of the average collection duration as a moving
                // average, weighted towards current average.
                self.average_collection = (elapsed + self.average_collection * 2) / 3;

                if self.thread_bins.is_empty() {
                    None
                } else {
                    Some((self.average_collection * 100).max(Duration::from_millis(100)))
                }
            }
            CollectResult::CouldntRun => {
                self.pause_failures += 1;
                Some(self.average_collection)
            }
        };

        if let Some(pause) = gc_pause {
            self.schedule_gc(gc_finish + pause);
        }
        let mut info = self.shared.info.lock();
        info.last_run = gc_finish;
        drop(info);
        self.shared.reader_state.release_write();
        self.shared.collection_sync.notify_all();
        self.shared
            .signalled_collector
            .store(false, Ordering::Relaxed);
    }

    fn acquire_all_locks<'a>(
        thread_bins: &'a Map<CollectorThreadId, ThreadBins>,
        start: Instant,
        average_collection_locking: Duration,
        pause_failures: u8,
        collector: &CollectorInfo,
        parker: &Parker,
    ) -> Option<Map<CollectorThreadId, &'a mut Bins>> {
        let force_gc = pause_failures >= 2;
        let lock_wait = average_collection_locking * u32::from(pause_failures + 1) * 3;
        let long_lock_deadline = start + lock_wait;

        if !collector.reader_state.write() {
            while collector.reader_state.readers() > 0 {
                if force_gc {
                    parker.park();
                } else {
                    parker.park_deadline(long_lock_deadline);
                    if Instant::now() > long_lock_deadline && collector.reader_state.readers() > 0 {
                        collector.reader_state.release_write();
                        collector.collection_sync.notify_all();
                        return None;
                    }
                }
            }
        }

        Some(
            thread_bins
                .iter()
                // SAFETY: We have acquired all of the locks, so we can now gain
                // exclusive access safely.
                .map(|entry| (*entry.key(), unsafe { entry.value.bins.assume_mut() }))
                .collect(),
        )
    }

    fn collect(&mut self, threads: &CollectorThreadChannels, parker: &Parker) -> CollectResult {
        self.mark_bits = self.mark_bits.wrapping_add(1);
        if self.mark_bits == 0 {
            self.mark_bits = 1;
        }

        let start = Instant::now();
        let Some(mut all_bins) = Self::acquire_all_locks(
            &self.thread_bins,
            start,
            self.average_collection_locking,
            self.pause_failures,
            &self.shared,
            parker,
        ) else {
            self.pause_failures += 1;
            return CollectResult::CouldntRun;
        };

        let locking_time = start.elapsed();
        self.average_collection_locking = (locking_time + self.average_collection_locking * 2) / 3;
        self.pause_failures = 0;

        let (mark_one_sender, mark_ones) = flume::bounded(1024);
        let mark_one_sender = Arc::new(mark_one_sender);

        for (id, bins) in &mut all_bins {
            let by_type = bins.by_type.read();
            for i in 0..by_type.len() {
                threads
                    .tracer
                    .send(TraceRequest {
                        thread: *id,
                        bins: by_type.field(i).expect("length checked").value.clone(),
                        mark_one_sender: mark_one_sender.clone(),
                    })
                    .expect("tracer stopped");
            }
        }

        loop {
            let MarkRequest {
                thread,
                type_index,
                slot_generation,
                bin_id,
                mark_one_sender,
            } = {
                if Arc::strong_count(&mark_one_sender) == 1 {
                    // We are the final source of trace requests, do not block
                    match mark_ones.try_recv().ok() {
                        Some(msg) => msg,
                        None => break,
                    }
                } else {
                    match mark_ones.recv_timeout(Duration::from_micros(1)) {
                        Ok(msg) => msg,
                        Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => continue,
                    }
                }
            };

            let bins = all_bins[&thread]
                .by_type
                .read()
                .get(&type_index)
                .expect("areas are never deallocated")
                .clone();
            if bins.mark_one(self.mark_bits, slot_generation, bin_id) {
                bins.trace_one(
                    slot_generation,
                    bin_id,
                    &mut Tracer::new(thread, &mark_one_sender),
                );
            }
        }

        atomic::fence(Ordering::Acquire);
        let mut threads_to_remove = Vec::new();
        for (thread_id, bins) in all_bins.into_iter().map(Field::into_parts) {
            let mut live_objects = 0_usize;
            for bin in bins.by_type.write().values_mut() {
                live_objects = live_objects.saturating_add(bin.sweep(self.mark_bits));
            }
            if live_objects == 0 {
                threads_to_remove.push(thread_id);
            }
        }

        if !threads_to_remove.is_empty() {
            let mut all_threads = self.shared.all_threads.write();
            for thread_id in threads_to_remove {
                if !self.thread_bins[&thread_id].alive {
                    self.thread_bins.remove(&thread_id);
                    all_threads.remove(&thread_id);
                }
            }
        }

        CollectResult::Ok
    }
}

enum CollectResult {
    Ok,
    CouldntRun,
}

struct GlobalCollector {
    sender: Sender<CollectorCommand>,
    info: Arc<CollectorInfo>,
}

impl GlobalCollector {
    fn get() -> &'static GlobalCollector {
        COLLECTOR.get_or_init(|| {
            let (sender, receiver) = flume::bounded(1024);
            let parker = Parker::new();
            let info = Arc::new(CollectorInfo {
                info: Mutex::new(CollectorInfoData {
                    last_run: Instant::now(),
                }),
                collection_sync: Condvar::new(),
                collector_unparker: parker.unparker().clone(),
                signalled_collector: AtomicBool::new(false),
                reader_state: ReaderState(AtomicUsize::new(0)),
                all_threads: AllThreadBins::default(),
                type_indexes: RwLock::default(),
            });
            thread::Builder::new()
                .name(String::from("collector"))
                .spawn({
                    let info = info.clone();
                    move || Collector::new(receiver, info).run(&parker)
                })
                .expect("error starting collector thread");
            GlobalCollector { sender, info }
        })
    }
}

static COLLECTOR: OnceLock<GlobalCollector> = OnceLock::new();

thread_local! {
    static THREAD_POOL: RefCell<OnceCell<ThreadPool>> = const { RefCell::new(OnceCell::new()) };
}

#[derive(Default)]
struct UnsafeBins(UnsafeCell<ManuallyDrop<Bins>>);

impl UnsafeBins {
    /// # Safety
    ///
    /// The caller of this function must ensure that no exclusive references
    /// exist to this data.
    unsafe fn assume_readable(&self) -> &Bins {
        &*self.0.get()
    }

    /// # Safety
    ///
    /// The caller of this function must ensure that no other references exist
    /// to this data. For the design of this collector, this function should
    /// only be called by the garbage collector thread after acquiring the lock
    /// that contains this structure.
    #[allow(clippy::mut_from_ref)]
    unsafe fn assume_mut(&self) -> &mut Bins {
        &mut *self.0.get()
    }
}

// SAFETY: Auto-implementation is prevented by UnsafeCell. The contained type
// would be `Send`, and this crate takes care to ensure correct thread-safe
// access to the contents of the UnsafeCell.
unsafe impl Send for UnsafeBins {}
// SAFETY: Auto-implementation is prevented by UnsafeCell. The contained type
// would be `Send`, and this crate takes care to ensure correct thread-safe
// access to the contents of the UnsafeCell.
unsafe impl Sync for UnsafeBins {}

impl Drop for UnsafeBins {
    fn drop(&mut self) {
        // SAFETY: We never leave the contents of the unsafe cell in an invalid
        // state, making it safe to invoke the drop function without any extra
        // checks.
        unsafe {
            ManuallyDrop::drop(self.0.get_mut());
        }
    }
}

#[derive(Default)]
struct ThreadPool {
    pool: LocalPool,
    guard_depth: Rc<Cell<usize>>,
}

impl ThreadPool {
    fn map_current<R>(map: impl FnOnce(&Self) -> R) -> R {
        THREAD_POOL.with_borrow(|tp| map(tp.get_or_init(ThreadPool::default)))
    }

    fn get() -> Self {
        Self::map_current(Self::clone)
    }

    fn current_depth() -> usize {
        Self::map_current(|tp| tp.guard_depth.get())
    }

    fn push_thread_guard() -> usize {
        Self::map_current(Self::push_guard)
    }

    fn release_thread_guard() {
        Self::map_current(Self::release_guard);
    }

    fn push_guard(&self) -> usize {
        let depth = self.guard_depth.get();
        self.guard_depth.set(depth + 1);
        depth
    }

    fn release_guard(&self) {
        self.guard_depth.set(self.guard_depth.get() - 1);
    }
}

impl Clone for ThreadPool {
    fn clone(&self) -> Self {
        Self {
            pool: LocalPool {
                bins: self.pool.bins.clone(),
                thread_id: self.pool.thread_id,
                all_threads: self.pool.all_threads.clone(),
            },
            guard_depth: self.guard_depth.clone(),
        }
    }
}

/// A pool of garbage collected values.
///
/// Values from any pool can be read using any [`CollectionGuard`]. Using
/// independent pools for specific types of data that are meant to be shared
/// across many threads might be beneficial. However, an individual local pool
/// will not be fully deallocated until all values allocated have been
/// collected. Because of this, it may make sense to store some types in their
/// own pool, ensuring their collection is independent of how the values are
/// used amongst other threads.
pub struct LocalPool {
    bins: Arc<UnsafeBins>,
    thread_id: CollectorThreadId,
    all_threads: AllThreadBins,
}

impl Default for LocalPool {
    fn default() -> Self {
        let all_threads = GlobalCollector::get().info.all_threads.clone();
        loop {
            let thread_id = CollectorThreadId::unique();
            let mut threads = all_threads.write();
            if let kempt::map::Entry::Vacant(entry) = threads.entry(thread_id) {
                let bins = Arc::<UnsafeBins>::default();
                CollectorCommand::NewThread(thread_id, bins.clone()).send();
                entry.insert(Arc::downgrade(&bins));
                drop(threads);
                return LocalPool {
                    bins,
                    thread_id,
                    all_threads,
                };
            }
        }
    }
}

impl LocalPool {
    /// Acquires a collection guard for this pool.
    #[must_use]
    pub fn enter(&self) -> CollectionGuard<'_> {
        let depth = ThreadPool::push_thread_guard();
        let collector = if depth == 0 {
            CollectorReadGuard::acquire()
        } else {
            CollectorReadGuard::acquire_recursive()
        };
        CollectionGuard {
            collector,
            thread: Guarded::Local(self),
        }
    }
}

impl Drop for LocalPool {
    fn drop(&mut self) {
        // If this reference and the one in the collector are the last
        // references, send a shutdown notice.
        if Arc::strong_count(&self.bins) == 2 {
            CollectorCommand::ThreadShutdown(self.thread_id).send();
        }
    }
}

struct ReaderState(AtomicUsize);

impl ReaderState {
    const COLLECTING_BIT: usize = 1 << (usize::BITS - 1);

    fn read(&self) -> bool {
        self.0
            .fetch_update(Ordering::Release, Ordering::Acquire, |state| {
                (state & Self::COLLECTING_BIT == 0).then_some(state + 1)
            })
            .is_ok()
    }

    fn read_recursive(&self) {
        self.0.fetch_add(1, Ordering::Acquire);
    }

    fn release_read(&self) -> bool {
        self.0.fetch_sub(1, Ordering::Acquire) == (Self::COLLECTING_BIT | 1)
    }

    fn write(&self) -> bool {
        let mut current_readers = 0;
        self.0
            .fetch_update(Ordering::Release, Ordering::Acquire, |state| {
                current_readers = state;
                Some(state | Self::COLLECTING_BIT)
            })
            .expect("error updating reader_state");
        current_readers == 0
    }

    fn release_write(&self) {
        self.0
            .fetch_update(Ordering::Release, Ordering::Acquire, |state| {
                Some(state & !Self::COLLECTING_BIT)
            })
            .expect("error updating reader_state");
    }

    fn readers(&self) -> usize {
        self.0.load(Ordering::Acquire) & !Self::COLLECTING_BIT
    }

    fn release_read_if_collecting(&self) -> ReleaseReadResult {
        match self
            .0
            .fetch_update(Ordering::Release, Ordering::Acquire, |state| {
                (state & Self::COLLECTING_BIT != 0).then_some(state - 1)
            }) {
            Ok(previous) if previous == (Self::COLLECTING_BIT | 1) => {
                ReleaseReadResult::CollectAndUnpark
            }
            Ok(_) => ReleaseReadResult::Collect,
            Err(_) => ReleaseReadResult::Noop,
        }
    }
}

enum ReleaseReadResult {
    CollectAndUnpark,
    Collect,
    Noop,
}

struct CollectorReadGuard {
    global: &'static CollectorInfo,
}

impl CollectorReadGuard {
    fn acquire() -> Self {
        let mut this = Self {
            global: &GlobalCollector::get().info,
        };
        this.acquire_reader();
        this
    }

    fn try_acquire() -> Option<Self> {
        let global = &GlobalCollector::get().info;
        global.reader_state.read().then_some(Self { global })
    }

    fn acquire_recursive() -> Self {
        let this = Self {
            global: &GlobalCollector::get().info,
        };
        this.global.reader_state.read_recursive();
        this
    }

    fn acquire_reader(&mut self) {
        if !self.global.reader_state.read() {
            let mut info_data = self.global.info.lock();
            while !self.global.reader_state.read() {
                self.global.collection_sync.wait(&mut info_data);
            }
        }
    }

    fn read_recursive(&self) -> Self {
        self.global.reader_state.read_recursive();
        Self {
            global: self.global,
        }
    }

    fn release_reader(&mut self) {
        if self.global.reader_state.release_read() {
            self.global.collector_unparker.unpark();
        }
    }

    fn release_reader_if_collecting(&mut self) -> bool {
        match self.global.reader_state.release_read_if_collecting() {
            ReleaseReadResult::CollectAndUnpark => {
                self.global.collector_unparker.unpark();
                true
            }
            ReleaseReadResult::Collect => true,
            ReleaseReadResult::Noop => false,
        }
    }
}

impl Drop for CollectorReadGuard {
    fn drop(&mut self) {
        self.release_reader();
    }
}

enum Guarded<'a> {
    Local(&'a LocalPool),
    Thread(ThreadPool),
}

impl Deref for Guarded<'_> {
    type Target = LocalPool;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Local(value) => value,
            Self::Thread(value) => &value.pool,
        }
    }
}

/// A guard that prevents garbage collection while held.
///
/// To perform garbage collection, all threads must be paused to be traced. A
/// [`CollectionGuard`] allows the ability to read garbage-collectable data by
/// ensuring the garbage collector can't run while it exists.
///
/// To ensure the garbage collector can run without long pauses, either:
///
/// - Acquire [`CollectionGuard`]s for short periods of time, dropping the
///   guards when not needed.
/// - Call [`CollectionGuard::yield_to_collector()`] at a regular basis. This
///   function is very cheap to invoke if the collector is not trying to acquire
///   the lock.
///
/// This type should not be held across potentially blocking operations such as
/// IO, reading from a channel, or any other operation that may pause the
/// current thread. [`CollectionGuard::while_unlocked()`] can be used to
/// temporarily release a guard during a long operation.
pub struct CollectionGuard<'a> {
    collector: CollectorReadGuard,
    thread: Guarded<'a>,
}

impl CollectionGuard<'static> {
    /// Acquires a lock that prevents the garbage collector from running.
    ///
    /// This guard is used to provide read-only access to garbage collected
    /// allocations.
    ///
    /// It is safe to acquire multiple guards on the same thread. The collector
    /// will only be able to run when all guards have been released.
    #[must_use]
    pub fn acquire() -> Self {
        let thread = ThreadPool::get();

        let depth = thread.push_guard();

        let collector = if depth == 0 {
            CollectorReadGuard::acquire()
        } else {
            CollectorReadGuard::acquire_recursive()
        };
        Self {
            collector,
            thread: Guarded::Thread(thread),
        }
    }

    /// Tries to acquire a lock that prevents the garbage collector from
    /// running.
    ///
    /// The function will return None if the collector is attempting to run
    /// garbage collection, and this thread is not already guarded.
    ///
    /// This guard is used to provide read-only access to garbage collected
    /// allocations.
    ///
    /// It is safe to acquire multiple guards on the same thread. The collector
    /// will only be able to run when all guards have been released.
    #[must_use]
    pub fn try_acquire() -> Option<Self> {
        let thread = ThreadPool::get();

        let depth = thread.push_guard();

        let collector = if depth == 0 {
            CollectorReadGuard::try_acquire()?
        } else {
            CollectorReadGuard::acquire_recursive()
        };
        Some(Self {
            collector,
            thread: Guarded::Thread(thread),
        })
    }
}

impl CollectionGuard<'_> {
    #[allow(clippy::unused_self)]
    fn bins_for<'a>(&'a self, bins: &'a UnsafeBins) -> &'a Bins {
        // SAFETY: We have read access ensuring the collector can't call
        // `assume_mut()` while `self` exists. We can use this knowledge to
        // safely create a reference tied to `self`'s lifetime.
        unsafe { bins.assume_readable() }
    }

    fn bins(&self) -> &Bins {
        self.bins_for(&self.thread.bins)
    }

    /// Returns a guard that allocates from `pool`.
    #[must_use]
    pub fn allocating_in<'a>(&self, pool: &'a LocalPool) -> CollectionGuard<'a> {
        ThreadPool::push_thread_guard();
        CollectionGuard {
            collector: self.collector.read_recursive(),
            thread: Guarded::Local(pool),
        }
    }

    /// Manually invokes the garbage collector.
    ///
    /// This method temporarily releases this guard's lock and waits for a
    /// garbage collection to run. If a garbage collection is already in
    /// progress, this function will return when the in-progress collection
    /// completes. Otherwise, the collector is started and this function waits
    /// until the collection finishes.
    ///
    /// Finally, the guard is reacquired before returning.
    ///
    /// # Panics
    ///
    /// This function will panic if any other [`CollectionGuard`]s are held by
    /// the current thread when invoked.
    pub fn collect(&mut self) {
        self.try_collect().unwrap();
    }

    /// Manually invokes the garbage collector.
    ///
    /// This method temporarily releases this guard's lock and waits for a
    /// garbage collection to run. If a garbage collection is already in
    /// progress, this function will return when the in-progress collection
    /// completes. Otherwise, the collector is started and this function waits
    /// until the collection finishes.
    ///
    /// Finally, the guard is reacquired before returning.
    ///
    /// # Errors
    ///
    /// If another [`CollectionGuard`] is held by the current thread,
    /// [`WouldDeadlock`] will be returned and `unlocked` will not be invoked.
    pub fn try_collect(&mut self) -> Result<(), WouldDeadlock> {
        self.try_while_unlocked(collect_unchecked)
    }

    /// Yield to the garbage collector, if needed.
    ///
    /// This function will not yield unless the garbage collector is trying to
    /// acquire this thread's lock. Because of this, it is a fairly efficient
    /// function to invoke. To minimize collection pauses, long-held guards
    /// should call this function regularly.
    ///
    /// If any other guards are currently held by this thread, this function
    /// does nothing.
    #[allow(clippy::redundant_closure_for_method_calls)] // produces compiler error
    pub fn yield_to_collector(&mut self) {
        self.coordinated_yield(|yielder| yielder.wait());
    }

    /// Perform a coordinated yield to the collector, if needed.
    ///
    /// This function is useful if the code yielding has held locks that the
    /// garbage collector might need during the tracing process. Instead of
    /// always unlocking the locks before calling
    /// [`Self::yield_to_collector()`], this function can be used to only
    /// release the local locks when yielding.
    ///
    /// If this function detects that it should yield to the collector,
    /// `yielder` will be invoked. The function can do whatever else is needed
    /// before waiting for the collector to finish, and then invoke
    /// [`Yielder::wait()`].
    ///
    /// If this function does not yield, `yielder` is not invoked.
    ///
    /// This function will not yield unless the garbage collector is trying to
    /// acquire this thread's lock. Because of this, it is a fairly efficient
    /// function to invoke. To minimize collection pauses, long-held guards
    /// should call this function regularly.
    ///
    /// If any other guards are currently held by this thread, this function
    /// does nothing.
    pub fn coordinated_yield(
        &mut self,
        yielder: impl FnOnce(Yielder<'_>) -> YieldComplete,
    ) -> bool {
        // We only need to attempt yielding if we are the outermost guard.
        if ThreadPool::current_depth() == 1 && self.collector.release_reader_if_collecting() {
            let _complete: YieldComplete = yielder(Yielder(&mut self.collector));
            true
        } else {
            false
        }
    }

    /// Executes `unlocked` while this guard is temporarily released.
    ///
    /// # Panics
    ///
    /// This function will panic if any other [`CollectionGuard`]s are held by
    /// the current thread when invoked.
    pub fn while_unlocked<R>(&mut self, unlocked: impl FnOnce() -> R) -> R {
        self.try_while_unlocked(unlocked).unwrap()
    }

    /// Executes `unlocked` while this guard is temporarily released.
    ///
    /// # Errors
    ///
    /// If another [`CollectionGuard`] is held by the current thread,
    /// [`WouldDeadlock`] will be returned and `unlocked` will not be invoked.
    pub fn try_while_unlocked<R>(
        &mut self,
        unlocked: impl FnOnce() -> R,
    ) -> Result<R, WouldDeadlock> {
        if ThreadPool::current_depth() == 1 {
            self.collector.release_reader();
            let result = unlocked();
            self.collector.acquire_reader();

            Ok(result)
        } else {
            Err(WouldDeadlock)
        }
    }

    fn adopt<T: Collectable>(&self, value: Rooted<T>) -> (TypeIndex, u32, BinId) {
        Bins::adopt(value, self)
    }
}

impl Drop for CollectionGuard<'_> {
    fn drop(&mut self) {
        ThreadPool::release_thread_guard();
    }
}

impl<'a> AsRef<CollectionGuard<'a>> for CollectionGuard<'a> {
    fn as_ref(&self) -> &CollectionGuard<'a> {
        self
    }
}

impl<'a> AsMut<CollectionGuard<'a>> for CollectionGuard<'a> {
    fn as_mut(&mut self) -> &mut CollectionGuard<'a> {
        self
    }
}

/// A pending yield to the garbage collector.
pub struct Yielder<'a>(&'a mut CollectorReadGuard);

impl Yielder<'_> {
    /// Waits for the garbage collector to finish the current collection.
    #[must_use]
    pub fn wait(self) -> YieldComplete {
        self.0.acquire_reader();
        YieldComplete { _priv: PhantomData }
    }
}

/// A marker indicating that a [coordinated
/// yield](CollectionGuard::coordinated_yield) has completed.
pub struct YieldComplete {
    _priv: PhantomData<()>,
}

/// A type that can be garbage collected.
///
/// A type needs to implement both [`Trace`] and [`MapAs`] to be collectable.
///
/// If a type can't contain any [`Ref<T>`]s and no mapping functionality is
/// desired, the [`SimpleType`] trait can be implemented instead of [`Trace`]
/// and [`MapAs`] to enable collection.
///
/// If a type can't contain any [`Ref<T>`]s, [`ContainsNoRefs`] can be
/// implemented instead of [`Trace`].
///
/// If no mapping functionality is desired, [`NoMapping`] can be implemented
/// instead of [`MapAs`].
pub trait Collectable: Trace + MapAs + Send + Sync + 'static {}

impl<T> Collectable for T where T: Trace + MapAs + Send + Sync + 'static {}

/// A type that can find and mark any references it has.
pub trait Trace: Send + Sync + 'static {
    /// If true, this type may contain references and should have its `trace()`
    /// function invoked during the collector's "mark" phase.
    const MAY_CONTAIN_REFERENCES: bool;

    /// Traces all refrences that this value references.
    ///
    /// This function should invoke [`Tracer::mark()`] for each [`Ref<T>`] it
    /// contains. Failing to do so will allow the garbage collector to free the
    /// data, preventing the ability to [`load()`](Ref::load) the data in the
    /// future.
    fn trace(&self, tracer: &mut Tracer);
}

/// A mapping from one type to another.
///
/// This trait is used by [`AnyRef::load_mapped()`] to enable type-erased
/// loading of a secondary type.
///
/// If no mapping is desired, implement [`NoMapping`] instead.
pub trait MapAs: Send + Sync + 'static {
    /// The target type of the mapping.
    type Target: ?Sized + 'static;

    /// Maps `self` to target type.
    fn map_as(&self) -> &Self::Target;
}

/// A type that can be garbage collected that cannot contain any [`Ref<T>`]s.
///
/// Types that implement this trait automatically implement [`Collectable`].
/// This trait reduces the boilerplate for implementing [`Collectable`] for
/// self-contained types.
pub trait ContainsNoRefs: Send + Sync + 'static {}

impl<T> Trace for T
where
    T: ContainsNoRefs,
{
    const MAY_CONTAIN_REFERENCES: bool = false;

    fn trace(&self, _tracer: &mut Tracer) {}
}

/// A type that implements [`MapAs`] with an empty implementation.
pub trait NoMapping: Send + Sync + 'static {}

impl<T> MapAs for T
where
    T: NoMapping,
{
    type Target = ();

    fn map_as(&self) -> &Self::Target {
        &()
    }
}

/// A type that can contain no [`Ref<T>`]s and has an empty [`MapAs`]
/// implementation.
///
/// Implementing this trait for a type automatically implements [`NoMapping`]
/// and [`ContainsNoRefs`], which makes the type [`Collectable`].
pub trait SimpleType: Send + Sync + 'static {}

impl<T> NoMapping for T where T: SimpleType {}
impl<T> ContainsNoRefs for T where T: SimpleType {}

macro_rules! impl_simple_type {
    ($($ty:ty),+ ,) => {
        $(impl SimpleType for $ty {})+
    }
}

impl_simple_type!(
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    AtomicU8,
    AtomicU16,
    AtomicU32,
    AtomicU64,
    AtomicUsize,
    AtomicI8,
    AtomicI16,
    AtomicI32,
    AtomicI64,
    AtomicIsize,
    NonZeroU8,
    NonZeroU16,
    NonZeroU32,
    NonZeroU64,
    NonZeroU128,
    NonZeroUsize,
    NonZeroI8,
    NonZeroI16,
    NonZeroI32,
    NonZeroI64,
    NonZeroI128,
    NonZeroIsize,
    String,
);

impl<T> Trace for Vec<T>
where
    T: Trace,
{
    const MAY_CONTAIN_REFERENCES: bool = T::MAY_CONTAIN_REFERENCES;

    fn trace(&self, tracer: &mut Tracer) {
        for item in self {
            item.trace(tracer);
        }
    }
}

impl<T> NoMapping for Vec<T> where T: Send + Sync + 'static {}

impl<T> Trace for VecDeque<T>
where
    T: Trace,
{
    const MAY_CONTAIN_REFERENCES: bool = T::MAY_CONTAIN_REFERENCES;

    fn trace(&self, tracer: &mut Tracer) {
        for item in self {
            item.trace(tracer);
        }
    }
}

impl<T> NoMapping for VecDeque<T> where T: Send + Sync + 'static {}

impl<T> Trace for BinaryHeap<T>
where
    T: Trace,
{
    const MAY_CONTAIN_REFERENCES: bool = T::MAY_CONTAIN_REFERENCES;

    fn trace(&self, tracer: &mut Tracer) {
        for item in self {
            item.trace(tracer);
        }
    }
}

impl<T> NoMapping for BinaryHeap<T> where T: Send + Sync + 'static {}

impl<T> Trace for LinkedList<T>
where
    T: Trace,
{
    const MAY_CONTAIN_REFERENCES: bool = T::MAY_CONTAIN_REFERENCES;

    fn trace(&self, tracer: &mut Tracer) {
        for item in self {
            item.trace(tracer);
        }
    }
}

impl<T> NoMapping for LinkedList<T> where T: Send + Sync + 'static {}

impl<K, V, S> Trace for HashMap<K, V, S>
where
    K: Trace,
    V: Trace,
    S: Send + Sync + 'static,
{
    const MAY_CONTAIN_REFERENCES: bool = K::MAY_CONTAIN_REFERENCES || V::MAY_CONTAIN_REFERENCES;

    fn trace(&self, tracer: &mut Tracer) {
        for (k, v) in self {
            k.trace(tracer);
            v.trace(tracer);
        }
    }
}

impl<K, V, S> NoMapping for HashMap<K, V, S>
where
    K: Send + Sync + 'static,
    V: Send + Sync + 'static,
    S: Send + Sync + 'static,
{
}

impl<K, S> Trace for HashSet<K, S>
where
    K: Trace,
    S: Send + Sync + 'static,
{
    const MAY_CONTAIN_REFERENCES: bool = K::MAY_CONTAIN_REFERENCES;

    fn trace(&self, tracer: &mut Tracer) {
        for k in self {
            k.trace(tracer);
        }
    }
}

impl<K, S> NoMapping for HashSet<K, S>
where
    K: Send + Sync + 'static,
    S: Send + Sync + 'static,
{
}

impl<K, V> Trace for BTreeMap<K, V>
where
    K: Trace,
    V: Trace,
{
    const MAY_CONTAIN_REFERENCES: bool = K::MAY_CONTAIN_REFERENCES || V::MAY_CONTAIN_REFERENCES;

    fn trace(&self, tracer: &mut Tracer) {
        for (k, v) in self {
            k.trace(tracer);
            v.trace(tracer);
        }
    }
}

impl<K, V> NoMapping for BTreeMap<K, V>
where
    K: Send + Sync + 'static,
    V: Send + Sync + 'static,
{
}

impl<K> Trace for BTreeSet<K>
where
    K: Trace,
{
    const MAY_CONTAIN_REFERENCES: bool = K::MAY_CONTAIN_REFERENCES;

    fn trace(&self, tracer: &mut Tracer) {
        for k in self {
            k.trace(tracer);
        }
    }
}

impl<K> NoMapping for BTreeSet<K> where K: Send + Sync + 'static {}

impl<K, V> Trace for Map<K, V>
where
    K: Trace + kempt::Sort,
    V: Trace,
{
    const MAY_CONTAIN_REFERENCES: bool = K::MAY_CONTAIN_REFERENCES || V::MAY_CONTAIN_REFERENCES;

    fn trace(&self, tracer: &mut Tracer) {
        for field in self {
            field.key().trace(tracer);
            field.value.trace(tracer);
        }
    }
}

impl<K, V> NoMapping for Map<K, V>
where
    K: kempt::Sort + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
}

impl<K> Trace for Set<K>
where
    K: Trace + kempt::Sort,
{
    const MAY_CONTAIN_REFERENCES: bool = K::MAY_CONTAIN_REFERENCES;

    fn trace(&self, tracer: &mut Tracer) {
        for k in self {
            k.trace(tracer);
        }
    }
}

impl<K> NoMapping for Set<K> where K: kempt::Sort + Send + Sync + 'static {}

impl<T, const N: usize> Trace for [T; N]
where
    T: Trace,
{
    const MAY_CONTAIN_REFERENCES: bool = T::MAY_CONTAIN_REFERENCES;

    fn trace(&self, tracer: &mut Tracer) {
        for item in self {
            item.trace(tracer);
        }
    }
}
impl<T, const N: usize> NoMapping for [T; N] where T: Send + Sync + 'static {}

impl<T> Trace for Root<T>
where
    T: Collectable,
{
    const MAY_CONTAIN_REFERENCES: bool = false;

    fn trace(&self, _tracer: &mut Tracer) {
        // Root<T> is already a root, thus calling trace on a Root<T> has no
        // effect.
    }
}

impl<T> Trace for Ref<T>
where
    T: Collectable,
{
    const MAY_CONTAIN_REFERENCES: bool = true;

    fn trace(&self, tracer: &mut Tracer) {
        tracer.mark(*self);
    }
}

impl<T> AsRef<Ref<T>> for Ref<T> {
    fn as_ref(&self) -> &Ref<T> {
        self
    }
}

/// A tracer for the garbage collector.
///
/// This type allows [`Collectable`] values to [`mark()`](Self::mark) any
/// [`Ref<T>`]s they contain.
pub struct Tracer<'a> {
    tracing_thread: CollectorThreadId,
    mark_one_sender: &'a Arc<Sender<MarkRequest>>,
}

impl<'a> Tracer<'a> {
    fn new(thread: CollectorThreadId, mark_one_sender: &'a Arc<Sender<MarkRequest>>) -> Self {
        Self {
            tracing_thread: thread,
            mark_one_sender,
        }
    }

    /// Marks `collectable` as being referenced, ensuring it is not garbage
    /// collected.
    pub fn mark(&mut self, collectable: impl Into<AnyRef>) {
        let collectable = collectable.into();
        self.mark_one_sender
            .send(MarkRequest {
                thread: collectable.creating_thread,
                type_index: collectable.type_index,
                slot_generation: collectable.slot_generation,
                bin_id: collectable.bin_id,
                mark_one_sender: self.mark_one_sender.clone(),
            })
            .assert("marker thread not running");
    }
}

#[std::prelude::v1::test]
fn size_of_types() {
    assert_eq!(std::mem::size_of::<Root<u32>>(), 24);
    assert_eq!(std::mem::size_of::<Ref<u32>>(), 16);
    assert_eq!(std::mem::size_of::<AnyRef>(), 16);
}

/// A root reference to a `T` that has been allocated in the garbage collector.
///
/// This type behaves very similarly to [`Arc<T>`]. It implements `Deref<Target
/// = T>`, and it is also cheap-to-clone, utilizing atomic reference counting to
/// track the number of root references currently exist to the underlying value.
///
/// While any root references exist for a given allocation, the garbage
/// collector will not collect the allocation.
pub struct Root<T>
where
    T: Collectable,
{
    data: *const Rooted<T>,
    reference: Ref<T>,
}

impl<T> Root<T>
where
    T: Collectable,
{
    fn from_parts(
        type_index: TypeIndex,
        slot_generation: u32,
        bin_id: BinId,
        guard: &CollectionGuard<'_>,
    ) -> Self {
        // SAFETY: The guard is always present except during allocation which
        // never invokes this function. Since `bin_id` was just allocated, we
        // also can assume that it is allocated.
        let data = unsafe { guard.bins().allocated_slot_pointer::<T>(type_index, bin_id) };
        Self {
            data,
            reference: Ref {
                any: AnyRef {
                    type_index,
                    creating_thread: guard.thread.thread_id,
                    slot_generation,
                    bin_id,
                },

                _t: PhantomData,
            },
        }
    }

    /// Stores `value` in the garbage collector, returning a root reference to
    /// the data.
    pub fn new<'a>(value: T, guard: &impl AsRef<CollectionGuard<'a>>) -> Self {
        let guard = guard.as_ref();
        let (type_index, gen, bin) = guard.adopt(Rooted::root(value));
        Self::from_parts(type_index, gen, bin, guard)
    }

    /// Try to convert a typeless root reference into a `Root<T>`.
    ///
    /// # Errors
    ///
    /// Returns `Err(root)` if `root` does not contain a `T`.
    pub fn try_from_any<'a>(
        root: AnyRoot,
        guard: &impl AsRef<CollectionGuard<'a>>,
    ) -> Result<Self, AnyRoot> {
        if TypeIndex::of::<T>() == root.any.type_index {
            let slot = root.any.load_slot(guard.as_ref()).assert("root missing");
            Ok(Self {
                data: slot,
                reference: root.any.downcast_ref(),
            })
        } else {
            Err(root)
        }
    }

    /// Returns the current number of root references to this value, including
    /// `self`.
    #[must_use]
    pub fn root_count(&self) -> u64 {
        self.as_rooted().roots.load(Ordering::Acquire)
    }

    /// Returns a "weak" reference to this root.
    #[must_use]
    pub const fn downgrade(&self) -> Ref<T> {
        self.reference
    }

    /// Returns an untyped "weak" reference erased to this root.
    #[must_use]
    pub const fn downgrade_any(&self) -> AnyRef {
        self.reference.as_any()
    }

    /// Returns an untyped root reference.
    #[must_use]
    pub fn to_any_root(&self) -> AnyRoot {
        let roots = &self.as_rooted().roots;
        roots.fetch_add(1, Ordering::Acquire);

        AnyRoot {
            rooted: self.data.cast(),
            roots,
            any: self.reference.as_any(),
        }
    }

    /// Returns this root as an untyped root.
    #[must_use]
    pub fn into_any_root(self) -> AnyRoot {
        // We transfer ownership of this reference to the AnyRoot, so we want to
        // avoid calling drop on `self`.
        let this = ManuallyDrop::new(self);
        AnyRoot {
            rooted: this.data.cast(),
            roots: &this.as_rooted().roots,
            any: this.reference.as_any(),
        }
    }

    fn as_rooted(&self) -> &Rooted<T> {
        // SAFETY: The garbage collector will not collect data while we have a
        // non-zero root count. The returned lifetime of the data is tied to
        // `self`, which ensures the returned lifetime is valid only for as long
        // as this `Root<T>` is alive. This ensures at least one root will
        // remain in existence, preventing the count from reaching 0.
        unsafe { &(*self.data) }
    }

    /// Returns true if these two references point to the same underlying
    /// allocation.
    #[must_use]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        this.reference == other.reference
    }
}

impl<T> Debug for Root<T>
where
    T: Collectable + Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&**self, f)
    }
}

impl<T> Clone for Root<T>
where
    T: Collectable,
{
    fn clone(&self) -> Self {
        self.as_rooted().roots.fetch_add(1, Ordering::Acquire);
        Self {
            data: self.data,
            reference: self.reference,
        }
    }
}

impl<T> AsRef<Ref<T>> for Root<T>
where
    T: Collectable,
{
    fn as_ref(&self) -> &Ref<T> {
        &self.reference
    }
}

// SAFETY: Root<T>'s usage of a pointer prevents auto implementation.
// `Collectable` requires `Send`, and `Root<T>` ensures proper Send + Sync
// behavior in its memory accesses.
unsafe impl<T> Send for Root<T> where T: Collectable {}
// SAFETY: Root<T>'s usage of a pointer prevents auto implementation.
// `Collectable` requires `Send`, and `Root<T>` ensures proper Send + Sync
// behavior in its memory accesses.
unsafe impl<T> Sync for Root<T> where T: Collectable {}

impl<T> Deref for Root<T>
where
    T: Collectable,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.as_rooted().value
    }
}

impl<T> Drop for Root<T>
where
    T: Collectable,
{
    fn drop(&mut self) {
        if self.as_rooted().roots.fetch_sub(1, Ordering::Acquire) == 1 {
            CollectorCommand::schedule_collect_if_needed();
        }
    }
}

impl<T> Eq for Root<T> where T: Collectable + Eq {}

impl<T> PartialEq for Root<T>
where
    T: Collectable + PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.reference == other.reference || **self == **other
    }
}

impl<T> PartialEq<Ref<T>> for Root<T>
where
    T: Collectable,
{
    fn eq(&self, other: &Ref<T>) -> bool {
        self.reference == *other
    }
}

impl<T> PartialEq<&'_ Ref<T>> for Root<T>
where
    T: Collectable,
{
    fn eq(&self, other: &&'_ Ref<T>) -> bool {
        self == *other
    }
}

impl<T> PartialEq<AnyRef> for Root<T>
where
    T: Collectable,
{
    fn eq(&self, other: &AnyRef) -> bool {
        self.reference == *other
    }
}

impl<T> PartialEq<&'_ AnyRef> for Root<T>
where
    T: Collectable,
{
    fn eq(&self, other: &&'_ AnyRef) -> bool {
        self == *other
    }
}

impl<T> Hash for Root<T>
where
    T: Collectable + Hash,
{
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

impl<T> Ord for Root<T>
where
    T: Collectable + Ord,
{
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (**self).cmp(&**other)
    }
}

impl<T> PartialOrd for Root<T>
where
    T: Collectable + PartialOrd,
{
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        (**self).partial_cmp(&**other)
    }
}

/// A reference to data stored in a garbage collector.
///
/// Unlike a [`Root<T>`], this type is not guaranteed to have access to its
/// underlying data. If no [`Collectable`] reachable via all active [`Root`]s
/// marks this allocation, it will be collected.
///
/// Because of this, direct access to the data is not provided. To obtain a
/// reference, call [`Ref::load()`].
///
/// # Loading a reference
///
/// [`Ref::load()`] is used to provide a reference to data stored in the garbage
/// collector.
///
/// ```rust
/// use refuse::{CollectionGuard, Ref};
///
/// let guard = CollectionGuard::acquire();
/// let data = Ref::new(42, &guard);
///
/// assert_eq!(data.load(&guard), Some(&42));
/// ```
///
/// References returned from [`Ref::load()`] are tied to the lifetime of the
/// guard. This ensures that a reference to data can only be held between
/// moments when the garbage collector can be run. For example these usages are
/// prevented by the compiler:
///
/// ```rust,compile_fail
/// # use refuse::{CollectionGuard, Ref};
/// let guard = CollectionGuard::acquire();
/// let data = Ref::new(42, &guard);
/// let reference = data.load(&guard).unwrap();
///
/// drop(guard);
///
/// // error[E0505]: cannot move out of `guard` because it is borrowed
/// assert_eq!(reference, &42);
/// ```
///
/// ```rust,compile_fail
/// # use refuse::{CollectionGuard, Ref};
/// let mut guard = CollectionGuard::acquire();
/// let data = Ref::new(42, &guard);
/// let reference = data.load(&guard).unwrap();
///
/// guard.yield_to_collector();
///
/// // error[E0502]: cannot borrow `guard` as mutable because it is also borrowed as immutable
/// assert_eq!(reference, &42);
/// ```
pub struct Ref<T> {
    any: AnyRef,
    _t: PhantomData<fn(&T)>,
}

impl<T> Ref<T>
where
    T: Collectable,
{
    /// Stores `value` in the garbage collector, returning a "weak" reference to
    /// it.
    pub fn new<'a>(value: T, guard: &impl AsRef<CollectionGuard<'a>>) -> Self {
        let guard = guard.as_ref();
        let (type_index, slot_generation, bin_id) = guard.adopt(Rooted::reference(value));

        Self {
            any: AnyRef {
                type_index,
                creating_thread: guard.thread.thread_id,
                slot_generation,
                bin_id,
            },
            _t: PhantomData,
        }
    }

    /// Returns this reference as an untyped reference.
    #[must_use]
    pub const fn as_any(self) -> AnyRef {
        self.any
    }

    fn load_slot<'guard>(&self, guard: &'guard CollectionGuard<'_>) -> Option<&'guard Rooted<T>> {
        self.any.load_slot(guard)
    }

    /// Loads a reference to the underlying data. Returns `None` if the data has
    /// been collected and is no longer available.
    #[must_use]
    pub fn load<'guard>(&self, guard: &'guard CollectionGuard<'_>) -> Option<&'guard T> {
        self.load_slot(guard).map(|allocated| &allocated.value)
    }

    /// Loads a root reference to the underlying data. Returns `None` if the
    /// data has been collected and is no longer available.
    #[must_use]
    pub fn as_root(&self, guard: &CollectionGuard<'_>) -> Option<Root<T>> {
        self.load_slot(guard).map(|allocated| {
            allocated.roots.fetch_add(1, Ordering::Acquire);
            Root {
                data: allocated,
                reference: *self,
            }
        })
    }
}

impl<T> Eq for Ref<T> {}

impl<T> PartialEq for Ref<T> {
    fn eq(&self, other: &Self) -> bool {
        self.any == other.any
    }
}

impl<T> Ord for Ref<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.any.cmp(&other.any)
    }
}

impl<T> PartialOrd for Ref<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.any.cmp(&other.any))
    }
}

impl<T> Clone for Ref<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Ref<T> {}

impl<T> Hash for Ref<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.any.hash(state);
    }
}

impl<T> PartialEq<Root<T>> for Ref<T>
where
    T: Collectable,
{
    fn eq(&self, other: &Root<T>) -> bool {
        *self == other.reference
    }
}

impl<T> PartialEq<&'_ Root<T>> for Ref<T>
where
    T: Collectable,
{
    fn eq(&self, other: &&'_ Root<T>) -> bool {
        self == *other
    }
}

impl<T> PartialEq<AnyRef> for Ref<T>
where
    T: Collectable,
{
    fn eq(&self, other: &AnyRef) -> bool {
        self.any == *other
    }
}

impl<T> PartialEq<&'_ AnyRef> for Ref<T>
where
    T: Collectable,
{
    fn eq(&self, other: &&'_ AnyRef) -> bool {
        self == *other
    }
}

impl<T> Debug for Ref<T>
where
    T: Collectable + Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let guard = CollectionGuard::acquire();
        if let Some(contents) = self.load(&guard) {
            Debug::fmt(contents, f)
        } else {
            f.debug_tuple("Ref").field(&"<freed>").finish()
        }
    }
}

// SAFETY: Ref<T>'s usage of a pointer prevents auto implementation.
// `Collectable` requires `Send`, and `Ref<T>` ensures proper Send + Sync
// behavior in its memory accesses.
unsafe impl<T> Send for Ref<T> where T: Send {}
// SAFETY: Ref<T>'s usage of a pointer prevents auto implementation.
// `Collectable` requires `Send`, and `Ref<T>` ensures proper Send + Sync
// behavior in its memory accesses.
unsafe impl<T> Sync for Ref<T> where T: Sync {}

#[derive(Default)]
struct Bins {
    by_type: RwLock<Map<TypeIndex, Arc<dyn AnyBin>>>,
}

impl Bins {
    /// # Safety
    ///
    /// This function must only be called when `bin_id` is known to be
    /// allocated.
    unsafe fn allocated_slot_pointer<T>(
        &self,
        type_index: TypeIndex,
        bin_id: BinId,
    ) -> *const Rooted<T>
    where
        T: Collectable,
    {
        let by_type = self.by_type.read();
        let slot = &by_type
            .get(&type_index)
            .expect("areas are never deallocated")
            .as_any()
            .downcast_ref::<Bin<T>>()
            .expect("type mismatch")
            .slabs[bin_id.slab() as usize]
            .slots[usize::from(bin_id.slot())];

        // The actual unsafe operation: Requires that this slot is allocated to
        // be safe.
        &*(*slot.value.get()).allocated
    }

    fn adopt<T>(value: Rooted<T>, bins_guard: &CollectionGuard<'_>) -> (TypeIndex, u32, BinId)
    where
        T: Collectable,
    {
        let type_id = TypeIndex::of::<T>();
        let mut by_type = bins_guard.bins().by_type.upgradable_read();
        if let Some(bin) = by_type.get(&type_id) {
            let (gen, bin) = bin
                .as_any()
                .downcast_ref::<Bin<T>>()
                .expect("type mismatch")
                .adopt(value);
            (type_id, gen, bin)
        } else {
            by_type.with_upgraded(|by_type| {
                // We don't need to check for another thread allocating, because the
                // only thread that can allocate is the local thread. We needed a
                // write guard, however, because other threads could be trying to
                // load data this thread allocated.
                let bin = Bin::new(value, type_id, bins_guard.thread.thread_id);
                by_type.insert(type_id, Arc::new(bin));
                (type_id, 0, BinId::first())
            })
        }
    }
}

struct Bin<T>
where
    T: Collectable,
{
    thread_id: CollectorThreadId,
    type_index: TypeIndex,
    free_head: AtomicU32,
    slabs: Slabs<T>,
    slabs_tail: Cell<Option<*const Slabs<T>>>,
    mapper: Mapper<T::Target>,
}

impl<T> Bin<T>
where
    T: Collectable,
{
    fn new(first_value: Rooted<T>, type_index: TypeIndex, thread_id: CollectorThreadId) -> Self {
        Self {
            thread_id,
            type_index,
            free_head: AtomicU32::new(0),
            slabs: Slabs::new(first_value, 0),
            slabs_tail: Cell::new(None),
            mapper: Mapper(Box::new(MappingFunction::<T>::new())),
        }
    }

    fn adopt(&self, value: Rooted<T>) -> (u32, BinId) {
        let mut value = Some(value);
        loop {
            let bin_id = BinId(self.free_head.load(Ordering::Acquire));
            if bin_id.invalid() {
                break;
            }
            let slab = &self.slabs[bin_id.slab() as usize];
            let slot_index = bin_id.slot();
            let slot = &slab.slots[usize::from(slot_index)];

            // SAFETY: Unallocated slots are only accessed through the
            // current local thread while a guard is held, which must be
            // true for this function to be invoked. try_allocate ensures
            // this slot wasn't previously allocated, making it safe for us
            // to initialize the data with `value`.
            if let Some((generation, next)) = slot.state.try_allocate(|| unsafe {
                let next = (*slot.value.get()).free;
                slot.value.get().write(SlotData {
                    allocated: ManuallyDrop::new(value.take().expect("only taken once")),
                });
                next
            }) {
                self.free_head.store(next, Ordering::Release);
                let _result = slab.last_allocated.fetch_update(
                    Ordering::Release,
                    Ordering::Acquire,
                    |last_allocated| (last_allocated < slot_index).then_some(slot_index),
                );
                return (generation, bin_id);
            }
        }

        let tail = if let Some(tail) = self.slabs_tail.get() {
            // SAFETY: slabs_tail is never deallocated, and this unsafe
            // operation only extends the lifetime as far as `self`.
            unsafe { &*tail }
        } else {
            &self.slabs
        };
        let (generation, bin_id, new_tail) = tail.adopt(value.take().expect("only taken once"));

        if new_tail.is_some() {
            self.slabs_tail.set(new_tail);
        }
        (generation, bin_id)
    }

    fn load<'guard>(
        &self,
        bin_id: BinId,
        slot_generation: u32,
        _guard: &'guard CollectionGuard<'_>,
    ) -> Option<&'guard Rooted<T>> {
        let slab = self.slabs.get(bin_id.slab() as usize)?;
        let slot = &slab.slots[usize::from(bin_id.slot())];
        slot.state
            .allocated_with_generation(slot_generation)
            .then_some(
                // SAFETY: The collector cannot collect data while `guard` is
                // active, so it is safe to create a reference to this data
                // bound to the guard's lifetime.
                unsafe { &(*slot.value.get()).allocated },
            )
    }
}

trait AnyBin: Send + Sync {
    fn trace(&self, tracer: &mut Tracer<'_>);
    fn trace_one(&self, slot_generation: u32, bin: BinId, tracer: &mut Tracer<'_>);
    fn mark_one(&self, mark_bits: u8, slot_generation: u32, bin: BinId) -> bool;
    fn sweep(&self, mark_bits: u8) -> usize;
    fn as_any(&self) -> &dyn Any;
    fn mapper(&self) -> &dyn Any;
    fn load_root(
        &self,
        bin_id: BinId,
        slot_generation: u32,
        guard: &CollectionGuard<'_>,
    ) -> Option<AnyRoot>;
}

impl<T> AnyBin for Bin<T>
where
    T: Collectable,
{
    fn trace(&self, tracer: &mut Tracer) {
        for (slab_index, slab) in self.slabs.iter().enumerate() {
            for (index, slot) in slab.slots.iter().enumerate() {
                let Some(slot_generation) = slot.state.generation() else {
                    continue;
                };
                // SAFETY: `state.generation()` only returns `Some()` when the
                // slot is allocated.
                let root_count =
                    unsafe { (*slot.value.get()).allocated.roots.load(Ordering::Relaxed) };
                if root_count > 0 {
                    tracer.mark(AnyRef {
                        type_index: self.type_index,
                        creating_thread: tracer.tracing_thread,
                        slot_generation,
                        bin_id: BinId::new(slab_index.cast::<u32>(), index.cast::<u8>()),
                    });
                }
            }
        }
    }

    fn trace_one(&self, slot_generation: u32, bin: BinId, tracer: &mut Tracer) {
        let slot = &self.slabs[bin.slab() as usize].slots[usize::from(bin.slot())];
        if slot.state.generation() == Some(slot_generation) {
            // SAFETY: `state.generation()` only returns `Some()` when the slot
            // is allocated.
            unsafe {
                (*slot.value.get()).allocated.value.trace(tracer);
            }
        }
    }

    fn mark_one(&self, mark_bits: u8, slot_generation: u32, bin: BinId) -> bool {
        let slot = &self.slabs[bin.slab() as usize].slots[usize::from(bin.slot())];
        slot.state.mark(mark_bits, slot_generation) && T::MAY_CONTAIN_REFERENCES
    }

    fn sweep(&self, mark_bits: u8) -> usize {
        let mut free_head = BinId(self.free_head.load(Ordering::Acquire));
        let mut allocated = 0;
        for (slab_index, slab) in self.slabs.iter().enumerate() {
            let current_last_allocated = slab.last_allocated.load(Ordering::Acquire);
            let mut last_allocated = 0;
            for (slot_index, slot) in slab.slots[0..=usize::from(current_last_allocated)]
                .iter()
                .enumerate()
            {
                match slot.sweep(mark_bits, free_head) {
                    SlotSweepStatus::Swept => {
                        free_head = BinId::new(slab_index.cast::<u32>(), slot_index.cast::<u8>());
                    }
                    SlotSweepStatus::Allocated => {
                        allocated += 1;
                        last_allocated = slot_index.cast::<u8>();
                    }
                    SlotSweepStatus::NotAllocated => {}
                }
            }
            if last_allocated < current_last_allocated {
                slab.last_allocated.store(last_allocated, Ordering::Release);
            }
        }
        self.free_head.store(free_head.0, Ordering::Release);
        allocated
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn mapper(&self) -> &dyn Any {
        &self.mapper
    }

    fn load_root(
        &self,
        bin_id: BinId,
        slot_generation: u32,
        _guard: &CollectionGuard<'_>,
    ) -> Option<AnyRoot> {
        let slab = self.slabs.get(bin_id.slab() as usize)?;
        let slot = &slab.slots[usize::from(bin_id.slot())];
        if slot.state.allocated_with_generation(slot_generation) {
            // SAFETY: The collector cannot collect data while `_guard` is
            // active, so it is safe to create a reference to this data within
            // this function.
            let slot = unsafe { &(*slot.value.get()).allocated };
            slot.roots.fetch_add(1, Ordering::Relaxed);
            Some(AnyRoot {
                rooted: std::ptr::addr_of!(**slot).cast::<()>(),
                roots: &slot.roots,
                any: AnyRef {
                    bin_id,
                    creating_thread: self.thread_id,
                    type_index: self.type_index,
                    slot_generation,
                },
            })
        } else {
            None
        }
    }
}

struct Mapper<T>(Box<dyn MapFn<T>>)
where
    T: ?Sized + 'static;

trait MapFn<T>
where
    T: ?Sized + 'static,
{
    fn load_mapped<'guard>(
        &self,
        id: BinId,
        slot_generation: u32,
        bin: &dyn AnyBin,
        guard: &'guard CollectionGuard<'_>,
    ) -> Option<&'guard T>;
}

struct MappingFunction<C: Collectable>(PhantomData<C>);

impl<C> MappingFunction<C>
where
    C: Collectable,
{
    const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<C> MapFn<C::Target> for MappingFunction<C>
where
    C: Collectable,
{
    fn load_mapped<'guard>(
        &self,
        id: BinId,
        slot_generation: u32,
        bin: &dyn AnyBin,
        guard: &'guard CollectionGuard<'_>,
    ) -> Option<&'guard C::Target> {
        let ref_counted =
            bin.as_any()
                .downcast_ref::<Bin<C>>()?
                .load(id, slot_generation, guard)?;

        Some(ref_counted.value.map_as())
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
struct BinId(u32);

impl BinId {
    const fn new(slab: u32, slot: u8) -> Self {
        assert!(slab < 0xFF_FFFF);
        Self((slab + 1) << 8 | slot as u32)
    }

    const fn first() -> Self {
        Self::new(0, 0)
    }

    const fn invalid(self) -> bool {
        self.0 == 0
    }

    const fn slab(self) -> u32 {
        (self.0 >> 8) - 1
    }

    #[allow(clippy::cast_possible_truncation)] // intentional
    const fn slot(self) -> u8 {
        self.0 as u8
    }
}

struct Slabs<T> {
    offset: usize,
    first_free_slab: Cell<usize>,
    slab_slots: [UnsafeCell<Option<Box<Slab<T>>>>; 256],
    next: UnsafeCell<Option<Box<Slabs<T>>>>,
}

impl<T> Slabs<T>
where
    T: Collectable,
{
    fn new(initial_value: Rooted<T>, offset: usize) -> Self {
        let mut initial_value = Some(initial_value);
        Self {
            offset,
            first_free_slab: Cell::new(0),
            slab_slots: array::from_fn(|index| {
                UnsafeCell::new(if index == 0 {
                    Some(Slab::new(initial_value.take().expect("only called once")))
                } else {
                    None
                })
            }),
            next: UnsafeCell::new(None),
        }
    }

    fn get(&self, index: usize) -> Option<&Slab<T>> {
        if index < 256 {
            // SAFETY: Slabs are always initialized and this function ties the
            // lifetime of the slab to `self`.
            unsafe { (*self.slab_slots[index].get()).as_ref() }.map(|slab| &**slab)
        } else {
            // SAFETY: Slabs are always initialized and this function ties the
            // lifetime of the slab to `self`.
            unsafe { (*self.next.get()).as_ref() }.and_then(|slabs| slabs.get(index - 256))
        }
    }

    fn adopt(&self, mut value: Rooted<T>) -> (u32, BinId, Option<*const Slabs<T>>) {
        let first_free = self.first_free_slab.get();

        for index in first_free..256 {
            // SAFETY: Slabs are always initialized and this
            // function ties the lifetime of the slab to `this`.
            let slab = unsafe { &mut (*self.slab_slots[index].get()) };

            if let Some(slab) = slab {
                match slab.try_adopt(value, index + self.offset) {
                    Ok((gen, bin)) => {
                        if first_free < index {
                            self.first_free_slab.set(index);
                        }
                        return (gen, bin, None);
                    }
                    Err(returned) => value = returned,
                }
            } else {
                *slab = Some(Slab::new(value));
                return (0, BinId::new(index.cast::<u32>(), 0), None);
            }
        }

        // SAFETY: Slabs are always initialized and this function ties
        // the lifetime of `next` to `this`.
        if let Some(next) = unsafe { &*self.next.get() } {
            next.adopt(value)
        } else {
            let slabs = Box::new(Slabs::new(value, self.offset + 256));
            let new_tail: *const Slabs<T> = &*slabs;

            // SAFETY: next is never accessed by any other thread except the
            // thread that owns this set of bins.
            unsafe { self.next.get().write(Some(slabs)) };

            (
                0,
                BinId::new(self.offset.cast::<u32>() + 256, 0),
                Some(new_tail),
            )
        }
    }

    fn iter(&self) -> SlabsIter<'_, T> {
        SlabsIter {
            slabs: self.slab_slots.iter(),
            this: self,
        }
    }
}

impl<T> std::ops::Index<usize> for Slabs<T>
where
    T: Collectable,
{
    type Output = Slab<T>;

    fn index(&self, index: usize) -> &Self::Output {
        self.get(index).expect("out of bounds")
    }
}

struct SlabsIter<'a, T> {
    slabs: slice::Iter<'a, UnsafeCell<Option<Box<Slab<T>>>>>,
    this: &'a Slabs<T>,
}

impl<'a, T> Iterator for SlabsIter<'a, T> {
    type Item = &'a Slab<T>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            for slab in &mut self.slabs {
                // SAFETY: Slabs are always initialized and this function ties
                // the lifetime of `slab` to `'a`.
                let Some(slab) = (unsafe { (*slab.get()).as_ref() }) else {
                    continue;
                };
                return Some(slab);
            }
            // SAFETY: Slabs are always initialized and this function ties
            // the lifetime of `slab` to `'a`.
            self.this = unsafe { (*self.this.next.get()).as_ref() }?;
        }
    }
}

struct Slab<T> {
    slots: [Slot<T>; 256],
    last_allocated: AtomicU8,
}

impl<T> Slab<T> {
    fn new(first_value: Rooted<T>) -> Box<Self>
    where
        T: Collectable,
    {
        // SAFETY: `Slot<T>` only utilizes types that zero-initialized data is a
        // valid representation.
        let mut this: Box<Self> =
            unsafe { Box::from_raw(alloc_zeroed(Layout::new::<Self>()).cast()) };
        this.slots[0] = Slot {
            state: SlotState::new_allocated(),
            value: UnsafeCell::new(SlotData {
                allocated: ManuallyDrop::new(first_value),
            }),
        };
        this
    }

    fn try_adopt(&self, value: Rooted<T>, slab_index: usize) -> Result<(u32, BinId), Rooted<T>> {
        if let Ok(last_allocated) = self.last_allocated.fetch_update(
            Ordering::Release,
            Ordering::Acquire,
            |last_allocated| last_allocated.checked_add(1),
        ) {
            let slot_index = last_allocated + 1;
            if slot_index == u8::MAX {
                CollectorCommand::schedule_collect_if_needed();
            }
            let slot = &self.slots[usize::from(slot_index)];
            let generation = slot.allocate(value);

            Ok((generation, BinId::new(slab_index.cast::<u32>(), slot_index)))
        } else {
            Err(value)
        }
    }
}

union SlotData<T> {
    allocated: ManuallyDrop<Rooted<T>>,
    free: u32,
}

struct Slot<T> {
    state: SlotState,
    value: UnsafeCell<SlotData<T>>,
}

impl<T> Slot<T> {
    fn allocate(&self, value: Rooted<T>) -> u32 {
        let generation = self.state.allocate();
        // SAFETY: `state.allocate()` will panic if the slot was previously
        // allocated.
        unsafe {
            self.value.get().write(SlotData {
                allocated: ManuallyDrop::new(value),
            });
        }
        generation
    }

    fn sweep(&self, mark_bits: u8, free_head: BinId) -> SlotSweepStatus {
        match self.state.sweep(mark_bits) {
            SlotSweepStatus::Swept => {
                // SAFETY: `state.sweep()` has marked this slot as free,
                // ensuring all attempts to get a reference will fail. It is
                // safe to drop the data, which we then overwrite with the free
                // head for reusing slots.
                unsafe {
                    ManuallyDrop::drop(&mut (*self.value.get()).allocated);
                    self.value.get().write(SlotData { free: free_head.0 });
                }
                SlotSweepStatus::Swept
            }
            other => other,
        }
    }
}

// SAFETY: Bin<T> is Send as long as T is Send.
unsafe impl<T> Send for Bin<T> where T: Collectable {}
// SAFETY: Bin<T> is Sync as long as T is Sync.
unsafe impl<T> Sync for Bin<T> where T: Collectable {}

struct Rooted<T> {
    roots: AtomicU64,
    value: T,
}

impl<T> Rooted<T> {
    fn reference(value: T) -> Self {
        Self {
            roots: AtomicU64::new(0),
            value,
        }
    }

    fn root(value: T) -> Self {
        Self {
            roots: AtomicU64::new(1),
            value,
        }
    }
}

struct SlotState(AtomicU64);

impl SlotState {
    const ALLOCATED: u64 = 1 << 33;
    const MARK_OFFSET: u32 = 34;

    const fn new_allocated() -> Self {
        Self(AtomicU64::new(Self::ALLOCATED))
    }

    fn generation(&self) -> Option<u32> {
        let state = self.0.load(Ordering::Relaxed);
        (state & Self::ALLOCATED != 0).then(|| state.cast::<u32>())
    }

    fn allocated_with_generation(&self, generation: u32) -> bool {
        let state = self.0.load(Ordering::Relaxed);
        state & Self::ALLOCATED != 0 && state.cast::<u32>() == generation
    }

    fn try_allocate<R>(&self, allocated: impl FnOnce() -> R) -> Option<(u32, R)> {
        let state = self.0.load(Ordering::Acquire);
        if state & Self::ALLOCATED != 0 {
            return None;
        }

        let result = allocated();
        let generation = state.cast::<u32>().wrapping_add(1);

        self.0
            .store(Self::ALLOCATED | u64::from(generation), Ordering::Release);

        Some((generation, result))
    }

    fn allocate(&self) -> u32 {
        let state = self.0.load(Ordering::Acquire);
        debug_assert_eq!(state & Self::ALLOCATED, 0);

        let generation = state.cast::<u32>().wrapping_add(1);

        self.0
            .store(Self::ALLOCATED | u64::from(generation), Ordering::Release);
        generation
    }

    fn mark(&self, mark_bits: u8, slot_generation: u32) -> bool {
        let mut state = self.0.load(Ordering::Acquire);
        if state & Self::ALLOCATED == 0 || state.cast::<u32>() != slot_generation {
            return false;
        }

        let mark_bits = u64::from(mark_bits);
        let current_mark = state >> Self::MARK_OFFSET;
        if current_mark == mark_bits {
            return false;
        }

        state &= !(0xFF << Self::MARK_OFFSET);
        state |= mark_bits << Self::MARK_OFFSET;

        self.0.store(state, Ordering::Release);
        true
    }

    fn sweep(&self, mark_bits: u8) -> SlotSweepStatus {
        let mark_bits = u64::from(mark_bits);
        let state = self.0.load(Ordering::Acquire);
        let current_mark = state >> Self::MARK_OFFSET;
        if state & Self::ALLOCATED == 0 {
            return SlotSweepStatus::NotAllocated;
        } else if current_mark == mark_bits {
            return SlotSweepStatus::Allocated;
        }

        let generation = state.cast::<u32>();
        self.0.store(u64::from(generation), Ordering::Release);
        SlotSweepStatus::Swept
    }
}

#[derive(Clone, Copy)]
enum SlotSweepStatus {
    Allocated,
    NotAllocated,
    Swept,
}

/// Invokes the garbage collector.
///
/// # Panics
///
/// This function will panic if any [`CollectionGuard`]s are held and not
/// yielding by the current thread when invoked. If a guard is held, consider
/// calling [`CollectionGuard::collect()`] instead.
pub fn collect() {
    try_collect().unwrap();
}

/// Invokes the garbage collector.
///
/// # Errors
///
/// If any [`CollectionGuard`]s are held by this thread when this function is
/// invoked, [`WouldDeadlock`] is returned.
pub fn try_collect() -> Result<(), WouldDeadlock> {
    if ThreadPool::current_depth() > 0 {
        return Err(WouldDeadlock);
    }
    collect_unchecked();
    Ok(())
}

/// An error indicating an operation would deadlock.
///
/// [`CollectionGuard::acquire`] can be called multiple times from the same
/// thread, but some operations require that all guards for the current thread
/// have been released before performing. This error signals when an operation
/// can't succeed because the current thread has an outstanding guard that must
/// be dropped for the operation to be able to be performed.
///
/// ```rust
/// use refuse::{CollectionGuard, WouldDeadlock};
///
/// let mut guard1 = CollectionGuard::acquire();
/// let guard2 = CollectionGuard::acquire();
///
/// assert_eq!(guard1.try_collect(), Err(WouldDeadlock));
///
/// drop(guard2);
///
/// assert_eq!(guard1.try_collect(), Ok(()));
/// ```
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct WouldDeadlock;

impl Display for WouldDeadlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("current thread has a non-yielding CollectionGuard")
    }
}

fn collect_unchecked() {
    let now = Instant::now();
    CollectorCommand::Collect(now).send();
    GlobalCollector::get().info.wait_for_collection(now);
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
struct TypeIndex(u32);

impl TypeIndex {
    fn of<T: 'static>() -> TypeIndex {
        let collector = GlobalCollector::get();
        let types = collector.info.type_indexes.read();
        let type_id = TypeId::of::<T>();
        if let Some(index) = types.get(&type_id) {
            *index
        } else {
            drop(types);
            let mut types = collector.info.type_indexes.write();
            let next_id = types.len();

            *types
                .entry(type_id)
                .or_insert(TypeIndex(u32::try_from(next_id).expect("too many types")))
        }
    }
}

/// A type-erased root garbage collected reference.
#[derive(Eq, PartialEq, PartialOrd, Ord)]
pub struct AnyRoot {
    rooted: *const (),
    roots: *const AtomicU64,
    any: AnyRef,
}

impl AnyRoot {
    /// Loads a reference to the underlying data. Returns `None` if `T` is not
    /// the type of the underlying data.
    #[must_use]
    pub fn load<T>(&self) -> Option<&T>
    where
        T: Collectable,
    {
        if TypeIndex::of::<T>() == self.any.type_index {
            // SAFETY: `self` has a root reference to the underlying data,
            // ensuring that it cannot be collected while `self` exists. We've
            // verified that `T` is the same underlying type. We can return a
            // reference bound to `self`'s lifetime safely.
            let rooted = unsafe { &*self.rooted.cast::<Rooted<T>>() };
            Some(&rooted.value)
        } else {
            None
        }
    }

    /// Returns a [`Root<T>`] if the underlying reference points to a `T`.
    #[must_use]
    pub fn downcast_root<T>(&self) -> Option<Root<T>>
    where
        T: Collectable,
    {
        if TypeIndex::of::<T>() == self.any.type_index {
            // SAFETY: `self` has a root reference to the underlying data,
            // ensuring that it cannot be collected while `self` exists. We've
            // verified that `T` is the same underlying type. We can return a
            // reference bound to `self`'s lifetime safely.
            let rooted = unsafe { &*self.rooted.cast::<Rooted<T>>() };

            // Increment the strong count for the returned root.
            rooted.roots.fetch_add(1, Ordering::Relaxed);

            Some(Root {
                data: rooted,
                reference: self.downcast_ref(),
            })
        } else {
            None
        }
    }

    /// Returns a [`Ref<T>`].
    ///
    /// This function does not do any type checking. If `T` is not the correct
    /// type, attempting to load the underyling value will fail.
    #[must_use]
    pub const fn downcast_ref<T>(&self) -> Ref<T>
    where
        T: Collectable,
    {
        self.any.downcast_ref()
    }

    /// Returns a [`Ref<T>`], if `T` matches the type of this reference.
    #[must_use]
    pub fn downcast_checked<T>(&self) -> Option<Ref<T>>
    where
        T: Collectable,
    {
        self.any.downcast_checked()
    }

    /// Returns an untyped "weak" reference to this root.
    #[must_use]
    pub const fn as_any(&self) -> AnyRef {
        self.any
    }
}

impl Clone for AnyRoot {
    fn clone(&self) -> Self {
        // SAFETY: `self` has a root reference to the underlying data, ensuring
        // that it cannot be collected while `self` exists.
        unsafe { &*self.roots }.fetch_add(1, Ordering::Acquire);
        Self {
            rooted: self.rooted,
            roots: self.roots,
            any: self.any,
        }
    }
}

impl Drop for AnyRoot {
    fn drop(&mut self) {
        // SAFETY: `self` has a root reference to the underlying data, ensuring
        // that it cannot be collected while `self` exists.
        if unsafe { &*self.roots }.fetch_sub(1, Ordering::Acquire) == 1 {
            CollectorCommand::schedule_collect_if_needed();
        }
    }
}

impl Hash for AnyRoot {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.any.hash(state);
    }
}

impl<T> From<Root<T>> for AnyRoot
where
    T: Collectable,
{
    fn from(value: Root<T>) -> Self {
        value.into_any_root()
    }
}

// SAFETY: AnyRoot's usage of a pointer prevents auto implementation.
// `Collectable` requires `Send`, and `Root<T>` ensures proper Send + Sync
// behavior in its memory accesses.
unsafe impl Send for AnyRoot {}
// SAFETY: AnyRoot's usage of a pointer prevents auto implementation.
// `Collectable` requires `Send`, and `Root<T>` ensures proper Send + Sync
// behavior in its memory accesses.
unsafe impl Sync for AnyRoot {}

/// A type-erased garbage collected reference.
#[derive(Debug, Clone, Copy, Eq, PartialEq, PartialOrd, Ord)]
pub struct AnyRef {
    bin_id: BinId,
    creating_thread: CollectorThreadId,
    type_index: TypeIndex,
    slot_generation: u32,
}

impl AnyRef {
    /// Loads a slot from a specific thread's bins, performing type checking in
    /// the process.
    fn load_slot_from<'guard, T>(
        &self,
        bins: &Map<TypeIndex, Arc<dyn AnyBin>>,
        guard: &'guard CollectionGuard<'_>,
    ) -> Option<&'guard Rooted<T>>
    where
        T: Collectable,
    {
        // The Any::downcast_ref ensures that type_index and T match.
        bins.get(&self.type_index)?
            .as_any()
            .downcast_ref::<Bin<T>>()?
            .load(self.bin_id, self.slot_generation, guard)
    }

    /// Loads a slot, performing type checking in the process.
    fn load_slot<'guard, T>(&self, guard: &'guard CollectionGuard<'_>) -> Option<&'guard Rooted<T>>
    where
        T: Collectable,
    {
        if guard.thread.thread_id == self.creating_thread {
            self.load_slot_from(&guard.bins().by_type.read(), guard)
        } else {
            let all_threads = guard.thread.all_threads.read();
            let other_thread_bins = all_threads
                .get(&self.creating_thread)
                .and_then(Weak::upgrade)?;
            let bins = guard.bins_for(&other_thread_bins);

            let result = self.load_slot_from(&bins.by_type.read(), guard);
            drop(other_thread_bins);
            result
        }
    }

    /// Returns a [`Ref<T>`].
    ///
    /// This function does not do any type checking. If `T` is not the correct
    /// type, attempting to load the underyling value will fail.
    #[must_use]
    pub const fn downcast_ref<T>(&self) -> Ref<T>
    where
        T: Collectable,
    {
        Ref {
            any: *self,
            _t: PhantomData,
        }
    }

    /// Returns a root for this reference, if the value has not been collected.
    pub fn upgrade(&self, guard: &CollectionGuard<'_>) -> Option<AnyRoot> {
        if guard.thread.thread_id == self.creating_thread {
            self.load_root_from(&guard.bins().by_type.read(), guard)
        } else {
            let all_threads = guard.thread.all_threads.read();
            let other_thread_bins = all_threads
                .get(&self.creating_thread)
                .and_then(Weak::upgrade)?;
            let bins = guard.bins_for(&other_thread_bins);

            let result = self.load_root_from(&bins.by_type.read(), guard);
            drop(other_thread_bins);
            result
        }
    }

    fn load_root_from(
        &self,
        bins: &Map<TypeIndex, Arc<dyn AnyBin>>,
        guard: &CollectionGuard<'_>,
    ) -> Option<AnyRoot> {
        bins.get(&self.type_index)?
            .load_root(self.bin_id, self.slot_generation, guard)
    }

    /// Returns a [`Ref<T>`], if `T` matches the type of this reference.
    #[must_use]
    pub fn downcast_checked<T>(&self) -> Option<Ref<T>>
    where
        T: Collectable,
    {
        (TypeIndex::of::<T>() == self.type_index).then_some(self.downcast_ref())
    }

    /// Returns a [`Root<T>`] if the underlying reference points to a `T` that
    /// has not been collected.
    #[must_use]
    pub fn downcast_root<T>(&self, guard: &CollectionGuard) -> Option<Root<T>>
    where
        T: Collectable,
    {
        self.downcast_ref().as_root(guard)
    }

    /// Loads a reference to the underlying data. Returns `None` if the data has
    /// been collected and is no longer available.
    #[must_use]
    pub fn load<'guard, T>(&self, guard: &'guard CollectionGuard<'_>) -> Option<&'guard T>
    where
        T: Collectable,
    {
        self.downcast_ref().load(guard)
    }

    /// Returns a reference to the result of [`MapAs::map_as()`], if the value
    /// has not been collected and [`MapAs::Target`] is `T`.
    pub fn load_mapped<'guard, T>(&self, guard: &'guard CollectionGuard<'_>) -> Option<&'guard T>
    where
        T: ?Sized + 'static,
    {
        if guard.thread.thread_id == self.creating_thread {
            self.load_mapped_slot_from(&guard.bins().by_type.read(), guard)
        } else {
            let all_threads = guard.thread.all_threads.read();
            let other_thread_bins = all_threads
                .get(&self.creating_thread)
                .and_then(Weak::upgrade)?;
            let bins = guard.bins_for(&other_thread_bins);

            let result = self.load_mapped_slot_from(&bins.by_type.read(), guard);
            drop(other_thread_bins);
            result
        }
    }

    fn load_mapped_slot_from<'guard, T>(
        &self,
        bins: &Map<TypeIndex, Arc<dyn AnyBin>>,
        guard: &'guard CollectionGuard<'_>,
    ) -> Option<&'guard T>
    where
        T: ?Sized + 'static,
    {
        let bins = bins.get(&self.type_index)?;

        bins.mapper().downcast_ref::<Mapper<T>>()?.0.load_mapped(
            self.bin_id,
            self.slot_generation,
            &**bins,
            guard,
        )
    }
}

impl Trace for AnyRef {
    const MAY_CONTAIN_REFERENCES: bool = true;

    fn trace(&self, tracer: &mut Tracer) {
        tracer.mark(self);
    }
}

impl From<&'_ AnyRef> for AnyRef {
    fn from(value: &'_ AnyRef) -> Self {
        *value
    }
}

impl<T> From<&'_ Ref<T>> for AnyRef
where
    T: Collectable,
{
    fn from(value: &'_ Ref<T>) -> Self {
        value.as_any()
    }
}

impl<T> From<Ref<T>> for AnyRef
where
    T: Collectable,
{
    fn from(value: Ref<T>) -> Self {
        value.as_any()
    }
}

impl<T> From<&'_ Root<T>> for AnyRef
where
    T: Collectable,
{
    fn from(value: &'_ Root<T>) -> Self {
        value.downgrade_any()
    }
}

impl Hash for AnyRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.type_index.hash(state);
        self.creating_thread.hash(state);
        self.slot_generation.hash(state);
        self.bin_id.hash(state);
    }
}

#[cfg(test)]
mod tests;
