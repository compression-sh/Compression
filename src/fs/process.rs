//! Running another program and collecting what it prints, without letting it hold a thread for
//! ever or fill the memory: custom actions and git go through here.
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// What a program that ran to its end printed.
pub struct Output {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Why a run gave no output.
#[derive(Debug)]
pub enum Failure {
    /// It printed more than the limit on one of its streams, and was stopped.
    TooMuchOutput,
    /// It did not end within the time, and was stopped.
    TimedOut,
    /// The caller asked to stop waiting for it, and it was stopped.
    Cancelled,
    /// It could not be started or waited for.
    Io(std::io::Error),
}

enum Message {
    /// A stream ended, or was read up to the limit.
    Stream,
    Exit(std::io::Result<std::process::ExitStatus>),
}

/// Reads `stream` into `buffer`, which can be looked at while it is still being filled, and stops
/// one byte after `limit`.
fn read_into(stream: Box<dyn Read + Send>, buffer: &Mutex<Vec<u8>>, limit: usize, too_much: &AtomicBool) {
    let mut stream = stream;
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => {
                let mut bytes = buffer.lock().unwrap_or_else(|e| e.into_inner());
                bytes.extend_from_slice(&chunk[..n]);
                if bytes.len() > limit {
                    too_much.store(true, Ordering::SeqCst);
                    return;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Runs `command` (its input is closed) and collects what it prints, at most `limit` bytes on each
/// of the two streams, and waits for it for at most `timeout`. A program that goes over either is
/// killed together with what it started: it runs in a process group of its own. The wait does not
/// poll: the program's exit and its output arrive as messages. What the program left running may
/// hold its pipes open after it ended; its output is waited for one more second, and what was
/// read by then is the output.
pub fn run_capped(command: Command, limit: usize, timeout: Duration) -> Result<Output, Failure> {
    run_capped_with(command, None, limit, timeout, &|| false)
}

/// `run_capped`, with `input` written to the program's standard input (which is closed after it;
/// without input it is closed at once), and a `cancelled` question that is asked every 50
/// milliseconds while waiting: when it says yes, the program is killed and the answer is
/// `Failure::Cancelled`.
pub fn run_capped_with(mut command: Command, input: Option<Vec<u8>>, limit: usize, timeout: Duration, cancelled: &dyn Fn() -> bool) -> Result<Output, Failure> {
    use std::io::Write;
    use std::os::unix::process::CommandExt;
    command.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    let mut child = command.spawn().map_err(Failure::Io)?;
    let id = child.id();
    if let (Some(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
        // On a thread of its own: a program that does not read must not hold up the wait
        std::thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
    }
    let (tx, rx) = mpsc::channel();
    let too_much = Arc::new(AtomicBool::new(false));
    let buffers = [Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new()))];
    let streams: [Option<Box<dyn Read + Send>>; 2] = [
        child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
        child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
    ];
    for (stream, buffer) in streams.into_iter().zip(&buffers) {
        let (tx, too_much, buffer) = (tx.clone(), too_much.clone(), buffer.clone());
        std::thread::spawn(move || {
            if let Some(stream) = stream {
                read_into(stream, &buffer, limit, &too_much);
            }
            let _ = tx.send(Message::Stream);
        });
    }
    // The child is waited for on a thread of its own, also when it was killed: no zombie is left
    let exit_tx = tx.clone();
    std::thread::spawn(move || {
        let _ = exit_tx.send(Message::Exit(child.wait()));
    });
    drop(tx);

    let kill = || {
        // SAFETY: a signal to the process group this function made; a group that is gone only fails
        unsafe { libc::kill(-(id as i32), libc::SIGKILL) };
    };
    let deadline = std::time::Instant::now() + timeout;
    let (mut status, mut streams_ended, mut ended_at) = (None, 0, None);
    loop {
        let wait_until = match ended_at {
            Some(at) => at + Duration::from_secs(1),
            None => deadline,
        };
        if ended_at.is_none() && cancelled() {
            kill();
            return Err(Failure::Cancelled);
        }
        let slice = wait_until.saturating_duration_since(std::time::Instant::now()).min(Duration::from_millis(50));
        match rx.recv_timeout(slice) {
            Ok(Message::Stream) => {
                streams_ended += 1;
                if too_much.load(Ordering::SeqCst) {
                    kill();
                    return Err(Failure::TooMuchOutput);
                }
            }
            Ok(Message::Exit(result)) => {
                status = Some(result.map_err(Failure::Io)?);
                ended_at = Some(std::time::Instant::now());
            }
            Err(_) if ended_at.is_some() && std::time::Instant::now() >= wait_until => break,
            Err(_) if std::time::Instant::now() >= wait_until => {
                kill();
                return Err(Failure::TimedOut);
            }
            // A slice of the wait went by: ask again whether to go on
            Err(_) => {}
        }
        if status.is_some() && streams_ended == 2 {
            break;
        }
    }
    let take = |i: usize| std::mem::take(&mut *buffers[i].lock().unwrap_or_else(|e| e.into_inner()));
    Ok(Output { success: status.is_some_and(|s| s.success()), stdout: take(0), stderr: take(1) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    const SECS: Duration = Duration::from_secs(10);

    #[test]
    fn a_normal_program_gives_its_output_and_its_result() {
        let done = run_capped(sh("echo out; echo oops >&2; exit 3"), 1024, SECS).unwrap();
        assert!(!done.success);
        assert_eq!(done.stdout, b"out\n");
        assert_eq!(done.stderr, b"oops\n");
        assert!(run_capped(sh("true"), 1024, SECS).unwrap().success);
        // Exactly the limit is fine
        assert_eq!(run_capped(sh("printf aaaaaaaaaa"), 10, SECS).unwrap().stdout.len(), 10);
    }

    #[test]
    fn endless_output_is_stopped_at_the_limit_on_either_stream() {
        let started = std::time::Instant::now();
        assert!(matches!(run_capped(sh("while :; do echo xxxxxxxxxxxxxxxx; done"), 64 * 1024, Duration::from_secs(30)), Err(Failure::TooMuchOutput)));
        assert!(matches!(run_capped(sh("while :; do echo xxxxxxxxxxxxxxxx >&2; done"), 64 * 1024, Duration::from_secs(30)), Err(Failure::TooMuchOutput)));
        assert!(matches!(run_capped(sh("printf aaaaaaaaaaa"), 10, SECS), Err(Failure::TooMuchOutput)));
        assert!(started.elapsed() < Duration::from_secs(10), "the programs were not stopped at the limit");
    }

    #[test]
    fn a_program_that_hangs_is_stopped_at_the_time_limit_with_what_it_started() {
        let marker = std::env::temp_dir().join(format!("compression_process_hang_{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let started = std::time::Instant::now();
        // The shell starts a child of its own, which would write the marker after a while if it lived on
        let script = format!("(sleep 1; touch '{}') & wait", marker.display());
        assert!(matches!(run_capped(sh(&script), 1024, Duration::from_millis(200)), Err(Failure::TimedOut)));
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(1500));
        assert!(!marker.exists(), "what the program started was left running");
        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    fn waiting_can_be_called_off_and_input_reaches_the_program() {
        let started = std::time::Instant::now();
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let cancel = || asked.fetch_add(1, Ordering::SeqCst) >= 3;
        let result = run_capped_with(sh("sleep 30"), None, 1024, Duration::from_secs(60), &cancel);
        assert!(matches!(result, Err(Failure::Cancelled)), "the wait was not called off");
        assert!(started.elapsed() < Duration::from_secs(5), "the program was not stopped");
        // Input is written and closed, so a program that reads to the end finishes
        let done = run_capped_with(sh("cat >&2"), Some(b"secret\n".to_vec()), 1024, SECS, &|| false).unwrap();
        assert!(done.success);
        assert_eq!(done.stderr, b"secret\n");
    }

    #[test]
    fn a_program_that_cannot_be_started_is_an_error() {
        assert!(matches!(run_capped(Command::new("/nonexistent/compression-test-program"), 1024, SECS), Err(Failure::Io(_))));
    }

    #[test]
    fn something_the_program_left_running_does_not_hold_the_answer() {
        // The shell ends at once; the sleep it started keeps the pipes open for a few seconds
        let started = std::time::Instant::now();
        let done = run_capped(sh("echo hi; sleep 5 &"), 1024, SECS).unwrap();
        assert!(done.success && done.stdout == b"hi\n");
        assert!(started.elapsed() < Duration::from_secs(4), "the answer waited for the program's child");
    }
}
