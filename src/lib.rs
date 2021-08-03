use std::{fmt::Display, iter, num::ParseIntError, time::Duration};

use crossbeam_channel::Receiver;
use once_cell::sync::Lazy;
use structopt::StructOpt;
use thiserror::Error;

mod alloc;
mod bench;
pub mod misc;
pub mod parsers;

pub use alloc::TracingAlloc;
pub use bench::Bench;
use bench::{simple::run_simple_bench, AlternateAnswer, BenchEvent, Function, MemoryBenchError};

static ARGS: Lazy<Args> = Lazy::new(Args::from_args);

pub type BenchResult = Result<(), BenchError>;

#[derive(Debug, Error)]
pub enum BenchError {
    #[error("Error performing memory benchmark function {}: {}", .1, .0)]
    MemoryBenchError(MemoryBenchError, usize),

    #[error("Error returning benchmark result for function {}", .0)]
    ChannelError(usize),

    #[error("Error opening input file '{}': {:}", .name, .inner)]
    InputFileError {
        #[source]
        inner: std::io::Error,
        name: String,
    },

    #[error("{}", .0)]
    UserError(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("Day {} not defined", .0)]
    DaysFilterError(u8),
}

#[allow(non_snake_case)]
pub fn UserError<E: Into<Box<dyn std::error::Error + Send + Sync>>>(e: E) -> BenchError {
    BenchError::UserError(e.into())
}

#[derive(Debug, Error)]
pub enum NoError {}

// Getting an inexplicable compiler error if I just try let structopt handle a the
// Option<Vec<u8>>, so I'm using this as a workaround.
fn parse_days_list(src: &str) -> Result<u8, ParseIntError> {
    src.parse()
}

#[derive(Clone, StructOpt, PartialEq, Eq)]
pub(crate) enum RunType {
    /// Just runs the day's primary functions.
    Run {
        #[structopt(parse(try_from_str = parse_days_list))]
        /// List of days to run [default: all]
        days: Vec<u8>,
    },
    /// Benchmarks the days' primary functions, and lists them in a simple format.
    Bench {
        #[structopt(parse(try_from_str = parse_days_list))]
        /// List of days to run [default: all]
        days: Vec<u8>,

        #[structopt(short)]
        /// Render more detailed benchmarking info.
        detailed: bool,
    },
    /// Benchmarks all the days' functions, and provides a more detailed listing.
    Detailed {
        #[structopt(parse(try_from_str = parse_days_list))]
        /// List of days to run [default: all]
        days: Vec<u8>,
    },
}

impl RunType {
    pub(crate) fn is_run_only(&self) -> bool {
        matches!(self, RunType::Run { .. })
    }

    fn days(&self) -> &[u8] {
        match self {
            RunType::Run { days } | RunType::Bench { days, .. } | RunType::Detailed { days } => {
                days
            }
        }
    }
}

#[derive(StructOpt)]
pub(crate) struct Args {
    #[structopt(subcommand)]
    // Selects how to run the days
    run_type: RunType,

    #[structopt(long, default_value = "3")]
    /// Benchmarking period in seconds to measure run time of parts
    bench_time: u64,

    #[structopt(long = "threads")]
    /// How many worker threads to spawn for benchmarking [default: cores - 2, min: 1]
    num_threads: Option<usize>,
}

pub struct ProblemInput;
impl Display for ProblemInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("")
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Example {
    Parse,
    Part1,
    Part2,
    Other(&'static str),
}

impl Display for Example {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let output = match self {
            Example::Parse => "parse",
            Example::Part1 => "part1",
            Example::Part2 => "part2",
            Example::Other(s) => s,
        };

        f.write_str(output)
    }
}

pub struct InputFile<T> {
    year: u16,
    day: u8,
    example_id: Option<(Example, T)>,
}

impl InputFile<ProblemInput> {
    pub fn example<T: Display>(self, part: Example, id: T) -> InputFile<T> {
        InputFile {
            year: self.year,
            day: self.day,
            example_id: Some((part, id)),
        }
    }
}

impl<T: Display> InputFile<T> {
    pub fn open(self) -> Result<String, BenchError> {
        let path = if let Some((part, id)) = self.example_id {
            format!(
                "./example_inputs/aoc_{:02}{:02}_{}-{}.txt",
                self.year % 100,
                self.day,
                part,
                id
            )
        } else {
            format!("./inputs/aoc_{:02}{:02}.txt", self.year % 100, self.day)
        };

        std::fs::read_to_string(&path).map_err(|e| BenchError::InputFileError {
            inner: e,
            name: path,
        })
    }
}

pub fn input(year: u16, day: u8) -> InputFile<ProblemInput> {
    InputFile {
        year,
        day,
        example_id: None,
    }
}

#[derive(Copy, Clone)]
pub struct Day {
    pub name: &'static str,
    pub day: u8,
    pub part_1: Function,
    pub part_2: Option<Function>,
}

fn get_days<'d>(days: &'d [Day], filter: &[u8]) -> Result<Vec<&'d Day>, BenchError> {
    match filter {
        [] => Ok(days.iter().collect()),
        filter => {
            let mut new_days = Vec::with_capacity(filter.len());

            for &filter_day in filter {
                let day = days
                    .iter()
                    .find(|d| d.day == filter_day)
                    .ok_or(BenchError::DaysFilterError(filter_day))?;
                new_days.push(day);
            }

            new_days.sort_by_key(|d| d.day);
            Ok(new_days)
        }
    }
}

pub(crate) fn render_decimal(val: usize) -> String {
    let (factor, unit) = if val < 10usize.pow(3) {
        (10f64.powi(0), "")
    } else if val < 10usize.pow(6) {
        (10f64.powi(-3), " k")
    } else if val < 10usize.pow(9) {
        (10f64.powi(-6), " M")
    } else {
        (10f64.powi(-9), " B")
    };

    let val_f = (val as f64) * factor;
    let prec = if val < 1000 {
        0 // No need for decimals here.
    } else if val_f < 10.0 {
        3
    } else if val_f < 100.0 {
        2
    } else if val_f < 1000.0 {
        1
    } else {
        0
    };

    format!(
        "{:>width$.prec$}{}",
        val_f,
        unit,
        prec = prec,
        width = 7 - unit.len()
    )
}

pub fn render_duration(time: Duration) -> String {
    // The logic here is basically copied from Criterion.
    let time = time.as_nanos() as f64;

    let (factor, unit) = if time < 10f64.powi(0) {
        (10f64.powi(3), "ps")
    } else if time < 10f64.powi(3) {
        (10f64.powi(0), "ns")
    } else if time < 10f64.powi(6) {
        (10f64.powi(-3), "µs")
    } else if time < 10f64.powi(9) {
        (10f64.powi(-6), "ms")
    } else {
        (10f64.powi(-9), "s")
    };

    let time = time * factor;

    let prec = if time < 10.0 {
        3
    } else if time < 100.0 {
        2
    } else if time < 1000.0 {
        1
    } else {
        0
    };

    format!("{:>5.prec$} {}", time, unit, prec = prec)
}

fn print_header() {
    if ARGS.run_type.is_run_only() {
        println!("   Day | {:<30} ", "Answer");
        println!("_______|_{0:_<30}", "");
    } else if let RunType::Bench {
        detailed: false, ..
    } = &ARGS.run_type
    {
        println!("   Day | {:<30} | {:<10} | Max Mem.", "Answer", "Time");
        println!("_______|_{0:_<30}_|_{0:_<10}_|______________", "");
    } else {
        println!(
            "   Day | {:<30} | {:<32} | Allocs  | Max Mem.",
            "Answer", "Time"
        );
        println!("_______|_{0:_<30}_|_{0:_<32}_|_________|_____________", "");
    }
}

fn print_footer(total_time: Duration) {
    if ARGS.run_type.is_run_only() {
        println!("_______|_{0:_<30}", "");
    } else if let RunType::Bench {
        detailed: false, ..
    } = &ARGS.run_type
    {
        let time = render_duration(total_time);
        println!("_______|_{0:_<30}_|_{0:_<10}_|______________", "");
        println!(" Total Time: {:26} | {}", "", time);
    } else {
        let time = render_duration(total_time);
        println!("_______|_{0:_<30}_|_{0:_<32}_|_________|_____________", "");
        println!(" Total Time: {:26} | {}", "", time);
    }
}

fn print_alt_answers(receiver: Receiver<AlternateAnswer>) {
    if !receiver.is_empty() {
        println!("\n -- Alternate Answers --");
        for alt_ans in receiver.iter() {
            println!("Day {}, Part: {}", alt_ans.day, alt_ans.day_function_id);
            println!("{}\n", alt_ans.answer);
        }
    }
}

// No need for all of the complex machinery just to run the two functions, given we want
// panics to happen as normal.
fn run_single(alloc: &'static TracingAlloc, year: u16, day: &Day) -> Result<(), BenchError> {
    print_header();

    let (sender, receiver) = crossbeam_channel::unbounded();
    let (alt_answer_sender, alt_answer_receiver) = crossbeam_channel::unbounded();

    let parts = iter::once(day.part_1).chain(day.part_2).zip(1..);

    for (part, id) in parts {
        let dummy = Bench {
            alloc,
            id: 0,
            day: day.day,
            day_function_id: id,
            alt_answer_chan: alt_answer_sender.clone(),
            chan: sender.clone(),
            run_only: true,
            bench_time: 0,
        };

        let input = input(year, day.day).open()?;
        part(&input, dummy)?;

        let message = match receiver.recv().expect("Failed to receive from channel") {
            BenchEvent::Answer { answer: msg, .. } | BenchEvent::Error { err: msg, .. } => msg,
            _ => unreachable!("Should only receive an Answer or Error"),
        };

        println!("  {:>2}.{} | {}", day.day, id, message);
    }

    print_footer(Duration::ZERO);

    drop(alt_answer_sender);
    print_alt_answers(alt_answer_receiver);

    Ok(())
}

pub fn run(alloc: &'static TracingAlloc, year: u16, days: &[Day]) -> Result<(), BenchError> {
    let days = get_days(days, ARGS.run_type.days())?;

    println!("Advent of Code {}", year);
    match (&ARGS.run_type, &*days) {
        (RunType::Run { .. }, [day]) => run_single(alloc, year, day),
        (RunType::Detailed { .. }, _) => todo!(),
        (RunType::Run { .. } | RunType::Bench { .. }, days) => run_simple_bench(alloc, year, days),
    }
}

#[macro_export]
macro_rules! day {
    (day $id:literal: $name:literal
        1: $p1:ident
    ) => {
        pub static DAY: $crate::Day = $crate::Day {
            name: $name,
            day: $id,
            part_1: $p1,
            part_2: None,
        };
    };
    (day $id:literal: $name:literal
        1: $p1:ident
        2: $p2:ident
    ) => {
        pub static DAY: $crate::Day = $crate::Day {
            name: $name,
            day: $id,
            part_1: $p1,
            part_2: Some($p2),
        };
    };
}
