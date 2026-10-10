/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStdout;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Context as _;
use anyhow::anyhow;
use tempfile::TempDir;
use tracing::info;
use tracing::warn;

use crate::cancellation::Cancellation;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
/// Controls how a nested Buck invocation reports diagnostics.
pub enum BuckDiagnostics {
    /// Preserve Buck's normal console output.
    #[default]
    Inherit,
    /// Suppress routine console output and report a Buck UI link.
    QuietWithUi,
}

struct BuildIdOutput {
    path: PathBuf,
    _directory: TempDir,
}

/// A running Buck child process and its diagnostic output state.
pub struct BuckProcess {
    child: RunningChild,
    build_id: Option<BuildIdOutput>,
}

/// A Buck `targets` command whose diagnostics are placed before caller arguments.
pub struct BuckCommand {
    command: Command,
    stdout: Stdio,
    build_id: Option<BuildIdOutput>,
    cancellation: Option<Cancellation>,
}

enum RunningChild {
    Direct(OwnedChild),
    Cancellable {
        child: Arc<Mutex<OwnedChild>>,
        cancellation: Cancellation,
    },
}

impl RunningChild {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match self {
            Self::Direct(child) => child.try_wait(),
            Self::Cancellable { child, .. } => {
                child.lock().expect("Buck child mutex poisoned").try_wait()
            }
        }
    }

    fn stop(&mut self) -> io::Result<()> {
        match self {
            Self::Direct(child) => child.stop(),
            Self::Cancellable { child, .. } => {
                child.lock().expect("Buck child mutex poisoned").stop()
            }
        }
    }
}

/// Own a child and, on Linux, the lifetime of its invocation group.
#[derive(Debug)]
pub(crate) struct OwnedChild {
    child: Child,
    #[cfg(target_os = "linux")]
    lifetime: InvocationLifetime,
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
enum InvocationLifetime {
    Running(std::os::unix::net::UnixStream),
    Released,
    Failed(String),
}

// Linux's atomic SOCK_CLOEXEC is required:
// a leaked endpoint could keep abandoned work alive on another platform.
// The lifetime socket occupies the supervisor's stdin. Buck consumes arguments
// and files, never this socket. No pre_exec hook is needed, so spawning from a
// large decoded graph can still use posix_spawn. setsid detaches the controlling
// terminal before sh starts: even interpreter startup diagnostics must not stop
// this noninteractive invocation under terminal TOSTOP.
// Start sh with -p so imported functions, startup files and options cannot change
// the supervisor. TMOUT is a read timeout, not part of the lifetime protocol.
#[cfg(target_os = "linux")]
const SUPERVISOR: &str = r#"
set -e
set +m
exec 3<&0
(
    unset TMOUT
    if IFS= read -r decision <&3; then
        case "$decision" in complete) exit 0 ;; esac
    fi
    kill -KILL 0
) </dev/null >/dev/null 2>/dev/null &
case $! in '') exit 125 ;; esac
result=0
(exec "$@") </dev/null 3<&- || result=$?
printf D >&3
exit "$result"
"#;

impl OwnedChild {
    // The only production caller is BuckCommand, which exposes this query context
    // rather than arbitrary process configuration.
    fn spawn(command: &Command, stdout: Stdio) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        let (mut child_command, lifetime) = {
            use std::os::fd::OwnedFd;
            use std::os::unix::net::UnixStream;

            let (lifetime, channel) = UnixStream::pair()?;
            lifetime.set_nonblocking(true)?;
            let mut child = Command::new("/usr/bin/setsid");
            // A fresh child is not a process-group leader, so setsid creates
            // the private session and group, then execs sh without forking.
            child
                .args([
                    "--wait",
                    "/bin/sh",
                    "-p",
                    "-c",
                    SUPERVISOR,
                    "buck-targets-supervisor",
                ])
                .stdin(Stdio::from(OwnedFd::from(channel)));
            // Bash rewrites these exported, readonly variables even under -p.
            // Restore the caller's options only for Buck, not for supervision.
            let mut options = Vec::new();
            for name in ["SHELLOPTS", "BASHOPTS"] {
                let value = command
                    .get_envs()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.map(std::ffi::OsString::from))
                    .unwrap_or_else(|| std::env::var_os(name));
                if let Some(value) = value {
                    let mut assignment = std::ffi::OsString::from(name);
                    assignment.push("=");
                    assignment.push(value);
                    options.push(assignment);
                }
            }
            if !options.is_empty() {
                // env treats '=' in a command path as another assignment.
                // nice -n0 preserves priority and supplies a command boundary.
                child.args(["/usr/bin/env", "--"]).args(options).args([
                    "/usr/bin/nice",
                    "-n",
                    "0",
                    "--",
                ]);
            }
            child.arg(command.get_program());
            (child, lifetime)
        };
        #[cfg(not(target_os = "linux"))]
        let mut child_command = {
            let mut child = Command::new(command.get_program());
            child.stdin(Stdio::null());
            child
        };

        child_command.args(command.get_args()).stdout(stdout);
        if let Some(directory) = command.get_current_dir() {
            child_command.current_dir(directory);
        }
        for (name, value) in command.get_envs() {
            match value {
                Some(value) => {
                    child_command.env(name, value);
                }
                None => {
                    child_command.env_remove(name);
                }
            }
        }
        let child = child_command.spawn()?;
        drop(child_command);
        Ok(Self {
            child,
            #[cfg(target_os = "linux")]
            lifetime: InvocationLifetime::Running(lifetime),
        })
    }

    pub(crate) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    pub(crate) fn stop(&mut self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            // EOF tells the watcher to kill its own group. Its membership pins
            // the group's identity even after the supervisor has been reaped.
            if matches!(&self.lifetime, InvocationLifetime::Running(_)) {
                self.lifetime = InvocationLifetime::Released;
            }
        }
        #[cfg(not(target_os = "linux"))]
        if self.child.try_wait()?.is_none() {
            match self.child.kill() {
                Ok(()) => {}
                Err(_) if self.child.try_wait()?.is_some() => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn completed(&mut self, status: ExitStatus) -> io::Result<()> {
        use std::io::Read;
        use std::io::Write;

        let result = match std::mem::replace(&mut self.lifetime, InvocationLifetime::Released) {
            InvocationLifetime::Released => return Ok(()),
            InvocationLifetime::Failed(message) => Err(message),
            InvocationLifetime::Running(mut lifetime) => {
                // Only the supervisor can send D, after the wrapped command exits.
                // A killed supervisor must not release unfinished descendants.
                let confirm = (|| -> io::Result<()> {
                    let mut marker = [0];
                    lifetime.read_exact(&mut marker)?;
                    if marker != *b"D" {
                        return Err(io::Error::other("invalid completion marker"));
                    }
                    lifetime.write_all(b"complete\n")
                })();
                confirm.map_err(|error| format!(
                    "Buck supervisor exited with {status} without confirming completion: {error}"
                ))
            }
        };
        result.map_err(|message| {
            self.lifetime = InvocationLifetime::Failed(message.clone());
            io::Error::other(message)
        })
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        #[cfg(target_os = "linux")]
        if let Some(status) = status {
            self.completed(status)?;
        }
        Ok(status)
    }

    pub(crate) fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait()?;
        #[cfg(target_os = "linux")]
        self.completed(status)?;
        Ok(status)
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if matches!(&self.lifetime, InvocationLifetime::Failed(_)) {
            // Completion validation already reaped the child and closed its channel.
            return;
        }
        if let Err(error) = self.stop().and_then(|()| self.wait().map(|_| ())) {
            warn!("Buck child cleanup failed: {error:#}");
        }
    }
}

impl BuckCommand {
    /// Construct a Buck `targets` command with diagnostics before caller arguments.
    pub fn targets(
        program: impl AsRef<OsStr>,
        isolation_dir: Option<&str>,
        diagnostics: BuckDiagnostics,
    ) -> anyhow::Result<Self> {
        let mut command = Command::new(program);
        if let Some(isolation_dir) = isolation_dir {
            command.args(["--isolation-dir", isolation_dir]);
        }
        command.arg("targets");
        let build_id = match diagnostics {
            BuckDiagnostics::Inherit => None,
            BuckDiagnostics::QuietWithUi => {
                let directory = tempfile::tempdir().context("creating Buck build-ID directory")?;
                let path = directory.path().join("build_id");
                // The "none" console also discards streaming stdout from targets.
                command
                    .args(["--console=simple", "--verbose=0"])
                    .arg(format!("--write-build-id={}", path.display()));
                Some(BuildIdOutput {
                    path,
                    _directory: directory,
                })
            }
        };

        Ok(Self {
            command,
            stdout: Stdio::inherit(),
            build_id,
            cancellation: None,
        })
    }

    /// Bind this targets invocation to its preparation's cancellation lifecycle.
    pub fn with_cancellation(mut self, cancellation: Option<Cancellation>) -> Self {
        self.cancellation = cancellation;
        self
    }

    /// Append a `targets` option or pattern.
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.command.arg(arg);
        self
    }

    /// Append `targets` options and patterns. Input must use arguments or files.
    pub fn args(&mut self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> &mut Self {
        self.command.args(args);
        self
    }

    /// Configure graph output without changing the noninteractive input contract.
    pub fn stdout(&mut self, stdout: Stdio) -> &mut Self {
        self.stdout = stdout;
        self
    }

    /// Set the repository directory for this query.
    pub fn current_dir(&mut self, directory: impl AsRef<Path>) -> &mut Self {
        self.command.current_dir(directory);
        self
    }

    /// Set an environment variable for the Buck invocation.
    pub fn env(&mut self, name: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.command.env(name, value);
        self
    }

    /// Access the final command for logging.
    pub fn as_command(&self) -> &Command {
        &self.command
    }

    /// Spawn the configured command.
    pub fn spawn(self) -> anyhow::Result<BuckProcess> {
        let child = match self.cancellation {
            Some(cancellation) => RunningChild::Cancellable {
                child: cancellation.spawn(|| OwnedChild::spawn(&self.command, self.stdout))?,
                cancellation,
            },
            None => RunningChild::Direct(
                OwnedChild::spawn(&self.command, self.stdout).context("spawning Buck")?,
            ),
        };
        Ok(BuckProcess {
            child,
            build_id: self.build_id,
        })
    }
}

impl BuckProcess {
    fn take_stdout(&mut self) -> anyhow::Result<ChildStdout> {
        match &mut self.child {
            RunningChild::Direct(child) => child.take_stdout(),
            RunningChild::Cancellable { child, .. } => child
                .lock()
                .expect("Buck child mutex poisoned")
                .take_stdout(),
        }
        .context("capturing Buck stdout")
    }

    /// Read graph output on a scoped worker while observing invocation failures.
    pub fn read_stdout<T: Send>(
        mut self,
        read: impl FnOnce(&mut ChildStdout) -> anyhow::Result<T> + Send,
    ) -> anyhow::Result<T> {
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        let mut stdout = self.take_stdout()?;
        let output = thread::scope(|scope| {
            let (sender, receiver) = mpsc::sync_channel(1);
            let reader = thread::Builder::new()
                .spawn_scoped(scope, move || {
                    let output = read(&mut stdout);
                    drop(stdout);
                    let _ = sender.send(output);
                })
                .context("starting Buck stdout reader")?;
            let result = loop {
                match receiver.recv_timeout(Duration::from_millis(50)) {
                    Ok(output) => break output,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        break Err(anyhow!("Buck stdout reader terminated without a result"));
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        // A dead supervisor cannot close stdout held by a
                        // descendant. Observe its failure before waiting for EOF.
                        if let Err(error) = self.child.try_wait() {
                            break Err(error).context("observing Buck while reading stdout");
                        }
                    }
                }
            };
            if result.is_err() {
                self.child.stop().context("stopping failed Buck output")?;
            }
            if let Err(payload) = reader.join() {
                std::panic::resume_unwind(payload);
            }
            result
        });
        self.wait_with_output(output)
    }

    /// Wait for Buck and report its build UI on failure when available.
    pub fn wait(self) -> anyhow::Result<()> {
        self.wait_with_output(Ok(()))
    }

    /// Wait for Buck while preserving a caller's stdout-processing result.
    fn wait_with_output<T>(mut self, output: anyhow::Result<T>) -> anyhow::Result<T> {
        // Thread creation can fail before read_stdout starts its reader/exit loop.
        if output.is_err() {
            self.child.stop().context("stopping failed Buck output")?;
        }
        let status = match &mut self.child {
            RunningChild::Direct(child) => child.wait().map_err(anyhow::Error::from),
            RunningChild::Cancellable {
                child,
                cancellation,
            } => cancellation.wait(child),
        };
        let diagnostic = match read_build_id(self.build_id.as_ref()) {
            Ok(Some(build_id)) => {
                let url = format!("https://www.internalfb.com/buck2/{build_id}");
                info!("Buck UI: {url}");
                format!("; Buck UI: {url}")
            }
            Ok(None) => String::new(),
            Err(error) => {
                warn!("Failed to read Buck build ID: {error:#}");
                format!("; failed to read Buck build ID: {error:#}")
            }
        };

        let status = status.with_context(|| format!("waiting for Buck{diagnostic}"))?;
        match (status.success(), output) {
            (true, Ok(output)) => Ok(output),
            (false, Ok(_)) => Err(anyhow!("Buck command failed with {status}{diagnostic}")),
            (_, Err(error)) => Err(error.context(format!(
                "Buck stdout processing failed (command {status}){diagnostic}"
            ))),
        }
    }
}

fn read_build_id(output: Option<&BuildIdOutput>) -> anyhow::Result<Option<String>> {
    let Some(output) = output else {
        return Ok(None);
    };
    let value = match fs::read_to_string(&output.path) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading Buck build ID from {}", output.path.display()));
        }
    };
    Ok((!value.trim().is_empty()).then(|| value.trim().to_owned()))
}

#[cfg(all(test, unix))]
pub(crate) mod test_support {
    use std::fs::File;
    use std::fs::OpenOptions;
    use std::io;
    use std::io::BufRead;
    use std::io::BufReader;
    use std::io::Write;
    use std::process::ChildStdout;
    use std::process::Command;
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    pub fn spawn_child(command: &Command, stdout: Stdio) -> io::Result<super::OwnedChild> {
        super::OwnedChild::spawn(command, stdout)
    }

    pub struct Gate {
        pub directory: tempfile::TempDir,
        pub input: File,
    }

    impl Gate {
        pub fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("gate");
            assert!(
                Command::new("/bin/sh")
                    .arg("-c")
                    .arg("mkfifo \"$1\"")
                    .arg("fixture")
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            let input = OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .unwrap();
            Self { directory, input }
        }

        pub fn command(&self, script: &str) -> Command {
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", script])
                .current_dir(self.directory.path());
            command
        }
    }

    impl Drop for Gate {
        fn drop(&mut self) {
            // Unblock a failed fixture without closing the gate before assertions.
            let _ = self.input.write_all(b"finish\nfinish\nfinish\nfinish\n");
        }
    }

    pub fn lines(stdout: ChildStdout) -> mpsc::Receiver<String> {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if sender.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        receiver
    }

    pub fn line(receiver: &mpsc::Receiver<String>) -> String {
        receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("fixture must make progress")
    }

    pub fn eof(receiver: &mpsc::Receiver<String>) {
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(10)),
            Err(mpsc::RecvTimeoutError::Disconnected),
            "descendants must close stdout"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rstest::rstest]
    #[case(false)]
    #[case(true)]
    #[cfg(unix)]
    fn normal_completion_releases_background_postlude(#[case] cancellable: bool) {
        use std::io::Write;

        use super::test_support::*;

        let mut gate = Gate::new();
        let command = gate.command(
            r#"exec 4<gate; (printf 'ready\n'; read value <&4; printf '%s\n' "$value") & exit 7"#,
        );
        let cancellation = Cancellation::default();
        let mut child = if cancellable {
            RunningChild::Cancellable {
                child: cancellation
                    .spawn(|| OwnedChild::spawn(&command, Stdio::piped()))
                    .unwrap(),
                cancellation: cancellation.clone(),
            }
        } else {
            RunningChild::Direct(OwnedChild::spawn(&command, Stdio::piped()).unwrap())
        };
        let (stdout, status) = match &mut child {
            RunningChild::Direct(child) => (child.take_stdout().unwrap(), child.wait().unwrap()),
            RunningChild::Cancellable {
                child,
                cancellation,
            } => {
                let stdout = child.lock().unwrap().take_stdout().unwrap();
                (stdout, cancellation.wait(child).unwrap())
            }
        };
        assert_eq!(status.code(), Some(7));
        drop(child);
        cancellation.cancel().unwrap();
        let output = lines(stdout);
        assert_eq!(line(&output), "ready");
        gate.input.write_all(b"postlude\n").unwrap();
        assert_eq!(line(&output), "postlude");
        eof(&output);
    }

    #[rstest::rstest]
    #[case(false)]
    #[case(true)]
    #[cfg(target_os = "linux")]
    fn early_drop_stops_descendants(#[case] cancellable: bool) {
        use super::test_support::*;

        let gate = Gate::new();
        // Interpret a file instead of executing a newly written executable:
        // concurrent forks can briefly inherit the writer and cause ETXTBSY.
        fs::write(
            gate.directory.path().join("targets"),
            "trap '' TERM; exec 4<gate; (printf 'ready\\n'; read value <&4) & wait",
        )
        .unwrap();
        let mut command = BuckCommand::targets("/bin/sh", None, BuckDiagnostics::Inherit).unwrap();
        command.current_dir(gate.directory.path());
        command.stdout(Stdio::piped());
        if cancellable {
            command = command.with_cancellation(Some(Cancellation::default()));
        }
        let mut process = command.spawn().unwrap();
        let output = lines(process.take_stdout().unwrap());
        assert_eq!(line(&output), "ready");
        drop(process);
        eof(&output);
    }

    #[rstest::rstest]
    #[case(false, false)]
    #[case(true, false)]
    #[case(false, true)]
    #[case(true, true)]
    #[cfg(target_os = "linux")]
    fn streaming_failure_stops_readers_and_work(
        #[case] cancellable: bool,
        #[case] supervisor_dies: bool,
    ) {
        use std::io::BufRead;
        use std::io::BufReader;
        use std::io::Read;
        use std::io::Write;
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        use super::test_support::*;

        let mut gate = Gate::new();
        let mut command = gate.command("exec 4<gate; printf 'ready\\n'; read value <&4");
        command.env("BASH_FUNC_kill%%", "() { return 0; }");
        let cancellation = Cancellation::default();
        let (child, pid) = if cancellable {
            let child = cancellation
                .spawn(|| OwnedChild::spawn(&command, Stdio::piped()))
                .unwrap();
            let pid = child.lock().unwrap().child.id();
            (
                RunningChild::Cancellable {
                    child,
                    cancellation,
                },
                pid,
            )
        } else {
            let child = OwnedChild::spawn(&command, Stdio::piped()).unwrap();
            let pid = child.child.id();
            (RunningChild::Direct(child), pid)
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("build_id");
        fs::write(&path, "test-buck-uuid\n").unwrap();
        let process = BuckProcess {
            child,
            build_id: Some(BuildIdOutput {
                path,
                _directory: directory,
            }),
        };
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            done_tx
                .send(process.read_stdout(|stdout| {
                    let mut reader = BufReader::new(stdout);
                    let mut ready = String::new();
                    reader.read_line(&mut ready)?;
                    assert_eq!(ready, "ready\n");
                    ready_tx.send(()).unwrap();
                    if !supervisor_dies {
                        anyhow::bail!("invalid graph");
                    }
                    // Match graph construction: consume stdout before returning.
                    reader.read_to_end(&mut Vec::new())?;
                    Ok(())
                }))
                .unwrap();
        });
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        if supervisor_dies {
            assert!(
                Command::new("/bin/kill")
                    .arg("-KILL")
                    .arg(pid.to_string())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let result = done_rx.recv_timeout(Duration::from_secs(10));
        // Release a failed fixture before joining, not before the assertion's deadline.
        gate.input.write_all(b"finish\n").unwrap();
        worker.join().unwrap();
        let error = result
            .expect("streaming failure must stop the invocation")
            .unwrap_err();
        assert!(format!("{error:#}").contains("https://www.internalfb.com/buck2/test-buck-uuid"));
        if !supervisor_dies {
            assert!(format!("{error:#}").contains("invalid graph"));
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn stdout_panic_preserves_payload_and_stops_work() {
        use std::io::BufRead;
        use std::io::BufReader;
        use std::io::Write;
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        use super::test_support::*;

        let mut gate = Gate::new();
        let child = OwnedChild::spawn(
            &gate.command("exec 4<gate; printf 'ready\\n'; read value <&4"),
            Stdio::piped(),
        )
        .unwrap();
        let process = BuckProcess {
            child: RunningChild::Direct(child),
            build_id: None,
        };
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                process.read_stdout::<()>(|stdout| {
                    let mut ready = String::new();
                    BufReader::new(stdout).read_line(&mut ready)?;
                    assert_eq!(ready, "ready\n");
                    ready_tx.send(()).unwrap();
                    std::panic::panic_any("reader invariant");
                })
            }));
            assert!(done_tx.send(result).is_ok());
        });
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let result = done_rx.recv_timeout(Duration::from_secs(10));
        gate.input.write_all(b"finish\n").unwrap();
        worker.join().unwrap();
        let payload = result
            .expect("panic cleanup must finish")
            .expect_err("reader panic must propagate");
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"reader invariant"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn supervisor_failure_remains_an_error_after_reaping() {
        use super::test_support::*;

        let gate = Gate::new();
        let mut child = OwnedChild::spawn(
            &gate.command("exec 4<gate; printf 'ready\\n'; read value <&4"),
            Stdio::piped(),
        )
        .unwrap();
        let output = lines(child.take_stdout().unwrap());
        assert_eq!(line(&output), "ready");
        child.child.kill().unwrap();
        let first = child.wait().unwrap_err().to_string();
        assert!(first.contains("Buck supervisor exited with signal"));
        child.stop().unwrap();
        assert_eq!(child.try_wait().unwrap_err().to_string(), first);
        eof(&output);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn detached_descendant_survives_cancellation() {
        use std::io::Write;

        use super::test_support::*;

        let mut gate = Gate::new();
        let mut child = OwnedChild::spawn(&gate.command(
            r#"/usr/bin/setsid /bin/sh -c 'exec 4<gate; printf "detached\n"; read value <&4; printf "%s\n" "$value"' & wait"#),
            Stdio::piped()).unwrap();
        let output = lines(child.take_stdout().unwrap());
        assert_eq!(line(&output), "detached");
        drop(child);
        gate.input.write_all(b"alive\n").unwrap();
        assert_eq!(line(&output), "alive");
        eof(&output);
    }

    #[test]
    #[cfg(unix)]
    fn query_input_is_noninteractive_and_arguments_are_data() {
        use super::test_support::*;

        let gate = Gate::new();
        let mut command =
            gate.command(r#"test ! -t 0 && ! read value && printf '%s\n' "$1" && pwd"#);
        let argument = "$(exit 99); spaces 'quotes'";
        command.args(["fixture", argument]);
        let mut child = OwnedChild::spawn(&command, Stdio::piped()).unwrap();
        let output = lines(child.take_stdout().unwrap());
        assert!(child.wait().unwrap().success());
        assert_eq!(line(&output), argument);
        assert_eq!(
            line(&output),
            gate.directory
                .path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
        );
        eof(&output);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn invocation_has_a_private_session() {
        use std::io::Write;

        use super::test_support::*;

        let mut gate = Gate::new();
        let mut child = OwnedChild::spawn(
            &gate.command("exec 4<gate; printf '%s\\n' \"$$\"; read value <&4"),
            Stdio::piped(),
        )
        .unwrap();
        let output = lines(child.take_stdout().unwrap());
        let pid: u32 = line(&output).parse().unwrap();
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let fields: Vec<_> = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        let group: u32 = fields[2].parse().unwrap();
        let session: u32 = fields[3].parse().unwrap();
        gate.input.write_all(b"finish\n").unwrap();
        assert!(child.wait().unwrap().success());
        eof(&output);
        assert_eq!(group, child.child.id());
        assert_eq!(session, child.child.id());
    }

    #[test]
    #[cfg(unix)]
    fn query_environment_is_preserved() {
        use super::test_support::*;

        let gate = Gate::new();
        let mut command = gate.command("printf '%s\\n' \"$TD_TEST_VALUE\"");
        command.env("TD_TEST_VALUE", "with spaces");
        let mut child = OwnedChild::spawn(&command, Stdio::piped()).unwrap();
        let output = lines(child.take_stdout().unwrap());
        assert!(child.wait().unwrap().success());
        assert_eq!(line(&output), "with spaces");
        eof(&output);
    }

    #[rstest::rstest]
    #[case("buck2")]
    #[case("true")]
    #[cfg(target_os = "linux")]
    fn query_runs_the_executable_not_a_shell_function_or_builtin(#[case] program: &str) {
        use super::test_support::*;

        let gate = Gate::new();
        std::os::unix::fs::symlink("/bin/echo", gate.directory.path().join(program)).unwrap();
        let mut command = Command::new(program);
        command
            .arg("from executable")
            .env("PATH", gate.directory.path())
            .env(
                format!("BASH_FUNC_{program}%%"),
                "() { printf 'function-shadowed\\n'; }",
            );
        for builtin in [
            "read", "kill", "printf", "command", "exec", "set", "trap", "unset", "exit",
        ] {
            command.env(format!("BASH_FUNC_{builtin}%%"), "() { return 0; }");
        }
        let mut child = OwnedChild::spawn(&command, Stdio::piped()).unwrap();
        let output = lines(child.take_stdout().unwrap());
        assert!(child.wait().unwrap().success());
        assert_eq!(line(&output), "from executable");
        eof(&output);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn supervisor_settings_do_not_change_query_environment() {
        use super::test_support::*;

        let mut command = Command::new("/usr/bin/printenv");
        let settings = [
            ("SHELLOPTS", "noexec:xtrace"),
            ("BASHOPTS", "extdebug"),
            ("TMOUT", "1"),
            ("BASH_ENV", "/no/supervisor/startup"),
            ("ENV", "/no/supervisor/startup"),
            ("CDPATH", "/query/search/path"),
            ("GLOBIGNORE", "query-pattern"),
        ];
        for (name, value) in settings {
            command.arg(name).env(name, value);
        }
        let mut child = OwnedChild::spawn(&command, Stdio::piped()).unwrap();
        let output = lines(child.take_stdout().unwrap());
        assert!(child.wait().unwrap().success());
        for (_, value) in settings {
            assert_eq!(line(&output), value);
        }
        eof(&output);
    }

    #[rstest::rstest]
    #[case("buck=2")]
    #[case("version=2/buck2")]
    #[cfg(target_os = "linux")]
    fn exported_shell_options_allow_equals_in_executable_paths(#[case] program: &str) {
        use super::test_support::*;

        let gate = Gate::new();
        let executable = gate.directory.path().join(program);
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("/bin/echo", &executable).unwrap();
        let mut command = Command::new(executable);
        command.arg("from executable").env("SHELLOPTS", "errexit");
        let mut child = OwnedChild::spawn(&command, Stdio::piped()).unwrap();
        let output = lines(child.take_stdout().unwrap());
        assert!(child.wait().unwrap().success());
        assert_eq!(line(&output), "from executable");
        eof(&output);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn terminal_output_signal_does_not_suspend_invocation() {
        use std::io::Write;
        use std::sync::mpsc;
        use std::time::Duration;

        use super::test_support::*;

        let mut gate = Gate::new();
        let mut child = OwnedChild::spawn(
            &gate.command("exec 4<gate; kill -TTOU 0; printf 'alive\\n'; read value <&4"),
            Stdio::piped(),
        )
        .unwrap();
        let output = lines(child.take_stdout().unwrap());
        let ready = output.recv_timeout(Duration::from_secs(10));
        child.stop().unwrap();
        let result = output.recv_timeout(Duration::from_secs(10));
        if result != Err(mpsc::RecvTimeoutError::Disconnected) {
            // Resume only a broken fixture, after its cancellation deadline.
            // The unreaped leader still pins this invocation's group identity.
            assert!(
                Command::new("/bin/kill")
                    .arg("-CONT")
                    .arg(format!("-{}", child.child.id()))
                    .status()
                    .unwrap()
                    .success()
            );
            gate.input.write_all(b"finish\n").unwrap();
        }
        child.wait().unwrap();
        assert_eq!(ready.unwrap(), "alive");
        assert_eq!(result, Err(mpsc::RecvTimeoutError::Disconnected));
    }

    // This isolated subprocess owns the invocation. Killing it must close the
    // lifetime capability even though Rust destructors cannot run.
    #[test]
    #[cfg(target_os = "linux")]
    fn owner_fixture() {
        let Some(directory) = std::env::var_os("TD_TEST_OWNER_DIRECTORY") else {
            return;
        };
        let mut command = Command::new("/bin/sh");
        command.current_dir(directory).args([
            "-c",
            "trap '' INT TERM; exec 4<gate; (printf 'worker-ready\\n'; read value <&4) & wait",
        ]);
        let mut child = OwnedChild::spawn(&command, Stdio::inherit()).unwrap();
        child.wait().unwrap();
        panic!("owner should have been terminated");
    }

    #[rstest::rstest]
    #[case("INT", 2)]
    #[case("TERM", 15)]
    #[case("KILL", 9)]
    #[cfg(target_os = "linux")]
    fn owner_termination_stops_invocation(#[case] signal: &str, #[case] signal_number: i32) {
        use std::os::unix::process::ExitStatusExt;
        use std::time::Duration;
        use std::time::Instant;

        use super::test_support::*;

        struct Owner(Child);
        impl Drop for Owner {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let gate = Gate::new();
        let mut owner = Owner(
            Command::new("/usr/bin/env")
                .arg("--default-signal=INT,TERM")
                .arg(std::env::current_exe().unwrap())
                .args(["--exact", "process::tests::owner_fixture", "--nocapture"])
                .env("TD_TEST_OWNER_DIRECTORY", gate.directory.path())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let output = lines(owner.0.stdout.take().unwrap());
        while !line(&output).contains("worker-ready") {}
        assert!(
            Command::new("/bin/kill")
                .args(["-s", signal, "--"])
                .arg(owner.0.id().to_string())
                .status()
                .unwrap()
                .success()
        );
        // Check the injected signal separately from the invocation's cleanup.
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = owner.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "fixture owner did not exit after {signal}"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.signal(), Some(signal_number));
        eof(&output);
    }

    #[test]
    fn quiet_command_places_native_flags_before_caller_arguments() {
        let mut command =
            BuckCommand::targets("buck2", Some("isolation"), BuckDiagnostics::QuietWithUi)
                .expect("should construct Buck command");
        command.args(["--", "//example:target"]);

        let arguments = command
            .as_command()
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments[0..5],
            [
                "--isolation-dir",
                "isolation",
                "targets",
                "--console=simple",
                "--verbose=0",
            ]
        );
        assert!(arguments[5].starts_with("--write-build-id="));
        assert_eq!(arguments[6..], ["--", "//example:target"]);
    }

    #[test]
    fn reads_trimmed_build_id() {
        let command = BuckCommand::targets("buck2", None, BuckDiagnostics::QuietWithUi)
            .expect("should construct Buck command");
        let output = command
            .build_id
            .as_ref()
            .expect("quiet diagnostics should allocate build-ID output");
        fs::write(&output.path, " test-buck-uuid\n").expect("should write build ID");

        assert_eq!(
            read_build_id(Some(output)).expect("should read build ID"),
            Some("test-buck-uuid".to_owned())
        );
    }
}
