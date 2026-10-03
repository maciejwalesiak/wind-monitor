//! Alert dedup state, persisted as JSON.
//!
//! A window is identified by (spot, start). A window is alerted once. It is
//! alerted again only if its start shifts by 3h or more, its peak changes by
//! 5 kn or more, or it is a new window. A window that has already begun
//! (its start is clamped to the current hour) re-alerts only when its peak
//! rises by 5 kn or more, so it does not re-alert every hour as it runs.

use crate::criteria::Window;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

pub const START_SHIFT_HOURS: i64 = 3;
pub const PEAK_CHANGE_KN: f64 = 5.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertedWindow {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub peak_kn: f64,
    pub alerted_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    New,
    Changed,
    AlreadyAlerted,
}

impl Decision {
    pub fn should_alert(self) -> bool {
        self != Decision::AlreadyAlerted
    }
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// Alerted windows by spot id.
    #[serde(default)]
    pub spots: BTreeMap<u64, Vec<AlertedWindow>>,
}

impl State {
    /// Loads state; a missing file gives an empty state.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("parsing state file {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading state file {}", path.display())),
        }
    }

    /// Writes the state atomically: temp file in the same dir, fsync, rename.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating state dir {}", dir.display()))?;
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("state.json");
        let tmp = dir.join(format!(".{file_name}.tmp"));
        let json = serde_json::to_vec_pretty(self)?;
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(&json)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Drops windows that have ended.
    pub fn prune(&mut self, now: DateTime<Utc>) {
        for windows in self.spots.values_mut() {
            windows.retain(|w| w.end > now);
        }
        self.spots.retain(|_, w| !w.is_empty());
    }

    /// Decides whether `window` needs an alert. `horizon_start` is the first
    /// hour evaluated this cycle (see criteria::horizon_start).
    pub fn decide(&self, window: &Window, horizon_start: DateTime<Utc>) -> Decision {
        let Some(prev) = self.matching(window) else {
            return Decision::New;
        };
        let peak_delta = window.peak_kn() - prev.peak_kn;
        let ongoing = window.start <= horizon_start;
        let changed = if ongoing {
            peak_delta >= PEAK_CHANGE_KN
        } else {
            let prev_start = prev.start.max(horizon_start);
            (window.start - prev_start).abs() >= Duration::hours(START_SHIFT_HOURS)
                || peak_delta.abs() >= PEAK_CHANGE_KN
        };
        if changed {
            Decision::Changed
        } else {
            Decision::AlreadyAlerted
        }
    }

    /// Records an alert for `window`, replacing the entries it supersedes.
    pub fn record(&mut self, window: &Window, now: DateTime<Utc>) {
        let matched = self.matching(window).cloned();
        let entries = self.spots.entry(window.spot_id).or_default();
        entries.retain(|p| Some(p) != matched.as_ref() && !overlaps(p, window));
        entries.push(AlertedWindow {
            start: window.start,
            end: window.end,
            peak_kn: window.peak_kn(),
            alerted_at: now,
        });
        entries.sort_by_key(|p| p.start);
    }

    /// The previously alerted window this one corresponds to: one that
    /// overlaps it or starts within START_SHIFT_HOURS, closest start first.
    fn matching(&self, window: &Window) -> Option<&AlertedWindow> {
        self.spots
            .get(&window.spot_id)?
            .iter()
            .filter(|p| {
                overlaps(p, window)
                    || (p.start - window.start).abs() < Duration::hours(START_SHIFT_HOURS)
            })
            .min_by_key(|p| (p.start - window.start).abs())
    }
}

fn overlaps(p: &AlertedWindow, w: &Window) -> bool {
    p.start < w.end && w.start < p.end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::criteria::HourPoint;

    fn h(n: i64) -> DateTime<Utc> {
        "2026-10-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap() + Duration::hours(n)
    }

    fn window(spot_id: u64, start: i64, speeds: &[f64]) -> Window {
        Window {
            spot_id,
            start: h(start),
            end: h(start + speeds.len() as i64),
            hours: speeds
                .iter()
                .enumerate()
                .map(|(i, s)| HourPoint {
                    time: h(start + i as i64),
                    model: "GFS 13 km".into(),
                    speed_kn: *s,
                    gust_kn: None,
                    dir_deg: None,
                })
                .collect(),
        }
    }

    fn alerted(w: &Window) -> State {
        let mut s = State::default();
        s.record(w, h(0));
        s
    }

    #[test]
    fn new_window_alerts_once() {
        let w = window(1, 10, &[15.0, 16.0, 15.0]);
        assert_eq!(State::default().decide(&w, h(0)), Decision::New);
        let s = alerted(&w);
        assert_eq!(s.decide(&w, h(0)), Decision::AlreadyAlerted);
        // Other spot is independent.
        assert_eq!(s.decide(&window(2, 10, &[15.0]), h(0)), Decision::New);
    }

    #[test]
    fn small_changes_do_not_realert() {
        let s = alerted(&window(1, 10, &[15.0, 16.0, 15.0]));
        assert_eq!(
            s.decide(&window(1, 12, &[15.0, 20.9]), h(0)),
            Decision::AlreadyAlerted
        );
        assert_eq!(
            s.decide(&window(1, 8, &[11.1, 15.0]), h(0)),
            Decision::AlreadyAlerted
        );
    }

    #[test]
    fn start_shift_realerts() {
        let s = alerted(&window(1, 10, &[15.0, 16.0, 15.0]));
        assert_eq!(
            s.decide(&window(1, 12, &[15.0, 16.0]), h(0)),
            Decision::AlreadyAlerted
        );
        assert_eq!(
            s.decide(&window(1, 7, &[15.0, 16.0, 15.0, 15.0]), h(0)),
            Decision::Changed
        );
        // Starts right after the old window ended, 3h later: a separate window.
        assert_eq!(s.decide(&window(1, 13, &[16.0]), h(0)), Decision::New);
    }

    #[test]
    fn peak_change_realerts() {
        let s = alerted(&window(1, 10, &[15.0, 16.0, 15.0]));
        assert_eq!(
            s.decide(&window(1, 10, &[15.0, 21.0]), h(0)),
            Decision::Changed
        );
        assert_eq!(
            s.decide(&window(1, 10, &[11.0, 11.0]), h(0)),
            Decision::Changed
        );
    }

    #[test]
    fn separate_window_same_day_is_new() {
        let s = alerted(&window(1, 10, &[15.0, 16.0]));
        assert_eq!(s.decide(&window(1, 16, &[15.0, 16.0]), h(0)), Decision::New);
    }

    #[test]
    fn ongoing_window_does_not_realert_as_it_elapses() {
        let s = alerted(&window(1, 10, &[15.0, 22.0, 15.0, 15.0, 15.0]));
        // Four hours later the window is clamped to start at the current hour
        // and its peak has passed.
        let now = h(14);
        assert_eq!(
            s.decide(&window(1, 14, &[15.0]), now),
            Decision::AlreadyAlerted
        );
        // ...but a significant strengthening still alerts.
        assert_eq!(s.decide(&window(1, 14, &[27.0]), now), Decision::Changed);
    }

    #[test]
    fn record_replaces_superseded_and_prune_drops_past() {
        let mut s = alerted(&window(1, 10, &[15.0, 16.0]));
        s.record(&window(1, 11, &[25.0, 25.0]), h(1));
        s.record(&window(1, 30, &[15.0]), h(1));
        let starts: Vec<_> = s.spots[&1].iter().map(|p| p.start).collect();
        assert_eq!(starts, vec![h(11), h(30)]);
        s.prune(h(13));
        assert_eq!(s.spots[&1].len(), 1);
        s.prune(h(31));
        assert!(s.spots.is_empty());
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("wind-monitor-test-{}", std::process::id()));
        let path = dir.join("state.json");
        assert_eq!(State::load(&path).unwrap(), State::default());
        let s = alerted(&window(1, 10, &[15.0, 16.0]));
        s.save(&path).unwrap();
        assert_eq!(State::load(&path).unwrap(), s);
        assert!(!dir.join(".state.json.tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
