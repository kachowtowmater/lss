//! Temperatures in both units. Every temperature a person reads goes through here:
//! `47°C / 117°F`, or `47C/117F` where the room is tight. `temp_units = "both" | "c" | "f"`
//! (both configs) picks; JSON always keeps Celsius and adds a `_f` twin.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TempUnits {
    #[default]
    Both,
    C,
    F,
}

impl TempUnits {
    /// `""` and anything unknown = both (the default), so a typo never hides a temperature.
    pub fn parse(text: &str) -> TempUnits {
        match text.trim().to_ascii_lowercase().as_str() {
            "c" | "celsius" => TempUnits::C,
            "f" | "fahrenheit" => TempUnits::F,
            _ => TempUnits::Both,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            TempUnits::Both => "both",
            TempUnits::C => "c",
            TempUnits::F => "f",
        }
    }
}

static UNITS: AtomicU8 = AtomicU8::new(0);

/// Set once at start-up from the config (`temp_units`).
pub fn set_temp_units(u: TempUnits) {
    UNITS.store(u as u8, Ordering::Relaxed);
}

pub fn temp_units() -> TempUnits {
    match UNITS.load(Ordering::Relaxed) {
        1 => TempUnits::C,
        2 => TempUnits::F,
        _ => TempUnits::Both,
    }
}

pub fn c_to_f(c: f64) -> f64 {
    c * 9.0 / 5.0 + 32.0
}

/// `_f` twin of a Celsius JSON field, rounded to one decimal.
pub fn c_to_f_round(c: f64) -> f64 {
    (c_to_f(c) * 10.0).round() / 10.0
}

/// How much room a temperature gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TempStyle {
    /// `47°C / 117°F`
    Full,
    /// `47C/117F`
    Compact,
    /// `47/117` - only where a caption next to it says `C/F` (the narrowest GPU cards)
    Bare,
}

pub fn fmt_temp_with(c: f64, units: TempUnits, style: TempStyle) -> String {
    let f = c_to_f(c);
    match (units, style) {
        (TempUnits::Both, TempStyle::Full) => format!("{c:.0}°C / {f:.0}°F"),
        (TempUnits::Both, TempStyle::Compact) => format!("{c:.0}C/{f:.0}F"),
        (TempUnits::Both, TempStyle::Bare) => format!("{c:.0}/{f:.0}"),
        (TempUnits::C, TempStyle::Full) => format!("{c:.0}°C"),
        (TempUnits::C, TempStyle::Compact) => format!("{c:.0}C"),
        (TempUnits::C, TempStyle::Bare) => format!("{c:.0}"),
        (TempUnits::F, TempStyle::Full) => format!("{f:.0}°F"),
        (TempUnits::F, TempStyle::Compact) => format!("{f:.0}F"),
        (TempUnits::F, TempStyle::Bare) => format!("{f:.0}"),
    }
}

/// `47°C / 117°F` in the configured units.
pub fn temp(c: f64) -> String {
    fmt_temp_with(c, temp_units(), TempStyle::Full)
}

/// `47C/117F` in the configured units.
pub fn temp_compact(c: f64) -> String {
    fmt_temp_with(c, temp_units(), TempStyle::Compact)
}

/// `-` for a reading that is missing.
pub fn temp_opt(c: Option<f64>, style: TempStyle) -> String {
    c.map_or_else(|| "-".to_string(), |c| fmt_temp_with(c, temp_units(), style))
}

/// The widest form that fits `width` columns: full, compact, then bare.
pub fn temp_fit(c: Option<f64>, width: usize) -> String {
    let Some(c) = c else { return "-".into() };
    for style in [TempStyle::Full, TempStyle::Compact, TempStyle::Bare] {
        let s = fmt_temp_with(c, temp_units(), style);
        if s.chars().count() <= width || style == TempStyle::Bare {
            return s;
        }
    }
    unreachable!()
}

/// The caption that goes with `TempStyle::Bare` numbers: `C/F`, `C` or `F`.
pub fn unit_caption() -> &'static str {
    match temp_units() {
        TempUnits::Both => "C/F",
        TempUnits::C => "C",
        TempUnits::F => "F",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_units_everywhere_and_each_alone() {
        assert_eq!(fmt_temp_with(47.0, TempUnits::Both, TempStyle::Full), "47°C / 117°F");
        assert_eq!(fmt_temp_with(47.0, TempUnits::Both, TempStyle::Compact), "47C/117F");
        assert_eq!(fmt_temp_with(47.0, TempUnits::Both, TempStyle::Bare), "47/117");
        assert_eq!(fmt_temp_with(90.0, TempUnits::C, TempStyle::Full), "90°C");
        assert_eq!(fmt_temp_with(90.0, TempUnits::F, TempStyle::Full), "194°F");
        assert_eq!(fmt_temp_with(90.0, TempUnits::F, TempStyle::Compact), "194F");
        assert_eq!(fmt_temp_with(-40.0, TempUnits::Both, TempStyle::Compact), "-40C/-40F");
        assert_eq!((c_to_f(0.0), c_to_f(100.0), c_to_f_round(36.6)), (32.0, 212.0, 97.9));
    }

    #[test]
    fn units_parse_and_default_to_both() {
        assert_eq!(TempUnits::parse(""), TempUnits::Both);
        assert_eq!(TempUnits::parse("both"), TempUnits::Both);
        assert_eq!(TempUnits::parse(" C "), TempUnits::C);
        assert_eq!(TempUnits::parse("f"), TempUnits::F);
        assert_eq!(TempUnits::parse("kelvin"), TempUnits::Both, "a typo never hides a temperature");
        assert_eq!(TempUnits::default().name(), "both");
    }

    #[test]
    fn the_widest_form_that_fits() {
        // (the process-wide setting is `both` unless a binary changed it)
        assert_eq!(temp_fit(Some(52.0), 20), "52°C / 126°F");
        assert_eq!(temp_fit(Some(52.0), 9), "52C/126F");
        assert_eq!(temp_fit(Some(52.0), 6), "52/126");
        assert_eq!(temp_fit(None, 6), "-");
    }
}
