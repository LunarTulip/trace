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
    subcommand: Subcommand,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum Subcommand {
    ListLogins(ListLogins),
    ListRooms(ListRooms),
    Login(Login),
    Logout(Logout),
}

#[derive(FromArgs)]
#[argh(subcommand, name = "list-logins")]
/// List currently-logged-in accounts
struct ListLogins {}

#[derive(FromArgs)]
#[argh(subcommand, name = "list-rooms")]
/// List rooms accessible from a given user ID's login
struct ListRooms {
    #[argh(positional)]
    /// user id (of the form @alice:example.com) to list rooms from
    user_id: String,
}

#[derive(FromArgs)]
#[argh(subcommand, name = "login")]
/// Log in a new account, creating a new session
struct Login {
    #[argh(positional)]
    /// user id (of the form @alice:example.com) to be logged in
    user_id: String,
    // #[argh(positional)]
    // /// password associated with user_id
    // password: String,
    #[argh(positional)]
    /// optional session name for use in place of the default randomized one
    session_name: Option<String>
}

#[derive(FromArgs)]
#[argh(subcommand, name = "logout")]
/// Log out a previously-logged-in account
struct Logout {
    #[argh(positional)]
    /// user id (of the form @alice:example.com) to be logged out
    user_id: String,
}

/////////////////
//   Helpers   //
/////////////////

async fn nonfirst_login(user_id: &str, sessions_file: &SessionsFile) -> anyhow::Result<Client> {
    let session = sessions_file.get(user_id).unwrap();
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

fn list_logins(sessions_file: &SessionsFile) {
    // In the long run, replace this with something properly structured with the retrieval and display factored apart from one another
    println!("Currently-logged-in sessions:");
    for session in &sessions_file.sessions {
        println!("{}", session.user_id)
    }
}

async fn list_rooms(config: ListRooms, sessions_file: &SessionsFile) -> anyhow::Result<()> {
    // In the long run, replace this with something properly structured with the retrieval and display factored apart from one another
    let client = nonfirst_login(&config.user_id, sessions_file).await?;
    client.sync_once(SyncSettings::new().set_presence(PresenceState::Offline)).await?;
    let rooms = client.joined_rooms().into_iter().map(|r| r.name()).collect::<Vec<Option<String>>>();
    println!("{:?}", rooms);

    Ok(())
}

async fn first_login(config: Login, sessions_file: &mut SessionsFile) -> anyhow::Result<()> {
    if let Ok(_) = sessions_file.get(&config.user_id) {
        panic!("Tried to log into account {}, but you were already logged in to this account.", &config.user_id); // Replace this with real error-handling.
    }

    println!("Please input password for account {}.", &config.user_id);
    let password = read_password().unwrap();

    let session_name = match config.session_name {
        Some(name) => name,
        None => format!("Trace (Session UUID: {})", Uuid::new_v4())
    };

    let user = UserId::parse(&config.user_id)?;
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

async fn logout(config: Logout, sessions_file: &mut SessionsFile) -> anyhow::Result<()> {
    let client = nonfirst_login(&config.user_id, sessions_file).await?;
    client.matrix_auth().logout().await?;
    sessions_file.delete_session(&config.user_id).unwrap();

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dirs = ProjectDirs::from("", "", "Trace").unwrap(); // Figure out qualifier and organization
    let mut sessions_file = SessionsFile::open([dirs.data_dir(), Path::new("sessions.json")].iter().collect());

    let args: Args = argh::from_env();
    match args.subcommand {
        Subcommand::ListLogins(_l) => list_logins(&sessions_file),
        Subcommand::ListRooms(l) => list_rooms(l, &sessions_file).await?,
        Subcommand::Login(l) => first_login(l, &mut sessions_file).await?,
        Subcommand::Logout(l) => logout(l, &mut sessions_file).await?,
    };

    // client_auth.logout().await?;

    Ok(())
}
