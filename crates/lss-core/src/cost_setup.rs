//! card #298: the pure half of the cost wizard (`lss-collector cost-setup`). No I/O, no network,
//! no clock - the collector's `cost_setup_run.rs` does the asking, the one optional HTTPS call and
//! the file write; everything that decides a NUMBER lives here, where a test can pin it.
//!
//! Three ways to a rate, all ending in the same flat `rates.toml` the collector already reads:
//!   1. a ZIP code -> its state (3-digit prefix table) -> that state's average residential price
//!      from the EMBEDDED, DATED EIA table in `cost_tables.rs` (no network, ever);
//!   2. the caller's IP -> a geolocation service's answer (parsed here, fetched elsewhere, only
//!      after explicit consent) -> the same state table;
//!   3. a $/kWh typed by hand.
//!
//! A state average is honest about being an average: its label says "avg" and names the EIA
//! month, and the written file says in words that a real bill (and any time-of-use plan) beats it.

use crate::cost_tables::{EIA_MONTH, EIA_SOURCE, EIA_URL, RESIDENTIAL_CENTS_PER_KWH, ZIP3_STATE};

/// USPS code -> the name a person reads. 50 states + DC, the territories and the military
/// "states" the ZIP table can return.
pub fn place_name(code: &str) -> Option<&'static str> {
    Some(match code {
        "AL" => "Alabama", "AK" => "Alaska", "AZ" => "Arizona", "AR" => "Arkansas", "CA" => "California",
        "CO" => "Colorado", "CT" => "Connecticut", "DE" => "Delaware", "DC" => "District of Columbia",
        "FL" => "Florida", "GA" => "Georgia", "HI" => "Hawaii", "ID" => "Idaho", "IL" => "Illinois",
        "IN" => "Indiana", "IA" => "Iowa", "KS" => "Kansas", "KY" => "Kentucky", "LA" => "Louisiana",
        "ME" => "Maine", "MD" => "Maryland", "MA" => "Massachusetts", "MI" => "Michigan", "MN" => "Minnesota",
        "MS" => "Mississippi", "MO" => "Missouri", "MT" => "Montana", "NE" => "Nebraska", "NV" => "Nevada",
        "NH" => "New Hampshire", "NJ" => "New Jersey", "NM" => "New Mexico", "NY" => "New York",
        "NC" => "North Carolina", "ND" => "North Dakota", "OH" => "Ohio", "OK" => "Oklahoma", "OR" => "Oregon",
        "PA" => "Pennsylvania", "RI" => "Rhode Island", "SC" => "South Carolina", "SD" => "South Dakota",
        "TN" => "Tennessee", "TX" => "Texas", "UT" => "Utah", "VT" => "Vermont", "VA" => "Virginia",
        "WA" => "Washington", "WV" => "West Virginia", "WI" => "Wisconsin", "WY" => "Wyoming",
        "PR" => "Puerto Rico", "VI" => "U.S. Virgin Islands", "GU" => "Guam", "AS" => "American Samoa",
        "MP" => "Northern Mariana Islands", "PW" => "Palau", "FM" => "Micronesia", "MH" => "Marshall Islands",
        "AA" => "a military (APO/FPO) address, Americas", "AE" => "a military (APO/FPO) address, Europe/Middle East/Africa",
        "AP" => "a military (APO/FPO) address, Pacific",
        _ => return None,
    })
}

/// A state's average residential price from the embedded EIA table, in $/kWh. `None` for
/// anything EIA's state table does not cover (territories, military addresses, unknown codes).
pub fn state_usd_per_kwh(code: &str) -> Option<f64> {
    if code == "US" {
        return None; // the national total is a row in the table, never a place someone lives
    }
    RESIDENTIAL_CENTS_PER_KWH.iter().find(|(c, _)| *c == code).map(|(_, cents)| cents / 100.0)
}

/// What a ZIP code resolved to.
#[derive(Debug, Clone, PartialEq)]
pub enum ZipLookup {
    /// a state (or DC) with an EIA average
    State { code: &'static str, usd_per_kwh: f64 },
    /// a real place EIA's state table does not cover (territory, military) - the caller asks for
    /// a rate by hand instead of borrowing some other state's number
    NotCovered { code: &'static str },
    /// well-formed, but no ZIP code uses this 3-digit prefix
    UnknownPrefix { prefix: String },
    /// not a ZIP code at all - the reason is written for a person
    Invalid { reason: String },
}

/// `input` = what someone typed: "02134", " 02134 ", "02134-1234", "021341234". Always handled
/// as TEXT - a ZIP code is not a number, and "02134" parsed as one loses the leading zero that
/// decides it is Massachusetts and not nowhere.
pub fn lookup_zip(input: &str) -> ZipLookup {
    let s: String = input.trim().chars().filter(|c| *c != '-' && *c != ' ').collect();
    if s.is_empty() {
        return ZipLookup::Invalid { reason: "no ZIP code was entered".into() };
    }
    if !s.chars().all(|c| c.is_ascii_digit()) {
        return ZipLookup::Invalid { reason: format!("'{}' is not a US ZIP code (digits only, like 02134)", input.trim()) };
    }
    if s.len() != 5 && s.len() != 9 {
        return ZipLookup::Invalid { reason: format!("a US ZIP code has 5 digits (or 5+4); '{}' has {}", input.trim(), s.len()) };
    }
    let five = &s[..5];
    // 5-digit overrides inside prefixes shared by several places - all of them territories EIA's
    // state table does not cover, so the only effect is naming the right place, never a rate.
    let over = match five {
        "96799" => Some("AS"),
        "96939" | "96940" => Some("PW"),
        "96941" | "96942" | "96943" | "96944" => Some("FM"),
        "96950" | "96951" | "96952" => Some("MP"),
        "96960" | "96970" => Some("MH"),
        _ => None,
    };
    let idx: usize = five[..3].parse().unwrap_or(0);
    let code = over.unwrap_or(ZIP3_STATE[idx]);
    if code.is_empty() {
        return ZipLookup::UnknownPrefix { prefix: five[..3].to_string() };
    }
    match state_usd_per_kwh(code) {
        Some(usd_per_kwh) => ZipLookup::State { code, usd_per_kwh },
        None => ZipLookup::NotCovered { code },
    }
}

/// A state as a geolocation service names it: "California", "california", "CA".
pub fn state_code_from_region(region: &str) -> Option<&'static str> {
    let r = region.trim();
    RESIDENTIAL_CENTS_PER_KWH
        .iter()
        .map(|(c, _)| *c)
        .filter(|c| *c != "US")
        .find(|c| c.eq_ignore_ascii_case(r) || place_name(c).is_some_and(|n| n.eq_ignore_ascii_case(r)))
}

/// Where an IP geolocation answer puts the caller.
#[derive(Debug, Clone, PartialEq)]
pub enum IpLocation {
    UsState { code: &'static str, usd_per_kwh: f64, city: String },
    /// in the US per the service, but in a place the state table does not cover
    UsNotCovered { region: String },
    /// outside the US: this table has nothing to offer - ask for a rate by hand
    NotUs { country: String },
}

/// The one geolocation service the wizard may call (only after the person says yes): a single
/// HTTPS GET, no key, no account. Named here so the consent prompt and the code cannot disagree.
pub const IP_SERVICE_NAME: &str = "ipinfo.io";
pub const IP_SERVICE_URL: &str = "https://ipinfo.io/json";

/// Parses ipinfo.io's `/json` answer (`{"ip":..,"city":..,"region":"California","country":"US",
/// "postal":"94103",..}`). The region NAME decides the state; the postal code is the fallback
/// when a region is missing or unrecognised. Anything that is not a readable answer is an Err
/// the caller turns into "type your ZIP instead".
pub fn parse_ipinfo(json: &str) -> Result<IpLocation, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("the location service's answer was not readable ({e})"))?;
    if let Some(err) = v.get("error") {
        return Err(format!("the location service refused: {err}"));
    }
    let country = v["country"].as_str().unwrap_or("").trim().to_string();
    if country.is_empty() {
        return Err("the location service did not say which country this is".into());
    }
    if !country.eq_ignore_ascii_case("US") {
        return Ok(IpLocation::NotUs { country });
    }
    let region = v["region"].as_str().unwrap_or("").to_string();
    let city = v["city"].as_str().unwrap_or("").to_string();
    if let Some(code) = state_code_from_region(&region) {
        if let Some(usd_per_kwh) = state_usd_per_kwh(code) {
            return Ok(IpLocation::UsState { code, usd_per_kwh, city });
        }
    }
    if let Some(ZipLookup::State { code, usd_per_kwh }) = v["postal"].as_str().map(lookup_zip) {
        return Ok(IpLocation::UsState { code, usd_per_kwh, city });
    }
    Ok(IpLocation::UsNotCovered { region })
}

/// Parses a typed rate: "0.31", "$0.31", "0.31/kWh", "31c", "31 cents", "31¢". A bare number
/// of 1 or more is read as CENTS only when it is clearly not dollars (>= 1 and <= 200) AND the
/// person did not write a "$" - no US residential rate is a dollar or more per kWh, and "31" is
/// what people type when a bill says 31¢. Anything else is refused with the reason.
pub fn parse_rate(input: &str) -> Result<f64, String> {
    let raw = input.trim().to_ascii_lowercase();
    if raw.contains('-') {
        return Err("a rate must be more than zero".into());
    }
    let dollars_sign = raw.contains('$');
    let cents_word = raw.contains('c') || raw.contains('\u{a2}');
    let num: String = raw.chars().filter(|c| c.is_ascii_digit() || *c == '.').collect();
    let v: f64 = num.parse().map_err(|_| format!("'{}' is not a rate - type it like 0.31 (dollars per kWh)", input.trim()))?;
    if !v.is_finite() || v <= 0.0 {
        return Err("a rate must be more than zero".into());
    }
    // cents when the person said so ("31c", "31¢"), or wrote a bare 1-200 with no "$"
    let is_cents = !dollars_sign && (cents_word || (1.0..=200.0).contains(&v));
    let usd = if is_cents { v / 100.0 } else { v };
    if usd > 2.0 {
        return Err(format!("${usd:.2}/kWh is far above any real residential rate - type it in dollars, like 0.31"));
    }
    if usd < 0.01 {
        return Err(format!("${usd:.4}/kWh is below any real residential rate - type it in dollars, like 0.31"));
    }
    Ok(usd)
}

/// card #332: `parse_rate` reads a BARE 1-200 as cents ("31" = 31c, the way a bill writes it).
/// That is right for 31 and silently wrong for "2.5" meant as something else: 2.5 cents =
/// $0.025/kWh, below almost any home rate. Some(the reading in words) when a bare number (no "$",
/// no "c"/"¢") was read as cents and lands below $0.05/kWh - the caller asks "are you sure?" (or,
/// with no one to ask, refuses). None for everything else, including an explicit "2.5c".
pub fn cents_doubt(input: &str) -> Option<String> {
    let t = input.trim();
    let raw = t.to_ascii_lowercase();
    if raw.contains('$') || raw.contains('c') || raw.contains('\u{a2}') {
        return None;
    }
    let v: f64 = raw.chars().filter(|c| c.is_ascii_digit() || *c == '.').collect::<String>().parse().ok()?;
    let usd = parse_rate(t).ok()?;
    let read_as_cents = (1.0..=200.0).contains(&v);
    (read_as_cents && usd < 0.05).then(|| {
        let shown = format!("{usd:.4}");
        let shown = shown.trim_end_matches('0');
        format!("'{t}' reads as {t} cents = ${shown}/kWh, below almost any home rate (most are 10 to 60 cents)")
    })
}

/// card #325: numbers that PASS `check_tou` but are very likely a slip - asked about once more
/// ("are you sure?"), never refused: off-peak dearer than on-peak (the other way round is what
/// makes it a time-of-use plan), a price above $2/kWh, or a fixed charge above $2 a day (the
/// verification typed 21 there by accident and it was written). Empty = nothing to ask.
pub fn tou_doubts(p: &TouPlan) -> Vec<String> {
    let mut out = Vec::new();
    if p.off_peak > p.on_peak {
        out.push(format!("off-peak (${}/kWh) is dearer than on-peak (${}/kWh) - usually it is the other way round", trim_money(p.off_peak), trim_money(p.on_peak)));
    }
    let mut prices = vec![("on-peak", p.on_peak), ("off-peak", p.off_peak), ("weekend/holiday", p.weekend_peak)];
    if let Some(s) = &p.seasons {
        prices.push(("winter on-peak", s.winter_peak));
    }
    for (name, v) in prices {
        if v > 2.0 {
            out.push(format!("the {name} price ${}/kWh is far above any residential rate", trim_money(v)));
        }
    }
    if p.fixed_usd_per_day > 2.0 {
        out.push(format!("a fixed charge of ${} a day is far above the usual (most are under $2 a day)", trim_money(p.fixed_usd_per_day)));
    }
    out
}

fn trim_money(v: f64) -> String {
    let t = format!("{v:.4}");
    let t = t.trim_end_matches('0');
    let t = t.strip_suffix('.').unwrap_or(t);
    if t.contains('.') && t.split('.').nth(1).is_some_and(|d| d.len() == 1) { format!("{t}0") } else { t.to_string() }
}

/// Where a written rate came from - everything `rates.toml` needs to say about it.
#[derive(Debug, Clone, PartialEq)]
pub enum RateOrigin {
    /// an EIA state average, reached via a ZIP code or the IP lookup (`how` says which, in words)
    StateAverage { code: &'static str, how: String },
    /// typed by hand
    Manual,
}

/// The short label lss prints after "rate:" - e.g. `CA avg (EIA 2026-06)` / `entered by hand
/// (2026-09-24)`. Kept short on purpose: it rides on every cost line.
pub fn source_label(origin: &RateOrigin, today: &str) -> String {
    match origin {
        RateOrigin::StateAverage { code, .. } => format!("{code} avg (EIA {EIA_MONTH})"),
        RateOrigin::Manual => format!("entered by hand ({today})"),
    }
}

/// The complete `rates.toml` - a FLAT plan (`kind = "flat"`) in exactly the shape
/// `rates::parse_rates_file` reads, plus the provenance fields (`source`, `source_detail`,
/// `source_url`) it also reads. `today` = "YYYY-MM-DD" (the caller's clock), `tool` = who wrote it.
pub fn render_rates_toml(usd_per_kwh: f64, origin: &RateOrigin, today: &str, tool: &str) -> String {
    let label = source_label(origin, today);
    let (name, effective, detail, url) = match origin {
        RateOrigin::StateAverage { code, how } => (
            format!("{} average residential (EIA)", place_name(code).unwrap_or(code)),
            EIA_MONTH.to_string(),
            format!("{EIA_SOURCE}, {} residential, {EIA_MONTH}; location from {how}", place_name(code).unwrap_or(code)),
            EIA_URL.to_string(),
        ),
        RateOrigin::Manual => ("Flat rate (entered by hand)".to_string(), today.to_string(), "typed by hand during setup".to_string(), String::new()),
    };
    let mut o = String::new();
    o.push_str(&format!("# lss electricity cost - written by {tool} on {today}.\n"));
    o.push_str("# This file is YOURS: re-run `lss-collector cost-setup` to change it, or edit it by hand\n");
    o.push_str("# (packaging/rates.toml.example shows a full time-of-use plan). Restart the collector after.\n");
    if matches!(origin, RateOrigin::StateAverage { .. }) {
        o.push_str("# A state AVERAGE is a starting point, not your bill: your own utility's rate - and any\n");
        o.push_str("# time-of-use plan, where peak hours can cost double - is more accurate. Replace it when you can.\n");
    }
    o.push_str(&format!("name = {}\n", toml_str(&name)));
    o.push_str(&format!("effective_date = {}\n", toml_str(&effective)));
    o.push_str(&format!("source = {}\n", toml_str(&label)));
    o.push_str(&format!("source_detail = {}\n", toml_str(&detail)));
    if !url.is_empty() {
        o.push_str(&format!("source_url = {}\n", toml_str(&url)));
    }
    o.push_str("\n[plan]\nkind = \"flat\"\n");
    o.push_str(&format!("usd_per_kwh = {}\n", fmt_rate(usd_per_kwh)));
    o
}

/// Enough digits to round-trip a typed or EIA rate exactly (EIA gives cents to 2 places).
fn fmt_rate(v: f64) -> String {
    let s = format!("{v:.6}");
    let s = s.trim_end_matches('0');
    if s.ends_with('.') { format!("{s}0") } else { s.to_string() }
}

fn toml_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if c.is_control() => o.push_str(&format!("\\u{:04X}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

// ------------------------------------------------------------------ card #313: a typed time-of-use plan

/// A time-of-use plan typed during setup: the shape most utility TOU plans have - one peak window
/// on weekdays, its own price on weekends/holidays in the same hours, off-peak the rest - optionally
/// split by season. Rendered into `packaging/rates.toml.example`'s time-of-use shape.
#[derive(Debug, Clone, PartialEq)]
pub struct TouPlan {
    pub on_peak: f64,
    /// local hour the peak starts (0-23, inclusive) and ends (1-24, exclusive)
    pub peak_start: u32,
    pub peak_end: u32,
    pub off_peak: f64,
    /// weekends and holidays, during the peak hours
    pub weekend_peak: f64,
    pub seasons: Option<Seasons>,
    /// 0 = none
    pub fixed_usd_per_day: f64,
}

/// Summer = months [summer_start, summer_end); winter = the rest, with its own peak price.
#[derive(Debug, Clone, PartialEq)]
pub struct Seasons {
    pub summer_start: u32,
    pub summer_end: u32,
    pub winter_peak: f64,
}

/// Is the plan one lss can price every hour of? `Err` = a sentence the person can act on.
pub fn check_tou(p: &TouPlan) -> Result<(), String> {
    let price = |v: f64, what: &str| if v.is_finite() && v > 0.0 && v < 10.0 { Ok(()) } else { Err(format!("the {what} price must be a $/kWh above 0 and below 10 (got {v})")) };
    price(p.on_peak, "on-peak")?;
    price(p.off_peak, "off-peak")?;
    price(p.weekend_peak, "weekend/holiday")?;
    if p.peak_start > 23 || p.peak_end == 0 || p.peak_end > 24 {
        return Err(format!("hours are 0-23 for the start and 1-24 for the end (got {}-{})", p.peak_start, p.peak_end));
    }
    if p.peak_start >= p.peak_end {
        return Err(format!("the peak must end after it starts on the same day (got {}:00 to {}:00); a window across midnight is not supported here - edit rates.toml by hand for that", p.peak_start, p.peak_end));
    }
    if let Some(s) = &p.seasons {
        price(s.winter_peak, "winter on-peak")?;
        if !(1..=12).contains(&s.summer_start) || !(1..=12).contains(&s.summer_end) || s.summer_start >= s.summer_end {
            return Err(format!("summer runs from its first month up to (not including) its last, both 1-12, first < last (got {}-{})", s.summer_start, s.summer_end));
        }
    }
    if !p.fixed_usd_per_day.is_finite() || p.fixed_usd_per_day < 0.0 || p.fixed_usd_per_day >= 100.0 {
        return Err(format!("the fixed daily charge must be 0 or more dollars a day (got {})", p.fixed_usd_per_day));
    }
    Ok(())
}

/// Where a typed TOU plan came from, as `status.cost` shows it.
pub fn tou_source_label(today: &str) -> String {
    format!("time-of-use, entered by hand ({today})")
}

/// The complete `rates.toml` for a typed TOU plan, in `packaging/rates.toml.example`'s
/// time-of-use shape: every hour of every day is covered by exactly one period.
pub fn render_tou_toml(p: &TouPlan, today: &str, tool: &str) -> String {
    let (a, b) = (p.peak_start, p.peak_end);
    let summer = if p.seasons.is_some() { "summer" } else { "any" };
    let mut periods: Vec<(String, f64, &str, &str, u32, u32)> = Vec::new();
    let mut span = |name: &str, usd: f64, season: &'static str, day: &'static str, from: u32, to: u32| {
        if from < to {
            periods.push((name.to_string(), usd, season, day, from, to));
        }
    };
    let pre = if p.seasons.is_some() { "summer_" } else { "" };
    span(&format!("{pre}on_peak"), p.on_peak, summer, "weekday", a, b);
    span(&format!("{pre}off_peak"), p.off_peak, summer, "weekday", 0, a);
    span(&format!("{pre}off_peak"), p.off_peak, summer, "weekday", b, 24);
    span(&format!("{pre}weekend_peak"), p.weekend_peak, summer, "weekend_or_holiday", a, b);
    span(&format!("{pre}off_peak"), p.off_peak, summer, "weekend_or_holiday", 0, a);
    span(&format!("{pre}off_peak"), p.off_peak, summer, "weekend_or_holiday", b, 24);
    if let Some(s) = &p.seasons {
        span("winter_on_peak", s.winter_peak, "winter", "any", a, b);
        span("winter_off_peak", p.off_peak, "winter", "any", 0, a);
        span("winter_off_peak", p.off_peak, "winter", "any", b, 24);
    }
    let (sm, em) = p.seasons.as_ref().map_or((6, 10), |s| (s.summer_start, s.summer_end));
    let mut o = String::new();
    o.push_str(&format!("# lss electricity cost - a time-of-use plan typed into {tool} on {today}.\n"));
    o.push_str("# This file is YOURS: re-run `lss-collector cost-setup` to change it, or edit it by hand\n");
    o.push_str("# (packaging/rates.toml.example shows every key). Restart the collector after.\n");
    o.push_str("name = \"Time-of-use (entered by hand)\"\n");
    o.push_str(&format!("effective_date = {}\n", toml_str(today)));
    o.push_str(&format!("source = {}\n", toml_str(&tou_source_label(today))));
    o.push_str(&format!("source_detail = {}\n", toml_str(&format!("typed by hand during setup: on-peak {}-{}h weekdays", a, b))));
    if p.fixed_usd_per_day > 0.0 {
        o.push_str(&format!("fixed_usd_per_day = {}\n", fmt_rate(p.fixed_usd_per_day)));
    }
    o.push_str("\n[plan]\nkind = \"time_of_use\"\n");
    if p.seasons.is_none() {
        o.push_str("# no seasons: every period below is season = \"any\", so these two months are not used\n");
    }
    o.push_str(&format!("summer_start_month = {sm}\nsummer_end_month = {em}\n"));
    o.push_str("holidays = []  # your utility's own holiday dates (\"YYYY-MM-DD\"): weekend prices apply on them\n");
    for (name, usd, season, day, from, to) in &periods {
        o.push_str(&format!("\n[[plan.periods]]\nname = {}\nusd_per_kwh = {}\nseason = \"{season}\"\nday_kind = \"{day}\"\nstart_hour = {from}\nend_hour = {to}\n", toml_str(name), fmt_rate(*usd)));
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rates::parse_rates_file;

    #[test]
    fn a_plan_that_looks_like_a_slip_is_doubted_not_refused() {
        // card #325
        let base = TouPlan { on_peak: 0.52, peak_start: 16, peak_end: 21, off_peak: 0.31, weekend_peak: 0.40, seasons: None, fixed_usd_per_day: 0.25 };
        assert!(tou_doubts(&base).is_empty() && check_tou(&base).is_ok());
        let flipped = TouPlan { off_peak: 0.60, ..base.clone() };
        assert!(check_tou(&flipped).is_ok(), "not refused");
        assert_eq!(tou_doubts(&flipped), vec!["off-peak ($0.60/kWh) is dearer than on-peak ($0.52/kWh) - usually it is the other way round".to_string()]);
        let daily = TouPlan { fixed_usd_per_day: 21.0, ..base.clone() };
        assert_eq!(tou_doubts(&daily), vec!["a fixed charge of $21 a day is far above the usual (most are under $2 a day)".to_string()]);
        let winter = TouPlan { seasons: Some(Seasons { summer_start: 6, summer_end: 10, winter_peak: 2.5 }), ..base };
        assert!(tou_doubts(&winter)[0].contains("winter on-peak price $2.50/kWh"), "{:?}", tou_doubts(&winter));
    }

    #[test]
    fn leading_zero_zips_keep_their_state() {
        // "02134" as a NUMBER is 2134 -> prefix 021 survives only if the text is never parsed whole
        assert!(matches!(lookup_zip("02134"), ZipLookup::State { code: "MA", .. }));
        assert!(matches!(lookup_zip("00501"), ZipLookup::State { code: "NY", .. }), "005 = Holtsville NY (IRS), a PO-only prefix from GeoNames");
        assert!(matches!(lookup_zip("06103"), ZipLookup::State { code: "CT", .. }));
        assert!(matches!(lookup_zip("07030"), ZipLookup::State { code: "NJ", .. }));
        assert!(matches!(lookup_zip("05401"), ZipLookup::State { code: "VT", .. }));
    }

    #[test]
    fn well_known_zips_resolve_to_the_right_state_and_eia_price() {
        for (zip, st) in [("94103", "CA"), ("10001", "NY"), ("60601", "IL"), ("73301", "TX"), ("75201", "TX"), ("98101", "WA"),
            ("99501", "AK"), ("96813", "HI"), ("20001", "DC"), ("20500", "DC"), ("22201", "VA"), ("33101", "FL"), ("83702", "ID"),
            ("59601", "MT"), ("88901", "NV"), ("84101", "UT"), ("87501", "NM"), ("58501", "ND"), ("57501", "SD"), ("03101", "NH"), ("04101", "ME")] {
            match lookup_zip(zip) {
                ZipLookup::State { code, usd_per_kwh } => {
                    assert_eq!(code, st, "{zip}");
                    assert!((0.05..0.70).contains(&usd_per_kwh), "{zip}: {usd_per_kwh}");
                }
                other => panic!("{zip}: {other:?}"),
            }
        }
    }

    #[test]
    fn eia_values_are_the_published_june_2026_residential_column() {
        // spot-checked by hand against Table 5.6.A (June 2026, released 2026-08-26), cents/kWh
        assert_eq!(EIA_MONTH, "2026-06");
        for (st, cents) in [("CA", 34.74), ("HI", 52.72), ("TX", 15.94), ("NY", 29.49), ("ND", 14.12), ("DC", 24.39), ("WA", 14.91)] {
            let got = state_usd_per_kwh(st).unwrap();
            assert!((got - cents / 100.0).abs() < 1e-12, "{st}: {got}");
        }
        let us = RESIDENTIAL_CENTS_PER_KWH.iter().find(|(c, _)| *c == "US").unwrap().1;
        assert_eq!(us, 18.34);
        assert_eq!(state_usd_per_kwh("US"), None, "the national total is never a place");
        assert_eq!(RESIDENTIAL_CENTS_PER_KWH.len(), 52, "50 states + DC + US total");
        for (c, _) in RESIDENTIAL_CENTS_PER_KWH.iter().filter(|(c, _)| *c != "US") {
            assert!(place_name(c).is_some(), "{c} has no name");
        }
    }

    #[test]
    fn every_state_and_dc_is_reachable_from_some_zip_prefix() {
        for (c, _) in RESIDENTIAL_CENTS_PER_KWH.iter().filter(|(c, _)| *c != "US") {
            assert!(ZIP3_STATE.contains(c), "no prefix maps to {c}");
        }
        for code in ZIP3_STATE.iter().filter(|c| !c.is_empty()) {
            assert!(place_name(code).is_some(), "prefix table holds unknown code {code}");
        }
    }

    #[test]
    fn territories_and_military_are_not_covered_never_borrowed_from_a_state() {
        for (zip, code) in [("00901", "PR"), ("00602", "PR"), ("00802", "VI"), ("96910", "GU"), ("96950", "MP"), ("96799", "AS"),
            ("96940", "PW"), ("96941", "FM"), ("96960", "MH"), ("09001", "AE"), ("34001", "AA"), ("96201", "AP"), ("96601", "AP")] {
            assert_eq!(lookup_zip(zip), ZipLookup::NotCovered { code }, "{zip}");
        }
        // 967xx is Hawaii EXCEPT 96799 (American Samoa)
        assert!(matches!(lookup_zip("96701"), ZipLookup::State { code: "HI", .. }));
    }

    #[test]
    fn invalid_and_unused_input_is_refused_with_a_reason() {
        assert!(matches!(lookup_zip(""), ZipLookup::Invalid { .. }));
        assert!(matches!(lookup_zip("   "), ZipLookup::Invalid { .. }));
        assert!(matches!(lookup_zip("9410"), ZipLookup::Invalid { .. }), "4 digits");
        assert!(matches!(lookup_zip("941033"), ZipLookup::Invalid { .. }), "6 digits");
        assert!(matches!(lookup_zip("abcde"), ZipLookup::Invalid { .. }));
        assert!(matches!(lookup_zip("SW1A 1AA"), ZipLookup::Invalid { .. }), "a UK postcode");
        assert!(matches!(lookup_zip("K1A0B1"), ZipLookup::Invalid { .. }), "a Canadian postcode");
        assert!(matches!(lookup_zip("９４１０３"), ZipLookup::Invalid { .. }), "full-width digits are not ASCII digits");
        assert_eq!(lookup_zip("00012"), ZipLookup::UnknownPrefix { prefix: "000".into() });
        assert_eq!(lookup_zip("00312"), ZipLookup::UnknownPrefix { prefix: "003".into() });
        assert_eq!(lookup_zip("21301"), ZipLookup::UnknownPrefix { prefix: "213".into() });
        // ZIP+4 and stray whitespace are fine
        assert!(matches!(lookup_zip(" 94103-1234 "), ZipLookup::State { code: "CA", .. }));
        assert!(matches!(lookup_zip("941031234"), ZipLookup::State { code: "CA", .. }));
    }

    #[test]
    fn ipinfo_answers_parse_to_a_state_a_foreign_country_or_an_error() {
        let ca = r#"{"ip":"203.0.113.9","city":"San Francisco","region":"California","country":"US","postal":"94103"}"#;
        assert!(matches!(parse_ipinfo(ca), Ok(IpLocation::UsState { code: "CA", .. })));
        let dc = r#"{"city":"Washington","region":"District of Columbia","country":"US"}"#;
        assert!(matches!(parse_ipinfo(dc), Ok(IpLocation::UsState { code: "DC", .. })));
        let postal_only = r#"{"region":"","country":"US","postal":"02134"}"#;
        assert!(matches!(parse_ipinfo(postal_only), Ok(IpLocation::UsState { code: "MA", .. })));
        let pr = r#"{"region":"Puerto Rico","country":"US","postal":"00901"}"#;
        assert_eq!(parse_ipinfo(pr), Ok(IpLocation::UsNotCovered { region: "Puerto Rico".into() }));
        let de = r#"{"city":"Berlin","region":"Berlin","country":"DE"}"#;
        assert_eq!(parse_ipinfo(de), Ok(IpLocation::NotUs { country: "DE".into() }));
        assert!(parse_ipinfo("<html>rate limited</html>").is_err());
        assert!(parse_ipinfo(r#"{"error":{"title":"Rate limit exceeded"}}"#).is_err());
        assert!(parse_ipinfo(r#"{"ip":"203.0.113.9"}"#).is_err(), "no country = no answer");
    }

    #[test]
    fn typed_rates_read_as_dollars_or_cents_and_absurd_ones_are_refused() {
        assert_eq!(parse_rate("0.31"), Ok(0.31));
        assert_eq!(parse_rate("$0.31"), Ok(0.31));
        assert_eq!(parse_rate("0.31/kWh"), Ok(0.31));
        assert_eq!(parse_rate(" .185 "), Ok(0.185));
        assert_eq!(parse_rate("31c"), Ok(0.31));
        assert_eq!(parse_rate("31 cents"), Ok(0.31));
        assert_eq!(parse_rate("31"), Ok(0.31), "a bare 31 is what a bill's 31¢ looks like");
        assert_eq!(parse_rate("12.5¢"), Ok(0.125));
        assert!(parse_rate("$31").is_err(), "$31/kWh is not a residential rate");
        assert!(parse_rate("0").is_err());
        assert!(parse_rate("-0.2").is_err(), "a negative rate is refused, not silently made positive");
        assert!(parse_rate("abc").is_err());
        assert!(parse_rate("").is_err());
        assert!(parse_rate("0.001").is_err());
        assert!(parse_rate("500").is_err());
    }

    #[test]
    fn a_bare_number_read_as_a_sub_five_cent_rate_is_said_back_in_words() {
        // card #332: "2.5" is read as 2.5 CENTS (a bare 1-200 is cents, the way a bill writes it),
        // so it became $0.025/kWh without a word - below almost any home rate
        let d = cents_doubt("2.5").expect("2.5 -> 2.5 cents is doubted");
        assert_eq!(d, "'2.5' reads as 2.5 cents = $0.025/kWh, below almost any home rate (most are 10 to 60 cents)");
        assert!(cents_doubt(" 4.99 ").is_some(), "just under 5 cents");
        assert!(cents_doubt("1").is_some(), "1 = 1 cent");
        // not doubted: 5 cents and up, anything said explicitly, anything read as dollars
        for ok in ["5", "31", "0.31", "0.025", "2.5c", "2.5 cents", "2.5\u{a2}", "$0.03", "0.5", "abc", ""] {
            assert_eq!(cents_doubt(ok), None, "{ok}");
        }
    }

    #[test]
    fn written_file_round_trips_through_the_collectors_own_parser() {
        let origin = RateOrigin::StateAverage { code: "CA", how: "ZIP 94103".into() };
        let text = render_rates_toml(0.3474, &origin, "2026-09-24", "lss-collector cost-setup 1.1.2");
        let t = parse_rates_file(&text).expect("the collector must read what the wizard writes");
        assert!(t.is_flat());
        assert_eq!(t.name, "California average residential (EIA)");
        assert_eq!(t.effective_date, "2026-06");
        assert_eq!(t.source.as_deref(), Some("CA avg (EIA 2026-06)"));
        assert_eq!(t.price_for(crate::rates::DayContext { hour: 12, is_summer: true, is_weekend_or_holiday: false }), Some((0.3474, "flat")));
        assert!(text.contains("source_url = \"https://www.eia.gov/"));
        assert!(text.contains("ZIP 94103"));

        let text = render_rates_toml(0.185, &RateOrigin::Manual, "2026-09-24", "t");
        let t = parse_rates_file(&text).unwrap();
        assert_eq!(t.source.as_deref(), Some("entered by hand (2026-09-24)"));
        assert_eq!(t.effective_date, "2026-09-24");
        assert!(!text.contains("source_url"));
    }

    #[test]
    fn quotes_in_a_location_cannot_break_the_toml() {
        let origin = RateOrigin::StateAverage { code: "NY", how: "IP lookup (\"New York\" \\ city)\nx".into() };
        let text = render_rates_toml(0.2949, &origin, "2026-09-24", "t");
        assert!(parse_rates_file(&text).is_ok(), "{text}");
    }

    // ---------------------------------------------------------- card #313
    fn tou(seasons: bool) -> TouPlan {
        TouPlan { on_peak: 0.52, peak_start: 16, peak_end: 21, off_peak: 0.31, weekend_peak: 0.40, seasons: seasons.then_some(Seasons { summer_start: 6, summer_end: 10, winter_peak: 0.45 }), fixed_usd_per_day: 0.25 }
    }

    fn price(table: &crate::rates::RateTable, hour: u32, summer: bool, weekend: bool) -> (f64, String) {
        let (v, n) = table.price_for(crate::rates::DayContext { hour, is_summer: summer, is_weekend_or_holiday: weekend }).unwrap_or_else(|| panic!("no period covers hour {hour} summer={summer} weekend={weekend}"));
        (v, n.to_string())
    }

    #[test]
    fn a_typed_tou_plan_is_a_rates_file_the_collector_prices_every_hour_of() {
        for seasons in [false, true] {
            let text = render_tou_toml(&tou(seasons), "2026-09-24", "test");
            let t = crate::rates::parse_rates_file(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
            assert!(!t.is_flat());
            assert_eq!(t.fixed_usd_per_day, Some(0.25));
            assert_eq!(t.source.as_deref(), Some("time-of-use, entered by hand (2026-09-24)"));
            // every hour of every kind of day is priced (no gap an hour could fall through)
            for summer in [true, false] {
                for weekend in [true, false] {
                    for h in 0..24 {
                        price(&t, h, summer, weekend);
                    }
                }
            }
            assert_eq!(price(&t, 16, true, false).0, 0.52, "weekday peak starts at 16");
            assert_eq!(price(&t, 20, true, false).0, 0.52);
            assert_eq!(price(&t, 21, true, false).0, 0.31, "and ends before 21");
            assert_eq!(price(&t, 15, true, false).0, 0.31);
            assert_eq!(price(&t, 18, true, true).0, 0.40, "weekend/holiday in the peak hours");
            assert_eq!(price(&t, 3, true, true).0, 0.31);
            let winter_peak = price(&t, 18, false, false).0;
            assert_eq!(winter_peak, if seasons { 0.45 } else { 0.52 }, "seasons={seasons}");
        }
    }

    #[test]
    fn a_peak_at_the_edge_of_the_day_leaves_no_empty_period() {
        let p = TouPlan { peak_start: 0, peak_end: 24, ..tou(false) };
        let t = crate::rates::parse_rates_file(&render_tou_toml(&p, "2026-09-24", "t")).unwrap();
        assert_eq!(price(&t, 0, true, false).0, 0.52);
        assert_eq!(price(&t, 23, true, false).0, 0.52);
        assert!(!render_tou_toml(&p, "d", "t").contains("start_hour = 0\nend_hour = 0"), "no zero-length period");
    }

    #[test]
    fn a_plan_that_cannot_be_priced_is_refused_in_words() {
        assert!(check_tou(&tou(true)).is_ok());
        for (p, says) in [
            (TouPlan { peak_start: 21, peak_end: 16, ..tou(false) }, "across midnight"),
            (TouPlan { peak_start: 24, peak_end: 24, ..tou(false) }, "0-23"),
            (TouPlan { peak_end: 25, ..tou(false) }, "1-24"),
            (TouPlan { on_peak: 0.0, ..tou(false) }, "on-peak"),
            (TouPlan { off_peak: f64::NAN, ..tou(false) }, "off-peak"),
            (TouPlan { fixed_usd_per_day: -1.0, ..tou(false) }, "fixed daily"),
            (TouPlan { seasons: Some(Seasons { summer_start: 10, summer_end: 6, winter_peak: 0.4 }), ..tou(false) }, "summer runs"),
            (TouPlan { seasons: Some(Seasons { summer_start: 6, summer_end: 10, winter_peak: 0.0 }), ..tou(false) }, "winter on-peak"),
        ] {
            let e = check_tou(&p).expect_err(says);
            assert!(e.contains(says), "{e}");
        }
    }
}
