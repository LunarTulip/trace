use std::{
    cmp::Ordering,
    fs::{
        create_dir_all,
        read_to_string,
        write,
    },
    path::PathBuf,
};

pub mod export;

use futures::future::join_all;
use matrix_sdk::{
    matrix_auth::{
        MatrixSession,
        MatrixSessionTokens,
    }, 
    ruma::{
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

////////////////////
//   Re-exports   //
////////////////////

pub use export::export;

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
