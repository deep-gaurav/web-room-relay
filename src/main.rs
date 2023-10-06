use anyhow::Result;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::{Receiver, Sender};
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing::warn;
use tracing::Instrument;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::EnvFilter;

use wtransport::endpoint::IncomingSession;
use wtransport::tls::Certificate;
use wtransport::Connection;
use wtransport::Endpoint;
use wtransport::ServerConfig;

macro_rules! regex {
    ($re:literal $(,)?) => {{
        static RE: once_cell::sync::OnceCell<regex::Regex> = once_cell::sync::OnceCell::new();
        RE.get_or_init(|| regex::Regex::new($re).unwrap())
    }};
}

pub struct Room {
    _id: String,
    users: Vec<User>,
}

type MessagePacket = Mesagge;

pub struct User {
    id: String,
    sender: Sender<MessagePacket>,
}

static ROOMS: once_cell::sync::OnceCell<Arc<tokio::sync::RwLock<HashMap<String, Room>>>> =
    once_cell::sync::OnceCell::new();

#[derive(Debug, serde::Serialize)]
pub enum Mesagge {
    UserConnected(String),
    UserDisconnected(String),
    UserMessage(String, Vec<u8>),
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    ROOMS
        .set(Arc::new(tokio::sync::RwLock::new(HashMap::new())))
        .map_err(|_| anyhow::anyhow!("Cant init"))?;

    let config = ServerConfig::builder()
        .with_bind_default(4433)
        .with_certificate(Certificate::load("cert.pem", "key.pem")?)
        .keep_alive_interval(Some(Duration::from_secs(3)))
        .build();

    let server = Endpoint::server(config)?;

    info!("Server ready!");

    for id in 0.. {
        let incoming_session = server.accept().await;
        tokio::spawn(handle_connection(incoming_session).instrument(info_span!("Connection", id)));
    }

    Ok(())
}

async fn handle_connection(incoming_session: IncomingSession) {
    let (room_id, connection) = match get_connection_from_session(incoming_session).await {
        Ok(con) => con,
        Err(err) => {
            warn!("Errored {err:?}");
            return;
        }
    };
    let user_id = connection.stable_id().to_string();
    let (sender, receiver) = tokio::sync::mpsc::channel(20);
    let user = User {
        id: user_id.clone(),
        sender,
    };
    let mut rooms: tokio::sync::RwLockWriteGuard<'_, HashMap<String, Room>> =
        ROOMS.get().unwrap().write().await;
    if let Some(room) = rooms.get_mut(&room_id) {
        let mut send_futures = vec![];
        for user in room.users.iter() {
            send_futures.push(user.sender.send(Mesagge::UserConnected(user_id.clone())));
        }
        futures::future::join_all(send_futures).await;
        room.users.push(user);
    } else {
        rooms.insert(
            room_id.clone(),
            Room {
                _id: room_id.clone(),
                users: vec![user],
            },
        );
    }
    drop(rooms);
    let result = handle_connection_impl(&user_id, &room_id, connection, receiver).await;
    let mut rooms: tokio::sync::RwLockWriteGuard<'_, HashMap<String, Room>> =
        ROOMS.get().unwrap().write().await;
    if let Some(room) = rooms.get_mut(&room_id) {
        if let Some(user_index) = room.users.iter().position(|el| el.id == user_id) {
            room.users.remove(user_index);
        }
        if room.users.is_empty() {
            rooms.remove(&room_id);
        } else {
            let mut send_futures = vec![];

            for user in room.users.iter() {
                send_futures.push(user.sender.send(Mesagge::UserDisconnected(user_id.clone())))
            }
            futures::future::join_all(send_futures).await;
        }
    }
    error!("{:?}", result);
}

async fn get_connection_from_session(
    incoming_session: IncomingSession,
) -> Result<(String, Connection)> {
    let session_request = incoming_session.await?;

    let path = session_request.path().to_owned();

    let reg = regex!(r"(?m)^\/room\/(\w{6})$");
    if let Some(room_id) = reg.captures(&path).and_then(|cap| cap.get(1)) {
        let room_id = room_id.as_str();
        info!(
            "New session: Authority: '{}', Path: '{}', Room_id: '{}'",
            session_request.authority(),
            path,
            room_id,
        );

        let connection = session_request.accept().await?;
        Ok((room_id.to_string(), connection))
    } else {
        session_request.not_found().await;
        Err(anyhow::anyhow!("Not valid path"))
    }
}
async fn handle_connection_impl(
    user_id: &str,
    room_id: &str,
    connection: Connection,
    mut receiver: Receiver<MessagePacket>,
) -> Result<()> {
    let mut buffer = vec![0; 65536].into_boxed_slice();

    info!("Waiting for session request...");

    info!("Waiting for data from client...");
    loop {
        tokio::select! {
            stream = connection.accept_bi() => {
                let mut stream = stream?;
                info!("Accepted BI stream");

                let bytes_read = match stream.1.read(&mut buffer).await? {
                    Some(bytes_read) => bytes_read,
                    None => continue,
                };

                let str_data = std::str::from_utf8(&buffer[..bytes_read])?;

                info!("Received (bi) '{str_data}' from client");

                stream.0.write_all(b"ACK").await?;
            }
            stream = connection.accept_uni() => {
                let mut stream = stream?;
                info!("Accepted UNI stream");

                let bytes_read = match stream.read(&mut buffer).await? {
                    Some(bytes_read) => bytes_read,
                    None => continue,
                };

                let str_data = std::str::from_utf8(&buffer[..bytes_read])?;

                info!("Received (uni) '{str_data}' from client");

                let mut stream = connection.open_uni().await?.await?;
                stream.write_all(b"ACK").await?;
            }
            dgram = connection.receive_datagram() => {
                let dgram = dgram?;
                let dgram_veg = (&dgram).to_vec();
                {
                    let  rooms = ROOMS.get().unwrap().read().await;
                    if let Some(room) = rooms.get(room_id) {
                        for user in room.users.iter(){
                            if user.id != user_id{
                                let msg = Mesagge::UserMessage(user_id.to_string(),dgram_veg.clone());
                                if let Err(err) = user.sender.send(msg).await{
                                    warn!("Failed tosend msg {err:?}")
                                }
                            }
                        }
                    }
                }
            }
            dragm = receiver.recv() => {
                if let Some(dragm)= dragm{
                    if let Ok(bin) = bincode::serialize(&dragm){
                        connection.send_datagram(&bin)?;
                    };
                }
            }
        }
    }
}

fn init_logging() {
    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    tracing_subscriber::fmt()
        .with_target(true)
        .with_level(true)
        .with_env_filter(env_filter)
        .init();
}
