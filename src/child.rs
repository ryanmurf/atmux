//! Bounded output from helper processes.
//!
//! The web process asks tmux, git, and platform tools for output from blocking
//! threads, often while holding a lock the request handlers also need. A helper
//! that never answers must therefore turn into an error, not a stuck thread:
//! both pipes are drained by detached reader threads that stream chunks back
//! over a channel, so the caller waits on the deadline alone and never joins a
//! reader whose pipe a grandchild still holds open.
use std::{
    io::{self, Read},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

/// How long after the child exits its remaining pipe output is collected.
const DRAIN_GRACE: Duration = Duration::from_millis(250);
const POLL: Duration = Duration::from_millis(5);

#[derive(Debug)]
pub(crate) struct BoundedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Either stream exceeded the byte limit; the kept prefix is exact.
    pub truncated: bool,
}

impl BoundedOutput {
    pub(crate) fn into_output(self) -> Output {
        Output {
            status: self.status,
            stdout: self.stdout,
            stderr: self.stderr,
        }
    }
}

enum Chunk {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Done,
}

/// Runs `command` with piped stdout/stderr and returns its output, or a
/// `TimedOut` error after killing it when it outlives `timeout`. At most
/// `limit` bytes of each stream are kept.
///
/// # Errors
///
/// Spawn failures are returned as-is; a timeout is `ErrorKind::TimedOut`.
pub(crate) fn output_within(
    command: &mut Command,
    timeout: Duration,
    limit: usize,
) -> io::Result<BoundedOutput> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (sender, receiver) = mpsc::channel();
    let readers: [Option<(Box<dyn Read + Send>, bool)>; 2] = [
        child
            .stdout
            .take()
            .map(|pipe| (Box::new(pipe) as Box<dyn Read + Send>, true)),
        child
            .stderr
            .take()
            .map(|pipe| (Box::new(pipe) as Box<dyn Read + Send>, false)),
    ];
    let mut open = 0;
    for (pipe, is_stdout) in readers.into_iter().flatten() {
        open += 1;
        let sender = sender.clone();
        thread::spawn(move || pump(pipe, is_stdout, &sender));
    }
    drop(sender);
    let deadline = Instant::now() + timeout;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut truncated = false;
    let mut status = None;
    let mut drain_until = None;
    while open > 0 {
        let now = Instant::now();
        if status.is_none() {
            if let Some(exit) = child.try_wait()? {
                status = Some(exit);
                drain_until = Some(now + DRAIN_GRACE);
            } else if now >= deadline {
                return Err(abandon(&mut child, "helper process exceeded its deadline"));
            }
        }
        if let Some(until) = drain_until
            && now >= until
        {
            // The child is gone; whatever still holds its pipes is not ours.
            break;
        }
        match receiver.recv_timeout(POLL) {
            Ok(Chunk::Stdout(bytes)) => truncated |= append(&mut stdout, &bytes, limit),
            Ok(Chunk::Stderr(bytes)) => truncated |= append(&mut stderr, &bytes, limit),
            Ok(Chunk::Done) => open -= 1,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = match status {
        Some(status) => status,
        None => wait_within(
            &mut child,
            &receiver,
            deadline,
            limit,
            &mut stdout,
            &mut stderr,
        )?,
    };
    Ok(BoundedOutput {
        status,
        stdout,
        stderr,
        truncated,
    })
}

/// Both pipes closed before the child exited: keep waiting for its status
/// until the deadline, which is the common `tmux` and `git` shape.
fn wait_within(
    child: &mut Child,
    receiver: &Receiver<Chunk>,
    deadline: Instant,
    limit: usize,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
) -> io::Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            // Late chunks are still exact output; keep them.
            while let Ok(chunk) = receiver.try_recv() {
                match chunk {
                    Chunk::Stdout(bytes) => {
                        append(stdout, &bytes, limit);
                    }
                    Chunk::Stderr(bytes) => {
                        append(stderr, &bytes, limit);
                    }
                    Chunk::Done => {}
                }
            }
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(abandon(child, "helper process exceeded its deadline"));
        }
        thread::sleep(POLL);
    }
}

fn abandon(child: &mut Child, message: &str) -> io::Error {
    let _ = child.kill();
    let _ = child.wait();
    io::Error::new(io::ErrorKind::TimedOut, message.to_owned())
}

/// Appends up to the limit; returns whether anything was dropped.
fn append(into: &mut Vec<u8>, bytes: &[u8], limit: usize) -> bool {
    let room = limit.saturating_sub(into.len());
    into.extend_from_slice(&bytes[..bytes.len().min(room)]);
    bytes.len() > room
}

fn pump(mut pipe: impl Read, is_stdout: bool, sender: &mpsc::Sender<Chunk>) {
    let mut buffer = [0; 16 * 1024];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let bytes = buffer[..n].to_vec();
                let chunk = if is_stdout {
                    Chunk::Stdout(bytes)
                } else {
                    Chunk::Stderr(bytes)
                };
                if sender.send(chunk).is_err() {
                    break;
                }
            }
        }
    }
    let _ = sender.send(Chunk::Done);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]).stdin(Stdio::null());
        command
    }

    #[test]
    fn collects_both_streams_and_the_status() {
        let output = output_within(
            &mut sh("printf out; printf err >&2; exit 3"),
            Duration::from_secs(5),
            1024,
        )
        .unwrap();
        assert_eq!(output.stdout, b"out");
        assert_eq!(output.stderr, b"err");
        assert_eq!(output.status.code(), Some(3));
        assert!(!output.truncated);
    }

    #[test]
    fn keeps_an_exact_prefix_when_output_exceeds_the_limit() {
        let output = output_within(&mut sh("printf abcdefgh"), Duration::from_secs(5), 3).unwrap();
        assert_eq!(output.stdout, b"abc");
        assert!(output.truncated);
    }

    #[test]
    fn a_silent_child_becomes_a_timeout_and_is_gone() {
        let started = Instant::now();
        let error = output_within(
            &mut sh("echo $$; sleep 30"),
            Duration::from_millis(300),
            1024,
        )
        .expect_err("the sleep outlives the deadline");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_pipe_kept_open_by_a_grandchild_does_not_stall_the_caller() {
        // The sh exits at once; its background child keeps stdout open.
        let started = Instant::now();
        let output = output_within(
            &mut sh("(sleep 20 &) ; printf parent-done"),
            Duration::from_secs(5),
            1024,
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"parent-done");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }
}
