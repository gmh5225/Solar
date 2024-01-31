//! Sulk test runner.
//!
//! This crate is invoked in `crates/sulk/tests.rs`.

#![allow(unreachable_pub)]
#![cfg_attr(feature = "nightly", feature(test))]

#[cfg(feature = "nightly")]
extern crate test;
#[cfg(feature = "nightly")]
use tester as _;

#[cfg(not(feature = "nightly"))]
use tester as test;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

mod compute_diff;

mod context;
use context::{ProcRes, TestCx, TestPaths};

mod errors;

mod header;
use header::TestProps;

mod solc;

mod json;

mod ui;

mod utils;
use utils::TestResult;

pub fn run_tests(cmd: &'static Path) -> i32 {
    let args = std::env::args().collect::<Vec<_>>();
    let mut opts = match test::test::parse_opts(&args) {
        Some(Ok(o)) => o,
        Some(Err(msg)) => {
            eprintln!("error: {msg}");
            return 101;
        }
        None => return 0,
    };
    // Condense output if not explicitly requested.
    let requested_pretty = || args.iter().any(|x| x.contains("--format"));
    if opts.format == test::OutputFormat::Pretty && !requested_pretty() {
        opts.format = test::OutputFormat::Terse;
    }
    // [`tester`] currently (0.9.1) uses `num_cpus::get_physical`;
    // use all available threads instead.
    if opts.test_threads.is_none() {
        opts.test_threads = std::thread::available_parallelism().map(|x| x.get()).ok();
    }

    let mut modes = &[Mode::Ui, Mode::SolcSolidity, Mode::SolcYul][..];
    let mode_tmp;
    if let Ok(mode) = std::env::var("TESTER_MODE") {
        mode_tmp = match mode.as_str() {
            "ui" => Mode::Ui,
            "solc-solidity" => Mode::SolcSolidity,
            "solc-yul" => Mode::SolcYul,
            _ => panic!("unknown mode: {mode}"),
        };
        modes = std::slice::from_ref(&mode_tmp);
    }

    let mut tests = Vec::new();
    let config = Arc::new(Config::new(cmd));
    for &mode in modes {
        make_tests(&config, &mut tests, mode);
    }
    tests.sort_by(|a, b| a.desc.name.as_slice().cmp(b.desc.name.as_slice()));

    match test::run_tests_console(&opts, tests) {
        Ok(true) => 0,
        Ok(false) => {
            eprintln!("Some tests failed");
            1
        }
        Err(e) => {
            eprintln!("I/O failure during tests: {e}");
            101
        }
    }
}

fn make_tests(config: &Arc<Config>, tests: &mut Vec<test::TestDescAndFn>, mode: Mode) {
    let TestFns { check, run } = match mode {
        Mode::Ui => ui::FNS,
        Mode::SolcSolidity => solc::solidity::FNS,
        Mode::SolcYul => solc::yul::FNS,
    };
    let load = if mode.solc_props() { TestProps::load_solc } else { TestProps::load };

    let inputs = collect_tests(config, mode);
    tests.reserve(inputs.len());
    for input in &inputs {
        let mut make_test = |revision: Option<String>| {
            let config = Arc::clone(config);
            let path = input.path().to_path_buf();
            let rel_path = path.strip_prefix(config.root).unwrap_or(&path);
            let relative_dir = rel_path.parent().unwrap().to_path_buf();

            if !mode.solc_props() {
                let build_path = context::output_relative_path(&config, &relative_dir);
                std::fs::create_dir_all(build_path).unwrap();
            }

            let mode = match mode {
                Mode::Ui => "ui",
                Mode::SolcSolidity => "solc-solidity",
                Mode::SolcYul => "solc-yul",
            };
            let rev_name = revision.as_ref().map(|r| format!("#{r}")).unwrap_or_default();
            let name = format!("[{mode}] {}{rev_name}", rel_path.display());
            let ignore_reason = match check(&config, &path) {
                TestResult::Skipped(reason) => Some(reason),
                _ => None,
            };

            tests.push(test::TestDescAndFn {
                #[cfg(feature = "nightly")]
                desc: test::TestDesc {
                    name: test::TestName::DynTestName(name),
                    ignore: ignore_reason.is_some(),
                    ignore_message: ignore_reason,
                    source_file: "",
                    start_line: 0,
                    start_col: 0,
                    end_line: 0,
                    end_col: 0,
                    should_panic: test::ShouldPanic::No,
                    compile_fail: false,
                    no_run: false,
                    test_type: test::TestType::Unknown,
                },
                #[cfg(not(feature = "nightly"))]
                desc: test::TestDesc {
                    name: test::TestName::DynTestName(name),
                    ignore: ignore_reason.is_some(),
                    should_panic: test::ShouldPanic::No,
                    allow_fail: false,
                    test_type: test::TestType::Unknown,
                },
                testfn: test::DynTestFn(Box::new(move || {
                    let src = std::fs::read_to_string(&path).unwrap();
                    let props = load(&src, revision.as_deref());
                    let revision = revision.as_deref();
                    let paths = TestPaths { file: path, relative_dir };

                    let cx = TestCx { config: &config, paths, src, props, revision };
                    std::fs::create_dir_all(cx.output_base_dir()).unwrap();
                    let r = run(&cx);
                    if r == TestResult::Failed {
                        #[cfg(not(feature = "nightly"))]
                        panic!("test failed");
                        #[cfg(feature = "nightly")]
                        return Err(String::from("test failed"));
                    }
                    #[cfg(feature = "nightly")]
                    Ok(())
                })),
            });
        };

        if matches!(mode, Mode::Ui) {
            let revisions = TestProps::load_revisions(input.path());
            if !revisions.is_empty() {
                for rev in revisions {
                    make_test(Some(rev));
                }
                continue;
            }
        }

        make_test(None);
    }
}

fn collect_tests(config: &Config, mode: Mode) -> Vec<walkdir::DirEntry> {
    let path = match mode {
        Mode::Ui => "tests/ui/",
        Mode::SolcSolidity => "testdata/solidity/test/",
        Mode::SolcYul => "testdata/solidity/test/libyul/",
    };
    let path = config.root.join(path);
    let yul = match mode {
        Mode::Ui => true,
        Mode::SolcSolidity => false,
        Mode::SolcYul => true,
    };
    walkdir::WalkDir::new(path)
        .sort_by_file_name()
        .into_iter()
        .map(|entry| entry.unwrap())
        .filter(|entry| {
            entry.path().extension() == Some("sol".as_ref())
                || (yul && entry.path().extension() == Some("yul".as_ref()))
        })
        .collect::<Vec<_>>()
}

#[derive(Clone, Copy)]
enum Mode {
    Ui,
    SolcSolidity,
    SolcYul,
}

impl Mode {
    fn solc_props(self) -> bool {
        matches!(self, Self::SolcSolidity | Self::SolcYul)
    }
}

struct TestFns {
    check: fn(&Config, &Path) -> TestResult,
    run: fn(&TestCx<'_>) -> TestResult,
}

struct Config {
    cmd: &'static Path,
    root: &'static Path,
    build_base: PathBuf,

    #[allow(dead_code)]
    verbose: bool,
    bless: bool,
}

impl Config {
    fn new(cmd: &'static Path) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
        let build_base = root.join("target/tester");
        std::fs::create_dir_all(&build_base).unwrap();
        Self {
            cmd,
            root,
            build_base,
            verbose: false,
            bless: std::env::var("TESTER_BLESS").is_ok_and(|x| x != "0"),
        }
    }
}
