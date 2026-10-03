//! Turns a parsed forecast into matching wind windows for one spot.

use crate::config::SpotConfig;
use crate::windguru::{Forecast, ModelForecast, Row, degrees_to_compass};
use chrono::{DateTime, Duration, DurationRound, Utc};

/// Longest gap between consecutive rows that is treated as one forecast step
/// (models switch to 3-hourly output further out).
const MAX_STEP_HOURS: i64 = 3;

/// One forecast hour, taken from the highest-priority model covering it.
#[derive(Debug, Clone, PartialEq)]
pub struct HourPoint {
    pub time: DateTime<Utc>,
    pub model: String,
    pub speed_kn: f64,
    pub gust_kn: Option<f64>,
    pub dir_deg: Option<f64>,
}

/// A run of consecutive matching hours.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub spot_id: u64,
    pub start: DateTime<Utc>,
    /// Exclusive end (last hour + 1h).
    pub end: DateTime<Utc>,
    pub hours: Vec<HourPoint>,
}

impl Window {
    pub fn peak_kn(&self) -> f64 {
        self.hours
            .iter()
            .map(|h| h.speed_kn)
            .fold(f64::MIN, f64::max)
    }

    pub fn mean_kn(&self) -> f64 {
        self.hours.iter().map(|h| h.speed_kn).sum::<f64>() / self.hours.len() as f64
    }

    pub fn max_gust_kn(&self) -> Option<f64> {
        self.hours.iter().filter_map(|h| h.gust_kn).reduce(f64::max)
    }

    /// Circular mean of the hourly directions.
    pub fn mean_dir_deg(&self) -> Option<f64> {
        let dirs: Vec<f64> = self.hours.iter().filter_map(|h| h.dir_deg).collect();
        if dirs.is_empty() {
            return None;
        }
        let (s, c) = dirs.iter().fold((0.0, 0.0), |(s, c), d| {
            let r = d.to_radians();
            (s + r.sin(), c + r.cos())
        });
        Some(s.atan2(c).to_degrees().round().rem_euclid(360.0))
    }

    pub fn mean_dir_text(&self) -> Option<&'static str> {
        self.mean_dir_deg().map(degrees_to_compass)
    }

    /// Models used, in order of first appearance.
    pub fn models(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for h in &self.hours {
            if !out.contains(&h.model.as_str()) {
                out.push(&h.model);
            }
        }
        out
    }
}

/// First hour considered for `now`: the current hour.
pub fn horizon_start(now: DateTime<Utc>) -> DateTime<Utc> {
    now.duration_trunc(Duration::hours(1))
        .expect("hour truncation")
}

/// Builds the hourly timeline between now and now + days_ahead, choosing for
/// each hour the first model in the spot's priority list that covers it.
pub fn timeline(forecast: &Forecast, spot: &SpotConfig, now: DateTime<Utc>) -> Vec<HourPoint> {
    let models: Vec<&ModelForecast> = spot
        .models
        .iter()
        .filter_map(|name| forecast.model(name))
        .collect();

    let end = now + Duration::days(spot.days_ahead as i64);
    let mut out = Vec::new();
    let mut t = horizon_start(now);
    while t <= end {
        if let Some((model, row)) = models
            .iter()
            .find_map(|m| row_covering(m, t).map(|r| (m, r)))
            && let Some(speed) = row.speed_kn
        {
            out.push(HourPoint {
                time: t,
                model: model.name.clone(),
                speed_kn: speed,
                gust_kn: row.gust_kn,
                dir_deg: row.dir_deg,
            });
        }
        t += Duration::hours(1);
    }
    out
}

/// The row whose forecast step covers hour `t`, if it has a speed value.
/// A row covers [row.time, next row) as long as that step is at most
/// MAX_STEP_HOURS; the last row covers the length of the step before it.
fn row_covering(model: &ModelForecast, t: DateTime<Utc>) -> Option<&Row> {
    let rows = &model.rows;
    let idx = rows.partition_point(|r| r.time <= t).checked_sub(1)?;
    let row = &rows[idx];
    let step = match (rows.get(idx + 1), idx.checked_sub(1).map(|p| &rows[p])) {
        (Some(next), _) => next.time - row.time,
        (None, Some(prev)) => row.time - prev.time,
        (None, None) => Duration::hours(1),
    };
    let step = if step > Duration::hours(MAX_STEP_HOURS) {
        Duration::hours(1)
    } else {
        step
    };
    (t < row.time + step && row.speed_kn.is_some()).then_some(row)
}

fn hour_matches(spot: &SpotConfig, h: &HourPoint) -> bool {
    if h.speed_kn <= spot.min_speed_kn {
        return false;
    }
    match (spot.sector, h.dir_deg) {
        (None, _) => true,
        (Some(sector), Some(dir)) => sector.contains(dir),
        (Some(_), None) => false,
    }
}

/// All windows of at least `min_consecutive_hours` matching hours.
pub fn find_windows(forecast: &Forecast, spot: &SpotConfig, now: DateTime<Utc>) -> Vec<Window> {
    let mut windows = Vec::new();
    let mut run: Vec<HourPoint> = Vec::new();
    let mut flush = |run: &mut Vec<HourPoint>| {
        if run.len() >= spot.min_consecutive_hours as usize {
            windows.push(Window {
                spot_id: spot.id,
                start: run[0].time,
                end: run[run.len() - 1].time + Duration::hours(1),
                hours: std::mem::take(run),
            });
        } else {
            run.clear();
        }
    };
    for h in timeline(forecast, spot, now) {
        let contiguous = run
            .last()
            .is_none_or(|last| h.time - last.time == Duration::hours(1));
        if !contiguous {
            flush(&mut run);
        }
        if hour_matches(spot, &h) {
            run.push(h);
        } else {
            flush(&mut run);
        }
    }
    flush(&mut run);
    windows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Sector;

    fn t0() -> DateTime<Utc> {
        "2026-10-01T00:00:00Z".parse().unwrap()
    }

    fn h(n: i64) -> DateTime<Utc> {
        t0() + Duration::hours(n)
    }

    /// Model with rows every `step` hours from `start`, speeds from `speeds`,
    /// direction 270.
    fn model(name: &str, start: i64, step: i64, speeds: &[Option<f64>]) -> ModelForecast {
        ModelForecast {
            name: name.to_string(),
            init: t0(),
            rows: speeds
                .iter()
                .enumerate()
                .map(|(i, s)| Row {
                    time: h(start + i as i64 * step),
                    speed_kn: *s,
                    gust_kn: s.map(|s| s + 5.0),
                    dir_deg: Some(270.0),
                })
                .collect(),
        }
    }

    fn spot(models: &[&str]) -> SpotConfig {
        SpotConfig {
            id: 1,
            name: "Test".into(),
            days_ahead: 3,
            min_speed_kn: 12.0,
            sector: None,
            min_consecutive_hours: 1,
            models: models.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn fc(models: Vec<ModelForecast>) -> Forecast {
        Forecast {
            spot_name: "Test".into(),
            models,
        }
    }

    fn s(v: &[f64]) -> Vec<Option<f64>> {
        v.iter().copied().map(Some).collect()
    }

    #[test]
    fn threshold_is_strictly_greater() {
        let f = fc(vec![model("GFS", 0, 1, &s(&[12.0, 12.1, 13.0, 12.0]))]);
        let w = find_windows(&f, &spot(&["GFS"]), t0());
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].start, h(1));
        assert_eq!(w[0].end, h(3));
        assert_eq!(w[0].peak_kn(), 13.0);
        assert!((w[0].mean_kn() - 12.55).abs() < 1e-9);
        assert_eq!(w[0].max_gust_kn(), Some(18.0));
    }

    #[test]
    fn consecutive_hours_filter_blips() {
        let f = fc(vec![model(
            "GFS",
            0,
            1,
            &s(&[15.0, 5.0, 15.0, 15.0, 5.0, 15.0, 15.0, 15.0]),
        )]);
        let mut sp = spot(&["GFS"]);
        sp.min_consecutive_hours = 2;
        let w = find_windows(&f, &sp, t0());
        assert_eq!(
            w.iter().map(|w| (w.start, w.end)).collect::<Vec<_>>(),
            vec![(h(2), h(4)), (h(5), h(8))]
        );
    }

    #[test]
    fn sector_filter_wraps() {
        let mut m = model("GFS", 0, 1, &s(&[15.0, 15.0, 15.0, 15.0]));
        for (r, d) in m.rows.iter_mut().zip([350.0, 10.0, 90.0, 300.0]) {
            r.dir_deg = Some(d);
        }
        let mut sp = spot(&["GFS"]);
        sp.sector = Some(Sector {
            from: 300.0,
            to: 60.0,
        });
        let w = find_windows(&fc(vec![m]), &sp, t0());
        assert_eq!(
            w.iter().map(|w| (w.start, w.end)).collect::<Vec<_>>(),
            vec![(h(0), h(2)), (h(3), h(4))]
        );
        assert_eq!(w[0].mean_dir_deg(), Some(0.0));
        assert_eq!(w[0].mean_dir_text(), Some("N"));
    }

    #[test]
    fn per_hour_model_fallback() {
        // High-res covers hours 0..3 with a gap (missing value) at hour 1;
        // GFS covers everything.
        let hires = model("ICON 2.2 km", 0, 1, &[Some(20.0), None, Some(20.0)]);
        let gfs = model("GFS 13 km", 0, 1, &s(&[14.0; 6]));
        let f = fc(vec![gfs, hires]);
        let tl = timeline(&f, &spot(&["icon  2.2 KM", "GFS 13 km"]), t0());
        let picked: Vec<(&str, f64)> = tl
            .iter()
            .take(5)
            .map(|p| (p.model.as_str(), p.speed_kn))
            .collect();
        assert_eq!(
            picked,
            vec![
                ("ICON 2.2 km", 20.0),
                ("GFS 13 km", 14.0),
                ("ICON 2.2 km", 20.0),
                ("GFS 13 km", 14.0),
                ("GFS 13 km", 14.0),
            ]
        );
        let w = find_windows(&f, &spot(&["ICON 2.2 km", "GFS 13 km"]), t0());
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].models(), vec!["ICON 2.2 km", "GFS 13 km"]);
    }

    #[test]
    fn unlisted_models_are_ignored() {
        let f = fc(vec![model("WRF 9 km", 0, 1, &s(&[30.0; 5]))]);
        assert!(find_windows(&f, &spot(&["GFS 13 km"]), t0()).is_empty());
    }

    #[test]
    fn days_ahead_cutoff() {
        let f = fc(vec![model("GFS", 0, 1, &s(&[15.0; 100]))]);
        let mut sp = spot(&["GFS"]);
        sp.days_ahead = 1;
        let now = t0() + Duration::minutes(20);
        let w = find_windows(&f, &sp, now);
        assert_eq!(w.len(), 1);
        // Starts at the current hour, last hour is <= now + 1 day.
        assert_eq!(w[0].start, h(0));
        assert_eq!(w[0].end, h(25));
    }

    #[test]
    fn past_hours_are_ignored() {
        let f = fc(vec![model("GFS", 0, 1, &s(&[15.0, 15.0, 5.0, 5.0]))]);
        let w = find_windows(&f, &spot(&["GFS"]), h(2));
        assert!(w.is_empty());
    }

    #[test]
    fn three_hourly_rows_cover_their_step() {
        // Hourly until hour 2, then 3-hourly.
        let mut m = model("GFS", 0, 1, &s(&[15.0, 15.0, 15.0]));
        m.rows.extend(model("GFS", 3, 3, &s(&[15.0, 15.0])).rows);
        let tl = timeline(&fc(vec![m]), &spot(&["GFS"]), t0());
        assert_eq!(tl.len(), 9); // hours 0..=8, last row covers 6..9
        assert_eq!(tl[4].time, h(4));
        assert_eq!(tl[4].speed_kn, 15.0);
    }
}
