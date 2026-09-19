//! [`StorageController`] owns JSON files declared with `persistent_table`, keyed by absolute path
//! (ADR-0136).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use inotify::{Inotify, WatchMask, Watches};
use serde_json::{Map, Value};
use shared::{info, warn};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

/// Delay after the last write before rewriting. Scroll offsets and search drafts can call `:set()`
/// per keystroke; each otherwise serializes, writes, and renames.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(1000);

/// `mantle.storage`'s payload (ADR-0136).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct StorageState {
    /// One entry per declared `persistent_table`, keyed by the absolute `path` joined from `path`
    /// and `name`. Absent until declared, so unopened files read as `nil`, not an empty table.
    pub files: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageSignal {
    Changed,
}

/// Behind one lock, so a sync sees memory and the last disk copy together.
#[derive(Default)]
struct Stores {
    files: HashMap<String, Store>,
    watches: Option<Watches>,
}

#[derive(Default)]
struct Store {
    values: Map<String, Value>,
    defaults: Value,
    /// The file as this shell last read or wrote it; any other content is someone else's write.
    on_disk: Option<Map<String, Value>>,
    /// The parse error last logged, so a file left broken logs once.
    error: Option<String>,
}

pub struct StorageController {
    stores: Arc<Mutex<Stores>>,
    /// One pending save per file, replaced by its next write. Files debounce independently; one
    /// file written every second cannot starve another's save.
    saves: Mutex<HashMap<PathBuf, JoinHandle<()>>>,
    signal_tx: UnboundedSender<StorageSignal>,
}

impl StorageController {
    /// Watches declared files for other writers, other shells included (ADR-0223).
    pub fn new(signal_tx: UnboundedSender<StorageSignal>) -> Self {
        let stores = Arc::new(Mutex::new(Stores::default()));
        match Inotify::init().and_then(|inotify| Ok((inotify.watches(), inotify.into_event_stream(vec![0u8; 4096])?))) {
            Ok((watches, mut events)) => {
                stores.lock().expect("storage state mutex poisoned").watches = Some(watches);
                let (stores, signal_tx) = (Arc::clone(&stores), signal_tx.clone());
                tokio::spawn(async move {
                    while let Some(event) = events.next().await {
                        let event = match event {
                            Ok(event) => event,
                            Err(err) => {
                                warn!("reading the store watch failed: {err}");
                                // A failure that repeats must not spin.
                                tokio::time::sleep(Duration::from_secs(1)).await;
                                continue;
                            }
                        };
                        let guard = stores.lock().expect("storage state mutex poisoned");
                        // A nameless event is a queue overflow, which may have hidden any store's write.
                        let named = guard
                            .files
                            .keys()
                            .map(PathBuf::from)
                            .filter(|path| event.name.is_none() || path.file_name() == event.name.as_deref());
                        for path in named.collect::<Vec<_>>() {
                            let (stores, signal_tx) = (Arc::clone(&stores), signal_tx.clone());
                            tokio::task::spawn_blocking(move || sync(&stores, &path, false, &signal_tx));
                        }
                    }
                });
            }
            Err(err) => warn!("cannot watch stores, so other writers go unseen: {err}"),
        }
        Self { stores, saves: Mutex::new(HashMap::new()), signal_tx }
    }

    pub fn snapshot(&self) -> StorageState {
        let guard = self.stores.lock().expect("storage state mutex poisoned");
        StorageState {
            files: guard.files.iter().map(|(key, store)| (key.clone(), store.values.clone().into())).collect(),
        }
    }

    /// `persistent_table { path, name, defaults }`, re-sent each evaluation (ADR-0136 decision 1).
    /// The first declaration reads the file; later ones use the newer in-memory copy.
    ///
    /// `defaults` fills missing keys without overwriting stored ones, so adding a default is a new
    /// key, not a reset. A changed merge schedules a save, creating the file on first run.
    pub fn open(&self, path: &str, defaults: &Value) {
        let Some(path) = absolute_path(path) else {
            warn!("refused to open {path:?}; a store's path must be absolute");
            return;
        };
        let key = path.to_string_lossy().into_owned();

        let (declared, changed) = {
            let mut guard = self.stores.lock().expect("storage state mutex poisoned");
            let Stores { files, watches } = &mut *guard;
            let declared = !files.contains_key(&key);
            let store = files.entry(key).or_default();
            if declared {
                watch(watches.as_mut(), &path);
                store.on_disk = load(&path).unwrap_or_else(|err| {
                    log_parse_error(store, &path, err);
                    None
                });
                store.values = store.on_disk.clone().unwrap_or_default();
            }
            store.defaults = defaults.clone();
            (declared, fill_missing(&mut store.values, defaults))
        };

        if changed {
            self.schedule_save(path);
        }
        if declared || changed {
            let _ = self.signal_tx.send(StorageSignal::Changed);
        }
    }

    /// `store:set(key, value)` (ADR-0136 decision 2): stores one key, pushes immediately for the
    /// next resolve, and saves after writes stop.
    ///
    /// JSON `null` deletes the key; this is how Lua `nil` arrives.
    ///
    /// A write that changes nothing publishes nothing, sparing a whole-store snapshot and the
    /// renderer it would dirty; configs need no equality guard of their own around each write.
    ///
    /// The save stays unconditional: rewriting is the only repair for a deleted file.
    pub fn set(&self, path: &str, key: &str, value: Value) {
        let Some(path) = absolute_path(path) else {
            warn!("refused a write to {path:?}; a store's path must be absolute");
            return;
        };
        if key.is_empty() {
            warn!("refused a write to {}; a key cannot be empty", path.display());
            return;
        }

        let changed = {
            let mut guard = self.stores.lock().expect("storage state mutex poisoned");
            let Some(store) = guard.files.get_mut(&*path.to_string_lossy()) else {
                warn!("refused a write to {}; no persistent_table declared it", path.display());
                return;
            };
            match value {
                Value::Null => store.values.remove(key).is_some(),
                value if store.values.get(key) == Some(&value) => false,
                value => {
                    store.values.insert(key.to_string(), value);
                    true
                }
            }
        };

        self.schedule_save(path);
        if changed {
            let _ = self.signal_tx.send(StorageSignal::Changed);
        }
    }

    /// Replaces this file's pending save with one [`SAVE_DEBOUNCE`] away.
    ///
    /// ponytail: a save still in the window at session end is lost. Upgrade with a Supervisor
    /// shutdown flush shared by controllers.
    fn schedule_save(&self, path: PathBuf) {
        let (stores, signal_tx, target) = (Arc::clone(&self.stores), self.signal_tx.clone(), path.clone());
        let handle = tokio::spawn(async move {
            tokio::time::sleep(SAVE_DEBOUNCE).await;
            // Off the Supervisor loop: a read, directory creation, write, and rename, potentially on
            // a spun-down disk or NFS mount.
            let _ = tokio::task::spawn_blocking(move || sync(&stores, &target, true, &signal_tx)).await;
        });
        if let Some(previous) = self.saves.lock().expect("storage saves mutex poisoned").insert(path, handle) {
            previous.abort();
        }
    }
}

/// Takes `path`'s disk copy whole when someone else changed it, dropping unsaved writes, pushes a
/// difference, and writes when `save` asks or a just-fixed file lacks defaults (ADR-0223).
fn sync(stores: &Mutex<Stores>, path: &Path, save: bool, signal_tx: &UnboundedSender<StorageSignal>) {
    // Read to rename in turn, so a watch and a save cannot land out of order.
    static SERIAL: Mutex<()> = Mutex::new(());
    let _serial = SERIAL.lock().expect("storage sync mutex poisoned");
    let key = path.to_string_lossy();
    let disk = load(path);
    let contents = {
        let mut guard = stores.lock().expect("storage state mutex poisoned");
        let Stores { files, watches } = &mut *guard;
        let Some(store) = files.get_mut(&*key) else { return };
        let disk = match disk {
            Ok(disk) => disk,
            Err(err) => return log_parse_error(store, path, err),
        };
        let fixed = store.error.take().is_some();
        if fixed {
            info!("{} is readable again", path.display());
        }
        if let Some(disk) = disk.filter(|disk| store.on_disk.as_ref() != Some(disk)) {
            let mut values = disk.clone();
            fill_missing(&mut values, &store.defaults);
            store.on_disk = Some(disk);
            if store.values != values {
                store.values = values;
                let _ = signal_tx.send(StorageSignal::Changed);
            }
        }
        if !(save || fixed && Some(&store.values) != store.on_disk.as_ref()) {
            return;
        }
        watch(watches.as_mut(), path);
        store.values.clone()
    };
    if let Err(err) = write(path, &contents) {
        return warn!("could not save {}: {err}", path.display());
    }
    if let Some(store) = stores.lock().expect("storage state mutex poisoned").files.get_mut(&*key) {
        store.on_disk = Some(contents);
    }
}

/// Creates `path`'s directory and watches it for writes that land on a name, again at each save in
/// case the directory was recreated. Our temporary's name never matches a declared file.
fn watch(watches: Option<&mut Watches>, path: &Path) {
    let Some(dir) = path.parent() else { return };
    let mask = WatchMask::CLOSE_WRITE | WatchMask::MOVED_TO;
    let watched = std::fs::create_dir_all(dir).and_then(|()| watches.map_or(Ok(()), |w| w.add(dir, mask).map(drop)));
    if let Err(err) = watched {
        warn!("cannot watch {}: {err}", dir.display());
    }
}

fn log_parse_error(store: &mut Store, path: &Path, err: String) {
    if store.error.as_ref() != Some(&err) {
        warn!("{} keeps its last values and is not saved over until it parses: {err}", path.display());
        store.error = Some(err);
    }
}

/// The named path, or `None` when relative. Relative paths use the unset Supervisor working
/// directory and would land somewhere neither the author nor next session can name
/// (ADR-0136 decision 6).
fn absolute_path(path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(path);
    path.is_absolute().then_some(path)
}

/// `None` when missing, normal on first run; an error for anything a save would destroy.
fn load(path: &Path) -> Result<Option<Map<String, Value>>, String> {
    match std::fs::read_to_string(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.to_string()),
        Ok(contents) => match serde_json::from_str(&contents).map_err(|err| err.to_string())? {
            Value::Object(map) => Ok(Some(map)),
            _ => Err("not a JSON object".to_string()),
        },
    }
}

/// Copies missing top-level keys from `defaults` into `stored` and reports whether it changed.
/// Values, including tables, replace as one key; nested tables do not merge.
fn fill_missing(stored: &mut Map<String, Value>, defaults: &Value) -> bool {
    let Some(defaults) = defaults.as_object() else { return false };
    let mut changed = false;
    for (key, value) in defaults {
        if !stored.contains_key(key) {
            stored.insert(key.clone(), value.clone());
            changed = true;
        }
    }
    changed
}

/// Writes pretty JSON to a same-directory temporary file, then renames atomically within one
/// filesystem. A crash cannot leave a half-written file.
fn write(path: &Path, contents: &Map<String, Value>) -> std::io::Result<()> {
    let mut serialized = serde_json::to_vec_pretty(contents).map_err(std::io::Error::other)?;
    serialized.push(b'\n');

    // The temporary has to be a name no declared table can also be. `Path::with_extension` was
    // worse than it looked: it derives from the *stem*, so `notes.json` and `notes.db` shared one
    // `notes.json.tmp` and either rename could publish the other's bytes. Appending alone is not
    // enough either -- a config declaring both `foo` and `foo.tmp` would have the first table's
    // temporary land on the second table's file. The leading dot and the pid together are outside
    // what `persistent_table` hands us, and the pid keeps a second supervisor off this one's
    // temporary.
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    std::fs::write(&temporary, &serialized)?;
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn controller() -> (StorageController, tokio::sync::mpsc::UnboundedReceiver<StorageSignal>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (StorageController::new(tx), rx)
    }

    #[tokio::test]
    async fn defaults_fill_a_file_that_does_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json").to_string_lossy().into_owned();
        let (controller, _rx) = controller();

        controller.open(&path, &json!({ "theme": "mocha", "dnd": false }));

        assert_eq!(controller.snapshot().files[&path], json!({ "theme": "mocha", "dnd": false }));
    }

    #[tokio::test]
    async fn a_stored_key_wins_over_a_default_and_a_new_default_is_added_beside_it() {
        // Reload: an author adds a default after the config already ran.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{ "theme": "latte" }"#).unwrap();
        let path = path.to_string_lossy().into_owned();
        let (controller, _rx) = controller();

        controller.open(&path, &json!({ "theme": "mocha", "dnd": true }));

        assert_eq!(controller.snapshot().files[&path], json!({ "theme": "latte", "dnd": true }));
    }

    #[tokio::test]
    async fn a_reopen_does_not_re_read_the_file_under_a_live_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json").to_string_lossy().into_owned();
        let (controller, _rx) = controller();
        controller.open(&path, &json!({ "theme": "mocha" }));

        controller.set(&path, "theme", json!("latte"));
        controller.open(&path, &json!({ "theme": "mocha" }));

        assert_eq!(
            controller.snapshot().files[&path]["theme"],
            json!("latte"),
            "the in-memory copy is newer than the disk one by the time an evaluation re-declares it"
        );
    }

    #[tokio::test]
    async fn only_a_first_declaration_or_a_new_default_pushes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{ "theme": "latte" }"#).unwrap();
        let path = path.to_string_lossy().into_owned();
        let (controller, mut rx) = controller();

        controller.open(&path, &json!({ "theme": "mocha" }));
        assert!(rx.try_recv().is_ok(), "a first declaration pushes even when the file already holds every default");
        controller.open(&path, &json!({ "theme": "mocha" }));
        assert!(rx.try_recv().is_err(), "re-declaring on each evaluation pushes nothing");
        controller.open(&path, &json!({ "dnd": true }));
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn a_write_takes_a_table_and_a_nil_deletes_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json").to_string_lossy().into_owned();
        let (controller, _rx) = controller();
        controller.open(&path, &json!({}));

        controller.set(&path, "wallpaper", json!({ "path": "/w/1.jpg", "fit": "cover" }));
        assert_eq!(controller.snapshot().files[&path]["wallpaper"]["fit"], json!("cover"));

        controller.set(&path, "wallpaper", serde_json::Value::Null);
        assert_eq!(controller.snapshot().files[&path], json!({}));
    }

    #[tokio::test]
    async fn a_write_that_changes_nothing_pushes_nothing_and_still_saves() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json").to_string_lossy().into_owned();
        // Read an existing file with no defaults to merge, so `open` schedules no save of its own
        // and only the writes below can recreate the file removed here.
        std::fs::write(&path, r#"{"fit":"cover"}"#).unwrap();
        let (controller, mut rx) = controller();
        controller.open(&path, &json!({}));
        while rx.try_recv().is_ok() {}
        std::fs::remove_file(&path).unwrap();

        controller.set(&path, "fit", json!("cover"));
        controller.set(&path, "never-stored", serde_json::Value::Null);
        assert!(rx.try_recv().is_err(), "a write that edits nothing must not push the whole store");

        tokio::time::sleep(SAVE_DEBOUNCE + Duration::from_millis(150)).await;
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("an unchanged write still repairs the file"))
                .unwrap();
        assert_eq!(written["fit"], json!("cover"));

        // After our own rename in the watch queue, so exactly one push proves that one pushed nothing.
        std::fs::write(&path, r#"{"fit":"fill"}"#).unwrap();
        pushed(&mut rx).await;
        assert_eq!(controller.snapshot().files[&path]["fit"], json!("fill"));
        assert!(rx.try_recv().is_err(), "the watch saw our rename and found nothing new");

        controller.set(&path, "fit", json!("contain"));
        assert!(rx.try_recv().is_ok(), "a real edit still pushes");
    }

    #[tokio::test]
    async fn a_write_to_a_file_no_config_declared_is_refused() {
        let (controller, _rx) = controller();

        controller.set("/tmp/never-opened.json", "key", json!(1));

        assert!(controller.snapshot().files.is_empty());
    }

    #[tokio::test]
    async fn a_relative_path_is_refused_rather_than_resolved_against_the_working_directory() {
        let (controller, _rx) = controller();

        controller.open("settings.json", &json!({ "theme": "mocha" }));

        assert!(controller.snapshot().files.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn the_last_write_of_a_burst_is_the_one_that_reaches_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested").join("state.json");
        let path = file.to_string_lossy().into_owned();
        let (controller, _rx) = controller();
        controller.open(&path, &json!({}));

        for value in 1..=5 {
            controller.set(&path, "scroll", json!(value));
        }
        assert!(!file.exists(), "nothing is written while the writes are still coming");

        tokio::time::sleep(SAVE_DEBOUNCE * 2).await;
        tokio::task::yield_now().await;

        let written: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(written, json!({ "scroll": 5 }));
    }

    #[tokio::test(start_paused = true)]
    async fn saving_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json").to_string_lossy().into_owned();
        let (controller, _rx) = controller();

        controller.open(&path, &json!({ "a": 1 }));
        tokio::time::sleep(SAVE_DEBOUNCE * 2).await;
        tokio::task::yield_now().await;

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "state.json")
            .collect();
        assert!(leftovers.is_empty(), "the rename is the write; nothing else may survive it: {leftovers:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn what_was_saved_reads_back_through_a_fresh_controller() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json").to_string_lossy().into_owned();
        let (first, _rx) = controller();
        first.open(&path, &json!({}));
        first.set(&path, "theme", json!("latte"));
        tokio::time::sleep(SAVE_DEBOUNCE * 2).await;
        tokio::task::yield_now().await;

        let (second, _rx) = controller();
        second.open(&path, &json!({ "theme": "mocha" }));

        assert_eq!(second.snapshot().files[&path]["theme"], json!("latte"), "the round trip is the contract");
    }

    /// Waits for a push, or fails after the inotify round trip should long have landed.
    async fn pushed(rx: &mut tokio::sync::mpsc::UnboundedReceiver<StorageSignal>) {
        tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("no push arrived");
    }

    #[tokio::test]
    async fn a_second_shell_writing_after_the_first_saved_keeps_both_keys() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state.json");
        let path = file.to_string_lossy().into_owned();
        let ((first, _a), (second, mut b)) = (controller(), controller());
        first.open(&path, &json!({}));
        second.open(&path, &json!({}));
        while b.try_recv().is_ok() {}

        first.set(&path, "wallpaper", json!("/w/1.jpg"));
        pushed(&mut b).await;
        second.set(&path, "theme", json!("latte"));
        tokio::time::sleep(SAVE_DEBOUNCE + Duration::from_millis(300)).await;

        let written: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(written, json!({ "wallpaper": "/w/1.jpg", "theme": "latte" }));
    }

    #[tokio::test]
    async fn an_external_write_is_pushed_whether_renamed_or_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state.json");
        let path = file.to_string_lossy().into_owned();
        std::fs::write(&file, r#"{"theme":"mocha"}"#).unwrap();
        let (controller, mut rx) = controller();
        controller.open(&path, &json!({ "dnd": false }));
        pushed(&mut rx).await;

        let renamed = dir.path().join("sedXYZ");
        std::fs::write(&renamed, r#"{"theme":"latte"}"#).unwrap();
        std::fs::rename(&renamed, &file).unwrap();
        pushed(&mut rx).await;
        assert_eq!(controller.snapshot().files[&path], json!({ "theme": "latte", "dnd": false }), "defaults stay");

        std::fs::write(&file, r#"{"theme":"frappe"}"#).unwrap();
        pushed(&mut rx).await;
        assert_eq!(controller.snapshot().files[&path]["theme"], json!("frappe"));
    }

    #[tokio::test]
    async fn a_fixed_malformed_file_wins_whole_over_writes_made_while_it_was_broken() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state.json");
        std::fs::write(&file, "{oops").unwrap();
        let path = file.to_string_lossy().into_owned();
        let (controller, mut rx) = controller();
        controller.open(&path, &json!({}));
        controller.set(&path, "theme", json!("latte"));
        tokio::time::sleep(SAVE_DEBOUNCE + Duration::from_millis(150)).await;
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{oops", "a hand edit gone wrong is the owner's to fix");
        while rx.try_recv().is_ok() {}

        let fixed = r#"{"wallpaper":"/w/1.jpg"}"#;
        std::fs::write(&file, fixed).unwrap();
        pushed(&mut rx).await;
        assert_eq!(controller.snapshot().files[&path], json!({ "wallpaper": "/w/1.jpg" }), "the newer edit wins");
        tokio::time::sleep(SAVE_DEBOUNCE + Duration::from_millis(150)).await;
        assert_eq!(std::fs::read_to_string(&file).unwrap(), fixed, "nothing rewrites a file the shell agrees with");
    }

    #[test]
    fn a_file_fixed_before_a_save_reads_it_logs_the_next_break_again() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state.json");
        let stores = Mutex::new(Stores::default());
        stores.lock().unwrap().files.insert(file.to_string_lossy().into_owned(), Store::default());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let error = || stores.lock().unwrap().files.values().next().unwrap().error.clone();

        std::fs::write(&file, "{oops").unwrap();
        sync(&stores, &file, false, &tx);
        assert!(error().is_some());
        std::fs::write(&file, "{}").unwrap();
        sync(&stores, &file, true, &tx);
        assert_eq!(error(), None, "a save that finds the file fixed must re-arm the log for the same breakage");
    }
}
