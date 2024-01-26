use std::{
    cmp::Ordering,
    fs::{
        create_dir_all,
        read_to_string,
        write,
    },
    path::{
        Path,
        PathBuf,
    }
};

use argh::FromArgs;
use directories::ProjectDirs;
use futures::future::join_all;
use matrix_sdk::{
    config::SyncSettings, matrix_auth::{
        MatrixSession,
        MatrixSessionTokens,
    }, 
    room::{
        MessagesOptions,
        Room,
    },
    ruma::{
        OwnedRoomAliasId,
        OwnedRoomId,
        UserId,
        presence::PresenceState,
    }, Client, SessionMeta
};
use rpassword::read_password;
use serde::{
    Deserialize,
    Serialize,
};
use uuid::Uuid;

///////////////////////
//   Non-arg types   //
///////////////////////

#[derive(Clone, Deserialize, Serialize)]
struct Session {
    user_id: String,
    device_id: String,
    access_token: String,
    refresh_token: Option<String>,
}

struct SessionsFile {
    path: PathBuf,
    sessions: Vec<Session>,
}

impl SessionsFile {
    fn open(path: PathBuf) -> Self {
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

    fn get(&self, user_id: &str) -> Result<Session, String> {
        match self.sessions.iter().find(|session| &session.user_id == user_id) {
            Some(session) => Ok(session.clone()),
            None => Err(format!("Couldn't find currently-existing login session for user_id {}.", user_id))
        }
    }

    fn delete_session(&mut self, user_id: &str) -> Result<(), String> {
        match self.sessions.iter().position(|session| &session.user_id == user_id) {
            Some(session_index) => {
                self.sessions.remove(session_index);
                self.write();
                Ok(())
            }
            None => Err(format!("Couldn't find currently-existing login session for user_id {}.", user_id))
        }
    }

    fn new_session(&mut self, session: Session) -> Result<(), String> {
        if !self.sessions.iter().any(|preexisting_session| preexisting_session.user_id == session.user_id) {
            self.sessions.push(session);
            self.write();
            Ok(())
        } else {
            Err(format!("Tried to create new session with user_id {}, but you already have a logged-in session with that user ID.", session.user_id))
        }
    }

    fn write(&self) {
        let updated_file = serde_json::to_string(&self.sessions).unwrap();
        write(&self.path, updated_file).unwrap();
    }
}

struct RoomWithCachedInfo {
    id: OwnedRoomId,
    name: Option<String>,
    canonical_alias: Option<OwnedRoomAliasId>,
    alt_aliases: Vec<OwnedRoomAliasId>,
    room: Room,
}

enum RoomIndexRetrievalError {
    MultipleRoomsWithSpecifiedName(Vec<String>),
    NoRoomsWithSpecifiedName,
}

//////////////
//   Args   //
//////////////

#[derive(FromArgs)]
/// Trace Matrix downloader client
struct Args {
    #[argh(subcommand)]
    subcommand: RootSubcommand,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum RootSubcommand {
    Export(Export),
    ListRooms(ListRooms),
    Session(SessionCommand),
}

#[derive(FromArgs)]
#[argh(subcommand, name = "export")]
/// Export logs from rooms
struct Export {
    #[argh(positional)]
    /// user_id (of the form @alice:example.com) to export rooms accessible to
    user_id: String,
    #[argh(positional)]
    /// space-separated list of room IDs (of the form !abcdefghijklmnopqr:example.com), aliases (of the form #room:example.com), or names to export
    rooms: Vec<String>,
}

#[derive(FromArgs)]
#[argh(subcommand, name = "list-rooms")]
/// List rooms accessible from a given user ID's login
struct ListRooms {
    #[argh(positional)]
    /// user id (of the form @alice:example.com) to list rooms from
    user_id: String,
}

#[derive(FromArgs)]
#[argh(subcommand, name = "session")]
/// Add, remove, list, or modify sessions
struct SessionCommand {
    #[argh(subcommand)]
    subcommand: SessionSubcommand,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum SessionSubcommand {
    List(SessionList),
    Login(SessionLogin),
    Logout(SessionLogout),
    Rename(SessionRename),
}

#[derive(FromArgs)]
#[argh(subcommand, name = "list")]
/// List currently-logged-in accounts
struct SessionList {}

#[derive(FromArgs)]
#[argh(subcommand, name = "login")]
/// Log in a new account, creating a new session
struct SessionLogin {
    #[argh(positional)]
    /// user id (of the form @alice:example.com) to be logged in
    user_id: String,
    #[argh(positional)]
    /// optional session name for use in place of the default randomized one
    session_name: Option<String>
}

#[derive(FromArgs)]
#[argh(subcommand, name = "logout")]
/// Log out a previously-logged-in account
struct SessionLogout {
    #[argh(positional)]
    /// user id (of the form @alice:example.com) to be logged out
    user_id: String,
}

#[derive(FromArgs)]
#[argh(subcommand, name = "rename")]
/// Rename a logged-in session
struct SessionRename {
    #[argh(positional)]
    /// user id (of the form @alice:example.com) to be renamed
    user_id: String,
    #[argh(positional)]
    /// new name for session
    session_name: String,
}

/////////////////
//   Helpers   //
/////////////////

fn add_at_to_user_id_if_applicable(user_id: &str) -> String {
    if user_id.starts_with('@') {
        String::from(user_id)
    } else {
        format!("@{}", user_id)
    }
}

async fn nonfirst_login(user_id: &str, sessions_file: &SessionsFile) -> anyhow::Result<Client> {
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

async fn get_rooms_info(client: &Client) -> anyhow::Result<Vec<RoomWithCachedInfo>> {
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

async fn export(config: Export, sessions_file: &SessionsFile) -> anyhow::Result<()> {
    // Allow setting export destination other than "directly where run"
    let client = nonfirst_login(&config.user_id, sessions_file).await?;
    client.sync_once(SyncSettings::new().set_presence(PresenceState::Offline)).await?;

    let accessible_rooms_info = get_rooms_info(&client).await?; // This should be possible to optimize out for request-piles without names included, given client.resolve_room_alias and client.get_room. Although that might end up actually costlier if handled indelicately, since it'll involve more serial processing.

    for room_identifier in config.rooms {
        let room_to_export_info = match get_room_index_by_identifier(&accessible_rooms_info, &room_identifier) {
            Ok(index) => &accessible_rooms_info[index],
            Err(e) => match e {
                RoomIndexRetrievalError::MultipleRoomsWithSpecifiedName(room_ids) => {
                    println!("Found more than one room accessible to {} with name {}. Room IDs: {:?}", config.user_id, room_identifier, room_ids);
                    continue
                },
                RoomIndexRetrievalError::NoRoomsWithSpecifiedName => {
                    println!("Couldn't find any rooms accessible to {} with name {}.", config.user_id, room_identifier);
                    continue
                },
            }
        };
        let messages = room_to_export_info.room.messages(MessagesOptions::forward()).await?; // Could async this better; try that at some point. Also, looks like for now this is going to get only the first 10 messages?
        let mut room_export = String::new();
        for event in messages.chunk {
            // Add real handling here; this is unreadable, right now
            room_export.push_str(&format!("{:?}\n", event))
        }
        write(format!("{}.txt", format_export_filename(&room_to_export_info)), room_export).unwrap(); // Ideally let users pass format strings of some sort here
    }

    Ok(())
}

async fn list_rooms(config: ListRooms, sessions_file: &SessionsFile) -> anyhow::Result<()> {
    let client = nonfirst_login(&config.user_id, sessions_file).await?;
    client.sync_once(SyncSettings::new().set_presence(PresenceState::Offline)).await?;

    let rooms_info = get_rooms_info(&client).await?;
    println!("Rooms joined by {}:", config.user_id);
    for room_info in rooms_info {
        let room_name = match room_info.name {
            Some(name) => name,
            None => String::from("[Unnamed]"),
        };
        let room_alias = match room_info.canonical_alias {
            Some(alias) => alias.to_string(),
            None => String::from("[No canonical alias]"),
        };
        let room_id = room_info.id;
        println!("{} | {} | {}", room_name, room_alias, room_id) // Replace with properly-justified table-formatting in the future
    }

    Ok(())
}

async fn session_list(sessions_file: &SessionsFile) -> anyhow::Result<()> {
    if sessions_file.sessions.len() > 0 {
        let mut session_info_to_print = join_all(sessions_file.sessions.iter().map(|session| async {
            let client = nonfirst_login(&session.user_id, sessions_file).await?;
            let device_list = client.devices().await?.devices;
            let device_name = device_list.into_iter().find(|device| device.device_id == session.device_id).unwrap().display_name.unwrap_or_else(|| String::from("[Unnamed]"));
            anyhow::Result::<(&str, String)>::Ok((&session.user_id, device_name))
        })).await.into_iter().collect::<anyhow::Result<Vec<(&str, String)>, _>>()?;
        session_info_to_print.sort_by_key(|(user_id, _display_name)| *user_id);

        println!("Currently-logged-in sessions:");
        for (user_id, session_name) in session_info_to_print {
            println!("{} | {}", user_id, session_name) // Replace with properly-justified table-formatting in the future
        }
    } else {
        println!("You have no sessions currently logged in.");
    }

    Ok(())
}

async fn session_login(config: SessionLogin, sessions_file: &mut SessionsFile) -> anyhow::Result<()> {
    let normalized_user_id = add_at_to_user_id_if_applicable(&config.user_id);
    if let Ok(_) = sessions_file.get(&normalized_user_id) {
        panic!("Tried to log into account {}, but you were already logged into this account.", &normalized_user_id); // Replace this with real error-handling.
    }

    println!("Please input password for account {}.", &normalized_user_id);
    let password = read_password().unwrap();

    let session_name = match config.session_name {
        Some(name) => name,
        None => format!("Trace (Session UUID: {})", Uuid::new_v4())
    };

    let user = UserId::parse(&normalized_user_id)?;
    let client = Client::builder().server_name(user.server_name()).build().await?;

    let login_result = client.matrix_auth().login_username(user, &password).initial_device_display_name(&session_name).send().await?;
    // Add a branch with SSO support, once I know how that's supposed to work

    sessions_file.new_session(Session {
        user_id: login_result.user_id.to_string(),
        device_id: login_result.device_id.to_string(),
        access_token: login_result.access_token.to_string(),
        refresh_token: login_result.refresh_token,
    }).unwrap();

    Ok(())
}

async fn session_logout(config: SessionLogout, sessions_file: &mut SessionsFile) -> anyhow::Result<()> {
    let client = nonfirst_login(&config.user_id, sessions_file).await?;
    client.matrix_auth().logout().await?;
    sessions_file.delete_session(&config.user_id).unwrap();

    Ok(())
}

async fn session_rename(config: SessionRename, sessions_file: &SessionsFile) -> anyhow::Result<()> {
    let client = nonfirst_login(&config.user_id, sessions_file).await?;
    client.rename_device(client.device_id().unwrap(), &config.session_name).await?;

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dirs = ProjectDirs::from("", "", "Trace").unwrap(); // Figure out qualifier and organization
    let mut sessions_file = SessionsFile::open([dirs.data_dir(), Path::new("sessions.json")].iter().collect());

    let args: Args = argh::from_env();
    match args.subcommand {
        RootSubcommand::Export(config) => export(config, &sessions_file).await?,
        RootSubcommand::ListRooms(config) => list_rooms(config, &sessions_file).await?,
        RootSubcommand::Session(s) => match s.subcommand {
            SessionSubcommand::List(_) => session_list(&sessions_file).await?,
            SessionSubcommand::Login(config) => session_login(config, &mut sessions_file).await?,
            SessionSubcommand::Logout(config) => session_logout(config, &mut sessions_file).await?,
            SessionSubcommand::Rename(config) => session_rename(config, &sessions_file).await?,
        }
    };

    Ok(())
}
