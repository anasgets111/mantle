//! A child's output pipe that leaves tokio's epoll set while a line arrives in pieces.
//!
//! Linux wakes an epoll waiter on every write to a pipe that has ever been polled, read or not. A
//! registered pipe therefore wakes the I/O driver once per child `write`, and a child writing each
//! field of a line separately wakes it hundreds of times a line. After a read that ends mid-line,
//! [`Pipe`] unregisters and waits [`PIECE_LINGER`] so the rest piles up for one read: 400 pieces
//! cost 5 worker parks per line, not 70-82.

use std::fs::File;
use std::future::Future;
use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use nix::fcntl::{FcntlArg, OFlag, fcntl};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::net::unix::pipe::Receiver;
use tokio::time::{Instant, Sleep};

/// How long a line a child wrote in pieces waits for its next piece before reading again.
// ponytail: holds such lines 1-2 ms (tokio's timer tick), and a sparse fragment, such as a `\r`
// progress update, pays one unregister, timer, empty read and register. Upgrade: a per-call linger.
const PIECE_LINGER: Duration = Duration::from_millis(1);

/// A pipe's read end, held by `waiting` while registered and by `idle` while not. Both are empty
/// only once a failed register or unregister has lost it, which reads as EOF.
pub(crate) struct Pipe {
    waiting: Option<Receiver>,
    idle: Option<File>,
    /// Armed by a short read that ended mid-line; the next read waits it out.
    linger: Pin<Box<Sleep>>,
    lingering: bool,
}

impl Pipe {
    /// Wraps a pipe's read end, such as `ChildStdout::into_owned_fd`'s, and makes it nonblocking.
    pub(crate) fn new(fd: OwnedFd) -> io::Result<Self> {
        fcntl(&fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
        let linger = Box::pin(tokio::time::sleep(Duration::ZERO));
        Ok(Self { waiting: None, idle: Some(File::from(fd)), linger, lingering: false })
    }

    /// Arms the linger after a short read that ended mid-line, and says whether it did.
    fn linger_after(&mut self, buf: &ReadBuf<'_>, before: usize) -> bool {
        let filled = buf.filled();
        self.lingering = filled.len() > before && buf.remaining() > 0 && filled.last() != Some(&b'\n');
        if self.lingering {
            self.linger.as_mut().reset(Instant::now() + PIECE_LINGER);
        }
        self.lingering
    }
}

impl AsyncRead for Pipe {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.lingering {
            ready!(this.linger.as_mut().poll(cx));
            this.lingering = false;
        }
        let before = buf.filled().len();
        loop {
            if let Some(receiver) = &mut this.waiting {
                ready!(Pin::new(receiver).poll_read(cx, buf))?;
                if this.linger_after(buf, before) {
                    let receiver = this.waiting.take().expect("`waiting` was just read");
                    this.idle = Some(File::from(receiver.into_nonblocking_fd()?));
                }
                return Poll::Ready(Ok(()));
            }
            let Some(file) = &this.idle else { return Poll::Ready(Ok(())) };
            // Reads off tokio's registration skip its budget, and a pipe that never runs dry would
            // otherwise hold the task, and the `select!` arm beside it, forever.
            let coop = ready!(tokio::task::coop::poll_proceed(cx));
            match read(file, buf) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    let file = this.idle.take().expect("`idle` was just read");
                    this.waiting = Some(Receiver::from_file_unchecked(file)?);
                }
                result => {
                    if result.is_ok() {
                        coop.made_progress();
                        this.linger_after(buf, before);
                    }
                    return Poll::Ready(result);
                }
            }
        }
    }
}

fn read(mut file: &File, buf: &mut ReadBuf<'_>) -> io::Result<()> {
    let read = file.read(buf.initialize_unfilled())?;
    buf.advance(read);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::process::registry::BoundedLines;

    async fn waits(lines: &mut BoundedLines<Pipe>) {
        let read = tokio::time::timeout(Duration::from_millis(50), lines.next_line());
        assert!(read.await.is_err(), "the pipe is empty, so the read must be left waiting");
    }

    #[tokio::test(start_paused = true)]
    async fn lines_survive_each_wait_for_the_pipe_and_eof_ends_the_stream_either_way() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        let mut lines = BoundedLines::new(Pipe::new(reader.into()).unwrap(), "test".to_string());

        // Each wait registers the pipe and the next read deregisters it; no byte may go missing.
        writer.write_all(b"a").unwrap();
        waits(&mut lines).await;
        writer.write_all(b"b\n").unwrap();
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("ab"));

        writer.write_all(b"tail").unwrap();
        waits(&mut lines).await;
        drop(writer);
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("tail"), "EOF while waiting delivers the line");
        assert_eq!(lines.next_line().await.unwrap(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn the_pipe_leaves_epoll_only_while_a_line_arrives_in_pieces() {
        use tokio::io::AsyncReadExt;
        let (reader, mut writer) = std::io::pipe().unwrap();
        let mut pipe = Pipe::new(reader.into()).unwrap();
        let mut buf = [0; 8];
        let mut wait_then_read = async |pipe: &mut Pipe, data: &[u8]| {
            assert!(tokio::time::timeout(Duration::from_millis(50), pipe.read(&mut buf)).await.is_err());
            assert!(pipe.waiting.is_some(), "an empty pipe registers to wait");
            writer.write_all(data).unwrap();
            assert_eq!(pipe.read(&mut buf).await.unwrap(), data.len());
        };

        wait_then_read(&mut pipe, b"a").await;
        assert!(pipe.waiting.is_none(), "mid-line, the pipe leaves epoll so the next pieces wake nothing");
        wait_then_read(&mut pipe, b"b\n").await;
        assert!(pipe.waiting.is_some(), "after a whole line it stays, saving a register per line");
    }

    #[tokio::test(start_paused = true)]
    async fn only_a_short_read_that_ends_mid_line_waits_for_the_rest() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        let mut lines = BoundedLines::new(Pipe::new(reader.into()).unwrap(), "test".to_string());
        // `next_line` with `before` already written and `during` written once it is waiting.
        let mut held = async |before: &[u8], during: &[u8]| {
            writer.write_all(before).unwrap();
            let start = Instant::now();
            let (line, ()) = tokio::join!(lines.next_line(), async { writer.write_all(during).unwrap() });
            (line.unwrap().unwrap().len(), start.elapsed() >= PIECE_LINGER)
        };

        assert_eq!(held(b"whole\n", b"").await, (5, false), "a line written at once is not held");
        assert_eq!(held(b"pie", b"ces\n").await, (6, true), "a short read mid-line lingers for the next piece");
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_that_fills_the_buffer_mid_line_is_not_held() {
        // A file, not a pipe: a pipe past the user's soft page limit holds 4 KiB, and this write
        // would block. `BoundedLines` reads 8 KiB at a time, so the first read fills mid-line.
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&[vec![b'x'; 8 * 1024 + 10], vec![b'\n']].concat()).unwrap();
        std::io::Seek::rewind(&mut file).unwrap();
        let mut lines = BoundedLines::new(Pipe::new(file.into()).unwrap(), "test".to_string());
        let start = Instant::now();
        assert_eq!(lines.next_line().await.unwrap().map(|line| line.len()), Some(8 * 1024 + 10));
        assert!(start.elapsed() < PIECE_LINGER, "a full read is not a short one");
    }

    #[test]
    fn a_pipe_that_never_runs_dry_still_lets_its_task_move_on() {
        // A pipe with no gaps never registers, so only the budget makes `next_line` yield. Its own
        // thread and runtime let a hang fail the test instead of wedging it.
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            runtime.block_on(async {
                let flood = Pipe::new(File::open("/dev/zero").unwrap().into()).unwrap();
                let mut lines = BoundedLines::new(flood, "test".to_string());
                tokio::select! {
                    line = lines.next_line() => panic!("/dev/zero has no newline, so no line: {line:?}"),
                    () = tokio::task::yield_now() => {}
                }
            });
            done_tx.send(()).unwrap();
        });
        done_rx.recv_timeout(Duration::from_secs(5)).expect("a newline-free flood held its task");
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_error_is_returned_rather_than_waited_on() {
        // A write end refuses reads with EBADF, standing in for a pipe that fails.
        let (_reader, writer) = std::io::pipe().unwrap();
        let mut pipe = Pipe::new(writer.into()).unwrap();
        let mut buf = [0; 8];
        let read = tokio::time::timeout(Duration::from_secs(1), tokio::io::AsyncReadExt::read(&mut pipe, &mut buf));
        assert!(matches!(read.await, Ok(Err(_))), "the error must end the read, not register and wait");
    }
}
