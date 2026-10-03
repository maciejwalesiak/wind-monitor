//! Parser and criteria against real micro.windguru.cz responses saved on
//! 2026-09-30 (`m=all&v=WSPD,GUST,WDIRN,WDEG`).

use chrono::{DateTime, Duration, Utc};
use wind_monitor::config::{Sector, SpotConfig};
use wind_monitor::criteria;
use wind_monitor::windguru::{Forecast, parse};

const CHALUPY: &str = include_str!("fixtures/chalupy_931293.html");
const ZEGRZE: &str = include_str!("fixtures/zegrze_347.html");

fn utc(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

fn check_rows_sane(f: &Forecast) {
    for m in &f.models {
        assert!(!m.rows.is_empty(), "{} has no rows", m.name);
        assert!(m.rows[0].time >= m.init, "{} starts before init", m.name);
        // Strictly increasing; steps may be long (Zephr-HD skips nights).
        for pair in m.rows.windows(2) {
            let step = pair[1].time - pair[0].time;
            assert!(step >= Duration::hours(1), "{}: step {step}", m.name);
        }
    }
}

#[test]
fn parses_chalupy() {
    let f = parse(CHALUPY).unwrap();
    assert!(f.spot_name.contains("Chałupach"), "{}", f.spot_name);
    let names: Vec<&str> = f.models.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "GFS 13 km",
            "IFS-HRES 9 km",
            "MET Nordic 1 km",
            "HARM-DK 2 km",
            "ICON 2.2 km",
            "ALADIN 2.3 km",
            "HARM-FI 2.5 km",
            "Zephr-HD 2.6 km",
            "WRF 9 km",
            "HARMONIE 5 km",
            "ICON 7 km",
            "ICON 13 km",
            "GDPS 15 km",
        ]
    );
    check_rows_sane(&f);

    let gfs = f.model("GFS 13 km").unwrap();
    assert_eq!(gfs.init, utc("2026-09-30T12:00:00Z"));
    // " Wed 30. 14h      11      17     ESE     117" at UTC+2
    let first = &gfs.rows[0];
    assert_eq!(first.time, utc("2026-09-30T12:00:00Z"));
    assert_eq!(
        (first.speed_kn, first.gust_kn, first.dir_deg),
        (Some(11.0), Some(17.0), Some(117.0))
    );
    // Month rollover: " Fri 16. 14h      13      13       W     259" is October 16th.
    let last = gfs.rows.last().unwrap();
    assert_eq!(last.time, utc("2026-10-16T12:00:00Z"));
    assert_eq!((last.speed_kn, last.dir_deg), (Some(13.0), Some(259.0)));
    assert!(
        gfs.rows
            .iter()
            .any(|r| r.time == utc("2026-09-30T22:00:00Z"))
    );
    assert!(
        gfs.rows
            .iter()
            .any(|r| r.time == utc("2026-09-30T23:00:00Z"))
    );

    // " Wed 30. 14h       8       -     ESE     109": missing gust.
    let harm_fi = f.model("HARM-FI 2.5 km").unwrap();
    assert_eq!(harm_fi.rows[0].speed_kn, Some(8.0));
    assert_eq!(harm_fi.rows[0].gust_kn, None);
    assert_eq!(harm_fi.rows[0].dir_deg, Some(109.0));
}

#[test]
fn parses_zegrze() {
    let f = parse(ZEGRZE).unwrap();
    assert_eq!(f.spot_name, "Poland - Zegrze");
    assert_eq!(f.models.len(), 11);
    check_rows_sane(&f);
    assert!(f.model("icon 7 KM").is_some());
    assert!(f.model("ICON 2.2 km").is_none());
}

#[test]
fn night_gap_falls_back_to_next_model() {
    // Zephr-HD has no rows between Wed 30. 20h and Thu 1. 06h (UTC+2).
    let f = parse(CHALUPY).unwrap();
    let spot = SpotConfig {
        id: 931293,
        name: "Chałupy".into(),
        days_ahead: 1,
        min_speed_kn: 0.0,
        sector: None,
        min_consecutive_hours: 1,
        models: vec!["Zephr-HD 2.6 km".into(), "GFS 13 km".into()],
    };
    let tl = criteria::timeline(&f, &spot, utc("2026-09-30T17:00:00Z"));
    let model_at = |t: &str| tl.iter().find(|p| p.time == utc(t)).unwrap().model.clone();
    assert_eq!(model_at("2026-09-30T18:00:00Z"), "Zephr-HD 2.6 km");
    assert_eq!(model_at("2026-09-30T19:00:00Z"), "GFS 13 km");
    assert_eq!(model_at("2026-10-01T03:00:00Z"), "GFS 13 km");
    assert_eq!(model_at("2026-10-01T04:00:00Z"), "Zephr-HD 2.6 km");
}

#[test]
fn criteria_on_real_feed() {
    let f = parse(CHALUPY).unwrap();
    let spot = SpotConfig {
        id: 931293,
        name: "Chałupy".into(),
        days_ahead: 3,
        min_speed_kn: 0.0,
        sector: None,
        min_consecutive_hours: 1,
        models: vec![
            "ICON 2.2 km".into(),
            "HARM-DK 2 km".into(),
            "GFS 13 km".into(),
        ],
    };
    let now = utc("2026-09-30T15:20:00Z");
    let tl = criteria::timeline(&f, &spot, now);
    // Every hour from 15:00 UTC to now + 3 days is covered by some model.
    assert_eq!(tl.first().unwrap().time, utc("2026-09-30T15:00:00Z"));
    assert_eq!(tl.len(), 72 + 1);
    assert_eq!(tl[0].model, "ICON 2.2 km");
    assert_eq!(tl.last().unwrap().model, "GFS 13 km");

    // A sector nobody forecasts yields nothing.
    let mut none = spot.clone();
    none.sector = Some(Sector { from: 1.0, to: 2.0 });
    none.min_speed_kn = 40.0;
    assert!(criteria::find_windows(&f, &none, now).is_empty());
}
