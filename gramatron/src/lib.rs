//! A libfuzzer-like fuzzer with llmp-multithreading support and restarts
//! The example harness is built for libpng.
//! In this example, you will see the use of the `launcher` feature.
//! The `launcher` will spawn new processes for each cpu core.
use mimalloc::MiMalloc;
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use clap::{App, Arg};
use core::time::Duration;
#[cfg(unix)]
use nix::{self, unistd::dup};
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::{
    env,
    fs::{self, File},
    io::{self, BufReader, Read, Write},
    path::{Path, PathBuf},
};

use libafl::{
    bolts::{
        current_nanos,
        os::dup2,
        rands::StdRand,
        shmem::{ShMemProvider, StdShMemProvider},
        tuples::tuple_list,
    },
    corpus::{
        CachedOnDiskCorpus, Corpus, IndexesLenTimeMinimizerCorpusScheduler, OnDiskCorpus,
        QueueCorpusScheduler,
    },
    events::SimpleRestartingEventManager,
    executors::{inprocess::InProcessExecutor, ExitKind, TimeoutExecutor},
    feedback_or,
    feedbacks::{CrashFeedback, MapFeedbackState, MaxMapFeedback, TimeFeedback},
    fuzzer::{Fuzzer, StdFuzzer},
    generators::{Automaton, GramatronGenerator},
    inputs::GramatronInput,
    monitors::SimpleMonitor,
    mutators::{
        GramatronRandomMutator, GramatronRecursionMutator, GramatronSpliceMutator,
        StdScheduledMutator,
    },
    observers::{HitcountsMapObserver, StdMapObserver, TimeObserver},
    stages::mutational::StdMutationalStage,
    state::{HasCorpus, StdState},
    Error,
};

use libafl_targets::{libfuzzer_initialize, libfuzzer_test_one_input, EDGES_MAP, MAX_EDGES_NUM};

/// The fuzzer main (as `no_mangle` C function)
#[no_mangle]
pub fn libafl_main() {
    // Registry the metadata types used in this fuzzer
    // Needed only on no_std
    //RegistryBuilder::register::<Tokens>();

    let res = match App::new(env!("CARGO_PKG_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .author("AFLplusplus team")
        .about("LibAFL-based fuzzer for Fuzzbench")
        .arg(
            Arg::new("out")
                .short('o')
                .long("output")
                .help("The directory to place finds in ('corpus')")
                .takes_value(true),
        )
        .arg(
            Arg::new("grammar")
                .short('g')
                .long("grammar")
                .help("The grammar model")
                .takes_value(true),
        )
        .arg(
            Arg::new("timeout")
                .short('t')
                .long("timeout")
                .help("Timeout for each individual execution, in milliseconds")
                .default_value("1200"),
        )
        .arg(Arg::new("remaining").multiple_values(true))
        .try_get_matches()
    {
        Ok(res) => res,
        Err(err) => {
            println!(
                "Syntax: {}, -o corpus_dir -g grammar.json\n{:?}",
                env::current_exe()
                    .unwrap_or_else(|_| "fuzzer".into())
                    .to_string_lossy(),
                err.info,
            );
            return;
        }
    };

    println!(
        "Workdir: {:?}",
        env::current_dir().unwrap().to_string_lossy().to_string()
    );

    if let Some(filenames) = res.values_of("remaining") {
        let filenames: Vec<&str> = filenames.collect();
        if !filenames.is_empty() {
            run_testcases(&filenames);
            return;
        }
    }

    let grammar_path = PathBuf::from(
        res.value_of("grammar")
            .expect("The --grammar parameter is missing")
            .to_string(),
    );
    if !grammar_path.is_file() {
        println!("{:?} is not a valid file!", &grammar_path);
        return;
    }
    let automaton = read_automaton_from_file(grammar_path);

    // For fuzzbench, crashes and finds are inside the same `corpus` directory, in the "queue" and "crashes" subdir.
    let mut out_dir = PathBuf::from(
        res.value_of("out")
            .expect("The --output parameter is missing")
            .to_string(),
    );
    if fs::create_dir(&out_dir).is_err() {
        println!("Out dir at {:?} already exists.", &out_dir);
        if !out_dir.is_dir() {
            println!("Out dir at {:?} is not a valid directory!", &out_dir);
            return;
        }
    }
    let mut crashes = out_dir.clone();
    crashes.push("crashes");
    out_dir.push("queue");

    let timeout = Duration::from_millis(
        res.value_of("timeout")
            .unwrap()
            .to_string()
            .parse()
            .expect("Could not parse timeout in milliseconds"),
    );

    fuzz(out_dir, crashes, automaton, timeout).expect("An error occurred while fuzzing");
}

fn read_automaton_from_file<P: AsRef<Path>>(path: P) -> Automaton {
    let file = fs::File::open(path).unwrap();
    let mut reader = BufReader::new(file);
    let mut buffer = Vec::new();
    reader.read_to_end(&mut buffer).unwrap();
    postcard::from_bytes(&buffer).unwrap()
}

fn run_testcases(filenames: &[&str]) {
    // The actual target run starts here.
    // Call LLVMFUzzerInitialize() if present.
    let args: Vec<String> = env::args().collect();
    if libfuzzer_initialize(&args) == -1 {
        println!("Warning: LLVMFuzzerInitialize failed with -1")
    }

    println!(
        "You are not fuzzing, just executing {} testcases",
        filenames.len()
    );
    for fname in filenames {
        println!("Executing {}", fname);

        let f = fs::File::open(fname).expect("Unable to open file");
        let mut reader = BufReader::new(f);
        let mut buffer = Vec::new();

        reader.read_to_end(&mut buffer).unwrap();

        let input: GramatronInput = postcard::from_bytes(&buffer).unwrap();

        let mut bytes = vec![];
        input.unparse(&mut bytes);
        if *bytes.last().unwrap() != 0 {
            bytes.push(0);
        }
        unsafe {
            println!("Testcase: {}", std::str::from_utf8_unchecked(&bytes));
        }
        libfuzzer_test_one_input(&bytes);
    }
}

/// The actual fuzzer
fn fuzz(
    corpus_dir: PathBuf,
    objective_dir: PathBuf,
    automaton: Automaton,
    timeout: Duration,
) -> Result<(), Error> {
    #[cfg(unix)]
    let mut stdout_cpy = unsafe {
        let new_fd = dup(io::stdout().as_raw_fd())?;
        File::from_raw_fd(new_fd)
    };
    #[cfg(unix)]
    let file_null = File::open("/dev/null")?;

    // 'While the monitor are state, they are usually used in the broker - which is likely never restarted
    let monitor = SimpleMonitor::new(|s| {
        #[cfg(unix)]
        writeln!(&mut stdout_cpy, "{}", s).unwrap();
        #[cfg(windows)]
        println!("{}", s);
    });

    // We need a shared map to store our state before a crash.
    // This way, we are able to continue fuzzing afterwards.
    let mut shmem_provider = StdShMemProvider::new()?;

    let (state, mut mgr) = match SimpleRestartingEventManager::launch(monitor, &mut shmem_provider)
    {
        // The restarting state will spawn the same process again as child, then restarted it each time it crashes.
        Ok(res) => res,
        Err(err) => match err {
            Error::ShuttingDown => {
                return Ok(());
            }
            _ => {
                panic!("Failed to setup the restarter: {}", err);
            }
        },
    };

    // Create an observation channel using the coverage map
    let edges = unsafe { &mut EDGES_MAP[0..MAX_EDGES_NUM] };
    let edges_observer = HitcountsMapObserver::new(StdMapObserver::new("edges", edges));

    // Create an observation channel to keep track of the execution time
    let time_observer = TimeObserver::new("time");

    // The state of the edges feedback.
    let feedback_state = MapFeedbackState::with_observer(&edges_observer);

    // Feedback to rate the interestingness of an input
    // This one is composed by two Feedbacks in OR
    let feedback = feedback_or!(
        // New maximization map feedback linked to the edges observer and the feedback state
        MaxMapFeedback::new_tracking(&feedback_state, &edges_observer, true, false),
        // Time feedback, this one does not need a feedback state
        TimeFeedback::new_with_observer(&time_observer)
    );

    // A feedback to choose if an input is a solution or not
    let objective = CrashFeedback::new();

    // If not restarting, create a State from scratch
    let mut state = state.unwrap_or_else(|| {
        StdState::new(
            // RNG
            StdRand::with_seed(current_nanos()),
            // Corpus that will be evolved, we keep it in memory for performance
            CachedOnDiskCorpus::new(corpus_dir, 4096).unwrap(),
            // Corpus in which we store solutions (crashes in this example),
            // on disk so the user can get them after stopping the fuzzer
            OnDiskCorpus::new(objective_dir).unwrap(),
            // States of the feedbacks.
            // They are the data related to the feedbacks that you want to persist in the State.
            tuple_list!(feedback_state),
        )
    });

    // A minimization+queue policy to get testcasess from the corpus
    let scheduler = IndexesLenTimeMinimizerCorpusScheduler::new(QueueCorpusScheduler::new());

    // A fuzzer with feedbacks and a corpus scheduler
    let mut fuzzer = StdFuzzer::new(scheduler, feedback, objective);

    // The wrapped harness function, calling out to the LLVM-style harness
    let mut bytes = vec![];
    let mut harness = |input: &GramatronInput| {
        if input.terminals().is_empty() {
            return ExitKind::Ok;
        }
        input.unparse(&mut bytes);
        libfuzzer_test_one_input(&bytes);
        ExitKind::Ok
    };

    // Create the executor for an in-process function with one observer for edge coverage and one for the execution time
    let mut executor = TimeoutExecutor::new(
        InProcessExecutor::new(
            &mut harness,
            tuple_list!(edges_observer, time_observer),
            &mut fuzzer,
            &mut state,
            &mut mgr,
        )?,
        timeout,
    );

    // The actual target run starts here.
    // Call LLVMFUzzerInitialize() if present.
    let args: Vec<String> = env::args().collect();
    if libfuzzer_initialize(&args) == -1 {
        println!("Warning: LLVMFuzzerInitialize failed with -1")
    }

    let mut generator = GramatronGenerator::new(&automaton);

    // In case the corpus is empty (on first run), reset
    if state.corpus().count() < 1 {
        state
            .generate_initial_inputs_forced(
                &mut fuzzer,
                &mut executor,
                &mut generator,
                &mut mgr,
                1024,
            )
            .expect("Failed to generate the initial corpus");
    }

    // Setup a basic mutator with a mutational stage
    let mutator = StdScheduledMutator::with_max_iterations(
        tuple_list!(
            GramatronRandomMutator::new(&generator),
            GramatronRandomMutator::new(&generator),
            GramatronRandomMutator::new(&generator),
            GramatronSpliceMutator::new(),
            GramatronSpliceMutator::new(),
            GramatronRecursionMutator::new()
        ),
        3,
    );
    let mut stages = tuple_list!(StdMutationalStage::new(mutator));

    // Remove target ouput (logs still survive)
    #[cfg(unix)]
    {
        let null_fd = file_null.as_raw_fd();
        dup2(null_fd, io::stdout().as_raw_fd())?;
        dup2(null_fd, io::stderr().as_raw_fd())?;
    }

    fuzzer.fuzz_loop(&mut stages, &mut executor, &mut state, &mut mgr)?;
    Ok(())
}