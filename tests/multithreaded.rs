use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use flume::{Receiver, Sender};
use musegc::{collected, Collectable, CollectionGuard, ContainsNoCollectables, Ref, Root};
use parking_lot::Mutex;

const WORK_ITERS: usize = 100;
const WORK_ITEMS: usize = 100;

#[test]
fn round_robin() {
    let threads = thread::available_parallelism()
        .map_or(1, NonZeroUsize::get)
        .max(2);
    let channels = (0..threads).map(|_| flume::unbounded()).collect::<Vec<_>>();
    let outstanding = OutstandingWork::default();

    for (index, (_, receiver)) in channels.iter().enumerate() {
        let next = if let Some(next) = channels.get(index + 1) {
            next
        } else {
            &channels[0]
        };
        let next_sender = next.0.clone();
        let receiver = receiver.clone();
        let outstanding = outstanding.clone();
        thread::spawn(move || {
            collected(move || thread_worker(&receiver, &next_sender, &outstanding));
        });
    }

    collected(|| {
        let mut guard = CollectionGuard::acquire();
        for i in 0..WORK_ITEMS {
            channels[i % channels.len()]
                .0
                .send(Command::Enqueue(Root::new(WorkUnit::default(), &mut guard)))
                .expect("worker disconnected early");
        }
        drop(guard);
    });

    for i in 0..(WORK_ITEMS * WORK_ITERS) {
        channels[i % channels.len()]
            .0
            .send(Command::Work)
            .expect("worker disconnected early");
    }

    while outstanding.count() > 0 {
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn thread_worker(
    task_receiver: &Receiver<Command>,
    next_thread: &Sender<Command>,
    outstanding: &OutstandingWork,
) {
    let mut guard = CollectionGuard::acquire();
    let state = Root::new(Worker::default(), &mut guard);
    drop(guard);
    while let Ok(work) = task_receiver.recv() {
        match work {
            Command::Enqueue(work) => {
                // We need to ensure we have a guard for this downgrade, because
                // if the collector is currently running and already scanned
                // `queue`, `work` could be missed.
                let _guard = CollectionGuard::acquire();
                let mut queue = state.queue.lock();
                queue.items.push(work.downgrade());
            }
            Command::Work => {
                let guard = CollectionGuard::acquire();
                let mut queue = state.queue.lock();
                if queue.items.is_empty() {
                    next_thread
                        .send(Command::Work)
                        .expect("next thread disconnted");
                } else {
                    let work = match queue.items[0].as_root(&guard) {
                        Some(work) => {
                            queue.items.remove(0);
                            work
                        }
                        None => unreachable!("missing work"),
                    };
                    if work.counter.fetch_sub(1, Ordering::Acquire) > 1 {
                        next_thread
                            .send(Command::Enqueue(work))
                            .expect("next thread disconnected");
                    } else {
                        outstanding.complete_one();
                        if outstanding.count() == 0 {
                            break;
                        }
                    }
                }
            }
        }
    }

    println!("Thread completed");
}

enum Command {
    Enqueue(Root<WorkUnit>),
    Work,
}

#[derive(Clone, Default)]
struct OutstandingWork(Arc<AtomicUsize>);

impl OutstandingWork {
    fn count(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }

    fn complete_one(&self) {
        self.0.fetch_sub(1, Ordering::Acquire);
    }
}

struct WorkUnit {
    counter: AtomicUsize,
}

impl Default for WorkUnit {
    fn default() -> Self {
        Self {
            counter: AtomicUsize::new(WORK_ITERS),
        }
    }
}

impl Drop for WorkUnit {
    fn drop(&mut self) {
        assert_eq!(self.counter.load(Ordering::Acquire), 0);
    }
}

impl ContainsNoCollectables for WorkUnit {}

#[derive(Default)]
struct Worker {
    queue: Mutex<WorkQueue>,
}

impl Collectable for Worker {
    const MAY_CONTAIN_REFERENCES: bool = true;

    fn trace(&self, tracer: &mut musegc::Tracer) {
        let queue = self.queue.lock();
        queue.items.trace(tracer);
    }
}

#[derive(Default)]
struct WorkQueue {
    items: Vec<Ref<WorkUnit>>,
}
