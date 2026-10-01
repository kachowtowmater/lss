//! Card #516: countries and currencies for the cost wizard's non-US path. The US keeps its own
//! embedded EIA state table (`cost_setup` / `cost_tables`); everywhere else the person types the
//! price per kWh off their own bill, in their own currency, as a plain decimal (0.25 = 25 cents
//! of whatever the bill is in). lss keeps printing a `$` sign - the number is whatever the person
//! typed - so the COUNTRY and CURRENCY are recorded in rates.toml next to the rate as provenance.
//!
//! Deliberately a small, self-contained module and deliberately SHORT: the point is catching the
//! spellings people actually type, not shipping a gazetteer. Anything unknown is still accepted
//! as typed, because the country is provenance, never a decision. No country AVERAGE table: the
//! only open-licence one we know of is a follow-up card, and a commercial one may not ship.

/// The English name a person types for an ISO-2 code. `None` = code only. Kept to one row per
/// line so a missing country is one inserted line.
pub fn place_name_country(code: &str) -> Option<&'static str> {
    Some(match code {
        "AT" => "Austria", "AU" => "Australia", "BE" => "Belgium", "BR" => "Brazil", "CA" => "Canada",
        "CH" => "Switzerland", "CL" => "Chile", "CN" => "China", "CZ" => "Czechia", "DE" => "Germany",
        "DK" => "Denmark", "EE" => "Estonia", "ES" => "Spain", "FI" => "Finland", "FR" => "France",
        "GB" => "United Kingdom", "GR" => "Greece", "HK" => "Hong Kong", "HR" => "Croatia",
        "HU" => "Hungary", "IE" => "Ireland", "IL" => "Israel", "IN" => "India", "IS" => "Iceland",
        "IT" => "Italy", "JP" => "Japan", "KR" => "South Korea", "LT" => "Lithuania", "LU" => "Luxembourg",
        "LV" => "Latvia", "MX" => "Mexico", "MY" => "Malaysia", "NL" => "Netherlands", "NO" => "Norway",
        "NZ" => "New Zealand", "PH" => "Philippines", "PL" => "Poland", "PT" => "Portugal", "RO" => "Romania",
        "SE" => "Sweden", "SG" => "Singapore", "SI" => "Slovenia", "SK" => "Slovakia", "TH" => "Thailand",
        "TR" => "Turkey", "TW" => "Taiwan", "UA" => "Ukraine", "VN" => "Vietnam", "ZA" => "South Africa",
        _ => return None,
    })
}

/// The listed country codes, for the matcher loop (`place_name_country`'s keys).
fn place_name_countries() -> impl Iterator<Item = &'static str> {
    ["AT", "AU", "BE", "BR", "CA", "CH", "CL", "CN", "CZ", "DE", "DK", "EE", "ES", "FI", "FR", "GB",
     "GR", "HK", "HR", "HU", "IE", "IL", "IN", "IS", "IT", "JP", "KR", "LT", "LU", "LV", "MX", "MY",
     "NL", "NO", "NZ", "PH", "PL", "PT", "RO", "SE", "SG", "SI", "SK", "TH", "TR", "TW", "UA", "VN", "ZA"]
        .into_iter()
}

/// The currency a rate in `code` is most likely quoted in (ISO 4217), or `None` when it is not
/// obvious from the country alone - the caller then just asks. One currency per country: where
/// several are in use this names the one a home utility bills in.
pub fn currency_of_country(code: &str) -> Option<&'static str> {
    Some(match code.to_ascii_uppercase().as_str() {
        "AT" | "BE" | "HR" | "EE" | "FI" | "FR" | "DE" | "GR" | "IE" | "IT" | "LV" | "LT" | "LU"
        | "NL" | "PT" | "SK" | "SI" | "ES" => "EUR",
        "AU" => "AUD", "BR" => "BRL", "CA" => "CAD", "CH" => "CHF", "CL" => "CLP", "CN" => "CNY",
        "CZ" => "CZK", "DK" => "DKK", "GB" => "GBP", "HK" => "HKD", "HU" => "HUF", "IL" => "ILS",
        "IN" => "INR", "IS" => "ISK", "JP" => "JPY", "KR" => "KRW", "MX" => "MXN", "MY" => "MYR",
        "NO" => "NOK", "NZ" => "NZD", "PH" => "PHP", "PL" => "PLN", "RO" => "RON", "SE" => "SEK",
        "SG" => "SGD", "TH" => "THB", "TR" => "TRY", "TW" => "TWD", "UA" => "UAH", "VN" => "VND",
        "ZA" => "ZAR",
        _ => return None,
    })
}

/// What the country question resolves to: `Us` runs the ZIP flow exactly as before; `Other`
/// carries the country as recorded (`Other("GB")` for "uk" too, uppercased codes).
#[derive(Debug, Clone, PartialEq)]
pub enum Country {
    Us,
    Other(String),
}

impl Country {
    /// The ISO-2 code, or `None` for the US (whose own flow never records a country - the EIA
    /// table's state code is the provenance it already has).
    pub fn code(&self) -> Option<&str> {
        match self {
            Country::Us => None,
            Country::Other(c) => Some(c),
        }
    }
}

/// Reads a country answer: empty or "US"/"usa"/"United States" = the ZIP flow; a listed name or
/// 2-letter code = that country; anything else is still accepted as typed. `Err` only for
/// something that is plainly a RATE typed at the country question - never for a spelling the
/// table does not know.
pub fn parse_country(input: &str) -> Result<Country, String> {
    let t = input.trim();
    if t.is_empty()
        || ["us", "usa", "u.s.", "u.s.a.", "united states", "united states of america", "america"]
            .iter().any(|n| t.eq_ignore_ascii_case(n))
    {
        return Ok(Country::Us);
    }
    if is_rate_like(t) {
        return Err(format!("'{t}' is not a country - type a country name or 2-letter code, then your rate"));
    }
    if t.eq_ignore_ascii_case("uk") {
        return Ok(Country::Other("GB".into()));
    }
    for code in place_name_countries() {
        if t.eq_ignore_ascii_case(code) || place_name_country(code).is_some_and(|n| n.eq_ignore_ascii_case(t)) {
            return Ok(Country::Other(code.to_string()));
        }
    }
    let up = t.to_ascii_uppercase();
    if up.len() == 2 && up.chars().all(|c| c.is_ascii_alphabetic()) {
        return Ok(Country::Other(up));
    }
    Ok(Country::Other(t.to_string()))
}

/// A bare 2-letter alphabetic token ("no", "ok", "de"): the shape a country CODE has, but also
/// the shape a stray word or a slip has. `parse_country` reads one as that code; the cost wizard
/// asks one confirmation on EVERY bare code - listed (NO) or not (XZ) alike - because 'no'
/// silently storing Norway is a wrong record, while 'Bolivia' kept as typed is only a spelling.
/// A 3+ letter word ("usa", "uk") is NOT bare, and neither is a US spelling ("us"): that answer
/// runs the ZIP flow, so it never names a country to confirm.
pub fn is_bare_two_letter_code(input: &str) -> bool {
    let t = input.trim();
    t.len() == 2
        && t.chars().all(|c| c.is_ascii_alphabetic())
        && !t.eq_ignore_ascii_case("us")
}

/// True when the input looks like a rate typed at the country question: starts with a digit,
/// `$` or `¢`, or carries a per-kWh unit.
fn is_rate_like(t: &str) -> bool {
    t.starts_with(|c: char| c.is_ascii_digit() || c == '$' || c == '\u{a2}')
        || t.to_ascii_lowercase().contains("/kwh")
}

/// Parses a price per kWh typed for a country outside the US, in the person's own currency:
/// "0.25", "0,25" (a comma decimal, as most of Europe writes it; "2,500" is still 2500), "€0.25", "0.25 EUR", "25c"
/// (cents of that currency). Unlike `cost_setup::parse_rate` there is NO bare-number-as-cents
/// reading and no dollar ceiling: 31 is 31 yen, 150 is 150 won, 8 is 8 rupees - every one of
/// them a real home rate. Only a real currency-cents marker ("c", "¢", "cent") divides by 100.
pub fn parse_typed_rate(input: &str) -> Result<f64, String> {
    let t = input.trim();
    let raw = t.to_ascii_lowercase();
    if raw.contains('-') {
        return Err("a rate must be more than zero".into());
    }
    let cents = raw.ends_with('c') || raw.contains('\u{a2}') || raw.contains("cent");
    let mut num: String = raw.chars().filter(|c| c.is_ascii_digit() || *c == '.' || *c == ',').collect();
    // one comma and no dot: a decimal comma ("0,25") unless exactly three digits follow it, the
    // one shape ("2,500") that is a thousands separator in every convention that uses commas
    if num.contains(',') && !num.contains('.') && num.matches(',').count() == 1
        && num.rsplit(',').next().is_some_and(|d| d.len() != 3)
    {
        num = num.replace(',', ".");
    }
    let v: f64 = num.replace(',', "").parse().map_err(|_| format!("'{t}' is not a rate - type the price per kWh from your bill as a plain number, like 0.25"))?;
    if !v.is_finite() || v <= 0.0 {
        return Err("a rate must be more than zero".into());
    }
    let v = if cents { v / 100.0 } else { v };
    if v > 100_000.0 {
        return Err(format!("{v} per kWh is far above any real home rate - type the price per kWh from your bill, like 0.25"));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_country_answer_reads_from_the_us_spellings_a_code_or_a_listed_name() {
        for us in ["", "US", "usa", "u.s.", "United States", "UNITED STATES OF AMERICA", "america"] {
            assert_eq!(parse_country(us), Ok(Country::Us), "{us}");
        }
        for (typed, want) in [("GB", "GB"), ("uk", "GB"), ("germany", "DE"), ("Germany", "DE"),
                              ("GERMANY", "DE"), ("de", "DE"), ("south korea", "KR"), ("canada", "CA")] {
            assert_eq!(parse_country(typed), Ok(Country::Other(want.into())), "{typed}");
        }
    }

    #[test]
    fn a_bare_two_letter_word_is_the_code_shape_not_necessarily_a_country() {
        // stray words people type at the country prompt, and real codes: every bare 2-letter
        // answer is the ambiguous shape the wizard confirms (listed NO, unlisted XZ alike)
        for t in ["no", "ok", "hi", "NO", "Ok", " de ", "me", "de", "DE", "gb", "uk", "nz"] {
            assert!(is_bare_two_letter_code(t), "{t}");
        }
        // everything that must pass unconfirmed: names, longer words, US spellings, rates
        for t in ["Germany", "Bolivia", "south korea", "usa", "us", "United States",
                  "0.25", "$0.25", "a", "abc", "d3", "3d", "", "  "] {
            assert!(!is_bare_two_letter_code(t), "{t}");
        }
    }

    #[test]
    fn an_unknown_country_is_kept_as_typed_and_a_rate_typed_early_is_refused() {
        assert_eq!(parse_country("Bolivia"), Ok(Country::Other("Bolivia".into())));
        assert_eq!(parse_country("nz"), Ok(Country::Other("NZ".into())));
        assert_eq!(parse_country("  India  "), Ok(Country::Other("IN".into())));
        assert!(parse_country("0.25").is_err());
        assert!(parse_country("$0.25").is_err());
        assert!(parse_country("31c").is_err(), "a rate typed early is refused, never read as a country");
    }

    #[test]
    fn currencies_are_inferred_only_where_obvious() {
        assert_eq!(currency_of_country("DE"), Some("EUR"));
        assert_eq!(currency_of_country("gb"), Some("GBP"));
        assert_eq!(currency_of_country("JP"), Some("JPY"));
        assert_eq!(currency_of_country("US"), None, "the US never takes this path");
        assert_eq!(currency_of_country("XX"), None, "unknown code: the wizard asks instead of guessing");
        assert_eq!(currency_of_country("Bolivia"), None);
    }

    #[test]
    fn a_typed_rate_is_read_in_the_persons_currency_with_no_cents_guess() {
        for (typed, want) in [("0.25", 0.25), ("0,25", 0.25), ("€0.25", 0.25), ("0.25 EUR", 0.25), ("£0.30", 0.30),
                              ("0,3", 0.3), ("31", 31.0), ("150", 150.0), ("8.5", 8.5), ("2,500", 2500.0), ("25c", 0.25), ("25¢", 0.25), ("25 cents", 0.25)] {
            assert_eq!(parse_typed_rate(typed), Ok(want), "{typed}");
        }
        for bad in ["", "abc", "0", "-0.25", "200000"] {
            assert!(parse_typed_rate(bad).is_err(), "{bad}");
        }
    }
}
