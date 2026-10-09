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
//! default, [`RealRunner::with_timeout`] otherwise). The **calling
//! thread** is the watchdog: it polls the child's status until it exits
//! or the timeout elapses. Each of the child's pipes is drained by a
//! dedicated **reader thread** that forwards the collected bytes over a
//! channel, so a child producing more output than the pipe buffer can
//! never deadlock against the caller. On timeout the child is killed
//! and given a grace period to exit; a child stuck in uninterruptible
//! sleep (D-state, plausible for `blkdiscard`/`lvremove` on a wedged
//! device) is handed to a detached reaper thread and the typed
//! `INTERNAL` timeout error is returned without blocking the caller
//! (see [`RealRunner`] for the exact design and its caveats).

use std::collections::VecDeque;
use std::io::{self, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

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

/// How often the calling-thread watchdog polls the child's status.
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How long the watchdog waits for a killed child to exit before
/// handing it over to the detached reaper.
const KILL_GRACE: Duration = Duration::from_secs(5);

/// How long the calling thread waits for the reader threads to deliver
/// their bytes after the child exited (normally immediate: EOF arrives
/// when the child's write ends close).
const PIPE_RECV_GRACE: Duration = Duration::from_secs(2);

/// How long the timeout path waits for the reader threads before
/// returning the typed timeout error (best-effort; the error is the
/// operative fact, not the output).
const TIMEOUT_RECV_GRACE: Duration = Duration::from_millis(100);

/// Production runner: executes commands via [`std::process::Command`].
///
/// No shell is involved; arguments are passed verbatim, so values derived
/// from identifiers (volume names, paths) can never be re-parsed as shell
/// syntax.
///
/// # Watchdog design
///
/// Every command is bounded by a watchdog timeout ([`DEFAULT_TIMEOUT`] by
/// default, [`RealRunner::with_timeout`] to customize), and the calling
/// thread *is* the watchdog — it never blocks on an unbounded read:
///
/// - Both child pipes (stdout, stderr) are drained by dedicated **reader
///   threads** that read to EOF and forward the bytes over channels.
///   Draining concurrently is what allows a child to write more than the
///   pipe buffer without deadlocking against the caller.
/// - The calling thread polls `try_wait` every
///   `WATCHDOG_POLL_INTERVAL`. A child that exits within the timeout
///   has its output collected from the channels (with a short
///   `PIPE_RECV_GRACE` recv timeout) and its exit status reported
///   honestly.
/// - When the timeout elapses, the child is `kill()`ed and given
///   `KILL_GRACE` to exit. A child that exits within the grace period
///   is reported as a typed `INTERNAL` timeout error naming the program
///   and the timeout.
/// - A child that *still* has not exited after the grace period is stuck
///   in uninterruptible sleep (D-state — plausible for `blkdiscard` or
///   `lvremove` on a wedged device, where even `kill` cannot take
///   effect). The [`Child`] is handed to a detached **reaper thread**
///   that keeps polling `try_wait` until the kernel reaps the process,
///   and the typed timeout error is returned immediately. This is a
///   bounded, documented thread leak (one reaper thread — and two
///   still-blocked reader threads — per D-state event); the alternative
///   would be blocking the calling thread forever.
///
/// # Out of scope
///
/// A grandchild that inherits the child's pipes and outlives it can keep
/// the reader threads waiting past `PIPE_RECV_GRACE`; the collected
/// output is then whatever arrived within the grace window. The LVM
/// toolchain does not daemonize, so this is accepted and documented
/// rather than handled.
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

    /// The typed timeout error for `program` (same shape as always).
    fn timeout_error(&self, program: &str) -> ApiError {
        ApiError::new(
            ApiErrorCode::Internal,
            format!(
                "command {program} timed out after {}s",
                self.timeout.as_secs_f64()
            ),
        )
    }

    /// Kill a timed-out child and report the typed timeout error.
    ///
    /// After the kill the watchdog keeps polling for `KILL_GRACE`: a
    /// child that exits within the grace period is a plain timeout. A
    /// child still running after the grace period is in D-state (the
    /// kill cannot take effect): it is handed to the detached reaper
    /// thread and the timeout error is returned without reading the
    /// pipes (a short best-effort recv only, so the reader threads are
    /// not waited on).
    fn kill_and_report_timeout(
        &self,
        mut child: Child,
        program: &str,
        stdout: &Receiver<io::Result<Vec<u8>>>,
        stderr: &Receiver<io::Result<Vec<u8>>>,
    ) -> ApiError {
        // The kill result is deliberately not surfaced: the timeout is
        // the operative fact, and a child that exits between the poll
        // and the kill is still a timeout.
        drop(child.kill());
        let kill_started = Instant::now();
        loop {
            match child.try_wait() {
                // Exited within the grace period (or the status is not
                // obtainable): a plain, honest timeout.
                Ok(Some(_)) | Err(_) => return self.timeout_error(program),
                Ok(None) => {}
            }
            if kill_started.elapsed() >= KILL_GRACE {
                break;
            }
            thread::sleep(WATCHDOG_POLL_INTERVAL);
        }

        // D-state: hand the child to the detached reaper so the kernel
        // eventually collects it, and return without waiting on the
        // (still open) pipes.
        thread::spawn(move || reap_detached(child));
        let _ = stdout.recv_timeout(TIMEOUT_RECV_GRACE);
        let _ = stderr.recv_timeout(TIMEOUT_RECV_GRACE);
        self.timeout_error(program)
    }
}

/// Reap a child that outlived the kill grace period (D-state).
///
/// The thread polls `try_wait` until the kernel reports an exit status
/// (or the status becomes unobtainable) and then finishes: one bounded
/// thread per D-state event, documented in [`RealRunner`]. The
/// alternative — waiting synchronously — would block the calling thread
/// forever.
fn reap_detached(mut child: Child) {
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => thread::sleep(WATCHDOG_POLL_INTERVAL),
        }
    }
}

/// Spawn a dedicated reader thread for one child pipe.
///
/// The thread reads the pipe to EOF and sends the collected bytes (or
/// the read error) over a channel. Both pipes are drained concurrently,
/// so a child writing more than the pipe buffer can never deadlock
/// against the calling thread.
fn spawn_pipe_reader(pipe: Option<impl Read + Send + 'static>) -> Receiver<io::Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let result = pipe.map_or(Ok(Vec::new()), |mut pipe| {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).map(|_| bytes)
        });
        // A closed receiver (the caller already returned) is fine.
        let _ = sender.send(result);
    });
    receiver
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

        let stdout_receiver = spawn_pipe_reader(child.stdout.take());
        let stderr_receiver = spawn_pipe_reader(child.stderr.take());

        // The calling thread is the watchdog: poll the child's status
        // until it exits or the timeout elapses.
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|e| internal(format!("failed to wait for {program}: {e}")))?
            {
                break status;
            }
            if started.elapsed() >= self.timeout {
                return Err(self.kill_and_report_timeout(
                    child,
                    program,
                    &stdout_receiver,
                    &stderr_receiver,
                ));
            }
            thread::sleep(WATCHDOG_POLL_INTERVAL);
        };

        // The child exited within the timeout: collect the drained
        // output. EOF normally arrives immediately (the write ends
        // closed on exit); the grace timeout only bounds a grandchild
        // that inherited the pipes (documented out of scope).
        let stdout = match stdout_receiver.recv_timeout(PIPE_RECV_GRACE) {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(e)) => return Err(internal(format!("failed to read stdout of {program}: {e}"))),
            Err(_) => Vec::new(),
        };
        let stderr = match stderr_receiver.recv_timeout(PIPE_RECV_GRACE) {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(e)) => return Err(internal(format!("failed to read stderr of {program}: {e}"))),
            Err(_) => Vec::new(),
        };
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
    fn real_runner_times_out_hung_commands_without_blocking_the_caller() {
        let runner = RealRunner::with_timeout(Duration::from_secs(1));
        let started = std::time::Instant::now();
        // The command runs on a dedicated thread and the test proves the
        // calling thread of `run` is never blocked indefinitely: the
        // completion signal is awaited with a bounded receive, not a
        // naked join. `sleep` is invoked directly as the program with a
        // numeric argument (never a shell).
        let (done_sender, done_receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let result = runner.run("sleep", &["30"]);
            let _ = done_sender.send(());
            result
        });
        assert!(
            done_receiver.recv_timeout(Duration::from_secs(10)).is_ok(),
            "run() must return after the timeout instead of blocking its caller forever"
        );
        let err = handle
            .join()
            .expect("runner thread must not panic")
            .expect_err("hung command");
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

    #[test]
    fn real_runner_drains_output_larger_than_the_pipe_buffer() {
        let runner = RealRunner::with_timeout(Duration::from_secs(10));
        // 256 KiB is far beyond the 64 KiB kernel pipe buffer: without
        // the concurrent reader threads the child would block on its
        // write (and the caller on the read) forever.
        let out = runner
            .run("head", &["-c", "262144", "/dev/zero"])
            .expect("large output command succeeds");
        assert!(out.success);
        assert_eq!(out.stdout.len(), 262_144);
        assert!(out.stdout.bytes().all(|byte| byte == 0));
    }
}
