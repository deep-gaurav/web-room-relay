use anyhow::Result;
use dashmap::DashMap;
use tokio::sync::RwLock;

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
type BroadCastMsg = (String, u32, Vec<u8>);

pub struct User {
    id: u32,
    sender: Sender<MessagePacket>,
}

static ROOMS: once_cell::sync::OnceCell<Arc<RwLock<HashMap<String, Room>>>> =
    once_cell::sync::OnceCell::new();

#[derive(Debug, serde::Serialize)]
pub enum Mesagge {
    RoomJoined(u32),
    UserConnected(u32),
    UserDisconnected(u32),
    UserMessage(u32, Vec<u8>),
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    ROOMS
        .set(Arc::new(RwLock::new(HashMap::new())))
        .map_err(|_| anyhow::anyhow!("Cant init"))?;

    let config = ServerConfig::builder()
        .with_bind_default(4433)
        .with_certificate(Certificate::load("cert.pem", "key.pem")?)
        .keep_alive_interval(Some(Duration::from_secs(3)))
        .build();

    let server = Endpoint::server(config)?;

    info!("Server ready!");
    let (broadcast_sender, broadcast_receiver) = tokio::sync::mpsc::channel(10);
    tokio::spawn(handle_broadcast(broadcast_receiver));
    for id in 0.. {
        info!("Awaiting new connection request");
        let incoming_session = server.accept().await;
        info!("Some connecection request received");
        tokio::spawn(
            handle_connection(incoming_session, broadcast_sender.clone())
                .instrument(info_span!("Connection", id)),
        );
    }

    Ok(())
}

async fn handle_broadcast(mut reciver: Receiver<BroadCastMsg>) {
    loop {
        let msg = reciver.recv().await;
        if let Some(msg) = msg {
            {
                if let Some(room) = ROOMS.get() {
                    let room = room.read().await;
                    let room = room.get(&msg.0);

                    if let Some(room) = room {
                        let mut futures = vec![];

                        for user in room.users.iter() {
                            if user.id != msg.1 {
                                futures.push(
                                    user.sender.send(Mesagge::UserMessage(msg.1, msg.2.clone())),
                                );
                            }
                        }
                        futures::future::join_all(futures).await;
                    }
                }
            }
        }
    }
}

async fn handle_connection(incoming_session: IncomingSession, broadcaster: Sender<BroadCastMsg>) {
    let (room_id, connection) = match get_connection_from_session(incoming_session).await {
        Ok(con) => con,
        Err(err) => {
            warn!("Errored {err:?}");
            return;
        }
    };
    let user_id = rand::random();
    let (sender, receiver) = tokio::sync::mpsc::channel(20);
    let user = User {
        id: user_id,
        sender,
    };
    {
        let mut rooms = ROOMS.get().unwrap().write().await;
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
    }
    info!("Running room {room_id}");
    let result = handle_connection_impl(user_id, &room_id, connection, receiver, broadcaster).await;
    info!("Room has stopped {room_id}");
    {
        let mut rooms = ROOMS.get().unwrap().write().await;
        if let Some(room) = rooms.get_mut(&room_id) {
            if let Some(user_index) = room.users.iter().position(|el| el.id == user_id) {
                room.users.remove(user_index);
            }
            if room.users.is_empty() {
                info!("Room empty, deleting room");
                rooms.remove(&room_id);
                info!("Room removed");
            } else {
                let mut send_futures = vec![];

                for user in room.users.iter() {
                    send_futures.push(user.sender.send(Mesagge::UserDisconnected(user_id.clone())))
                }
                info!("Notifying {} users of user leaving", send_futures.len());
                futures::future::join_all(send_futures).await;
                info!("Notified everyone")
            }
        }
    }
    info!("Room cleaned up {room_id}");
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
        info!("Connection Opened: {room_id}",);
        Ok((room_id.to_string(), connection))
    } else {
        session_request.not_found().await;
        Err(anyhow::anyhow!("Not valid path"))
    }
}

async fn handle_connection_impl(
    user_id: u32,
    room_id: &str,
    connection: Connection,
    mut receiver: Receiver<MessagePacket>,
    broadcaster: Sender<BroadCastMsg>,
) -> Result<()> {
    let mut buffer = vec![0; 65536].into_boxed_slice();

    info!("Waiting for session request...");

    info!("Waiting for data from client...");
    let mut received_data = false;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                if !received_data{
                    if let Ok(bin) = bincode::serialize(&Mesagge::RoomJoined(user_id)){
                        connection.send_datagram(&bin)?;
                    };
                }
            }
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
                received_data = true;
                let dgram = dgram?;
                let dgram_veg = (&dgram).to_vec();
                if let Err(err)=broadcaster.send((room_id.to_string(),user_id, dgram_veg)).await{
                    warn!("Broadcast error {err:?}")
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
