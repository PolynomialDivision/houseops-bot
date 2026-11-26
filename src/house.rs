//! The house: the task store plus its Matrix presence. Both interfaces go
//! through [`House::create`] and [`House::change`], which update the store,
//! then bring the task's card in the house room up to date, so Matrix and
//! the web always show the same thing.

use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, Utc};
use mxbot_common::{
    format,
    matrix_sdk::{
        attachment::AttachmentConfig,
        room::reply::{EnforceThread, Reply},
        ruma::{
            events::{
                reaction::ReactionEventContent,
                relation::Annotation,
                room::message::{
                    AddMentions, ReplacementMetadata, ReplyWithinThread, RoomMessageEventContent,
                },
                Mentions,
            },
            OwnedEventId, OwnedRoomId, OwnedUserId,
        },
        Room,
    },
    send, Bot,
};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::{
    config::{HouseConfig, Settings},
    model::{CardRef, Change, Draft, HistoryKind, Priority, Task, TaskId},
    render,
    store::Store,
};

/// Where a change came from. Changes made on the web are announced in the
/// task's thread; in Matrix, the command or reaction itself is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Matrix,
    Web,
}

#[derive(Clone)]
pub struct House {
    inner: Arc<Inner>,
}

struct Inner {
    /// `None` only in tests, which exercise the web interface without Matrix.
    bot: Option<Bot>,
    store: Store,
    config: HouseConfig,
    room_id: OwnedRoomId,
    public_url: Option<String>,
    settings: mxbot_common::settings::Settings<Settings>,
    /// Serialises card edits, so an older rendering never lands last.
    card_lock: Mutex<()>,
}

/// The local calendar date (the container's `TZ`).
pub fn today() -> NaiveDate {
    Local::now().date_naive()
}

impl House {
    pub fn new(
        bot: Bot,
        store: Store,
        config: HouseConfig,
        public_url: Option<String>,
        settings: mxbot_common::settings::Settings<Settings>,
    ) -> Result<House> {
        let room_id = OwnedRoomId::try_from(config.room_id.as_str())
            .with_context(|| format!("[house] room_id {:?} is not a room ID", config.room_id))?;
        Ok(House {
            inner: Arc::new(Inner {
                bot: Some(bot),
                store,
                config,
                room_id,
                public_url,
                settings,
                card_lock: Mutex::new(()),
            }),
        })
    }

    /// Without Matrix (tests): the web interface works, cards are skipped.
    #[cfg(test)]
    pub async fn offline(store: Store, config: HouseConfig, public_url: Option<String>) -> House {
        let dir = tempfile::tempdir().unwrap().keep();
        let settings = mxbot_common::settings::Settings::load(
            dir.join("settings.json"),
            Settings::from_config(&Default::default()),
        )
        .await
        .unwrap();
        House {
            inner: Arc::new(Inner {
                bot: None,
                room_id: OwnedRoomId::try_from(config.room_id.as_str()).unwrap(),
                store,
                config,
                public_url,
                settings,
                card_lock: Mutex::new(()),
            }),
        }
    }

    /// The Matrix bot. Only the Matrix handlers use this, and they only run
    /// with one.
    pub fn bot(&self) -> &Bot {
        self.inner.bot.as_ref().expect("House without Matrix")
    }
    pub fn store(&self) -> &Store {
        &self.inner.store
    }
    pub fn config(&self) -> &HouseConfig {
        &self.inner.config
    }
    pub fn public_url(&self) -> Option<&str> {
        self.inner.public_url.as_deref()
    }
    pub fn settings(&self) -> Settings {
        self.inner.settings.get()
    }
    pub fn room_id(&self) -> &OwnedRoomId {
        &self.inner.room_id
    }

    pub fn room(&self) -> Option<Room> {
        self.inner
            .bot
            .as_ref()?
            .client
            .get_room(&self.inner.room_id)
    }

    // ── Writes ────────────────────────────────────────────────────────────────

    /// Create a task and post its card.
    pub async fn create(&self, draft: Draft, actor: &str) -> Task {
        let task = self.inner.store.create(draft, actor, Utc::now()).await;
        info!("Task #{} created by {actor}: {}", task.id, task.title);
        self.publish_card(&task).await;
        task
    }

    /// Apply `changes` to task `id` and bring Matrix up to date.
    pub async fn change(
        &self,
        id: TaskId,
        actor: &str,
        changes: Vec<Change>,
        origin: Origin,
    ) -> Result<Task> {
        let updated = self
            .inner
            .store
            .update(id, actor, changes, Utc::now())
            .await?;
        if updated.changes.is_empty() {
            return Ok(updated.task);
        }
        self.refresh_card(id).await;

        // Tell a newly assigned person (unless they took it themselves).
        for (change, _) in &updated.changes {
            if let Change::Assignee(Some(user)) = change {
                if user != actor {
                    let text = format!("👋 {user}, {actor} assigned this to you.");
                    self.thread_note(&updated.task, &text, vec![user.clone()])
                        .await;
                }
            }
        }
        if origin == Origin::Web {
            let notes: Vec<String> = updated
                .changes
                .iter()
                .filter(|(change, _)| {
                    change.is_notable()
                        && !matches!(change, Change::Assignee(Some(u)) if u != actor)
                })
                .map(|(_, kind)| web_note(actor, kind))
                .collect();
            if !notes.is_empty() {
                self.thread_note(&updated.task, &notes.join("\n"), Vec::new())
                    .await;
            }
        }
        Ok(updated.task)
    }

    /// Store a photo sent from the web, post it into the task's thread (so
    /// it is visible in Matrix too) and attach it to the task.
    pub async fn add_web_attachment(
        &self,
        id: TaskId,
        actor: &str,
        file_name: &str,
        mime: &str,
        data: Vec<u8>,
    ) -> Result<Task> {
        let attachment = self
            .inner
            .store
            .save_attachment(file_name, mime, &data, actor, Utc::now())
            .await?;
        let task = self
            .change(
                id,
                actor,
                vec![Change::AddAttachment(attachment)],
                Origin::Web,
            )
            .await?;
        if let (Some(room), Some(card)) = (self.room(), task.card.as_ref()) {
            if let (Ok(event_id), Ok(content_type)) = (
                card.event_id.parse::<OwnedEventId>(),
                mime.parse::<mime::Mime>(),
            ) {
                let config = AttachmentConfig::new().reply(Some(Reply {
                    event_id,
                    enforce_thread: EnforceThread::Threaded(ReplyWithinThread::No),
                    add_mentions: AddMentions::No,
                }));
                if let Err(e) = room
                    .send_attachment(file_name, &content_type, data, config)
                    .await
                {
                    warn!("Could not post photo for #{id} to Matrix: {e}");
                }
            }
        }
        Ok(task)
    }

    // ── Cards ─────────────────────────────────────────────────────────────────

    fn render_card(&self, task: &Task, state: &crate::model::State) -> render::Message {
        render::card(task, state, &self.inner.config, self.public_url(), today())
    }

    /// Post a new task's card, offer ✋/✅ as one-tap reactions, and remember
    /// where the card lives.
    async fn publish_card(&self, task: &Task) {
        let Some(room) = self.room() else {
            warn!(
                "Not in the house room {}: card for #{} not posted",
                self.inner.room_id, task.id
            );
            return;
        };
        let (mut plain, mut html) = {
            let state = self.inner.store.read().await;
            self.render_card(task, &state)
        };
        let mut mentions = Mentions::new();
        if task.priority == Priority::Urgent && self.settings().urgent_pings_room {
            plain = format!("@room {plain}");
            html = format!("@room {html}");
            mentions.room = true;
        }
        let content = RoomMessageEventContent::text_html(plain, html).add_mentions(mentions);
        match send::send_with_retry(&room, content).await {
            Ok(event_id) => {
                self.inner
                    .store
                    .set_card(
                        task.id,
                        CardRef {
                            room_id: room.room_id().to_string(),
                            event_id: event_id.to_string(),
                        },
                    )
                    .await;
                for key in ["✋", "✅"] {
                    let reaction = ReactionEventContent::new(Annotation::new(
                        event_id.clone(),
                        key.to_owned(),
                    ));
                    if let Err(e) = room.send(reaction).await {
                        warn!("Could not add {key} to card #{}: {e}", task.id);
                    }
                }
            }
            Err(e) => error!("Could not post card for #{}: {e}", task.id),
        }
    }

    /// Re-render task `id`'s card in place.
    pub async fn refresh_card(&self, id: TaskId) {
        let _serialised = self.inner.card_lock.lock().await;
        let (task, rendered) = {
            let state = self.inner.store.read().await;
            let Some(task) = state.tasks.get(&id).cloned() else {
                return;
            };
            let rendered = self.render_card(&task, &state);
            (task, rendered)
        };
        let Some(card) = task.card.as_ref() else {
            // Created while the house room was unreachable: post it now.
            drop(_serialised);
            self.publish_card(&task).await;
            return;
        };
        let (Some(room), Ok(event_id)) = (self.room(), card.event_id.parse::<OwnedEventId>())
        else {
            return;
        };
        let content = RoomMessageEventContent::text_html(rendered.0, rendered.1)
            // An urgent card mentioned @room when posted; saying so keeps the
            // edit from notifying anyone again.
            .make_replacement(ReplacementMetadata::new(
                event_id,
                Some(Mentions::with_room_mention()),
            ));
        if let Err(e) = send::send_with_retry(&room, content).await {
            warn!("Could not update card #{id}: {e}");
        }
    }

    /// A short message in the task's thread. `text` may contain MXIDs; only
    /// those in `notify` are notified.
    pub async fn thread_note(&self, task: &Task, text: &str, notify: Vec<String>) {
        let (Some(room), Some(card)) = (self.room(), task.card.as_ref()) else {
            return;
        };
        let Ok(root) = card.event_id.parse::<OwnedEventId>() else {
            return;
        };
        let names = self
            .inner
            .store
            .read()
            .await
            .members
            .clone()
            .into_iter()
            .collect();
        let mut content = format::mentionify_with_names(text, &names);
        content.mentions = Some(Mentions::with_user_ids(
            notify
                .iter()
                .filter_map(|u| OwnedUserId::try_from(u.as_str()).ok()),
        ));
        let content = send::in_thread(content, root.clone(), root);
        if let Err(e) = send::send_with_retry(&room, content).await {
            warn!("Could not post a note on #{}: {e}", task.id);
        }
    }
}

/// How a web change reads in the task's thread.
fn web_note(actor: &str, kind: &HistoryKind) -> String {
    match kind {
        HistoryKind::Comment { text } => format!("💬 {actor}: {text}"),
        HistoryKind::Changed { what } => format!("🌐 {actor} {what}"),
        HistoryKind::Attachment { file_name } => format!("📎 {actor} added {file_name}"),
        HistoryKind::Created => format!("🌐 {actor} created this"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_notes_name_the_person_and_what_they_did() {
        assert_eq!(
            web_note(
                "@a:x",
                &HistoryKind::Changed {
                    what: "marked done".into()
                }
            ),
            "🌐 @a:x marked done"
        );
        assert_eq!(
            web_note(
                "@a:x",
                &HistoryKind::Comment {
                    text: "on it".into()
                }
            ),
            "💬 @a:x: on it"
        );
    }
}
