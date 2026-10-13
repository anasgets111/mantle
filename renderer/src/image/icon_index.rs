//! One `index.theme`: `Inherits`, `Directories`, and each directory's size keys.

use std::collections::HashMap;

#[derive(Clone, Copy)]
enum DirKind {
    Fixed,
    Scalable,
    Threshold,
}

pub(crate) struct ThemeIndex {
    pub inherits: Vec<String>,
    pub dirs: Vec<IconDir>,
}

#[derive(Clone)]
pub(crate) struct IconDir {
    pub subdir: String,
    size: u16,
    scale: u16,
    min_size: u16,
    max_size: u16,
    threshold: u16,
    kind: DirKind,
}

impl IconDir {
    fn scaled(&self, value: u16) -> u32 {
        u32::from(value).saturating_mul(u32::from(self.scale))
    }

    /// Spec directory distance: `Size * Scale` against the requested device pixels. In range is 0, an exact
    /// match in device pixels: integer `px / Scale` would match 33 as well as 32 in a `Scale=2` size-16 directory.
    /// Outside a threshold window the distance is to that window's edge, not to `MinSize`.
    pub(crate) fn distance(&self, px: u32) -> u32 {
        let size = self.scaled(self.size);
        let min = self.scaled(self.min_size);
        let max = self.scaled(self.max_size);
        match self.kind {
            DirKind::Fixed => size.abs_diff(px),
            DirKind::Scalable if px < min => min.saturating_sub(px),
            DirKind::Scalable if px > max => px.saturating_sub(max),
            DirKind::Scalable => 0,
            DirKind::Threshold => {
                let threshold = self.scaled(self.threshold);
                let low = size.saturating_sub(threshold);
                let high = size.saturating_add(threshold);
                if px < low { low.saturating_sub(px) } else { px.saturating_sub(high) }
            }
        }
    }
}

pub(crate) fn parse(text: &str) -> ThemeIndex {
    let mut inherits = Vec::new();
    let mut order = Vec::new();
    let mut section = String::new();
    let mut raw = RawDir::default();
    let mut parsed = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            push_dir(&section, &raw, &mut parsed);
            section = name.trim().to_string();
            raw = RawDir::default();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        let value = value.trim();
        if section == "Icon Theme" {
            match key {
                "Inherits" => inherits = comma_list(value),
                "Directories" => order = comma_list(value),
                _ => {}
            }
            continue;
        }
        match key {
            "Size" => raw.size = value.parse().ok(),
            "Scale" => raw.scale = value.parse().ok(),
            "MinSize" => raw.min_size = value.parse().ok(),
            "MaxSize" => raw.max_size = value.parse().ok(),
            "Threshold" => raw.threshold = value.parse().ok(),
            "Type" => {
                raw.kind = Some(match value {
                    "Fixed" => DirKind::Fixed,
                    "Scalable" => DirKind::Scalable,
                    _ => DirKind::Threshold,
                })
            }
            _ => {}
        }
    }
    push_dir(&section, &raw, &mut parsed);

    let by_name: HashMap<String, IconDir> = parsed.into_iter().map(|dir| (dir.subdir.clone(), dir)).collect();
    let dirs = order.into_iter().filter_map(|name| by_name.get(&name).cloned()).collect();
    ThemeIndex { inherits, dirs }
}

fn push_dir(section: &str, raw: &RawDir, parsed: &mut Vec<IconDir>) {
    if section.is_empty() {
        return;
    }
    let Some(size) = raw.size else { return };
    let scale = raw.scale.unwrap_or(1).max(1);
    parsed.push(IconDir {
        subdir: section.to_string(),
        size,
        scale,
        min_size: raw.min_size.unwrap_or(size),
        max_size: raw.max_size.unwrap_or(size),
        threshold: raw.threshold.unwrap_or(2),
        kind: raw.kind.unwrap_or(DirKind::Threshold),
    });
}

fn comma_list(value: &str) -> Vec<String> {
    value.split(',').map(str::trim).filter(|part| !part.is_empty()).map(str::to_string).collect()
}

#[derive(Default)]
struct RawDir {
    size: Option<u16>,
    scale: Option<u16>,
    min_size: Option<u16>,
    max_size: Option<u16>,
    threshold: Option<u16>,
    kind: Option<DirKind>,
}
