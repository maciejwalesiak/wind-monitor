//! Windguru micro feed (<https://micro.windguru.cz/help.php>): fetching and parsing.
//!
//! The feed is an HTML page wrapping a `<pre>` block:
//!
//! ```text
//! Poland - Zegrze,  lat: 52.46, lon: 21.01, alt: 74, SST: - C
//!
//! GFS 13 km (init: 2026-09-30 12 UTC)
//!
//!         Date    WSPD    GUST   WDIRN    WDEG
//!      (UTC+2)   knots   knots    dir.    deg.
//!
//!  Wed 30. 14h      11      15      SE     128
//!   Thu 1. 00h       7      20     ESE     117
//! ```
//!
//! Rows carry only weekday, day of month and hour in the offset given under
//! `Date`, so full timestamps are rebuilt from the model init time.

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Datelike, Duration, FixedOffset, NaiveDate, TimeZone, Utc, Weekday};
use std::time::Duration as StdDuration;

pub const BASE_URL: &str = "https://micro.windguru.cz/";
/// Variables requested from the feed. The parser reads columns by header name,
/// so their order here does not matter.
pub const VARIABLES: &str = "WSPD,GUST,WDIRN,WDEG";

#[derive(Debug, Clone)]
pub struct Forecast {
    /// Spot description from the feed header, e.g. "Poland - Zegrze".
    pub spot_name: String,
    pub models: Vec<ModelForecast>,
}

#[derive(Debug, Clone)]
pub struct ModelForecast {
    pub name: String,
    pub init: DateTime<Utc>,
    /// Rows in chronological order.
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub time: DateTime<Utc>,
    pub speed_kn: Option<f64>,
    pub gust_kn: Option<f64>,
    pub dir_deg: Option<f64>,
}

impl Forecast {
    pub fn model(&self, name: &str) -> Option<&ModelForecast> {
        let wanted = normalize_model_name(name);
        self.models
            .iter()
            .find(|m| normalize_model_name(&m.name) == wanted)
    }
}

/// Model names are compared case-insensitively with whitespace collapsed.
pub fn normalize_model_name(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub struct Client {
    http: reqwest::Client,
    base_url: String,
}

impl Client {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("wind-monitor/", env!("CARGO_PKG_VERSION")))
            .timeout(StdDuration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            base_url: BASE_URL.to_string(),
        })
    }

    pub async fn fetch(&self, spot_id: u64) -> Result<Forecast> {
        let body = self
            .http
            .get(&self.base_url)
            .query(&[
                ("s", spot_id.to_string().as_str()),
                ("m", "all"),
                ("v", VARIABLES),
            ])
            .send()
            .await
            .context("request to windguru failed")?
            .error_for_status()
            .context("windguru returned an error status")?
            .text()
            .await
            .context("reading windguru response failed")?;
        parse(&body).with_context(|| format!("parsing windguru feed for spot {spot_id}"))
    }
}

/// Parses a micro feed response (either the raw HTML page or just its text).
pub fn parse(input: &str) -> Result<Forecast> {
    let text = extract_pre(input);
    let lines: Vec<&str> = text.lines().collect();

    let spot_name = lines
        .iter()
        .find_map(|l| {
            l.find("lat:")
                .map(|i| l[..i].trim().trim_end_matches(',').trim())
        })
        .unwrap_or_default()
        .to_string();

    let mut models = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some((name, init)) = parse_model_header(lines[i])? {
            let (model, next) = parse_model_block(&name, init, &lines, i + 1)
                .with_context(|| format!("model {name}"))?;
            models.push(model);
            i = next;
        } else {
            i += 1;
        }
    }

    if models.is_empty() {
        let snippet: String = text.trim().chars().take(200).collect();
        bail!("no model forecasts found in response: {snippet:?}");
    }
    Ok(Forecast { spot_name, models })
}

fn extract_pre(input: &str) -> String {
    let body = match (input.find("<pre>"), input.find("</pre>")) {
        (Some(start), Some(end)) if start < end => &input[start + "<pre>".len()..end],
        _ => input,
    };
    body.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&amp;", "&")
}

/// `GFS 13 km (init: 2026-09-30 12 UTC)` → ("GFS 13 km", 2026-09-30T12:00Z)
fn parse_model_header(line: &str) -> Result<Option<(String, DateTime<Utc>)>> {
    let line = line.trim();
    let Some(idx) = line.find(" (init: ") else {
        return Ok(None);
    };
    let name = line[..idx].trim().to_string();
    let init = line[idx + " (init: ".len()..]
        .trim_end_matches(')')
        .trim_end_matches("UTC")
        .trim();
    let init = chrono::NaiveDateTime::parse_from_str(&format!("{init}:00"), "%Y-%m-%d %H:%M")
        .with_context(|| format!("bad init time in {line:?}"))?;
    Ok(Some((name, init.and_utc())))
}

fn parse_model_block(
    name: &str,
    init: DateTime<Utc>,
    lines: &[&str],
    start: usize,
) -> Result<(ModelForecast, usize)> {
    let mut i = start;
    // Column header: "Date  WSPD  GUST  WDIRN  WDEG"
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    let header: Vec<&str> = lines
        .get(i)
        .ok_or_else(|| anyhow!("missing column header"))?
        .split_whitespace()
        .collect();
    if header.first() != Some(&"Date") {
        bail!("expected column header, got {:?}", lines[i]);
    }
    let columns = &header[1..];
    let col = |name: &str| columns.iter().position(|c| *c == name);
    let (speed_col, gust_col, deg_col, dirn_col) =
        (col("WSPD"), col("GUST"), col("WDEG"), col("WDIRN"));
    if speed_col.is_none() {
        bail!("feed has no WSPD column");
    }
    i += 1;

    // Units line, first token is the row time offset: "(UTC+2)".
    let units = lines.get(i).ok_or_else(|| anyhow!("missing units line"))?;
    let offset = parse_offset(units.split_whitespace().next().unwrap_or_default())?;
    i += 1;

    let mut rows = Vec::new();
    let mut prev_date: Option<NaiveDate> = None;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let Some((weekday, day, hour)) = parse_row_prefix(&tokens) else {
            break; // next model header or trailer
        };
        if tokens.len() != 3 + columns.len() {
            bail!(
                "row has {} values, expected {}: {line:?}",
                tokens.len() - 3,
                columns.len()
            );
        }
        let values = &tokens[3..];

        // Search forward from the previous row (or the day before init) for
        // the first date with this day of month and weekday.
        let from = prev_date
            .unwrap_or_else(|| init.with_timezone(&offset).date_naive() - Duration::days(1));
        let date = (0..62)
            .map(|d| from + Duration::days(d))
            .find(|d| d.day() == day && d.weekday() == weekday)
            .ok_or_else(|| anyhow!("cannot place row {line:?} after {from}"))?;
        prev_date = Some(date);
        let local = date
            .and_hms_opt(hour, 0, 0)
            .ok_or_else(|| anyhow!("bad hour in {line:?}"))?;
        let time = offset
            .from_local_datetime(&local)
            .single()
            .ok_or_else(|| anyhow!("bad local time in {line:?}"))?
            .with_timezone(&Utc);

        let num = |c: Option<usize>| -> Result<Option<f64>> {
            match c.map(|c| values[c]) {
                None | Some("-") => Ok(None),
                Some(v) => v
                    .parse::<f64>()
                    .map(Some)
                    .with_context(|| format!("bad number {v:?} in {line:?}")),
            }
        };
        let dir_deg = match num(deg_col)? {
            Some(d) => Some(d),
            None => dirn_col.and_then(|c| compass_to_degrees(values[c])),
        };
        rows.push(Row {
            time,
            speed_kn: num(speed_col)?,
            gust_kn: num(gust_col)?,
            dir_deg,
        });
        i += 1;
    }

    rows.sort_by_key(|r| r.time);
    Ok((
        ModelForecast {
            name: name.to_string(),
            init,
            rows,
        },
        i,
    ))
}

/// `["Wed", "30.", "14h", ...]` → (Wed, 30, 14)
fn parse_row_prefix(tokens: &[&str]) -> Option<(Weekday, u32, u32)> {
    if tokens.len() < 3 {
        return None;
    }
    let weekday: Weekday = tokens[0].parse().ok()?;
    let day: u32 = tokens[1].strip_suffix('.')?.parse().ok()?;
    let hour: u32 = tokens[2].strip_suffix('h')?.parse().ok()?;
    Some((weekday, day, hour))
}

/// `(UTC+2)`, `(UTC-3)`, `(UTC+5:30)`, `(UTC)` → fixed offset
fn parse_offset(token: &str) -> Result<FixedOffset> {
    let inner = token
        .strip_prefix("(UTC")
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(|| anyhow!("unrecognised time offset {token:?}"))?;
    if inner.is_empty() {
        return Ok(FixedOffset::east_opt(0).unwrap());
    }
    let (sign, rest) = match inner.as_bytes()[0] {
        b'+' => (1, &inner[1..]),
        b'-' => (-1, &inner[1..]),
        _ => bail!("unrecognised time offset {token:?}"),
    };
    let (h, m) = rest.split_once(':').unwrap_or((rest, "0"));
    let secs = h.parse::<i32>()? * 3600 + m.parse::<i32>()? * 60;
    FixedOffset::east_opt(sign * secs).ok_or_else(|| anyhow!("offset out of range {token:?}"))
}

const COMPASS: [&str; 16] = [
    "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW", "NW",
    "NNW",
];

fn compass_to_degrees(s: &str) -> Option<f64> {
    COMPASS
        .iter()
        .position(|c| *c == s)
        .map(|i| i as f64 * 22.5)
}

pub fn degrees_to_compass(deg: f64) -> &'static str {
    let idx = ((deg.rem_euclid(360.0) / 22.5).round() as usize) % 16;
    COMPASS[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn parses_offsets() {
        assert_eq!(parse_offset("(UTC+2)").unwrap().local_minus_utc(), 7200);
        assert_eq!(parse_offset("(UTC-3)").unwrap().local_minus_utc(), -10800);
        assert_eq!(parse_offset("(UTC+5:30)").unwrap().local_minus_utc(), 19800);
        assert_eq!(parse_offset("(UTC)").unwrap().local_minus_utc(), 0);
        assert!(parse_offset("(CET)").is_err());
    }

    #[test]
    fn compass_round_trip() {
        assert_eq!(compass_to_degrees("WSW"), Some(247.5));
        assert_eq!(degrees_to_compass(355.0), "N");
        assert_eq!(degrees_to_compass(128.0), "SE");
    }

    #[test]
    fn year_rollover_and_missing_values() {
        let text = "\
Somewhere,  lat: 1, lon: 2

GFS 13 km (init: 2026-12-31 18 UTC)

        Date    WSPD    GUST   WDIRN    WDEG
       (UTC)   knots   knots    dir.    deg.

 Thu 31. 21h      11       -      SE     128
 Thu 31. 23h       -      15     ESE       -

  Fri 1. 02h      13      20       W     270
";
        let f = parse(text).unwrap();
        assert_eq!(f.spot_name, "Somewhere");
        let rows = &f.models[0].rows;
        assert_eq!(rows[0].time, utc("2026-12-31T21:00:00Z"));
        assert_eq!(rows[0].gust_kn, None);
        assert_eq!(rows[1].speed_kn, None);
        // WDEG missing: falls back to the WDIRN text.
        assert_eq!(rows[1].dir_deg, Some(112.5));
        assert_eq!(rows[2].time, utc("2027-01-01T02:00:00Z"));
    }

    #[test]
    fn rejects_non_forecast_response() {
        assert!(parse("<html><body>Spot not found</body></html>").is_err());
    }
}
