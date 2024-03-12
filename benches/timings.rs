//! A multi-threaded benchmark

use std::convert::Infallible;
use std::hint::black_box;
use std::sync::Arc;

use musegc::{collected, CollectionGuard, Ref, Root};
use timings::{Benchmark, BenchmarkImplementation, Label, LabeledTimings, Timings};

const TOTAL_ITERS: usize = 100_000;
const ITERS_PER_RELEASE: usize = 100;
const OUTER_ITERS: usize = TOTAL_ITERS / ITERS_PER_RELEASE;

fn main() {
    let timings = Timings::default();

    Benchmark::default()
        .with_each_number_of_threads([1, 4, 8, 16, 32])
        .with::<StdArc>()
        .with::<GcRef>()
        .with::<GcRoot>()
        .run(&timings)
        .unwrap();

    let report = timings.wait_for_stats();
    timings::print_table_summaries(&report).unwrap();
}

struct StdArc {
    metric: Label,
}

impl BenchmarkImplementation<Label, (), Infallible> for StdArc {
    type SharedConfig = Label;

    fn label(_number_of_threads: usize, _config: &()) -> Label {
        Label::from("Arc")
    }

    fn initialize_shared_config(
        number_of_threads: usize,
        _config: &(),
    ) -> Result<Self::SharedConfig, Infallible> {
        Ok(Label::from(format!("alloc-{number_of_threads:02}t")))
    }

    fn reset(_shutting_down: bool) -> Result<(), Infallible> {
        Ok(())
    }

    fn initialize(
        _number_of_threads: usize,
        metric: Self::SharedConfig,
    ) -> Result<Self, Infallible> {
        Ok(Self { metric })
    }

    fn measure(&mut self, measurements: &LabeledTimings<Label>) -> Result<(), Infallible> {
        let mut allocated = Vec::<Arc<[u8; 32]>>::with_capacity(ITERS_PER_RELEASE);

        for _ in 0..OUTER_ITERS {
            for i in 0..ITERS_PER_RELEASE {
                let timing = measurements.begin(self.metric.clone());
                let result = black_box(Arc::default());
                if i == ITERS_PER_RELEASE - 1 {
                    allocated.clear();
                }
                timing.finish();
                allocated.push(result);
            }
        }
        Ok(())
    }
}

struct GcRef {
    metric: Label,
}

impl BenchmarkImplementation<Label, (), Infallible> for GcRef {
    type SharedConfig = Label;

    fn label(_number_of_threads: usize, _config: &()) -> Label {
        Label::from("Weak")
    }

    fn initialize_shared_config(
        number_of_threads: usize,
        _config: &(),
    ) -> Result<Self::SharedConfig, Infallible> {
        Ok(Label::from(format!("alloc-{number_of_threads:02}t")))
    }

    fn reset(_shutting_down: bool) -> Result<(), Infallible> {
        // collect();
        Ok(())
    }

    fn initialize(
        _number_of_threads: usize,
        metric: Self::SharedConfig,
    ) -> Result<Self, Infallible> {
        Ok(Self { metric })
    }

    fn measure(&mut self, measurements: &LabeledTimings<Label>) -> Result<(), Infallible> {
        let mut allocated = Vec::<Ref<[u8; 32]>>::with_capacity(ITERS_PER_RELEASE);
        collected(|| {
            let mut guard = CollectionGuard::acquire();
            for _ in 0..OUTER_ITERS {
                for i in 0..ITERS_PER_RELEASE {
                    let timing = measurements.begin(self.metric.clone());
                    let result = black_box(Ref::new([0; 32], &mut guard));
                    if i == ITERS_PER_RELEASE - 1 {
                        allocated.clear();
                        guard.yield_to_collector();
                    }
                    timing.finish();
                    allocated.push(result);
                }
            }
        });

        Ok(())
    }
}

struct GcRoot {
    metric: Label,
}

impl BenchmarkImplementation<Label, (), Infallible> for GcRoot {
    type SharedConfig = Label;

    fn label(_number_of_threads: usize, _config: &()) -> Label {
        Label::from("Strong")
    }

    fn initialize_shared_config(
        number_of_threads: usize,
        _config: &(),
    ) -> Result<Self::SharedConfig, Infallible> {
        Ok(Label::from(format!("alloc-{number_of_threads:02}t")))
    }

    fn reset(_shutting_down: bool) -> Result<(), Infallible> {
        // collect();
        Ok(())
    }

    fn initialize(
        _number_of_threads: usize,
        metric: Self::SharedConfig,
    ) -> Result<Self, Infallible> {
        Ok(Self { metric })
    }

    fn measure(&mut self, measurements: &LabeledTimings<Label>) -> Result<(), Infallible> {
        let mut allocated = Vec::<Root<[u8; 32]>>::with_capacity(ITERS_PER_RELEASE);
        collected(|| {
            let mut guard = CollectionGuard::acquire();
            for _ in 0..OUTER_ITERS {
                for i in 0..ITERS_PER_RELEASE {
                    let timing = measurements.begin(self.metric.clone());
                    let result = black_box(Root::new([0; 32], &mut guard));
                    if i == ITERS_PER_RELEASE - 1 {
                        allocated.clear();
                        guard.yield_to_collector();
                    }
                    timing.finish();
                    allocated.push(result);
                }
            }
        });

        Ok(())
    }
}
