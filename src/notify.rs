use crate::config::{NotifyConfig, NtfyConfig};
use crate::criteria::Window;
use crate::state::Decision;
use anyhow::{Result, bail};
use async_trait::async_trait;
use chrono_tz::Tz;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub title: String,
    pub body: String,
}

#[async_trait]
pub trait Notifier: Send + Sync {
    fn name(&self) -> &str;
    async fn send(&self, msg: &Message) -> Result<()>;
}

/// Builds every notifier present in the config.
pub fn from_config(cfg: &NotifyConfig) -> Result<Vec<Box<dyn Notifier>>> {
    let mut out: Vec<Box<dyn Notifier>> = Vec::new();
    if let Some(ntfy) = &cfg.ntfy {
        out.push(Box::new(Ntfy::new(ntfy.clone())?));
    }
    if out.is_empty() {
        bail!("no notifier configured (add a [notify.ntfy] section)");
    }
    Ok(out)
}

/// Sends to every notifier; succeeds if at least one delivery worked.
pub async fn send_all(notifiers: &[Box<dyn Notifier>], msg: &Message) -> Result<()> {
    let mut last_err = None;
    let mut delivered = false;
    for n in notifiers {
        match n.send(msg).await {
            Ok(()) => delivered = true,
            Err(e) => {
                tracing::error!(notifier = n.name(), "sending notification failed: {e:#}");
                last_err = Some(e);
            }
        }
    }
    match (delivered, last_err) {
        (false, Some(e)) => Err(e),
        _ => Ok(()),
    }
}

pub struct Ntfy {
    cfg: NtfyConfig,
    http: reqwest::Client,
}

impl Ntfy {
    pub fn new(cfg: NtfyConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("wind-monitor/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { cfg, http })
    }
}

#[async_trait]
impl Notifier for Ntfy {
    fn name(&self) -> &str {
        "ntfy"
    }

    async fn send(&self, msg: &Message) -> Result<()> {
        let url = format!(
            "{}/{}",
            self.cfg.server.trim_end_matches('/'),
            self.cfg.topic
        );
        let mut req = self
            .http
            .post(url)
            .header("Title", header_text(&msg.title))
            .header("Priority", "high")
            .header("Tags", "wind")
            .body(msg.body.clone());
        if let Some(token) = &self.cfg.token {
            req = req.bearer_auth(token);
        }
        // Don't let the secret topic show up in logs via the error's URL.
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("ntfy request failed: {}", e.without_url()))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!("ntfy returned {status}: {}", text.trim());
        }
        Ok(())
    }
}

/// HTTP header values must be ASCII; ntfy accepts RFC 2047 encoded words.
fn header_text(s: &str) -> String {
    if s.is_ascii() {
        s.to_string()
    } else {
        format!("=?UTF-8?B?{}?=", base64(s.as_bytes()))
    }
}

fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Alert text for a matching window, times shown in `tz`.
pub fn window_message(spot_name: &str, window: &Window, decision: Decision, tz: Tz) -> Message {
    let start = window.start.with_timezone(&tz);
    let end = window.end.with_timezone(&tz);
    let hours = (window.end - window.start).num_hours();
    // A window ending at midnight still reads as a single day: "18:00–00:00".
    let last_hour = end - chrono::Duration::hours(1);
    let when = if start.date_naive() == last_hour.date_naive() {
        format!(
            "{} {}–{}",
            start.format("%a %d.%m"),
            start.format("%H:%M"),
            end.format("%H:%M")
        )
    } else {
        format!(
            "{} – {}",
            start.format("%a %d.%m %H:%M"),
            end.format("%a %d.%m %H:%M")
        )
    };
    let dir = match (window.mean_dir_text(), window.mean_dir_deg()) {
        (Some(t), Some(d)) => format!("{t} ({d:.0}°)"),
        _ => "unknown".to_string(),
    };
    let gusts = window
        .max_gust_kn()
        .map(|g| format!("{g:.0} kn"))
        .unwrap_or_else(|| "n/a".to_string());
    let prefix = if decision == Decision::Changed {
        "Update: "
    } else {
        ""
    };

    Message {
        title: format!(
            "{prefix}{spot_name}: {:.0} kn {}",
            window.peak_kn(),
            window.mean_dir_text().unwrap_or("")
        )
        .trim_end()
        .to_string(),
        body: format!(
            "{when} ({hours} h)\n\
             Wind: max {:.0} kn, mean {:.1} kn\n\
             Gusts: up to {gusts}\n\
             Direction: {dir}\n\
             Model: {}",
            window.peak_kn(),
            window.mean_kn(),
            window.models().join(", "),
        ),
    }
}

pub fn test_message() -> Message {
    Message {
        title: "wind-monitor test".to_string(),
        body: "If you can read this, wind alerts will reach you.".to_string(),
    }
}

pub fn feed_failure_message(spot_name: &str, failures: u32, err: &anyhow::Error) -> Message {
    Message {
        title: format!("wind-monitor: feed broken for {spot_name}"),
        body: format!("{failures} consecutive failures. Last error: {err:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::criteria::HourPoint;
    use chrono::{DateTime, Duration, Utc};

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64("Chałupy".as_bytes()), "Q2hhxYJ1cHk=");
        assert_eq!(header_text("plain"), "plain");
        assert_eq!(header_text("ł"), "=?UTF-8?B?xYI=?=");
    }

    #[test]
    fn formats_window_in_local_time() {
        let start: DateTime<Utc> = "2026-10-01T08:00:00Z".parse().unwrap();
        let hours: Vec<HourPoint> = [
            (15.0, 20.0, 250.0, "ICON 2.2 km"),
            (18.0, 24.0, 260.0, "ICON 2.2 km"),
            (16.0, 22.0, 255.0, "GFS 13 km"),
        ]
        .iter()
        .enumerate()
        .map(|(i, (s, g, d, m))| HourPoint {
            time: start + Duration::hours(i as i64),
            model: m.to_string(),
            speed_kn: *s,
            gust_kn: Some(*g),
            dir_deg: Some(*d),
        })
        .collect();
        let w = Window {
            spot_id: 1,
            start,
            end: start + Duration::hours(3),
            hours,
        };
        let m = window_message("Chałupy", &w, Decision::New, chrono_tz::Europe::Warsaw);
        assert_eq!(m.title, "Chałupy: 18 kn WSW");
        assert_eq!(
            m.body,
            "Thu 01.10 10:00–13:00 (3 h)\n\
             Wind: max 18 kn, mean 16.3 kn\n\
             Gusts: up to 24 kn\n\
             Direction: WSW (255°)\n\
             Model: ICON 2.2 km, GFS 13 km"
        );
        let m = window_message("Chałupy", &w, Decision::Changed, chrono_tz::Europe::Warsaw);
        assert!(m.title.starts_with("Update: "));
    }
}
