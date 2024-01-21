use std::{
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
    Client,
    SessionMeta,
    config::SyncSettings,
    matrix_auth::{
        MatrixSession,
        MatrixSessionTokens,
    },
    ruma::{
        UserId,
        presence::PresenceState,
    }
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
    ListRooms(ListRooms),
    Session(SessionCommand),
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

//////////////
//   Main   //
//////////////

async fn list_rooms(config: ListRooms, sessions_file: &SessionsFile) -> anyhow::Result<()> {
    // In the long run, replace this with something properly structured with the retrieval and display factored apart from one another
    let client = nonfirst_login(&config.user_id, sessions_file).await?;
    client.sync_once(SyncSettings::new().set_presence(PresenceState::Offline)).await?;
    let rooms = client.joined_rooms().into_iter().map(|r| r.name()).collect::<Vec<Option<String>>>();
    println!("{:?}", rooms);

    Ok(())
}

async fn session_list(sessions_file: &SessionsFile) -> anyhow::Result<()> {
    // In the long run, replace this with something properly structured with the retrieval and display factored apart from one another
    if sessions_file.sessions.len() > 0 {
        let session_info_to_print = join_all(sessions_file.sessions.iter().map(|session| async {
            let client = nonfirst_login(&session.user_id, sessions_file).await?;
            let device_list = client.devices().await?.devices;
            let device_name = device_list.into_iter().find(|device| device.device_id == session.device_id).unwrap().display_name.unwrap_or_else(|| String::from("[Unnamed]"));
            anyhow::Result::<(&str, String)>::Ok((&session.user_id, device_name))
        })).await.into_iter().collect::<anyhow::Result<Vec<(&str, String)>, _>>()?;
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
