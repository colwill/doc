//! The DNS server itself: UDP and TCP on the address in the settings, answering from the table
//! for DOC's domains and relaying every other question, unchanged, to the first upstream server
//! that answers it. It runs inside the plugin's process from `load` to `unload`, so it keeps
//! answering from the last records it read while the backend is away.
//!
//! Nothing here waits on the backend: the records are read into a new table every little while,
//! and at once when they change here, and questions are answered from whichever table is current.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use doc_plugin_sdk::Backend;
use hickory_proto::op::{Edns, Message, MessageType, OpCode, ResponseCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Mutex, Notify, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout, timeout_at};
use tokio_util::sync::CancellationToken;

use crate::settings::Serving;
use crate::store::Store;
use crate::zone::{self, Decision, Table};

/// How often the records are read again, for changes made by another copy of the plugin.
const REFRESH_EVERY: Duration = Duration::from_secs(30);
/// How often a taken address is tried again, such as while the version before hands over.
const RETRY_BIND: Duration = Duration::from_secs(2);
/// How long each upstream server has to answer before the next is asked.
const UPSTREAM_UDP: Duration = Duration::from_secs(2);
const UPSTREAM_TCP: Duration = Duration::from_secs(4);
/// How long a TCP connection may sit between questions.
const TCP_IDLE: Duration = Duration::from_secs(10);
/// Questions being worked on at once; beyond these, UDP questions are dropped, as a busy server's
/// are, rather than queued without end.
const MAX_IN_FLIGHT: usize = 512;
const MAX_TCP_CONNECTIONS: usize = 128;
/// RFC 9715's advice for UDP answers: larger ones fragment, and fragments go missing.
const MAX_UDP_ANSWER: u16 = 1_232;

#[derive(Debug, Clone)]
pub enum Status {
    /// Neither way of answering is on, so nothing is bound and nothing is read.
    Off,
    /// Answering over HTTPS only: no port is bound, and the records are still kept fresh.
    OverHttpsOnly,
    /// The address could not be had, and is being tried again.
    Waiting {
        listen: SocketAddr,
        problem: String,
    },
    Answering {
        listen: SocketAddr,
        since: DateTime<Utc>,
    },
}

#[derive(Default)]
pub struct Counts {
    pub answered: AtomicU64,
    pub forwarded: AtomicU64,
    pub refused: AtomicU64,
    pub failed: AtomicU64,
    pub dropped: AtomicU64,
    /// Of the questions above, those that came over HTTPS rather than a DNS port.
    pub over_https: AtomicU64,
}

/// What the page and the API show about the server.
pub struct Snapshot {
    pub status: Status,
    pub serving: Serving,
    /// Whether the records have been read since the server started.
    pub ready: bool,
    pub names: usize,
    pub unserved: usize,
    pub problem: Option<String>,
    pub answered: u64,
    pub forwarded: u64,
    pub refused: u64,
    pub failed: u64,
    pub dropped: u64,
    pub over_https: u64,
    /// The plugin names answered for, in order.
    pub named: Vec<String>,
}

struct Shared {
    table: RwLock<Arc<Table>>,
    status: RwLock<Status>,
    problem: RwLock<Option<String>>,
    counts: Counts,
    refresh: Notify,
}

impl Shared {
    fn table(&self) -> Arc<Table> {
        self.table
            .read()
            .map(|table| table.clone())
            .unwrap_or_else(|held| held.into_inner().clone())
    }

    fn set_status(&self, status: Status) {
        if let Ok(mut held) = self.status.write() {
            *held = status;
        }
    }

    fn set_problem(&self, problem: Option<String>) {
        if let Ok(mut held) = self.problem.write() {
            *held = problem;
        }
    }
}

enum Transport {
    Udp,
    Tcp,
    /// RFC 8484: a question in an HTTP request. Like TCP, it has no 512-byte limit and no
    /// truncation, so an answer goes back whole.
    Https,
}

/// Why a question sent over HTTPS was not answered, in words the route turns into a status.
#[derive(Debug)]
pub enum NotResolved {
    /// The bytes are not a DNS message.
    Malformed,
    /// A stray response or something that is not a question at all.
    NotAQuestion,
    /// The plugin is not answering over HTTPS at all.
    Off,
}

pub struct Server {
    shared: Arc<Shared>,
    running: Mutex<Option<(CancellationToken, JoinHandle<()>)>>,
}

impl Default for Server {
    fn default() -> Self {
        let serving = Serving::read(&doc_plugin_sdk::Settings::default());
        Self {
            shared: Arc::new(Shared {
                table: RwLock::new(Arc::new(Table::unready(serving))),
                status: RwLock::new(Status::Off),
                problem: RwLock::new(None),
                counts: Counts::default(),
                refresh: Notify::new(),
            }),
            running: Mutex::new(None),
        }
    }
}

impl Server {
    /// Starts answering with the settings as they are now, stopping whatever ran before.
    pub async fn start(&self, backend: &Backend) {
        self.stop().await;
        let serving = Serving::read(&backend.settings());
        // Answering over HTTPS needs no port, but it does need the records, so the task runs for
        // either: it binds only when the DNS server itself is on.
        if !serving.answering() {
            tracing::info!("the DNS server is off");
            return;
        }
        if let Ok(mut table) = self.shared.table.write() {
            *table = Arc::new(Table::unready(serving.clone()));
        }
        let stop = CancellationToken::new();
        let task = tokio::spawn(run(self.shared.clone(), backend.clone(), serving, stop.clone()));
        *self.running.lock().await = Some((stop, task));
    }

    /// Stops answering and lets go of the address, waiting a little for that to happen.
    pub async fn stop(&self) {
        if let Some((stop, task)) = self.running.lock().await.take() {
            stop.cancel();
            let _ = timeout(Duration::from_secs(5), task).await;
        }
        self.shared.set_status(Status::Off);
    }

    /// Has the records read again now, after a change made here.
    pub fn refresh(&self) {
        self.shared.refresh.notify_one();
    }

    /// One question over HTTPS, as wire bytes in and wire bytes out (RFC 8484).
    ///
    /// `forwarding` says whether a name outside DOC's domains may be sent upstream for this
    /// caller. Over a DNS port that is decided by the asker's address; over HTTPS the platform
    /// does not pass one on, so it is decided by who they signed in as — which is the stricter
    /// of the two, since an unauthenticated caller is given DOC's own domains and nothing else.
    pub async fn resolve(&self, question: &[u8], forwarding: bool) -> Result<Vec<u8>, NotResolved> {
        let shared = &self.shared;
        if !shared.table().serving().over_https {
            return Err(NotResolved::Off);
        }
        shared.counts.over_https.fetch_add(1, Ordering::Relaxed);
        let counts = &shared.counts;
        let Ok(request) = Message::from_vec(question) else {
            counts.refused.fetch_add(1, Ordering::Relaxed);
            return Err(NotResolved::Malformed);
        };
        let table = shared.table();
        match table.decide(&request) {
            Decision::Ignore => {
                counts.refused.fetch_add(1, Ordering::Relaxed);
                Err(NotResolved::NotAQuestion)
            }
            Decision::Answer(answer) => {
                let counted = match answer.metadata.response_code {
                    ResponseCode::NoError | ResponseCode::NXDomain => &counts.answered,
                    ResponseCode::ServFail => &counts.failed,
                    _ => &counts.refused,
                };
                counted.fetch_add(1, Ordering::Relaxed);
                encode(&request, answer, &Transport::Https).ok_or(NotResolved::Malformed)
            }
            // A name DOC does not answer for, from somebody who may not have it forwarded, or
            // with nowhere to forward it to. RFC 8484 sends a DNS refusal back with 200, the way
            // the DNS port does, rather than an HTTP error: the caller asked correctly, and the
            // answer is that this resolver will not say.
            Decision::Forward if !forwarding || table.serving().upstreams.is_empty() => {
                counts.refused.fetch_add(1, Ordering::Relaxed);
                let refused = zone::failure(&request, ResponseCode::Refused);
                encode(&request, refused, &Transport::Https).ok_or(NotResolved::Malformed)
            }
            Decision::Forward => {
                // Over TCP: an answer here is never truncated, so the caller never has to ask
                // again by another route.
                let upstreams = &table.serving().upstreams;
                match forward_tcp(question, &request, upstreams).await {
                    Some(answer) => {
                        counts.forwarded.fetch_add(1, Ordering::Relaxed);
                        Ok(answer)
                    }
                    None => {
                        counts.failed.fetch_add(1, Ordering::Relaxed);
                        let failed = zone::failure(&request, ResponseCode::ServFail);
                        encode(&request, failed, &Transport::Https).ok_or(NotResolved::Malformed)
                    }
                }
            }
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let table = self.shared.table();
        let counts = &self.shared.counts;
        Snapshot {
            status: self.shared.status.read().map(|status| status.clone()).unwrap_or(Status::Off),
            serving: table.serving().clone(),
            ready: table.ready(),
            names: table.names(),
            unserved: table.unserved,
            problem: self.shared.problem.read().ok().and_then(|problem| problem.clone()),
            answered: counts.answered.load(Ordering::Relaxed),
            forwarded: counts.forwarded.load(Ordering::Relaxed),
            refused: counts.refused.load(Ordering::Relaxed),
            failed: counts.failed.load(Ordering::Relaxed),
            dropped: counts.dropped.load(Ordering::Relaxed),
            over_https: counts.over_https.load(Ordering::Relaxed),
            named: table.named().to_vec(),
        }
    }
}

async fn run(shared: Arc<Shared>, backend: Backend, serving: Serving, stop: CancellationToken) {
    if !serving.on {
        shared.set_status(Status::OverHttpsOnly);
        tracing::info!(zones = ?serving.zones, "answering DNS over HTTPS only; no port is bound");
    }
    tokio::select! {
        () = stop.cancelled() => {}
        () = keep_fresh(&shared, &backend, &serving) => {}
        () = async {
            match serving.on {
                true => listen(&shared, &serving, &stop).await,
                // Nothing to listen on, but the task must not end: `keep_fresh` runs beside it.
                false => std::future::pending().await,
            }
        } => {}
    }
    tracing::info!(listen = %serving.listen, "the DNS server stopped");
}

/// Reads the records into a new table now, then whenever asked to and every little while.
async fn keep_fresh(shared: &Shared, backend: &Backend, serving: &Serving) {
    loop {
        // The plugins are read beside the records, since a name per plugin is worked out from
        // them. A read that fails leaves the names as they were rather than dropping them all.
        let mut plugins = Vec::new();
        match Store(backend).records().await {
            Ok(records) => {
                if serving.plugin_domain.is_some() {
                    match Store(backend).plugins().await {
                        Ok(found) => plugins = found,
                        Err(refusal) => {
                            tracing::warn!(
                                problem = %refusal.detail,
                                "the plugins could not be read for their names"
                            );
                            plugins = shared.table().plugins().to_vec();
                        }
                    }
                }
                let before = shared.table();
                let before = before.ready().then_some(&*before);
                let table = Table::new(serving.clone(), &records, &plugins, before);
                if let Ok(mut held) = shared.table.write() {
                    *held = Arc::new(table);
                }
                shared.set_problem(None);
            }
            // The last table read stays in use; one never read leaves DOC's domains failing.
            Err(refusal) => {
                tracing::warn!(problem = %refusal.detail, "the DNS records could not be read");
                shared.set_problem(Some(refusal.detail));
            }
        }
        tokio::select! {
            () = shared.refresh.notified() => {}
            () = sleep(REFRESH_EVERY) => {}
        }
    }
}

async fn listen(shared: &Arc<Shared>, serving: &Serving, stop: &CancellationToken) {
    let (udp, tcp) = loop {
        match bind(serving.listen).await {
            Ok(bound) => break bound,
            Err(err) => {
                let problem = err.to_string();
                tracing::warn!(listen = %serving.listen, %problem, "the DNS server cannot listen yet");
                shared.set_status(Status::Waiting { listen: serving.listen, problem });
                sleep(RETRY_BIND).await;
            }
        }
    };
    tracing::info!(listen = %serving.listen, zones = ?serving.zones, "the DNS server is answering");
    shared.set_status(Status::Answering { listen: serving.listen, since: Utc::now() });
    let work = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
    tokio::join!(
        answer_udp(shared.clone(), Arc::new(udp), work.clone(), stop.clone()),
        answer_tcp(shared.clone(), tcp, stop.clone()),
    );
}

async fn bind(listen: SocketAddr) -> std::io::Result<(UdpSocket, TcpListener)> {
    let udp = UdpSocket::bind(listen).await?;
    let tcp = TcpListener::bind(listen).await?;
    Ok((udp, tcp))
}

async fn answer_udp(
    shared: Arc<Shared>,
    socket: Arc<UdpSocket>,
    work: Arc<Semaphore>,
    stop: CancellationToken,
) {
    let mut buffer = vec![0u8; 4_096];
    loop {
        let (length, client) = match socket.recv_from(&mut buffer).await {
            Ok(received) => received,
            // Such as a port unreachable for an answer sent earlier: nothing to do with this one.
            Err(err) => {
                tracing::debug!(%err, "a UDP question could not be read");
                tokio::task::yield_now().await;
                continue;
            }
        };
        let Ok(permit) = work.clone().try_acquire_owned() else {
            shared.counts.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let question = buffer[..length].to_vec();
        let (shared, socket, stop) = (shared.clone(), socket.clone(), stop.clone());
        tokio::spawn(async move {
            let _permit = permit;
            tokio::select! {
                () = stop.cancelled() => {}
                () = async {
                    if let Some(answer) = respond(&shared, &question, client, &Transport::Udp).await
                        && let Err(err) = socket.send_to(&answer, client).await
                    {
                        tracing::debug!(%err, %client, "a UDP answer could not be sent");
                    }
                } => {}
            }
        });
    }
}

async fn answer_tcp(shared: Arc<Shared>, listener: TcpListener, stop: CancellationToken) {
    let connections = Arc::new(Semaphore::new(MAX_TCP_CONNECTIONS));
    loop {
        let (stream, client) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                tracing::debug!(%err, "a TCP connection could not be accepted");
                sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = connections.clone().try_acquire_owned() else {
            shared.counts.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let (shared, stop) = (shared.clone(), stop.clone());
        tokio::spawn(async move {
            let _permit = permit;
            tokio::select! {
                () = stop.cancelled() => {}
                () = converse(&shared, stream, client) => {}
            }
        });
    }
}

/// Questions and answers on one TCP connection, each with its two-byte length first, until the
/// client goes quiet or away.
async fn converse(shared: &Shared, mut stream: TcpStream, client: SocketAddr) {
    loop {
        let mut length = [0u8; 2];
        if !matches!(timeout(TCP_IDLE, stream.read_exact(&mut length)).await, Ok(Ok(_))) {
            return;
        }
        let mut question = vec![0u8; usize::from(u16::from_be_bytes(length))];
        if !matches!(timeout(TCP_IDLE, stream.read_exact(&mut question)).await, Ok(Ok(_))) {
            return;
        }
        let Some(answer) = respond(shared, &question, client, &Transport::Tcp).await else {
            continue;
        };
        let Ok(length) = u16::try_from(answer.len()) else { return };
        let mut framed = Vec::with_capacity(answer.len() + 2);
        framed.extend_from_slice(&length.to_be_bytes());
        framed.extend_from_slice(&answer);
        if stream.write_all(&framed).await.is_err() {
            return;
        }
    }
}

/// The answer to one question as it goes back on the wire, or `None` to say nothing.
async fn respond(
    shared: &Shared,
    question: &[u8],
    client: SocketAddr,
    transport: &Transport,
) -> Option<Vec<u8>> {
    let counts = &shared.counts;
    let Ok(request) = Message::from_vec(question) else {
        counts.refused.fetch_add(1, Ordering::Relaxed);
        return malformed(question);
    };
    let table = shared.table();
    match table.decide(&request) {
        Decision::Ignore => None,
        Decision::Answer(answer) => {
            let counted = match answer.metadata.response_code {
                ResponseCode::NoError | ResponseCode::NXDomain => &counts.answered,
                ResponseCode::ServFail => &counts.failed,
                _ => &counts.refused,
            };
            counted.fetch_add(1, Ordering::Relaxed);
            encode(&request, answer, transport)
        }
        Decision::Forward => {
            let serving = table.serving();
            if !serving.forwards_for(client.ip()) {
                counts.refused.fetch_add(1, Ordering::Relaxed);
                return encode(&request, zone::failure(&request, ResponseCode::Refused), transport);
            }
            let relayed = match transport {
                Transport::Udp => forward_udp(question, &request, &serving.upstreams).await,
                // A question over HTTPS never reaches here: `resolve` forwards it itself.
                Transport::Tcp | Transport::Https => {
                    forward_tcp(question, &request, &serving.upstreams).await
                }
            };
            match relayed {
                Some(answer) => {
                    counts.forwarded.fetch_add(1, Ordering::Relaxed);
                    Some(answer)
                }
                None => {
                    counts.failed.fetch_add(1, Ordering::Relaxed);
                    encode(&request, zone::failure(&request, ResponseCode::ServFail), transport)
                }
            }
        }
    }
}

/// FORMERR for a question too broken to read, when its header at least says it is a question.
fn malformed(question: &[u8]) -> Option<Vec<u8>> {
    let header = question.get(..12)?;
    if header[2] & 0x80 != 0 {
        return None;
    }
    let id = u16::from_be_bytes([header[0], header[1]]);
    Message::error_msg(id, OpCode::Query, ResponseCode::FormErr).to_vec().ok()
}

/// An answer on the wire: with EDNS when the question had it, and cut down to the header and the
/// question, with TC set, when it is too long for UDP, so the asker comes back over TCP.
fn encode(request: &Message, mut answer: Message, transport: &Transport) -> Option<Vec<u8>> {
    if request.edns.is_some() {
        let mut edns = Edns::new();
        edns.set_max_payload(MAX_UDP_ANSWER);
        answer.set_edns(edns);
    }
    let bytes = match answer.to_vec() {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(%err, "an answer could not be written");
            return zone::failure(request, ResponseCode::ServFail).to_vec().ok();
        }
    };
    let limit = usize::from(request.max_payload().min(MAX_UDP_ANSWER));
    match transport {
        Transport::Udp if bytes.len() > limit => answer.truncate().to_vec().ok(),
        _ => Some(bytes),
    }
}

/// Whether `bytes` is the answer to `request`: its ID, flagged as a response, to the same question.
fn answers(request: &Message, bytes: &[u8]) -> bool {
    Message::from_vec(bytes).is_ok_and(|answer| {
        answer.metadata.id == request.metadata.id
            && answer.metadata.message_type == MessageType::Response
            && answer.queries == request.queries
    })
}

/// Relays the question as it came, from a new socket on a random port for each, to each upstream
/// in turn until one answers it.
async fn forward_udp(
    question: &[u8],
    request: &Message,
    upstreams: &[SocketAddr],
) -> Option<Vec<u8>> {
    let mut buffer = vec![0u8; 65_535];
    for upstream in upstreams {
        let local: SocketAddr = match upstream {
            SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
            SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
        };
        let Ok(socket) = UdpSocket::bind(local).await else { continue };
        if socket.connect(upstream).await.is_err() || socket.send(question).await.is_err() {
            continue;
        }
        let deadline = Instant::now() + UPSTREAM_UDP;
        // Anything that is not the answer, such as a forged one, is passed over.
        while let Ok(Ok(length)) = timeout_at(deadline, socket.recv(&mut buffer)).await {
            if answers(request, &buffer[..length]) {
                return Some(buffer[..length].to_vec());
            }
        }
        tracing::debug!(%upstream, "an upstream DNS server did not answer in time");
    }
    None
}

async fn forward_tcp(
    question: &[u8],
    request: &Message,
    upstreams: &[SocketAddr],
) -> Option<Vec<u8>> {
    let length = u16::try_from(question.len()).ok()?;
    for upstream in upstreams {
        let asked = async {
            let mut stream = TcpStream::connect(upstream).await.ok()?;
            let mut framed = Vec::with_capacity(question.len() + 2);
            framed.extend_from_slice(&length.to_be_bytes());
            framed.extend_from_slice(question);
            stream.write_all(&framed).await.ok()?;
            let mut size = [0u8; 2];
            stream.read_exact(&mut size).await.ok()?;
            let mut answer = vec![0u8; usize::from(u16::from_be_bytes(size))];
            stream.read_exact(&mut answer).await.ok()?;
            answers(request, &answer).then_some(answer)
        };
        match timeout(UPSTREAM_TCP, asked).await {
            Ok(Some(answer)) => return Some(answer),
            _ => tracing::debug!(%upstream, "an upstream DNS server did not answer over TCP"),
        }
    }
    None
}
