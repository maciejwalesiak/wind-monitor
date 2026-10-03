//! One check cycle: fetch → parse → criteria → dedup → notify.

use crate::config::{Config, SpotConfig};
use crate::criteria::{self, Window};
use crate::notify::{self, Notifier};
use crate::state::{Decision, State};
use crate::windguru::{self, Forecast};
use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;

/// Longest backoff after repeated fetch failures of a spot.
const MAX_BACKOFF: Duration = Duration::hours(6);

#[derive(Debug, Default)]
struct SpotHealth {
    failures: u32,
    next_attempt: Option<DateTime<Utc>>,
}

pub struct Service {
    config: Config,
    client: windguru::Client,
    notifiers: Vec<Box<dyn Notifier>>,
    state: State,
    health: HashMap<u64, SpotHealth>,
}

impl Service {
    pub fn new(config: Config, notifiers: Vec<Box<dyn Notifier>>) -> Result<Self> {
        let state = match State::load(&config.state_file) {
            Ok(s) => s,
            Err(e) => {
                // Starting fresh may repeat a few alerts, which beats a crash loop.
                tracing::error!("{e:#}; starting with empty state");
                State::default()
            }
        };
        Ok(Self {
            client: windguru::Client::new()?,
            config,
            notifiers,
            state,
            health: HashMap::new(),
        })
    }

    pub async fn run_cycle(&mut self, now: DateTime<Utc>) {
        self.state.prune(now);
        let mut dirty = false;
        for spot in self.config.spots.clone() {
            let health = self.health.entry(spot.id).or_default();
            if health.next_attempt.is_some_and(|t| t > now) {
                tracing::debug!(spot = spot.name, "backing off, skipping this cycle");
                continue;
            }
            match self.client.fetch(spot.id).await {
                Ok(forecast) => {
                    if health.failures > 0 {
                        tracing::info!(spot = spot.name, "feed recovered");
                    }
                    *health = SpotHealth::default();
                    dirty |= self.handle_forecast(&spot, &forecast, now).await;
                }
                Err(e) => self.handle_failure(&spot, e, now).await,
            }
        }
        if dirty && let Err(e) = self.state.save(&self.config.state_file) {
            tracing::error!("saving state failed: {e:#}");
        }
    }

    /// Returns whether the state changed.
    async fn handle_forecast(
        &mut self,
        spot: &SpotConfig,
        forecast: &Forecast,
        now: DateTime<Utc>,
    ) -> bool {
        warn_missing_models(spot, forecast);
        let windows = criteria::find_windows(forecast, spot, now);
        tracing::info!(spot = spot.name, windows = windows.len(), "checked");
        let horizon = criteria::horizon_start(now);
        let mut dirty = false;
        for w in &windows {
            let decision = self.state.decide(w, horizon);
            if !decision.should_alert() {
                tracing::debug!(spot = spot.name, start = %w.start, "already alerted");
                continue;
            }
            let msg = notify::window_message(&spot.name, w, decision, self.config.timezone);
            match notify::send_all(&self.notifiers, &msg).await {
                Ok(()) => {
                    tracing::info!(spot = spot.name, start = %w.start, peak = w.peak_kn(), ?decision, "alert sent");
                    self.state.record(w, now);
                    dirty = true;
                }
                // Not recorded, so the next cycle retries.
                Err(e) => tracing::error!(spot = spot.name, "alert not delivered: {e:#}"),
            }
        }
        dirty
    }

    async fn handle_failure(&mut self, spot: &SpotConfig, err: anyhow::Error, now: DateTime<Utc>) {
        let health = self.health.entry(spot.id).or_default();
        health.failures += 1;
        let interval = Duration::minutes(self.config.check_interval_min as i64);
        let backoff = (interval * 2i32.pow(health.failures.min(8) - 1)).min(MAX_BACKOFF);
        health.next_attempt = Some(now + backoff);
        tracing::warn!(
            spot = spot.name,
            failures = health.failures,
            retry_in_min = backoff.num_minutes(),
            "fetch failed: {err:#}"
        );
        if self.config.failure_notice_after == Some(health.failures) {
            let msg = notify::feed_failure_message(&spot.name, health.failures, &err);
            if let Err(e) = notify::send_all(&self.notifiers, &msg).await {
                tracing::error!("feed failure notice not delivered: {e:#}");
            }
        }
    }
}

fn warn_missing_models(spot: &SpotConfig, forecast: &Forecast) {
    let missing: Vec<&str> = spot
        .models
        .iter()
        .filter(|m| forecast.model(m).is_none())
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        let available: Vec<&str> = forecast.models.iter().map(|m| m.name.as_str()).collect();
        tracing::warn!(
            spot = spot.name,
            "configured models not in feed: {missing:?}; available: {available:?}"
        );
    }
}

/// `--once`: runs a single cycle, prints matches, sends nothing and leaves
/// the state untouched. Returns false if any spot failed.
pub async fn run_once(config: &Config, now: DateTime<Utc>) -> Result<bool> {
    let client = windguru::Client::new()?;
    let state = State::load(&config.state_file).unwrap_or_else(|e| {
        eprintln!("warning: {e:#}; ignoring state");
        State::default()
    });
    let horizon = criteria::horizon_start(now);
    let mut ok = true;
    for spot in &config.spots {
        println!("== {} (spot {}) ==", spot.name, spot.id);
        let forecast = match client.fetch(spot.id).await {
            Ok(f) => f,
            Err(e) => {
                println!("  fetch failed: {e:#}\n");
                ok = false;
                continue;
            }
        };
        println!("  feed: {}", forecast.spot_name);
        for name in &spot.models {
            match forecast.model(name) {
                Some(m) => println!("  model {name}: init {}, {} rows", m.init, m.rows.len()),
                None => println!("  model {name}: NOT IN FEED"),
            }
        }
        let windows: Vec<Window> = criteria::find_windows(&forecast, spot, now);
        if windows.is_empty() {
            println!("  no matching windows\n");
            continue;
        }
        for w in &windows {
            let decision = state.decide(w, horizon);
            let note = match decision {
                Decision::New => "would alert (new)",
                Decision::Changed => "would alert (changed)",
                Decision::AlreadyAlerted => "already alerted",
            };
            let msg = notify::window_message(&spot.name, w, decision, config.timezone);
            println!("\n  [{note}] {}", msg.title);
            for line in msg.body.lines() {
                println!("    {line}");
            }
        }
        println!();
    }
    Ok(ok)
}
