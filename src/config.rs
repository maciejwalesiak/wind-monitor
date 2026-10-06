use anyhow::{Context, Result, bail};
use chrono_tz::Tz;
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG_PATH: &str = "/etc/wind-monitor/config.toml";
pub const DEFAULT_STATE_FILE: &str = "/var/lib/wind-monitor/state.json";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_check_interval")]
    pub check_interval_min: u64,
    /// Timezone used to display times in alerts.
    #[serde(default = "default_timezone")]
    pub timezone: Tz,
    #[serde(default = "default_state_file")]
    pub state_file: PathBuf,
    /// Send a "feed broken" notice after this many consecutive failed
    /// fetches of a spot. Off when unset.
    #[serde(default)]
    pub failure_notice_after: Option<u32>,
    pub notify: NotifyConfig,
    #[serde(rename = "spot")]
    pub spots: Vec<SpotConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotifyConfig {
    pub ntfy: Option<NtfyConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NtfyConfig {
    #[serde(default = "default_ntfy_server")]
    pub server: String,
    /// Secret topic name.
    pub topic: String,
    /// Access token, for self-hosted servers with auth.
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpotConfig {
    /// Windguru spot id.
    pub id: u64,
    pub name: String,
    pub days_ahead: u32,
    /// Mean wind speed must be strictly greater than this.
    pub min_speed_kn: f64,
    #[serde(default)]
    pub sector: Option<Sector>,
    #[serde(default = "default_min_consecutive_hours")]
    pub min_consecutive_hours: u32,
    /// Only count hours when it is light (sun above civil twilight, -6°) at
    /// the spot's coordinates from the feed.
    #[serde(default)]
    pub daylight_only: bool,
    /// Model priority list, e.g. ["ICON 2.2 km", "GFS 13 km"].
    pub models: Vec<String>,
}

/// Wind direction sector in degrees, read clockwise from `from` to `to`
/// (so 300..60 wraps through north).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sector {
    pub from: f64,
    pub to: f64,
}

impl Sector {
    pub fn contains(&self, deg: f64) -> bool {
        let d = deg.rem_euclid(360.0);
        let (from, to) = (self.from.rem_euclid(360.0), self.to.rem_euclid(360.0));
        if self.to - self.from >= 360.0 {
            true
        } else if from <= to {
            from <= d && d <= to
        } else {
            d >= from || d <= to
        }
    }
}

fn default_check_interval() -> u64 {
    30
}
fn default_timezone() -> Tz {
    chrono_tz::Europe::Warsaw
}
fn default_state_file() -> PathBuf {
    PathBuf::from(DEFAULT_STATE_FILE)
}
fn default_ntfy_server() -> String {
    "https://ntfy.sh".to_string()
}
fn default_min_consecutive_hours() -> u32 {
    1
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.check_interval_min == 0 {
            bail!("check_interval_min must be at least 1");
        }
        if self.spots.is_empty() {
            bail!("no [[spot]] configured");
        }
        if let Some(ntfy) = &self.notify.ntfy
            && ntfy.topic.trim().is_empty()
        {
            bail!("notify.ntfy.topic is empty");
        }
        for s in &self.spots {
            if s.days_ahead == 0 {
                bail!("spot {}: days_ahead must be at least 1", s.name);
            }
            if s.min_consecutive_hours == 0 {
                bail!("spot {}: min_consecutive_hours must be at least 1", s.name);
            }
            if s.models.is_empty() {
                bail!("spot {}: models must list at least one model", s.name);
            }
            if let Some(sec) = s.sector {
                for v in [sec.from, sec.to] {
                    if !(0.0..=360.0).contains(&v) {
                        bail!("spot {}: sector bounds must be within 0..=360", s.name);
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sector_wraps_through_north() {
        let s = Sector {
            from: 300.0,
            to: 60.0,
        };
        assert!(s.contains(300.0));
        assert!(s.contains(0.0));
        assert!(s.contains(360.0));
        assert!(s.contains(60.0));
        assert!(!s.contains(61.0));
        assert!(!s.contains(180.0));
        assert!(!s.contains(299.0));
    }

    #[test]
    fn sector_plain() {
        let s = Sector {
            from: 250.0,
            to: 340.0,
        };
        assert!(s.contains(250.0) && s.contains(300.0) && s.contains(340.0));
        assert!(!s.contains(249.0) && !s.contains(341.0) && !s.contains(10.0));
        assert!(
            Sector {
                from: 0.0,
                to: 360.0
            }
            .contains(180.0)
        );
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../config.example.toml");
        let c: Config = toml::from_str(text).unwrap();
        c.validate().unwrap();
        assert_eq!(c.spots.len(), 2);
        assert_eq!(c.check_interval_min, 30);
        assert_eq!(c.spots[0].min_consecutive_hours, 2);
        assert_eq!(c.spots[1].sector, None);
        assert_eq!(c.spots[1].min_consecutive_hours, 1);
    }

    #[test]
    fn rejects_unknown_fields() {
        let text = r#"
            [notify]
            [[spot]]
            id = 1
            name = "x"
            days_ahead = 1
            min_speed_kn = 10
            models = ["GFS 13 km"]
            min_speed = 3
        "#;
        assert!(toml::from_str::<Config>(text).is_err());
    }
}
