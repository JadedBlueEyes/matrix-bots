use matrix_sdk::ruma::{
    events::{
        room::{
            member::StrippedRoomMemberEvent,
            message::{
                sanitize::remove_plain_reply_fallback, AddMentions, ForwardThread, MessageType,
                OriginalSyncRoomMessageEvent, Relation, ReplyWithinThread, RoomMessageEventContent,
            },
        },
        AnyMessageLikeEvent, AnyTimelineEvent, MessageLikeEvent,
    },
    uint,
};
use matrix_sdk::{Client, Room, RoomState};
use regex::Regex;
use similar::utils::TextDiffRemapper;
use similar::{ChangeTag, TextDiff};
use std::sync::LazyLock;
use tokio::time::{sleep, Duration};
use tracing::{error, info, instrument, trace, warn};

pub async fn send_or_log_error(room: &Room, message: RoomMessageEventContent) {
    if let Err(e) = room.send(message).await {
        warn!("Failed to send message to room {}: {}", room.room_id(), e);
    }
}

#[instrument(fields(room_member = room_member.state_key.as_str(), room = room.room_id().as_str(), client = client.user_id().map(|u| u.as_str()).unwrap_or("None")))]
pub async fn on_stripped_state_member(
    room_member: StrippedRoomMemberEvent,
    client: Client,
    room: Room,
) {
    if room_member.state_key != client.user_id().unwrap() {
        return;
    }

    tokio::spawn(async move {
        info!("Autojoining room {}", room.room_id());
        let mut delay = 2;

        while let Err(err) = room.join().await {
            // retry autojoin due to synapse sending invites, before the
            // invited user can join for more information see
            // https://github.com/matrix-org/synapse/issues/4345
            warn!(
                "Failed to join room {} ({err:?}), retrying in {delay}s",
                room.room_id()
            );

            sleep(Duration::from_secs(delay)).await;
            delay *= 2;

            if delay > 3600 {
                error!("Can't join room {} ({err:?})", room.room_id());
                break;
            }
        }
        info!("Successfully joined room {}", room.room_id());
    });
}

#[instrument(fields(event = event.event_id.as_str(), room = room.room_id().as_str()))]
pub async fn on_room_message(
    event: OriginalSyncRoomMessageEvent,
    room: Room,
) -> anyhow::Result<()> {
    if room.state() != RoomState::Joined {
        return Ok(());
    }
    let MessageType::Text(text_content) = event.content.msgtype else {
        return Ok(());
    };

    let body_text = remove_plain_reply_fallback(&text_content.body);

    static MATCH_COMMAND: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:^|[^a-zA-Z0-9])sed (s.+)").unwrap());
    static MATCH_PATTERN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(s[#/].+[#/].+)$").unwrap());

    let command = if let Some(c) = MATCH_COMMAND.captures(body_text) {
        c[1].to_string()
    } else if let Some(c) = MATCH_PATTERN.captures(body_text) {
        c[1].to_string()
    } else {
        return Ok(());
    };

    let mut thread_root = None;

    trace!("Searching for target");
    let target_event = if let Some(target_id) =
        event
            .content
            .relates_to
            .and_then(|relation| match relation {
                // Normal replies
                Relation::Reply { in_reply_to } => Some(in_reply_to.event_id),
                // Replies in threads
                Relation::Thread(thread) => {
                    thread_root = Some(thread.event_id);

                    // In the event that this isn't a reply to a message, we still want to get the last message in the thread, which is the "fallback" reply
                    if thread.is_falling_back {
                        return thread.in_reply_to.map(|in_reply_to| in_reply_to.event_id);
                    }

                    thread.in_reply_to.map(|in_reply_to| in_reply_to.event_id)
                }
                _ => None,
            }) {
        room.event(&target_id, None)
            .await?
            .raw()
            .deserialize()?
            .into_full_event(room.room_id().to_owned())
    } else {
        trace!("No related event found, using event context");
        // TODO: Filter to only events outside of a thread
        let context = room
            .event_with_context(&event.event_id, false, uint!(2), None)
            .await?;
        let Some(prev) = context.events_before.first() else {
            trace!("No previous event found, aborting");
            return Ok(());
        };
        prev.raw()
            .deserialize()?
            .into_full_event(room.room_id().to_owned())
    };

    trace!(
        id = target_event.event_id().as_str(),
        "Target message found"
    );
    // Only continue if it's a message-like event (filter out non-message events)
    let AnyTimelineEvent::MessageLike(AnyMessageLikeEvent::RoomMessage(
        MessageLikeEvent::Original(ref target_event_message),
    )) = target_event
    else {
        trace!("Target is not a message");
        return Ok(());
    };

    let target_event_text = remove_plain_reply_fallback(target_event_message.content.body());
    let command = sedregex::ReplaceCommand::new(&command)?;
    let result = command.execute(target_event_text);

    let diff = TextDiff::from_words(target_event_text, &result);
    let remapper = TextDiffRemapper::from_text_diff(&diff, target_event_text, &result);
    let changes: String = diff
        .ops()
        .iter()
        .flat_map(move |x| remapper.iter_slices(x))
        .map(|(tag, text)| match tag {
            ChangeTag::Equal => text.to_string(),
            ChangeTag::Delete => String::new(),
            ChangeTag::Insert => "<u>".to_string() + text + "</u>",
        })
        .collect();

    let message = if thread_root.is_some() {
        // If the original message is not in a thread, make_reply_to won't create a reply in the thread
        // so we need to make_for_thread instead, which will always reply in the thread.
        RoomMessageEventContent::notice_html(result, changes).make_for_thread(
            target_event_message,
            ReplyWithinThread::Yes,
            AddMentions::No,
        )
    } else {
        RoomMessageEventContent::notice_html(result, changes).make_reply_to(
            target_event_message,
            ForwardThread::Yes,
            AddMentions::No,
        )
    };

    send_or_log_error(&room, message).await;
    Ok(())
}
