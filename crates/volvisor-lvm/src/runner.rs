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
//!
//! [`RealRunner`] bounds every command with a watchdog timeout (60 s by
//! default, [`RealRunner::with_timeout`] otherwise): a hung child is
//! killed and surfaced as a typed `INTERNAL` timeout error instead of
//! blocking the caller forever. Commands still *execute* on the calling
//! thread (bounded by the timeout) — moving them to `spawn_blocking` is a
//! recorded follow-up, not part of this design.

use std::collections::VecDeque;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

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

/// Default per-command watchdog timeout (60 seconds).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Production runner: executes commands via [`std::process::Command`].
///
/// No shell is involved; arguments are passed verbatim, so values derived
/// from identifiers (volume names, paths) can never be re-parsed as shell
/// syntax.
///
/// Every command is bounded by a watchdog timeout ([`DEFAULT_TIMEOUT`] by
/// default, [`RealRunner::with_timeout`] to customize): a watchdog thread
/// sleeps for the timeout and, unless the command has finished by then,
/// kills the child. The kill/reap hand-off is guarded by a mutex around
/// the [`Child`] plus a done-flag, so the watchdog can never signal an
/// already-reaped process (no pid-reuse hazard) and no `unsafe` code is
/// required. A command that hits the timeout is reported as a typed
/// `INTERNAL` error naming the program and the timeout.
///
/// Commands still execute on the calling thread (bounded by the timeout);
/// refactoring the providers to call the runner from `spawn_blocking` is a
/// recorded follow-up.
#[derive(Debug, Clone, Copy)]
pub struct RealRunner {
    /// Watchdog timeout applied to every executed command.
    timeout: Duration,
}

impl Default for RealRunner {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl RealRunner {
    /// A runner whose commands are killed after `timeout`.
    #[must_use]
    pub fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }

    /// The configured per-command watchdog timeout.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

impl CommandRunner for RealRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ApiError> {
        let internal = |detail: String| ApiError::new(ApiErrorCode::Internal, detail);
        let mut child = Command::new(program)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| internal(format!("failed to execute {program}: {e}")))?;
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();

        // The child is shared with the watchdog behind a mutex; the
        // done-flag plus taking the child out of the slot before dropping
        // the lock make a kill-after-reap (pid reuse) impossible.
        let shared: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(Some(child)));
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let shared = Arc::clone(&shared);
            let done = Arc::clone(&done);
            let timed_out = Arc::clone(&timed_out);
            let timeout = self.timeout;
            thread::spawn(move || {
                thread::sleep(timeout);
                if !done.load(std::sync::atomic::Ordering::SeqCst) {
                    if let Ok(mut guard) = shared.lock() {
                        // Re-check under the lock: the caller may have
                        // finished between the load and the lock.
                        if !done.load(std::sync::atomic::Ordering::SeqCst) {
                            if let Some(child) = guard.as_mut() {
                                if child.kill().is_ok() {
                                    timed_out.store(true, std::sync::atomic::Ordering::SeqCst);
                                }
                            }
                        }
                    }
                }
            });
        }

        // Read both pipes to end on the calling thread. A hung child keeps
        // the pipes open, so this blocks until the child exits — or until
        // the watchdog kills it, which closes the pipes.
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let stdout_result = stdout_pipe
            .as_mut()
            .map_or(Ok(0), |pipe| pipe.read_to_end(&mut stdout));
        let stderr_result = stderr_pipe
            .as_mut()
            .map_or(Ok(0), |pipe| pipe.read_to_end(&mut stderr));

        // Reap the child while holding the lock, then publish done. The
        // watchdog can only kill what is still in the shared slot, so it
        // can never signal a reaped (possibly reused) pid.
        let status = {
            let mut guard = shared
                .lock()
                .map_err(|_| internal(format!("command runner lock poisoned for {program}")))?;
            let child = guard
                .as_mut()
                .ok_or_else(|| internal(format!("command {program} was already reaped")))?;
            let status = child
                .wait()
                .map_err(|e| internal(format!("failed to wait for {program}: {e}")))?;
            *guard = None;
            done.store(true, std::sync::atomic::Ordering::SeqCst);
            status
        };

        if timed_out.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(internal(format!(
                "command {program} timed out after {}s",
                self.timeout.as_secs_f64()
            )));
        }
        stdout_result.map_err(|e| internal(format!("failed to read stdout of {program}: {e}")))?;
        stderr_result.map_err(|e| internal(format!("failed to read stderr of {program}: {e}")))?;
        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            success: status.success(),
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

    #[test]
    fn real_runner_defaults_to_the_documented_timeout() {
        assert_eq!(RealRunner::default().timeout(), DEFAULT_TIMEOUT);
        assert_eq!(RealRunner::default().timeout(), Duration::from_secs(60));
        let custom = Duration::from_millis(250);
        assert_eq!(RealRunner::with_timeout(custom).timeout(), custom);
    }

    // The remaining RealRunner tests execute real subprocesses; they are
    // kept fast (sub-second apart from the deliberate 1 s timeout).
    #[test]
    fn real_runner_times_out_hung_commands() {
        let runner = RealRunner::with_timeout(Duration::from_secs(1));
        let started = std::time::Instant::now();
        // `sh -c` is forbidden in this codebase; `sleep` is invoked
        // directly as the program with a numeric argument.
        let err = runner.run("sleep", &["30"]).expect_err("hung command");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(err.detail.contains("timed out"), "{err}");
        assert!(err.detail.contains("sleep"), "{err}");
        assert!(err.detail.contains("1s"), "{err}");
        // The watchdog fired near the timeout, not near the sleep length.
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn real_runner_completes_fast_commands() {
        let runner = RealRunner::default();
        let out = runner.run("true", &[]).expect("fast command succeeds");
        assert!(out.success);
        assert!(out.stdout.is_empty());
        assert!(out.stderr.is_empty());
    }

    #[test]
    fn real_runner_captures_output_and_failure_status() {
        let runner = RealRunner::with_timeout(Duration::from_secs(10));
        // `false` exits non-zero without a shell.
        let out = runner.run("false", &[]).expect("runs");
        assert!(!out.success);
    }
}
