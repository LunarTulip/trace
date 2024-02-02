use std::collections::{
    HashMap,
    HashSet,
};
use std::fs::{
    create_dir_all,
    write,
};
use std::path::PathBuf;

use crate::{
    get_rooms_info,
    RoomWithCachedInfo,
};

use chrono::{DateTime, SecondsFormat};
use matrix_sdk::{
    deserialized_responses::TimelineEvent,
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

#[derive(PartialEq, Eq, Hash)]
pub enum ExportOutputFormat {
    Json,
    Txt,
}

enum RoomIndexRetrievalError {
    MultipleRoomsWithSpecifiedName(Vec<String>),
    NoRoomsWithSpecifiedName,
}

//////////////
//   Main   //
//////////////

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

fn messages_to_json(events: &Vec<TimelineEvent>) -> String {
    // Possibly add more secondary-representations-of-events here, analogous to e.g. the display-name-retrieval and datetime-formatting and so forth in the txt output?
    // Also possibly some metadata analogous to what gets output at the head of DiscordChatExporter's JSON exports?
    let mut events_to_export = Vec::new();

    for event in events {
        let event_serialized = event.event.deserialize_as::<serde_json::Value>().expect("Failed to deserialize a message to JSON value. (This is surprising.)"); // Add real error-handling here
        events_to_export.push(event_serialized);
    }

    serde_json::to_string_pretty(&events_to_export).unwrap()
}

async fn messages_to_txt(events: &Vec<TimelineEvent>, room_info: &RoomWithCachedInfo) -> anyhow::Result<String> {
    let mut user_ids_to_display_names: HashMap<String, Option<String>> = HashMap::new();
    let mut room_export = String::new();

    for event in events {
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
        let event_sender_id_string = event_sender_id.to_string();
        let event_sender_display_name = match user_ids_to_display_names.get(&event_sender_id_string) {
            Some(display_name_option) => display_name_option,
            None => &match room_info.room.get_member_no_sync(event_sender_id).await? {
                Some(room_member) => {
                    let display_name = room_member.display_name().map(|s| String::from(s));
                    user_ids_to_display_names.insert(event_sender_id_string.clone(), display_name);
                    user_ids_to_display_names.get(&event_sender_id_string).unwrap()
                }
                None => &None,
            },
        };
        let event_sender_string_representation = match event_sender_display_name {
            // Possibly factor this into the display-name-caching since this is the only place the raw name is used?
            Some(display_name) => format!("{} ({})", display_name, event_sender_id_string),
            None => event_sender_id.to_string(),
        };

        let event_stringified = match &event_deserialized {
            AnyTimelineEvent::MessageLike(e) => match e {
                AnyMessageLikeEvent::RoomMessage(e) => match &e.as_original() {
                    Some(unredacted_room_message) => match &unredacted_room_message.content.msgtype {
                        // Add handling for formatted messages
                        MessageType::Emote(e) => format!("[{}] {}: *{}*", event_timestamp_string_representation, event_sender_string_representation, &e.body), // Think harder about whether asterisks are the correct representation here
                        MessageType::Notice(e) => format!("[{}] {}: [{}]", event_timestamp_string_representation, event_sender_string_representation, &e.body), // Think harder about whether brackets are the correct representation here
                        MessageType::Text(e) => format!("[{}] {}: {}", event_timestamp_string_representation, event_sender_string_representation, &e.body),
                        _ => String::from("[Placeholder message]"),
                    }
                    None => String::from("[Placeholder redacted message]"),
                },
                _ => String::from("[Placeholder message-like]"),
            },
            AnyTimelineEvent::State(_e) => String::from("[Placeholder state-like]"),
        };
        room_export.push_str(&format!("{}\n", event_stringified))
    }

    Ok(room_export)
}

pub async fn export(client: &Client, rooms: Vec<String>, output_path: Option<PathBuf>, formats: HashSet<ExportOutputFormat>) -> anyhow::Result<()> {
    if let Some(path) = output_path.as_ref() {
        if path.exists() {
            if !path.is_dir() {
                // Add real error-handling here
                panic!("Output path {} isn't a directory.", path.display());
            }
        } else {
            create_dir_all(path).unwrap();
        }
    }

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

        let mut events = Vec::new();
        let mut last_end_token = None;
        loop {
            // Add emergency handling for rooms which are somehow presenting as infinitely long, to avoid slamming the server forever. (Analogous to Element's max 10 million messages.)
            let mut messages_options = MessagesOptions::forward().from(last_end_token.as_deref());
            messages_options.limit = 1000u16.into();
            let mut messages = room_to_export_info.room.messages(messages_options).await?;
            let messages_length = messages.chunk.len();
            events.append(&mut messages.chunk);
            if messages_length < 1000 {
                break
            } else {
                last_end_token = messages.end;
            }
        }

        let base_output_path = output_path.clone().unwrap_or_else(|| PathBuf::new());
        let base_output_filename = format_export_filename(&room_to_export_info);
        if formats.contains(&ExportOutputFormat::Json) {
            let json_output_file = messages_to_json(&events);
            let mut json_output_path_buf = base_output_path.clone();
            json_output_path_buf.push(format!("{}.json", base_output_filename));
            write(json_output_path_buf, json_output_file).unwrap();
        }
        if formats.contains(&ExportOutputFormat::Txt) {
            let txt_output_file = messages_to_txt(&events, room_to_export_info).await?;
            let mut txt_output_path_buf = base_output_path.clone();
            txt_output_path_buf.push(format!("{}.txt", base_output_filename));
            write(txt_output_path_buf, txt_output_file).unwrap();
        }
    }

    Ok(())
}
