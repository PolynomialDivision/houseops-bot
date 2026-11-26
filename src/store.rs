//! The single source of truth both interfaces read and write.
//!
//! State lives in memory behind one mutex and is written to
//! `store/state.json` after every change; attachment files sit next to it in
//! `store/attachments/`. Every change bumps `State::version`, which the web
//! interface polls and the Matrix side uses to refresh task cards.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use rand::RngCore;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, MutexGuard};
use tracing::error;

use crate::model::{
    Attachment, CardRef, Change, ClaimReaction, Draft, HistoryKind, Session, State, Task, TaskId,
};

/// Browsers per person that stay signed in; older sessions are dropped.
const MAX_SESSIONS_PER_USER: usize = 8;
/// A web sign-in ends after this long, like its cookie (`Max-Age`).
pub const SESSION_DAYS: i64 = 400;

pub struct Store {
    state: Mutex<State>,
    path: PathBuf,
    attachments_dir: PathBuf,
}

/// What an update did.
pub struct Updated {
    pub task: Task,
    /// History entries it added (empty: nothing changed).
    pub changes: Vec<(Change, HistoryKind)>,
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

impl Store {
    pub async fn open(store_dir: &Path) -> Result<Store> {
        let path = store_dir.join("state.json");
        let state: State = mxbot_common::persist::load_json_or_default(&path).await?;
        let attachments_dir = store_dir.join("attachments");
        tokio::fs::create_dir_all(&attachments_dir)
            .await
            .with_context(|| format!("Creating {}", attachments_dir.display()))?;
        Ok(Store {
            state: Mutex::new(state),
            path,
            attachments_dir,
        })
    }

    /// Read access. Keep the guard short. Every change waits on it.
    pub async fn read(&self) -> MutexGuard<'_, State> {
        self.state.lock().await
    }

    async fn save(&self, state: &mut State) {
        state.version += 1;
        if let Err(e) = mxbot_common::persist::save_json_atomic(&self.path, &*state).await {
            error!("Could not save state: {e:#}");
        }
    }

    pub async fn create(&self, draft: Draft, actor: &str, now: DateTime<Utc>) -> Task {
        let mut state = self.state.lock().await;
        state.next_id = state
            .next_id
            .max(state.tasks.keys().max().copied().unwrap_or(0))
            + 1;
        let task = Task::new(state.next_id, draft, actor, now);
        state.tasks.insert(task.id, task.clone());
        self.save(&mut state).await;
        task
    }

    /// Apply `changes` in order. Changes that turn out to be no-ops are
    /// dropped from the result.
    pub async fn update(
        &self,
        id: TaskId,
        actor: &str,
        changes: Vec<Change>,
        now: DateTime<Utc>,
    ) -> Result<Updated> {
        let mut state = self.state.lock().await;
        let Some(task) = state.tasks.get_mut(&id) else {
            bail!("There is no task #{id}.");
        };
        let mut applied = Vec::new();
        for change in changes {
            if let Some(kind) = task.apply(change.clone(), actor, now) {
                applied.push((change, kind));
            }
        }
        let task = task.clone();
        if !applied.is_empty() {
            self.save(&mut state).await;
        }
        Ok(Updated {
            task,
            changes: applied,
        })
    }

    pub async fn set_card(&self, id: TaskId, card: CardRef) {
        let mut state = self.state.lock().await;
        if let Some(task) = state.tasks.get_mut(&id) {
            task.card = Some(card);
            self.save(&mut state).await;
        }
    }

    pub async fn remember_names(&self, names: impl IntoIterator<Item = (String, String)>) {
        let mut state = self.state.lock().await;
        let mut changed = false;
        for (id, name) in names {
            if state.members.get(&id) != Some(&name) {
                state.members.insert(id, name);
                changed = true;
            }
        }
        if changed {
            self.save(&mut state).await;
        }
    }

    // ── Claim reactions ───────────────────────────────────────────────────────

    pub async fn remember_claim(&self, reaction_id: String, claim: ClaimReaction) {
        let mut state = self.state.lock().await;
        state.claim_reactions.insert(reaction_id, claim);
        // Forget reactions on closed tasks; they can no longer matter.
        let open: Vec<TaskId> = state
            .tasks
            .values()
            .filter(|t| t.is_open())
            .map(|t| t.id)
            .collect();
        state.claim_reactions.retain(|_, c| open.contains(&c.task));
        self.save(&mut state).await;
    }

    pub async fn take_claim(&self, reaction_id: &str) -> Option<ClaimReaction> {
        let mut state = self.state.lock().await;
        let claim = state.claim_reactions.remove(reaction_id)?;
        self.save(&mut state).await;
        Some(claim)
    }

    // ── Reminder bookkeeping ──────────────────────────────────────────────────

    pub async fn mark_reminded(&self, ids: &[TaskId], day: chrono::NaiveDate) {
        let mut state = self.state.lock().await;
        for id in ids {
            if let Some(t) = state.tasks.get_mut(id) {
                t.last_reminded = Some(day);
            }
        }
        state.last_digest = Some(day);
        self.save(&mut state).await;
    }

    pub async fn mark_weekly(&self, day: chrono::NaiveDate) {
        let mut state = self.state.lock().await;
        state.last_weekly = Some(day);
        self.save(&mut state).await;
    }

    // ── Web sessions ──────────────────────────────────────────────────────────

    /// A new sign-in token for `user_id`; only its hash is kept.
    pub async fn new_session(&self, user_id: &str, now: DateTime<Utc>) -> String {
        let token = random_hex(32);
        let mut state = self.state.lock().await;
        state.sessions.push(Session {
            token_hash: hash_token(&token),
            user_id: user_id.to_owned(),
            created_at: now,
        });
        let mine: Vec<usize> = state
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.user_id == user_id)
            .map(|(i, _)| i)
            .collect();
        if mine.len() > MAX_SESSIONS_PER_USER {
            let drop = mine[0];
            state.sessions.remove(drop);
        }
        self.save(&mut state).await;
        token
    }

    /// The user of a web session that hasn't expired by `now`.
    pub async fn session_user(&self, token: &str, now: DateTime<Utc>) -> Option<String> {
        let hash = hash_token(token);
        let state = self.state.lock().await;
        state
            .sessions
            .iter()
            .find(|s| s.token_hash == hash)
            .filter(|s| now - s.created_at < chrono::Duration::days(SESSION_DAYS))
            .map(|s| s.user_id.clone())
    }

    /// Sign `user_id` out everywhere; returns how many sessions ended.
    pub async fn revoke_sessions(&self, user_id: &str) -> usize {
        let mut state = self.state.lock().await;
        let before = state.sessions.len();
        state.sessions.retain(|s| s.user_id != user_id);
        let n = before - state.sessions.len();
        if n > 0 {
            self.save(&mut state).await;
        }
        n
    }

    pub async fn end_session(&self, token: &str) {
        let hash = hash_token(token);
        let mut state = self.state.lock().await;
        state.sessions.retain(|s| s.token_hash != hash);
        self.save(&mut state).await;
    }

    // ── Attachments ───────────────────────────────────────────────────────────

    /// Store attachment bytes; the caller then adds it with
    /// `Change::AddAttachment`.
    pub async fn save_attachment(
        &self,
        file_name: &str,
        mime: &str,
        data: &[u8],
        added_by: &str,
        now: DateTime<Utc>,
    ) -> Result<Attachment> {
        let id = random_hex(12);
        tokio::fs::write(self.attachments_dir.join(&id), data)
            .await
            .context("Writing attachment")?;
        Ok(Attachment {
            id,
            file_name: sanitize_file_name(file_name),
            mime: mime.to_owned(),
            size: data.len() as u64,
            added_by: added_by.to_owned(),
            added_at: now,
        })
    }

    pub async fn attachment(&self, id: &str) -> Option<(Attachment, Vec<u8>)> {
        // IDs are hex we generated; anything else never touches the disk.
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let meta = {
            let state = self.state.lock().await;
            state
                .tasks
                .values()
                .flat_map(|t| &t.attachments)
                .find(|a| a.id == id)
                .cloned()?
        };
        let data = tokio::fs::read(self.attachments_dir.join(id)).await.ok()?;
        Some((meta, data))
    }
}

fn sanitize_file_name(name: &str) -> String {
    let name: String = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(80)
        .collect();
    if name.trim().is_empty() {
        "photo".to_owned()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Status;

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    #[tokio::test]
    async fn changes_persist_and_bump_the_version() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let draft = Draft {
            title: "Fix tap".into(),
            ..Default::default()
        };
        let task = store.create(draft, "@a:x", now()).await;
        assert_eq!(task.id, 1);
        let v1 = store.read().await.version;

        let up = store
            .update(
                1,
                "@a:x",
                vec![
                    Change::Status(Status::Done, None),
                    Change::Status(Status::Done, None),
                ],
                now(),
            )
            .await
            .unwrap();
        assert_eq!(up.changes.len(), 1, "the repeat is a no-op");
        assert!(store.read().await.version > v1);

        let reopened = Store::open(dir.path()).await.unwrap();
        assert_eq!(reopened.read().await.tasks[&1].status, Status::Done);
        assert!(store.update(99, "@a:x", vec![], now()).await.is_err());
    }

    #[tokio::test]
    async fn ids_keep_counting_after_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        for title in ["a", "b"] {
            store
                .create(
                    Draft {
                        title: title.into(),
                        ..Default::default()
                    },
                    "@a:x",
                    now(),
                )
                .await;
        }
        let store = Store::open(dir.path()).await.unwrap();
        let t = store
            .create(
                Draft {
                    title: "c".into(),
                    ..Default::default()
                },
                "@a:x",
                now(),
            )
            .await;
        assert_eq!(t.id, 3);
    }

    #[tokio::test]
    async fn sessions_store_only_hashes_and_can_be_revoked() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let token = store.new_session("@a:x", now()).await;
        assert_eq!(
            store.session_user(&token, now()).await.as_deref(),
            Some("@a:x")
        );
        assert!(!std::fs::read_to_string(dir.path().join("state.json"))
            .unwrap()
            .contains(&token));
        assert_eq!(store.session_user("nope", now()).await, None);

        for _ in 0..MAX_SESSIONS_PER_USER {
            store.new_session("@a:x", now()).await;
        }
        assert_eq!(
            store.session_user(&token, now()).await,
            None,
            "oldest dropped"
        );
        assert_eq!(store.revoke_sessions("@a:x").await, MAX_SESSIONS_PER_USER);
    }

    #[tokio::test]
    async fn sessions_expire_like_their_cookie() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let token = store.new_session("@a:x", now()).await;
        let later = |days| now() + chrono::Duration::days(days);
        assert!(store
            .session_user(&token, later(SESSION_DAYS - 1))
            .await
            .is_some());
        assert!(store
            .session_user(&token, later(SESSION_DAYS))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn attachments_round_trip_and_reject_odd_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        store
            .create(
                Draft {
                    title: "t".into(),
                    ..Default::default()
                },
                "@a:x",
                now(),
            )
            .await;
        let att = store
            .save_attachment("../../etc/passwd.jpg", "image/jpeg", b"img", "@a:x", now())
            .await
            .unwrap();
        assert_eq!(att.file_name, "passwd.jpg");
        store
            .update(1, "@a:x", vec![Change::AddAttachment(att.clone())], now())
            .await
            .unwrap();

        let (meta, data) = store.attachment(&att.id).await.unwrap();
        assert_eq!(
            (meta.mime.as_str(), data.as_slice()),
            ("image/jpeg", &b"img"[..])
        );
        assert!(store.attachment("../state.json").await.is_none());
    }
}
