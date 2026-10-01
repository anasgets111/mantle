//! Disk and partition capacity telemetry.

pub use shared::state::sysinfo::{DiskDevice, DiskPartition};

#[derive(serde::Deserialize)]
struct LsblkResponse {
    #[serde(default)]
    blockdevices: Vec<LsblkDevice>,
}

#[derive(serde::Deserialize)]
struct LsblkDevice {
    name: Option<String>,
    #[serde(rename = "type")]
    device_type: Option<String>,
    #[serde(default)]
    mountpoints: Vec<Option<String>>,
    fsused: Option<serde_json::Value>,
    fssize: Option<serde_json::Value>,
    #[serde(default)]
    children: Vec<LsblkDevice>,
}

fn parse_bytes(value: Option<&serde_json::Value>) -> u64 {
    match value {
        Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(serde_json::Value::String(s)) => s.parse::<u64>().unwrap_or(0),
        _ => 0,
    }
}

fn collect_partitions(
    node: &LsblkDevice,
    partitions: &mut Vec<DiskPartition>,
    disk_used: &mut u64,
    disk_total: &mut u64,
) {
    let used_bytes = parse_bytes(node.fsused.as_ref());
    let total_bytes = parse_bytes(node.fssize.as_ref());

    let points: Vec<&str> =
        node.mountpoints.iter().filter_map(|opt| opt.as_deref()).filter(|s| !s.is_empty() && *s != "[SWAP]").collect();

    if !points.is_empty() && total_bytes > 0 {
        *disk_used = disk_used.saturating_add(used_bytes);
        *disk_total = disk_total.saturating_add(total_bytes);
        let percent = ((used_bytes as u128 * 100) / total_bytes as u128).min(100) as u8;
        for mp in points {
            partitions.push(DiskPartition { mount_point: mp.to_string(), used_bytes, total_bytes, percent });
        }
    }

    for child in &node.children {
        collect_partitions(child, partitions, disk_used, disk_total);
    }
}

/// Parses `lsblk --json --bytes` output into physical disk records.
pub fn parse_lsblk(json_text: &str) -> Vec<DiskDevice> {
    let Ok(resp) = serde_json::from_str::<LsblkResponse>(json_text) else {
        return Vec::new();
    };

    let mut disks = Vec::new();
    for dev in resp.blockdevices {
        let Some(name) = dev.name.as_deref() else { continue };
        let dev_type = dev.device_type.as_deref().unwrap_or("");
        if dev_type != "disk" || name.starts_with("zram") || name.starts_with("loop") {
            continue;
        }

        // ponytail: a filesystem spanning multiple block devices can repeat its capacity here.
        // Use filesystem UUIDs if multi-device pools need an exact per-disk total.
        let mut partitions = Vec::new();
        let mut disk_used: u64 = 0;
        let mut disk_total: u64 = 0;
        collect_partitions(&dev, &mut partitions, &mut disk_used, &mut disk_total);
        if partitions.is_empty() {
            continue;
        }

        let percent = if disk_total > 0 { ((disk_used as u128 * 100) / disk_total as u128).min(100) as u8 } else { 0 };

        disks.push(DiskDevice {
            name: name.to_string(),
            used_bytes: disk_used,
            total_bytes: disk_total,
            percent,
            partitions,
        });
    }

    disks
}

/// Reads storage topology by invoking `lsblk`.
pub async fn read_disks() -> Vec<DiskDevice> {
    let output = tokio::process::Command::new("lsblk")
        .args(["--json", "--bytes", "--output", "NAME,TYPE,MOUNTPOINTS,FSUSED,FSSIZE"])
        .kill_on_drop(true)
        .output();
    let Ok(Ok(output)) = tokio::time::timeout(std::time::Duration::from_secs(3), output).await else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_lsblk(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_LSBLK: &str = r#"{"blockdevices":[
        {"name":"zram0","type":"disk","mountpoints":["[SWAP]"]},
        {"name":"nvme0n1","type":"disk","children":[
            {"name":"nvme0n1p1","type":"part","mountpoints":["/boot"],"fsused":60000000,"fssize":1000000000},
            {"name":"nvme0n1p2","type":"part","mountpoints":["/home","/"],"fsused":50000000000,"fssize":100000000000}
        ]}
    ]}"#;

    #[test]
    fn parse_lsblk_parses_disks_and_ignores_zram() {
        let disks = parse_lsblk(SAMPLE_LSBLK);
        assert_eq!(disks.len(), 1);
        let nvme = &disks[0];
        assert_eq!(nvme.name, "nvme0n1");
        assert_eq!(nvme.partitions.len(), 3);
        assert_eq!(nvme.partitions[0].mount_point, "/boot");
        assert_eq!(nvme.partitions[0].percent, 6);
        assert_eq!(nvme.partitions[1].mount_point, "/home");
        assert_eq!(nvme.partitions[1].percent, 50);
        assert_eq!(nvme.partitions[2].mount_point, "/");
        assert_eq!(nvme.partitions[2].percent, 50);

        // Mountpoints on one filesystem share its capacity.
        assert_eq!(nvme.used_bytes, 50_060_000_000);
        assert_eq!(nvme.total_bytes, 101_000_000_000);
        assert_eq!(nvme.percent, 49);
    }

    #[test]
    fn equal_capacity_partitions_remain_distinct() {
        let json = r#"{"blockdevices":[{"name":"sda","type":"disk","children":[
            {"name":"sda1","mountpoints":["/a"],"fsused":50,"fssize":100},
            {"name":"sda2","mountpoints":["/b"],"fsused":50,"fssize":100}
        ]}]}"#;
        let disks = parse_lsblk(json);
        assert_eq!(disks[0].used_bytes, 100);
        assert_eq!(disks[0].total_bytes, 200);
    }

    #[test]
    fn parse_lsblk_handles_malformed_json_gracefully() {
        assert_eq!(parse_lsblk("not json"), Vec::new());
        assert_eq!(parse_lsblk("{}"), Vec::new());
    }
}
