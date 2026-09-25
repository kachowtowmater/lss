//! Card #178: read the NVIDIA GPUs through the driver's own library instead of forking
//! `nvidia-smi` on every poll. Measured on the GPU box: the fork costs ~85-90 ms; a direct NVML
//! read of all four GPUs costs ~0.012 ms - the fork is not reading sensors slowly, it is
//! paying process-startup cost every 5 s forever.
//!
//! `libnvidia-ml.so.1` is opened at RUNTIME (`dlopen`), never linked at build time - the musl
//! static binary and every non-NVIDIA install must keep working when the library is absent.
//! Anything that fails to load, to find every symbol this module needs, or to init cleanly
//! makes `Nvml::load()` return `None`, and the caller falls back to `nvidia-smi` unchanged -
//! this module can only ADD a faster path, never remove the one that already works everywhere.
//!
//! THE GOTCHA (pulse's changelog, and independently confirmed by three of our own verifiers
//! against live hardware): `NVML_CLOCK_SM` is enum value **1**, not 0 - 0 is
//! `NVML_CLOCK_GRAPHICS`. On at least one current workstation GPU the two clock domains report IDENTICAL values,
//! so a test that compares NVML's clock reading against nvidia-smi's `clocks.sm` on THIS
//! hardware passes whether the constant is right or wrong. The only defence is pinning the
//! constant itself (`nvml_clock_sm_is_enum_one`, below), not the value it produces here.

use lss_core::gpu::GpuSample;
use std::ffi::{c_void, CString};
use std::sync::atomic::{AtomicBool, Ordering};

const LIB_NAMES: &[&str] = &["libnvidia-ml.so.1", "libnvidia-ml.so"];

// ---- NVML's own constants (from nvml.h; stable across the versions we target) ----

/// `nvmlTemperatureSensors_t`: the one sensor `nvmlDeviceGetTemperature` supports everywhere.
const NVML_TEMPERATURE_GPU: u32 = 0;
/// `nvmlClockType_t::NVML_CLOCK_SM` - what `clocks.sm` in nvidia-smi's CSV actually reads.
/// THE GOTCHA: passing `NVML_CLOCK_GRAPHICS` (0) instead reads a DIFFERENT clock domain that
/// happens to report the same number on every GPU we own - see the module doc comment.
const NVML_CLOCK_SM: u32 = 1;
const NVML_SUCCESS: i32 = 0;
/// `nvmlFieldId_t::NVML_FI_DEV_MEMORY_TEMP` - the memory-junction temperature nvidia-smi
/// exposes as `temperature.memory`. HBM boards report it; GDDR boards answer NOT_SUPPORTED,
/// which is the honest "n/a on this board" and NOT the same as "we never asked" (card #178,
/// lss-verifier-4: the smi path read this field and the first NVML path silently dropped it,
/// after which the page claimed n/a about hardware it had not questioned).
const NVML_FI_DEV_MEMORY_TEMP: u32 = 82;
/// `nvmlValueType_t` discriminants we accept back from a field read (nvml.h's own enum order -
/// card #178, lss-verifier-4's second defect: an earlier version of this list named discriminant
/// 4 `SINT` and decoded it as a 4-byte signed int. nvml.h's actual order has 4 = SIGNED_LONG_LONG
/// (8 bytes) and 5 = SIGNED_INT (4 bytes) - a driver returning either of the two NVML types this
/// module had never seen (5 or 6) fell to `_ => None`, the same "n/a about hardware that isn't"
/// defect that defect 1 was fixed for, just one field-type away. Pinned as VALUES, not just sizes, in
/// `the_versioned_struct_layouts_match_what_nvml_expects` below - no reading on our own GDDR
/// boards can tell a wrong discriminant from a right one, same reasoning as `NVML_CLOCK_SM`.
const NVML_VALUE_TYPE_DOUBLE: u32 = 0;
const NVML_VALUE_TYPE_UINT: u32 = 1;
const NVML_VALUE_TYPE_ULONG: u32 = 2;
const NVML_VALUE_TYPE_ULONGLONG: u32 = 3;
const NVML_VALUE_TYPE_SIGNED_LONG_LONG: u32 = 4;
const NVML_VALUE_TYPE_SIGNED_INT: u32 = 5;
const NVML_VALUE_TYPE_UNSIGNED_SHORT: u32 = 6;

#[repr(C)]
#[derive(Default)]
struct NvmlUtilization {
    gpu: u32,
    memory: u32,
}

#[repr(C)]
#[derive(Default)]
struct NvmlMemory {
    total: u64,
    free: u64,
    used: u64,
}

/// `nvmlMemory_v2_t`. THE REASON THIS EXISTS (card #178, lss-verifier-4, measured): v1's
/// `used` INCLUDES the driver-reserved block and nvidia-smi's `memory.used` EXCLUDES it, so
/// the v1 path read 639 MiB high on every GPU on this box - a number that would have moved on
/// screen with nothing changing on the machine. v2 breaks `reserved` out into its own field
/// and leaves `used` comparable with smi.
#[repr(C)]
#[derive(Default)]
struct NvmlMemoryV2 {
    version: u32,
    total: u64,
    reserved: u64,
    free: u64,
    used: u64,
}

/// NVML's versioned-struct convention: the low 24 bits carry `sizeof(struct)` and the top byte
/// carries the version, so the library can tell which layout the caller compiled against.
const fn nvml_struct_version(size: usize, ver: u32) -> u32 {
    (size as u32) | (ver << 24)
}

/// `nvmlFieldValue_t`. Only read through `nvmlDeviceGetFieldValues`, and every field of the
/// reply is checked before the value is trusted: `nvml_return` must be SUCCESS and the
/// `value_type` must be one we know, else the reading is dropped rather than reinterpreted.
#[repr(C)]
#[derive(Default)]
struct NvmlFieldValue {
    field_id: u32,
    scope_id: u32,
    timestamp: i64,
    latency_usec: i64,
    value_type: u32,
    nvml_return: i32,
    /// `nvmlValue_t` is a union of d/ui/ul/ull/sll, all 8 bytes wide.
    value: [u8; 8],
}

type NvmlDevice = *mut c_void;

// ---- the handful of entry points this module calls, resolved by name at runtime ----
type FnInit = unsafe extern "C" fn() -> i32;
type FnShutdown = unsafe extern "C" fn() -> i32;
type FnDeviceCount = unsafe extern "C" fn(*mut u32) -> i32;
type FnDeviceHandle = unsafe extern "C" fn(u32, *mut NvmlDevice) -> i32;
type FnTemperature = unsafe extern "C" fn(NvmlDevice, u32, *mut u32) -> i32;
type FnPower = unsafe extern "C" fn(NvmlDevice, *mut u32) -> i32;
type FnPowerLimit = unsafe extern "C" fn(NvmlDevice, *mut u32) -> i32;
type FnClock = unsafe extern "C" fn(NvmlDevice, u32, *mut u32) -> i32;
type FnUtilization = unsafe extern "C" fn(NvmlDevice, *mut NvmlUtilization) -> i32;
type FnMemory = unsafe extern "C" fn(NvmlDevice, *mut NvmlMemory) -> i32;
type FnMemoryV2 = unsafe extern "C" fn(NvmlDevice, *mut NvmlMemoryV2) -> i32;
type FnFieldValues = unsafe extern "C" fn(NvmlDevice, i32, *mut NvmlFieldValue) -> i32;
type FnFan = unsafe extern "C" fn(NvmlDevice, *mut u32) -> i32;
type FnThrottle = unsafe extern "C" fn(NvmlDevice, *mut u64) -> i32;

/// A loaded `libnvidia-ml.so.1` with every symbol this module needs already resolved.
/// `Send + Sync`-free by design: the collector's poll thread that created it is the only one
/// that ever touches it (see `collect.rs`), so no locking is needed around calls.
pub struct Nvml {
    handle: *mut c_void,
    init: FnInit,
    shutdown: FnShutdown,
    device_count: FnDeviceCount,
    device_handle: FnDeviceHandle,
    temperature: FnTemperature,
    power: FnPower,
    power_limit: FnPowerLimit,
    clock: FnClock,
    utilization: FnUtilization,
    memory: FnMemory,
    /// OPTIONAL: absent on drivers older than the v2 memory API. `None` = fall back to v1.
    memory_v2: Option<FnMemoryV2>,
    /// OPTIONAL: absent on older drivers. `None` = memory temperature is simply not read.
    field_values: Option<FnFieldValues>,
    fan: FnFan,
    throttle: FnThrottle,
    /// set once shutdown has run, so Drop never double-shuts-down
    shut_down: AtomicBool,
}

// SAFETY: every call into the library goes through `&self` methods that pass NVML its own
// opaque device handles straight through; nothing here is shared across threads (see the
// struct doc comment), but the collector's `Poller` itself needs to be `Send` to move into
// its scope threads, which requires this. There is no interior mutability NVML itself is not
// already safe for (the driver serialises its own calls).
unsafe impl Send for Nvml {}
unsafe impl Sync for Nvml {}

macro_rules! sym {
    ($handle:expr, $name:expr) => {{
        let cname = CString::new($name).expect("static symbol name");
        // SAFETY: `handle` came from a successful `dlopen` just above and outlives every
        // `dlsym` call in this function; `dlsym` returns null (never a dangling pointer) on
        // a missing symbol, which the `?` below turns into `Nvml::load() -> None`.
        let p = unsafe { libc::dlsym($handle, cname.as_ptr()) };
        if p.is_null() {
            None
        } else {
            Some(p)
        }
    }};
}

/// Decodes an `nvmlValue_t` union through the `value_type` NVML says it wrote. Free function
/// (needs no device, no dlopen'd library) so its WIDTHS - not just the discriminant constants -
/// can be unit-tested with synthetic bytes, no GPU required.
///
/// Card #178, verifier-3's third defect: the discriminants were pinned by
/// `nvml_value_type_discriminants_match_the_header` but nothing pinned the DECODE WIDTHS this
/// match performs - and on little-endian hardware, an 8-byte SIGNED_LONG_LONG whose value fits
/// in 32 bits decodes IDENTICALLY whether read as 8 bytes or (wrongly) as the first 4, so every
/// live-hardware test stayed green through that regression. The union is little-end-aligned
/// (nvml.h's `nvmlValue_t`): a narrower type lives in the LOW bytes of the 8-byte slot, so a
/// u16/i32 read takes `b[0..N]` rather than the tail.
fn decode_nvml_value(value_type: u32, b: [u8; 8]) -> Option<f64> {
    match value_type {
        NVML_VALUE_TYPE_DOUBLE => Some(f64::from_ne_bytes(b)),
        NVML_VALUE_TYPE_UINT => Some(u32::from_ne_bytes([b[0], b[1], b[2], b[3]]) as f64),
        NVML_VALUE_TYPE_ULONG | NVML_VALUE_TYPE_ULONGLONG => Some(u64::from_ne_bytes(b) as f64),
        NVML_VALUE_TYPE_SIGNED_LONG_LONG => Some(i64::from_ne_bytes(b) as f64),
        NVML_VALUE_TYPE_SIGNED_INT => Some(i32::from_ne_bytes([b[0], b[1], b[2], b[3]]) as f64),
        NVML_VALUE_TYPE_UNSIGNED_SHORT => Some(u16::from_ne_bytes([b[0], b[1]]) as f64),
        _ => None,
    }
}

impl Nvml {
    /// Opens the library and resolves every symbol this module uses. `None` on ANY failure -
    /// library absent, a symbol missing (an unexpectedly old driver), or `nvmlInit_v2` itself
    /// failing - so the caller can fall back to `nvidia-smi` without knowing why.
    pub fn load() -> Option<Nvml> {
        let handle = LIB_NAMES.iter().find_map(|name| {
            let cname = CString::new(*name).ok()?;
            // SAFETY: `cname` is a valid, NUL-terminated C string for the duration of this call.
            let h = unsafe { libc::dlopen(cname.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            (!h.is_null()).then_some(h)
        })?;

        let load = |name: &'static str| sym!(handle, name);
        let nvml = (|| {
            Some(Nvml {
                handle,
                init: unsafe { std::mem::transmute::<*mut c_void, FnInit>(load("nvmlInit_v2")?) },
                shutdown: unsafe { std::mem::transmute::<*mut c_void, FnShutdown>(load("nvmlShutdown")?) },
                device_count: unsafe { std::mem::transmute::<*mut c_void, FnDeviceCount>(load("nvmlDeviceGetCount_v2")?) },
                device_handle: unsafe { std::mem::transmute::<*mut c_void, FnDeviceHandle>(load("nvmlDeviceGetHandleByIndex_v2")?) },
                temperature: unsafe { std::mem::transmute::<*mut c_void, FnTemperature>(load("nvmlDeviceGetTemperature")?) },
                power: unsafe { std::mem::transmute::<*mut c_void, FnPower>(load("nvmlDeviceGetPowerUsage")?) },
                power_limit: unsafe { std::mem::transmute::<*mut c_void, FnPowerLimit>(load("nvmlDeviceGetEnforcedPowerLimit")?) },
                clock: unsafe { std::mem::transmute::<*mut c_void, FnClock>(load("nvmlDeviceGetClockInfo")?) },
                utilization: unsafe { std::mem::transmute::<*mut c_void, FnUtilization>(load("nvmlDeviceGetUtilizationRates")?) },
                memory: unsafe { std::mem::transmute::<*mut c_void, FnMemory>(load("nvmlDeviceGetMemoryInfo")?) },
                // OPTIONAL symbols: loaded WITHOUT `?` so an older driver still yields a
                // working Nvml rather than falling the whole path back to forking nvidia-smi.
                memory_v2: load("nvmlDeviceGetMemoryInfo_v2").map(|p| unsafe { std::mem::transmute::<*mut c_void, FnMemoryV2>(p) }),
                field_values: load("nvmlDeviceGetFieldValues").map(|p| unsafe { std::mem::transmute::<*mut c_void, FnFieldValues>(p) }),
                fan: unsafe { std::mem::transmute::<*mut c_void, FnFan>(load("nvmlDeviceGetFanSpeed")?) },
                throttle: unsafe { std::mem::transmute::<*mut c_void, FnThrottle>(load("nvmlDeviceGetCurrentClocksThrottleReasons")?) },
                shut_down: AtomicBool::new(false),
            })
        })();

        let Some(nvml) = nvml else {
            // SAFETY: `handle` is a valid handle from the successful `dlopen` above; nothing
            // else references it yet since `nvml` never got constructed.
            unsafe {
                libc::dlclose(handle);
            }
            return None;
        };

        // SAFETY: `init` was just resolved from the freshly opened library.
        if unsafe { (nvml.init)() } != NVML_SUCCESS {
            // init failed: no shutdown call is valid, just release the library.
            // SAFETY: same handle as above, still valid, still ours alone.
            unsafe {
                libc::dlclose(nvml.handle);
            }
            return None;
        }
        Some(nvml)
    }

    fn handle_for(&self, index: u32) -> Option<NvmlDevice> {
        let mut dev: NvmlDevice = std::ptr::null_mut();
        // SAFETY: `device_handle` takes a plain index and an out-pointer to a stack local;
        // NVML never retains the pointer past the call.
        let rc = unsafe { (self.device_handle)(index, &mut dev) };
        (rc == NVML_SUCCESS).then_some(dev)
    }

    /// One GPU's reading, in the same shape `parse_gpu_csv` produces from nvidia-smi's CSV,
    /// so the two paths are interchangeable to every caller. A field NVML refuses (device
    /// does not have a fan, older driver lacks a symbol's behaviour) comes back `None`, same
    /// convention as the CSV path's `[N/A]` - never a fabricated 0.
    fn sample(&self, dev: NvmlDevice, index: u32) -> GpuSample {
        // SAFETY: every call below passes `dev` (a live handle from `handle_for`, used only
        // for the duration of this function) and an out-pointer to a local; NVML fills it or
        // returns a non-success code, which is checked before the value is trusted.
        unsafe {
            let mut temp = 0u32;
            let temp_c = ((self.temperature)(dev, NVML_TEMPERATURE_GPU, &mut temp) == NVML_SUCCESS).then_some(temp as f64);

            let mut power = 0u32;
            let power_w = ((self.power)(dev, &mut power) == NVML_SUCCESS).then_some(power as f64 / 1000.0);

            let mut limit = 0u32;
            let power_limit_w = ((self.power_limit)(dev, &mut limit) == NVML_SUCCESS).then_some(limit as f64 / 1000.0);

            let mut clock = 0u32;
            let clock_mhz = ((self.clock)(dev, NVML_CLOCK_SM, &mut clock) == NVML_SUCCESS).then_some(clock as f64);

            let mut util = NvmlUtilization::default();
            let (util_pct, mem_util_pct) = if (self.utilization)(dev, &mut util) == NVML_SUCCESS { (Some(util.gpu as f64), Some(util.memory as f64)) } else { (None, None) };

            // card #178: prefer the v2 memory call. v1's `used` INCLUDES the driver-reserved
            // block; nvidia-smi's `memory.used` does not, so v1 alone read ~639 MiB high per
            // GPU on this box. v1 stays as the fallback for drivers without the v2 symbol.
            let (mem_used_mib, mem_total_mib) = self.memory_mib(dev);

            let mut fan = 0u32;
            // not every board has a fan (blower-less datacenter cards): NOT_SUPPORTED is
            // expected there, not an error - the result is already `None` via `.then_some`.
            let fan_pct = ((self.fan)(dev, &mut fan) == NVML_SUCCESS).then_some(fan as f64);

            let mut mask = 0u64;
            // NVML's throttle-reason bits are the SAME bitmask nvidia-smi prints as
            // `clocks_throttle_reasons.active` (both come from the driver's one enum), so
            // `THROTTLE_*` in `lss_core::gpu` applies unchanged - no remapping needed.
            let throttle_mask = if (self.throttle)(dev, &mut mask) == NVML_SUCCESS { mask } else { 0 };

            // card #178: the smi path read `temperature.memory`; dropping it made the page
            // say "n/a on this board" about hardware nobody had asked. NOT_SUPPORTED from
            // NVML IS an honest n/a; a missing symbol or malformed reply also yields None,
            // which is the pre-existing behaviour and never a fabricated number.
            let mem_temp_c = self.memory_temp_c(dev);

            GpuSample { index, temp_c, power_w, power_limit_w, clock_mhz, util_pct, mem_used_mib, mem_total_mib, fan_pct, throttle_mask, mem_util_pct, mem_temp_c }
        }
    }

    /// Every GPU NVML can see, in index order. `None` when the device count cannot be read at
    /// all; an empty (never fabricated) list otherwise.
    ///
    /// Item 3 of the card: NVML is re-queried for the handle of each device by index, so if a
    /// GPU is added or removed by the driver between the count read and a per-device read (an
    /// MIG reconfiguration, a driver reset), `handle_for` for the now-invalid index fails
    /// cleanly and that device is skipped rather than read from a stale handle. If the COUNT
    /// itself changed - fewer or more devices than first observed - the whole read is retried
    /// once against the fresh count before giving up on that poll.
    /// `(used_mib, total_mib)`, preferring `nvmlDeviceGetMemoryInfo_v2` so `used` matches
    /// nvidia-smi's `memory.used` (card #178). Falls back to v1 when the symbol is absent.
    fn memory_mib(&self, dev: NvmlDevice) -> (Option<f64>, Option<f64>) {
        let mib = |b: u64| (b as f64 / 1_048_576.0).round();
        if let Some(v2) = self.memory_v2 {
            let mut m = NvmlMemoryV2 {
                version: nvml_struct_version(std::mem::size_of::<NvmlMemoryV2>(), 2),
                ..Default::default()
            };
            // SAFETY: `dev` is an opaque handle NVML gave us; `m` is a correctly versioned,
            // correctly sized v2 struct that outlives the call.
            if unsafe { v2(dev, &mut m) } == NVML_SUCCESS {
                return (Some(mib(m.used)), Some(mib(m.total)));
            }
        }
        let mut mem = NvmlMemory::default();
        // SAFETY: as above, for the v1 layout.
        if unsafe { (self.memory)(dev, &mut mem) } == NVML_SUCCESS {
            (Some(mib(mem.used)), Some(mib(mem.total)))
        } else {
            (None, None)
        }
    }

    /// Memory-junction temperature in C via `nvmlDeviceGetFieldValues`. `None` when the symbol
    /// is absent, when NVML reports anything but SUCCESS (NOT_SUPPORTED on a GDDR board is the
    /// expected, honest n/a), or when the declared value type is not one we know - the union is
    /// only ever read through the type NVML says it wrote.
    fn memory_temp_c(&self, dev: NvmlDevice) -> Option<f64> {
        let f = self.field_values?;
        let mut v = NvmlFieldValue { field_id: NVML_FI_DEV_MEMORY_TEMP, ..Default::default() };
        // SAFETY: one correctly sized `nvmlFieldValue_t` is passed with a count of 1; NVML
        // writes only into it and it outlives the call.
        if unsafe { f(dev, 1, &mut v) } != NVML_SUCCESS || v.nvml_return != NVML_SUCCESS {
            return None;
        }
        decode_nvml_value(v.value_type, v.value)
    }

    pub fn samples(&self) -> Option<Vec<GpuSample>> {
        for _ in 0..2 {
            let mut count = 0u32;
            // SAFETY: out-pointer to a local; see the module-level SAFETY note.
            if unsafe { (self.device_count)(&mut count) } != NVML_SUCCESS {
                return None;
            }
            let mut out = Vec::with_capacity(count as usize);
            let mut count_changed = false;
            for i in 0..count {
                match self.handle_for(i) {
                    Some(dev) => out.push(self.sample(dev, i)),
                    None => {
                        count_changed = true;
                        break;
                    }
                }
            }
            if !count_changed {
                return Some(out);
            }
            // device count moved under us mid-read: re-check and try exactly once more.
        }
        None
    }
}

impl Drop for Nvml {
    fn drop(&mut self) {
        if !self.shut_down.swap(true, Ordering::SeqCst) {
            // SAFETY: `shutdown` and `handle` were resolved/opened together in `load` and
            // nothing else holds this `Nvml`'s handle (it is not `Clone`).
            unsafe {
                (self.shutdown)();
                libc::dlclose(self.handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE GOTCHA, pinned on the CONSTANT rather than a reading: three of our own verifiers
    /// measured that `NVML_CLOCK_SM` and `NVML_CLOCK_GRAPHICS` report the IDENTICAL value on
    /// every GPU they measured, so a value-based test (assert the clock "looks right") would pass
    /// whether this module passed 0 or 1 to NVML. Only a test that pins the constant itself
    /// can catch the defect pulse's changelog describes.
    #[test]
    fn nvml_clock_sm_is_enum_one_not_graphics() {
        /// `nvmlClockType_t::NVML_CLOCK_GRAPHICS` - the wrong one to pass for "the SM clock",
        /// kept here rather than as production code since nothing outside this assertion
        /// needs it - see the module doc comment for why the assertion still matters.
        const NVML_CLOCK_GRAPHICS: u32 = 0;
        assert_eq!(NVML_CLOCK_SM, 1);
        assert_eq!(NVML_CLOCK_GRAPHICS, 0);
        assert_ne!(NVML_CLOCK_SM, NVML_CLOCK_GRAPHICS, "picking the wrong enum is invisible on our hardware - see the module doc comment");
    }

    /// A box with no NVIDIA driver at all (this dev machine, most CI runners): `load()` must
    /// return `None` cleanly, never panic, so the collector's fallback to nvidia-smi is the
    /// only path exercised. This is the actual test that runs everywhere; the NVML-present
    /// path is only exercisable on a real NVIDIA box, see the card's before/after
    /// note for that evidence instead of a unit test that cannot run in this environment.
    #[test]
    fn load_returns_none_without_a_driver_present_and_never_panics() {
        // On a box where the library IS present this legitimately returns Some; the
        // assertion that matters everywhere else is simply that it does not panic either way.
        let _ = Nvml::load();
    }

    /// Card #178, lss-verifier-4's defect 1. These layouts are a CONTRACT with the driver: a
    /// wrong size makes NVML reject the call (v2 encodes sizeof in its version word) or, worse,
    /// makes the union read at the wrong offset. Neither failure is visible in the numbers, so
    /// the sizes are asserted rather than trusted - the same reason NVML_CLOCK_SM is pinned as
    /// a constant below instead of checked against a clock reading.
    #[test]
    fn the_versioned_struct_layouts_match_what_nvml_expects() {
        assert_eq!(std::mem::size_of::<NvmlMemoryV2>(), 40, "nvmlMemory_v2_t is u32 + 4 x u64 with padding");
        assert_eq!(std::mem::size_of::<NvmlFieldValue>(), 40, "nvmlFieldValue_t layout changed - the union offset moves with it");
        // the version word NVML matches against: low 24 bits sizeof, top byte the version
        assert_eq!(nvml_struct_version(40, 2), 40 | (2 << 24));
        assert_eq!(nvml_struct_version(std::mem::size_of::<NvmlMemoryV2>(), 2), 0x0200_0028);
    }

    /// Card #178, lss-verifier-4's SECOND defect: `nvmlValueType_t`'s discriminants, checked
    /// against /usr/local/cuda-12.8/include/nvml.h:582-588 (DOUBLE=0, UNSIGNED_INT=1,
    /// UNSIGNED_LONG=2, UNSIGNED_LONG_LONG=3, SIGNED_LONG_LONG=4, SIGNED_INT=5,
    /// UNSIGNED_SHORT=6). No reading on our own hardware exercises 4, 5 or 6 (memory temp on a
    /// GDDR board answers NOT_SUPPORTED before a value type is ever decided), so - same as
    /// `NVML_CLOCK_SM` - the only ruler that can catch a transposed constant here is pinning
    /// the values themselves, not a live reading.
    #[test]
    fn nvml_value_type_discriminants_match_the_header() {
        assert_eq!(NVML_VALUE_TYPE_DOUBLE, 0);
        assert_eq!(NVML_VALUE_TYPE_UINT, 1);
        assert_eq!(NVML_VALUE_TYPE_ULONG, 2);
        assert_eq!(NVML_VALUE_TYPE_ULONGLONG, 3);
        assert_eq!(NVML_VALUE_TYPE_SIGNED_LONG_LONG, 4, "NOT signed int - an 8-byte value, not 4");
        assert_eq!(NVML_VALUE_TYPE_SIGNED_INT, 5);
        assert_eq!(NVML_VALUE_TYPE_UNSIGNED_SHORT, 6);
    }

    /// Card #178, verifier-3's third defect: the discriminants above are pinned, but nothing
    /// pinned the decode WIDTHS - and on little-endian hardware an i64 whose value fits in 32
    /// bits decodes identically whether read as 8 bytes or (wrongly) as the first 4, so no
    /// reading on our real GPUs (whose memory temp is always a small number) can tell a
    /// truncated decode from a correct one. These use values chosen specifically to be WRONG
    /// under the old, narrower reads - no GPU needed, synthetic bytes only.
    #[test]
    fn decode_nvml_value_reads_the_full_width_for_each_type() {
        // past 2^31 (2_147_483_648): a 4-byte i32 read of the low bytes would truncate/wrap
        // this, an 8-byte i64 read must not.
        let past_i32_max: i64 = 5_000_000_000;
        let mut b = [0u8; 8];
        b[..8].copy_from_slice(&past_i32_max.to_ne_bytes());
        assert_eq!(decode_nvml_value(NVML_VALUE_TYPE_SIGNED_LONG_LONG, b), Some(5_000_000_000.0));

        // a negative SIGNED_INT must stay negative, not be read as an unsigned width.
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&(-12_345i32).to_ne_bytes());
        assert_eq!(decode_nvml_value(NVML_VALUE_TYPE_SIGNED_INT, b), Some(-12_345.0));

        // the max UNSIGNED_SHORT: a 1-byte-short read would drop the high byte and read 255.
        let mut b = [0u8; 8];
        b[..2].copy_from_slice(&0xFFFFu16.to_ne_bytes());
        assert_eq!(decode_nvml_value(NVML_VALUE_TYPE_UNSIGNED_SHORT, b), Some(65_535.0));

        // an unknown discriminant is dropped, never guessed at.
        assert_eq!(decode_nvml_value(99, [0u8; 8]), None);
    }

    /// LIVE hardware check for card #178 defect 1, the one the oracle CAN discriminate:
    /// `cargo test -p lss-collector nvml_memory_used_agrees_with_nvidia_smi -- --ignored --nocapture`
    /// v1's `used` includes the driver-reserved block and smi's `memory.used` does not, which
    /// read 639 MiB high per GPU. If this regresses to v1 the gap comes straight back.
    #[test]
    #[ignore]
    fn nvml_memory_used_agrees_with_nvidia_smi() {
        let nvml = Nvml::load().expect("NVML should load on a box with the driver present");
        let samples = nvml.samples().expect("device count should read");
        let out = std::process::Command::new("nvidia-smi")
            .args(["--query-gpu=index,memory.used", "--format=csv,noheader,nounits"])
            .output()
            .expect("nvidia-smi runs on a box with the driver present");
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            let (Ok(idx), Ok(smi_mib)) = (f[0].parse::<u32>(), f[1].parse::<f64>()) else { continue };
            let g = samples.iter().find(|g| g.index == idx).expect("same GPU set");
            let nvml_mib = g.mem_used_mib.expect("NVML reported memory");
            let gap = (nvml_mib - smi_mib).abs();
            println!("gpu{idx}: nvml {nvml_mib} MiB vs smi {smi_mib} MiB (gap {gap})");
            // 64 MiB of slack for genuine allocation drift between the two reads; the v1 bug
            // was ~639 MiB and constant, so it cannot hide under this.
            assert!(gap < 64.0, "gpu{idx}: NVML {nvml_mib} MiB vs smi {smi_mib} MiB - gap {gap} MiB. v1 includes driver-reserved memory; v2 does not (card #178)");
        }
    }

    /// LIVE hardware check, run by hand where NVIDIA is actually present:
    /// `cargo test -p lss-collector nvml_reads_real_gpus_when_available -- --ignored --nocapture`.
    /// Not part of the default suite (nothing else in this workspace touches real GPU hardware
    /// either) - this is the card's own teeth-proof that the dlopen/dlsym path genuinely loads
    /// the driver and reads plausible numbers, not just that it compiles.
    #[test]
    #[ignore]
    fn nvml_reads_real_gpus_when_available() {
        let nvml = Nvml::load().expect("NVML should load on a box with the driver present");
        let samples = nvml.samples().expect("device count should read");
        assert!(!samples.is_empty(), "expected at least one GPU");
        for g in &samples {
            println!("{g:?}");
            assert!(g.temp_c.is_some_and(|t| t > 0.0 && t < 120.0), "implausible temp: {:?}", g.temp_c);
            assert!(g.clock_mhz.is_some_and(|c| c > 0.0), "implausible SM clock: {:?}", g.clock_mhz);
            assert!(g.mem_total_mib.is_some_and(|m| m > 0.0), "implausible total memory: {:?}", g.mem_total_mib);
        }
    }

    /// The card's own "after" measurement, on our binding rather than pulse's figure: run by
    /// hand where NVIDIA is present, alongside `nvml_reads_real_gpus_when_available`.
    /// `cargo test -p lss-collector nvml_timing_matches_the_cards_before_after_claim -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn nvml_timing_matches_the_cards_before_after_claim() {
        let nvml = Nvml::load().expect("NVML should load on a box with the driver present");
        let n = 200;
        let start = std::time::Instant::now();
        for _ in 0..n {
            let g = nvml.samples().expect("device count should read");
            assert!(!g.is_empty());
        }
        let per_read = start.elapsed() / n;
        println!("NVML: {n} full-GPU-set reads, {:.4} ms/read (fork-based nvidia-smi measured ~85-90 ms/read on this same box)", per_read.as_secs_f64() * 1000.0);
    }
}
