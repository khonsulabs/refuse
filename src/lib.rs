use core::slice;
use std::alloc::{alloc_zeroed, Layout};
use std::any::{Any, TypeId};
use std::cell::{OnceCell, RefCell, UnsafeCell};
use std::marker::PhantomData;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::sync::atomic::{self, AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use std::{array, thread};

use ahash::AHashMap;
use flume::{Receiver, RecvError, RecvTimeoutError, Sender};
use kempt::Map;
use nanorand::{Rng, WyRand};
use parking_lot::lock_api::ArcRwLockReadGuard;
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
    NewThread(CollectorThreadId, Arc<RwLock<Bins>>),
    ThreadShutdown(CollectorThreadId),
    Collect(Instant),
    ScheduleCollect,
}

impl CollectorCommand {
    fn send(self) {
        GlobalCollector::get()
            .sender
            .send(self)
            .expect("collector not running")
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

struct ThreadBins {
    alive: bool,
    bins: Arc<RwLock<Bins>>,
}

struct CollectorInfo {
    info: Mutex<CollectorInfoData>,
    sync: Condvar,
    signalled_collector: AtomicBool,
}

impl CollectorInfo {
    fn wait_for_collection(&self, requested_at: Instant) {
        let mut info = self.info.lock();
        while info.last_run < requested_at {
            self.sync.wait(&mut info);
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
    rng: WyRand,
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
            rng: WyRand::new(),
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

    fn run(mut self) {
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
                            ))
                        }
                    }
                });
            }

            loop {
                let command = match self.next_command() {
                    Ok(Some(command)) => command,
                    Ok(None) => {
                        self.collect_and_notify(&channels);
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
                            self.collect_and_notify(&channels);
                        }
                    }
                    CollectorCommand::ScheduleCollect => {
                        self.schedule_gc(Instant::now() + Duration::from_millis(10))
                    }
                }
            }

            // Dropping channels ensures the worker threads shut down, allowing
            // full cleanup. Because of thread local storage limitations, this
            // may never execute on some platforms.
            drop(channels);
        })
    }

    fn collect_and_notify(&mut self, channels: &CollectorThreadChannels) {
        self.next_gc = None;
        let gc_pause = match self.collect(channels) {
            CollectResult::Ok => {
                if self.thread_bins.is_empty() {
                    None
                } else {
                    Some(Duration::from_millis(50))
                }
            }
            CollectResult::CouldntRun => {
                self.pause_failures += 1;
                Some(Duration::from_millis(1))
            }
        };
        let now = Instant::now();
        if let Some(pause) = gc_pause {
            self.schedule_gc(now + pause);
        }
        let mut info = self.shared.info.lock();
        info.last_run = now;
        drop(info);
        self.shared.sync.notify_all();
        self.shared
            .signalled_collector
            .store(false, Ordering::Relaxed);
    }

    fn collect(&mut self, threads: &CollectorThreadChannels) -> CollectResult {
        self.mark_bits = self.mark_bits.wrapping_add(1);
        if self.mark_bits == 0 {
            self.mark_bits = 1;
        }

        let mut all_bins = AHashMap::new();
        let mut now = Instant::now();
        let long_lock_deadline = now + Duration::from_millis(10);
        let (force_gc, mut milli_wait) = if self.pause_failures >= 5 {
            eprintln!("Forcing garbage collection");
            (true, 5)
        } else {
            (false, 2)
        };

        let mut bins_to_lock = self.thread_bins.keys().copied().collect::<Vec<_>>();
        while !bins_to_lock.is_empty() {
            let lock_deadline = now + Duration::from_millis(milli_wait);
            let lock_deadline = if force_gc {
                lock_deadline
            } else if long_lock_deadline < now {
                break;
            } else {
                long_lock_deadline.min(lock_deadline)
            };
            let mut bin = bins_to_lock.len() - 1;
            loop {
                let thread = bins_to_lock[bin];

                if let Some(locked) = self.thread_bins[&thread]
                    .bins
                    .try_write_until(lock_deadline)
                {
                    all_bins.insert(thread, locked);
                    bins_to_lock.remove(bin);
                    if bin > 0 {
                        bin -= 1;
                    } else {
                        break;
                    }
                } else {
                    // Timeout
                    break;
                }
            }
            if !bins_to_lock.is_empty() {
                now = Instant::now();
                milli_wait *= 2;
                self.rng.shuffle(&mut bins_to_lock);
            }
        }

        if !bins_to_lock.is_empty() {
            drop(all_bins);
            return CollectResult::CouldntRun;
        }

        self.pause_failures = 0;

        let (mark_one_sender, mark_ones) = flume::bounded(1024);
        let mark_one_sender = Arc::new(mark_one_sender);

        for (id, bins) in &mut all_bins {
            for i in 0..bins.by_type.len() {
                threads
                    .tracer
                    .send(TraceRequest {
                        thread: *id,
                        bins: bins.by_type.field(i).expect("length checked").value.clone(),
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
        for (key, mut bins) in all_bins.drain() {
            let mut live_objects = 0usize;
            for bin in bins.by_type.values_mut() {
                live_objects = live_objects.saturating_add(bin.sweep(self.mark_bits));
            }
            if live_objects == 0 {
                threads_to_remove.push(key);
            }
        }

        for thread_id in threads_to_remove {
            all_bins.remove(&thread_id);
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
            let info = Arc::new(CollectorInfo {
                info: Mutex::new(CollectorInfoData {
                    last_run: Instant::now(),
                }),
                sync: Condvar::new(),
                signalled_collector: AtomicBool::new(false),
            });
            thread::Builder::new()
                .name(String::from("collector"))
                .spawn({
                    let info = info.clone();
                    move || Collector::new(receiver, info).run()
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

#[derive(Clone)]
struct ThreadLocalBins {
    bins: Arc<RwLock<Bins>>,
    thread_id: CollectorThreadId,
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

pub fn collected<R>(wrapped: impl FnOnce() -> R) -> R {
    // We initialize the global collector by invoking get.
    GlobalCollector::get();
    THREAD_BINS.with_borrow(|lock| {
        lock.get_or_init(|| {
            let bins = Arc::new(RwLock::new(Bins::default()));
            let thread_id = CollectorThreadId::unique();
            CollectorCommand::NewThread(thread_id, bins.clone()).send();
            ThreadLocalBins { bins, thread_id }
        });
        wrapped()
    })
}

type ArcRwBinGuard = ArcRwLockReadGuard<parking_lot::RawRwLock, Bins>;
pub struct CollectionGuard {
    thread: ThreadLocalBins,
    inner: MaybeUninit<ArcRwBinGuard>,
}

impl CollectionGuard {
    pub fn collect(&mut self) {
        let guard = unsafe { self.inner.assume_init_mut() };
        ArcRwBinGuard::unlocked(guard, collect);
    }

    pub fn yield_to_collector(&mut self) {
        let guard = unsafe { self.inner.assume_init_mut() };
        ArcRwBinGuard::bump(guard)
    }

    fn bins(&self) -> &Bins {
        unsafe { self.inner.assume_init_ref() }
    }
}

impl CollectionGuard {
    pub fn acquire() -> Self {
        let thread = ThreadLocalBins::get();
        Self {
            inner: MaybeUninit::new(thread.bins.read_arc()),
            thread,
        }
    }
}

impl Drop for CollectionGuard {
    fn drop(&mut self) {
        unsafe { self.inner.assume_init_drop() };
    }
}

pub trait Collectable: Send + Sync + 'static {
    const MAY_CONTAIN_REFERENCES: bool;

    fn trace(&self, tracer: &mut Tracer);
}

pub trait ContainsNoCollectables {}

impl<T> Collectable for T
where
    T: ContainsNoCollectables + Send + Sync + 'static,
{
    const MAY_CONTAIN_REFERENCES: bool = false;

    fn trace(&self, _tracer: &mut Tracer) {}
}

impl ContainsNoCollectables for u8 {}
impl ContainsNoCollectables for u16 {}
impl ContainsNoCollectables for u32 {}
impl ContainsNoCollectables for u64 {}
impl ContainsNoCollectables for u128 {}
impl ContainsNoCollectables for usize {}
impl ContainsNoCollectables for i8 {}
impl ContainsNoCollectables for i16 {}
impl ContainsNoCollectables for i32 {}
impl ContainsNoCollectables for i64 {}
impl ContainsNoCollectables for i128 {}
impl ContainsNoCollectables for isize {}

impl<T> Collectable for Vec<T>
where
    T: Collectable,
{
    const MAY_CONTAIN_REFERENCES: bool = T::MAY_CONTAIN_REFERENCES;

    fn trace(&self, tracer: &mut Tracer) {
        for item in self {
            item.trace(tracer);
        }
    }
}
impl<T, const N: usize> Collectable for [T; N]
where
    T: Collectable,
{
    const MAY_CONTAIN_REFERENCES: bool = T::MAY_CONTAIN_REFERENCES;

    fn trace(&self, tracer: &mut Tracer) {
        for item in self {
            item.trace(tracer);
        }
    }
}

impl<T> Collectable for Strong<T>
where
    T: Collectable,
{
    const MAY_CONTAIN_REFERENCES: bool = T::MAY_CONTAIN_REFERENCES;

    fn trace(&self, _tracer: &mut Tracer) {
        // Strong<T> is already a root, thus calling trace on a Strong<T> has no
        // effect.
    }
}

impl<T> Collectable for Weak<T>
where
    T: Collectable,
{
    const MAY_CONTAIN_REFERENCES: bool = T::MAY_CONTAIN_REFERENCES;

    fn trace(&self, _tracer: &mut Tracer) {
        // The only way for Weak<T>::trace to be invoked is by having been
        // marked already. Thus, marking this weak would just generate extra
        // traffic.
    }
}

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

    pub fn mark<T>(&mut self, collected: Weak<T>)
    where
        T: Collectable,
    {
        self.mark_one_sender
            .send(MarkRequest {
                thread: collected.creating_thread,
                type_id: TypeId::of::<T>(),
                slot_generation: collected.slot_generation,
                bin_id: collected.bin_id,
                mark_bits: self.mark_bit,
                mark_one_sender: self.mark_one_sender.clone(),
            })
            .expect("marker thread not running");
    }
}

#[test]
fn size_of_types() {
    assert_eq!(std::mem::size_of::<Strong<u32>>(), 24);
    assert_eq!(std::mem::size_of::<Weak<u32>>(), 16);
}

pub struct Strong<T>
where
    T: Collectable,
{
    data: *const RefCounted<T>,
    creating_thread: CollectorThreadId,
    slot_generation: u32,
    bin_id: BinId,
}

impl<T> Strong<T>
where
    T: Collectable,
{
    fn from_parts(slot_generation: u32, bin_id: BinId, guard: &mut CollectionGuard) -> Self {
        let data = guard.bins().slot_pointer::<T>(bin_id);
        Self {
            data,
            creating_thread: guard.thread.thread_id,
            slot_generation,
            bin_id,
        }
    }

    pub fn new(value: T, guard: &mut CollectionGuard) -> Self {
        let (gen, bin) = unsafe {
            let (gen, bin, returned_guard) =
                Bins::adopt(RefCounted::strong(value), guard.inner.assume_init_read());
            guard
                .inner
                .write(returned_guard.unwrap_or_else(|| guard.thread.bins.read_arc()));
            (gen, bin)
        };
        Self::from_parts(gen, bin, guard)
    }

    pub const fn downgrade(&self) -> Weak<T> {
        Weak {
            creating_thread: self.creating_thread,
            slot_generation: self.slot_generation,
            bin_id: self.bin_id,
            _t: PhantomData,
        }
    }

    fn ref_counted(&self) -> &RefCounted<T> {
        unsafe { &(*self.data) }
    }
}

unsafe impl<T> Send for Strong<T> where T: Collectable {}
unsafe impl<T> Sync for Strong<T> where T: Collectable {}

impl<T> Deref for Strong<T>
where
    T: Collectable,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.ref_counted().value
    }
}

impl<T> Drop for Strong<T>
where
    T: Collectable,
{
    fn drop(&mut self) {
        if self.ref_counted().strong.fetch_sub(1, Ordering::Acquire) == 1 {
            CollectorCommand::schedule_collect_if_needed()
        }
    }
}

pub struct Weak<T> {
    creating_thread: CollectorThreadId,
    slot_generation: u32,
    bin_id: BinId,
    _t: PhantomData<fn(&T)>,
}

impl<T> Weak<T>
where
    T: Collectable,
{
    pub fn new(value: T, guard: &mut CollectionGuard) -> Self {
        let (slot_generation, bin_id) = unsafe {
            let (slot_generation, bin_id, returned_guard) =
                Bins::adopt(RefCounted::weak(value), guard.inner.assume_init_read());
            guard
                .inner
                .write(returned_guard.unwrap_or_else(|| guard.thread.bins.read_arc()));
            (slot_generation, bin_id)
        };

        Self {
            creating_thread: guard.thread.thread_id,
            slot_generation,
            bin_id,
            _t: PhantomData,
        }
    }

    pub fn load<'guard>(&self, guard: &'guard CollectionGuard) -> Option<&'guard T> {
        let type_id = TypeId::of::<T>();
        let slabs = &guard
            .bins()
            .by_type
            .get(&type_id)
            .expect("areas are never deallocated")
            .as_any()
            .downcast_ref::<Bin<T>>()
            .expect("type mismatch")
            .slabs;
        let slab = slabs.get(self.bin_id.slab() as usize)?;
        let slot = &slab.slots[usize::from(self.bin_id.slot())];
        slot.state
            .allocated_with_generation(self.slot_generation)
            .then_some(unsafe { &(*slot.value.get()).allocated.value })
    }
}

impl<T> Clone for Weak<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Weak<T> {}

#[derive(Default)]
struct Bins {
    by_type: Map<TypeId, Arc<dyn AnyBin>>,
}

impl Bins {
    fn slot_pointer<T>(&self, bin_id: BinId) -> *const RefCounted<T>
    where
        T: Collectable,
    {
        unsafe {
            &*(*self
                .by_type
                .get(&TypeId::of::<T>())
                .expect("areas are never deallocated")
                .as_any()
                .downcast_ref::<Bin<T>>()
                .expect("type mismatch")
                .slabs[bin_id.slab() as usize]
                .slots[usize::from(bin_id.slot())]
            .value
            .get())
            .allocated
        }
    }

    fn adopt<T>(
        value: RefCounted<T>,
        bins_guard: ArcRwBinGuard,
    ) -> (u32, BinId, Option<ArcRwBinGuard>)
    where
        T: Collectable,
    {
        let type_id = TypeId::of::<T>();
        if let Some(bin) = bins_guard.by_type.get(&type_id) {
            let (gen, bin) = bin
                .as_any()
                .downcast_ref::<Bin<T>>()
                .expect("type mismatch")
                .adopt(value);
            (gen, bin, Some(bins_guard))
        } else {
            drop(bins_guard);
            let thread_local = ThreadLocalBins::get();
            let mut bins = thread_local.bins.write(); // TODO check for creation
            let bin = Bin::new(value);
            bins.by_type.insert(type_id, Arc::new(bin));
            (0, BinId::first(), None)
        }
    }
}

struct Bin<T> {
    free_head: AtomicU32,
    slabs: Slabs<T>,
    first_free_slab: AtomicUsize,
}

impl<T> Bin<T>
where
    T: Collectable,
{
    fn new(first_value: RefCounted<T>) -> Self {
        Self {
            free_head: AtomicU32::new(0),
            slabs: Slabs::new(first_value),
            first_free_slab: AtomicUsize::new(0),
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

        let first_free_slab = self.first_free_slab.load(Ordering::Relaxed);
        let (generation, bin_id) = self.slabs.adopt(value, first_free_slab);
        let allocated_in_slab = bin_id.slab() as usize;
        if first_free_slab < allocated_in_slab {
            let _result = self.first_free_slab.compare_exchange(
                first_free_slab,
                allocated_in_slab,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
        (generation, bin_id)
    }
}

trait AnyBin: Send + Sync {
    fn trace(&self, tracer: &mut Tracer<'_>);
    fn trace_one(&self, slot_generation: u32, bin: BinId, tracer: &mut Tracer<'_>);
    fn mark_one(&self, mark_bits: u8, slot_generation: u32, bin: BinId) -> bool;
    fn sweep(&self, mark_bits: u8) -> usize;
    fn as_any(&self) -> &dyn Any;
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
                let strong_count =
                    unsafe { (*slot.value.get()).allocated.strong.load(Ordering::Relaxed) };
                if strong_count > 0 {
                    tracer.mark::<T>(Weak {
                        creating_thread: tracer.tracing_thread,
                        slot_generation,
                        bin_id: BinId::new(slab_index as u32, index as u8),
                        _t: PhantomData,
                    });
                }
            }
        }
    }

    fn trace_one(&self, slot_generation: u32, bin: BinId, tracer: &mut Tracer) {
        let slot = &self.slabs[bin.slab() as usize].slots[usize::from(bin.slot())];
        if slot.state.generation() == Some(slot_generation) {
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
                        free_head = BinId::new(slab_index as u32, slot_index as u8);
                    }
                    SlotSweepStatus::Allocated => {
                        allocated += 1;
                        last_allocated = slot_index as u8;
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

    const fn slot(self) -> u8 {
        self.0 as u8
    }
}

struct Slabs<T> {
    slabs: [UnsafeCell<Option<Box<Slab<T>>>>; 256],
    next: UnsafeCell<Option<Box<Slabs<T>>>>,
}

impl<T> Slabs<T>
where
    T: Collectable,
{
    fn new(initial_value: RefCounted<T>) -> Self {
        let mut initial_value = Some(initial_value);
        Self {
            slabs: array::from_fn(|index| {
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
            unsafe { (*self.slabs[index].get()).as_ref() }.map(|slab| &**slab)
        } else {
            unsafe { (*self.next.get()).as_ref() }.and_then(|slabs| slabs.get(index - 256))
        }
    }

    fn adopt(&self, value: RefCounted<T>, first_free_slab: usize) -> (u32, BinId) {
        fn adopt_inner<T>(
            this: &Slabs<T>,
            mut value: RefCounted<T>,
            first_free_slab: usize,
            slab_offset: usize,
        ) -> (u32, BinId)
        where
            T: Collectable,
        {
            if first_free_slab < 256 {
                let mut slab_index = first_free_slab;
                while let Some(slab) = this
                    .slabs
                    .get(slab_index)
                    .and_then(|slab| unsafe { (*slab.get()).as_ref() })
                {
                    match slab.try_adopt(value, slab_index + slab_offset) {
                        Ok(result) => return result,
                        Err(returned) => value = returned,
                    }
                    slab_index += 1;
                }

                for index in slab_index..256 {
                    let slab = unsafe { &mut (*this.slabs[index].get()) };

                    if let Some(slab) = slab {
                        match slab.try_adopt(value, index + slab_offset) {
                            Ok(result) => return result,
                            Err(returned) => value = returned,
                        }
                    } else {
                        *slab = Some(Slab::new(value));
                        return (0, BinId::new(index as u32, 0));
                    }
                }

                if let Some(next) = unsafe { &*this.next.get() } {
                    adopt_inner(next, value, 0, slab_offset + 256)
                } else {
                    unsafe { this.next.get().write(Some(Box::new(Slabs::new(value)))) };
                    (0, BinId::new(slab_offset as u32 + 256, 0))
                }
            } else {
                let next_slab = unsafe { (*this.next.get()).as_ref() }.expect("invalid first_free");
                adopt_inner(next_slab, value, first_free_slab - 256, slab_offset + 256)
                // let next_slab = self.next.get_or_init(|| Slab::new(value.take().expect("invoked only once")));
            }
        }

        adopt_inner(self, value, first_free_slab, 0)
        // match last_slab.try_adopt(value, slabs.len() - 1) {
        //     Ok(result) => result,
        //     Err(value) => {
        //         drop(slabs);
        //         let mut slabs = self.slabs.write().expect("poisoned");
        //         // Between the lock reaquisition, another thread could have
        //         // allocated a new slab. Check to see if we can allocate
        //         // within it rather than allocating another slab.
        //         let last_slab = slabs.last().expect("never empty");
        //         match last_slab.try_adopt(value, slabs.len() - 1) {
        //             Ok(result) => result,
        //             Err(value) => {
        //                 let slab_index = slabs.len();
        //                 slabs.push(Box::new(Slab::new(value)));
        //                 (0, BinId::new(slab_index as u32, 0))
        //             }
        //         }
        //     }
        // }
    }

    fn iter(&self) -> SlabsIter<'_, T> {
        SlabsIter {
            slabs: self.slabs.iter(),
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
                let Some(slab) = (unsafe { (*slab.get()).as_ref() }) else {
                    continue;
                };
                return Some(slab);
            }
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

            Ok((generation, BinId::new(slab_index as u32, slot_index)))
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

unsafe impl<T> Send for Slabs<T> where T: Send {}
unsafe impl<T> Sync for Slabs<T> where T: Sync {}

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
        (state & Self::ALLOCATED != 0).then_some(state as u32)
    }

    fn allocated_with_generation(&self, generation: u32) -> bool {
        let state = self.0.load(Ordering::Relaxed);
        state & Self::ALLOCATED != 0 && state as u32 == generation
    }

    fn try_allocate(&self) -> Option<u32> {
        let mut new_generation = None;
        if self
            .0
            .fetch_update(Ordering::Release, Ordering::Acquire, |state| {
                (state & Self::ALLOCATED == 0).then(|| {
                    let generation = (state as u32).wrapping_add(1);
                    new_generation = Some(generation);
                    Self::ALLOCATED | generation as u64
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

        let generation = (state as u32).wrapping_add(1);

        self.0
            .store(Self::ALLOCATED | generation as u64, Ordering::Release);
        generation
    }

    fn mark(&self, mark_bits: u8, slot_generation: u32) -> bool {
        let mut state = self.0.load(Ordering::Acquire);
        if state & Self::ALLOCATED == 0 || state as u32 != slot_generation {
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

        let generation = state as u32;
        self.0.store(generation as u64, Ordering::Release);
        SlotSweepStatus::Swept
    }
}

#[derive(Clone, Copy)]
enum SlotSweepStatus {
    Allocated,
    NotAllocated,
    Swept,
}

pub fn collect() {
    let now = Instant::now();
    CollectorCommand::Collect(now).send();
    GlobalCollector::get().info.wait_for_collection(now);
}

#[test]
fn weak_lifecycle() {
    collected(|| {
        let mut guard = CollectionGuard::acquire();
        let collected = Weak::new(42_u32, &mut guard);

        assert_eq!(collected.load(&guard).cloned(), Some(42));
        drop(guard);

        collect();

        let guard = CollectionGuard::acquire();
        assert!(collected.load(&guard).is_none());
    })
}

#[test]
fn strong_lifecycle() {
    collected(|| {
        let mut guard = CollectionGuard::acquire();
        let strong = Strong::new(42_u32, &mut guard);
        let weak = strong.downgrade();

        assert_eq!(weak.load(&guard).cloned(), Some(42));

        // This collection should not remove anything.
        guard.collect();
        assert_eq!(weak.load(&guard).cloned(), Some(42));

        // Drop the strong reference
        drop(strong);

        // Now collection should remove the value.
        guard.collect();

        assert!(weak.load(&guard).is_none());
    })
}
