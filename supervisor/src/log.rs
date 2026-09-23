//! `mantle log`: the shell's own stdout and stderr, kept somewhere a detached run can be read
//! from (ADR-0199).
//!
//! Every diagnostic in both binaries goes to stderr through `shared::log`, so this is `dup2` per
//! descriptor, not a logging framework. The Renderer inherits them through `process::spawn_group_leader`. A
//! descriptor replaced outright carries a panic to the file with no thread of ours in between; one
//! that had a destination of its own is drained by [`tee`] instead, which does.

use std::fs::File;
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Duration;

use nix::sys::stat::fstat;
use nix::unistd::{dup2_stderr, dup2_stdout};

use crate::instance;

/// How often `--follow` looks for new bytes. inotify would want a runtime in a subcommand that has
/// none, and a fifth of a second is nobody's problem in a log reader.
const POLL: Duration = Duration::from_millis(200);

/// Sends stdout and stderr to the instance directory's log, whatever else they were going to.
///
/// Every shell gets a log, so `mantle log` never has to answer with an older run's. `/dev/null` is
/// the only destination with nothing to lose and is replaced; a terminal, redirect or pipe is one
/// someone chose and is copied to. Per descriptor, or `mantle >mine.log 2>/dev/null` would send
/// half of it to one place.
pub fn capture(dir: &Path) -> io::Result<()> {
    let file = File::create(dir.join(instance::LOG))?;
    let (stdout, stderr) = (io::stdout(), io::stderr());
    let streams: [(BorrowedFd, Redirect); 2] =
        [(stdout.as_fd(), |to| dup2_stdout(to)), (stderr.as_fd(), |to| dup2_stderr(to))];
    for (target, redirect) in streams {
        match goes_to_dev_null(target) {
            true => redirect(file.as_fd())?,
            false => tee(&file, target, redirect)?,
        }
    }
    Ok(())
}

/// Makes one standard stream a second name for its argument.
type Redirect = fn(BorrowedFd) -> nix::Result<()>;

/// Replaces `target` with a pipe, drained into both the log and where `target` pointed before.
///
/// ponytail: bytes still in the pipe when the process aborts reach neither, so a panic under a
/// terminal lands on screen but not in the file. Draining from a process that outlives the writer
/// would fix it, the way `MANTLE_PAM_WORKER` is already a second process.
fn tee(file: &File, target: BorrowedFd, redirect: Redirect) -> io::Result<()> {
    let mut original = File::from(target.try_clone_to_owned()?);
    // Asked before the pipe takes the descriptor's place, which is the last moment it is the truth.
    // `shared::log::init` asks the same question afterwards and gets `false`, which is what keeps
    // the file plain (ADR-0229) while the terminal still gets colour, from `paint` below.
    let colour = original.is_terminal();
    let (mut pipe, writer) = io::pipe()?;
    redirect(writer.as_fd())?;
    drop(writer);
    let mut file = file.try_clone()?;
    std::thread::Builder::new().name("mantle-log-tee".into()).spawn(move || {
        let mut chunk = [0; 8192];
        let mut pending = Vec::new();
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => return,
                Ok(read) if pump(&chunk[..read], &mut file, &mut original, colour, &mut pending).is_err() => return,
                Ok(_) => (),
                // A signal interrupting the read is not the writer leaving; anything else is.
                Err(error) if error.kind() == io::ErrorKind::Interrupted => (),
                Err(_) => return,
            }
        }
    })?;
    Ok(())
}

/// Writes `bytes` to the log verbatim and onward as `mantle log` would show them, holding a line
/// split across two reads in `pending`.
fn pump(
    bytes: &[u8],
    file: &mut impl Write,
    original: &mut impl Write,
    colour: bool,
    pending: &mut Vec<u8>,
) -> io::Result<()> {
    file.write_all(bytes)?;
    match colour {
        true => paint(&mut { bytes }, original, pending),
        false => original.write_all(bytes),
    }
}

/// `mantle log [--follow]`: `dir`'s log, until its Supervisor exits.
///
/// `colour` paints what the file deliberately does not hold (ADR-0229): the shell wrote these bytes
/// to a file, so it left them plain, and the terminal they are finally shown on is this process's.
///
/// Boxed, not `io::Result`: `main` prints errors with `Debug`, where an `io::Error` shows as its
/// struct rather than a sentence.
pub fn print(dir: &Path, follow: bool, colour: bool, out: &mut impl Write) -> Result<(), Box<dyn std::error::Error>> {
    let path = dir.join(instance::LOG);
    let mut file = File::open(&path).map_err(|err| format!("no log at {}: {err}", path.display()))?;
    let lock = File::open(dir.join(instance::LOCK))
        .map_err(|err| format!("{} has no lock, so its shell died starting: {err}", dir.display()))?;
    // Said once, before any of it is printed: without this a dead run's bytes are indistinguishable
    // from a live one's, and `--follow` returns at once looking like it simply caught up.
    if !instance::is_locked(&lock)? {
        eprintln!("mantle: no shell is writing {}; this is the last run's output", path.display());
    }
    let mut writer_left = false;
    let mut pending = Vec::new();
    loop {
        // Plain output stays a byte-for-byte copy, so `mantle log | grep` is what it always was.
        if colour {
            paint(&mut file, out, &mut pending)?;
        } else {
            io::copy(&mut file, out)?;
        }
        out.flush()?;
        if !follow || writer_left {
            // A final line the writer never terminated still has to be shown.
            out.write_all(&pending)?;
            out.flush()?;
            return Ok(());
        }
        // Read now, acted on after one more pass above, so a last line written between the copy
        // and the exit still prints.
        writer_left = !instance::is_locked(&lock)?;
        if !writer_left {
            std::thread::sleep(POLL);
        }
    }
}

/// Copies whole lines, painting each. `pending` holds a line split across two reads, which
/// `--follow` produces whenever it catches the writer mid-line.
fn paint(from: &mut impl Read, to: &mut impl Write, pending: &mut Vec<u8>) -> io::Result<()> {
    from.read_to_end(pending)?;
    let Some(last) = pending.iter().rposition(|byte| *byte == b'\n') else { return Ok(()) };
    for line in pending[..=last].split_inclusive(|byte| *byte == b'\n') {
        match std::str::from_utf8(line) {
            Ok(text) => to.write_all(shared::log::colourise(text).as_bytes())?,
            Err(_) => to.write_all(line)?,
        }
    }
    pending.drain(..=last);
    Ok(())
}

/// Whether `fd` points to `/dev/null`.
fn goes_to_dev_null(fd: BorrowedFd) -> bool {
    fstat(fd).is_ok_and(|stat| std::fs::metadata("/dev/null").is_ok_and(|null| null.rdev() == stat.st_rdev))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_dev_null_counts_as_dev_null() {
        assert!(goes_to_dev_null(File::open("/dev/null").unwrap().as_fd()));
        assert!(!goes_to_dev_null(tempfile::tempfile().unwrap().as_fd()));
    }

    #[test]
    fn a_teed_descriptor_keeps_the_log_plain_and_paints_what_it_replaced() {
        let line = "08:00:00 WARN  tray: the item went away\n";
        let (mut log, mut terminal) = (Vec::new(), Vec::new());

        pump(line.as_bytes(), &mut log, &mut terminal, true, &mut Vec::new()).unwrap();

        assert_eq!(String::from_utf8(log).unwrap(), line, "ADR-0229: the file holds no colour");
        assert_eq!(String::from_utf8(terminal).unwrap(), shared::log::colourise(line));
    }

    #[test]
    fn a_line_split_across_two_reads_is_painted_once_it_is_whole() {
        let (head, tail) = ("08:00:00 INFO  idle: notif", "y is live\n");
        let (mut log, mut terminal, mut pending) = (Vec::new(), Vec::new(), Vec::new());

        pump(head.as_bytes(), &mut log, &mut terminal, true, &mut pending).unwrap();
        assert!(terminal.is_empty(), "half a line cannot be painted yet");
        pump(tail.as_bytes(), &mut log, &mut terminal, true, &mut pending).unwrap();

        assert_eq!(String::from_utf8(log).unwrap(), format!("{head}{tail}"));
        assert_eq!(String::from_utf8(terminal).unwrap(), shared::log::colourise(&format!("{head}{tail}")));
    }

    #[test]
    fn one_read_of_many_lines_paints_each_and_keeps_the_unfinished_tail() {
        let (mut terminal, mut pending) = (Vec::new(), Vec::new());

        paint(&mut "a\nb\nc".as_bytes(), &mut terminal, &mut pending).unwrap();

        let painted = ["a\n", "b\n"].map(shared::log::colourise).concat();
        assert_eq!(String::from_utf8(terminal).unwrap(), painted);
        assert_eq!(pending, b"c");
    }

    #[test]
    fn a_follow_ends_when_its_supervisor_drops_the_lock() {
        use std::io::Read;
        let root = tempfile::tempdir().unwrap();
        let (dir, held) = instance::claim(root.path(), 42, Path::new("/cfg")).unwrap();
        let mut log = File::create(dir.join(instance::LOG)).unwrap();
        writeln!(log, "first").unwrap();

        let (mut printed, out) = io::pipe().unwrap();
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            print(&dir, true, false, &mut { out }).unwrap();
            done.send(()).unwrap();
        });
        // Printed only after the follower opened the lock.
        printed.read_exact(&mut [0; 6]).unwrap();
        writeln!(log, "last words").unwrap();
        drop(held);

        finished.recv_timeout(Duration::from_secs(5)).expect("the follow outlived its Supervisor");
        let mut rest = String::new();
        printed.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "last words\n");
    }
}
