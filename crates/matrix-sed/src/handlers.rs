use matrix_sdk::ruma::events::{
    room::{
        member::StrippedRoomMemberEvent,
        message::{
            sanitize::remove_plain_reply_fallback, AddMentions, ForwardThread, MessageType,
            OriginalSyncRoomMessageEvent, Relation, ReplyWithinThread, RoomMessageEventContent,
        },
    },
    AnyMessageLikeEvent, AnyTimelineEvent,
};
use matrix_sdk::{Client, Room, RoomState};
use regex::Regex;
use similar::utils::TextDiffRemapper;
use similar::{ChangeTag, TextDiff};
use std::sync::LazyLock;
use tokio::time::{sleep, Duration};

pub async fn send_or_log_error(room: &Room, message: RoomMessageEventContent) {
    if let Err(e) = room.send(message).await {
        println!("Failed to send message to room {}: {}", room.room_id(), e);
    }
}

pub async fn on_stripped_state_member(
    room_member: StrippedRoomMemberEvent,
    client: Client,
    room: Room,
) {
    if room_member.state_key != client.user_id().unwrap() {
        return;
    }

    tokio::spawn(async move {
        println!("Autojoining room {}", room.room_id());
        let mut delay = 2;

        while let Err(err) = room.join().await {
            // retry autojoin due to synapse sending invites, before the
            // invited user can join for more information see
            // https://github.com/matrix-org/synapse/issues/4345
            println!(
                "Failed to join room {} ({err:?}), retrying in {delay}s",
                room.room_id()
            );

            sleep(Duration::from_secs(delay)).await;
            delay *= 2;

            if delay > 3600 {
                println!("Can't join room {} ({err:?})", room.room_id());
                break;
            }
        }
        println!("Successfully joined room {}", room.room_id());
    });
}

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

    let Some(in_reply_to) = event
        .content
        .relates_to
        .and_then(|relation| match relation {
            // Normal replies
            Relation::Reply { in_reply_to } => Some(in_reply_to.event_id),
            // Replies in threads
            Relation::Thread(thread) => {
                thread_root = Some(thread.event_id);

                // Only give back "real" replies, not the last message in the thread
                if thread.is_falling_back {
                    return None;
                }

                thread.in_reply_to.map(|in_reply_to| in_reply_to.event_id)
            }
            _ => None,
        })
    else {
        return Ok(());
    };

    let reply_event = room.event(&in_reply_to).await?.event.deserialize()?;

    let AnyTimelineEvent::MessageLike(AnyMessageLikeEvent::RoomMessage(
        matrix_sdk::ruma::events::MessageLikeEvent::Original(reply_event_message),
    )) = reply_event
    else {
        return Ok(());
    };

    let reply_event_text = remove_plain_reply_fallback(reply_event_message.content.body());
    let command = sedregex::ReplaceCommand::new(&command)?;
    let result = command.execute(reply_event_text);

    let diff = TextDiff::from_words(reply_event_text, &result);
    let remapper = TextDiffRemapper::from_text_diff(&diff, reply_event_text, &result);
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
            &reply_event_message,
            ReplyWithinThread::Yes,
            AddMentions::No,
        )
    } else {
        RoomMessageEventContent::notice_html(result, changes).make_reply_to(
            &reply_event_message,
            ForwardThread::Yes,
            AddMentions::No,
        )
    };

    send_or_log_error(&room, message).await;
    Ok(())
}
