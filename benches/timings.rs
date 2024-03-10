use std::convert::Infallible;
use std::hint::black_box;
use std::sync::Arc;

use musegc::{collected, CollectionGuard, Strong, Weak};
use timings::{Benchmark, BenchmarkImplementation, Label, LabeledTimings, Timings};

const TOTAL_ITERS: usize = 100_000;
const ITERS_PER_RELEASE: usize = 100;
const OUTER_ITERS: usize = TOTAL_ITERS / ITERS_PER_RELEASE;

fn main() {
    let timings = Timings::default();

    Benchmark::default()
        .with_each_number_of_threads([1, 4, 8, 16, 32])
        .with::<StdArc>()
        .with::<GcWeak>()
        .with::<GcStrong>()
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

struct GcWeak {
    metric: Label,
}

impl BenchmarkImplementation<Label, (), Infallible> for GcWeak {
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
        let mut allocated = Vec::<Weak<[u8; 32]>>::with_capacity(ITERS_PER_RELEASE);
        collected(|| {
            let mut guard = CollectionGuard::acquire();
            for _ in 0..OUTER_ITERS {
                for i in 0..ITERS_PER_RELEASE {
                    let timing = measurements.begin(self.metric.clone());
                    let result = black_box(Weak::new([0; 32], &mut guard));
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

struct GcStrong {
    metric: Label,
}

impl BenchmarkImplementation<Label, (), Infallible> for GcStrong {
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
        let mut allocated = Vec::<Strong<[u8; 32]>>::with_capacity(ITERS_PER_RELEASE);
        collected(|| {
            let mut guard = CollectionGuard::acquire();
            for _ in 0..OUTER_ITERS {
                for i in 0..ITERS_PER_RELEASE {
                    let timing = measurements.begin(self.metric.clone());
                    let result = black_box(Strong::new([0; 32], &mut guard));
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
