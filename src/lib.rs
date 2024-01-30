use std::{
    cmp::Ordering,
    fs::{
        create_dir_all,
        read_to_string,
        write,
    },
    path::PathBuf,
};

use chrono::{DateTime, SecondsFormat};
use futures::future::join_all;
use matrix_sdk::{
    matrix_auth::{
        MatrixSession,
        MatrixSessionTokens,
    }, 
    room::MessagesOptions,
    ruma::{
        events::{
            room::message::MessageType,
            AnyMessageLikeEvent,
            AnyTimelineEvent,
        },
        OwnedRoomAliasId,
        OwnedRoomId,
        UserId,
    },
    Client,
    Room,
    SessionMeta,
};
use serde::{
    Deserialize,
    Serialize,
};
use uuid::Uuid;

///////////////////////
//   Non-arg types   //
///////////////////////

#[derive(Clone, Deserialize, Serialize)]
pub struct Session {
    pub user_id: String,
    pub device_id: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
}

pub struct SessionsFile {
    path: PathBuf,
    pub sessions: Vec<Session>,
}

impl SessionsFile {
    pub fn open(path: PathBuf) -> Self {
        if let Ok(file) = read_to_string(&path) {
            let sessions = serde_json::from_str(&file).expect("Sessions file is invalid JSON."); // Replace with better error-handling
            Self {
                path,
                sessions,
            }
        } else {
            create_dir_all(&path.parent().expect("Tried to open root as sessions file. (This should never happen.")).unwrap();
            write(&path, "[]").unwrap();
            Self {
                path,
                sessions: Vec::new(),
            }
        }
    }

    pub fn get(&self, user_id: &str) -> Result<Session, String> {
        match self.sessions.iter().find(|session| &session.user_id == user_id) {
            Some(session) => Ok(session.clone()),
            None => Err(format!("Couldn't find currently-existing login session for user_id {}.", user_id))
        }
    }

    pub fn delete_session(&mut self, user_id: &str) -> Result<(), String> {
        match self.sessions.iter().position(|session| &session.user_id == user_id) {
            Some(session_index) => {
                self.sessions.remove(session_index);
                self.write();
                Ok(())
            }
            None => Err(format!("Couldn't find currently-existing login session for user_id {}.", user_id))
        }
    }

    pub fn new_session(&mut self, session: Session) -> Result<(), String> {
        if !self.sessions.iter().any(|preexisting_session| preexisting_session.user_id == session.user_id) {
            self.sessions.push(session);
            self.write();
            Ok(())
        } else {
            Err(format!("Tried to create new session with user_id {}, but you already have a logged-in session with that user ID.", session.user_id))
        }
    }

    pub fn write(&self) {
        let updated_file = serde_json::to_string(&self.sessions).unwrap();
        write(&self.path, updated_file).unwrap();
    }
}

pub struct RoomWithCachedInfo {
    pub id: OwnedRoomId,
    pub name: Option<String>,
    pub canonical_alias: Option<OwnedRoomAliasId>,
    pub alt_aliases: Vec<OwnedRoomAliasId>,
    pub room: Room,
}

pub enum RoomIndexRetrievalError {
    MultipleRoomsWithSpecifiedName(Vec<String>),
    NoRoomsWithSpecifiedName,
}

//////////////////////////
//   Unshared helpers   //
//////////////////////////

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

////////////////////////
//   Shared helpers   //
////////////////////////

pub fn add_at_to_user_id_if_applicable(user_id: &str) -> String {
    if user_id.starts_with('@') {
        String::from(user_id)
    } else {
        format!("@{}", user_id)
    }
}

pub async fn nonfirst_login(user_id: &str, sessions_file: &SessionsFile) -> anyhow::Result<Client> {
    let normalized_user_id = add_at_to_user_id_if_applicable(user_id);
    let session = sessions_file.get(&normalized_user_id).unwrap();
    let user = UserId::parse(&session.user_id)?;
    let client = Client::builder().server_name(user.server_name()).build().await?;
    client.matrix_auth().restore_session(MatrixSession {
        meta: SessionMeta {
            user_id: user,
            device_id: session.device_id.into(),
        },
        tokens: MatrixSessionTokens {
            access_token: session.access_token,
            refresh_token: session.refresh_token,
        }
    }).await?;

    Ok(client)
}

//////////////////////////
//   Shared functions   //
//////////////////////////

pub async fn first_login(client: &Client, sessions_file: &mut SessionsFile, user_id: &str, password: &str, session_name: Option<String>) -> anyhow::Result<()> {
    let session_name = match session_name {
        Some(name) => name,
        None => format!("Trace (Session UUID: {})", Uuid::new_v4())
    };

    let login_result = client.matrix_auth().login_username(user_id, password).initial_device_display_name(&session_name).send().await?;
    // Add a branch with SSO support, once I know how that's supposed to work

    sessions_file.new_session(Session {
        user_id: login_result.user_id.to_string(),
        device_id: login_result.device_id.to_string(),
        access_token: login_result.access_token.to_string(),
        refresh_token: login_result.refresh_token,
    }).unwrap();

    Ok(())
}

pub async fn logout(client: &Client, sessions_file: &mut SessionsFile) -> anyhow::Result<()> {
    client.matrix_auth().logout().await?;
    sessions_file.delete_session(&client.user_id().unwrap().to_string()).unwrap();

    Ok(())
}

pub async fn list_sessions(sessions_file: &SessionsFile) -> anyhow::Result<Vec<(String, String)>> {
    let mut sessions_info = join_all(sessions_file.sessions.iter().map(|session| async {
        let client = nonfirst_login(&session.user_id, sessions_file).await?;
        let device_list = client.devices().await?.devices;
        let device_name = device_list.into_iter().find(|device| device.device_id == session.device_id).unwrap().display_name.unwrap_or_else(|| String::from("[Unnamed]"));
        anyhow::Result::<(String, String)>::Ok((session.user_id.clone(), device_name))
    })).await.into_iter().collect::<anyhow::Result<Vec<(String, String)>, _>>()?;
    sessions_info.sort_by(|(user_id_1, _display_name_1), (user_id_2, _display_name_2)| user_id_1.cmp(user_id_2)); // sort_by_key doesn't work here for weird lifetime reasons

    Ok(sessions_info)
}

pub async fn rename_session(client: &Client, new_session_name: &str) -> anyhow::Result<()> {
    client.rename_device(client.device_id().unwrap(), new_session_name).await?;

    Ok(())
}

pub async fn get_rooms_info(client: &Client) -> anyhow::Result<Vec<RoomWithCachedInfo>> {
    let mut rooms_info = client.joined_rooms().into_iter().map(|room| RoomWithCachedInfo {
        id: room.room_id().to_owned(),
        name: room.name(),
        canonical_alias: room.canonical_alias(),
        alt_aliases: room.alt_aliases(),
        room,
    }).collect::<Vec<RoomWithCachedInfo>>();
    rooms_info.sort_by(|room_1, room_2| match (&room_1.name, &room_2.name) {
        (Some(name_1), Some(name_2)) => name_1.cmp(&name_2),
        (Some(_name), None) => Ordering::Greater,
        (None, Some(_name)) => Ordering::Less,
        (None, None) => match (&room_1.canonical_alias, &room_2.canonical_alias) {
            (Some(alias_1), Some(alias_2)) => alias_1.cmp(&alias_2),
            (Some(_alias), None) => Ordering::Greater,
            (None, Some(_alias)) => Ordering::Less,
            (None, None) => room_1.id.cmp(&room_2.id),
        },
    });

    Ok(rooms_info)
}

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
        let messages = room_to_export_info.room.messages(MessagesOptions::forward()).await?; // Could async this better; try that at some point. Also, looks like for now this is going to get only the first 10 messages?
        let mut room_export = String::new();
        for event in messages.chunk {
            let event_deserialized = event.event.deserialize().unwrap(); // Add real error-handling in place of this unwrap
            let event_timestamp_millis = event_deserialized.origin_server_ts().0.into();
            let event_timestamp = DateTime::from_timestamp_millis(event_timestamp_millis).expect(&format!("Found message with millisecond timestamp {}, which can't be converted to datetime.", event_timestamp_millis)); // Add option to use local time zones
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
                        MessageType::Text(e) => format!("[{}] {}: {}", event_timestamp.to_rfc3339_opts(SecondsFormat::Millis, true), event_sender_string_representation, &e.body), // Add handling for formatted messages
                        _ => String::from("[Placeholder message]"),
                    }
                    _ => String::from("[Placeholder message-like]"),
                },
                AnyTimelineEvent::State(_e) => String::from("[Placeholder state-like]"),
            };
            // Add real handling here; this is unreadable, right now
            room_export.push_str(&format!("{}\n", event_stringified))
        }
        write(format!("{}.txt", format_export_filename(&room_to_export_info)), room_export).unwrap(); // Ideally let users pass format strings of some sort here
    }

    Ok(())
}
