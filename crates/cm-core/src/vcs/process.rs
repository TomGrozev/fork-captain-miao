//! Bounded Git children. The process group belongs to this invocation only.

use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{ChildStdout, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub(super) enum RunFail {
    NoTool,
    TimedOut,
    Denied,
    Message(String),
}

pub(super) struct GitOut<T = Vec<u8>> {
    pub status_ok: bool,
    pub stdout: T,
    pub stderr: Vec<u8>,
}

pub(super) fn run_git(cwd: &Path, args: &[&str], deadline: Instant) -> Result<GitOut, RunFail> {
    run_git_read(cwd, args, deadline, |pipe| super::read_capped(Some(pipe)))
}

pub(super) fn run_git_read<T: Send + 'static>(
    cwd: &Path,
    args: &[&str],
    deadline: Instant,
    read_stdout: impl FnOnce(DeadlineReader<ChildStdout>) -> T + Send + 'static,
) -> Result<GitOut<T>, RunFail> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0");
    run(command, deadline, read_stdout)
}

fn run<T: Send + 'static>(
    mut command: Command,
    deadline: Instant,
    read_stdout: impl FnOnce(DeadlineReader<ChildStdout>) -> T + Send + 'static,
) -> Result<GitOut<T>, RunFail> {
    if Instant::now() >= deadline {
        return Err(RunFail::TimedOut);
    }
    let mut child = command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| match err.kind() {
            io::ErrorKind::NotFound => RunFail::NoTool,
            io::ErrorKind::PermissionDenied => RunFail::Denied,
            _ => RunFail::Message(err.to_string()),
        })?;
    let pipes = DeadlineReader::new(child.stdout.take().unwrap(), deadline).and_then(|stdout| {
        DeadlineReader::new(child.stderr.take().unwrap(), deadline).map(|stderr| (stdout, stderr))
    });
    let (stdout, stderr) = match pipes {
        Ok(pipes) => pipes,
        Err(err) => {
            kill_group(child.id());
            let _ = child.wait();
            return Err(RunFail::Message(err.to_string()));
        }
    };
    let out_thread = thread::spawn(move || read_stdout(stdout));
    let err_thread = thread::spawn(move || super::read_capped(Some(stderr)));
    let result = loop {
        if Instant::now() >= deadline {
            break Err(RunFail::TimedOut);
        }
        // Do not reap the leader while inherited pipes are still open. Its
        // reserved PID prevents a timeout signal from hitting a reused group.
        if out_thread.is_finished() && err_thread.is_finished() {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {}
                Err(err) => break Err(RunFail::Message(err.to_string())),
            }
        }
        thread::sleep(Duration::from_millis(5));
    };
    if result.is_err() {
        kill_group(child.id());
        let _ = child.wait();
    }
    // Readers have the same deadline, including helpers that escape the group
    // but retain a pipe. Neither join depends on those helpers exiting.
    let stdout = out_thread.join();
    let stderr = err_thread.join().unwrap_or_default();
    let status = result?;
    let stdout = stdout.map_err(|_| RunFail::Message("could not read git output".into()))?;
    Ok(GitOut {
        status_ok: status.success(),
        stdout,
        stderr,
    })
}

fn kill_group(pid: u32) {
    // SAFETY: the child was started in its own group and has not been reaped.
    // Negative PID targets that group, never the dashboard or an SSH master.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

pub(super) struct DeadlineReader<R> {
    inner: R,
    deadline: Instant,
}

impl<R: Read + AsRawFd> DeadlineReader<R> {
    fn new(inner: R, deadline: Instant) -> io::Result<Self> {
        let fd = inner.as_raw_fd();
        // SAFETY: fd is owned by inner and remains open for both calls.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { inner, deadline })
    }
}

impl<R: Read> Read for DeadlineReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if Instant::now() >= self.deadline {
                return Err(io::ErrorKind::TimedOut.into());
            }
            match self.inner.read(buf) {
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                result => return result,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_covers_descendants_and_inherited_pipes() {
        for script in ["sleep 5 & wait", "sleep 5 & exit 0"] {
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            let start = Instant::now();
            let result = run(command, start + Duration::from_millis(100), |r| {
                super::super::read_capped(Some(r))
            });
            assert!(matches!(result, Err(RunFail::TimedOut)));
            assert!(start.elapsed() < Duration::from_secs(2));
        }
        let output = run_git(
            Path::new("/tmp"),
            &["--version"],
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        assert!(output.status_ok);
    }

    #[test]
    fn expired_work_never_starts() {
        let result = run(
            Command::new("nonexistent-review-command"),
            Instant::now(),
            |r| super::super::read_capped(Some(r)),
        );
        assert!(matches!(result, Err(RunFail::TimedOut)));
    }

    #[test]
    fn timeout_leaves_unrelated_processes_running() {
        let mut unrelated = Command::new("sleep").arg("5").spawn().unwrap();
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 5 & wait"]);
        let result = run(command, Instant::now() + Duration::from_millis(100), |r| {
            super::super::read_capped(Some(r))
        });
        let unaffected = unrelated.try_wait().unwrap().is_none();
        let _ = unrelated.kill();
        let _ = unrelated.wait();
        assert!(matches!(result, Err(RunFail::TimedOut)));
        assert!(unaffected);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn escaped_helper_cannot_hold_a_reader_forever() {
        let mut command = Command::new("sh");
        // The escaped helper self-expires; it intentionally retains stdout.
        command.args(["-c", "setsid sleep 1 & exit 0"]);
        let start = Instant::now();
        let result = run(command, start + Duration::from_millis(100), |r| {
            super::super::read_capped(Some(r))
        });
        assert!(matches!(result, Err(RunFail::TimedOut)));
        assert!(start.elapsed() < Duration::from_millis(800));
    }
}
