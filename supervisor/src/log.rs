//! `obelisk log`: the shell's own stdout and stderr, kept somewhere a detached run can be read
//! from (ADR-0199).
//!
//! Every diagnostic in both binaries is an `eprintln!`, so this is a `dup2` per descriptor, not a
//! logging framework. The Renderer inherits them through `process::spawn_group_leader`, and a
//! panic reaches the file directly because no thread of ours sits in between.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::Duration;

use crate::instance;

/// How often `--follow` looks for new bytes. inotify would want a runtime in a subcommand that has
/// none, and a fifth of a second is nobody's problem in a log reader.
const POLL: Duration = Duration::from_millis(200);

/// Points whichever of stdout and stderr go to `/dev/null` at the instance directory's log.
///
/// `/dev/null` is the only destination with nothing to lose; a terminal, redirect or pipe is one
/// someone chose. Per descriptor, or `obelisk >mine.log 2>/dev/null` leaves `mine.log` empty.
pub fn capture(dir: &Path) -> io::Result<()> {
    let discarded: Vec<i32> =
        [libc::STDOUT_FILENO, libc::STDERR_FILENO].into_iter().filter(|fd| goes_to_dev_null(*fd)).collect();
    if discarded.is_empty() {
        return Ok(());
    }
    let file = File::create(dir.join(instance::LOG))?;
    for target in discarded {
        // SAFETY: both arguments are live descriptors. `file`'s comes from the `open` above, and
        // the target is a standard stream this process has not closed.
        if unsafe { libc::dup2(file.as_raw_fd(), target) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// `obelisk log [--follow]`: `dir`'s log, until its Supervisor exits.
///
/// Boxed, not `io::Result`: `main` prints errors with `Debug`, where an `io::Error` shows as its
/// struct rather than a sentence.
pub fn print(dir: &Path, follow: bool, out: &mut impl Write) -> Result<(), Box<dyn std::error::Error>> {
    let path = dir.join(instance::LOG);
    let mut file = File::open(&path).map_err(|err| {
        format!("no log at {}: {err}. A shell with a terminal or a redirect writes there instead", path.display())
    })?;
    let lock = File::open(dir.join(instance::LOCK))
        .map_err(|err| format!("{} has no lock, so its shell died starting: {err}", dir.display()))?;
    // Said once, before any of it is printed: without this a dead run's bytes are indistinguishable
    // from a live one's, and `--follow` returns at once looking like it simply caught up.
    if !instance::is_locked(&lock)? {
        eprintln!("obelisk: no shell is writing {}; this is the last run's output", path.display());
    }
    let mut writer_left = false;
    loop {
        io::copy(&mut file, out)?;
        out.flush()?;
        if !follow || writer_left {
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

/// Whether `fd` goes to `/dev/null`, as `/proc/self/fd` spells it. Only a confirmed match counts:
/// a failed readlink says nothing, and guessing is how a redirect gets swallowed.
fn goes_to_dev_null(fd: i32) -> bool {
    std::fs::read_link(format!("/proc/self/fd/{fd}")).is_ok_and(|target| target == Path::new("/dev/null"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            print(&dir, true, &mut { out }).unwrap();
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
