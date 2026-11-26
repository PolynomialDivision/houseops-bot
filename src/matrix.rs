//! Matrix side: commands, thread replies, photos and reactions.
//!
//! Conventions that keep the house room calm:
//! * the card itself is the confirmation for new tasks and is edited in
//!   place instead of answered;
//! * a command that worked gets a 👍 reaction, not a reply;
//! * plain replies in a task's thread are comments, photos there are
//!   attachments: nothing to learn.

use chrono::Utc;
use mxbot_common::{
    admin::Dispatch,
    format,
    matrix_sdk::{
        deserialized_responses::EncryptionInfo,
        media::{MediaFormat, MediaRequestParameters},
        ruma::{
            events::{
                reaction::{OriginalSyncReactionEvent, ReactionEventContent},
                relation::Annotation,
                room::{
                    member::{MembershipState, OriginalSyncRoomMemberEvent},
                    message::{
                        MessageType, OriginalSyncRoomMessageEvent, Relation,
                        RoomMessageEventContent,
                    },
                    redaction::OriginalSyncRoomRedactionEvent,
                    MediaSource,
                },
            },
            OwnedEventId, OwnedUserId,
        },
        Room, RoomState,
    },
    private, send,
};
use tracing::{error, info, warn};

use crate::{
    house::{today, House, Origin},
    model::{Change, ClaimReaction, Status, TaskId},
    parse::{self, Action, Command, Tag},
    render,
};

/// Largest photo taken over from Matrix (bytes).
const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

pub fn install_handlers(house: &House) {
    let client = house.bot().client.clone();
    {
        let house = house.clone();
        client.add_event_handler(
            move |ev: OriginalSyncRoomMessageEvent,
                  room: Room,
                  encryption: Option<EncryptionInfo>| {
                let house = house.clone();
                async move {
                    if room.state() != RoomState::Joined || ev.sender == house.bot().user_id {
                        return;
                    }
                    if house
                        .bot()
                        .admin
                        .handle(&room, &ev, encryption.as_ref())
                        .await
                        == Dispatch::Handled
                    {
                        return;
                    }
                    tokio::spawn(async move {
                        if let Err(e) = on_message(&house, &room, &ev).await {
                            error!("Handling message {}: {e:#}", ev.event_id);
                        }
                    });
                }
            },
        );
    }
    {
        let house = house.clone();
        client.add_event_handler(move |ev: OriginalSyncReactionEvent, room: Room| {
            let house = house.clone();
            async move {
                if room.state() == RoomState::Joined
                    && ev.sender != house.bot().user_id
                    && room.room_id() == house.room_id()
                {
                    tokio::spawn(async move { on_reaction(&house, &ev).await });
                }
            }
        });
    }
    {
        // Leaving (or being removed from) the house room ends every web
        // sign-in of that person.
        let house = house.clone();
        client.add_event_handler(move |ev: OriginalSyncRoomMemberEvent, room: Room| {
            let house = house.clone();
            async move {
                let gone = matches!(
                    ev.content.membership,
                    MembershipState::Leave | MembershipState::Ban
                );
                if gone && room.room_id() == house.room_id() {
                    let n = house.store().revoke_sessions(ev.state_key.as_str()).await;
                    if n > 0 {
                        info!("Ended {n} web session(s) of someone who left the house room");
                    }
                }
            }
        });
    }
    {
        let house = house.clone();
        client.add_event_handler(move |ev: OriginalSyncRoomRedactionEvent, room: Room| {
            let house = house.clone();
            async move {
                if room.room_id() == house.room_id() && ev.sender != house.bot().user_id {
                    tokio::spawn(async move { on_redaction(&house, &ev).await });
                }
            }
        });
    }
}

// ── Messages ──────────────────────────────────────────────────────────────────

/// The task whose thread `ev` is in, if any.
async fn thread_task(house: &House, ev: &OriginalSyncRoomMessageEvent) -> Option<TaskId> {
    let Some(Relation::Thread(thread)) = &ev.content.relates_to else {
        return None;
    };
    house
        .store()
        .read()
        .await
        .task_by_card(thread.event_id.as_str())
}

async fn remember_sender(house: &House, room: &Room, sender: &OwnedUserId) {
    if let Ok(Some(member)) = room.get_member(sender).await {
        if let Some(name) = member
            .display_name()
            .and_then(format::sanitize_display_name)
        {
            house
                .store()
                .remember_names([(sender.to_string(), name)])
                .await;
        }
    }
}

async fn on_message(
    house: &House,
    room: &Room,
    ev: &OriginalSyncRoomMessageEvent,
) -> anyhow::Result<()> {
    // An edited message arrives as a new event ("* new text"); treating it
    // as a comment or command would duplicate or repeat it.
    if matches!(ev.content.relates_to, Some(Relation::Replacement(_))) {
        return Ok(());
    }
    let in_house = room.room_id() == house.room_id();
    if in_house {
        remember_sender(house, room, &ev.sender).await;
    } else if !house.bot().is_admin(&ev.sender)
        && !matches!(
            private::is_joined_now(&house.bot().client, house.room_id(), &ev.sender).await,
            Ok(true)
        )
    {
        // Outside the house room (a direct chat), only people who are in
        // the house room right now may use the bot: asked of the server.
        return Ok(());
    }
    let thread = if in_house {
        thread_task(house, ev).await
    } else {
        None
    };
    let sender = ev.sender.as_str();

    let (body, html) = match &ev.content.msgtype {
        MessageType::Text(t) => (
            t.body.as_str(),
            t.formatted.as_ref().map(|f| f.body.as_str()),
        ),
        MessageType::Notice(_) => return Ok(()),
        MessageType::Image(img) => {
            if !in_house {
                return Ok(());
            }
            let name = img.filename().to_owned();
            let caption = img.caption().map(str::to_owned);
            return on_media(
                house,
                room,
                ev,
                thread,
                img.source.clone(),
                &name,
                image_mime(img),
                caption,
            )
            .await;
        }
        MessageType::File(file) => {
            if !in_house {
                return Ok(());
            }
            let mime = file
                .info
                .as_ref()
                .and_then(|i| i.mimetype.clone())
                .unwrap_or_else(|| "application/octet-stream".to_owned());
            let name = file.filename().to_owned();
            let caption = file.caption().map(str::to_owned);
            return on_media(
                house,
                room,
                ev,
                thread,
                file.source.clone(),
                &name,
                mime,
                caption,
            )
            .await;
        }
        _ => return Ok(()),
    };

    let Some(command) = parse::parse_command(body) else {
        // Anything else said in a task's thread is a comment on it.
        if let Some(id) = thread {
            let text = strip_reply_fallback(body);
            house
                .change(id, sender, vec![Change::Comment(text)], Origin::Matrix)
                .await?;
        }
        return Ok(());
    };

    match command {
        Command::Create(text) => {
            if !in_house {
                reply(
                    house,
                    room,
                    ev,
                    "🏠 Create tasks in the house room, so everyone sees them.",
                )
                .await;
                return Ok(());
            }
            match parse::parse_draft(&text, house.config()) {
                Some(draft) => {
                    house.create(draft, sender).await;
                }
                None => {
                    reply(
                        house,
                        room,
                        ev,
                        "🤔 What needs doing?\n💡 e.g. !task Fix the kitchen tap #kitchen",
                    )
                    .await
                }
            }
        }
        Command::List(filter) => {
            let (plain, html) = {
                let state = house.store().read().await;
                render::list(&state, house.config(), filter, sender, house.public_url())
            };
            reply_html(room, ev, plain, html).await;
        }
        Command::Help => {
            let (plain, html) = render::help(house.public_url());
            reply_html(room, ev, plain, html).await;
        }
        Command::Usage(text) => reply(house, room, ev, &format!("💡 Usage: {text}")).await,
        Command::Web { logout } => web_link(house, room, ev, logout).await,
        Command::Act { id, action } => {
            let Some(id) = id.or(thread) else {
                reply(
                    house,
                    room,
                    ev,
                    "🤔 Which task?\n💡 Add its number (!done 12) or reply in its thread.",
                )
                .await;
                return Ok(());
            };
            act(house, room, ev, id, action, html).await?;
        }
    }
    Ok(())
}

/// Replies quote the original in their plain body ("> <@a:x> …"); a comment
/// is only the new text.
fn strip_reply_fallback(body: &str) -> String {
    let lines: Vec<&str> = body.lines().collect();
    let start = lines
        .iter()
        .position(|l| !l.starts_with('>'))
        .unwrap_or(lines.len());
    lines[start..].join("\n").trim().to_owned()
}

fn image_mime(
    img: &mxbot_common::matrix_sdk::ruma::events::room::message::ImageMessageEventContent,
) -> String {
    img.info
        .as_ref()
        .and_then(|i| i.mimetype.clone())
        .unwrap_or_else(|| "image/jpeg".to_owned())
}

#[allow(clippy::too_many_arguments)]
async fn on_media(
    house: &House,
    room: &Room,
    ev: &OriginalSyncRoomMessageEvent,
    thread: Option<TaskId>,
    source: MediaSource,
    file_name: &str,
    mime: String,
    caption: Option<String>,
) -> anyhow::Result<()> {
    let sender = ev.sender.as_str();
    // A photo captioned "!task …" in the room creates a task with it.
    let task_id = match (thread, caption.as_deref().and_then(parse::parse_command)) {
        (Some(id), _) => id,
        (None, Some(Command::Create(text))) => match parse::parse_draft(&text, house.config()) {
            Some(draft) => house.create(draft, sender).await.id,
            None => return Ok(()),
        },
        _ => return Ok(()),
    };
    let data = house
        .bot()
        .client
        .media()
        .get_media_content(
            &MediaRequestParameters {
                source,
                format: MediaFormat::File,
            },
            true,
        )
        .await?;
    if data.len() > MAX_ATTACHMENT_BYTES {
        reply(
            house,
            room,
            ev,
            "📦 That file is too big to attach (max 20 MB).",
        )
        .await;
        return Ok(());
    }
    let attachment = house
        .store()
        .save_attachment(file_name, &mime, &data, sender, Utc::now())
        .await?;
    house
        .change(
            task_id,
            sender,
            vec![Change::AddAttachment(attachment)],
            Origin::Matrix,
        )
        .await?;
    info!("Photo attached to #{task_id} from Matrix");
    Ok(())
}

async fn act(
    house: &House,
    room: &Room,
    ev: &OriginalSyncRoomMessageEvent,
    id: TaskId,
    action: Action,
    html: Option<&str>,
) -> anyhow::Result<()> {
    let sender = ev.sender.as_str();
    let exists = house.store().read().await.tasks.contains_key(&id);
    if !exists {
        reply(house, room, ev, &format!("🔍 There is no task #{id}.")).await;
        return Ok(());
    }
    let change = match action {
        Action::Show => {
            let (plain, html) = {
                let state = house.store().read().await;
                let task = &state.tasks[&id];
                render::card(task, &state, house.config(), house.public_url(), today())
            };
            reply_html(room, ev, plain, html).await;
            return Ok(());
        }
        Action::Done => Change::Status(Status::Done, None),
        Action::Claim => Change::Assignee(Some(sender.to_owned())),
        Action::Unclaim => Change::Assignee(None),
        Action::Drop => Change::Status(Status::Dropped, None),
        Action::Reopen => Change::Status(Status::Open, None),
        Action::Wait(reason) => Change::Status(Status::Waiting, reason),
        Action::Priority(p) => Change::Priority(p),
        Action::Rename(title) => Change::Title(title),
        Action::Describe(text) => Change::Description(text),
        Action::Location(None) => Change::Location(None),
        Action::Location(Some(text)) => match parse::resolve_tag(&text, house.config()) {
            Tag::Location(name) => Change::Location(Some(name)),
            // "!loc repair" is almost certainly a slip; take it literally.
            Tag::Category(_) => Change::Location(Some(text)),
        },
        Action::Category(None) => Change::Category(None),
        Action::Category(Some(text)) => match parse::resolve_tag(&text, house.config()) {
            Tag::Category(key) => Change::Category(Some(key)),
            Tag::Location(_) => {
                let keys: Vec<&str> = house
                    .config()
                    .categories
                    .iter()
                    .map(|c| c.key.as_str())
                    .collect();
                reply(
                    house,
                    room,
                    ev,
                    &format!("🏷️ Categories: {}", keys.join(", ")),
                )
                .await;
                return Ok(());
            }
        },
        Action::Due(text) => match parse::parse_due(&text, today()) {
            Ok(due) => Change::Due(due),
            Err(()) => {
                reply(house, room, ev, "📅 I couldn't read that date.\n💡 Try: fri, tomorrow, in 3 days, 3.10., 2026-10-03, none").await;
                return Ok(());
            }
        },
        Action::Assign(text) => {
            let members = house.store().read().await.members.clone();
            match parse::resolve_person(&text, html, sender, &members) {
                Ok(person) => Change::Assignee(person),
                Err(()) => {
                    reply(
                        house,
                        room,
                        ev,
                        "🤔 Who's that?\n💡 Mention them (@name), or use me or none.",
                    )
                    .await;
                    return Ok(());
                }
            }
        }
    };
    house
        .change(id, sender, vec![change], Origin::Matrix)
        .await?;
    acknowledge(room, &ev.event_id).await;
    Ok(())
}

async fn web_link(house: &House, room: &Room, ev: &OriginalSyncRoomMessageEvent, logout: bool) {
    let sender = ev.sender.as_str();
    let Some(public_url) = house.public_url() else {
        reply(
            house,
            room,
            ev,
            "🌐 The web app is not set up for this house.",
        )
        .await;
        return;
    };
    if logout {
        let n = house.store().revoke_sessions(sender).await;
        reply(
            house,
            room,
            ev,
            &format!("👋 Signed you out of the web app on {n} device(s)."),
        )
        .await;
        return;
    }
    let token = house.store().new_session(sender, Utc::now()).await;
    let link = format!("{public_url}/login#{token}");
    let text = format!(
        "🔑 Your personal link to the house tasks:\n{link}\n\n🤫 Don't share it. It keeps you signed in on that device.\n🚪 !web logout signs you out everywhere."
    );
    // Never post the link in a shared room: only in a chat that is, asked
    // of the server right now, just the bot and this person, encrypted.
    let client = &house.bot().client;
    let direct = private::is_private_dm(client, room.room_id(), &ev.sender).await;
    let dm = if direct {
        Some(room.clone())
    } else {
        let mut found = None;
        for candidate in client.get_dm_rooms(&ev.sender) {
            if private::is_private_dm(client, candidate.room_id(), &ev.sender).await {
                found = Some(candidate);
                break;
            }
        }
        match found {
            Some(dm) => Some(dm),
            None => {
                private::create_private_dm(client, &ev.sender, &private::PREFERRED_ROOM_VERSIONS)
                    .await
                    .ok()
            }
        }
    };
    match dm {
        Some(dm) => {
            if let Err(e) =
                send::send_with_retry(&dm, RoomMessageEventContent::text_plain(text)).await
            {
                warn!("Could not send web link to {sender}: {e}");
                reply(house, room, ev, "😕 I couldn't message you directly.\n💬 Open a direct chat with me and send !web there.").await;
            } else if !direct {
                reply(
                    house,
                    room,
                    ev,
                    "📬 Sent you a direct message with your link.",
                )
                .await;
            }
        }
        None => {
            reply(
                house,
                room,
                ev,
                "😕 I couldn't open a direct chat.\n💬 Start one with me and send !web there.",
            )
            .await;
        }
    }
}

async fn acknowledge(room: &Room, event_id: &OwnedEventId) {
    let reaction = ReactionEventContent::new(Annotation::new(event_id.clone(), "👍".to_owned()));
    if let Err(e) = room.send(reaction).await {
        warn!("Could not acknowledge {event_id}: {e}");
    }
}

/// Answer in the same place: the thread the message is in, else the room.
async fn reply(_house: &House, room: &Room, ev: &OriginalSyncRoomMessageEvent, text: &str) {
    let html = format::html_escape(text).replace('\n', "<br>");
    reply_html(room, ev, text.to_owned(), html).await;
}

async fn reply_html(room: &Room, ev: &OriginalSyncRoomMessageEvent, plain: String, html: String) {
    let mut content = RoomMessageEventContent::notice_html(plain, html);
    if matches!(ev.content.relates_to, Some(Relation::Thread(_))) {
        let root = send::thread_root(ev);
        content = send::in_thread(content, root, ev.event_id.clone());
    }
    if let Err(e) = send::send_with_retry(room, content).await {
        warn!("Could not reply in {}: {e}", room.room_id());
    }
}

// ── Reactions ─────────────────────────────────────────────────────────────────

/// Drop emoji variation selectors and skin tones: "✋🏽" counts as "✋".
fn base_emoji(key: &str) -> String {
    key.chars()
        .filter(|&c| {
            !('\u{FE00}'..='\u{FE0F}').contains(&c) && !('\u{1F3FB}'..='\u{1F3FF}').contains(&c)
        })
        .collect()
}

fn is_claim(key: &str) -> bool {
    matches!(
        base_emoji(key).as_str(),
        "✋" | "🙋" | "🙋‍♀" | "🙋‍♂" | "🖐" | "👋"
    )
}

fn is_done(key: &str) -> bool {
    matches!(base_emoji(key).as_str(), "✅" | "✔" | "☑")
}

async fn on_reaction(house: &House, ev: &OriginalSyncReactionEvent) {
    let Annotation { event_id, key, .. } = &ev.content.relates_to;
    let Some(id) = house.store().read().await.task_by_card(event_id.as_str()) else {
        return;
    };
    let sender = ev.sender.as_str();
    let result = if is_claim(key) {
        let assignee = house.store().read().await.tasks[&id].assignee.clone();
        // Taking over someone else's task goes through !assign, deliberately.
        if assignee.as_deref().is_some_and(|a| a != sender) {
            return;
        }
        house
            .store()
            .remember_claim(
                ev.event_id.to_string(),
                ClaimReaction {
                    task: id,
                    user_id: sender.to_owned(),
                },
            )
            .await;
        house
            .change(
                id,
                sender,
                vec![Change::Assignee(Some(sender.to_owned()))],
                Origin::Matrix,
            )
            .await
    } else if is_done(key) {
        house
            .change(
                id,
                sender,
                vec![Change::Status(Status::Done, None)],
                Origin::Matrix,
            )
            .await
    } else {
        return;
    };
    if let Err(e) = result {
        warn!("Reaction on #{id}: {e:#}");
    }
}

/// A retracted ✋ gives the task back, if that person still has it.
async fn on_redaction(house: &House, ev: &OriginalSyncRoomRedactionEvent) {
    let Some(redacted) = ev.content.redacts.clone().or_else(|| ev.redacts.clone()) else {
        return;
    };
    let Some(claim) = house.store().take_claim(redacted.as_str()).await else {
        return;
    };
    let still_theirs = house
        .store()
        .read()
        .await
        .tasks
        .get(&claim.task)
        .is_some_and(|t| t.is_open() && t.assignee.as_deref() == Some(claim.user_id.as_str()));
    if still_theirs {
        if let Err(e) = house
            .change(
                claim.task,
                &claim.user_id,
                vec![Change::Assignee(None)],
                Origin::Matrix,
            )
            .await
        {
            warn!("Unclaim of #{}: {e:#}", claim.task);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_fallbacks_are_not_part_of_the_comment() {
        assert_eq!(
            strip_reply_fallback("> <@a:x> Fix the tap\n> more\n\nOrdered a washer"),
            "Ordered a washer"
        );
        assert_eq!(strip_reply_fallback("plain comment"), "plain comment");
    }

    #[test]
    fn reaction_keys_ignore_skin_tones_and_variation_selectors() {
        assert!(is_claim("✋"));
        assert!(is_claim("✋🏽"));
        assert!(is_claim("🙋\u{200D}♀\u{FE0F}"));
        assert!(is_done("✔\u{FE0F}"));
        assert!(!is_done("✋"));
        assert!(!is_claim("👍"));
    }
}
