#![doc = include_str!("../README.md")]
use core::slice;
use std::alloc::{alloc_zeroed, Layout};
use std::any::{Any, TypeId};
use std::cell::{Cell, OnceCell, RefCell, UnsafeCell};
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::sync::atomic::{self, AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};
use std::{array, thread};

use ahash::AHashMap;
use crossbeam_utils::sync::{Parker, Unparker};
use flume::{Receiver, RecvError, RecvTimeoutError, Sender};
use intentional::{Assert, Cast};
use kempt::Map;
use parking_lot::{Condvar, Mutex, RwLock};

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
struct CollectorThreadId(u64);

impl CollectorThreadId {
    fn unique() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
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

type AllThreadBins = Arc<RwLock<AHashMap<CollectorThreadId, Weak<UnsafeBins>>>>;

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
    mark_bits: u8,
    mark_one_sender: Arc<Sender<MarkRequest>>,
}

struct MarkRequest {
    thread: CollectorThreadId,
    type_id: TypeId,
    slot_generation: u32,
    bin_id: BinId,
    mark_bits: u8,
    mark_one_sender: Arc<Sender<MarkRequest>>,
}

struct Collector {
    shared: Arc<CollectorInfo>,
    receiver: Receiver<CollectorCommand>,
    thread_bins: AHashMap<CollectorThreadId, ThreadBins>,
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
            thread_bins: AHashMap::new(),
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
                            request.bins.trace(&mut Tracer::new(
                                request.thread,
                                &request.mark_one_sender,
                                request.mark_bits,
                            ));
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
        thread_bins: &'a AHashMap<CollectorThreadId, ThreadBins>,
        start: Instant,
        average_collection_locking: Duration,
        pause_failures: u8,
        collector: &CollectorInfo,
        parker: &Parker,
    ) -> Option<AHashMap<CollectorThreadId, &'a mut Bins>> {
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
                .map(|(thread_id, bins)| (*thread_id, unsafe { bins.bins.assume_mut() }))
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
                        mark_bits: self.mark_bits,
                        mark_one_sender: mark_one_sender.clone(),
                    })
                    .expect("tracer stopped");
            }
        }

        loop {
            let MarkRequest {
                thread,
                type_id,
                slot_generation,
                bin_id,
                mark_bits,
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
                .get(&type_id)
                .expect("areas are never deallocated")
                .clone();
            if bins.mark_one(mark_bits, slot_generation, bin_id) {
                bins.trace_one(
                    slot_generation,
                    bin_id,
                    &mut Tracer::new(thread, &mark_one_sender, self.mark_bits),
                );
            }
        }

        atomic::fence(Ordering::Acquire);
        let mut threads_to_remove = Vec::new();
        for (thread_id, bins) in all_bins {
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
    static THREAD_BINS: RefCell<OnceCell<ThreadLocalBins>> = RefCell::new(OnceCell::new());
}

#[derive(Default)]
struct UnsafeBins(UnsafeCell<ManuallyDrop<Bins>>);

impl UnsafeBins {
    unsafe fn assume_readable(&self) -> &Bins {
        &*self.0.get()
    }

    #[allow(clippy::mut_from_ref)]
    unsafe fn assume_mut(&self) -> &mut Bins {
        &mut *self.0.get()
    }
}

unsafe impl Send for UnsafeBins {}
unsafe impl Sync for UnsafeBins {}

impl Drop for UnsafeBins {
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(self.0.get_mut());
        }
    }
}

#[derive(Clone)]
struct ThreadLocalBins {
    bins: Arc<UnsafeBins>,
    thread_id: CollectorThreadId,
    all_threads: AllThreadBins,
}

impl ThreadLocalBins {
    fn get() -> Self {
        THREAD_BINS.with_borrow(|bins| bins.get().expect("not invoked from collected()").clone())
    }
}

impl Drop for ThreadLocalBins {
    fn drop(&mut self) {
        // If this reference and the one in the collector are the last
        // references, send a shutdown notice.
        if Arc::strong_count(&self.bins) == 2 {
            CollectorCommand::ThreadShutdown(self.thread_id).send();
        }
    }
}

/// Executes `wrapped` with garbage collection available.
///
/// This function installs a garbage collector for this thread, if needed.
/// Repeated and nested calls are allowed.
///
/// Invoking [`CollectionGuard::acquire()`] within `wrapped` will return a
/// result, while invoking it outside of a collected context will panic.
///
/// This function utilizes Rust's thread-local storage.
pub fn collected<R>(wrapped: impl FnOnce() -> R) -> R {
    THREAD_BINS.with_borrow(|lock| {
        lock.get_or_init(|| {
            let all_threads = GlobalCollector::get().info.all_threads.clone();
            let bins = Arc::<UnsafeBins>::default();
            let thread_id = CollectorThreadId::unique();
            CollectorCommand::NewThread(thread_id, bins.clone()).send();
            all_threads.write().insert(thread_id, Arc::downgrade(&bins));
            ThreadLocalBins {
                bins,
                thread_id,
                all_threads,
            }
        });
        wrapped()
    })
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

    fn acquire_reader(&mut self) {
        if !self.global.reader_state.read() {
            let mut info_data = self.global.info.lock();
            while !self.global.reader_state.read() {
                self.global.collection_sync.wait(&mut info_data);
            }
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
pub struct CollectionGuard {
    collector: CollectorReadGuard,
    thread: ThreadLocalBins,
}

impl CollectionGuard {
    #[allow(clippy::unused_self)]
    fn bins_for<'a>(&'a self, bins: &'a UnsafeBins) -> &'a Bins {
        unsafe { bins.assume_readable() }
    }

    fn bins(&self) -> &Bins {
        self.bins_for(&self.thread.bins)
    }

    /// Acquires a lock that prevents the garbage collector from running.
    ///
    /// This guard is used to provide read-only access to garbage collected
    /// allocations.
    ///
    /// # Panics
    ///
    /// A panic will occur if this function is called outside of code executed
    /// by [`collected()`].
    #[must_use]
    pub fn acquire() -> Self {
        let collector = CollectorReadGuard::acquire();
        let thread = ThreadLocalBins::get();
        Self { collector, thread }
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
    pub fn collect(&mut self) {
        self.while_unlocked(collect);
    }

    /// Yield to the garbage collector, if needed.
    ///
    /// This function will not yield unless the garbage collector is trying to
    /// acquire this thread's lock. Because of this, it is a fairly efficient
    /// function to invoke. To minimize collection pauses, long-held guards
    /// should call this function regularly.
    pub fn yield_to_collector(&mut self) {
        if self.collector.release_reader_if_collecting() {
            self.collector.acquire_reader();
        }
    }

    /// Executes `unlocked` while this guard is temporarily released.
    pub fn while_unlocked<R>(&mut self, unlocked: impl FnOnce() -> R) -> R {
        self.collector.release_reader();
        let result = unlocked();
        self.collector.acquire_reader();

        result
    }

    fn adopt<T: Collectable>(&self, value: RefCounted<T>) -> (u32, BinId) {
        let (gen, bin) = Bins::adopt(value, self);
        (gen, bin)
    }
}

impl AsRef<CollectionGuard> for CollectionGuard {
    fn as_ref(&self) -> &CollectionGuard {
        self
    }
}

impl AsMut<CollectionGuard> for CollectionGuard {
    fn as_mut(&mut self) -> &mut CollectionGuard {
        self
    }
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
pub trait Trace {
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
pub trait MapAs {
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
pub trait ContainsNoRefs {}

impl<T> Trace for T
where
    T: ContainsNoRefs,
{
    const MAY_CONTAIN_REFERENCES: bool = false;

    fn trace(&self, _tracer: &mut Tracer) {}
}

/// A type that implements [`MapAs`] with an empty implementation.
pub trait NoMapping {}

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
pub trait SimpleType {}

impl<T> NoMapping for T where T: SimpleType {}
impl<T> ContainsNoRefs for T where T: SimpleType {}

impl SimpleType for u8 {}
impl SimpleType for u16 {}
impl SimpleType for u32 {}
impl SimpleType for u64 {}
impl SimpleType for u128 {}
impl SimpleType for usize {}
impl SimpleType for i8 {}
impl SimpleType for i16 {}
impl SimpleType for i32 {}
impl SimpleType for i64 {}
impl SimpleType for i128 {}
impl SimpleType for isize {}

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

impl<T> NoMapping for Vec<T> {}

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
impl<T, const N: usize> NoMapping for [T; N] {}

impl<T> Trace for Root<T>
where
    T: Collectable,
{
    const MAY_CONTAIN_REFERENCES: bool = T::MAY_CONTAIN_REFERENCES;

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

/// A tracer for the garbage collector.
///
/// This type allows [`Collectable`] values to [`mark()`](Self::mark) any
/// [`Ref<T>`]s they contain.
pub struct Tracer<'a> {
    tracing_thread: CollectorThreadId,
    mark_bit: u8,
    mark_one_sender: &'a Arc<Sender<MarkRequest>>,
}

impl<'a> Tracer<'a> {
    fn new(
        thread: CollectorThreadId,
        mark_one_sender: &'a Arc<Sender<MarkRequest>>,
        mark_bit: u8,
    ) -> Self {
        Self {
            tracing_thread: thread,
            mark_bit,
            mark_one_sender,
        }
    }

    /// Marks `collectable` as being referenced, ensuring it is not garbage
    /// collected.
    pub fn mark<T>(&mut self, collectable: Ref<T>)
    where
        T: Collectable,
    {
        self.mark_one_sender
            .send(MarkRequest {
                thread: collectable.creating_thread,
                type_id: TypeId::of::<T>(),
                slot_generation: collectable.slot_generation,
                bin_id: collectable.bin_id,
                mark_bits: self.mark_bit,
                mark_one_sender: self.mark_one_sender.clone(),
            })
            .assert("marker thread not running");
    }
}

#[test]
fn size_of_types() {
    assert_eq!(std::mem::size_of::<Root<u32>>(), 24);
    assert_eq!(std::mem::size_of::<Ref<u32>>(), 16);
}

/// A root reference to a `T` that has been allocated in the garbage collector.
///
/// This type behaves very similarly to [`Arc<T>`]. It is cheap-to-clone,
/// utilizing atomic reference counting to track the number of root references
/// currently exist to the underlying value.
///
/// While any root references exist for a given allocation, the garbage
/// collector will not collect the allocation.
pub struct Root<T>
where
    T: Collectable,
{
    data: *const RefCounted<T>,
    reference: Ref<T>,
}

impl<T> Root<T>
where
    T: Collectable,
{
    fn from_parts(slot_generation: u32, bin_id: BinId, guard: &CollectionGuard) -> Self {
        // SAFETY: The guard is always present except during allocation which
        // never invokes this function. Since `bin_id` was just allocated, we
        // also can assume that it is allocated.
        let data = unsafe { guard.bins().allocated_slot_pointer::<T>(bin_id) };
        Self {
            data,
            reference: Ref {
                creating_thread: guard.thread.thread_id,
                slot_generation,
                bin_id,
                _t: PhantomData,
            },
        }
    }

    /// Stores `value` in the garbage collector, returning a root reference to
    /// the data.
    pub fn new(value: T, guard: impl AsRef<CollectionGuard>) -> Self {
        let guard = guard.as_ref();
        let (gen, bin) = guard.adopt(RefCounted::strong(value));
        Self::from_parts(gen, bin, guard)
    }

    /// Returns a "weak" reference to this root.
    #[must_use]
    pub const fn downgrade(&self) -> Ref<T> {
        self.reference
    }

    /// Returns an untyped "weak" reference erased to this root.
    #[must_use]
    pub fn downgrade_any(&self) -> AnyRef {
        self.reference.as_any()
    }

    fn ref_counted(&self) -> &RefCounted<T> {
        // SAFETY: The garbage collector will not collect data while we have a
        // strong count. The returned lifetime of the data is tied to `self`,
        // which ensures the returned lifetime is valid only for as long as this
        // `Root<T>` is alive.
        unsafe { &(*self.data) }
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
        &self.ref_counted().value
    }
}

impl<T> Drop for Root<T>
where
    T: Collectable,
{
    fn drop(&mut self) {
        if self.ref_counted().strong.fetch_sub(1, Ordering::Acquire) == 1 {
            CollectorCommand::schedule_collect_if_needed();
        }
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
pub struct Ref<T> {
    creating_thread: CollectorThreadId,
    slot_generation: u32,
    bin_id: BinId,
    _t: PhantomData<fn(&T)>,
}

impl<T> Ref<T>
where
    T: Collectable,
{
    /// Stores `value` in the garbage collector, returning a "weak" reference to
    /// it.
    pub fn new(value: T, guard: impl AsRef<CollectionGuard>) -> Self {
        let guard = guard.as_ref();
        let (slot_generation, bin_id) = guard.adopt(RefCounted::weak(value));

        Self {
            creating_thread: guard.thread.thread_id,
            slot_generation,
            bin_id,
            _t: PhantomData,
        }
    }

    /// Returns this reference as an untyped reference.
    #[must_use]
    pub fn as_any(self) -> AnyRef {
        AnyRef {
            type_id: TypeId::of::<T>(),
            creating_thread: self.creating_thread,
            slot_generation: self.slot_generation,
            bin_id: self.bin_id,
        }
    }

    fn load_slot_from<'guard>(
        &self,
        bins: &Map<TypeId, Arc<dyn AnyBin>>,
        guard: &'guard CollectionGuard,
    ) -> Option<&'guard RefCounted<T>> {
        let type_id = TypeId::of::<T>();
        bins.get(&type_id)
            .assert("areas are never deallocated")
            .as_any()
            .downcast_ref::<Bin<T>>()
            .assert("type mismatch")
            .load(self.bin_id, self.slot_generation, guard)
    }

    fn load_slot<'guard>(&self, guard: &'guard CollectionGuard) -> Option<&'guard RefCounted<T>> {
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

    /// Loads a reference to the underlying data. Returns `None` if the data has
    /// been collected and is no longer available.
    ///
    /// # Errors
    ///
    /// Returns `CollectionStarting` if `self` was created in another thread and
    /// that thread is currently locked by the garbage collector.
    #[must_use]
    pub fn load<'guard>(&self, guard: &'guard CollectionGuard) -> Option<&'guard T> {
        self.load_slot(guard).map(|allocated| &allocated.value)
    }

    /// Loads a root reference to the underlying data. Returns `None` if the
    /// data has been collected and is no longer available.
    ///
    /// # Errors
    ///
    /// Returns `CollectionStarting` if `self` was created in another thread and
    /// that thread is currently locked by the garbage collector.
    #[must_use]
    pub fn as_root(&self, guard: &CollectionGuard) -> Option<Root<T>> {
        self.load_slot(guard).map(|allocated| {
            allocated.strong.fetch_add(1, Ordering::Acquire);
            Root {
                data: allocated,
                reference: *self,
            }
        })
    }

    /// Returns true if these two references point to the same underlying
    /// allocation.
    #[must_use]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        this.bin_id == other.bin_id
            && this.creating_thread == other.creating_thread
            && this.slot_generation == other.slot_generation
    }
}

impl<T> Clone for Ref<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Ref<T> {}

// SAFETY: Ref<T>'s usage of a pointer prevents auto implementation.
// `Collectable` requires `Send`, and `Ref<T>` ensures proper Send + Sync
// behavior in its memory accesses.
unsafe impl<T> Send for Ref<T> where T: Collectable {}
// SAFETY: Ref<T>'s usage of a pointer prevents auto implementation.
// `Collectable` requires `Send`, and `Ref<T>` ensures proper Send + Sync
// behavior in its memory accesses.
unsafe impl<T> Sync for Ref<T> where T: Collectable {}

/// A lock has been established by the collector on data needed to resolve a
/// reference.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct CollectionStarting;

#[derive(Default)]
struct Bins {
    by_type: RwLock<Map<TypeId, Arc<dyn AnyBin>>>,
}

impl Bins {
    /// # Safety
    ///
    /// This function must only be called when `bin_id` is known to be
    /// allocated.
    unsafe fn allocated_slot_pointer<T>(&self, bin_id: BinId) -> *const RefCounted<T>
    where
        T: Collectable,
    {
        let by_type = self.by_type.read();
        let slot = &by_type
            .get(&TypeId::of::<T>())
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

    fn adopt<T>(value: RefCounted<T>, bins_guard: &CollectionGuard) -> (u32, BinId)
    where
        T: Collectable,
    {
        let type_id = TypeId::of::<T>();
        let mut by_type = bins_guard.bins().by_type.upgradable_read();
        if let Some(bin) = by_type.get(&type_id) {
            let (gen, bin) = bin
                .as_any()
                .downcast_ref::<Bin<T>>()
                .expect("type mismatch")
                .adopt(value);
            (gen, bin)
        } else {
            by_type.with_upgraded(|by_type| {
                // We don't need to check for another thread allocating, because the
                // only thread that can allocate is the local thread. We needed a
                // write guard, however, because other threads could be trying to
                // load data this thread allocated.
                let bin = Bin::new(value);
                by_type.insert(type_id, Arc::new(bin));
                (0, BinId::first())
            })
        }
    }
}

struct Bin<T>
where
    T: Collectable,
{
    free_head: AtomicU32,
    slabs: Slabs<T>,
    slabs_tail: Cell<Option<*const Slabs<T>>>,
    mapper: Mapper<T::Target>,
}

impl<T> Bin<T>
where
    T: Collectable,
{
    fn new(first_value: RefCounted<T>) -> Self {
        Self {
            free_head: AtomicU32::new(0),
            slabs: Slabs::new(first_value, 0),
            slabs_tail: Cell::new(None),
            mapper: Mapper(Box::new(MappingFunction::<T>::new())),
        }
    }

    fn adopt(&self, value: RefCounted<T>) -> (u32, BinId) {
        loop {
            let bin_id = BinId(self.free_head.load(Ordering::Acquire));
            if bin_id.invalid() {
                break;
            }
            let slab = &self.slabs[bin_id.slab() as usize];
            let slot_index = bin_id.slot();
            let slot = &slab.slots[usize::from(slot_index)];
            if let Some(generation) = slot.state.try_allocate() {
                // SAFETY: Unallocated slots are only accessed through the
                // current local thread while a guard is held, which must be
                // true for this function to be invoked. try_allocate ensures
                // this slot wasn't previously allocated, making it safe for us
                // to initialize the data with `value`.
                let next = unsafe {
                    let next = (*slot.value.get()).free;
                    slot.value.get().write(SlotData {
                        allocated: ManuallyDrop::new(value),
                    });
                    next
                };
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
        let (generation, bin_id, new_tail) = tail.adopt(value);

        if new_tail.is_some() {
            self.slabs_tail.set(new_tail);
        }
        (generation, bin_id)
    }

    fn load<'guard>(
        &self,
        bin_id: BinId,
        slot_generation: u32,
        _guard: &'guard CollectionGuard,
    ) -> Option<&'guard RefCounted<T>> {
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
                let strong_count =
                    unsafe { (*slot.value.get()).allocated.strong.load(Ordering::Relaxed) };
                if strong_count > 0 {
                    tracer.mark::<T>(Ref {
                        creating_thread: tracer.tracing_thread,
                        slot_generation,
                        bin_id: BinId::new(slab_index.cast::<u32>(), index.cast::<u8>()),
                        _t: PhantomData,
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
        slot.state.mark(mark_bits, slot_generation)
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
        guard: &'guard CollectionGuard,
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
        guard: &'guard CollectionGuard,
    ) -> Option<&'guard C::Target> {
        let ref_counted = bin
            .as_any()
            .downcast_ref::<Bin<C>>()
            .expect("type mismatch")
            .load(id, slot_generation, guard)?;

        Some(ref_counted.value.map_as())
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
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
    fn new(initial_value: RefCounted<T>, offset: usize) -> Self {
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

    fn adopt(&self, mut value: RefCounted<T>) -> (u32, BinId, Option<*const Slabs<T>>) {
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
    fn new(first_value: RefCounted<T>) -> Box<Self>
    where
        T: Collectable,
    {
        // SAFETY: `Slot<T>` only utilizes types that zero-initialized data is a
        // valid representation.
        let mut this: Box<Self> =
            unsafe { Box::from_raw(alloc_zeroed(Layout::new::<Self>()).cast()) };
        this.slots[0] = Slot {
            state: SlotState::new_allocated(T::MAY_CONTAIN_REFERENCES),
            value: UnsafeCell::new(SlotData {
                allocated: ManuallyDrop::new(first_value),
            }),
        };
        this
    }

    fn try_adopt(
        &self,
        value: RefCounted<T>,
        slab_index: usize,
    ) -> Result<(u32, BinId), RefCounted<T>> {
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
    allocated: ManuallyDrop<RefCounted<T>>,
    free: u32,
}

struct Slot<T> {
    state: SlotState,
    value: UnsafeCell<SlotData<T>>,
}

impl<T> Slot<T> {
    fn allocate(&self, value: RefCounted<T>) -> u32 {
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

struct RefCounted<T> {
    strong: AtomicU64,
    value: T,
}

impl<T> RefCounted<T> {
    fn weak(value: T) -> Self {
        Self {
            strong: AtomicU64::new(0),
            value,
        }
    }

    fn strong(value: T) -> Self {
        Self {
            strong: AtomicU64::new(1),
            value,
        }
    }
}

struct SlotState(AtomicU64);

impl SlotState {
    const ALLOCATED: u64 = 1 << 33;
    const MARK_OFFSET: u32 = 35;
    const SHOULD_TRACE: u64 = 1 << 34;

    const fn new_allocated(should_trace: bool) -> Self {
        if should_trace {
            Self(AtomicU64::new(Self::ALLOCATED | Self::SHOULD_TRACE))
        } else {
            Self(AtomicU64::new(Self::ALLOCATED))
        }
    }

    fn generation(&self) -> Option<u32> {
        let state = self.0.load(Ordering::Relaxed);
        (state & Self::ALLOCATED != 0).then(|| state.cast::<u32>())
    }

    fn allocated_with_generation(&self, generation: u32) -> bool {
        let state = self.0.load(Ordering::Relaxed);
        state & Self::ALLOCATED != 0 && state.cast::<u32>() == generation
    }

    fn try_allocate(&self) -> Option<u32> {
        let mut new_generation = None;
        if self
            .0
            .fetch_update(Ordering::Release, Ordering::Acquire, |state| {
                (state & Self::ALLOCATED == 0).then(|| {
                    let generation = state.cast::<u32>().wrapping_add(1);
                    new_generation = Some(generation);
                    Self::ALLOCATED | u64::from(generation)
                })
            })
            .is_ok()
        {
            new_generation
        } else {
            None
        }
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
        state & Self::SHOULD_TRACE != 0
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
/// This function will deadlock if any [`CollectionGuard`]s are held by the
/// current thread when invoked. If a guard is held, consider calling
/// [`CollectionGuard::collect()`] instead.
pub fn collect() {
    let now = Instant::now();
    CollectorCommand::Collect(now).send();
    GlobalCollector::get().info.wait_for_collection(now);
}

/// A type-erased garbage collected reference.
pub struct AnyRef {
    type_id: TypeId,
    creating_thread: CollectorThreadId,
    slot_generation: u32,
    bin_id: BinId,
}

impl AnyRef {
    /// Returns a [`Ref<T>`] if the underlying reference points to a `T`.
    #[must_use]
    pub fn downcast_ref<T>(&self) -> Option<Ref<T>>
    where
        T: Collectable,
    {
        (TypeId::of::<T>() == self.type_id).then_some(Ref {
            creating_thread: self.creating_thread,
            slot_generation: self.slot_generation,
            bin_id: self.bin_id,
            _t: PhantomData,
        })
    }

    /// Returns a [`Strong<T>`] if the underlying reference points to a `T` that
    /// has not been collected.
    #[must_use]
    pub fn downcast_root<T>(&self, guard: &CollectionGuard) -> Option<Root<T>>
    where
        T: Collectable,
    {
        self.downcast_ref().and_then(|r| r.as_root(guard))
    }

    /// Returns a reference to the result of [`MapAs::map_as()`], if the value
    /// has not been collected and [`MapAs::Target`] is `T`.
    pub fn load_mapped<'guard, T>(&self, guard: &'guard CollectionGuard) -> Option<&'guard T>
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
        bins: &Map<TypeId, Arc<dyn AnyBin>>,
        guard: &'guard CollectionGuard,
    ) -> Option<&'guard T>
    where
        T: ?Sized + 'static,
    {
        let bins = bins
            .get(&self.type_id)
            .assert("areas are never deallocated");

        bins.mapper().downcast_ref::<Mapper<T>>()?.0.load_mapped(
            self.bin_id,
            self.slot_generation,
            &**bins,
            guard,
        )
    }
}
