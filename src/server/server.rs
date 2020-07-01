//https://github.com/snapview/tokio-tungstenite/blob/master/examples/server.rs

use super::skribbl::{get_time_now, SkribblState};
use crate::{
    data,
    message::{InitialState, ToClientMsg, ToServerMsg},
};
use data::{CommandMsg, Message, Username};
use futures_timer::Delay;
use futures_util::{SinkExt, StreamExt};
use std::io::Read;
use std::net::SocketAddr;
use std::{collections::HashMap, path::PathBuf, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::Mutex,
};

pub const ROUND_DURATION: u64 = 120;

type Result<T> = std::result::Result<T, ServerError>;

#[derive(Debug)]
pub enum ServerError {
    UserNotFound(String),
    SendError(String),
    WsError(tungstenite::error::Error),
    IOError(std::io::Error),
}

impl<T> From<tokio::sync::mpsc::error::SendError<T>> for ServerError {
    fn from(err: tokio::sync::mpsc::error::SendError<T>) -> Self {
        ServerError::SendError(err.to_string())
    }
}

impl From<tungstenite::error::Error> for ServerError {
    fn from(err: tungstenite::error::Error) -> Self {
        ServerError::WsError(err)
    }
}

impl From<std::io::Error> for ServerError {
    fn from(err: std::io::Error) -> Self {
        ServerError::IOError(err)
    }
}

#[derive(Debug)]
enum ServerEvent {
    ToServerMsg(Username, ToServerMsg),
    UserJoined(UserSession),
    UserLeft(Username),
    Tick,
}

#[derive(Debug)]
struct UserSession {
    username: Username,
    msg_send: Mutex<tokio::sync::mpsc::Sender<ToClientMsg>>,
    close_send: tokio::sync::mpsc::Sender<()>,
}

impl UserSession {
    fn new(
        username: Username,
        msg_send: tokio::sync::mpsc::Sender<ToClientMsg>,
        close_send: tokio::sync::mpsc::Sender<()>,
    ) -> Self {
        UserSession {
            username,
            msg_send: Mutex::new(msg_send),
            close_send,
        }
    }

    async fn close(mut self) -> Result<()> {
        self.close_send.send(()).await?;
        Ok(())
    }

    async fn send(&self, msg: ToClientMsg) -> Result<()> {
        self.msg_send.lock().await.send(msg.clone()).await?;
        Ok(())
    }
}

#[derive(Debug)]
pub enum GameState {
    FreeDraw,
    Skribbl(SkribblState),
}

impl GameState {
    fn skribbl_state(&self) -> Option<&SkribblState> {
        match self {
            GameState::Skribbl(state) => Some(state),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct ServerState {
    sessions: HashMap<Username, UserSession>,
    pub lines: Vec<data::Line>,
    pub dimensions: (usize, usize),
    pub game_state: GameState,
    pub words: Option<Vec<String>>,
}

impl ServerState {
    fn new(game_state: GameState, dimensions: (usize, usize), words: Option<Vec<String>>) -> Self {
        ServerState {
            sessions: HashMap::new(),
            lines: Vec::new(),
            dimensions,
            game_state,
            words,
        }
    }

    async fn remove_player(&mut self, username: &Username) -> Result<()> {
        self.sessions.remove(username).map(|x| x.close());
        match self.game_state {
            GameState::Skribbl(ref mut state) => {
                state.remove_user(username);
                if state.drawing_user == *username {
                    state.next_turn();
                }
                let state = state.clone();
                self.broadcast(ToClientMsg::SkribblStateChanged(state))
                    .await?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn on_command_msg(&mut self, _username: &Username, msg: &CommandMsg) -> Result<()> {
        match msg {
            CommandMsg::KickPlayer(kicked_player) => self.remove_player(kicked_player).await?,
        }
        Ok(())
    }

    async fn on_new_message(&mut self, username: Username, msg: data::Message) -> Result<()> {
        let mut did_solve = false;
        match self.game_state {
            GameState::Skribbl(ref mut state) => {
                let can_guess = state.can_guess(&username);
                let current_word = &state.current_word;

                if let Some(player_state) = state.player_states.get_mut(&username) {
                    if can_guess && msg.text().eq_ignore_ascii_case(&current_word) {
                        player_state.on_solve();
                        did_solve = true;
                        let all_solved = state.did_all_solve();
                        let old_word = state.current_word.clone();
                        if all_solved {
                            state.next_turn();
                        }
                        let state = state.clone();
                        self.broadcast(ToClientMsg::SkribblStateChanged(state))
                            .await?;
                        self.broadcast_system_msg(format!("{} guessed it!", username))
                            .await?;
                        if all_solved {
                            self.lines.clear();
                            self.broadcast(ToClientMsg::ClearCanvas).await?;
                            self.broadcast_system_msg(format!("The word was: \"{}\"", old_word))
                                .await?;
                        }
                    }
                }
            }
            GameState::FreeDraw => {
                if let Some(words) = &self.words {
                    let skribbl_state = SkribblState::with_users(
                        self.sessions.keys().cloned().collect::<Vec<Username>>(),
                        words.clone(),
                    );
                    self.game_state = GameState::Skribbl(skribbl_state.clone());
                    self.broadcast(ToClientMsg::SkribblStateChanged(skribbl_state))
                        .await?;
                }
            }
        }

        if !did_solve {
            self.broadcast(ToClientMsg::NewMessage(msg)).await?;
        }

        Ok(())
    }

    async fn on_to_srv_msg(&mut self, username: Username, msg: ToServerMsg) -> Result<()> {
        match msg {
            ToServerMsg::CommandMsg(msg) => {
                self.on_command_msg(&username, &msg).await?;
            }
            ToServerMsg::NewMessage(message) => {
                self.on_new_message(username, message).await?;
            }
            ToServerMsg::NewLine(line) => {
                self.lines.push(line);
                self.broadcast(ToClientMsg::NewLine(line)).await?;
            }
            ToServerMsg::ClearCanvas => {
                self.lines.clear();
                self.broadcast(ToClientMsg::ClearCanvas).await?;
            }
        }
        Ok(())
    }

    pub async fn on_tick(&mut self) -> Result<()> {
        if let GameState::Skribbl(ref mut state) = self.game_state {
            let elapsed_time = get_time_now() - state.round_start_time;
            let remaining_time = ROUND_DURATION - elapsed_time;
            if remaining_time <= 0 {
                let old_word = state.current_word.clone();
                state.next_turn();
                let state = state.clone();
                self.broadcast(ToClientMsg::SkribblStateChanged(state))
                    .await?;
                self.lines.clear();
                self.broadcast(ToClientMsg::ClearCanvas).await?;
                self.broadcast_system_msg(format!("The word was: \"{}\"", old_word))
                    .await?;
            }
            self.broadcast(ToClientMsg::TimeChanged(remaining_time as u32))
                .await?;
        }
        Ok(())
    }

    pub async fn on_user_joined(&mut self, session: UserSession) -> Result<()> {
        if let GameState::Skribbl(ref mut state) = self.game_state {
            state.add_player(session.username.clone());
            let state = state.clone();
            self.broadcast(ToClientMsg::SkribblStateChanged(state))
                .await?;
            self.broadcast_system_msg(format!("{} joined", session.username))
                .await?;
        }

        let initial_state = InitialState {
            lines: self.lines.clone(),
            skribbl_state: self.game_state.skribbl_state().cloned(),
            dimensions: self.dimensions,
        };
        session
            .send(ToClientMsg::InitialState(initial_state))
            .await?;
        self.sessions.insert(session.username.clone(), session);
        Ok(())
    }

    /// send a Message::SystemMsg to all active sessions
    async fn broadcast_system_msg(&self, msg: String) -> Result<()> {
        self.broadcast(ToClientMsg::NewMessage(Message::SystemMsg(msg)))
            .await?;
        Ok(())
    }

    /// send a ToClientMsg to a specific session
    #[allow(dead_code)]
    pub async fn send_to(&self, user: &Username, msg: ToClientMsg) -> Result<()> {
        self.sessions
            .get(user)
            .ok_or(ServerError::UserNotFound(user.to_string()))?
            .send(msg)
            .await?;
        Ok(())
    }

    /// broadcast a ToClientMsg to all running sessions
    async fn broadcast(&self, msg: ToClientMsg) -> Result<()> {
        for (_, session) in self.sessions.iter() {
            session.send(msg.clone()).await?;
        }
        Ok(())
    }

    /// run the main server, reacting to any server events
    async fn run(&mut self, mut evt_recv: tokio::sync::mpsc::Receiver<ServerEvent>) -> Result<()> {
        loop {
            if let Some(evt) = evt_recv.recv().await {
                match evt {
                    ServerEvent::ToServerMsg(name, msg) => {
                        self.on_to_srv_msg(name.into(), msg).await?
                    }
                    ServerEvent::UserJoined(session) => self.on_user_joined(session).await?,
                    ServerEvent::UserLeft(username) => self.remove_player(&username).await?,
                    ServerEvent::Tick => self.on_tick().await?,
                }
            }
        }
    }
}

pub async fn run_server(
    addr: &str,
    dimensions: (usize, usize),
    word_file: Option<PathBuf>,
) -> Result<()> {
    println!("Running server on {}", addr);
    let mut server_listener = TcpListener::bind(addr)
        .await
        .expect("Could not start webserver (could not bind)");

    let maybe_words = word_file.map(|path| read_words_file(&path).unwrap());

    let (srv_event_send, srv_event_recv) = tokio::sync::mpsc::channel::<ServerEvent>(1);
    let mut server_state = ServerState::new(GameState::FreeDraw, dimensions, maybe_words);

    tokio::spawn(async move {
        server_state.run(srv_event_recv).await.unwrap();
    });

    while let Ok((stream, _)) = server_listener.accept().await {
        let peer = stream.peer_addr().expect("Peer didn't have an address");
        tokio::spawn(handle_connection(peer, stream, srv_event_send.clone()));
    }
    Ok(())
}

async fn handle_connection(
    peer: SocketAddr,
    stream: TcpStream,
    mut srv_event_send: tokio::sync::mpsc::Sender<ServerEvent>,
) -> Result<()> {
    let ws_stream = tokio_tungstenite::accept_async(stream).await?;
    println!("new WebSocket connection: {}", peer);
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // first, wait for the client to send his username
    let username: Username = loop {
        let msg = ws_receiver
            .next()
            .await
            .expect("No username message received")?;
        if let tungstenite::Message::Text(username) = msg {
            break username.into();
        }
    };

    let (session_msg_send, mut session_msg_recv) = tokio::sync::mpsc::channel(1);
    let (session_close_send, mut session_close_recv) = tokio::sync::mpsc::channel(1);

    // then, create a session and send that session to the server's main thread
    let session = UserSession::new(username.clone(), session_msg_send, session_close_send);
    srv_event_send
        .send(ServerEvent::UserJoined(session))
        .await?;

    // TODO look at stream forwarding for this...
    // asynchronously read messages that the main server thread wants
    // to send to this client and forward them to the WS client
    let send_thread = tokio::spawn(async move {
        loop {
            tokio::select! {
                maybe_msg = session_msg_recv.recv() => match maybe_msg {
                    Some(msg) => {
                        let msg = serde_json::to_string(&msg).expect("Could not serialize msg");
                        let result = ws_sender.send(tungstenite::Message::Text(msg)).await;
                        if let Err(_) = result {
                            break result;
                        }
                    }
                    // if the msg received is None, all senders have been closed, so we can finish
                    None => {
                        ws_sender.send(tungstenite::Message::Close(None)).await?;
                        break Ok(());
                    }
                },
                _ = session_close_recv.recv() => {
                    ws_sender.send(tungstenite::Message::Close(None)).await?;
                    break Ok(());
                }
            }
        }
    });

    // TODO look at stream forwarding for this
    // forward other events to the main server thread
    loop {
        let delay = Delay::new(Duration::from_millis(500));
        tokio::select! {
            // every 100ms, send a tick event to the main server thread
            _ = delay => srv_event_send.send(ServerEvent::Tick).await?,

            // Websocket messages from the client
            msg = ws_receiver.next() => match msg {
                Some(Ok(tungstenite::Message::Text(msg))) => match serde_json::from_str(&msg) {
                    Ok(Some(msg)) => {
                        srv_event_send
                            .send(ServerEvent::ToServerMsg(username.clone(), msg))
                            .await?;
                    }
                    Ok(None) => {
                        break;
                    }
                    Err(err) => {
                        eprintln!("{} (msg was: {})", err, msg);
                    }
                },
                Some(Ok(tungstenite::Message::Close(_))) | Some(Err(_)) | None => break,
                _ => {}
            }
        }
    }

    drop(send_thread);
    srv_event_send.send(ServerEvent::UserLeft(username)).await?;
    Ok(())
}

pub fn read_words_file(path: &PathBuf) -> Result<Vec<String>> {
    let mut file = std::fs::File::open(path)?;
    let mut words = String::new();
    file.read_to_string(&mut words)?;
    Ok(words
        .lines()
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect::<Vec<String>>())
}
