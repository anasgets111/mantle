//! `temp_cores`/`temp_gpu` source `/sys/class/hwmon/`, resolving chips by name preference
//! (ADR-0035).

use std::path::{Path, PathBuf};

use super::super::read_attr;

/// Converts milli-Celsius to whole Celsius, rounding half away from zero. Plain division makes
/// `-500 / 1000 == 0`, silently reporting `0°C` instead of `-1°C`.
fn round_milli_c(milli_c: i64) -> i64 {
    if milli_c >= 0 { (milli_c + 500) / 1000 } else { (milli_c - 500) / 1000 }
}

/// Resolves the first `hwmon_root` chip whose `name` matches `preference`, trying that list in
/// order (ADR-0035); preference order beats directory order.
pub fn resolve_chip(hwmon_root: &Path, preference: &[&str]) -> Option<PathBuf> {
    let entries: Vec<PathBuf> =
        std::fs::read_dir(hwmon_root).ok()?.filter_map(|entry| entry.ok().map(|entry| entry.path())).collect();
    preference.iter().find_map(|wanted| {
        entries.iter().find_map(|dir| (read_attr(dir, "name").as_deref() == Some(*wanted)).then(|| dir.clone()))
    })
}

/// `tempN_input` paths whose paired `tempN_label` matches `Core \d+` (coretemp) or `Tccd\d+`
/// (k10temp), sorted by that index. Excludes package aggregates (`Package id N`, `Tctl`) and
/// unlabeled sensors.
fn core_inputs(chip_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(chip_dir) else {
        return Vec::new();
    };

    let mut cores: Vec<(u32, PathBuf)> = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else { continue };
        let Some(rest) = file_name.strip_suffix("_input") else { continue };
        let Some(label) = read_attr(chip_dir, &format!("{rest}_label")) else { continue };
        // `^(Core |Tccd)(\d+)$` without the regex. The digit check is what keeps `parse` from
        // accepting the leading `+` that `\d+` rejects; an empty remainder fails `parse` on its own.
        let Some(core_index) = label
            .strip_prefix("Core ")
            .or_else(|| label.strip_prefix("Tccd"))
            .filter(|index| index.chars().all(|c| c.is_ascii_digit()))
            .and_then(|index| index.parse::<u32>().ok())
        else {
            continue;
        };
        cores.push((core_index, entry.path()));
    }
    cores.sort_by_key(|(core_index, _)| *core_index);
    cores.into_iter().map(|(_, input)| input).collect()
}

/// The lowest-numbered `tempN_input`, regardless of label, for `acpitz`, `Tctl` and `temp_gpu`.
fn primary_input(chip_dir: &Path) -> Option<PathBuf> {
    let lowest = std::fs::read_dir(chip_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name().to_str().and_then(|n| n.strip_suffix("_input")?.strip_prefix("temp")?.parse::<u32>().ok())
        })
        .min()?;
    Some(chip_dir.join(format!("temp{lowest}_input")))
}

/// One `tempN_input` in whole Celsius.
fn read_celsius(input: &Path) -> Option<i64> {
    std::fs::read_to_string(input).ok()?.trim().parse::<i64>().ok().map(round_milli_c)
}

/// Controller-construction preference for `temp_cores` (ADR-0035).
const CPU_TEMP_PREFERENCE: &[&str] = &["k10temp", "coretemp"];
/// Generic ACPI thermal-zone fallback when neither `k10temp` nor `coretemp` exists; every machine
/// has this, and its one sensor becomes a one-element array.
const GENERIC_TEMP_FALLBACK: &str = "acpitz";
/// Controller-construction preference for `temp_gpu` (ADR-0035, ADR-0282).
const GPU_TEMP_PREFERENCE: &[&str] = &["amdgpu", "nouveau", "nvidia", "i915", "xe"];

/// `temp_cores` inputs: a CPU chip's per-core sensors, else its primary one, else `acpitz`'s
/// (ADR-0035). Resolved at startup and again by [`sample_temp_cores`] after a failed read.
pub fn resolve_temp_cores_inputs(hwmon_root: &Path) -> Vec<PathBuf> {
    if let Some(chip_dir) = resolve_chip(hwmon_root, CPU_TEMP_PREFERENCE) {
        let cores = core_inputs(&chip_dir);
        // A monolithic Ryzen APU exposes only `Tctl`; one aggregate beats an empty list.
        return if cores.is_empty() { primary_input(&chip_dir).into_iter().collect() } else { cores };
    }
    resolve_chip(hwmon_root, &[GENERIC_TEMP_FALLBACK])
        .and_then(|chip_dir| primary_input(&chip_dir))
        .into_iter()
        .collect()
}

/// `temp_cores` from resolved inputs; an unreadable one is skipped.
pub fn read_temp_cores(inputs: &[PathBuf]) -> Vec<i64> {
    inputs.iter().filter_map(|input| read_celsius(input)).collect()
}

/// [`read_temp_cores`], resolving again when no input is known or one failed: a driver reload
/// renumbers `hwmonN`, and a late-loading chip appears after startup.
pub fn sample_temp_cores(inputs: &mut Vec<PathBuf>, hwmon_root: &Path) -> Vec<i64> {
    let temps = read_temp_cores(inputs);
    if !inputs.is_empty() && temps.len() == inputs.len() {
        return temps;
    }
    *inputs = resolve_temp_cores_inputs(hwmon_root);
    read_temp_cores(inputs)
}

/// [`read_temp_gpu`], resolving again when the input is unknown or unreadable.
pub fn sample_temp_gpu(input: &mut Option<PathBuf>, hwmon_root: &Path) -> Option<i64> {
    if let Some(temp) = read_temp_gpu(input.as_deref()) {
        return Some(temp);
    }
    *input = resolve_gpu_input(hwmon_root);
    read_temp_gpu(input.as_deref())
}

/// `temp_gpu`'s input (ADR-0035); [`sample_temp_gpu`] resolves it again after a failed read.
pub fn resolve_gpu_input(hwmon_root: &Path) -> Option<PathBuf> {
    resolve_chip(hwmon_root, GPU_TEMP_PREFERENCE).and_then(|chip_dir| primary_input(&chip_dir))
}

/// `temp_gpu` from its resolved input; `None` without a readable one (ADR-0035).
pub fn read_temp_gpu(input: Option<&Path>) -> Option<i64> {
    input.and_then(read_celsius)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only resolve-and-read convenience; production resolves once and reads per tick.
    fn resolve_and_read_temp_cores(hwmon_root: &Path) -> Vec<i64> {
        read_temp_cores(&resolve_temp_cores_inputs(hwmon_root))
    }

    /// Builds fake `hwmon_root` chips under `dir`, each with only a `name` file, for
    /// [`resolve_chip`] tests.
    fn write_chip(dir: &Path, dir_name: &str, chip_name: &str) {
        let chip_dir = dir.join(dir_name);
        std::fs::create_dir_all(&chip_dir).unwrap();
        std::fs::write(chip_dir.join("name"), format!("{chip_name}\n")).unwrap();
    }

    /// Writes one `tempN_input`/`tempN_label` pair in milli-Celsius, the real hwmon convention;
    /// values came from this machine's `coretemp`.
    fn write_sensor(chip_dir: &Path, n: u32, label: &str, milli_c: i64) {
        std::fs::write(chip_dir.join(format!("temp{n}_input")), format!("{milli_c}\n")).unwrap();
        std::fs::write(chip_dir.join(format!("temp{n}_label")), format!("{label}\n")).unwrap();
    }

    #[test]
    fn sampling_finds_a_chip_that_loads_late_and_follows_a_renumbered_one() {
        let root = tempfile::tempdir().unwrap();
        let (mut gpu, mut cores) = (None, Vec::new());
        assert_eq!(sample_temp_gpu(&mut gpu, root.path()), None);

        write_chip(root.path(), "hwmon3", "amdgpu");
        write_sensor(&root.path().join("hwmon3"), 1, "edge", 51_000);
        write_chip(root.path(), "hwmon1", "coretemp");
        write_sensor(&root.path().join("hwmon1"), 2, "Core 0", 40_000);
        assert_eq!(sample_temp_gpu(&mut gpu, root.path()), Some(51));
        assert_eq!(sample_temp_cores(&mut cores, root.path()), [40]);

        std::fs::rename(root.path().join("hwmon3"), root.path().join("hwmon4")).unwrap();
        std::fs::rename(root.path().join("hwmon1"), root.path().join("hwmon2")).unwrap();
        assert_eq!(sample_temp_gpu(&mut gpu, root.path()), Some(51));
        assert_eq!(sample_temp_cores(&mut cores, root.path()), [40]);
        assert_eq!(gpu, Some(root.path().join("hwmon4/temp1_input")));
        assert_eq!(cores, [root.path().join("hwmon2/temp2_input")]);
    }

    #[test]
    fn core_inputs_extract_and_sort_by_core_index_not_filename_or_insertion_order() {
        let dir = tempfile::tempdir().unwrap();
        let chip_dir = dir.path().join("hwmon6");
        std::fs::create_dir_all(&chip_dir).unwrap();
        // Captured values are deliberately misordered: temp30/Core 28 and temp6/Core 4 prove
        // sorting uses core index, not filename.
        write_sensor(&chip_dir, 1, "Package id 0", 92000); // excluded: not a per-core sensor
        write_sensor(&chip_dir, 30, "Core 28", 65000);
        write_sensor(&chip_dir, 2, "Core 0", 57000);
        write_sensor(&chip_dir, 6, "Core 4", 78000);

        assert_eq!(read_temp_cores(&core_inputs(&chip_dir)), vec![57, 78, 65]);

        let acpitz = dir.path().join("hwmon1");
        std::fs::create_dir_all(&acpitz).unwrap();
        write_sensor(&acpitz, 1, "", 92000); // unlabeled single zone
        assert_eq!(core_inputs(&acpitz), Vec::<PathBuf>::new());
    }

    #[test]
    fn read_temp_cores_reads_k10temp_ccds_and_falls_back_to_tctl_without_them() {
        let dir = tempfile::tempdir().unwrap();
        write_chip(dir.path(), "hwmon2", "k10temp");
        let chip_dir = dir.path().join("hwmon2");
        write_sensor(&chip_dir, 1, "Tctl", 71000); // excluded: control aggregate
        write_sensor(&chip_dir, 4, "Tccd2", 63000);
        write_sensor(&chip_dir, 3, "Tccd1", 66000);
        assert_eq!(resolve_and_read_temp_cores(dir.path()), vec![66, 63]);

        std::fs::remove_file(chip_dir.join("temp3_label")).unwrap();
        std::fs::remove_file(chip_dir.join("temp3_input")).unwrap();
        std::fs::remove_file(chip_dir.join("temp4_label")).unwrap();
        std::fs::remove_file(chip_dir.join("temp4_input")).unwrap();
        assert_eq!(resolve_and_read_temp_cores(dir.path()), vec![71]);
    }

    #[test]
    fn primary_input_is_the_lowest_numbered_input_regardless_of_label_or_write_order() {
        let dir = tempfile::tempdir().unwrap();
        let chip = |name: &str| {
            let chip_dir = dir.path().join(name);
            std::fs::create_dir_all(&chip_dir).unwrap();
            chip_dir
        };
        // acpitz-shaped: one unlabeled sensor.
        let acpitz = chip("hwmon1");
        write_sensor(&acpitz, 1, "", 92000);
        assert_eq!(primary_input(&acpitz), Some(acpitz.join("temp1_input")));
        // nvme-shaped, written out of numeric order: the lowest is temp1, 35850 milli-C -> 36.
        let nvme = chip("hwmon3");
        write_sensor(&nvme, 3, "Sensor 2", 39850);
        write_sensor(&nvme, 1, "Composite", 35850);
        write_sensor(&nvme, 2, "Sensor 1", 35850);
        assert_eq!(primary_input(&nvme).as_deref().and_then(read_celsius), Some(36));
        assert_eq!(primary_input(&chip("hwmon4")), None, "a chip with no sensors");
    }

    #[test]
    fn round_milli_c_rounds_to_the_nearest_whole_degree_including_negative_readings() {
        assert_eq!(round_milli_c(57000), 57);
        assert_eq!(round_milli_c(35850), 36); // .85 rounds up
        assert_eq!(round_milli_c(35499), 35); // .499 rounds down
        assert_eq!(round_milli_c(0), 0);
        assert_eq!(round_milli_c(-500), -1); // -0.5degC rounds away from zero, not toward 0degC
        assert_eq!(round_milli_c(-499), 0);
    }

    #[test]
    fn read_temp_cores_prefers_coretemp_then_acpitz_and_is_empty_without_either() {
        let read = |chips: &[(&str, &str, &str, i64)]| {
            let dir = tempfile::tempdir().unwrap();
            for (dir_name, chip, label, milli_c) in chips {
                write_chip(dir.path(), dir_name, chip);
                write_sensor(&dir.path().join(dir_name), 2, label, *milli_c);
            }
            resolve_and_read_temp_cores(dir.path())
        };
        let acpitz = ("hwmon1", "acpitz", "", 92000);
        assert_eq!(read(&[acpitz, ("hwmon6", "coretemp", "Core 0", 57000)]), vec![57]);
        assert_eq!(read(&[acpitz]), vec![92], "acpitz is a one-element fallback");
        assert_eq!(read(&[("hwmon7", "mt7921_phy0", "", 1000)]), Vec::<i64>::new());
    }

    #[test]
    fn read_temp_gpu_reads_the_matching_chips_primary_sensor() {
        // (chip name, label, milli-C, expected): this machine has integrated graphics only, so coretemp is no GPU.
        for (chip, label, milli_c, expected) in
            [("amdgpu", "edge", 45000, Some(45)), ("xe", "", 52000, Some(52)), ("coretemp", "", 60000, None)]
        {
            let dir = tempfile::tempdir().unwrap();
            write_chip(dir.path(), "hwmon2", chip);
            write_sensor(&dir.path().join("hwmon2"), 1, label, milli_c);
            assert_eq!(read_temp_gpu(resolve_gpu_input(dir.path()).as_deref()), expected, "{chip}");
        }
    }

    #[test]
    fn resolved_inputs_are_reread_each_tick_without_rescanning_the_chip() {
        let dir = tempfile::tempdir().unwrap();
        write_chip(dir.path(), "hwmon6", "coretemp");
        let chip_dir = dir.path().join("hwmon6");
        write_sensor(&chip_dir, 2, "Core 0", 57000);
        let inputs = resolve_temp_cores_inputs(dir.path());

        write_sensor(&chip_dir, 2, "Core 0", 61000);
        write_sensor(&chip_dir, 3, "Core 1", 70000); // appears after resolution: never read
        assert_eq!(read_temp_cores(&inputs), vec![61]);
    }

    #[test]
    fn resolve_chip_follows_the_preference_list_not_directory_order() {
        let dir = tempfile::tempdir().unwrap();
        write_chip(dir.path(), "hwmon0", "coretemp");
        write_chip(dir.path(), "hwmon1", "k10temp");
        write_chip(dir.path(), "hwmon5", "acpitz");
        write_chip(dir.path(), "hwmon7", "mt7921_phy0");

        // hwmon0 is lexically earlier but lower preference.
        assert_eq!(resolve_chip(dir.path(), &["k10temp", "coretemp"]), Some(dir.path().join("hwmon1")));
        assert_eq!(resolve_chip(dir.path(), &["acpitz"]), Some(dir.path().join("hwmon5")));
        assert_eq!(resolve_chip(dir.path(), &["amdgpu", "nouveau", "nvidia"]), None);
    }
}
