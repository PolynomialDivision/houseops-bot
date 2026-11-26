//! The daily check-in and the weekly summary.
//!
//! A one-minute tick decides whether either is due, so a changed setting
//! applies at once and a restart shortly after the posting time still
//! posts. The last posting day is stored, so nothing is posted twice.

use chrono::{Datelike, Duration, Local, NaiveDate, NaiveTime, TimeZone, Utc, Weekday};
use mxbot_common::{config::TimeOfDay, format, send};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{info, warn};

use crate::{house::House, render};

/// How late a missed posting (bot was down) is still made up for.
const CATCH_UP: Duration = Duration::hours(6);

fn naive_time(t: TimeOfDay) -> NaiveTime {
    NaiveTime::from_hms_opt(t.hour, t.minute, 0).expect("TimeOfDay is validated")
}

pub fn parse_weekday(s: &str) -> Weekday {
    match s.trim().to_lowercase().as_str() {
        "monday" | "mon" => Weekday::Mon,
        "tuesday" | "tue" => Weekday::Tue,
        "wednesday" | "wed" => Weekday::Wed,
        "thursday" | "thu" => Weekday::Thu,
        "friday" | "fri" => Weekday::Fri,
        "saturday" | "sat" => Weekday::Sat,
        _ => Weekday::Sun,
    }
}

/// Whether a daily posting at `at` is due at `now` (local date and time),
/// given the day it last ran.
pub fn due(
    now_date: NaiveDate,
    now_time: NaiveTime,
    at: NaiveTime,
    last: Option<NaiveDate>,
) -> bool {
    if last == Some(now_date) || now_time < at {
        return false;
    }
    now_time - at <= CATCH_UP
}

pub async fn run(house: House) {
    let mut tick = interval(std::time::Duration::from_secs(60));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let now = Local::now();
        let (date, time) = (now.date_naive(), now.time());
        let settings = house.settings();
        let (last_digest, last_weekly) = {
            let state = house.store().read().await;
            (state.last_digest, state.last_weekly)
        };
        if settings.digest_enabled && due(date, time, naive_time(settings.digest_time), last_digest)
        {
            daily(&house, date, settings.stale_days).await;
        }
        if settings.weekly_enabled
            && date.weekday() == parse_weekday(&settings.weekly_day)
            && due(date, time, naive_time(settings.weekly_time), last_weekly)
        {
            weekly(&house, date).await;
        }
    }
}

async fn daily(house: &House, today: NaiveDate, stale_days: u32) {
    let (text, ids, names) = {
        let state = house.store().read().await;
        let digest = render::digest(&state, today, stale_days);
        let names: std::collections::HashMap<String, String> =
            state.members.clone().into_iter().collect();
        (
            (!digest.is_empty()).then(|| render::digest_message(&digest, &state, today)),
            digest.ids(),
            names,
        )
    };
    // Recorded before posting: a failed post is not retried into a spam loop.
    house.store().mark_reminded(&ids, today).await;
    let Some(text) = text else {
        info!("Daily check-in: nothing needs attention");
        return;
    };
    let Some(room) = house.room() else { return };
    // Mentions notify the assignees named in it. That is the point.
    let content = format::mentionify_with_names(&text, &names);
    match send::send_with_retry(&room, content).await {
        Ok(_) => info!("Daily check-in posted ({} task(s))", ids.len()),
        Err(e) => warn!("Could not post the daily check-in: {e}"),
    }
}

async fn weekly(house: &House, today: NaiveDate) {
    let since = Local
        .from_local_datetime(&(today - Duration::days(7)).and_time(NaiveTime::MIN))
        .earliest()
        .map_or_else(|| Utc::now() - Duration::days(7), |t| t.with_timezone(&Utc));
    let (text, names) = {
        let state = house.store().read().await;
        let names: std::collections::HashMap<String, String> =
            state.members.clone().into_iter().collect();
        (render::weekly_message(&state, since), names)
    };
    house.store().mark_weekly(today).await;
    let (Some(text), Some(room)) = (text, house.room()) else {
        return;
    };
    let mut content = format::mentionify_with_names(&text, &names);
    // Thanks by name, without a notification for everyone thanked.
    content.mentions = Some(mxbot_common::matrix_sdk::ruma::events::Mentions::new());
    if let Err(e) = send::send_with_retry(&room, content).await {
        warn!("Could not post the weekly summary: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn postings_are_due_once_after_their_time_and_caught_up_briefly() {
        let day = NaiveDate::from_ymd_opt(2026, 9, 27).unwrap();
        let yesterday = day.pred_opt();
        assert!(!due(day, t(8, 59), t(9, 0), yesterday));
        assert!(due(day, t(9, 0), t(9, 0), yesterday));
        assert!(
            due(day, t(14, 0), t(9, 0), None),
            "restart within the catch-up window"
        );
        assert!(
            !due(day, t(16, 0), t(9, 0), yesterday),
            "too late to be useful"
        );
        assert!(
            !due(day, t(9, 30), t(9, 0), Some(day)),
            "already posted today"
        );
    }

    #[test]
    fn weekday_names() {
        assert_eq!(parse_weekday("Sunday"), Weekday::Sun);
        assert_eq!(parse_weekday("fri"), Weekday::Fri);
    }
}
