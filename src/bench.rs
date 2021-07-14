use std::{
    fmt::Display,
    io::{BufWriter, Read, Seek, SeekFrom, Write},
    panic::catch_unwind,
    time::{Duration, Instant},
};

use crossbeam_channel::Sender;
use thiserror::Error;

use crate::{input, BenchError, BenchResult, TracingAlloc};

pub mod simple;

pub type Function = for<'a> fn(&'a str, Bench) -> BenchResult;

#[derive(Debug, Error)]
#[error("Error benching memory use: {:?}", .inner)]
pub struct MemoryBenchError {
    #[source]
    #[from]
    pub inner: std::io::Error,
}

#[derive(Default)]
pub(crate) struct RuntimeData {
    // pub(crate) total_runs: u32,
    // pub(crate) min_run: Duration,
    pub(crate) mean_run: Duration,
    // pub(crate) max_run: Duration,
}

#[derive(Default)]
pub(crate) struct MemoryData {
    // pub(crate) end_ts: u128,
    // pub(crate) graph_points: Vec<(f64, f64)>,
    pub(crate) max_memory: usize,
}

fn get_data(trace_input: &str) -> MemoryData {
    let mut points = Vec::new();
    let mut cur_bytes = 0;
    let mut prev_bytes = 0;
    // let mut end_ts = 0;
    let mut max_bytes = 0;

    for line in trace_input.lines() {
        let mut parts = line.split_whitespace().map(str::trim);

        let (size, ts): (isize, u128) = match (
            parts.next(),
            parts.next().map(str::parse),
            parts.next().map(str::parse),
        ) {
            (Some("A"), Some(Ok(ts)), Some(Ok(size))) => (size, ts),
            (Some("F"), Some(Ok(ts)), Some(Ok(size))) => (-size, ts),
            (Some("S"), Some(Ok(ts)), _) => (0, ts),
            (Some("E"), Some(Ok(ts)), _) => {
                // end_ts = ts;
                (0, ts)
            }
            _ => {
                continue;
            }
        };

        cur_bytes += size;
        max_bytes = max_bytes.max(cur_bytes);

        points.push((ts as f64, prev_bytes as f64));
        points.push((ts as f64, cur_bytes as f64));

        prev_bytes = cur_bytes;
    }

    MemoryData {
        // end_ts,
        // graph_points: points,
        max_memory: max_bytes as usize,
    }
}

fn bench_function_runtime<Output, OutputErr>(
    bench_time: u64,
    func: impl Fn() -> Result<Output, OutputErr>,
) -> RuntimeData {
    let mut total_time = Duration::default();
    let mut total_runs = 0;
    let mut min_run = Duration::from_secs(u64::MAX);
    let mut max_run = Duration::default();
    let bench_start = Instant::now();

    loop {
        let start = Instant::now();
        let res = func();
        let elapsed = start.elapsed();
        total_time += start.elapsed();
        total_runs += 1;

        // Don't drop while measuring, in case the user returns a non-trivial type.
        // Also don't handle errors, as the function is assumed to be pure, and has already
        // had its return value checked in our caller.
        drop(res);

        if elapsed < min_run {
            min_run = elapsed;
        }

        if elapsed > max_run {
            max_run = elapsed;
        }

        if bench_start.elapsed().as_secs() >= bench_time && total_runs >= 10 {
            break;
        }
    }

    let mean_run = total_time / total_runs;

    RuntimeData {
        // total_runs,
        // min_run,
        mean_run,
        // max_run,
    }
}

fn bench_function_memory<Output, OutputErr>(
    alloc: &TracingAlloc,
    func: impl Fn() -> Result<Output, OutputErr>,
) -> Result<MemoryData, MemoryBenchError> {
    let trace_file = tempfile::tempfile()?;

    let writer = BufWriter::new(trace_file);
    alloc.set_file(writer);

    // No need to handle an error here, we did it earlier.
    alloc.enable_tracing();
    // Don't discard here, or dropping the return value will be caught
    // by the tracer.
    let res = func();
    alloc.disable_tracing();
    let _ = res;

    let mut mem_trace = String::new();

    let mut trace_writer = alloc.clear_file().unwrap(); // Should get it back.
    trace_writer.flush()?;

    let mut trace_file = trace_writer.into_inner().unwrap();
    trace_file.seek(SeekFrom::Start(0))?;
    trace_file.read_to_string(&mut mem_trace)?;

    Ok(get_data(&mem_trace))
}

pub(crate) enum BenchEvent {
    Answer { answer: String, id: usize },
    Memory { data: MemoryData, id: usize },
    Timing { data: RuntimeData, id: usize },
    Error { err: String, id: usize },
    Finish { id: usize },
}

pub struct Bench {
    pub(crate) alloc: &'static TracingAlloc,
    pub(crate) id: usize,
    pub(crate) chan: Sender<BenchEvent>,
    pub(crate) run_only: bool,
    pub(crate) bench_time: u64,
}

impl Bench {
    pub fn bench<T: Display, E: Display>(
        self,
        f: impl Fn() -> Result<T, E> + Copy,
    ) -> Result<(), BenchError> {
        match f() {
            Ok(t) => {
                self.chan
                    .send(BenchEvent::Answer {
                        answer: t.to_string(),
                        id: self.id,
                    })
                    .map_err(|_| BenchError::ChannelError(self.id))?;

                if !self.run_only {
                    let data = bench_function_memory(self.alloc, f)
                        .map_err(|e| BenchError::MemoryBenchError(e, self.id))?;

                    self.chan
                        .send(BenchEvent::Memory { data, id: self.id })
                        .map_err(|_| BenchError::ChannelError(self.id))?;

                    let data = bench_function_runtime(self.bench_time, f);
                    self.chan
                        .send(BenchEvent::Timing { data, id: self.id })
                        .map_err(|_| BenchError::ChannelError(self.id))?;
                }
            }
            Err(e) => {
                self.chan
                    .send(BenchEvent::Error {
                        err: e.to_string(),
                        id: self.id,
                    })
                    .map_err(|_| BenchError::ChannelError(self.id))?;
            }
        }

        Ok(())
    }
}

pub(crate) fn bench_worker(year: u16, day: u8, bench: Bench, func: Function) {
    let id = bench.id;
    let sender = bench.chan.clone();
    match input(year, day).open() {
        Ok(input) => {
            let did_panic = catch_unwind(|| func(&input, bench));

            match did_panic {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    sender
                        .send(BenchEvent::Error {
                            err: e.to_string(),
                            id,
                        })
                        .expect("Unable to send error");
                }
                Err(_) => {
                    sender
                        .send(BenchEvent::Error {
                            err: "Function panicked!".to_owned(),
                            id,
                        })
                        .expect("Unable to send error");
                }
            }
        }
        Err(BenchError::InputFileError { inner, name }) => {
            sender
                .send(BenchEvent::Error {
                    err: format!("{}: {:?}", name, inner.kind()),
                    id,
                })
                .expect("Unable to send error");
        }
        Err(_) => unreachable!(), // InputFile::open only returns one error variant.
    }

    sender
        .send(BenchEvent::Finish { id })
        .expect("Unable to send finish");
}
