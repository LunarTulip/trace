use std::path::Path;

use trace::{
    SessionsFile,
    add_at_to_user_id_if_applicable,
    nonfirst_login,
};

use argh::FromArgs;
use directories::ProjectDirs;
use matrix_sdk::{
    config::SyncSettings,
    ruma::{
        presence::PresenceState,
        UserId,
    },
    Client,
};
use rpassword::read_password;

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

//////////////
//   Main   //
//////////////

async fn export(config: Export, sessions_file: &SessionsFile) -> anyhow::Result<()> {
    let client = nonfirst_login(&config.user_id, sessions_file).await?;
    client.sync_once(SyncSettings::new().set_presence(PresenceState::Offline)).await?;
    trace::export(&client, config.rooms).await?;

    Ok(())
}

async fn list_rooms(config: ListRooms, sessions_file: &SessionsFile) -> anyhow::Result<()> {
    let normalized_user_id = add_at_to_user_id_if_applicable(&config.user_id);
    let client = nonfirst_login(&normalized_user_id, sessions_file).await?;
    client.sync_once(SyncSettings::new().set_presence(PresenceState::Offline)).await?;

    let rooms_info = trace::get_rooms_info(&client).await?;
    println!("Rooms joined by {}:", normalized_user_id);
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
    let sessions = trace::list_sessions(sessions_file).await?;
    if sessions.len() > 0 {
        println!("Currently-logged-in sessions:");
        for (user_id, session_name) in sessions {
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
        panic!("Tried to log into account {}, but you already have a session logged into this account.", &normalized_user_id); // Replace this with real error-handling.
    }

    println!("Please input password for account {}.", &normalized_user_id);
    let password = read_password().unwrap();

    let user = UserId::parse(&normalized_user_id)?;
    let client = Client::builder().server_name(user.server_name()).build().await?;

    trace::first_login(&client, sessions_file, &normalized_user_id, &password, config.session_name).await?;

    Ok(())
}

async fn session_logout(config: SessionLogout, sessions_file: &mut SessionsFile) -> anyhow::Result<()> {
    let client = nonfirst_login(&config.user_id, sessions_file).await?;
    trace::logout(&client, sessions_file).await?;

    Ok(())
}

async fn session_rename(config: SessionRename, sessions_file: &SessionsFile) -> anyhow::Result<()> {
    let client = nonfirst_login(&config.user_id, sessions_file).await?;
    trace::rename_session(&client, &config.session_name).await?;

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
