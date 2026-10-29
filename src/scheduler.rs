//! Background loop: starts scheduled deployments, recurring schedules,
//! AD auto-sync, periodic inventory and update scans and the daily app
//! catalog check.

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use tracing::{error, info};

use crate::ad::{self, AdSettings};
use crate::app::App;
use crate::db::Schedule;
use crate::engine;

pub fn spawn(app: App) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = tick(&app).await {
                error!("scheduler: {e:#}");
            }
            if let Err(e) = crate::update::tick(&app).await {
                error!("update check: {e:#}");
            }
            // Keep roughly a day of per-agent metric points (one per minute).
            if let Err(e) = app.db.prune_agent_metrics(1440) {
                error!("pruning agent metrics: {e:#}");
            }
            if let Err(e) = crate::inventory::tick(&app) {
                error!("inventory: {e:#}");
            }
            if let Err(e) = crate::wu::tick(&app) {
                error!("update scan: {e:#}");
            }
            crate::catalog::tick(&app);
            tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        }
    });
}

async fn tick(app: &App) -> Result<()> {
    for id in app.db.due_scheduled_deployments()? {
        app.db.set_deployment_status(&id, "queued")?;
        info!("starting scheduled deployment {id}");
        engine::start(app, id);
    }

    let now = Utc::now();
    for s in app.db.list_schedules()? {
        let due = s
            .next_run
            .as_deref()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .is_some_and(|t| t <= now);
        if !s.enabled || !due {
            continue;
        }
        let next = next_run(&s, Local::now() + Duration::seconds(30))?;
        app.db.mark_schedule_run(s.id, next.as_deref())?;
        match run_schedule(app, &s) {
            Ok(id) => info!("schedule '{}' started deployment {id}", s.name),
            Err(e) => error!("schedule '{}': {e:#}", s.name),
        }
    }

    let ad: AdSettings = app.db.get_setting("ad")?;
    if ad.auto_sync_minutes > 0 {
        let last = ad
            .last_sync
            .as_deref()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc));
        if last.is_none_or(|t| now - t >= Duration::minutes(ad.auto_sync_minutes as i64)) {
            match ad::sync(app).await {
                Ok(n) => info!("AD auto-sync: {n} computers"),
                Err(e) => {
                    error!("AD auto-sync: {e:#}");
                    // Don't retry every 20 seconds after a failure.
                    let mut ad = ad;
                    ad.last_sync = Some(crate::db::now());
                    app.db.set_setting("ad", &ad)?;
                }
            }
        }
    }
    Ok(())
}

fn run_schedule(app: &App, s: &Schedule) -> Result<String> {
    let lib = app.library()?;
    let pkg = lib
        .packages
        .get(&s.package_id)
        .ok_or_else(|| anyhow!("package '{}' not found", s.package_id))?;
    let id = engine::create_deployment(
        app,
        pkg,
        s.variables.clone(),
        s.targets.clone(),
        s.options.clone(),
        &format!("schedule: {}", s.name),
        None,
    )?;
    engine::start(app, id.clone());
    Ok(id)
}

fn parse_hhmm(t: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(t.trim(), "%H:%M").map_err(|_| anyhow!("time must be HH:MM, got '{t}'"))
}

fn to_utc_string(t: DateTime<Local>) -> String {
    t.with_timezone(&Utc).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Parse "YYYY-MM-DD HH:MM", "YYYY-MM-DDTHH:MM" (local time) or RFC 3339.
pub fn parse_when(s: &str) -> Result<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    for fmt in ["%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s, fmt)
            && let Some(t) = Local.from_local_datetime(&n).earliest() {
                return Ok(t.with_timezone(&Utc));
            }
    }
    bail!("cannot parse date/time '{s}' (use YYYY-MM-DD HH:MM)")
}

/// When the schedule should next fire after `after` (None = never again).
pub fn next_run(s: &Schedule, after: DateTime<Local>) -> Result<Option<String>> {
    Ok(match s.kind.as_str() {
        "once" => {
            let t = parse_when(&s.time)?;
            (t > after.with_timezone(&Utc) && s.last_run.is_none())
                .then(|| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        }
        "interval" => {
            if s.interval_minutes < 1 {
                bail!("interval must be at least 1 minute");
            }
            Some(to_utc_string(after + Duration::minutes(s.interval_minutes)))
        }
        "daily" | "weekly" => {
            let at = parse_hhmm(&s.time)?;
            if s.kind == "weekly" && s.weekdays.is_empty() {
                bail!("pick at least one weekday");
            }
            let mut day = after.date_naive();
            let mut found = None;
            for _ in 0..8 {
                let ok_day = s.kind == "daily" || s.weekdays.contains(&day.weekday().num_days_from_monday());
                if ok_day
                    && let Some(t) = Local.from_local_datetime(&day.and_time(at)).earliest()
                        && t > after {
                            found = Some(to_utc_string(t));
                            break;
                        }
                day = day.succ_opt().unwrap();
            }
            found
        }
        "monthly" => {
            let at = parse_hhmm(&s.time)?;
            let weekday = *s.weekdays.first().ok_or_else(|| anyhow!("pick the weekday"))?;
            if weekday > 6 || !(1..=5).contains(&s.week) {
                bail!("monthly needs a weekday and a week 1-4 (or 5 = last)");
            }
            if s.offset_days.abs() > 27 {
                bail!("the day offset must be between -27 and 27");
            }
            // Start a month back: an offset can move last month's date into this one.
            let (mut y, mut m) = (after.year(), after.month());
            (y, m) = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
            let mut found = None;
            for _ in 0..15 {
                let day = nth_weekday(y, m, weekday, s.week) + Duration::days(s.offset_days);
                if let Some(t) = Local.from_local_datetime(&day.and_time(at)).earliest()
                    && t > after
                {
                    found = Some(to_utc_string(t));
                    break;
                }
                (y, m) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            }
            found
        }
        k => bail!("unknown schedule kind '{k}'"),
    })
}

/// The `week`-th (1-4, or 5 = last) `weekday` (0 = Monday) of a month.
fn nth_weekday(year: i32, month: u32, weekday: u32, week: u32) -> NaiveDate {
    let first = NaiveDate::from_ymd_opt(year, month, 1).expect("valid month");
    let shift = (weekday + 7 - first.weekday().num_days_from_monday()) % 7;
    let mut day = first + Duration::days(shift as i64);
    if week >= 5 {
        while (day + Duration::days(7)).month() == month {
            day += Duration::days(7);
        }
    } else {
        day += Duration::days(7 * (week as i64 - 1));
    }
    day
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sched(kind: &str, time: &str, weekdays: Vec<u32>) -> Schedule {
        Schedule {
            id: 0,
            name: "t".into(),
            package_id: "p".into(),
            targets: Default::default(),
            variables: Default::default(),
            options: Default::default(),
            kind: kind.into(),
            time: time.into(),
            weekdays,
            interval_minutes: 30,
            week: 2,
            offset_days: 0,
            enabled: true,
            last_run: None,
            next_run: None,
        }
    }

    #[test]
    fn daily_and_weekly() {
        // Wednesday 2026-10-28 10:00 local
        let after = Local.with_ymd_and_hms(2026, 10, 28, 10, 0, 0).unwrap();
        let next = |s: &Schedule| {
            DateTime::parse_from_rfc3339(&next_run(s, after).unwrap().unwrap())
                .unwrap()
                .with_timezone(&Local)
        };
        let d = next(&sched("daily", "02:00", vec![]));
        assert_eq!((d.day(), d.format("%H:%M").to_string()), (29, "02:00".into()));
        let d = next(&sched("daily", "11:30", vec![]));
        assert_eq!(d.day(), 28);
        // Saturday only
        let w = next(&sched("weekly", "03:00", vec![5]));
        assert_eq!(w.day(), 31);
        assert!(next_run(&sched("weekly", "03:00", vec![]), after).is_err());
        let i = next(&sched("interval", "", vec![]));
        assert_eq!(i, after + Duration::minutes(30));
    }

    #[test]
    fn monthly_patch_tuesday() {
        // Wednesday 2026-10-28 10:00 local; Patch Tuesdays: Oct 13, Nov 10, Dec 8.
        let after = Local.with_ymd_and_hms(2026, 10, 28, 10, 0, 0).unwrap();
        let next = |time: &str, week: u32, offset: i64| {
            let s = Schedule { week, offset_days: offset, ..sched("monthly", time, vec![1]) };
            let t = DateTime::parse_from_rfc3339(&next_run(&s, after).unwrap().unwrap()).unwrap().with_timezone(&Local);
            (t.month(), t.day(), t.format("%H:%M").to_string())
        };
        assert_eq!(next("02:00", 2, 0), (11, 10, "02:00".into()));
        assert_eq!(next("02:00", 2, 2), (11, 12, "02:00".into()));
        // Oct 13 + 15 = today, but 02:00 has passed; at 11:00 it is still ahead.
        assert_eq!(next("02:00", 2, 15), (11, 25, "02:00".into()));
        assert_eq!(next("11:00", 2, 15), (10, 28, "11:00".into()));
        // Patch Tuesday + 20 lands in the next month: Sep 8 + 20 = Sep 28, Oct 13 + 20 = Nov 2.
        assert_eq!(next("02:00", 2, 20), (11, 2, "02:00".into()));
        // Last Saturday of the month (Oct 31).
        let s = Schedule { week: 5, ..sched("monthly", "03:00", vec![5]) };
        let t = DateTime::parse_from_rfc3339(&next_run(&s, after).unwrap().unwrap()).unwrap().with_timezone(&Local);
        assert_eq!((t.month(), t.day()), (10, 31));
        assert!(next_run(&Schedule { week: 0, ..sched("monthly", "03:00", vec![1]) }, after).is_err());
        assert!(next_run(&sched("monthly", "03:00", vec![]), after).is_err());
    }
}
