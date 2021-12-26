use crate::code::{Code, ParseCode};
use crate::error::InboundError;
use crate::event::Event;
use crate::io::EslCodec;
use futures::SinkExt;
use log::debug;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{atomic::AtomicBool, Arc};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpStream, ToSocketAddrs};
use tokio::sync::{
    oneshot::{channel, Sender},
    Mutex,
};
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, FramedWrite};
#[derive(Debug)]
pub struct Inbound {
    password: String,
    commands: Arc<Mutex<VecDeque<Sender<Event>>>>,
    transport_tx: Arc<Mutex<FramedWrite<OwnedWriteHalf, EslCodec>>>,
    background_jobs: Arc<Mutex<HashMap<String, Sender<Event>>>>,
    connected: AtomicBool,
}

impl Inbound {
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
    pub async fn send(&self, item: &[u8]) -> Result<(), InboundError> {
        let mut transport = self.transport_tx.lock().await;
        transport
            .send(item)
            .await
            .map_err(|_error| InboundError::InternalError("unable to send item".to_string()))
    }
    pub async fn send_recv(&self, item: &[u8]) -> Result<Event, InboundError> {
        self.send(item).await?;
        let (tx, rx) = channel();
        self.commands.lock().await.push_back(tx);
        rx.await.map_err(|_error| {
            InboundError::InternalError("Unable to receive send_recv event from channel".into())
        })
    }

    pub async fn with_tcpstream(
        stream: TcpStream,
        password: impl ToString,
    ) -> Result<Self, InboundError> {
        // let sender = Arc::new(sender);
        let commands = Arc::new(Mutex::new(VecDeque::new()));
        let inner_commands = Arc::clone(&commands);
        let background_jobs = Arc::new(Mutex::new(HashMap::new()));
        let inner_background_jobs = Arc::clone(&background_jobs);
        let my_coded = EslCodec {};
        let (read_half, write_half) = stream.into_split();
        let mut transport_rx = FramedRead::new(read_half, my_coded.clone());
        let transport_tx = Arc::new(Mutex::new(FramedWrite::new(write_half, my_coded.clone())));
        transport_rx.next().await;
        let connection = Self {
            password: password.to_string(),
            commands,
            background_jobs,
            transport_tx,
            connected: AtomicBool::new(false),
        };
        tokio::spawn(async move {
            loop {
                if let Some(Ok(event)) = transport_rx.next().await {
                    if let Some(types) = event.headers.get("Content-Type") {
                        if types == "text/event-json" {
                            let data = event.body().expect("Unable to get body of event-json");

                            let my_hash_map: HashMap<String, String> =
                                parse_json_body(data).expect("Unable to parse body of event-json");
                            let job_uuid = my_hash_map.get("Job-UUID");
                            if let Some(job_uuid) = job_uuid {
                                if let Some(tx) =
                                    inner_background_jobs.lock().await.remove(job_uuid)
                                {
                                    let _ = tx
                                        .send(event)
                                        .expect("Unable to send channel message from bgapi");
                                }
                                debug!("continued");
                            }
                            continue;
                        }
                    }
                    if let Some(tx) = inner_commands.lock().await.pop_front() {
                        let _ = tx.send(event).expect("msg");
                    }
                }
            }
        });
        let auth_response = connection.auth().await?;
        debug!("auth_response {:?}", auth_response);
        connection
            .subscribe(vec!["BACKGROUND_JOB", "CHANNEL_EXECUTE_COMPLETE"])
            .await?;
        Ok(connection)
    }
    pub async fn subscribe(&self, events: Vec<&str>) -> Result<Event, InboundError> {
        let message = String::from(format!("event json {}", events.join(" ")));
        self.send_recv(message.as_bytes()).await
    }
    pub async fn new(
        socket: impl ToSocketAddrs,
        password: impl ToString,
    ) -> Result<Self, InboundError> {
        let stream = TcpStream::connect(socket)
            .await
            .map_err(|error| InboundError::ConnectionError(error.to_string()))?;
        Self::with_tcpstream(stream, password).await
    }
    pub async fn auth(&self) -> Result<String, InboundError> {
        let auth_response = self
            .send_recv(format!("auth {}", self.password).as_bytes())
            .await?;
        let auth_headers = auth_response.headers();
        let reply_text = auth_headers
            .get("Reply-Text")
            .ok_or(InboundError::InternalError(
                "Reply-Text in auth request was not found".into(),
            ))?;
        let space_index =
            reply_text
                .find(char::is_whitespace)
                .ok_or(InboundError::InternalError(
                    "Unable to find space index".into(),
                ))?;
        let code = &reply_text[..space_index];
        let code = code.parse_code()?;
        let text_start = space_index + 1;
        let text = reply_text[text_start..].to_string();
        match code {
            Code::Ok => {
                self.connected.store(true, Ordering::Relaxed);
                Ok(text)
            }
            Code::Err => Err(InboundError::AuthFailed),
            Code::Unknown => Err(InboundError::InternalError(
                "Got unknown code in auth request".into(),
            )),
        }
    }
    pub async fn api(&self, command: &str) -> Result<String, InboundError> {
        let response = self.send_recv(format!("api {}", command).as_bytes()).await;
        if let Ok(event) = response {
            let body = event.body.ok_or(InboundError::InternalError(
                "Didnt get body in api response".into(),
            ))?;

            let space_index = body
                .find(char::is_whitespace)
                .ok_or(InboundError::InternalError(
                    "Unable to find space index".into(),
                ))?;
            let code = &body[..space_index];
            let code = code.parse_code()?;
            let text_start = space_index + 1;
            let body_length = body.len();
            let text = body[text_start..(body_length - 1)].to_string();
            match code {
                Code::Ok => Ok(text),
                Code::Err => Err(InboundError::ApiError(text)),
                Code::Unknown => Ok(body.to_string()),
            }
        } else {
            panic!("Unable to receive event for api")
        }
    }
    pub async fn bgapi(&self, command: &str) -> Result<String, InboundError> {
        debug!("Send bgapi {}", command);
        let job_uuid = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = channel();
        self.background_jobs
            .lock()
            .await
            .insert(job_uuid.clone(), tx);

        self.send_recv(format!("bgapi {}\nJob-UUID: {}", command, job_uuid).as_bytes())
            .await?;

        if let Ok(resp) = rx.await {
            let body = resp.body().ok_or(InboundError::InternalError(
                "body was not found in event/json".into(),
            ))?;

            let body_hashmap = parse_json_body(body)?;

            let mut hsmp = resp.headers();
            hsmp.extend(body_hashmap);
            let body = hsmp.get("_body").ok_or(InboundError::InternalError(
                "body was not found in event/json".into(),
            ))?;
            let (code, text) = parse_api_response(body)?;
            match code {
                Code::Ok => Ok(text),
                Code::Err => Err(InboundError::ApiError(text)),
                Code::Unknown => Ok(body.to_string()),
            }
        } else {
            Err(InboundError::InternalError("Unable to get event".into()))
        }
    }
}
fn parse_api_response<'a>(body: &'a str) -> Result<(Code, String), InboundError> {
    let space_index = body
        .find(char::is_whitespace)
        .ok_or(InboundError::InternalError(
            "Unable to find space index".into(),
        ))?;
    let code = &body[..space_index];
    let text_start = space_index + 1;
    let body_length = body.len();
    let text = body[text_start..(body_length - 1)].to_string();
    let code = code.parse_code()?;
    Ok((code, text))
}
fn parse_json_body(body: String) -> Result<HashMap<String, String>, InboundError> {
    serde_json::from_str(&body)
        .map_err(|_| InboundError::InternalError("Unable to parse json event".into()))
}
