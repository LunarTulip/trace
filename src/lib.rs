use std::{
    cmp::Ordering,
    fs::{
        create_dir_all,
        read_to_string,
        write,
    },
    path::PathBuf,
};

use matrix_sdk::{
    matrix_auth::{
        MatrixSession,
        MatrixSessionTokens,
    }, 
    room::Room,
    ruma::{
        OwnedRoomAliasId,
        OwnedRoomId,
        UserId,
    },
    Client, 
    SessionMeta,
};
use serde::{
    Deserialize,
    Serialize,
};

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

/////////////////
//   Helpers   //
/////////////////

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

pub fn get_room_index_by_identifier(rooms_info: &Vec<RoomWithCachedInfo>, identifier: &str) -> Result<usize, RoomIndexRetrievalError> {
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

pub fn format_export_filename(room_info: &RoomWithCachedInfo) -> String {
    let (nonserver_id_component, server) = room_info.id.as_str().split_once(':').unwrap();
    match (&room_info.name, &room_info.canonical_alias) {
        (Some(name), Some(alias)) => format!("{} [{}, {}, {}]", name, alias.as_str().split_once(':').unwrap().0, nonserver_id_component, server),
        (Some(name), None) => format!("{} [{}, {}]", name, nonserver_id_component, server),
        (None, Some(alias)) => format!("{} [{}, {}]", alias.as_str().split_once(':').unwrap().0, nonserver_id_component, server),
        (None, None) => format!("{} [{}]", nonserver_id_component, server),
    }
}