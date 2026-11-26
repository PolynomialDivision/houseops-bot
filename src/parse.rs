//! Text → intent: quick-add syntax, chat commands, due dates, people.

use std::collections::BTreeMap;

use chrono::{Datelike, Duration, NaiveDate, Weekday};

use crate::{
    config::HouseConfig,
    model::{Draft, Priority, TaskId},
};

// ── Quick add ─────────────────────────────────────────────────────────────────

/// A `#tag` in quick-add text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tag {
    Category(String),
    Location(String),
}

/// `#repair` → a configured category (by key or label), `#kitchen` → a
/// configured location, anything else → a new location ("#roof_terrace" →
/// "Roof terrace").
pub fn resolve_tag(tag: &str, house: &HouseConfig) -> Tag {
    let words = tag.replace(['_', '-'], " ");
    let norm = words.trim().to_lowercase();
    if let Some(c) = house
        .categories
        .iter()
        .find(|c| c.key.to_lowercase() == norm || c.label.to_lowercase() == norm)
    {
        return Tag::Category(c.key.clone());
    }
    if let Some(l) = house.locations.iter().find(|l| l.to_lowercase() == norm) {
        return Tag::Location(l.clone());
    }
    let mut chars = words.trim().chars();
    let name = match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    };
    Tag::Location(name)
}

/// Quick-add text: "Fix the tap #kitchen #repair !high". Tags and priority
/// markers may appear anywhere; everything else is the title. `None` if no
/// title is left.
pub fn parse_draft(text: &str, house: &HouseConfig) -> Option<Draft> {
    let mut draft = Draft::default();
    let mut title = Vec::new();
    for word in text.split_whitespace() {
        if let Some(tag) = word
            .strip_prefix('#')
            .filter(|t| !t.is_empty() && !t.chars().all(|c| c.is_ascii_digit()))
        {
            match resolve_tag(tag, house) {
                Tag::Category(key) => draft.category = Some(key),
                Tag::Location(name) if !name.is_empty() => draft.location = Some(name),
                Tag::Location(_) => {}
            }
        } else if let Some(p) = word.strip_prefix('!').and_then(Priority::parse) {
            draft.priority = Some(p);
        } else {
            title.push(word);
        }
    }
    draft.title = title.join(" ");
    (!draft.title.is_empty()).then_some(draft)
}

// ── Commands ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListFilter {
    Open,
    Mine,
    Waiting,
    Done,
}

/// Something to do to one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Show,
    Done,
    Claim,
    Unclaim,
    Drop,
    Reopen,
    Wait(Option<String>),
    Priority(Priority),
    /// Raw location text (`None` = clear).
    Location(Option<String>),
    /// Raw category text (`None` = clear).
    Category(Option<String>),
    /// Raw due text, parsed against the local date later.
    Due(String),
    /// Raw person reference ("me", a name, an MXID or a pill).
    Assign(String),
    Describe(String),
    Rename(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `!task <quick-add text>`
    Create(String),
    List(ListFilter),
    /// A task action; `id` is `None` when it comes from the task's thread.
    Act {
        id: Option<TaskId>,
        action: Action,
    },
    /// `!web`: send a personal sign-in link. `logout`: revoke all.
    Web {
        logout: bool,
    },
    Help,
    /// A known command used wrongly; the text says how to use it.
    Usage(&'static str),
}

fn clear_word(s: &str) -> bool {
    matches!(
        s.trim().to_lowercase().as_str(),
        "" | "none" | "clear" | "-"
    )
}

/// Parse a message starting with `!`. `None` for anything that is not ours
/// (other bots' commands pass through untouched).
pub fn parse_command(body: &str) -> Option<Command> {
    let body = body.trim();
    let rest = body.strip_prefix('!')?;
    let (verb, args) = match rest.split_once(char::is_whitespace) {
        Some((v, a)) => (v, a.trim()),
        None => (rest, ""),
    };
    let verb = verb.to_lowercase();

    // Commands that don't take a task.
    match verb.as_str() {
        "task" | "t" | "add" | "new" => {
            if let Ok(id) = args.trim_start_matches('#').parse::<TaskId>() {
                return Some(Command::Act {
                    id: Some(id),
                    action: Action::Show,
                });
            }
            return Some(if args.is_empty() {
                Command::Usage(
                    "!task <what needs doing>\n💡 e.g. !task Fix the kitchen tap #kitchen !high",
                )
            } else {
                Command::Create(args.to_owned())
            });
        }
        "tasks" | "todo" | "list" => {
            let filter = match args.to_lowercase().as_str() {
                "" | "open" => ListFilter::Open,
                "mine" | "me" => ListFilter::Mine,
                "waiting" => ListFilter::Waiting,
                "done" | "closed" => ListFilter::Done,
                _ => return Some(Command::Usage("!tasks [mine|waiting|done]")),
            };
            return Some(Command::List(filter));
        }
        "mine" => return Some(Command::List(ListFilter::Mine)),
        "web" | "login" => {
            return Some(Command::Web {
                logout: matches!(args.to_lowercase().as_str(), "logout" | "revoke" | "off"),
            })
        }
        "help" | "house" | "houseops" => return Some(Command::Help),
        _ => {}
    }

    // Task commands: `!verb [#]<id> args` or, inside a task thread, `!verb args`.
    let (id, args) = split_id(args);
    let action = match verb.as_str() {
        "show" | "info" => Action::Show,
        "done" | "fixed" | "finish" => Action::Done,
        "claim" | "take" => Action::Claim,
        "unclaim" | "release" => Action::Unclaim,
        "drop" | "cancel" | "wontfix" => Action::Drop,
        "reopen" | "open" => Action::Reopen,
        "wait" | "waiting" | "blocked" => Action::Wait((!args.is_empty()).then(|| args.to_owned())),
        "prio" | "priority" => match Priority::parse(args) {
            Some(p) => Action::Priority(p),
            None => return Some(Command::Usage("!prio low|normal|high|urgent")),
        },
        "loc" | "location" | "where" => {
            Action::Location((!clear_word(args)).then(|| args.to_owned()))
        }
        "cat" | "category" => Action::Category((!clear_word(args)).then(|| args.to_owned())),
        "due" => {
            if args.is_empty() {
                return Some(Command::Usage(
                    "!due fri | tomorrow | in 3 days | 2026-10-03 | none",
                ));
            }
            Action::Due(args.to_owned())
        }
        "assign" | "give" => {
            if args.is_empty() {
                return Some(Command::Usage("!assign @someone (or: me, none)"));
            }
            Action::Assign(args.to_owned())
        }
        "desc" | "describe" | "details" | "note" => Action::Describe(args.to_owned()),
        "rename" | "title" => {
            if args.is_empty() {
                return Some(Command::Usage("!rename <new title>"));
            }
            Action::Rename(args.to_owned())
        }
        _ => return None,
    };
    Some(Command::Act { id, action })
}

/// Leading `12` or `#12` → the task ID and the rest.
fn split_id(args: &str) -> (Option<TaskId>, &str) {
    let (first, rest) = match args.split_once(char::is_whitespace) {
        Some((f, r)) => (f, r.trim()),
        None => (args, ""),
    };
    match first.trim_start_matches('#').parse::<TaskId>() {
        Ok(id) => (Some(id), rest),
        Err(_) => (None, args),
    }
}

// ── Due dates ─────────────────────────────────────────────────────────────────

fn weekday_from(s: &str) -> Option<Weekday> {
    Some(match s {
        "mon" | "monday" => Weekday::Mon,
        "tue" | "tues" | "tuesday" => Weekday::Tue,
        "wed" | "wednesday" => Weekday::Wed,
        "thu" | "thur" | "thurs" | "thursday" => Weekday::Thu,
        "fri" | "friday" => Weekday::Fri,
        "sat" | "saturday" => Weekday::Sat,
        "sun" | "sunday" => Weekday::Sun,
        _ => return None,
    })
}

/// A due date as people type it, relative to `today`. `Ok(None)` clears it;
/// `Err` means it could not be read.
pub fn parse_due(text: &str, today: NaiveDate) -> Result<Option<NaiveDate>, ()> {
    let t = text.trim().to_lowercase();
    if clear_word(&t) {
        return Ok(None);
    }
    let date = match t.as_str() {
        "today" | "tonight" => today,
        "tomorrow" | "tmrw" => today + Duration::days(1),
        "weekend" | "this weekend" => {
            let until_sat = (Weekday::Sat.num_days_from_monday() as i64
                - today.weekday().num_days_from_monday() as i64)
                .rem_euclid(7);
            today + Duration::days(until_sat)
        }
        "next week" => today + Duration::days(7 - today.weekday().num_days_from_monday() as i64),
        _ => {
            let t = t.strip_prefix("next ").unwrap_or(&t);
            if let Some(wd) = weekday_from(t) {
                // The coming such day, never today.
                let ahead = (wd.num_days_from_monday() as i64
                    - today.weekday().num_days_from_monday() as i64)
                    .rem_euclid(7);
                today + Duration::days(if ahead == 0 { 7 } else { ahead })
            } else if let Some(n) = relative_days(t) {
                today + Duration::days(n)
            } else if let Ok(d) = NaiveDate::parse_from_str(t, "%Y-%m-%d") {
                d
            } else {
                day_month(t, today).ok_or(())?
            }
        }
    };
    Ok(Some(date))
}

/// "in 3 days", "3d", "2 weeks", "in 1w".
fn relative_days(t: &str) -> Option<i64> {
    let t = t.strip_prefix("in ").unwrap_or(t).replace(' ', "");
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    let n: i64 = digits.parse().ok()?;
    match &t[digits.len()..] {
        "d" | "day" | "days" => Some(n),
        "w" | "wk" | "week" | "weeks" => Some(n * 7),
        _ => None,
    }
}

/// "3.10." / "3.10" / "3.10.2026" / "03/10": the next such day.
fn day_month(t: &str, today: NaiveDate) -> Option<NaiveDate> {
    let parts: Vec<&str> = t.split(['.', '/']).filter(|p| !p.is_empty()).collect();
    let (day, month): (u32, u32) = (parts.first()?.parse().ok()?, parts.get(1)?.parse().ok()?);
    if let Some(year) = parts.get(2) {
        let mut year: i32 = year.parse().ok()?;
        if year < 100 {
            year += 2000;
        }
        return NaiveDate::from_ymd_opt(year, month, day);
    }
    let this_year = NaiveDate::from_ymd_opt(today.year(), month, day)?;
    if this_year >= today {
        Some(this_year)
    } else {
        NaiveDate::from_ymd_opt(today.year() + 1, month, day)
    }
}

// ── People ────────────────────────────────────────────────────────────────────

/// First Matrix user ID in `text` (plain "@user:server" or a matrix.to link).
pub fn find_mxid(text: &str) -> Option<String> {
    let text = text
        .replace("https://matrix.to/#/", " ")
        .replace("%40", "@")
        .replace("%3A", ":");
    text.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '(' | ')' | ','))
        .map(|w| w.trim_end_matches(['.', '!', '?', ';']))
        .find(|w| {
            w.starts_with('@')
                && w[1..].split_once(':').is_some_and(|(local, server)| {
                    !local.is_empty() && server.contains('.') || server == "localhost"
                })
        })
        .map(str::to_owned)
}

/// Who `raw` refers to: "me", "none", a pill/MXID (also looked for in the
/// message's HTML), or a unique display-name/localpart match among
/// `members`. `Ok(None)` = nobody; `Err` = could not tell.
pub fn resolve_person(
    raw: &str,
    html: Option<&str>,
    sender: &str,
    members: &BTreeMap<String, String>,
) -> Result<Option<String>, ()> {
    let t = raw.trim().trim_start_matches('@').to_lowercase();
    if matches!(t.as_str(), "me" | "myself" | "i") {
        return Ok(Some(sender.to_owned()));
    }
    if clear_word(&t) || t == "nobody" {
        return Ok(None);
    }
    if let Some(mxid) = html.and_then(find_mxid).or_else(|| find_mxid(raw)) {
        return Ok(Some(mxid));
    }
    let matches: Vec<&String> = members
        .iter()
        .filter(|(id, name)| {
            name.to_lowercase() == t
                || crate::model::localpart(id).to_lowercase() == t
                || name.to_lowercase().split_whitespace().next() == Some(t.as_str())
        })
        .map(|(id, _)| id)
        .collect();
    match matches.as_slice() {
        [one] => Ok(Some((*one).clone())),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn house() -> HouseConfig {
        toml::from_str("room_id = \"!r:x\"").unwrap()
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn quick_add_pulls_tags_and_priority_out_of_the_title() {
        let d = parse_draft("Fix the tap #kitchen #repair !high", &house()).unwrap();
        assert_eq!(d.title, "Fix the tap");
        assert_eq!(d.location.as_deref(), Some("Kitchen"));
        assert_eq!(d.category.as_deref(), Some("repair"));
        assert_eq!(d.priority, Some(Priority::High));

        let d = parse_draft("#roof_terrace Leaves in the gutter !urgent", &house()).unwrap();
        assert_eq!(d.title, "Leaves in the gutter");
        assert_eq!(d.location.as_deref(), Some("Roof terrace"));
        assert_eq!(d.priority, Some(Priority::Urgent));

        // "#3" and exclamation marks in prose stay in the title.
        let d = parse_draft("Buy #3 batteries now!", &house()).unwrap();
        assert_eq!(d.title, "Buy #3 batteries now!");

        assert!(parse_draft("#kitchen !high", &house()).is_none());
    }

    #[test]
    fn commands_with_and_without_task_numbers() {
        assert_eq!(
            parse_command("!task Fix the tap"),
            Some(Command::Create("Fix the tap".into()))
        );
        assert_eq!(
            parse_command("!task #12"),
            Some(Command::Act {
                id: Some(12),
                action: Action::Show
            })
        );
        assert_eq!(
            parse_command("!done 12"),
            Some(Command::Act {
                id: Some(12),
                action: Action::Done
            })
        );
        assert_eq!(
            parse_command("!done"),
            Some(Command::Act {
                id: None,
                action: Action::Done
            })
        );
        assert_eq!(
            parse_command("!wait #4 new valve ordered"),
            Some(Command::Act {
                id: Some(4),
                action: Action::Wait(Some("new valve ordered".into()))
            })
        );
        assert_eq!(
            parse_command("!prio urgent"),
            Some(Command::Act {
                id: None,
                action: Action::Priority(Priority::Urgent)
            })
        );
        assert_eq!(
            parse_command("!loc none"),
            Some(Command::Act {
                id: None,
                action: Action::Location(None)
            })
        );
        assert_eq!(
            parse_command("!tasks mine"),
            Some(Command::List(ListFilter::Mine))
        );
        assert_eq!(
            parse_command("!web logout"),
            Some(Command::Web { logout: true })
        );
        assert!(matches!(
            parse_command("!prio whenever"),
            Some(Command::Usage(_))
        ));
        assert!(matches!(parse_command("!task"), Some(Command::Usage(_))));
        // Not ours: left to other bots.
        assert_eq!(parse_command("!garbage week"), None);
        assert_eq!(parse_command("hello"), None);
    }

    #[test]
    fn due_dates_read_like_people_write_them() {
        let sat = date(2026, 9, 26); // a Saturday
        assert_eq!(parse_due("today", sat), Ok(Some(sat)));
        assert_eq!(parse_due("tomorrow", sat), Ok(Some(date(2026, 9, 27))));
        assert_eq!(parse_due("fri", sat), Ok(Some(date(2026, 10, 2))));
        assert_eq!(
            parse_due("sat", sat),
            Ok(Some(date(2026, 10, 3))),
            "never today"
        );
        assert_eq!(parse_due("weekend", sat), Ok(Some(sat)));
        assert_eq!(parse_due("next week", sat), Ok(Some(date(2026, 9, 28))));
        assert_eq!(parse_due("in 3 days", sat), Ok(Some(date(2026, 9, 29))));
        assert_eq!(parse_due("2w", sat), Ok(Some(date(2026, 10, 10))));
        assert_eq!(parse_due("3.10.", sat), Ok(Some(date(2026, 10, 3))));
        assert_eq!(
            parse_due("1.2.", sat),
            Ok(Some(date(2027, 2, 1))),
            "rolls to next year"
        );
        assert_eq!(parse_due("2026-12-24", sat), Ok(Some(date(2026, 12, 24))));
        assert_eq!(parse_due("none", sat), Ok(None));
        assert_eq!(parse_due("whenever", sat), Err(()));
    }

    #[test]
    fn people_are_found_by_pill_mxid_name_or_me() {
        let members = BTreeMap::from([
            ("@alice:example.org".to_owned(), "Alice Smith".to_owned()),
            ("@bob:example.org".to_owned(), "Bob".to_owned()),
        ]);
        let r =
            |raw: &str, html: Option<&str>| resolve_person(raw, html, "@me:example.org", &members);

        assert_eq!(r("me", None), Ok(Some("@me:example.org".into())));
        assert_eq!(r("none", None), Ok(None));
        assert_eq!(
            r("@carol:example.org", None),
            Ok(Some("@carol:example.org".into()))
        );
        assert_eq!(
            r(
                "Alice",
                Some(r#"!assign <a href="https://matrix.to/#/@alice:example.org">Alice</a>"#)
            ),
            Ok(Some("@alice:example.org".into()))
        );
        assert_eq!(r("alice", None), Ok(Some("@alice:example.org".into())));
        assert_eq!(r("bob", None), Ok(Some("@bob:example.org".into())));
        assert_eq!(r("zed", None), Err(()));
    }
}
