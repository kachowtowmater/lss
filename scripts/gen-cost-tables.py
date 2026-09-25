#!/usr/bin/env python3
"""card #298: regenerate crates/lss-core/src/cost_tables.rs - the EMBEDDED, DATED tables the cost
wizard (`lss-collector cost-setup`) prices a ZIP code with, offline.

    scripts/gen-cost-tables.py                 # download the three sources, write the .rs file
    scripts/gen-cost-tables.py --from DIR      # use DIR/t56a.html, DIR/zcta_county.txt, DIR/US.txt

Sources (every number in the generated file comes from exactly one of these):
  1. EIA, Electric Power Monthly, Table 5.6.A "Average Price of Electricity to Ultimate Customers
     by End-Use Sector, by State" - the RESIDENTIAL column for the newest month on the page.
     https://www.eia.gov/electricity/monthly/epm_table_grapher.php?t=epmt_5_6_a
  2. U.S. Census Bureau, 2020 ZCTA-to-county relationship file - each ZCTA's state (the county
     holding the largest land share of it), then each 3-digit prefix's MAJORITY state.
     https://www2.census.gov/geo/docs/maps-data/data/rel2020/zcta520/tab20_zcta520_county20_natl.txt
  3. GeoNames US postal codes (CC BY 4.0) - ONLY for prefixes the Census file cannot see because
     they hold no residential area (PO-box-only, single-business and military ZIPs).
     https://download.geonames.org/export/zip/US.zip

To refresh when EIA publishes a new month: run this, review the diff (`git diff --stat` and the
changed cents), run the tests, commit. The month and release date come from the page itself;
nothing here is typed by hand except the state-name -> code table below and the documented
5-digit territory overrides in cost_tables.rs's own lookup (not generated).
"""
import collections, hashlib, html, io, re, sys, urllib.request, zipfile

EIA_URL = "https://www.eia.gov/electricity/monthly/epm_table_grapher.php?t=epmt_5_6_a"
CENSUS_URL = "https://www2.census.gov/geo/docs/maps-data/data/rel2020/zcta520/tab20_zcta520_county20_natl.txt"
GEONAMES_URL = "https://download.geonames.org/export/zip/US.zip"
OUT = "crates/lss-core/src/cost_tables.rs"

STATES = {
    "Alabama": "AL", "Alaska": "AK", "Arizona": "AZ", "Arkansas": "AR", "California": "CA", "Colorado": "CO",
    "Connecticut": "CT", "Delaware": "DE", "District of Columbia": "DC", "Florida": "FL", "Georgia": "GA",
    "Hawaii": "HI", "Idaho": "ID", "Illinois": "IL", "Indiana": "IN", "Iowa": "IA", "Kansas": "KS",
    "Kentucky": "KY", "Louisiana": "LA", "Maine": "ME", "Maryland": "MD", "Massachusetts": "MA",
    "Michigan": "MI", "Minnesota": "MN", "Mississippi": "MS", "Missouri": "MO", "Montana": "MT",
    "Nebraska": "NE", "Nevada": "NV", "New Hampshire": "NH", "New Jersey": "NJ", "New Mexico": "NM",
    "New York": "NY", "North Carolina": "NC", "North Dakota": "ND", "Ohio": "OH", "Oklahoma": "OK",
    "Oregon": "OR", "Pennsylvania": "PA", "Rhode Island": "RI", "South Carolina": "SC",
    "South Dakota": "SD", "Tennessee": "TN", "Texas": "TX", "Utah": "UT", "Vermont": "VT",
    "Virginia": "VA", "Washington": "WA", "West Virginia": "WV", "Wisconsin": "WI", "Wyoming": "WY",
}
FIPS = {
    "01": "AL", "02": "AK", "04": "AZ", "05": "AR", "06": "CA", "08": "CO", "09": "CT", "10": "DE", "11": "DC",
    "12": "FL", "13": "GA", "15": "HI", "16": "ID", "17": "IL", "18": "IN", "19": "IA", "20": "KS", "21": "KY",
    "22": "LA", "23": "ME", "24": "MD", "25": "MA", "26": "MI", "27": "MN", "28": "MS", "29": "MO", "30": "MT",
    "31": "NE", "32": "NV", "33": "NH", "34": "NJ", "35": "NM", "36": "NY", "37": "NC", "38": "ND", "39": "OH",
    "40": "OK", "41": "OR", "42": "PA", "44": "RI", "45": "SC", "46": "SD", "47": "TN", "48": "TX", "49": "UT",
    "50": "VT", "51": "VA", "53": "WA", "54": "WV", "55": "WI", "56": "WY",
    "60": "AS", "66": "GU", "69": "MP", "72": "PR", "78": "VI",
}
MONTHS = {m: i + 1 for i, m in enumerate("January February March April May June July August September October November December".split())}


def fetch(url):
    req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0 (lss gen-cost-tables)"})
    with urllib.request.urlopen(req, timeout=120) as r:
        return r.read()


def sha(b):
    return hashlib.sha256(b).hexdigest()


def parse_eia(raw):
    t = raw.decode("utf-8", "replace")
    rel = re.findall(r"Release Date:</span>\s*<span class=\"date\">\s*([A-Za-z]+ \d{1,2}, \d{4})", t)
    t = re.sub(r"<script.*?</script>", "", t, flags=re.S)
    txt = html.unescape(re.sub(r"<[^>]+>", "|", t))
    txt = re.sub(r"\|\s*(\|\s*)+", "|", txt)
    i = txt.find("Table 5.6.A.")
    if i < 0:
        sys.exit("EIA page has no 'Table 5.6.A.' - the page layout changed; do not guess, read it by hand")
    head = re.search(r"by State, ([A-Z][a-z]+) (\d{4}) and \d{4}", txt[i:i + 400])
    if not head:
        sys.exit("cannot find the data month in the table title")
    month = f"{head.group(2)}-{MONTHS[head.group(1)]:02d}"
    cells = [c.strip() for c in txt[i:].split("|")]
    rates = {}
    for k, c in enumerate(cells):
        name = "US" if c == "U.S. Total" else c
        if (c in STATES or c == "U.S. Total") and name not in rates:
            v = cells[k + 1]  # the first number after the name = Residential, newest month
            rates[STATES.get(c, "US")] = float(v)
        if c == "U.S. Total":
            break
    if len(rates) != 52:
        sys.exit(f"expected 50 states + DC + U.S. total, got {len(rates)}: {sorted(rates)}")
    released = rel[0] if rel else "unknown"
    return month, released, rates


def census_prefixes(raw):
    best = {}
    for line in raw.decode("utf-8-sig").splitlines():
        f = line.split("|")
        if len(f) < 17 or not f[1] or f[1].startswith("GEOID"):
            continue
        st = FIPS.get(f[9][:2])
        if st is None:
            continue
        area = int(f[16] or 0)
        if f[1] not in best or area > best[f[1]][1]:
            best[f[1]] = (st, area)
    p = collections.defaultdict(collections.Counter)
    for z, (st, _) in best.items():
        p[z[:3]][st] += 1
    return {k: v.most_common(1)[0][0] for k, v in p.items()}, {k: dict(v) for k, v in p.items() if len(v) > 1}


def geonames_prefixes(raw):
    p = collections.defaultdict(collections.Counter)
    for line in raw.decode("utf-8").splitlines():
        f = line.split("\t")
        if len(f) > 4:
            p[f[1][:3]][f[4] or "MIL"] += 1
    return {k: v.most_common(1)[0][0] for k, v in p.items()}


def main():
    src = sys.argv[2] if len(sys.argv) > 2 and sys.argv[1] == "--from" else None
    if src:
        eia_raw = open(f"{src}/t56a.html", "rb").read()
        cen_raw = open(f"{src}/zcta_county.txt", "rb").read()
        gn_raw = open(f"{src}/US.txt", "rb").read()
    else:
        eia_raw, cen_raw = fetch(EIA_URL), fetch(CENSUS_URL)
        gn_raw = zipfile.ZipFile(io.BytesIO(fetch(GEONAMES_URL))).read("US.txt")
    month, released, rates = parse_eia(eia_raw)
    cen, mixed = census_prefixes(cen_raw)
    gn = geonames_prefixes(gn_raw)
    zip3 = dict(cen)
    added = {}
    for k, st in gn.items():
        if k in zip3:
            continue
        if st == "MIL":  # APO/FPO/DPO: 090-098 = AE, 340 = AA, 962-966 = AP (USPS military "states")
            st = "AA" if k == "340" else ("AE" if k.startswith("09") else "AP")
        zip3[k] = st
        added[k] = st
    # card #298 (verifier INFO): a military prefix that NEITHER source lists still belongs to its
    # USPS military "state" - 966 (FPO AP, ships) has no land area for the Census file and no
    # entry in GeoNames, so it read as "no US ZIP starts with 966". The ranges are the USPS ones
    # the comment above already names; only prefixes both sources leave empty are filled.
    for k, st in [("340", "AA")] + [(f"{i:03d}", "AE") for i in range(90, 99)] + [(f"{i:03d}", "AP") for i in range(962, 967)]:
        if k not in zip3:
            zip3[k] = st
            added[k] = st
    rows = ",\n".join(f"    (\"{c}\", {rates[c]:.2f})" for c in sorted(rates))
    arr = []
    for i in range(1000):
        arr.append(f"\"{zip3.get(f'{i:03d}', '')}\"")
    arr_lines = "\n".join("    " + ", ".join(arr[i:i + 20]) + "," for i in range(0, 1000, 20))
    wrap = lambda items: "\n//!   ".join(" ".join(items[i:i + 8]) for i in range(0, len(items), 8))
    mixed_doc = wrap([f"{k}:" + "/".join(f"{s}{n}" for s, n in sorted(v.items(), key=lambda x: -x[1])) for k, v in sorted(mixed.items())])
    added_doc = wrap([f"{k}={v}" for k, v in sorted(added.items())])
    out = f"""//! GENERATED by scripts/gen-cost-tables.py - do not edit by hand; re-run the script (card #298).
//!
//! The embedded, dated data the cost wizard (`lss-collector cost-setup`) prices a ZIP code with,
//! with no network. Every number below comes from one of these, fetched when this was generated:
//!
//! RATES - U.S. Energy Information Administration (EIA), Electric Power Monthly, Table 5.6.A
//!   "Average Price of Electricity to Ultimate Customers by End-Use Sector, by State",
//!   RESIDENTIAL sector, {month} (preliminary), cents per kWh; page released {released}.
//!   {EIA_URL}
//!   sha256 of the page as fetched: {sha(eia_raw)}
//! ZIP PREFIX -> STATE - U.S. Census Bureau, 2020 ZCTA-to-county relationship file: each ZCTA's
//!   state is the county holding most of its land, and each 3-digit prefix takes the MAJORITY
//!   state of its ZCTAs. Prefixes whose ZCTAs straddle a state line (the majority wins; the few
//!   border ZIPs of the minority state get their neighbour's average), ZCTA counts per state:
//!   {mixed_doc}
//!   {CENSUS_URL}
//!   sha256: {sha(cen_raw)}
//! PO-BOX / BUSINESS / MILITARY PREFIXES the Census file cannot see (no land area) - GeoNames US
//!   postal codes (CC BY 4.0, https://www.geonames.org/), majority state per prefix; AA/AE/AP =
//!   the USPS military "states" (APO/FPO/DPO) - a military prefix in the USPS ranges (340 AA,
//!   090-098 AE, 962-966 AP) that neither source lists is filled from those ranges:
//!   {added_doc}
//!   {GEONAMES_URL}
//!   sha256 of US.txt: {sha(gn_raw)}
//!
//! An empty string = no ZIP code uses that prefix in either source (e.g. 000-004): the wizard
//! says so and asks again rather than guessing a state.

/// The EIA month the rates describe ("YYYY-MM") - shown next to every rate the wizard writes.
pub const EIA_MONTH: &str = "{month}";
/// When EIA published the page the rates were read from.
pub const EIA_RELEASED: &str = "{released}";
pub const EIA_SOURCE: &str = "EIA Electric Power Monthly, Table 5.6.A (average residential price by state)";
pub const EIA_URL: &str = "{EIA_URL}";

/// (state code, cents per kWh) - 50 states, DC, and "US" (the U.S. total row), residential.
pub const RESIDENTIAL_CENTS_PER_KWH: [(&str, f64); {len(rates)}] = [
{rows},
];

/// Index = the ZIP code's first three digits as a number (so "005" is index 5 - leading zeros
/// are the index, never lost). Value = USPS state/territory code, or "" for an unused prefix.
pub const ZIP3_STATE: [&str; 1000] = [
{arr_lines}
];
"""
    open(OUT, "w").write(out)
    print(f"wrote {OUT}: EIA {month} (released {released}), {len(rates)} rates, "
          f"{sum(1 for v in zip3.values() if v)} prefixes ({len(cen)} Census + {len(added)} GeoNames)")


if __name__ == "__main__":
    main()
