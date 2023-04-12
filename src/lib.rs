#![allow(
    clippy::enum_variant_names,
    clippy::useless_format,
    clippy::too_many_arguments,
    rustc::internal
)]
#![deny(missing_docs)]

//! A crate to run the Rust compiler (or other binaries) and test their command line output.

use bstr::ByteSlice;
pub use color_eyre;
use color_eyre::eyre::{Context, Result};
use colored::*;
use crossbeam_channel::unbounded;
use parser::{ErrorMatch, Pattern, Revisioned};
use regex::bytes::Regex;
use rustc_stderr::{Diagnostics, Level, Message};
use std::borrow::Cow;
use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::fmt::Display;
use std::fmt::Write;
use std::io::Write as _;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::thread;

use crate::dependencies::build_dependencies;
use crate::parser::{Comments, Condition};

mod dependencies;
mod diff;
pub mod github_actions;
mod parser;
mod rustc_stderr;
#[cfg(test)]
mod tests;

#[derive(Debug, Clone)]
/// Central datastructure containing all information to run the tests.
pub struct Config {
    /// Arguments passed to the binary that is executed.
    /// Take care to only append unless you actually meant to overwrite the defaults.
    /// Overwriting the defaults may make `//~ ERROR` style comments stop working.
    pub args: Vec<OsString>,
    /// Environment variables passed to the binary that is executed.
    /// The environment variable is removed if the second tuple field is `None`
    pub envs: Vec<(OsString, Option<OsString>)>,
    /// Arguments passed to the binary that is executed.
    /// These arguments are passed *after* the args inserted via `//@compile-flags:`.
    pub trailing_args: Vec<OsString>,
    /// Host triple; usually will be auto-detected.
    pub host: Option<String>,
    /// `None` to run on the host, otherwise a target triple
    pub target: Option<String>,
    /// Filters applied to stderr output before processing it.
    /// By default contains a filter for replacing backslashes with regular slashes.
    /// On windows, contains a filter to replace `\n` with `\r\n`.
    pub stderr_filters: Filter,
    /// Filters applied to stdout output before processing it.
    /// On windows, contains a filter to replace `\n` with `\r\n`.
    pub stdout_filters: Filter,
    /// The folder in which to start searching for .rs files
    pub root_dir: PathBuf,
    /// The mode in which to run the tests.
    pub mode: Mode,
    /// The binary to actually execute.
    pub program: PathBuf,
    /// What to do in case the stdout/stderr output differs from the expected one.
    pub output_conflict_handling: OutputConflictHandling,
    /// Only run tests with one of these strings in their path/name
    pub path_filter: Vec<String>,
    /// Path to a `Cargo.toml` that describes which dependencies the tests can access.
    pub dependencies_crate_manifest_path: Option<PathBuf>,
    /// The command to run can be changed from `cargo` to any custom command to build the
    /// dependencies in `dependencies_crate_manifest_path`
    pub dependency_builder: DependencyBuilder,
    /// Print one character per test instead of one line
    pub quiet: bool,
    /// How many threads to use for running tests. Defaults to number of cores
    pub num_test_threads: NonZeroUsize,
    /// Where to dump files like the binaries compiled from tests.
    pub out_dir: Option<PathBuf>,
    /// The default edition to use on all tests
    pub edition: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            args: vec!["--error-format=json".into()],
            envs: vec![],
            trailing_args: vec![],
            host: None,
            target: None,
            stderr_filters: vec![
                (Match::Exact(vec![b'\\']), b"/"),
                #[cfg(windows)]
                (Match::Exact(vec![b'\r']), b""),
            ],
            stdout_filters: vec![
                #[cfg(windows)]
                (Match::Exact(vec![b'\r']), b""),
            ],
            root_dir: PathBuf::new(),
            mode: Mode::Fail {
                require_patterns: true,
            },
            program: PathBuf::from("rustc"),
            output_conflict_handling: OutputConflictHandling::Error,
            path_filter: vec![],
            dependencies_crate_manifest_path: None,
            dependency_builder: DependencyBuilder::default(),
            quiet: false,
            num_test_threads: std::thread::available_parallelism().unwrap(),
            out_dir: None,
            edition: Some("2021".into()),
        }
    }
}

impl Config {
    /// Replace all occurrences of a path in stderr with a byte string.
    pub fn path_stderr_filter(
        &mut self,
        path: &Path,
        replacement: &'static (impl AsRef<[u8]> + ?Sized),
    ) {
        let pattern = path.canonicalize().unwrap();
        self.stderr_filters
            .push((pattern.parent().unwrap().into(), replacement.as_ref()));
    }

    /// Replace all occurrences of a regex pattern in stderr with a byte string.
    pub fn stderr_filter(
        &mut self,
        pattern: &str,
        replacement: &'static (impl AsRef<[u8]> + ?Sized),
    ) {
        self.stderr_filters
            .push((Regex::new(pattern).unwrap().into(), replacement.as_ref()));
    }

    /// Replace all occurrences of a regex pattern in stdout with a byte string.
    pub fn stdout_filter(
        &mut self,
        pattern: &str,
        replacement: &'static (impl AsRef<[u8]> + ?Sized),
    ) {
        self.stdout_filters
            .push((Regex::new(pattern).unwrap().into(), replacement.as_ref()));
    }

    fn build_dependencies_and_link_them(&mut self) -> Result<()> {
        let dependencies = build_dependencies(self)?;
        for (name, artifacts) in dependencies.dependencies {
            for dependency in artifacts {
                self.args.push("--extern".into());
                let mut dep = OsString::from(&name);
                dep.push("=");
                dep.push(dependency);
                self.args.push(dep);
            }
        }
        for import_path in dependencies.import_paths {
            self.args.push("-L".into());
            self.args.push(import_path.into());
        }
        Ok(())
    }

    /// Make sure we have the host and target triples.
    pub fn fill_host_and_target(&mut self) -> Result<()> {
        if self.host.is_none() {
            self.host = Some(
                rustc_version::VersionMeta::for_command(std::process::Command::new(&self.program))
                    .map_err(|err| {
                        color_eyre::eyre::Report::new(err).wrap_err(format!(
                            "failed to parse rustc version info: {}",
                            self.program.display()
                        ))
                    })?
                    .host,
            );
        }
        if self.target.is_none() {
            self.target = Some(self.host.clone().unwrap());
        }
        Ok(())
    }

    fn has_asm_support(&self) -> bool {
        static ASM_SUPPORTED_ARCHS: &[&str] = &[
            "x86", "x86_64", "arm", "aarch64", "riscv32",
            "riscv64",
            // These targets require an additional asm_experimental_arch feature.
            // "nvptx64", "hexagon", "mips", "mips64", "spirv", "wasm32",
        ];
        ASM_SUPPORTED_ARCHS
            .iter()
            .any(|arch| self.target.as_ref().unwrap().contains(arch))
    }
}

#[derive(Debug, Clone)]
/// The command line program that builds dependencies. Currently really only supports
/// `cargo`-like things.
pub struct DependencyBuilder {
    /// Path to the binary. Defaults to the `CARGO` env var or just a program named `cargo`
    pub program: PathBuf,
    /// Arguments to the binary. Defaults to `build`.
    pub args: Vec<String>,
    /// Environment variables to set before running the binary.
    pub envs: Vec<(String, OsString)>,
}

impl Default for DependencyBuilder {
    fn default() -> Self {
        Self {
            program: PathBuf::from(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())),
            args: vec!["build".into()],
            envs: vec![],
        }
    }
}

#[derive(Debug, Copy, Clone)]
/// The different options for what to do when stdout/stderr files differ from the actual output.
pub enum OutputConflictHandling {
    /// The default: emit a diff of the expected/actual output.
    Error,
    /// Ignore mismatches in the stderr/stdout files.
    Ignore,
    /// Instead of erroring if the stderr/stdout differs from the expected
    /// automatically replace it with the found output (after applying filters).
    Bless,
}

/// A filter's match rule.
#[derive(Clone, Debug)]
pub enum Match {
    /// If the regex matches, the filter applies
    Regex(Regex),
    /// If the exact byte sequence is found, the filter applies
    Exact(Vec<u8>),
}
impl Match {
    fn replace_all<'a>(&self, text: &'a [u8], replacement: &[u8]) -> Cow<'a, [u8]> {
        match self {
            Match::Regex(regex) => regex.replace_all(text, replacement),
            Match::Exact(needle) => text.replace(needle, replacement).into(),
        }
    }
}

impl From<&'_ Path> for Match {
    fn from(v: &Path) -> Self {
        let mut v = v.display().to_string();
        // Normalize away windows canonicalized paths.
        if v.starts_with(r#"\\?\"#) {
            v.drain(0..4);
        }
        let mut v = v.into_bytes();
        // Normalize paths on windows to use slashes instead of backslashes,
        // So that paths are rendered the same on all systems.
        for c in &mut v {
            if *c == b'\\' {
                *c = b'/';
            }
        }
        Self::Exact(v)
    }
}

impl From<Regex> for Match {
    fn from(v: Regex) -> Self {
        Self::Regex(v)
    }
}

/// Replacements to apply to output files.
pub type Filter = Vec<(Match, &'static [u8])>;

/// Run all tests as described in the config argument.
pub fn run_tests(mut config: Config) -> Result<()> {
    eprintln!("   Compiler flags: {:?}", config.args);

    config.build_dependencies_and_link_them()?;

    run_tests_generic(
        config,
        |path| path.extension().map(|ext| ext == "rs").unwrap_or(false),
        |_, _| None,
    )
}

/// Run a single file, with the settings from the `config` argument. Ignores various
/// settings from `Config` that relate to finding test files.
pub fn run_file(mut config: Config, path: &Path) -> Result<std::process::Output> {
    config.build_dependencies_and_link_them()?;

    let comments =
        Comments::parse_file(path)?.map_err(|errors| color_eyre::eyre::eyre!("{errors:#?}"))?;
    let mut errors = vec![];
    let result = build_command(
        path,
        &config,
        "",
        &comments,
        config.out_dir.as_deref(),
        &mut errors,
    )
    .output()
    .wrap_err_with(|| format!("path `{}` is not an executable", config.program.display()));
    assert!(errors.is_empty(), "{errors:#?}");
    result
}

#[allow(clippy::large_enum_variant)]
enum TestResult {
    Ok,
    Ignored,
    Filtered,
    Errored {
        command: Command,
        errors: Vec<Error>,
        stderr: Vec<u8>,
    },
}

struct TestRun {
    result: TestResult,
    path: PathBuf,
    revision: String,
}

/// A version of `run_tests` that allows more fine-grained control over running tests.
pub fn run_tests_generic(
    mut config: Config,
    file_filter: impl Fn(&Path) -> bool + Sync,
    per_file_config: impl Fn(&Config, &Path) -> Option<Config> + Sync,
) -> Result<()> {
    config.fill_host_and_target()?;

    // A channel for files to process
    let (submit, receive) = unbounded();

    let mut results = vec![];

    thread::scope(|s| -> Result<()> {
        // Create a thread that is in charge of walking the directory and submitting jobs.
        // It closes the channel when it is done.
        s.spawn(|| {
            let mut todo = VecDeque::new();
            todo.push_back(config.root_dir.clone());
            while let Some(path) = todo.pop_front() {
                if path.is_dir() {
                    if path.file_name().unwrap() == "auxiliary" {
                        continue;
                    }
                    // Enqueue everything inside this directory.
                    // We want it sorted, to have some control over scheduling of slow tests.
                    let mut entries = std::fs::read_dir(path)
                        .unwrap()
                        .collect::<Result<Vec<_>, _>>()
                        .unwrap();
                    entries.sort_by_key(|e| e.file_name());
                    for entry in entries {
                        todo.push_back(entry.path());
                    }
                } else if file_filter(&path) {
                    // Forward .rs files to the test workers.
                    submit.send(path).unwrap();
                }
            }
            // There will be no more jobs. This signals the workers to quit.
            // (This also ensures `submit` is moved into this closure.)
            drop(submit);
        });

        // A channel for the messages emitted by the individual test threads.
        // Used to produce live updates while running the tests.
        let (finished_files_sender, finished_files_recv) = unbounded::<TestRun>();

        s.spawn(|| {
            let _group = github_actions::group("run tests");
            if config.quiet {
                for (i, run) in finished_files_recv.into_iter().enumerate() {
                    // Humans start counting at 1
                    let i = i + 1;
                    match run.result {
                        TestResult::Ok => eprint!("{}", ".".green()),
                        TestResult::Errored { .. } => eprint!("{}", "F".red().bold()),
                        TestResult::Ignored => eprint!("{}", "i".yellow()),
                        TestResult::Filtered => {}
                    }
                    if i % 100 == 0 {
                        eprintln!(" {i}");
                    }
                    results.push(run);
                }
            } else {
                for run in finished_files_recv {
                    let result = match run.result {
                        TestResult::Ok => Some("ok".green()),
                        TestResult::Errored { .. } => Some("FAILED".red().bold()),
                        TestResult::Ignored => Some("ignored (in-test comment)".yellow()),
                        TestResult::Filtered => None,
                    };
                    if let Some(result) = result {
                        eprint!(
                            "{}{} ... ",
                            run.path.display(),
                            if run.revision.is_empty() {
                                "".into()
                            } else {
                                format!(" ({})", run.revision)
                            }
                        );
                        eprintln!("{result}");
                    }
                    results.push(run);
                }
            }
        });

        let mut threads = vec![];

        // Create N worker threads that receive files to test.
        for _ in 0..config.num_test_threads.get() {
            let finished_files_sender = finished_files_sender.clone();
            threads.push(s.spawn(|| -> Result<()> {
                let finished_files_sender = finished_files_sender;
                for path in &receive {
                    let maybe_config;
                    let config = match per_file_config(&config, &path) {
                        None => &config,
                        Some(config) => {
                            maybe_config = config;
                            &maybe_config
                        }
                    };
                    let result =
                        match std::panic::catch_unwind(|| parse_and_test_file(&path, config)) {
                            Ok(res) => res,
                            Err(err) => {
                                finished_files_sender.send(TestRun {
                                    result: TestResult::Errored {
                                        command: Command::new("<unknown>"),
                                        errors: vec![Error::Bug(
                                            *Box::<dyn std::any::Any + Send + 'static>::downcast::<
                                                String,
                                            >(err)
                                            .unwrap(),
                                        )],
                                        stderr: vec![],
                                    },
                                    path,
                                    revision: String::new(),
                                })?;
                                continue;
                            }
                        };
                    for result in result {
                        finished_files_sender.send(result)?;
                    }
                }
                Ok(())
            }));
        }

        for thread in threads {
            thread.join().unwrap()?;
        }
        Ok(())
    })?;

    let mut failures = vec![];
    let mut succeeded = 0;
    let mut ignored = 0;
    let mut filtered = 0;

    for run in results {
        match run.result {
            TestResult::Ok => succeeded += 1,
            TestResult::Ignored => ignored += 1,
            TestResult::Filtered => filtered += 1,
            TestResult::Errored {
                command,
                errors,
                stderr,
            } => failures.push((run.path, command, run.revision, errors, stderr)),
        }
    }

    // Print all errors in a single thread to show reliable output
    if !failures.is_empty() {
        for (path, cmd, revision, errors, stderr) in &failures {
            let _group = github_actions::group(format_args!("{}:{revision}", path.display()));

            eprintln!();
            let path = path.display().to_string();
            eprint!("{}", path.underline().bold());
            let revision = if revision.is_empty() {
                String::new()
            } else {
                format!(" (revision `{revision}`)")
            };
            eprint!("{revision}");
            eprint!(" {}", "FAILED:".red().bold());
            eprintln!();
            eprintln!("command: {cmd:?}");
            eprintln!();
            for error in errors {
                match error {
                    Error::ExitStatus {
                        mode,
                        status,
                        expected,
                    } => {
                        github_actions::error(
                            &path,
                            format!("{mode} test{revision} got {status}, but expected {expected}"),
                        );
                        eprintln!("{mode} test got {status}, but expected {expected}")
                    }
                    Error::Command { kind, status } => {
                        github_actions::error(
                            &path,
                            format!("{kind}{revision} failed with {status}"),
                        );
                        eprintln!("{kind} failed with {status}");
                    }
                    Error::PatternNotFound {
                        pattern,
                        definition_line,
                    } => {
                        github_actions::error(&path, format!("title=Pattern not found{revision}"))
                            .line(*definition_line);
                        match pattern {
                            Pattern::SubString(s) => {
                                eprintln!("substring `{s}` {} in stderr output", "not found".red())
                            }
                            Pattern::Regex(r) => {
                                eprintln!("`/{r}/` does {} stderr output", "not match".red())
                            }
                        }
                        eprintln!(
                            "expected because of pattern here: {}",
                            format!("{path}:{definition_line}").bold()
                        );
                    }
                    Error::NoPatternsFound => {
                        github_actions::error(
                            &path,
                            format!("no error patterns found in fail test{revision}"),
                        );
                        eprintln!("{}", "no error patterns found in fail test".red());
                    }
                    Error::PatternFoundInPassTest => {
                        github_actions::error(
                            &path,
                            format!("error pattern found in pass test{revision}"),
                        );
                        eprintln!("{}", "error pattern found in pass test".red())
                    }
                    Error::OutputDiffers {
                        path: output_path,
                        actual,
                        expected,
                    } => {
                        let mut err = github_actions::error(
                            if expected.is_empty() {
                                path.clone()
                            } else {
                                output_path.display().to_string()
                            },
                            "actual output differs from expected",
                        );
                        writeln!(err, "```diff").unwrap();
                        for r in ::diff::lines(expected.to_str().unwrap(), actual.to_str().unwrap())
                        {
                            match r {
                                ::diff::Result::Both(l, r) => {
                                    if l != r {
                                        writeln!(err, "-{l}").unwrap();
                                        writeln!(err, "+{r}").unwrap();
                                    } else {
                                        writeln!(err, " {l}").unwrap()
                                    }
                                }
                                ::diff::Result::Left(l) => {
                                    writeln!(err, "-{l}").unwrap();
                                }
                                ::diff::Result::Right(r) => {
                                    writeln!(err, "+{r}").unwrap();
                                }
                            }
                        }
                        writeln!(err, "```").unwrap();
                        eprintln!("{}", "actual output differed from expected".underline());
                        eprintln!("{}", format!("--- {}", output_path.display()).red());
                        eprintln!("{}", "+++ <stderr output>".green());
                        diff::print_diff(expected, actual);
                    }
                    Error::ErrorsWithoutPattern { path: None, msgs } => {
                        eprintln!(
                            "There were {} unmatched diagnostics that occurred outside the testfile and had no pattern",
                            msgs.len(),
                        );
                        for Message { level, message } in msgs {
                            eprintln!("    {level:?}: {message}")
                        }
                        let mut err = github_actions::error(
                            &path,
                            format!("Unmatched diagnostics outside the testfile{revision}"),
                        );
                        for Message { level, message } in msgs {
                            writeln!(err, "{level:?}: {message}").unwrap();
                        }
                    }
                    Error::ErrorsWithoutPattern {
                        path: Some((path, line)),
                        msgs,
                    } => {
                        let path = path.display();
                        eprintln!(
                            "There were {} unmatched diagnostics at {path}:{line}",
                            msgs.len(),
                        );
                        for Message { level, message } in msgs {
                            eprintln!("    {level:?}: {message}")
                        }
                        let mut err = github_actions::error(
                            &path,
                            format!("Unmatched diagnostics{revision}"),
                        )
                        .line(*line);
                        for Message { level, message } in msgs {
                            writeln!(err, "{level:?}: {message}").unwrap();
                        }
                    }
                    Error::InvalidComment { msg, line } => {
                        let mut err =
                            github_actions::error(&path, format!("Could not parse comment"))
                                .line(*line);
                        writeln!(err, "{msg}").unwrap();
                        eprintln!("Could not parse comment in {path}:{line} because\n{msg}",)
                    }
                    Error::Bug(msg) => {
                        eprintln!("A bug in `ui_test` occurred: {msg}");
                    }
                }
                eprintln!();
            }
            eprintln!("full stderr:");
            std::io::stderr().write_all(stderr).unwrap();
            eprintln!();
            eprintln!();
        }
        eprintln!("{}", "FAILURES:".red().underline().bold());
        for (path, _cmd, _revision, _errors, _stderr) in &failures {
            eprintln!("    {}", path.display());
        }
        eprintln!();
        eprintln!(
            "test result: {}. {} tests failed, {} tests passed, {} ignored, {} filtered out",
            "FAIL".red(),
            failures.len().to_string().red().bold(),
            succeeded.to_string().green(),
            ignored.to_string().yellow(),
            filtered.to_string().yellow(),
        );
        std::process::exit(1);
    }
    eprintln!();
    eprintln!(
        "test result: {}. {} tests passed, {} ignored, {} filtered out",
        "ok".green(),
        succeeded.to_string().green(),
        ignored.to_string().yellow(),
        filtered.to_string().yellow(),
    );
    eprintln!();
    Ok(())
}

fn parse_and_test_file(path: &Path, config: &Config) -> Vec<TestRun> {
    if !config.path_filter.is_empty() {
        let path_display = path.display().to_string();
        if !config
            .path_filter
            .iter()
            .any(|filter| path_display.contains(filter))
        {
            return vec![TestRun {
                result: TestResult::Filtered,
                path: path.into(),
                revision: "".into(),
            }];
        }
    }
    let comments = match parse_comments_in_file(path) {
        Ok(comments) => comments,
        Err((stderr, errors)) => {
            return vec![TestRun {
                result: TestResult::Errored {
                    command: Command::new("parse comments"),
                    errors,
                    stderr,
                },
                path: path.into(),
                revision: "".into(),
            }]
        }
    };
    // Run the test for all revisions
    comments
        .revisions
        .clone()
        .unwrap_or_else(|| vec![String::new()])
        .into_iter()
        .map(|revision| {
            // Ignore file if only/ignore rules do (not) apply
            if !test_file_conditions(&comments, config, &revision) {
                return TestRun {
                    result: TestResult::Ignored,
                    path: path.into(),
                    revision,
                };
            }
            let (command, errors, stderr) = run_test(path, config, &revision, &comments);
            let result = if errors.is_empty() {
                TestResult::Ok
            } else {
                TestResult::Errored {
                    command,
                    errors,
                    stderr,
                }
            };
            TestRun {
                result,
                revision,
                path: path.into(),
            }
        })
        .collect()
}

fn parse_comments_in_file(path: &Path) -> Result<Comments, (Vec<u8>, Vec<Error>)> {
    match Comments::parse_file(path) {
        Ok(Ok(comments)) => Ok(comments),
        Ok(Err(errors)) => Err((vec![], errors)),
        Err(err) => Err((format!("{err:?}").into(), vec![])),
    }
}

#[derive(Debug)]
enum Error {
    /// Got an invalid exit status for the given mode.
    ExitStatus {
        mode: Mode,
        status: ExitStatus,
        expected: i32,
    },
    PatternNotFound {
        pattern: Pattern,
        definition_line: usize,
    },
    /// A ui test checking for failure does not have any failure patterns
    NoPatternsFound,
    /// A ui test checking for success has failure patterns
    PatternFoundInPassTest,
    /// Stderr/Stdout differed from the `.stderr`/`.stdout` file present.
    OutputDiffers {
        path: PathBuf,
        actual: Vec<u8>,
        expected: Vec<u8>,
    },
    ErrorsWithoutPattern {
        msgs: Vec<Message>,
        path: Option<(PathBuf, usize)>,
    },
    InvalidComment {
        msg: String,
        line: usize,
    },
    Command {
        kind: String,
        status: ExitStatus,
    },
    /// This catches crashes of ui tests and reports them along the failed test.
    Bug(String),
}

type Errors = Vec<Error>;

fn build_command(
    path: &Path,
    config: &Config,
    revision: &str,
    comments: &Comments,
    out_dir: Option<&Path>,
    errors: &mut Vec<Error>,
) -> Command {
    let mut cmd = Command::new(&config.program);
    if let Some(out_dir) = out_dir {
        cmd.arg("--out-dir");
        cmd.arg(out_dir);
    }
    cmd.args(config.args.iter());
    for (var, val) in config.envs.iter() {
        if let Some(val) = val {
            cmd.env(var, val);
        } else {
            cmd.env_remove(var);
        }
    }
    cmd.arg(path);
    if !revision.is_empty() {
        cmd.arg(format!("--cfg={revision}"));
    }
    for arg in comments
        .for_revision(revision)
        .flat_map(|r| r.compile_flags.iter())
    {
        cmd.arg(arg);
    }
    let edition = comments
        .find_one_for_revision(
            revision,
            |r| r.edition.as_ref(),
            |&(_, line)| {
                errors.push(Error::InvalidComment {
                    msg: "`edition` specified twice".into(),
                    line,
                })
            },
        )
        .map(|(e, _)| e.as_str())
        .or(config.edition.as_deref());
    if let Some(edition) = edition {
        cmd.arg("--edition").arg(edition);
    }
    cmd.args(config.trailing_args.iter());
    cmd.envs(
        comments
            .for_revision(revision)
            .flat_map(|r| r.env_vars.iter())
            .map(|(k, v)| (k, v)),
    );

    cmd
}

fn run_test(
    path: &Path,
    config: &Config,
    revision: &str,
    comments: &Comments,
) -> (Command, Errors, Vec<u8>) {
    let mut extra_args = vec![];
    let aux_dir = path.parent().unwrap().join("auxiliary");
    for rev in comments.for_revision(revision) {
        for (aux, kind) in &rev.aux_builds {
            let aux_file = aux_dir.join(aux);
            let comments = match parse_comments_in_file(&aux_file) {
                Ok(comments) => comments,
                Err((msg, mut errors)) => {
                    return (
                        build_command(path, config, revision, comments, None, &mut errors),
                        errors,
                        msg,
                    )
                }
            };
            assert_eq!(comments.revisions, None);
            // Put aux builds into a separate directory per test so that
            // tests running in parallel but building the same aux build don't conflict.
            // FIXME: put aux builds into the regular build queue.
            let out_dir = config
                .out_dir
                .clone()
                .unwrap_or_default()
                .join(path.with_extension(""));

            let mut errors = vec![];

            let mut aux_cmd = build_command(
                &aux_file,
                config,
                revision,
                &comments,
                Some(&out_dir),
                &mut errors,
            );

            if !errors.is_empty() {
                return (aux_cmd, errors, vec![]);
            }

            aux_cmd.arg("--crate-type").arg(kind);
            aux_cmd.arg("--emit=link");
            let filename = aux.file_stem().unwrap().to_str().unwrap();
            let output = aux_cmd.output().unwrap();
            if !output.status.success() {
                let error = Error::Command {
                    kind: format!("auxiliary build for `{}`", path.display()),
                    status: output.status,
                };
                return (
                    aux_cmd,
                    vec![error],
                    rustc_stderr::process(path, &output.stderr).rendered,
                );
            }

            // Now run the command again to fetch the output filenames
            aux_cmd.arg("--print").arg("file-names");
            let output = aux_cmd.output().unwrap();
            assert!(output.status.success());

            for file in output.stdout.lines() {
                let file = std::str::from_utf8(file).unwrap();
                let crate_name = filename.replace('-', "_");
                let path = out_dir.join(file);
                extra_args.push("--extern".into());
                extra_args.push(format!("{crate_name}={}", path.display()));
            }
        }
    }

    let mut errors = vec![];

    let mut cmd = build_command(
        path,
        config,
        revision,
        comments,
        config.out_dir.as_deref(),
        &mut errors,
    );
    cmd.args(&extra_args);

    let output = cmd
        .output()
        .unwrap_or_else(|_| panic!("could not execute {cmd:?}"));
    let status_check = config
        .mode
        .maybe_override(comments, revision, &mut errors)
        .ok(output.status);
    errors.extend(status_check);
    if output.status.code() == Some(101) && !matches!(config.mode, Mode::Panic | Mode::Yolo) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        errors.push(Error::Bug(format!(
            "test panicked: stderr:\n{stderr}\nstdout:\n{stdout}",
        )));
        return (cmd, errors, vec![]);
    }
    // Always remove annotation comments from stderr.
    let diagnostics = rustc_stderr::process(path, &output.stderr);
    let rustfixed = comments
        .for_revision(revision)
        .any(|rev| rev.run_rustfix)
        .then(|| {
            run_rustfix(
                &output.stderr,
                path,
                comments,
                revision,
                config,
                extra_args,
                &mut errors,
            )
        });
    let stderr = check_test_result(
        path,
        config,
        revision,
        comments,
        &mut errors,
        &output.stdout,
        diagnostics,
    );
    if let Some((mut rustfix, rustfix_path)) = rustfixed {
        // picking the crate name from the file name is problematic when `.revision_name` is inserted
        rustfix.arg("--crate-name").arg(
            path.file_stem()
                .unwrap()
                .to_str()
                .unwrap()
                .replace('-', "_"),
        );
        let output = rustfix.output().unwrap();
        if !output.status.success() {
            errors.push(Error::Command {
                kind: "rustfix".into(),
                status: output.status,
            });
            return (
                rustfix,
                errors,
                rustc_stderr::process(&rustfix_path, &output.stderr).rendered,
            );
        }
    }
    (cmd, errors, stderr)
}

fn run_rustfix(
    stderr: &[u8],
    path: &Path,
    comments: &Comments,
    revision: &str,
    config: &Config,
    extra_args: Vec<String>,
    errors: &mut Vec<Error>,
) -> (Command, PathBuf) {
    let input = std::str::from_utf8(stderr).unwrap();
    let suggestions = rustfix::get_suggestions_from_json(
        input,
        &HashSet::new(),
        rustfix::Filter::MachineApplicableOnly,
    )
    .unwrap_or_else(|err| {
        panic!("could not deserialize diagnostics json for rustfix {err}:{input}")
    });
    let fixed_code =
        rustfix::apply_suggestions(&std::fs::read_to_string(path).unwrap(), &suggestions)
            .unwrap_or_else(|e| {
                panic!(
                    "failed to apply suggestions for {:?} with rustfix: {e}",
                    path.display()
                )
            });
    let rustfix_comments = Comments {
        revisions: None,
        revisioned: std::iter::once((
            vec![],
            Revisioned {
                ignore: vec![],
                only: vec![],
                stderr_per_bitwidth: false,
                compile_flags: comments
                    .for_revision(revision)
                    .flat_map(|r| r.compile_flags.iter().cloned())
                    .collect(),
                env_vars: comments
                    .for_revision(revision)
                    .flat_map(|r| r.env_vars.iter().cloned())
                    .collect(),
                normalize_stderr: vec![],
                error_patterns: vec![],
                error_matches: vec![],
                require_annotations_for_level: None,
                run_rustfix: false,
                aux_builds: comments
                    .for_revision(revision)
                    .flat_map(|r| r.aux_builds.iter().cloned())
                    .collect(),
                edition: None,
                mode: Some((Mode::Pass, 0)),
                needs_asm_support: false,
            },
        ))
        .collect(),
    };
    let path = check_output(
        fixed_code.as_bytes(),
        path,
        &mut vec![],
        revised(revision, "fixed"),
        &Filter::default(),
        config,
        &rustfix_comments,
        revision,
    );

    let mut cmd = build_command(
        &path,
        config,
        revision,
        &rustfix_comments,
        config.out_dir.as_deref(),
        errors,
    );
    cmd.args(extra_args);
    (cmd, path)
}

fn revised(revision: &str, extension: &str) -> String {
    if revision.is_empty() {
        extension.to_string()
    } else {
        format!("{revision}.{extension}")
    }
}

fn check_test_result(
    path: &Path,
    config: &Config,
    revision: &str,
    comments: &Comments,
    errors: &mut Errors,
    stdout: &[u8],
    diagnostics: Diagnostics,
) -> Vec<u8> {
    // Check output files (if any)
    // Check output files against actual output
    check_output(
        &diagnostics.rendered,
        path,
        errors,
        revised(revision, "stderr"),
        &config.stderr_filters,
        config,
        comments,
        revision,
    );
    check_output(
        stdout,
        path,
        errors,
        revised(revision, "stdout"),
        &config.stdout_filters,
        config,
        comments,
        revision,
    );
    // Check error annotations in the source against output
    check_annotations(
        diagnostics.messages,
        diagnostics.messages_from_unknown_file_or_line,
        path,
        errors,
        config,
        revision,
        comments,
    );
    diagnostics.rendered
}

fn check_annotations(
    mut messages: Vec<Vec<Message>>,
    mut messages_from_unknown_file_or_line: Vec<Message>,
    path: &Path,
    errors: &mut Errors,
    config: &Config,
    revision: &str,
    comments: &Comments,
) {
    let error_patterns = comments
        .for_revision(revision)
        .flat_map(|r| r.error_patterns.iter());

    let mut seen_error_match = false;
    for (error_pattern, definition_line) in error_patterns {
        seen_error_match = true;
        // first check the diagnostics messages outside of our file. We check this first, so that
        // you can mix in-file annotations with //@error-pattern annotations, even if there is overlap
        // in the messages.
        if let Some(i) = messages_from_unknown_file_or_line
            .iter()
            .position(|msg| error_pattern.matches(&msg.message))
        {
            messages_from_unknown_file_or_line.remove(i);
        } else {
            errors.push(Error::PatternNotFound {
                pattern: error_pattern.clone(),
                definition_line: *definition_line,
            });
        }
    }

    // The order on `Level` is such that `Error` is the highest level.
    // We will ensure that *all* diagnostics of level at least `lowest_annotation_level`
    // are matched.
    let mut lowest_annotation_level = Level::Error;
    for &ErrorMatch {
        ref pattern,
        definition_line,
        line,
        level,
    } in comments
        .for_revision(revision)
        .flat_map(|r| r.error_matches.iter())
    {
        seen_error_match = true;
        // If we found a diagnostic with a level annotation, make sure that all
        // diagnostics of that level have annotations, even if we don't end up finding a matching diagnostic
        // for this pattern.
        lowest_annotation_level = std::cmp::min(lowest_annotation_level, level);

        if let Some(msgs) = messages.get_mut(line) {
            let found = msgs
                .iter()
                .position(|msg| pattern.matches(&msg.message) && msg.level == level);
            if let Some(found) = found {
                msgs.remove(found);
                continue;
            }
        }

        errors.push(Error::PatternNotFound {
            pattern: pattern.clone(),
            definition_line,
        });
    }

    let required_annotation_level = comments
        .find_one_for_revision(
            revision,
            |r| r.require_annotations_for_level,
            |_| {
                errors.push(Error::InvalidComment {
                    msg: "`require_annotations_for_level` specified twice for same revision".into(),
                    line: 0,
                })
            },
        )
        .unwrap_or(lowest_annotation_level);
    let filter = |mut msgs: Vec<Message>| -> Vec<_> {
        msgs.retain(|msg| msg.level >= required_annotation_level);
        msgs
    };

    let messages_from_unknown_file_or_line = filter(messages_from_unknown_file_or_line);
    if !messages_from_unknown_file_or_line.is_empty() {
        errors.push(Error::ErrorsWithoutPattern {
            path: None,
            msgs: messages_from_unknown_file_or_line,
        });
    }

    for (line, msgs) in messages.into_iter().enumerate() {
        let msgs = filter(msgs);
        if !msgs.is_empty() {
            errors.push(Error::ErrorsWithoutPattern {
                path: Some((path.to_path_buf(), line)),
                msgs,
            });
        }
    }

    let mode = config.mode.maybe_override(comments, revision, errors);

    match (mode, seen_error_match) {
        (Mode::Pass, true) | (Mode::Panic, true) => errors.push(Error::PatternFoundInPassTest),
        (
            Mode::Fail {
                require_patterns: true,
            },
            false,
        ) => errors.push(Error::NoPatternsFound),
        _ => {}
    }
}

fn check_output(
    output: &[u8],
    path: &Path,
    errors: &mut Errors,
    kind: String,
    filters: &Filter,
    config: &Config,
    comments: &Comments,
    revision: &str,
) -> PathBuf {
    let target = config.target.as_ref().unwrap();
    let output = normalize(path, output, filters, comments, revision);
    let path = output_path(path, comments, kind, target, revision);
    match config.output_conflict_handling {
        OutputConflictHandling::Bless => {
            if output.is_empty() {
                let _ = std::fs::remove_file(&path);
            } else {
                std::fs::write(&path, &output).unwrap();
            }
        }
        OutputConflictHandling::Error => {
            let expected_output = std::fs::read(&path).unwrap_or_default();
            if output != expected_output {
                errors.push(Error::OutputDiffers {
                    path: path.clone(),
                    actual: output,
                    expected: expected_output,
                });
            }
        }
        OutputConflictHandling::Ignore => {}
    }
    path
}

fn output_path(
    path: &Path,
    comments: &Comments,
    kind: String,
    target: &str,
    revision: &str,
) -> PathBuf {
    if comments
        .for_revision(revision)
        .any(|r| r.stderr_per_bitwidth)
    {
        return path.with_extension(format!("{}bit.{kind}", get_pointer_width(target)));
    }
    path.with_extension(kind)
}

fn test_condition(condition: &Condition, config: &Config) -> bool {
    let target = config.target.as_ref().unwrap();
    match condition {
        Condition::Bitwidth(bits) => get_pointer_width(target) == *bits,
        Condition::Target(t) => target.contains(t),
        Condition::Host(t) => config.host.as_ref().unwrap().contains(t),
        Condition::OnHost => target == config.host.as_ref().unwrap(),
    }
}

/// Returns whether according to the in-file conditions, this file should be run.
fn test_file_conditions(comments: &Comments, config: &Config, revision: &str) -> bool {
    if comments
        .for_revision(revision)
        .flat_map(|r| r.ignore.iter())
        .any(|c| test_condition(c, config))
    {
        return false;
    }
    if comments
        .for_revision(revision)
        .any(|r| r.needs_asm_support && !config.has_asm_support())
    {
        return false;
    }
    comments
        .for_revision(revision)
        .flat_map(|r| r.only.iter())
        .all(|c| test_condition(c, config))
}

// Taken 1:1 from compiletest-rs
fn get_pointer_width(triple: &str) -> u8 {
    if (triple.contains("64") && !triple.ends_with("gnux32") && !triple.ends_with("gnu_ilp32"))
        || triple.starts_with("s390x")
    {
        64
    } else if triple.starts_with("avr") {
        16
    } else {
        32
    }
}

fn normalize(
    path: &Path,
    text: &[u8],
    filters: &Filter,
    comments: &Comments,
    revision: &str,
) -> Vec<u8> {
    // Useless paths
    let path_filter = (Match::from(path.parent().unwrap()), b"$DIR" as &[u8]);
    let filters = filters.iter().chain(std::iter::once(&path_filter));
    let mut text = text.to_owned();
    if let Some(lib_path) = option_env!("RUSTC_LIB_PATH") {
        text = text.replace(lib_path, "RUSTLIB");
    }

    for (regex, replacement) in filters {
        text = regex.replace_all(&text, replacement).into_owned();
    }

    for (from, to) in comments
        .for_revision(revision)
        .flat_map(|r| r.normalize_stderr.iter())
    {
        text = from.replace_all(&text, to).into_owned();
    }
    text
}

#[derive(Copy, Clone, Debug)]
/// Decides what is expected of each test's exit status.
pub enum Mode {
    /// The test passes a full execution of the rustc driver
    Pass,
    /// The rustc driver panicked
    Panic,
    /// The rustc driver emitted an error
    Fail {
        /// Whether failing tests must have error patterns. Set to false if you just care about .stderr output.
        require_patterns: bool,
    },
    /// Run the tests, but always pass them as long as all annotations are satisfied and stderr files match.
    Yolo,
}

impl Mode {
    fn ok(self, status: ExitStatus) -> Errors {
        let expected = match self {
            Mode::Pass => 0,
            Mode::Panic => 101,
            Mode::Fail { .. } => 1,
            Mode::Yolo => return vec![],
        };
        if status.code() == Some(expected) {
            vec![]
        } else {
            vec![Error::ExitStatus {
                mode: self,
                status,
                expected,
            }]
        }
    }
    fn maybe_override(self, comments: &Comments, revision: &str, errors: &mut Vec<Error>) -> Self {
        comments
            .find_one_for_revision(
                revision,
                |r| r.mode.as_ref(),
                |&(_, line)| {
                    errors.push(Error::InvalidComment {
                        msg: "multiple mode changes found".into(),
                        line,
                    })
                },
            )
            .map(|&(mode, _)| mode)
            .unwrap_or(self)
    }
}

impl Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mode::Pass => write!(f, "pass"),
            Mode::Panic => write!(f, "panic"),
            Mode::Fail {
                require_patterns: _,
            } => write!(f, "fail"),
            Mode::Yolo => write!(f, "yolo"),
        }
    }
}
