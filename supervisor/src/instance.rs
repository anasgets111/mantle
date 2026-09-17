//! One `$XDG_RUNTIME_DIR/obelisk/<pid>/` per Supervisor, and how clients pick one (ADR-0222).

use std::ffi::OsString;
use std::fs::{DirBuilder, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const LOCK: &str = "instance.lock";
pub const LOG: &str = "shell.log";

/// A Supervisor's directory as `list` read it.
pub struct Instance {
    pub pid: u32,
    pub config: PathBuf,
    pub started: SystemTime,
    pub live: bool,
    pub has_log: bool,
}

/// A whole-file `F_OFD_*` request: the Supervisor's claim on its directory.
///
/// Not `flock`, for one property it lacks: an OFD lock can be asked about without being taken. A
/// reader probing with `flock` holds what it tests, and a shell starting in that window finds its
/// own directory taken. Both die with the last descriptor, so a crash frees them like an exit.
fn whole_file(kind: libc::c_int) -> libc::flock {
    libc::flock {
        l_type: kind as libc::c_short,
        l_whence: libc::SEEK_SET as libc::c_short,
        l_start: 0,
        l_len: 0,
        l_pid: 0,
    }
}

/// Takes the lock, or reports that someone else holds it. `file` must be open for writing.
fn take_lock(file: &File) -> io::Result<bool> {
    let lock = whole_file(libc::F_WRLCK);
    // SAFETY: a live descriptor and a fully initialized `flock`. `F_OFD_SETLK` never blocks.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &lock) } == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    // Only contention means someone else has it; anything else is reported, not read as busy.
    match err.raw_os_error() {
        Some(libc::EACCES | libc::EAGAIN) => Ok(false),
        _ => Err(err),
    }
}

/// Whether anyone holds the lock on `file`. A query: it takes nothing.
pub fn is_locked(file: &File) -> io::Result<bool> {
    let mut lock = whole_file(libc::F_WRLCK);
    // SAFETY: as `take_lock`. `F_OFD_GETLK` only reads, overwriting `lock` with the holder it
    // found or `F_UNLCK`.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_GETLK, &mut lock) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(lock.l_type != libc::F_UNLCK as libc::c_short)
}

/// Whether a Supervisor still holds `dir`.
pub fn is_live(dir: &Path) -> bool {
    File::open(dir.join(LOCK)).and_then(|file| is_locked(&file)).unwrap_or(false)
}

/// Creates `root/<pid>/` for this Supervisor and holds its lock for as long as the file lives.
///
/// ponytail: stopped runs stay until logout clears the tmpfs; prune by age if that ever pressures it.
pub fn claim(root: &Path, pid: u32, config: &Path) -> io::Result<File> {
    DirBuilder::new().recursive(true).mode(0o700).create(root)?;
    let dir = root.join(pid.to_string());
    // A live holder of our own pid is a Supervisor in another pid namespace sharing this login.
    if is_live(&dir) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} is held by a running shell", dir.display()),
        ));
    }
    if let Err(err) = std::fs::remove_dir_all(&dir)
        && err.kind() != io::ErrorKind::NotFound
    {
        return Err(err);
    }
    DirBuilder::new().mode(0o700).create(&dir)?;
    // Before the lock, so a live instance always has a readable `config`.
    std::fs::write(dir.join("config"), config.as_os_str().as_bytes())?;
    let lock = File::create(dir.join(LOCK))?;
    if !take_lock(&lock)? {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} is already held", dir.display())));
    }
    Ok(lock)
}

/// Every numbered directory under `root` that has a `config`.
pub fn list(root: &Path) -> Vec<Instance> {
    let Ok(entries) = std::fs::read_dir(root) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse().ok()?;
            let dir = entry.path();
            let config = dir.join("config");
            Some(Instance {
                pid,
                started: std::fs::metadata(&config).and_then(|meta| meta.modified()).ok()?,
                config: PathBuf::from(OsString::from_vec(std::fs::read(config).ok()?)),
                live: is_live(&dir),
                has_log: dir.join(LOG).exists(),
            })
        })
        .collect()
}

/// Pids wrap, so start time orders instances.
fn newest<'a>(instances: impl Iterator<Item = &'a Instance>) -> Option<u32> {
    instances.max_by_key(|instance| instance.started).map(|instance| instance.pid)
}

/// The Supervisor `set`, `toggle` and `call` reach. `explicit` is `-c`, never an inherited
/// `$OBELISK_CONFIG_DIR`.
pub fn select_command(instances: &[Instance], pid: Option<u32>, config: &Path, explicit: bool) -> Result<u32, String> {
    let live = || instances.iter().filter(|instance| instance.live);
    if let Some(pid) = pid {
        return newest(live().filter(|instance| instance.pid == pid))
            .ok_or_else(|| format!("no running shell with pid {pid}; obelisk list shows them"));
    }
    newest(live().filter(|instance| instance.config == config))
        .or_else(|| if explicit { None } else { newest(live()) })
        .ok_or_else(|| match explicit {
            true => format!("no shell is running on {}", config.display()),
            false => "no shell is running".to_string(),
        })
}

/// The directory `obelisk log` reads, and a note when it picked one of several live shells; `config`
/// is `-c`.
pub fn select_log(
    instances: &[Instance],
    pid: Option<u32>,
    config: Option<&Path>,
) -> Result<(u32, Option<String>), String> {
    if let Some(pid) = pid {
        return newest(instances.iter().filter(|instance| instance.pid == pid))
            .map(|pid| (pid, None))
            .ok_or_else(|| format!("no shell with pid {pid}; obelisk list shows the running ones"));
    }
    let logs = || instances.iter().filter(|i| i.has_log && config.is_none_or(|config| i.config == config));
    let live = || logs().filter(|instance| instance.live);
    if let Some(pid) = newest(live()) {
        let count = live().count();
        let on = config.map(|config| format!(" on {}", config.display())).unwrap_or_default();
        let note = (count > 1).then(|| {
            format!("{count} shells running{on}; showing pid {pid} (newest). --pid picks one; obelisk list shows them")
        });
        return Ok((pid, note));
    }
    newest(logs()).map(|pid| (pid, None)).ok_or_else(|| match config {
        Some(config) => format!("no shell log this login on {}", config.display()),
        None => "no shell log this login".to_string(),
    })
}

/// `obelisk list`'s UPTIME: two units at most.
pub fn format_uptime(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match secs {
        ..60 => format!("{secs}s"),
        60..3600 => format!("{}m{:02}s", secs / 60, secs % 60),
        3600..86_400 => format!("{}h{:02}m", secs / 3600, secs / 60 % 60),
        _ => format!("{}d{:02}h", secs / 86_400, secs / 3600 % 24),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs::OpenOptions;

    use super::*;

    /// Why it is `F_OFD_GETLK` and not `flock`. OFD locks conflict between two descriptions in one
    /// process as they do between processes, so a second open stands in for a second process.
    #[test]
    fn the_holder_is_identified_by_a_lock_a_reader_can_ask_about_without_taking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOCK);
        let shell = File::create(&path).unwrap();
        let reader = File::open(&path).unwrap();

        assert!(!is_locked(&reader).unwrap(), "an unlocked file has no holder");
        assert!(take_lock(&shell).unwrap(), "a free lock is takeable");
        assert!(is_locked(&reader).unwrap(), "a held lock reads as live, through a read-only descriptor");

        // Probing must leave the lock where it found it.
        let second = OpenOptions::new().write(true).open(&path).unwrap();
        assert!(!take_lock(&second).unwrap(), "a second taker loses");
        drop(second);

        // What a crash does, since the kernel closes the descriptors either way.
        drop(shell);
        retry(|| (!is_locked(&reader).unwrap()).then_some(()));
    }

    /// A test forking in parallel keeps a lock alive until its child execs, so a release is awaited.
    pub(crate) fn retry<T>(mut attempt: impl FnMut() -> Option<T>) -> T {
        (0..500)
            .find_map(|_| {
                let result = attempt();
                if result.is_none() {
                    std::thread::sleep(Duration::from_millis(10));
                }
                result
            })
            .expect("the lock was never released")
    }

    /// An unlocked `root/<name>/config` naming `config`, started `age` ago.
    fn leftover(root: &Path, name: &str, config: &str, age: u64) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config"), config).unwrap();
        let started = SystemTime::now() - Duration::from_secs(age);
        File::options().write(true).open(dir.join("config")).unwrap().set_modified(started).unwrap();
        dir
    }

    #[test]
    fn claim_creates_a_private_dir_holding_config_and_a_held_lock() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let root = root.path().join("obelisk");

        let _held = claim(&root, 42, Path::new("/cfg")).unwrap();

        let dir = root.join("42");
        for private in [&root, &dir] {
            assert_eq!(std::fs::metadata(private).unwrap().permissions().mode() & 0o777, 0o700);
        }
        assert_eq!(std::fs::read(dir.join("config")).unwrap(), b"/cfg");
        assert!(is_live(&dir), "the lock is seen from a second descriptor");
    }

    #[test]
    fn claim_clears_a_dead_leftover_under_its_own_pid_and_refuses_a_live_one() {
        let root = tempfile::tempdir().unwrap();
        let stale = leftover(root.path(), "42", "/old", 60);
        std::fs::write(stale.join(LOG), "an earlier run").unwrap();

        let held = claim(root.path(), 42, Path::new("/new")).unwrap();
        assert!(!stale.join(LOG).exists(), "a reused pid starts from an empty directory");
        assert!(claim(root.path(), 42, Path::new("/new")).is_err(), "another pid namespace's live shell stays");
        drop(held);
    }

    fn at(pid: u32, config: &str, age: u64, live: bool, has_log: bool) -> Instance {
        let started = SystemTime::UNIX_EPOCH + Duration::from_secs(1000 - age);
        Instance { pid, config: config.into(), started, live, has_log }
    }

    #[test]
    fn commands_reach_the_newest_live_on_this_config_and_fall_back_only_without_dash_c() {
        let a = Path::new("/a");
        let running = [at(900, "/a", 50, true, true), at(100, "/a", 5, true, true), at(7, "/b", 1, true, true)];
        assert_eq!(select_command(&running, None, a, false), Ok(100), "newest, not highest pid");
        assert_eq!(select_command(&running, Some(900), a, true), Ok(900));
        assert_eq!(select_command(&running, None, Path::new("/c"), false), Ok(7), "any live without -c");
        assert!(select_command(&running, None, Path::new("/c"), true).is_err(), "-c names the one it wants");

        let dead = [at(5, "/a", 5, false, true)];
        assert!(select_command(&dead, Some(5), a, false).is_err(), "a dead pid takes no command");
        assert!(select_command(&dead, None, a, false).is_err());
    }

    #[test]
    fn log_reads_the_newest_live_with_a_log_then_the_newest_dead_run() {
        let a = Path::new("/a");
        let instances = [
            at(1, "/a", 50, false, true),
            at(2, "/a", 40, true, true),
            at(3, "/b", 30, true, true),
            at(4, "/b", 1, true, false),
        ];
        assert_eq!(select_log(&instances, None, Some(a)), Ok((2, None)), "-c narrows; a dead run is not counted");
        assert_eq!(select_log(&instances[..1], None, None), Ok((1, None)), "nothing live: the last run");
        assert_eq!(select_log(&instances, Some(1), None), Ok((1, None)), "a kept dead run is readable by pid");
        assert_eq!(
            select_log(&instances, None, None),
            Ok((3, Some("2 shells running; showing pid 3 (newest). --pid picks one; obelisk list shows them".into()))),
            "a terminal-started shell has nothing to read, and is not counted"
        );
        assert!(select_log(&instances, Some(9), None).is_err());
        assert!(select_log(&instances, None, Some(Path::new("/c"))).is_err());
    }

    #[test]
    fn format_uptime_rolls_units() {
        for (secs, shown) in [(0, "0s"), (59, "59s"), (60, "1m00s"), (3600, "1h00m"), (86_400, "1d00h")] {
            assert_eq!(format_uptime(Duration::from_secs(secs)), shown);
        }
    }
}
