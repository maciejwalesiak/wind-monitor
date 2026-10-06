//! Sun elevation, used to tell daylight hours from dark ones.
//!
//! NOAA "General Solar Position Calculations"
//! (<https://gml.noaa.gov/grad/solcalc/solareqns.PDF>), accurate to well under
//! a degree, which is plenty for telling day from night.

use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc};
use std::f64::consts::PI;

/// Sun elevation of civil twilight: above this it is not dark.
pub const CIVIL_TWILIGHT_DEG: f64 = -6.0;

/// Sun elevation above the horizon in degrees (negative below it), ignoring
/// atmospheric refraction.
pub fn elevation_deg(lat: f64, lon: f64, t: DateTime<Utc>) -> f64 {
    let days_in_year = if NaiveDate::from_ymd_opt(t.year(), 2, 29).is_some() {
        366.0
    } else {
        365.0
    };
    let hours = t.hour() as f64 + t.minute() as f64 / 60.0 + t.second() as f64 / 3600.0;
    // Fractional year in radians.
    let g = 2.0 * PI / days_in_year * (t.ordinal0() as f64 + (hours - 12.0) / 24.0);

    let eqtime_min = 229.18
        * (0.000075 + 0.001868 * g.cos()
            - 0.032077 * g.sin()
            - 0.014615 * (2.0 * g).cos()
            - 0.040849 * (2.0 * g).sin());
    let decl = 0.006918 - 0.399912 * g.cos() + 0.070257 * g.sin() - 0.006758 * (2.0 * g).cos()
        + 0.000907 * (2.0 * g).sin()
        - 0.002697 * (3.0 * g).cos()
        + 0.00148 * (3.0 * g).sin();

    let true_solar_min = hours * 60.0 + eqtime_min + 4.0 * lon;
    let hour_angle = (true_solar_min / 4.0 - 180.0).to_radians();
    let lat = lat.to_radians();
    let cos_zenith = lat.sin() * decl.sin() + lat.cos() * decl.cos() * hour_angle.cos();
    90.0 - cos_zenith.clamp(-1.0, 1.0).acos().to_degrees()
}

/// Whether it is light enough at `t`: the sun is above civil twilight.
pub fn is_daylight(lat: f64, lon: f64, t: DateTime<Utc>) -> bool {
    elevation_deg(lat, lon, t) > CIVIL_TWILIGHT_DEG
}

#[cfg(test)]
mod tests {
    use super::*;

    const WARSAW: (f64, f64) = (52.23, 21.01);

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn elev(t: &str) -> f64 {
        elevation_deg(WARSAW.0, WARSAW.1, utc(t))
    }

    #[test]
    fn solar_noon_elevation_at_solstices() {
        // Max elevation is 90 - lat ± 23.44. Solar noon in Warsaw is ~10:36 UTC
        // in June and ~10:34 UTC in December.
        assert!((elev("2026-06-21T10:36:00Z") - 61.3).abs() < 0.5);
        assert!((elev("2026-12-21T10:34:00Z") - 14.3).abs() < 0.5);
    }

    #[test]
    fn warsaw_winter_sunrise_and_sunset() {
        // Sunrise ~07:44 CET, sunset ~15:25 CET. At sunrise/sunset the
        // geometric elevation is about -0.8 (refraction lifts the disc).
        for t in ["2026-12-21T06:44:00Z", "2026-12-21T14:25:00Z"] {
            let e = elev(t);
            assert!((-1.6..0.0).contains(&e), "{t}: {e}");
        }
    }

    #[test]
    fn daylight_includes_civil_twilight() {
        // ~25 min after the December sunset is still civil twilight; ~1h35
        // after is dark; midnight is dark; midday is light.
        assert!(is_daylight(WARSAW.0, WARSAW.1, utc("2026-12-21T14:50:00Z")));
        assert!(!is_daylight(
            WARSAW.0,
            WARSAW.1,
            utc("2026-12-21T16:00:00Z")
        ));
        assert!(!is_daylight(
            WARSAW.0,
            WARSAW.1,
            utc("2026-06-21T23:00:00Z")
        ));
        assert!(is_daylight(WARSAW.0, WARSAW.1, utc("2026-06-21T12:00:00Z")));
    }

    #[test]
    fn polar_day_and_night() {
        // Svalbard: midnight sun in June, polar night in December.
        assert!(is_daylight(78.2, 15.6, utc("2026-06-21T23:00:00Z")));
        assert!(!is_daylight(78.2, 15.6, utc("2026-12-21T11:00:00Z")));
    }
}
