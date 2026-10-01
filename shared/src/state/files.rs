//! `mantle.files` snapshot payload.

use serde::Serialize;
use std::collections::BTreeMap;

/// `mantle.files` payload (ADR-0120).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FilesState {
    /// One entry per `"watch"`, keyed by its `path` minus trailing slashes; `nil` until watched.
    pub folders: BTreeMap<String, Folder>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Folder {
    /// `false` until the first listing lands, then `true` even when empty or failed.
    pub ready: bool,
    /// Files (and symlinks to files) directly inside, minus dotfiles, filtered by extension and
    /// sorted case-insensitively by name. Relisted 200 ms after the last change.
    pub entries: Vec<FileEntry>,
    /// Why listing failed, e.g. `"No such file or directory (os error 2)"`; `nil` on success. A
    /// missing or deleted folder is not watched for reappearing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FileEntry {
    /// File name, e.g. `"sunrise.jpg"`.
    pub name: String,
    /// Absolute path.
    pub path: String,
    /// Modification time in Unix seconds; `0` when unavailable.
    pub modified: i64,
}
