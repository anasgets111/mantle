//! Network throughput telemetry from `/proc/net/dev`.

use std::path::Path;

/// Transfer counters by interface, so hotplugged links cannot add lifetime traffic to one tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetSample {
    pub interfaces: std::collections::HashMap<String, (u64, u64)>,
}

/// Parses `/proc/net/dev` counters for each non-loopback interface.
pub fn parse_net_dev(content: &str) -> NetSample {
    // ponytail: bridge, tunnel and physical counters can count the same packet. Keep all
    // non-loopback interfaces until the API can identify the caller's preferred link.
    let mut interfaces = std::collections::HashMap::new();

    for line in content.lines() {
        let Some((iface, stats)) = line.split_once(':') else {
            continue;
        };
        let iface = iface.trim();
        if iface == "lo" || iface.is_empty() {
            continue;
        }

        let mut cols = stats.split_whitespace();
        let Some(rx) = cols.next().and_then(|s| s.parse::<u64>().ok()) else { continue };
        let Some(tx) = cols.nth(7).and_then(|s| s.parse::<u64>().ok()) else { continue };

        interfaces.insert(iface.to_string(), (rx, tx));
    }

    NetSample { interfaces }
}

/// Calculates per-second transfer rates from two timestamped samples.
pub fn delta_rate(prev: &NetSample, current: &NetSample, elapsed_secs: f64) -> (u64, u64) {
    if elapsed_secs <= 0.0 {
        return (0, 0);
    }
    let (rx, tx) = current.interfaces.iter().fold((0_u64, 0_u64), |(rx, tx), (name, &(new_rx, new_tx))| {
        let Some(&(old_rx, old_tx)) = prev.interfaces.get(name) else { return (rx, tx) };
        (rx.saturating_add(new_rx.saturating_sub(old_rx)), tx.saturating_add(new_tx.saturating_sub(old_tx)))
    });
    let rx = (rx as f64 / elapsed_secs).round() as u64;
    let tx = (tx as f64 / elapsed_secs).round() as u64;
    (rx, tx)
}

/// Reads `{proc_root}/net/dev`.
pub fn read_sample(proc_root: &Path) -> std::io::Result<NetSample> {
    let content = std::fs::read_to_string(proc_root.join("net/dev"))?;
    Ok(parse_net_dev(&content))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_NET_DEV: &str = r#"Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 2492817    7529    0    0    0     0          0         0  2492817    7529    0    0    0     0       0          0
  wlo1: 181566000  216473    0    0    0     0          0         0 105602000  184635    0    5    0     0       0          0
  eth0:    100000     100    0    0    0     0          0         0    200000     200    0    0    0     0       0          0
"#;

    #[test]
    fn parse_net_dev_keeps_interfaces_and_skips_lo() {
        let sample = parse_net_dev(SAMPLE_NET_DEV);
        assert_eq!(sample.interfaces["wlo1"].0, 181_566_000);
        assert_eq!(sample.interfaces["eth0"].1, 200_000);
    }

    #[test]
    fn delta_rate_computes_per_second_throughput() {
        let prev = NetSample { interfaces: [("eth0".into(), (1_000_000, 500_000))].into() };
        let curr = NetSample { interfaces: [("eth0".into(), (2_500_000, 700_000))].into() };
        let (rx, tx) = delta_rate(&prev, &curr, 1.0);
        assert_eq!(rx, 1_500_000);
        assert_eq!(tx, 200_000);

        let (rx_half, tx_half) = delta_rate(&prev, &curr, 2.0);
        assert_eq!(rx_half, 750_000);
        assert_eq!(tx_half, 100_000);
    }

    #[test]
    fn nonpositive_elapsed_time_has_no_rate() {
        let prev = NetSample { interfaces: [("eth0".into(), (100, 100))].into() };
        let current = NetSample { interfaces: [("eth0".into(), (200, 300))].into() };
        assert_eq!(delta_rate(&prev, &current, 0.0), (0, 0));
        assert_eq!(delta_rate(&prev, &current, -1.0), (0, 0));
    }

    #[test]
    fn new_interface_does_not_add_lifetime_bytes_to_rate() {
        let prev = NetSample { interfaces: [("eth0".into(), (100, 100))].into() };
        let current =
            NetSample { interfaces: [("eth0".into(), (150, 120)), ("wlan0".into(), (1_000_000, 2_000_000))].into() };
        assert_eq!(delta_rate(&prev, &current, 1.0), (50, 20));
    }

    #[test]
    fn read_sample_reads_from_tempdir_file() {
        let dir = tempfile::tempdir().unwrap();
        let net_dir = dir.path().join("net");
        std::fs::create_dir_all(&net_dir).unwrap();
        std::fs::write(net_dir.join("dev"), SAMPLE_NET_DEV).unwrap();

        let sample = read_sample(dir.path()).unwrap();
        assert_eq!(sample.interfaces["wlo1"].0, 181_566_000);
        assert_eq!(sample.interfaces["eth0"].1, 200_000);
    }
}
