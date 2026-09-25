//! `nvidia-smi --query-gpu=... --format=csv,noheader,nounits` parsing and the throttle
//! bitmask (`clocks_throttle_reasons.active`).

use serde::{Deserialize, Serialize};

pub const QUERY_FIELDS: &str = "index,temperature.gpu,power.draw,power.limit,clocks.sm,utilization.gpu,memory.used,memory.total,fan.speed,clocks_throttle_reasons.active";

/// The fast query plus two fields an older driver may not know. The collector tries this one
/// first and falls back to `QUERY_FIELDS` for good when nvidia-smi refuses it, so a driver
/// change can never turn into a false "GPU missing".
pub const QUERY_FIELDS_EXT: &str = "index,temperature.gpu,power.draw,power.limit,clocks.sm,utilization.gpu,memory.used,memory.total,fan.speed,clocks_throttle_reasons.active,utilization.memory,temperature.memory";

/// Slow (60 s) health query: link, ECC, row remapping, page retirement, limits.
pub const HEALTH_QUERY_FIELDS: &str = "index,clocks.max.sm,pstate,pcie.link.gen.current,pcie.link.gen.max,pcie.link.width.current,pcie.link.width.max,ecc.errors.corrected.volatile.total,ecc.errors.uncorrected.volatile.total,remapped_rows.correctable,remapped_rows.uncorrectable,remapped_rows.pending,remapped_rows.failure,retired_pages.single_bit_ecc.count,retired_pages.double_bit.count,retired_pages.pending,power.max_limit,enforced.power.limit";

pub const THROTTLE_SW_POWER_CAP: u64 = 0x4;
pub const THROTTLE_HW_SLOWDOWN: u64 = 0x8;
pub const THROTTLE_SW_THERMAL: u64 = 0x20;
pub const THROTTLE_HW_THERMAL: u64 = 0x40;
pub const THROTTLE_HW_POWER_BRAKE: u64 = 0x80;
/// The bits the thermal alert rule looks at.
pub const THROTTLE_THERMAL_MASK: u64 = THROTTLE_SW_THERMAL | THROTTLE_HW_THERMAL;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GpuSample {
    pub index: u32,
    pub temp_c: Option<f64>,
    pub power_w: Option<f64>,
    pub power_limit_w: Option<f64>,
    pub clock_mhz: Option<f64>,
    pub util_pct: Option<f64>,
    pub mem_used_mib: Option<f64>,
    pub mem_total_mib: Option<f64>,
    pub fan_pct: Option<f64>,
    pub throttle_mask: u64,
    /// memory-controller utilisation (`utilization.memory`)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_util_pct: Option<f64>,
    /// `temperature.memory`; `N/A` on boards without the sensor
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_temp_c: Option<f64>,
}

/// Slow-moving health facts, read once a minute. `None` = `[N/A]` on this board.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GpuHealth {
    pub index: u32,
    pub clock_max_mhz: Option<f64>,
    pub pstate: Option<String>,
    pub pcie_gen: Option<f64>,
    pub pcie_gen_max: Option<f64>,
    pub pcie_width: Option<f64>,
    pub pcie_width_max: Option<f64>,
    pub ecc_corrected: Option<f64>,
    pub ecc_uncorrected: Option<f64>,
    pub remap_correctable: Option<f64>,
    pub remap_uncorrectable: Option<f64>,
    pub remap_pending: Option<bool>,
    pub remap_failure: Option<bool>,
    pub retired_sbe: Option<f64>,
    pub retired_dbe: Option<f64>,
    pub retired_pending: Option<bool>,
    pub power_max_limit_w: Option<f64>,
    pub power_enforced_w: Option<f64>,
    /// when this was read
    pub ts: i64,
}

impl GpuHealth {
    /// The link trained below what the slot and card can do. (An idle GPU may drop its link
    /// GEN to save power, so only the WIDTH is a fault on its own.)
    pub fn pcie_width_degraded(&self) -> bool {
        matches!((self.pcie_width, self.pcie_width_max), (Some(c), Some(m)) if c < m)
    }

    /// Anything here that a human should look at.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        let pos = |v: Option<f64>| v.is_some_and(|x| x > 0.0);
        if pos(self.ecc_uncorrected) {
            out.push(format!("ECC uncorrected {:.0}", self.ecc_uncorrected.unwrap_or(0.0)));
        }
        if pos(self.remap_uncorrectable) {
            out.push(format!("remapped uncorrectable {:.0}", self.remap_uncorrectable.unwrap_or(0.0)));
        }
        if self.remap_failure == Some(true) {
            out.push("row remap FAILURE".into());
        }
        if self.remap_pending == Some(true) {
            out.push("row remap pending".into());
        }
        if pos(self.retired_dbe) {
            out.push(format!("retired pages (double bit) {:.0}", self.retired_dbe.unwrap_or(0.0)));
        }
        if self.retired_pending == Some(true) {
            out.push("page retirement pending".into());
        }
        if self.pcie_width_degraded() {
            out.push(format!("PCIe x{:.0} of x{:.0}", self.pcie_width.unwrap_or(0.0), self.pcie_width_max.unwrap_or(0.0)));
        }
        out
    }
}

impl GpuSample {
    pub fn thermal_throttled(&self) -> bool {
        self.throttle_mask & THROTTLE_THERMAL_MASK != 0
    }
}

/// Names of the decoded throttle bits, in a fixed order. Idle / application-clock bits are
/// not problems and are left out.
pub fn throttle_flags(mask: u64) -> Vec<&'static str> {
    let mut out = Vec::new();
    for (bit, name) in [
        (THROTTLE_HW_THERMAL, "hw_thermal"),
        (THROTTLE_SW_THERMAL, "sw_thermal"),
        (THROTTLE_HW_SLOWDOWN, "hw_slowdown"),
        (THROTTLE_HW_POWER_BRAKE, "hw_power_brake"),
        (THROTTLE_SW_POWER_CAP, "sw_power_cap"),
    ] {
        if mask & bit != 0 {
            out.push(name);
        }
    }
    out
}

fn num(field: &str) -> Option<f64> {
    // "[N/A]", "[Not Supported]", "N/A" → None
    field.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

fn hex_mask(field: &str) -> u64 {
    let f = field.trim();
    let f = f.strip_prefix("0x").or_else(|| f.strip_prefix("0X")).unwrap_or(f);
    u64::from_str_radix(f, 16).unwrap_or(0)
}

pub fn parse_gpu_csv(text: &str) -> Vec<GpuSample> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(',').collect();
            if f.len() < 10 {
                return None;
            }
            Some(GpuSample {
                index: f[0].trim().parse().ok()?,
                temp_c: num(f[1]),
                power_w: num(f[2]),
                power_limit_w: num(f[3]),
                clock_mhz: num(f[4]),
                util_pct: num(f[5]),
                mem_used_mib: num(f[6]),
                mem_total_mib: num(f[7]),
                fan_pct: num(f[8]),
                throttle_mask: hex_mask(f[9]),
                mem_util_pct: f.get(10).copied().and_then(num),
                mem_temp_c: f.get(11).copied().and_then(num),
            })
        })
        .collect()
}

fn yes_no(field: &str) -> Option<bool> {
    match field.trim().to_ascii_lowercase().as_str() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

/// `nvidia-smi --query-gpu=<HEALTH_QUERY_FIELDS> --format=csv,noheader,nounits`
pub fn parse_gpu_health_csv(text: &str, ts: i64) -> Vec<GpuHealth> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(',').collect();
            if f.len() < 18 {
                return None;
            }
            let pstate = f[2].trim();
            Some(GpuHealth {
                index: f[0].trim().parse().ok()?,
                clock_max_mhz: num(f[1]),
                pstate: (pstate.starts_with('P') && pstate.len() <= 3).then(|| pstate.to_string()),
                pcie_gen: num(f[3]),
                pcie_gen_max: num(f[4]),
                pcie_width: num(f[5]),
                pcie_width_max: num(f[6]),
                ecc_corrected: num(f[7]),
                ecc_uncorrected: num(f[8]),
                remap_correctable: num(f[9]),
                remap_uncorrectable: num(f[10]),
                remap_pending: yes_no(f[11]),
                remap_failure: yes_no(f[12]),
                retired_sbe: num(f[13]),
                retired_dbe: num(f[14]),
                retired_pending: yes_no(f[15]),
                power_max_limit_w: num(f[16]),
                power_enforced_w: num(f[17]),
                ts,
            })
        })
        .collect()
}

/// PCI address reduced to (domain, bus, device) so the kernel's `0000:f1:00` and
/// nvidia-smi's `00000000:F1:00.0` compare equal.
pub type PciKey = (u32, u32, u32);

pub fn parse_pci(addr: &str) -> Option<PciKey> {
    let addr = addr.trim().trim_start_matches("PCI:");
    let mut parts = addr.split(':');
    let domain = u32::from_str_radix(parts.next()?, 16).ok()?;
    let bus = u32::from_str_radix(parts.next()?, 16).ok()?;
    let dev_fn = parts.next()?;
    let dev = u32::from_str_radix(dev_fn.split('.').next()?, 16).ok()?;
    Some((domain, bus, dev))
}

/// `nvidia-smi --query-gpu=index,pci.bus_id --format=csv,noheader,nounits`
pub fn parse_pci_map(text: &str) -> Vec<(PciKey, u32)> {
    text.lines()
        .filter_map(|line| {
            let (idx, addr) = line.split_once(',')?;
            Some((parse_pci(addr)?, idx.trim().parse().ok()?))
        })
        .collect()
}

// ------------------------------------------------------------------ GPU adapters
// NVIDIA is `nvidia-smi` (above). The rest of the world:

/// Where the GPU numbers come from on this machine. `None` = no tool at all: the screen hides
/// GPUS and says "GPU stats unavailable" instead of raising an alarm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuSource {
    #[default]
    Nvidia,
    Amd,
    Apple,
    None,
}

impl GpuSource {
    pub fn name(self) -> &'static str {
        match self {
            GpuSource::Nvidia => "nvidia",
            GpuSource::Amd => "amd",
            GpuSource::Apple => "apple",
            GpuSource::None => "none",
        }
    }
    pub fn parse(text: &str) -> GpuSource {
        match text {
            "amd" => GpuSource::Amd,
            "apple" => GpuSource::Apple,
            "none" => GpuSource::None,
            _ => GpuSource::Nvidia,
        }
    }
}

/// `rocm-smi --showtemp --showpower --showuse --showmeminfo vram --json`: one `cardN` object
/// per GPU, every value a STRING. The power key depends on the chip.
pub fn parse_rocm_smi_json(text: &str) -> Vec<GpuSample> {
    let Ok(serde_json::Value::Object(cards)) = serde_json::from_str::<serde_json::Value>(text) else { return Vec::new() };
    let mut out: Vec<GpuSample> = cards
        .iter()
        .filter_map(|(name, card)| {
            let index: u32 = name.strip_prefix("card")?.parse().ok()?;
            let num = |keys: &[&str]| keys.iter().find_map(|k| card.get(*k)).and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_f64().map(|n| n.to_string()))).and_then(|t| t.trim().parse::<f64>().ok());
            let mib = |keys: &[&str]| num(keys).map(|b| (b / 1_048_576.0).round());
            Some(GpuSample {
                index,
                // the junction is the hot spot the driver throttles on; the edge sensor is what older cards have
                temp_c: num(&["Temperature (Sensor junction) (C)", "Temperature (Sensor edge) (C)"]),
                power_w: num(&["Average Graphics Package Power (W)", "Current Socket Graphics Package Power (W)"]),
                power_limit_w: num(&["Max Graphics Package Power (W)"]),
                util_pct: num(&["GPU use (%)"]),
                mem_used_mib: mib(&["VRAM Total Used Memory (B)"]),
                mem_total_mib: mib(&["VRAM Total Memory (B)"]),
                mem_temp_c: num(&["Temperature (Sensor memory) (C)"]),
                ..Default::default()
            })
        })
        .collect();
    out.sort_by_key(|g| g.index);
    out
}

/// `amd-smi metric --json` (rocm-smi's successor): `gpu_data[]`, numbers as `{value, unit}`
/// objects, memory in MB. Best effort: key names moved between releases, so each is looked for
/// in the places it has been seen.
pub fn parse_amd_smi_json(text: &str) -> Vec<GpuSample> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else { return Vec::new() };
    let list = v.get("gpu_data").and_then(|g| g.as_array()).or_else(|| v.as_array()).cloned().unwrap_or_default();
    let val = |x: &serde_json::Value| x.get("value").and_then(serde_json::Value::as_f64).or_else(|| x.as_f64());
    list.iter()
        .enumerate()
        .map(|(i, g)| GpuSample {
            index: g["gpu"].as_u64().map_or(i as u32, |n| n as u32),
            temp_c: val(&g["temperature"]["hotspot"]).or_else(|| val(&g["temperature"]["edge"])),
            power_w: val(&g["power"]["socket_power"]).or_else(|| val(&g["power"]["average_socket_power"])),
            util_pct: val(&g["usage"]["gfx_activity"]),
            mem_used_mib: val(&g["mem_usage"]["used_vram"]),
            mem_total_mib: val(&g["mem_usage"]["total_vram"]),
            mem_temp_c: val(&g["temperature"]["mem"]),
            ..Default::default()
        })
        .collect()
}

/// Apple Silicon without sudo: `ioreg -r -d 1 -w 0 -c IOAccelerator`. It gives the GPU's load
/// and the unified memory it holds; temperature and power need `powermetrics`, which needs
/// root, so they stay `None` (shown as n/a, never as 0). One line of this output is ~44 KB.
pub fn parse_ioreg_accelerator(text: &str) -> Vec<GpuSample> {
    let stats = |key: &str| -> Option<f64> {
        let at = text.find(&format!("\"{key}\"="))? + key.len() + 3;
        text[at..].chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect::<String>().parse().ok()
    };
    let Some(util) = stats("Device Utilization %") else { return Vec::new() };
    vec![GpuSample { index: 0, util_pct: Some(util), mem_used_mib: stats("In use system memory").map(|b| (b / 1_048_576.0).round()), ..Default::default() }]
}

/// The chip's name from the same output: `"model" = "Apple M4"`.
pub fn ioreg_model(text: &str) -> Option<String> {
    let at = text.find("\"model\" = \"")? + 11;
    text[at..].split('"').next().map(String::from)
}

#[cfg(test)]
mod adapter_tests {
    use super::*;
    const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/engines/");

    #[test]
    fn amd_cards_from_rocm_smi_and_amd_smi() {
        let g = parse_rocm_smi_json(&std::fs::read_to_string(format!("{DIR}rocm_smi.json")).unwrap());
        assert_eq!(g.len(), 2);
        assert_eq!((g[0].index, g[0].temp_c, g[0].power_w, g[0].util_pct, g[0].mem_total_mib, g[0].mem_temp_c), (0, Some(47.0), Some(38.0), Some(0.0), Some(20464.0), Some(52.0)));
        assert_eq!((g[1].power_w, g[1].util_pct, g[1].mem_used_mib), (Some(287.0), Some(97.0), Some(18951.0)), "the other power key, same meaning");
        assert!(parse_rocm_smi_json("not json").is_empty() && parse_rocm_smi_json("{\"system\":{}}").is_empty());
        let amd = r#"{"gpu_data":[{"gpu":0,"usage":{"gfx_activity":{"value":12,"unit":"%"}},"power":{"socket_power":{"value":55,"unit":"W"}},"temperature":{"edge":{"value":40,"unit":"C"},"hotspot":{"value":48,"unit":"C"},"mem":{"value":44,"unit":"C"}},"mem_usage":{"total_vram":{"value":24560,"unit":"MB"},"used_vram":{"value":1200,"unit":"MB"}}}]}"#;
        let g = parse_amd_smi_json(amd);
        assert_eq!((g[0].temp_c, g[0].power_w, g[0].util_pct, g[0].mem_used_mib), (Some(48.0), Some(55.0), Some(12.0), Some(1200.0)));
    }

    #[test]
    fn apple_silicon_without_sudo_gives_load_and_memory_and_nothing_made_up() {
        // #25, 2026-09-22: a LIVE capture (`ioreg -r -d 1 -w 0 -c IOAccelerator`, no sudo) on a
        // real Apple M4 Mac - the first time this parser was checked against real Apple
        // hardware output rather than an assembled sample (no macOS collector binary exists yet
        // to run end-to-end; this is the parser itself, hand-verified against the real text).
        let text = std::fs::read_to_string(format!("{DIR}ioreg_ioaccelerator.txt")).unwrap();
        let g = parse_ioreg_accelerator(&text);
        assert_eq!(g.len(), 1);
        assert_eq!((g[0].util_pct, g[0].mem_used_mib, g[0].temp_c, g[0].power_w), (Some(2.0), Some(318.0), None, None), "no temperature or power without root: n/a, not 0");
        assert_eq!(ioreg_model(&text).as_deref(), Some("Apple M4"));
        assert!(parse_ioreg_accelerator("").is_empty());
        assert_eq!((GpuSource::parse("apple"), GpuSource::parse(""), GpuSource::None.name()), (GpuSource::Apple, GpuSource::Nvidia, "none"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_nvidia_smi_fixture() {
        let gpus = parse_gpu_csv(include_str!("../../../fixtures/nvidia_smi.csv"));
        assert_eq!(gpus.len(), 4);
        assert_eq!(gpus[0].index, 0);
        assert_eq!(gpus[0].temp_c, Some(52.0));
        assert_eq!(gpus[0].power_limit_w, Some(300.0));
        assert_eq!(gpus[3].mem_total_mib, Some(97887.0));
        assert_eq!(gpus[3].throttle_mask, 0);
        assert!(!gpus[0].thermal_throttled());
    }

    #[test]
    fn real_health_fixture_and_the_extended_fast_query() {
        let h = parse_gpu_health_csv(include_str!("../../../fixtures/nvidia_smi_health.csv"), 42);
        assert_eq!(h.len(), 4);
        assert_eq!((h[0].clock_max_mhz, h[0].pstate.as_deref()), (Some(3090.0), Some("P1")));
        assert_eq!((h[0].pcie_gen, h[0].pcie_gen_max, h[0].pcie_width, h[0].pcie_width_max), (Some(5.0), Some(5.0), Some(16.0), Some(16.0)));
        assert_eq!((h[1].ecc_corrected, h[1].ecc_uncorrected, h[1].remap_pending, h[1].remap_failure), (Some(0.0), Some(0.0), Some(false), Some(false)));
        assert_eq!(h[2].retired_sbe, None, "[N/A] on this board");
        assert_eq!((h[3].power_max_limit_w, h[3].power_enforced_w, h[3].ts), (Some(325.0), Some(300.0), 42));
        assert!(h.iter().all(|g| g.problems().is_empty()));

        let sick = parse_gpu_health_csv("1, 3090, P0, 4, 5, 8, 16, 12, 2, 1, 1, Yes, No, [N/A], [N/A], [N/A], 300.00, 300.00\n", 0);
        assert_eq!(sick[0].problems(), vec!["ECC uncorrected 2", "remapped uncorrectable 1", "row remap pending", "PCIe x8 of x16"]);

        let g = parse_gpu_csv("0, 52, 187.10, 300.00, 1830, 98, 46200, 97887, 30, 0x0000000000000004, 41, N/A\n");
        assert_eq!((g[0].mem_util_pct, g[0].mem_temp_c), (Some(41.0), None));
    }

    #[test]
    fn na_fields_and_masks() {
        let g = parse_gpu_csv("2, 91, [N/A], 300.00, 1200, 99, 1, 2, [Not Supported], 0x0000000000000064\nshort,line\n");
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].power_w, None);
        assert_eq!(g[0].fan_pct, None);
        assert_eq!(g[0].throttle_mask, 0x64);
        assert!(g[0].thermal_throttled());
        assert_eq!(throttle_flags(0x64), vec!["hw_thermal", "sw_thermal", "sw_power_cap"]);
        assert_eq!(throttle_flags(0x8), vec!["hw_slowdown"]);
        assert_eq!(throttle_flags(0x1), Vec::<&str>::new());
    }

    #[test]
    fn real_pci_map_matches_kernel_form() {
        let map = parse_pci_map(include_str!("../../../fixtures/nvidia_pci.csv"));
        assert_eq!(map.len(), 4);
        let key = parse_pci("PCI:0000:f1:00").unwrap();
        assert_eq!(map.iter().find(|(k, _)| *k == key).map(|(_, i)| *i), Some(3));
        let key = parse_pci("0000:21:00").unwrap();
        assert_eq!(map.iter().find(|(k, _)| *k == key).map(|(_, i)| *i), Some(1));
    }
}
