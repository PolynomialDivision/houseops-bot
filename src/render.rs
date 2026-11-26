//! Matrix messages: task cards, lists, reminders. Short lines for phones;
//! names instead of pills wherever a mention would notify for no reason.

use chrono::{NaiveDate, Utc};
use mxbot_common::format::html_escape;

use crate::{
    config::HouseConfig,
    model::{Priority, State, Status, Task},
    parse::ListFilter,
};

/// Plain body and HTML of one message.
pub type Message = (String, String);

/// Lines in a `!tasks` list before it points to the web interface.
const LIST_LIMIT: usize = 15;
const DESCRIPTION_PREVIEW: usize = 100;

fn truncate(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max {
        return s;
    }
    let cut: String = s.chars().take(max - 1).collect();
    format!("{}…", cut.trim_end())
}

pub fn short_date(d: NaiveDate) -> String {
    d.format("%a %-d %b").to_string()
}

pub fn task_url(public_url: Option<&str>, id: u32) -> Option<String> {
    public_url.map(|u| format!("{}/#/task/{id}", u.trim_end_matches('/')))
}

/// "🔥 Urgent · 🔧 Repair · 📍 Kitchen · 📅 Fri 2 Oct": only what is set.
fn meta_parts(task: &Task, house: &HouseConfig, today: NaiveDate) -> Vec<String> {
    let mut parts = Vec::new();
    if task.priority != Priority::Normal {
        parts.push(format!(
            "{} {}",
            task.priority.emoji(),
            task.priority.label()
        ));
    }
    if let Some(cat) = task.category.as_deref() {
        match house.category(cat) {
            Some(c) => parts.push(format!("{} {}", c.emoji, c.label)),
            None => parts.push(format!("🏷️ {cat}")),
        }
    }
    if let Some(loc) = &task.location {
        parts.push(format!("📍 {loc}"));
    }
    if let (Some(due), true) = (task.due, task.is_open()) {
        if due < today {
            parts.push(format!("⚠️ overdue since {}", short_date(due)));
        } else if due == today {
            parts.push("📅 due today".to_owned());
        } else {
            parts.push(format!("📅 {}", short_date(due)));
        }
    }
    parts
}

/// The task card: the thread root in the house room, edited in place on
/// every change.
pub fn card(
    task: &Task,
    state: &State,
    house: &HouseConfig,
    public_url: Option<&str>,
    today: NaiveDate,
) -> Message {
    let mut plain = Vec::new();
    let mut html = Vec::new();
    let head = format!("{} #{} {}", task.status.emoji(), task.id, task.title);
    plain.push(head);
    html.push(format!(
        "{} <strong>#{} {}</strong>",
        task.status.emoji(),
        task.id,
        html_escape(&task.title)
    ));

    if !task.is_open() {
        let by = task.closed_by.as_deref().map(|u| state.name_of(u));
        let when = task.closed_at.map(|t| short_date(t.date_naive()));
        let line = match (task.status, by, when) {
            (Status::Done, Some(by), Some(when)) => format!("🙌 Done by {by} · {when}"),
            (Status::Dropped, Some(by), Some(when)) => format!("🗑️ Dropped by {by} · {when}"),
            (status, ..) => status.label().to_owned(),
        };
        plain.push(line.clone());
        html.push(format!("<em>{}</em>", html_escape(&line)));
        return (plain.join("\n"), html.join("<br>"));
    }

    let meta = meta_parts(task, house, today);
    if !meta.is_empty() {
        let line = meta.join(" · ");
        plain.push(line.clone());
        html.push(html_escape(&line));
    }

    let who = match &task.assignee {
        Some(u) => format!("👤 {}", state.name_of(u)),
        None => "🙋 Up for grabs".to_owned(),
    };
    let line = format!("{who} · ✍️ by {}", state.name_of(&task.created_by));
    plain.push(line.clone());
    html.push(html_escape(&line));

    if task.status == Status::Waiting {
        let line = match &task.waiting_on {
            Some(r) => format!("⏸️ Waiting on {r}"),
            None => "⏸️ Waiting".to_owned(),
        };
        plain.push(line.clone());
        html.push(html_escape(&line));
    }
    if !task.description.is_empty() {
        let line = format!("📝 {}", truncate(&task.description, DESCRIPTION_PREVIEW));
        plain.push(line.clone());
        html.push(html_escape(&line));
    }
    if !task.attachments.is_empty() {
        let n = task.attachments.len();
        let line = format!("📎 {n} photo{}", if n == 1 { "" } else { "s" });
        plain.push(line.clone());
        html.push(html_escape(&line));
    }

    let hint = if task.assignee.is_some() {
        "✅ done · 💬 details in thread"
    } else {
        "✋ take it · ✅ done · 💬 details in thread"
    };
    match task_url(public_url, task.id) {
        Some(url) => {
            plain.push(format!("{hint} · {url}"));
            html.push(format!(
                "<sub>{hint} · <a href=\"{}\">open</a></sub>",
                html_escape(&url)
            ));
        }
        None => {
            plain.push(hint.to_owned());
            html.push(format!("<sub>{hint}</sub>"));
        }
    }
    (plain.join("\n"), html.join("<br>"))
}

fn list_line(task: &Task, state: &State, house: &HouseConfig) -> String {
    let mut line = String::new();
    let marker = match (task.status, task.priority) {
        (Status::Waiting, _) => "⏸️",
        (Status::Done | Status::Dropped, _) => task.status.emoji(),
        (_, p) => p.emoji(),
    };
    if !marker.is_empty() {
        line.push_str(marker);
        line.push(' ');
    }
    line.push_str(&format!("#{} {}", task.id, truncate(&task.title, 60)));
    if let Some(c) = task.category.as_deref().and_then(|c| house.category(c)) {
        line.push_str(&format!(" {}", c.emoji));
    }
    if let Some(loc) = &task.location {
        line.push_str(&format!(" · {loc}"));
    }
    match (&task.assignee, task.status) {
        (_, Status::Waiting) => {
            if let Some(r) = &task.waiting_on {
                line.push_str(&format!(" · waiting on {r}"));
            }
        }
        (Some(u), _) => line.push_str(&format!(" · {}", state.name_of(u))),
        (None, _) if task.is_open() => line.push_str(" · up for grabs"),
        _ => {}
    }
    line
}

/// Open tasks worst first: urgent → high → normal → low, then waiting.
fn sort_key(task: &Task) -> (bool, std::cmp::Reverse<Priority>, u32) {
    (
        task.status == Status::Waiting,
        std::cmp::Reverse(task.priority),
        task.id,
    )
}

pub fn list(
    state: &State,
    house: &HouseConfig,
    filter: ListFilter,
    viewer: &str,
    public_url: Option<&str>,
) -> Message {
    let mut tasks: Vec<&Task> = state
        .tasks
        .values()
        .filter(|t| match filter {
            ListFilter::Open => t.is_open(),
            ListFilter::Mine => t.is_open() && t.assignee.as_deref() == Some(viewer),
            ListFilter::Waiting => t.status == Status::Waiting,
            ListFilter::Done => t.status == Status::Done,
        })
        .collect();
    if filter == ListFilter::Done {
        tasks.sort_by_key(|t| std::cmp::Reverse(t.closed_at));
    } else {
        tasks.sort_by_key(|t| sort_key(t));
    }
    let title = match filter {
        ListFilter::Open => "📋 Open tasks",
        ListFilter::Mine => "📋 Your tasks",
        ListFilter::Waiting => "⏸️ Waiting",
        ListFilter::Done => "✅ Recently done",
    };
    if tasks.is_empty() {
        let text = match filter {
            ListFilter::Open => "📋 Nothing open. The house is in order! 🎉".to_owned(),
            ListFilter::Mine => "📋 You have no open tasks.".to_owned(),
            _ => format!("{title}: none yet."),
        };
        return (text.clone(), html_escape(&text));
    }
    let mut lines = vec![format!("{title} ({})", tasks.len())];
    lines.extend(
        tasks
            .iter()
            .take(LIST_LIMIT)
            .map(|t| list_line(t, state, house)),
    );
    let hidden = tasks.len().saturating_sub(LIST_LIMIT);
    let more = match (hidden, public_url) {
        (0, _) => None,
        (n, Some(url)) => Some((
            format!("… and {n} more: {url}"),
            format!(
                "… and {n} more: <a href=\"{}\">open the list</a>",
                html_escape(url)
            ),
        )),
        (n, None) => Some((format!("… and {n} more"), format!("… and {n} more"))),
    };
    let mut plain = lines.join("\n");
    let mut html = format!(
        "<strong>{}</strong><br>{}",
        html_escape(&lines[0]),
        lines[1..]
            .iter()
            .map(|l| html_escape(l))
            .collect::<Vec<_>>()
            .join("<br>")
    );
    if let Some((p, h)) = more {
        plain.push('\n');
        plain.push_str(&p);
        html.push_str("<br>");
        html.push_str(&h);
    }
    (plain, html)
}

pub fn help(public_url: Option<&str>) -> Message {
    let mut lines = vec![
        "🏠 House tasks",
        "➕ !task <what>: new task",
        "🏷️ Tag it: #kitchen #repair !urgent",
        "📋 !tasks · !mine",
        "⏸️ !tasks waiting · ✅ !tasks done",
        "",
        "👆 On a task card:",
        "✋ take it · ✅ done",
        "💬 Reply in its thread to comment or add photos.",
        "",
        "🧵 In a task thread, or with a number (!done 12):",
        "✅ !done · 🙋 !claim · 👋 !unclaim",
        "⏸️ !wait <reason> · 🗑️ !drop · 🔄 !reopen",
        "🔥 !prio high · 📍 !loc Kitchen",
        "🏷️ !cat repair · 📅 !due fri",
        "👤 !assign @name · 📝 !desc <text>",
        "✏️ !rename <title>",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    if public_url.is_some() {
        lines.push(String::new());
        lines.push("🌐 !web: your personal link to the web app".to_owned());
    }
    let plain = lines.join("\n");
    let html = format!(
        "<strong>{}</strong><br>{}",
        html_escape(&lines[0]),
        lines[1..]
            .iter()
            .map(|l| html_escape(l))
            .collect::<Vec<_>>()
            .join("<br>")
    );
    (plain, html)
}

// ── Reminders ─────────────────────────────────────────────────────────────────

/// A task is mentioned in a reminder at most this often.
pub const REMIND_EVERY_DAYS: i64 = 3;

/// What the daily check-in is about. Empty when nothing needs attention.
#[derive(Debug, Default, PartialEq)]
pub struct Digest {
    pub overdue: Vec<u32>,
    pub urgent_unclaimed: Vec<u32>,
    pub stuck: Vec<u32>,
}

impl Digest {
    pub fn is_empty(&self) -> bool {
        self.overdue.is_empty() && self.urgent_unclaimed.is_empty() && self.stuck.is_empty()
    }

    pub fn ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self
            .overdue
            .iter()
            .chain(&self.urgent_unclaimed)
            .chain(&self.stuck)
            .copied()
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}

pub fn digest(state: &State, today: NaiveDate, stale_days: u32) -> Digest {
    let mut d = Digest::default();
    let recently_reminded = |t: &Task| {
        t.last_reminded
            .is_some_and(|r| (today - r).num_days() < REMIND_EVERY_DAYS)
    };
    for t in state
        .tasks
        .values()
        .filter(|t| t.is_open() && !recently_reminded(t))
    {
        if t.due.is_some_and(|due| due < today) {
            d.overdue.push(t.id);
        } else if t.priority == Priority::Urgent && t.assignee.is_none() {
            d.urgent_unclaimed.push(t.id);
        } else if t.status == Status::Doing
            && (today - t.updated_at.date_naive()).num_days() >= stale_days as i64
        {
            d.stuck.push(t.id);
        }
    }
    d
}

/// The daily check-in. Assignees of overdue and stuck tasks are mentioned;
/// the text is raw MXIDs for `format::mentionify_with_names`.
pub fn digest_message(d: &Digest, state: &State, today: NaiveDate) -> String {
    let mut lines = vec!["🏠 **House check-in**".to_owned()];
    let who = |t: &Task| {
        t.assignee
            .clone()
            .unwrap_or_else(|| "nobody yet".to_owned())
    };
    if !d.overdue.is_empty() {
        lines.push("⚠️ **Overdue**".to_owned());
        for t in d.overdue.iter().filter_map(|id| state.tasks.get(id)) {
            let due = t.due.map(short_date).unwrap_or_default();
            lines.push(format!(
                "• #{} {} · was due {due} · {}",
                t.id,
                t.title,
                who(t)
            ));
        }
    }
    if !d.urgent_unclaimed.is_empty() {
        lines.push("🔥 **Urgent, nobody on it yet**".to_owned());
        for t in d
            .urgent_unclaimed
            .iter()
            .filter_map(|id| state.tasks.get(id))
        {
            let place = t
                .location
                .as_deref()
                .map(|l| format!(" · {l}"))
                .unwrap_or_default();
            lines.push(format!("• #{} {}{place}", t.id, t.title));
        }
    }
    if !d.stuck.is_empty() {
        lines.push("💤 **No news for a while**".to_owned());
        for t in d.stuck.iter().filter_map(|id| state.tasks.get(id)) {
            let days = (today - t.updated_at.date_naive()).num_days();
            lines.push(format!(
                "• #{} {} · {} · {days} days",
                t.id,
                t.title,
                who(t)
            ));
        }
    }
    lines.push("👉 Reply !done <number> or update it on the web.".to_owned());
    lines.join("\n")
}

/// The weekly summary: what got done (with thanks) and what is left.
/// `None` in a completely quiet week.
pub fn weekly_message(state: &State, since: chrono::DateTime<Utc>) -> Option<String> {
    let done: Vec<&Task> = state
        .tasks
        .values()
        .filter(|t| t.status == Status::Done && t.closed_at.is_some_and(|c| c >= since))
        .collect();
    let open = state.tasks.values().filter(|t| t.is_open()).count();
    let waiting = state
        .tasks
        .values()
        .filter(|t| t.status == Status::Waiting)
        .count();
    let urgent = state
        .tasks
        .values()
        .filter(|t| t.is_open() && t.priority == Priority::Urgent)
        .count();
    if done.is_empty() && open == 0 {
        return None;
    }
    let mut lines = Vec::new();
    if done.is_empty() {
        lines.push("🏠 **This week** · nothing closed yet 🌱".to_owned());
    } else {
        let mut by_person: std::collections::BTreeMap<String, usize> = Default::default();
        for t in &done {
            if let Some(u) = &t.closed_by {
                *by_person.entry(state.name_of(u)).or_default() += 1;
            }
        }
        let mut thanks: Vec<(String, usize)> = by_person.into_iter().collect();
        thanks.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let thanks = thanks
            .iter()
            .map(|(name, n)| {
                if *n > 1 {
                    format!("{name} ({n})")
                } else {
                    name.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!(
            "🏠 **This week** · {} task{} done",
            done.len(),
            if done.len() == 1 { "" } else { "s" }
        ));
        lines.push(format!("🙌 Thanks {thanks}!"));
    }
    let mut left = vec![format!("{open} open")];
    if waiting > 0 {
        left.push(format!("{waiting} waiting"));
    }
    if urgent > 0 {
        left.push(format!("🔥 {urgent} urgent"));
    }
    lines.push(format!("📋 {}", left.join(" · ")));
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Change, Draft};
    use chrono::{DateTime, Duration};

    fn house() -> HouseConfig {
        toml::from_str("room_id = \"!r:x\"").unwrap()
    }

    fn at(day: u32) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&format!("2026-09-{day:02}T10:00:00Z"))
            .unwrap()
            .with_timezone(&Utc)
    }

    fn state_with(tasks: Vec<Task>) -> State {
        let mut state = State::default();
        state.members.insert("@alice:x".into(), "Alice".into());
        state.members.insert("@bob:x".into(), "Bob".into());
        for t in tasks {
            state.tasks.insert(t.id, t);
        }
        state
    }

    fn task(id: u32, title: &str) -> Task {
        Task::new(
            id,
            Draft {
                title: title.into(),
                ..Default::default()
            },
            "@bob:x",
            at(20),
        )
    }

    #[test]
    fn open_card_shows_what_matters_and_how_to_act() {
        let mut t = task(12, "Fix the kitchen tap");
        t.priority = Priority::Urgent;
        t.category = Some("repair".into());
        t.location = Some("Kitchen".into());
        t.due = NaiveDate::from_ymd_opt(2026, 10, 2);
        let state = state_with(vec![t.clone()]);
        let today = NaiveDate::from_ymd_opt(2026, 9, 27).unwrap();

        let (plain, html) = card(&t, &state, &house(), Some("https://h.example"), today);

        assert_eq!(
            plain,
            "🟠 #12 Fix the kitchen tap\n\
             🔥 Urgent · 🔧 Repair · 📍 Kitchen · 📅 Fri 2 Oct\n\
             🙋 Up for grabs · ✍️ by Bob\n\
             ✋ take it · ✅ done · 💬 details in thread · https://h.example/#/task/12"
        );
        assert!(html.contains("<strong>#12 Fix the kitchen tap</strong>"));
        assert!(html.contains("href=\"https://h.example/#/task/12\""));
    }

    #[test]
    fn closed_card_collapses_to_who_and_when() {
        let mut t = task(3, "Buy bulbs");
        t.apply(Change::Status(Status::Done, None), "@alice:x", at(26));
        let state = state_with(vec![t.clone()]);
        let (plain, _) = card(&t, &state, &house(), None, at(27).date_naive());
        assert_eq!(plain, "✅ #3 Buy bulbs\n🙌 Done by Alice · Sat 26 Sep");
    }

    #[test]
    fn lists_put_urgent_first_and_waiting_last() {
        let mut a = task(1, "Buy bulbs");
        a.priority = Priority::Low;
        let mut b = task(2, "Water leak");
        b.priority = Priority::Urgent;
        b.location = Some("Bathroom".into());
        let mut c = task(3, "Boiler service");
        c.apply(
            Change::Status(Status::Waiting, Some("landlord".into())),
            "@bob:x",
            at(21),
        );
        let mut d = task(4, "Mow the lawn");
        d.apply(
            Change::Assignee(Some("@alice:x".into())),
            "@alice:x",
            at(21),
        );
        let state = state_with(vec![a, b, c, d]);

        let (plain, _) = list(&state, &house(), ListFilter::Open, "@alice:x", None);
        assert_eq!(
            plain,
            "📋 Open tasks (4)\n\
             🔥 #2 Water leak · Bathroom · up for grabs\n\
             #4 Mow the lawn · Alice\n\
             💤 #1 Buy bulbs · up for grabs\n\
             ⏸️ #3 Boiler service · waiting on landlord"
        );
        let (mine, _) = list(&state, &house(), ListFilter::Mine, "@alice:x", None);
        assert!(mine.contains("#4 Mow the lawn"));
        assert!(!mine.contains("#2"));

        let empty = state_with(vec![]);
        assert!(list(&empty, &house(), ListFilter::Open, "@a:x", None)
            .0
            .contains("in order"));
    }

    #[test]
    fn digest_picks_overdue_urgent_and_stuck_but_not_twice_in_a_row() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 27).unwrap();
        let mut overdue = task(1, "Gutter");
        overdue.due = NaiveDate::from_ymd_opt(2026, 9, 25);
        let mut urgent = task(2, "Leak");
        urgent.priority = Priority::Urgent;
        let mut stuck = task(3, "Paint door");
        stuck.apply(Change::Assignee(Some("@bob:x".into())), "@bob:x", at(10));
        let mut fresh = task(4, "Fine");
        fresh.apply(Change::Assignee(Some("@bob:x".into())), "@bob:x", at(26));
        let mut nagged = task(5, "Nagged");
        nagged.due = NaiveDate::from_ymd_opt(2026, 9, 1);
        nagged.last_reminded = Some(today - Duration::days(1));
        let state = state_with(vec![overdue, urgent, stuck, fresh, nagged]);

        let d = digest(&state, today, 7);
        assert_eq!(
            d,
            Digest {
                overdue: vec![1],
                urgent_unclaimed: vec![2],
                stuck: vec![3]
            }
        );

        let text = digest_message(&d, &state, today);
        assert!(
            text.contains("• #3 Paint door · @bob:x · 17 days"),
            "{text}"
        );
        assert!(digest(&state_with(vec![task(9, "calm")]), today, 7).is_empty());
    }

    #[test]
    fn weekly_summary_thanks_people_and_stays_quiet_in_an_empty_house() {
        let mut a = task(1, "Bulbs");
        a.apply(Change::Status(Status::Done, None), "@alice:x", at(25));
        let mut b = task(2, "Bins");
        b.apply(Change::Status(Status::Done, None), "@alice:x", at(26));
        let mut c = task(3, "Door");
        c.apply(Change::Status(Status::Done, None), "@bob:x", at(26));
        let old = {
            let mut t = task(4, "Old");
            t.apply(Change::Status(Status::Done, None), "@bob:x", at(1));
            t
        };
        let state = state_with(vec![a, b, c, old, task(5, "Open one")]);

        let text = weekly_message(&state, at(21)).unwrap();
        assert!(
            text.contains("3 tasks done\n🙌 Thanks Alice (2), Bob!"),
            "{text}"
        );
        assert!(text.contains("📋 1 open"));

        assert!(weekly_message(&State::default(), at(21)).is_none());
    }
}
