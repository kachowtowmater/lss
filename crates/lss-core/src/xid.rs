//! Kernel NVRM Xid lines from `journalctl -k -o short-iso`.

use crate::gpu::{parse_pci, PciKey};
use crate::timeutil::parse_iso_loose;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct XidEvent {
    pub ts: i64,
    /// PCI address as the kernel printed it, e.g. `0000:f1:00`.
    pub pci: String,
    pub xid: u32,
    /// GPU index, once resolved against the nvidia-smi PCI map.
    pub gpu: Option<u32>,
    /// The rest of the line after the Xid number (pid, name, channel …).
    pub detail: String,
}

impl XidEvent {
    pub fn gpu_label(&self) -> String {
        match self.gpu {
            Some(i) => format!("GPU{i}"),
            None => format!("PCI {}", self.pci),
        }
    }
}

/// Returns None for any line that is not an Xid report.
pub fn parse_xid_line(line: &str, pci_map: &[(PciKey, u32)]) -> Option<XidEvent> {
    const MARK: &str = "NVRM: Xid (PCI:";
    let at = line.find(MARK)?;
    let ts = parse_iso_loose(line.split_whitespace().next()?)?;
    let after = &line[at + MARK.len()..];
    let close = after.find(')')?;
    let pci = after[..close].trim().to_string();
    let rest = after[close + 1..].trim_start_matches(':').trim();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let xid: u32 = digits.parse().ok()?;
    let detail = rest[digits.len()..].trim_start_matches(',').trim().to_string();
    let gpu = parse_pci(&pci).and_then(|k| pci_map.iter().find(|(p, _)| *p == k).map(|(_, i)| *i));
    Some(XidEvent { ts, pci, xid, gpu, detail })
}

/// Short operator hint for the Xids this box has actually produced, plus the classic ones.
pub fn xid_hint(xid: u32) -> &'static str {
    match xid {
        8 => "GPU stopped processing (hang / watchdog)",
        13 => "graphics engine exception",
        31 => "GPU memory page fault",
        43 => "GPU stopped processing",
        45 => "preemptive cleanup, channel torn down",
        48 => "double-bit ECC error",
        61 | 62 => "internal micro-controller halt",
        63 | 64 => "ECC page retirement / row remap",
        74 => "NVLink error",
        79 => "GPU has fallen off the bus",
        94 | 95 => "contained / uncontained ECC error",
        119 | 120 => "GSP RPC timeout / GSP error",
        154 => "GPU recovery action changed (reset / reboot required)",
        _ => "see NVIDIA Xid catalogue",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::parse_pci_map;

    const JOURNAL: &str = include_str!("../../../fixtures/journal_xid.txt");

    #[test]
    fn real_format_line() {
        let map = parse_pci_map(include_str!("../../../fixtures/nvidia_pci.csv"));
        let line = "2026-09-18T21:22:57-07:00 gpu-box kernel: NVRM: Xid (PCI:0000:f1:00): 8, pid=3177412, name=python3, channel 0x00000004";
        let e = parse_xid_line(line, &map).unwrap();
        assert_eq!(e.ts, 1_789_791_777);
        assert_eq!(e.pci, "0000:f1:00");
        assert_eq!(e.xid, 8);
        assert_eq!(e.gpu, Some(3));
        assert_eq!(e.detail, "pid=3177412, name=python3, channel 0x00000004");
        assert_eq!(e.gpu_label(), "GPU3");
    }

    #[test]
    fn journal_fixture_only_yields_xid_lines() {
        let map = parse_pci_map(include_str!("../../../fixtures/nvidia_pci.csv"));
        let events: Vec<XidEvent> = JOURNAL.lines().filter_map(|l| parse_xid_line(l, &map)).collect();
        let xids: Vec<(u32, Option<u32>)> = events.iter().map(|e| (e.xid, e.gpu)).collect();
        assert_eq!(xids, vec![(8, Some(1)), (8, Some(3)), (79, Some(0)), (119, Some(2)), (31, None)]);
        // Xid 79 has no pid= tail; 31 is on a PCI address that is not one of ours
        assert_eq!(events[2].detail, "GPU has fallen off the bus.");
        assert_eq!(events[4].gpu_label(), "PCI 0000:99:00");
    }

    #[test]
    fn rejects_non_xid_and_garbage() {
        assert!(parse_xid_line("2026-09-18T21:22:57-07:00 gpu-box kernel: NVRM: GPU at PCI:0000:f1:00", &[]).is_none());
        assert!(parse_xid_line("NVRM: Xid (PCI:0000:f1:00): notanumber", &[]).is_none());
        assert!(parse_xid_line("", &[]).is_none());
    }
}
