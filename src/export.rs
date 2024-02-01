use std::fs::write;

use crate::{
    get_rooms_info,
    RoomWithCachedInfo,
};

use chrono::{DateTime, SecondsFormat};
use matrix_sdk::{
    room::MessagesOptions,
    ruma::events::{
        room::message::MessageType,
        AnyMessageLikeEvent,
        AnyTimelineEvent,
    },
    Client,
};

///////////////
//   Types   //
///////////////

enum RoomIndexRetrievalError {
    MultipleRoomsWithSpecifiedName(Vec<String>),
    NoRoomsWithSpecifiedName,
}

/////////////////
//   Helpers   //
/////////////////

fn get_room_index_by_identifier(rooms_info: &Vec<RoomWithCachedInfo>, identifier: &str) -> Result<usize, RoomIndexRetrievalError> {
    if let Some(index) = rooms_info.iter().position(|room_info| &room_info.id == identifier) {
        Ok(index)
    } else if let Some(index) = rooms_info.iter().position(|room_info| room_info.canonical_alias.as_ref().is_some_and(|alias| alias == identifier)) {
        Ok(index)
    } else if let Some(index) = rooms_info.iter().position(|room_info| room_info.alt_aliases.iter().any(|alias| alias == identifier)) {
        Ok(index)
    } else {
        let name_matches = rooms_info.iter().filter(|room_info| room_info.name.as_ref().is_some_and(|name| name == identifier)).collect::<Vec<&RoomWithCachedInfo>>();
        match name_matches.len() {
            0 => Err(RoomIndexRetrievalError::NoRoomsWithSpecifiedName),
            1 => Ok(rooms_info.iter().position(|room_info| room_info.name.as_ref().is_some_and(|name| name  == identifier)).unwrap()),
            _ => Err(RoomIndexRetrievalError::MultipleRoomsWithSpecifiedName(name_matches.iter().map(|room_info| room_info.id.to_string()).collect())),
        }
    }
}

fn format_export_filename(room_info: &RoomWithCachedInfo) -> String {
    let (nonserver_id_component, server) = room_info.id.as_str().split_once(':').unwrap();
    match (&room_info.name, &room_info.canonical_alias) {
        (Some(name), Some(alias)) => format!("{} [{}, {}, {}]", name, alias.as_str().split_once(':').unwrap().0, nonserver_id_component, server),
        (Some(name), None) => format!("{} [{}, {}]", name, nonserver_id_component, server),
        (None, Some(alias)) => format!("{} [{}, {}]", alias.as_str().split_once(':').unwrap().0, nonserver_id_component, server),
        (None, None) => format!("{} [{}]", nonserver_id_component, server),
    }
}

//////////////
//   Main   //
//////////////

pub async fn export(client: &Client, rooms: Vec<String>) -> anyhow::Result<()> {
    // Allow setting export destination other than "directly where run"
    let accessible_rooms_info = get_rooms_info(&client).await?; // This should be possible to optimize out for request-piles without names included, given client.resolve_room_alias and client.get_room. Although that might end up actually costlier if handled indelicately, since it'll involve more serial processing.

    for room_identifier in rooms {
        let room_to_export_info = match get_room_index_by_identifier(&accessible_rooms_info, &room_identifier) {
            Ok(index) => &accessible_rooms_info[index],
            Err(e) => match e {
                // This is currently CLI-biased; modify it to return error-info in a more neutral way
                RoomIndexRetrievalError::MultipleRoomsWithSpecifiedName(room_ids) => {
                    println!("Found more than one room accessible to {} with name {}. Room IDs: {:?}", client.user_id().unwrap(), room_identifier, room_ids);
                    continue
                },
                RoomIndexRetrievalError::NoRoomsWithSpecifiedName => {
                    println!("Couldn't find any rooms accessible to {} with name {}.", client.user_id().unwrap(), room_identifier);
                    continue
                },
            }
        };
        let mut messages_options = MessagesOptions::forward();
        messages_options.limit = 1000u32.into();
        let messages = room_to_export_info.room.messages(messages_options).await?; // Could async this better between rooms-to-export; try that at some point. Also put this in a loop so I can get messages from rooms with over 1000 of the things.
        let mut room_export = String::new();
        for event in messages.chunk {
            let event_deserialized = match event.event.deserialize() {
                Ok(event_deserialized) => event_deserialized,
                Err(_) => {
                    // Add more nuanced error-handling here
                    room_export.push_str("[Message skipped due to deserialization failure]\n");
                    continue
                }
            };
            let event_timestamp_millis = event_deserialized.origin_server_ts().0.into();
            let event_timestamp_string_representation = DateTime::from_timestamp_millis(event_timestamp_millis).expect(&format!("Found message with millisecond timestamp {}, which can't be converted to datetime.", event_timestamp_millis)).to_rfc3339_opts(SecondsFormat::Millis, true); // Add real error-handling, and also an option to use local time zones
            let event_sender_id = event_deserialized.sender();
            let event_sender_string_representation = match room_to_export_info.room.get_member_no_sync(event_sender_id).await? {
                Some(room_member) => match room_member.display_name() {
                    Some(display_name) => format!("{} ({})", display_name, event_sender_id),
                    None => event_sender_id.to_string(),
                }
                None => event_sender_id.to_string(),
            };
            let event_stringified = match &event_deserialized {
                AnyTimelineEvent::MessageLike(e) => match e {
                    AnyMessageLikeEvent::RoomMessage(e) => match &e.as_original().unwrap().content.msgtype { // Add real handling for the case where the event is redacted
                        // Add handling for formatted messages
                        MessageType::Emote(e) => format!("[{}] {}: *{}*", event_timestamp_string_representation, event_sender_string_representation, &e.body), // Think harder about whether asterisks are the correct representation here
                        MessageType::Notice(e) => format!("[{}] {}: [{}]", event_timestamp_string_representation, event_sender_string_representation, &e.body), // Think harder about whether brackets are the correct representation here
                        MessageType::Text(e) => format!("[{}] {}: {}", event_timestamp_string_representation, event_sender_string_representation, &e.body),
                        _ => String::from("[Placeholder message]"),
                    }
                    _ => String::from("[Placeholder message-like]"),
                },
                AnyTimelineEvent::State(_e) => String::from("[Placeholder state-like]"),
            };
            room_export.push_str(&format!("{}\n", event_stringified))
        }
        write(format!("{}.txt", format_export_filename(&room_to_export_info)), room_export).unwrap(); // Ideally let users pass format strings of some sort here
    }

    Ok(())
}
