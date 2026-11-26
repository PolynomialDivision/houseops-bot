//! Tasks and the one way they change.
//!
//! Matrix commands, reactions and the web interface all describe an edit as
//! a [`Change`] and apply it with [`Task::apply`], which also records the
//! history entry, so both interfaces behave identically by construction.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

pub type TaskId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Nobody is on it yet.
    Open,
    /// Someone is working on it.
    Doing,
    /// Blocked on something outside the house (parts, landlord, …).
    Waiting,
    Done,
    /// Won't be done.
    Dropped,
}

impl Status {
    pub fn is_closed(self) -> bool {
        matches!(self, Status::Done | Status::Dropped)
    }

    pub fn label(self) -> &'static str {
        match self {
            Status::Open => "Open",
            Status::Doing => "In progress",
            Status::Waiting => "Waiting",
            Status::Done => "Done",
            Status::Dropped => "Dropped",
        }
    }

    pub fn emoji(self) -> &'static str {
        match self {
            Status::Open => "🟠",
            Status::Doing => "🔵",
            Status::Waiting => "⏸️",
            Status::Done => "✅",
            Status::Dropped => "🚫",
        }
    }

    pub fn parse(s: &str) -> Option<Status> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "open" | "todo" | "new" => Status::Open,
            "doing" | "in_progress" | "progress" | "started" => Status::Doing,
            "waiting" | "wait" | "blocked" => Status::Waiting,
            "done" | "closed" | "fixed" => Status::Done,
            "dropped" | "drop" | "wontfix" | "cancelled" => Status::Dropped,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Normal,
    High,
    Urgent,
}

impl Priority {
    pub fn label(self) -> &'static str {
        match self {
            Priority::Low => "Low",
            Priority::Normal => "Normal",
            Priority::High => "High",
            Priority::Urgent => "Urgent",
        }
    }

    /// Marker shown next to a title; normal priority stays unmarked.
    pub fn emoji(self) -> &'static str {
        match self {
            Priority::Low => "💤",
            Priority::Normal => "",
            Priority::High => "❗",
            Priority::Urgent => "🔥",
        }
    }

    pub fn parse(s: &str) -> Option<Priority> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "low" | "later" | "someday" => Priority::Low,
            "normal" | "medium" | "default" => Priority::Normal,
            "high" | "important" | "soon" => Priority::High,
            "urgent" | "asap" | "emergency" => Priority::Urgent,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Random hex ID; the file lives at `<store>/attachments/<id>`.
    pub id: String,
    pub file_name: String,
    pub mime: String,
    pub size: u64,
    pub added_by: String,
    pub added_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistoryKind {
    Created,
    Comment {
        text: String,
    },
    /// A human-readable description of an edit: "status → Done".
    Changed {
        what: String,
    },
    Attachment {
        file_name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub at: DateTime<Utc>,
    /// Matrix user ID of whoever made the change.
    pub actor: String,
    #[serde(flatten)]
    pub kind: HistoryKind,
}

/// Where a task's card message lives in Matrix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardRef {
    pub room_id: String,
    pub event_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub status: Status,
    pub priority: Priority,
    #[serde(default)]
    pub location: Option<String>,
    /// Key of a configured category.
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    /// What a waiting task is waiting for.
    #[serde(default)]
    pub waiting_on: Option<String>,
    #[serde(default)]
    pub due: Option<NaiveDate>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub closed_at: Option<DateTime<Utc>>,
    /// Who closed it (done or dropped).
    #[serde(default)]
    pub closed_by: Option<String>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
    #[serde(default)]
    pub card: Option<CardRef>,
    /// Last day a reminder mentioned this task, so nobody is nagged daily.
    #[serde(default)]
    pub last_reminded: Option<NaiveDate>,
}

/// A task as quick-add text describes it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Draft {
    pub title: String,
    pub priority: Option<Priority>,
    pub location: Option<String>,
    pub category: Option<String>,
}

/// One edit to a task.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Title(String),
    Description(String),
    /// New status, with what it waits for when `Waiting`.
    Status(Status, Option<String>),
    Priority(Priority),
    Location(Option<String>),
    Category(Option<String>),
    Assignee(Option<String>),
    Due(Option<NaiveDate>),
    Comment(String),
    AddAttachment(Attachment),
    RemoveAttachment(String),
}

impl Change {
    /// Worth a note in the Matrix thread when made from the web (field
    /// tweaks only update the card).
    pub fn is_notable(&self) -> bool {
        matches!(
            self,
            Change::Status(..)
                | Change::Assignee(_)
                | Change::Comment(_)
                | Change::AddAttachment(_)
        )
    }
}

fn clean(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn opt_clean(s: Option<String>) -> Option<String> {
    s.map(|s| clean(&s)).filter(|s| !s.is_empty())
}

impl Task {
    pub fn new(id: TaskId, draft: Draft, actor: &str, now: DateTime<Utc>) -> Task {
        Task {
            id,
            title: clean(&draft.title),
            description: String::new(),
            status: Status::Open,
            priority: draft.priority.unwrap_or(Priority::Normal),
            location: opt_clean(draft.location),
            category: draft.category,
            assignee: None,
            waiting_on: None,
            due: None,
            created_by: actor.to_owned(),
            created_at: now,
            updated_at: now,
            closed_at: None,
            closed_by: None,
            attachments: Vec::new(),
            history: vec![HistoryEntry {
                at: now,
                actor: actor.to_owned(),
                kind: HistoryKind::Created,
            }],
            card: None,
            last_reminded: None,
        }
    }

    pub fn is_open(&self) -> bool {
        !self.status.is_closed()
    }

    /// Apply `change` by `actor`. Returns the history entry it produced, or
    /// `None` when nothing actually changed (so repeated taps are harmless).
    pub fn apply(
        &mut self,
        change: Change,
        actor: &str,
        now: DateTime<Utc>,
    ) -> Option<HistoryKind> {
        let kind = match change {
            Change::Title(title) => {
                let title = clean(&title);
                if title.is_empty() || title == self.title {
                    return None;
                }
                self.title = title.clone();
                HistoryKind::Changed {
                    what: format!("renamed to “{title}”"),
                }
            }
            Change::Description(text) => {
                let text = text.trim().to_owned();
                if text == self.description {
                    return None;
                }
                self.description = text;
                HistoryKind::Changed {
                    what: "updated the description".to_owned(),
                }
            }
            Change::Status(status, reason) => {
                let reason = opt_clean(reason);
                if status == self.status && (status != Status::Waiting || reason == self.waiting_on)
                {
                    return None;
                }
                self.status = status;
                self.waiting_on = if status == Status::Waiting {
                    reason.clone()
                } else {
                    None
                };
                if status.is_closed() {
                    self.closed_at = Some(now);
                    self.closed_by = Some(actor.to_owned());
                } else {
                    self.closed_at = None;
                    self.closed_by = None;
                }
                let what = match (status, reason) {
                    (Status::Waiting, Some(r)) => format!("waiting on {r}"),
                    (Status::Open, _) if self.history_has_close() => "reopened".to_owned(),
                    (s, _) => format!("marked {}", s.label().to_lowercase()),
                };
                HistoryKind::Changed { what }
            }
            Change::Priority(priority) => {
                if priority == self.priority {
                    return None;
                }
                self.priority = priority;
                HistoryKind::Changed {
                    what: format!("set the priority to {}", priority.label().to_lowercase()),
                }
            }
            Change::Location(location) => {
                let location = opt_clean(location);
                if location == self.location {
                    return None;
                }
                self.location = location.clone();
                HistoryKind::Changed {
                    what: match location {
                        Some(l) => format!("moved it to {l}"),
                        None => "cleared the location".to_owned(),
                    },
                }
            }
            Change::Category(category) => {
                if category == self.category {
                    return None;
                }
                self.category = category.clone();
                HistoryKind::Changed {
                    what: match category {
                        Some(c) => format!("filed it under {c}"),
                        None => "cleared the category".to_owned(),
                    },
                }
            }
            Change::Assignee(assignee) => {
                if assignee == self.assignee {
                    return None;
                }
                // Taking a task starts it; giving it back reopens it.
                match (&assignee, self.status) {
                    (Some(_), Status::Open) => self.status = Status::Doing,
                    (None, Status::Doing) => self.status = Status::Open,
                    _ => {}
                }
                self.assignee = assignee.clone();
                HistoryKind::Changed {
                    what: match assignee {
                        Some(user) if user == actor => "claimed it".to_owned(),
                        Some(user) => format!("assigned it to {user}"),
                        None => "unassigned it".to_owned(),
                    },
                }
            }
            Change::Due(due) => {
                if due == self.due {
                    return None;
                }
                self.due = due;
                HistoryKind::Changed {
                    what: match due {
                        Some(d) => format!("set the due date to {}", d.format("%a %-d %b")),
                        None => "cleared the due date".to_owned(),
                    },
                }
            }
            Change::Comment(text) => {
                let text = text.trim().to_owned();
                if text.is_empty() {
                    return None;
                }
                HistoryKind::Comment { text }
            }
            Change::AddAttachment(attachment) => {
                let file_name = attachment.file_name.clone();
                self.attachments.push(attachment);
                HistoryKind::Attachment { file_name }
            }
            Change::RemoveAttachment(id) => {
                let before = self.attachments.len();
                self.attachments.retain(|a| a.id != id);
                if self.attachments.len() == before {
                    return None;
                }
                HistoryKind::Changed {
                    what: "removed a photo".to_owned(),
                }
            }
        };
        self.updated_at = now;
        self.history.push(HistoryEntry {
            at: now,
            actor: actor.to_owned(),
            kind: kind.clone(),
        });
        Some(kind)
    }

    fn history_has_close(&self) -> bool {
        self.history.iter().any(|h| {
            matches!(&h.kind, HistoryKind::Changed { what } if what.starts_with("marked done") || what.starts_with("marked dropped"))
        })
    }
}

/// A browser signed in through a personal link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// SHA-256 of the token; the token itself is never stored.
    pub token_hash: String,
    pub user_id: String,
    pub created_at: DateTime<Utc>,
}

/// Which ✋ reaction claimed which task, so retracting it unclaims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimReaction {
    pub task: TaskId,
    pub user_id: String,
}

/// Everything the bot persists (`store/state.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub next_id: TaskId,
    #[serde(default)]
    pub tasks: BTreeMap<TaskId, Task>,
    #[serde(default)]
    pub sessions: Vec<Session>,
    /// Reaction event ID → claim it made.
    #[serde(default)]
    pub claim_reactions: BTreeMap<String, ClaimReaction>,
    /// Display names of people seen in the house room (user ID → name).
    #[serde(default)]
    pub members: BTreeMap<String, String>,
    /// Bumped on every change; the web interface polls it.
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub last_digest: Option<NaiveDate>,
    #[serde(default)]
    pub last_weekly: Option<NaiveDate>,
}

impl State {
    pub fn task_by_card(&self, event_id: &str) -> Option<TaskId> {
        self.tasks
            .values()
            .find(|t| t.card.as_ref().is_some_and(|c| c.event_id == event_id))
            .map(|t| t.id)
    }

    /// Display name for a user ID: the known name, else the localpart.
    pub fn name_of(&self, user_id: &str) -> String {
        self.members
            .get(user_id)
            .cloned()
            .unwrap_or_else(|| localpart(user_id).to_owned())
    }
}

pub fn localpart(user_id: &str) -> &str {
    user_id
        .strip_prefix('@')
        .and_then(|u| u.split(':').next())
        .unwrap_or(user_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-27T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn task() -> Task {
        Task::new(
            1,
            Draft {
                title: "  Fix   the tap ".into(),
                ..Default::default()
            },
            "@bob:x",
            now(),
        )
    }

    #[test]
    fn new_tasks_are_open_normal_and_record_their_creation() {
        let t = task();
        assert_eq!(t.title, "Fix the tap");
        assert_eq!(t.status, Status::Open);
        assert_eq!(t.priority, Priority::Normal);
        assert_eq!(t.history.len(), 1);
    }

    #[test]
    fn claiming_starts_a_task_and_giving_it_back_reopens_it() {
        let mut t = task();
        assert!(t
            .apply(Change::Assignee(Some("@alice:x".into())), "@alice:x", now())
            .is_some());
        assert_eq!(t.status, Status::Doing);
        assert_eq!(
            t.history.last().unwrap().kind,
            HistoryKind::Changed {
                what: "claimed it".into()
            }
        );

        t.apply(Change::Assignee(None), "@alice:x", now());
        assert_eq!(t.status, Status::Open);
    }

    #[test]
    fn repeating_a_change_is_a_no_op() {
        let mut t = task();
        assert!(t
            .apply(Change::Priority(Priority::High), "@a:x", now())
            .is_some());
        assert!(t
            .apply(Change::Priority(Priority::High), "@a:x", now())
            .is_none());
        assert!(t
            .apply(Change::Title("Fix the tap".into()), "@a:x", now())
            .is_none());
        assert!(t
            .apply(Change::Comment("  ".into()), "@a:x", now())
            .is_none());
        assert_eq!(t.history.len(), 2);
    }

    #[test]
    fn closing_records_who_and_when_and_reopening_clears_it() {
        let mut t = task();
        t.apply(Change::Status(Status::Done, None), "@alice:x", now());
        assert_eq!(t.closed_by.as_deref(), Some("@alice:x"));
        assert!(t.closed_at.is_some());

        let reopened = t.apply(Change::Status(Status::Open, None), "@bob:x", now());
        assert_eq!(
            reopened,
            Some(HistoryKind::Changed {
                what: "reopened".into()
            })
        );
        assert!(t.closed_at.is_none());
    }

    #[test]
    fn waiting_keeps_its_reason_until_the_task_moves_on() {
        let mut t = task();
        t.apply(
            Change::Status(Status::Waiting, Some("new valve".into())),
            "@a:x",
            now(),
        );
        assert_eq!(t.waiting_on.as_deref(), Some("new valve"));
        // A new reason is a real change.
        assert!(t
            .apply(
                Change::Status(Status::Waiting, Some("landlord".into())),
                "@a:x",
                now()
            )
            .is_some());
        t.apply(Change::Status(Status::Doing, None), "@a:x", now());
        assert_eq!(t.waiting_on, None);
    }

    #[test]
    fn names_fall_back_to_the_localpart() {
        let mut state = State::default();
        state.members.insert("@alice:x".into(), "Alice".into());
        assert_eq!(state.name_of("@alice:x"), "Alice");
        assert_eq!(state.name_of("@bob:example.org"), "bob");
    }
}
