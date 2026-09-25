//! Money formatting shared by BOTH surfaces - the TUI's ELECTRICITY panel and `lss status`'s
//! COST block (card #173).
//!
//! It lives here rather than in either surface because card #173 is precisely what happens when
//! it does not: the cost feature existed only in the TUI, so the plain CLI - the thing a
//! stranger and a script both read - showed no money at all, and the owner said "i dont see any
//! of the things we talked about in terms of pricing". Sharing the FORMATTERS is what makes the
//! two surfaces carry the same caveats by construction instead of by somebody remembering to
//! copy the wording across.

use lss_core::timeutil::fmt_duration;

/// A dollar figure, or an em dash with why there is not one yet - never a fabricated $0.00.
pub fn usd(v: Option<f64>, reason: &str) -> String {
    v.map_or_else(|| format!("\u{2014} {reason}"), |v| format!("${v:.2}"))
}

/// #103, 2026-09-22 (verifier), precision fixed by #114 (verifier-3): a real per-1M-token figure
/// can be well under a cent (e.g. $0.00031) - two decimals rounds it to "$0.00", the exact
/// confident-wrong-number the honesty rule exists to prevent. #103's first fix had a CLIFF at
/// exactly one cent: $0.0123 (just above) rendered "$0.01" (19% off the real number), while
/// $0.00999 (just below) rendered all 5 decimals - two neighbouring values, wildly different
/// precision, for no reason a reader could see. This picks the decimal count from the value's
/// own MAGNITUDE instead of a hard threshold, so precision changes smoothly (never more than one
/// decimal place between neighbouring order-of-magnitude values) and the number itself is never
/// rounded away: at least 2 decimals (a normal dollar figure still reads like one), at most 6
/// (an absurd number of decimals for money is its own kind of unreadable).
pub fn usd_precise(v: Option<f64>, reason: &str) -> String {
    let Some(v) = v else { return format!("\u{2014} {reason}") };
    if v == 0.0 {
        return "$0.00".to_string();
    }
    // enough decimals for ~3 significant figures: v=0.0123 (order -2) -> 4 decimals -> "$0.0123";
    // v=9.21 (order 0) -> 2 decimals -> "$9.21"
    let order = v.abs().log10().floor() as i32;
    let decimals = (2 - order).clamp(2, 6) as usize;
    format!("${v:.decimals$}")
}

/// The $/kWh rate itself, at the tariff's own precision (a real utility's table can publish 5 decimal
/// places; 4 is enough on screen to tell two nearby periods apart without a long string).
pub fn usd_per_kwh(v: Option<f64>) -> String {
    v.map_or_else(|| "\u{2014}".to_string(), |v| format!("${v:.4}/kWh"))
}

/// #102, 2026-09-22 (verifier): "today $0.02 (since local midnight)" at 03:16 on a collector
/// that started at 02:41 reads as a full 3-hour figure when it is really 35 minutes of data - a
/// plausible, confidently WRONG number under an honest-sounding label. Empty string when
/// coverage is at least 95% of the window's own nominal length (not worth mentioning); otherwise
/// a plain caveat naming how much data is actually behind the figure.
pub fn coverage_note(covered_secs: Option<i64>, nominal_secs: i64) -> String {
    match covered_secs {
        Some(c) if nominal_secs > 0 && (c as f64) < nominal_secs as f64 * 0.95 => format!("only {} of data so far, not the full window", fmt_duration(c)),
        _ => String::new(),
    }
}

