//! Command execution abstraction for driving the LVM toolchain.
//!
//! Every external command (`lsblk`, `pvcreate`, `vgcreate`, `lvcreate`,
//! `lvextend`, `lvremove`, `blkdiscard`, ...) is executed through a
//! [`CommandRunner`]. The production [`RealRunner`] uses
//! [`std::process::Command`] directly — never a shell — so arguments can
//! never be re-interpreted. Tests script a [`FakeRunner`] instead, either
//! from a queue of outputs or from a closure keyed by program and args.
//!
//! Failures are honest: a non-zero exit is surfaced as
//! [`CommandOutput::success == false`] (the caller decides the typed error,
//! embedding a short [`CommandOutput::stderr_excerpt`]), and a failure to
//! even spawn the command is an `INTERNAL` [`ApiError`]. Nothing is ever
//! reported as successful without evidence.

use std::collections::VecDeque;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

use volvisor_types::{ApiError, ApiErrorCode};

/// Maximum length of a stderr excerpt embedded in error details.
pub const STDERR_EXCERPT_MAX_CHARS: usize = 256;

/// Captured output of one executed command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandOutput {
    /// Standard output (UTF-8, lossily decoded; the tools emit text/JSON).
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
    /// Whether the command exited with a success status.
    pub success: bool,
}

impl CommandOutput {
    /// A successful output carrying `stdout`.
    #[must_use]
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: String::new(),
            success: true,
        }
    }

    /// A failed output carrying `stderr`.
    #[must_use]
    pub fn failure(stderr: impl Into<String>) -> Self {
        Self {
            stdout: String::new(),
            stderr: stderr.into(),
            success: false,
        }
    }

    /// A short, flattened excerpt of stderr for error details.
    ///
    /// Never the full blob (error details are logged and returned to
    /// callers); whitespace is collapsed and the result is capped at
    /// [`STDERR_EXCERPT_MAX_CHARS`] characters.
    #[must_use]
    pub fn stderr_excerpt(&self) -> String {
        let flattened = self.stderr.split_whitespace().collect::<Vec<_>>().join(" ");
        flattened.chars().take(STDERR_EXCERPT_MAX_CHARS).collect()
    }
}

/// One recorded command invocation (test introspection and assertions).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    /// Program that was (or would be) executed.
    pub program: String,
    /// Arguments passed to the program.
    pub args: Vec<String>,
}

/// Executes external commands without a shell.
///
/// Implementations must be `Send + Sync`: the provider holds the runner as
/// an `Arc<dyn CommandRunner>` behind the async trait surface.
pub trait CommandRunner: Send + Sync {
    /// Run `program` with `args`, returning the captured output.
    ///
    /// A non-zero exit status is *not* an `Err`: it is returned as
    /// [`CommandOutput::success == false`] so the caller can map it to a
    /// typed, honest error. Only failure to execute at all is an `Err`
    /// (`INTERNAL`).
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ApiError>;
}

/// Production runner: executes commands via [`std::process::Command`].
///
/// No shell is involved; arguments are passed verbatim, so values derived
/// from identifiers (volume names, paths) can never be re-parsed as shell
/// syntax.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealRunner;

impl CommandRunner for RealRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ApiError> {
        let output = Command::new(program).args(args).output().map_err(|e| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!("failed to execute {program}: {e}"),
            )
        })?;
        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            success: output.status.success(),
        })
    }
}

/// Scripted runner for tests.
///
/// Two modes exist:
///
/// - **queue mode** ([`FakeRunner::with_queue`]): outputs are consumed in
///   order regardless of the command; an exhausted queue is an `INTERNAL`
///   error (a test that under-scripts fails loudly, never silently);
/// - **closure mode** ([`FakeRunner::with_closure`]): the closure receives
///   program and args and returns the scripted output (or `None` for "not
///   scripted", which is an `INTERNAL` error). This is the easiest way to
///   simulate a stateful LVM for the conformance kit.
///
/// Every invocation is recorded and can be inspected via
/// [`FakeRunner::invocations`] to assert exactly which commands ran.
pub struct FakeRunner {
    invocations: Mutex<Vec<Invocation>>,
    mode: FakeMode,
}

/// The scripted-output closure of [`FakeRunner::with_closure`].
type ScriptFn = Box<dyn Fn(&str, &[&str]) -> Option<CommandOutput> + Send + Sync>;

enum FakeMode {
    Queue(Mutex<VecDeque<CommandOutput>>),
    Closure(ScriptFn),
}

impl FakeRunner {
    /// Queue-mode runner: outputs are returned in order.
    #[must_use]
    pub fn with_queue(outputs: impl IntoIterator<Item = CommandOutput>) -> Self {
        Self {
            invocations: Mutex::new(Vec::new()),
            mode: FakeMode::Queue(Mutex::new(outputs.into_iter().collect())),
        }
    }

    /// Closure-mode runner: the closure decides the output per invocation.
    #[must_use]
    pub fn with_closure(
        script: impl Fn(&str, &[&str]) -> Option<CommandOutput> + Send + Sync + 'static,
    ) -> Self {
        Self {
            invocations: Mutex::new(Vec::new()),
            mode: FakeMode::Closure(Box::new(script)),
        }
    }

    /// All invocations recorded so far, in order.
    #[must_use]
    pub fn invocations(&self) -> Vec<Invocation> {
        lock(&self.invocations)
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// The program names recorded so far (convenient partial assertions).
    #[must_use]
    pub fn programs(&self) -> Vec<String> {
        self.invocations()
            .into_iter()
            .map(|invocation| invocation.program)
            .collect()
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ApiError> {
        lock(&self.invocations)?.push(Invocation {
            program: program.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        });
        match &self.mode {
            FakeMode::Queue(queue) => lock(queue)?.pop_front().ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    format!("fake runner script queue exhausted at {program}"),
                )
            }),
            FakeMode::Closure(script) => script(program, args).ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    format!("fake runner has no scripted output for {program}"),
                )
            }),
        }
    }
}

/// Lock a mutex, mapping poisoning to an `INTERNAL` error (never a panic).
fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, ApiError> {
    mutex.lock().map_err(|_| {
        ApiError::new(
            ApiErrorCode::Internal,
            "command runner lock poisoned by a previous failure",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_mode_returns_outputs_in_order_and_records() {
        let runner = FakeRunner::with_queue([
            CommandOutput::success("first"),
            CommandOutput::success("second"),
        ]);
        let out = runner
            .run("vgs", &["--reportformat", "json"])
            .expect("run 1");
        assert!(out.success);
        assert_eq!(out.stdout, "first");
        let out = runner.run("lvs", &[]).expect("run 2");
        assert_eq!(out.stdout, "second");

        let invocations = runner.invocations();
        assert_eq!(invocations.len(), 2);
        assert_eq!(invocations[0].program, "vgs");
        assert_eq!(invocations[0].args, vec!["--reportformat", "json"]);

        // An exhausted queue is a loud failure, never a silent default.
        let err = runner.run("lvs", &[]).expect_err("queue exhausted");
        assert_eq!(err.code, ApiErrorCode::Internal);
    }

    #[test]
    fn closure_mode_sees_program_and_args() {
        let runner = FakeRunner::with_closure(|program, _args| {
            (program == "echo").then(|| CommandOutput::success("hi"))
        });
        let out = runner.run("echo", &[]).expect("scripted");
        assert_eq!(out.stdout, "hi");
        let err = runner.run("missing-cmd", &[]).expect_err("unscripted");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(err.detail.contains("missing-cmd"));
    }

    #[test]
    fn stderr_excerpt_is_flattened_and_capped() {
        let mut output = CommandOutput::failure("line one\nline two\n");
        assert_eq!(output.stderr_excerpt(), "line one line two");
        output.stderr = "x".repeat(STDERR_EXCERPT_MAX_CHARS + 100);
        assert_eq!(
            output.stderr_excerpt().chars().count(),
            STDERR_EXCERPT_MAX_CHARS
        );
    }
}
