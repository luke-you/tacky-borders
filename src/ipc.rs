use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::cell::Cell;
use std::collections::{HashSet, VecDeque};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows::Win32::Networking::WinSock::{WSAECONNABORTED, WSAECONNRESET};

use crate::APP_STATE;
use crate::border_registry::{BorderRegistry, TrackingWindowFingerprint};
use crate::border_updates::{AppearanceSequencer, IpcUpdate};
use crate::colors::ColorBrushConfig;
use crate::command_context::CommandContext;
use crate::config::{
    Config, OffsetConfig, RadiusConfig, ReloadOutcome, RuntimeConfig, WidthConfig,
};
use crate::iocp::{SocketId, UnixListener, UnixStream, shutdown_socket};
use crate::ipc_security::restrict_socket_path;
use crate::utils::{LogIfErr, get_foreground_window, remove_file_if_exists};

const IPC_WORKER_COUNT: usize = 4;
const IPC_QUEUE_CAPACITY: usize = 4;
const IPC_ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const IPC_SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(50);
const IPC_OVERLOAD_LOG_INTERVAL: Duration = Duration::from_secs(5);
const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_COMMANDS_PER_CONNECTION: usize = 32;
const IPC_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const IPC_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
pub const IPC_CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);
pub const CLI_COMMAND_TIMEOUT: Duration =
    IPC_CONNECTION_TIMEOUT.saturating_add(Duration::from_secs(5));
const IPC_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
struct IpcLimits {
    max_command_bytes: usize,
    max_response_bytes: usize,
    max_commands_per_connection: usize,
    idle_timeout: Duration,
    command_timeout: Duration,
    connection_timeout: Duration,
    write_timeout: Duration,
}

impl Default for IpcLimits {
    fn default() -> Self {
        Self {
            max_command_bytes: MAX_COMMAND_BYTES,
            max_response_bytes: MAX_RESPONSE_BYTES,
            max_commands_per_connection: MAX_COMMANDS_PER_CONNECTION,
            idle_timeout: IPC_IDLE_TIMEOUT,
            command_timeout: IPC_COMMAND_TIMEOUT,
            connection_timeout: IPC_CONNECTION_TIMEOUT,
            write_timeout: IPC_WRITE_TIMEOUT,
        }
    }
}

pub fn socket_path() -> anyhow::Result<PathBuf> {
    Config::get_dir().map(|dir| dir.join("tacky-borders.sock"))
}

pub fn send_command_to(path: &Path, command: &str, timeout: Duration) -> anyhow::Result<String> {
    anyhow::ensure!(
        command.len() <= MAX_COMMAND_BYTES,
        "ipc command exceeds the byte limit"
    );
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("CLI deadline overflow")?;
    let stream = UnixStream::connect_timeout(path, timeout).with_context(|| {
        format!(
            "could not connect to {}; is the ipc server enabled?",
            path.display()
        )
    })?;
    write_response(
        &stream,
        command.to_owned(),
        IpcLimits {
            max_response_bytes: MAX_COMMAND_BYTES,
            write_timeout: timeout,
            ..IpcLimits::default()
        },
        deadline,
        &AtomicBool::new(false),
    )
    .context("could not send command")?;
    let mut reader = BufReader::new(&stream);
    let response = read_bounded_command(
        &mut reader,
        MAX_RESPONSE_BYTES,
        || {
            let duration = remaining(deadline, "CLI response")?;
            stream
                .set_read_timeout(duration.min(IPC_SOCKET_POLL_INTERVAL))
                .map_err(CommandReadError::Io)
        },
        || {},
    )
    .map_err(|error| match error {
        CommandReadError::Deadline(_) => anyhow::anyhow!("timed out waiting for IPC response"),
        other => anyhow::Error::new(other).context("could not read IPC response"),
    })?;
    match response {
        CommandRead::Command(bytes) => {
            String::from_utf8(bytes).context("IPC response is not valid UTF-8")
        }
        CommandRead::Eof => anyhow::bail!("IPC server closed without a response"),
    }
}

/// IPC Server that handles communication between a CLI and daemon.
/// Changes made via IPC are not written back to the config file.
pub struct IpcServer {
    socket_path: PathBuf,
    stop: Arc<AtomicBool>,
    queue: Arc<ClientQueue<UnixStream>>,
    active: Arc<ActiveClients>,
    rejected: Arc<AtomicU64>,
    acceptor: Option<JoinHandle<()>>,
    workers: Vec<JoinHandle<()>>,
}

impl IpcServer {
    pub fn new(socket_path: &Path) -> anyhow::Result<Self> {
        // Remove a stale socket file left over from a previous run; bind fails otherwise
        remove_file_if_exists(socket_path).context("could not remove stale ipc socket")?;

        let listener = UnixListener::bind(socket_path).context("could not bind ipc socket")?;
        listener
            .set_nonblocking(true)
            .context("could not make ipc listener nonblocking")?;
        let socket_path_restricted = match restrict_socket_path(socket_path) {
            Ok(()) => true,
            Err(error) => {
                warn!(
                    "could not restrict the ipc socket path; continuing with protocol limits: {error:#}"
                );
                false
            }
        };

        let stop = Arc::new(AtomicBool::new(false));
        let queue = Arc::new(ClientQueue::new(IPC_QUEUE_CAPACITY));
        let active = Arc::new(ActiveClients::default());
        let rejected = Arc::new(AtomicU64::new(0));
        let mut workers = Vec::with_capacity(IPC_WORKER_COUNT);

        for index in 0..IPC_WORKER_COUNT {
            let worker_queue = queue.clone();
            let worker_active = active.clone();
            let worker_stop = stop.clone();
            match thread::Builder::new()
                .name(format!("ipc-worker-{index}"))
                .spawn(move || run_worker(worker_queue, worker_active, worker_stop))
            {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    stop.store(true, Ordering::Release);
                    queue.close_and_drain();
                    join_threads(&mut workers, "ipc worker");
                    drop(listener);
                    remove_file_if_exists(socket_path)
                        .context("could not remove ipc socket after startup failure")
                        .log_if_err();
                    return Err(error).context("could not spawn ipc worker");
                }
            }
        }

        let acceptor_queue = queue.clone();
        let acceptor_stop = stop.clone();
        let acceptor_rejected = rejected.clone();
        let acceptor = match thread::Builder::new()
            .name("ipc-acceptor".into())
            .spawn(move || run_acceptor(listener, acceptor_queue, acceptor_stop, acceptor_rejected))
        {
            Ok(acceptor) => acceptor,
            Err(error) => {
                stop.store(true, Ordering::Release);
                queue.close_and_drain();
                active.shutdown_all();
                join_threads(&mut workers, "ipc worker");
                remove_file_if_exists(socket_path)
                    .context("could not remove ipc socket after startup failure")
                    .log_if_err();
                return Err(error).context("could not spawn ipc acceptor");
            }
        };

        info!(
            "ipc server listening on {} (socket path restricted: {socket_path_restricted})",
            socket_path.display()
        );

        Ok(Self {
            socket_path: socket_path.to_owned(),
            stop,
            queue,
            active,
            rejected,
            acceptor: Some(acceptor),
            workers,
        })
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);

        if let Some(acceptor) = self.acceptor.take() {
            acceptor.thread().unpark();
            if let Err(error) = acceptor.join() {
                error!("could not join ipc acceptor: {error:?}");
            }
        } else {
            error!("could not take ipc acceptor thread handle");
        }

        self.queue.close_and_drain();
        self.active.shutdown_all();
        join_threads(&mut self.workers, "ipc worker");
        let rejected = self.rejected.swap(0, Ordering::Relaxed);
        if rejected != 0 {
            debug!("rejected {rejected} ipc clients before shutdown");
        }

        remove_file_if_exists(&self.socket_path)
            .context("could not remove ipc socket")
            .log_if_err();

        debug!("ipc server stopped");
    }
}

enum PushResult<T> {
    Queued,
    Full(T),
    Closed(T),
}

struct ClientQueueState<T> {
    closed: bool,
    clients: VecDeque<T>,
}

struct ClientQueue<T> {
    capacity: usize,
    state: Mutex<ClientQueueState<T>>,
    available: Condvar,
}

impl<T> ClientQueue<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(ClientQueueState {
                closed: false,
                clients: VecDeque::with_capacity(capacity),
            }),
            available: Condvar::new(),
        }
    }

    fn try_push(&self, client: T) -> PushResult<T> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.closed {
            return PushResult::Closed(client);
        }
        if state.clients.len() == self.capacity {
            return PushResult::Full(client);
        }

        state.clients.push_back(client);
        self.available.notify_one();
        PushResult::Queued
    }

    fn pop(&self) -> Option<T> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        loop {
            if let Some(client) = state.clients.pop_front() {
                return Some(client);
            }
            if state.closed {
                return None;
            }
            state = self
                .available
                .wait(state)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }

    fn close_and_drain(&self) {
        let clients = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            state.closed = true;
            self.available.notify_all();
            state.clients.drain(..).collect::<Vec<_>>()
        };
        drop(clients);
    }
}

#[derive(Default)]
struct ActiveClients {
    state: Mutex<ActiveClientsState>,
}

#[derive(Default)]
struct ActiveClientsState {
    stopping: bool,
    sockets: HashSet<SocketId>,
}

impl ActiveClients {
    fn register(self: &Arc<Self>, stream: UnixStream) -> Result<ActiveClient, UnixStream> {
        let socket_id = stream.socket_id();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.stopping {
            return Err(stream);
        }
        state.sockets.insert(socket_id);
        drop(state);

        Ok(ActiveClient {
            _registration: ActiveClientRegistration {
                clients: self.clone(),
                socket_id,
            },
            stream,
        })
    }

    fn shutdown_all(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.stopping = true;
        for socket_id in state.sockets.iter().copied() {
            shutdown_socket(socket_id)
                .context("could not shut down active ipc client")
                .log_if_err();
        }
    }
}

struct ActiveClientRegistration {
    clients: Arc<ActiveClients>,
    socket_id: SocketId,
}

impl Drop for ActiveClientRegistration {
    fn drop(&mut self) {
        self.clients
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .sockets
            .remove(&self.socket_id);
    }
}

struct ActiveClient {
    _registration: ActiveClientRegistration,
    stream: UnixStream,
}

fn run_worker(
    queue: Arc<ClientQueue<UnixStream>>,
    active: Arc<ActiveClients>,
    stop: Arc<AtomicBool>,
) {
    run_worker_with(queue, active, stop, handle_client)
}

fn run_worker_with(
    queue: Arc<ClientQueue<UnixStream>>,
    active: Arc<ActiveClients>,
    stop: Arc<AtomicBool>,
    mut handle: impl FnMut(&UnixStream, &Arc<AtomicBool>) -> anyhow::Result<()>,
) {
    while let Some(stream) = queue.pop() {
        if stop.load(Ordering::Acquire) {
            break;
        }

        let Ok(client) = active.register(stream) else {
            break;
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle(&client.stream, &stop)
        }));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => debug!("ipc client disconnected: {error:#}"),
            Err(error) => error!("ipc client handler panicked: {error:?}"),
        }
    }
}

fn run_acceptor(
    listener: UnixListener,
    queue: Arc<ClientQueue<UnixStream>>,
    stop: Arc<AtomicBool>,
    rejected: Arc<AtomicU64>,
) {
    debug!("entering ipc acceptor thread");
    let mut last_overload_log = Instant::now();
    let mut transient_errors = 0u64;

    run_accept_loop(
        &stop,
        || listener.accept(),
        |stream| {
            if let Err(error) = stream.set_nonblocking(false) {
                error!("could not make accepted ipc client blocking: {error}");
                return;
            }
            match queue.try_push(stream) {
                PushResult::Queued => {}
                PushResult::Full(_) | PushResult::Closed(_) => {
                    rejected.fetch_add(1, Ordering::Relaxed);
                }
            }
        },
        |kind, error| {
            if kind == AcceptErrorKind::TransientConnection {
                transient_errors = transient_errors.saturating_add(1);
            }
            if last_overload_log.elapsed() >= IPC_OVERLOAD_LOG_INTERVAL {
                let rejected_count = rejected.swap(0, Ordering::Relaxed);
                if rejected_count != 0 {
                    warn!("rejected {rejected_count} ipc clients while the worker queue was full");
                }
                if transient_errors != 0 {
                    warn!(
                        "ignored {transient_errors} transient ipc accept errors; latest error: {error}"
                    );
                }
                transient_errors = 0;
                last_overload_log = Instant::now();
            }
        },
        |error| {
            if !stop.load(Ordering::Acquire) {
                error!("could not accept ipc client: {error}");
            }
        },
        thread::park_timeout,
    );

    debug!("exiting ipc acceptor thread");
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AcceptErrorKind {
    WouldBlock,
    TransientConnection,
    Fatal,
}

fn classify_accept_error(error: &std::io::Error) -> AcceptErrorKind {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return AcceptErrorKind::WouldBlock;
    }

    match error.raw_os_error() {
        Some(code) if code == WSAECONNABORTED.0 || code == WSAECONNRESET.0 => {
            AcceptErrorKind::TransientConnection
        }
        _ => AcceptErrorKind::Fatal,
    }
}

fn run_accept_loop<T>(
    stop: &AtomicBool,
    mut accept: impl FnMut() -> std::io::Result<T>,
    mut on_accept: impl FnMut(T),
    mut on_retry: impl FnMut(AcceptErrorKind, &std::io::Error),
    mut on_fatal: impl FnMut(std::io::Error),
    mut wait: impl FnMut(Duration),
) {
    while !stop.load(Ordering::Acquire) {
        match accept() {
            Ok(value) => {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                on_accept(value);
            }
            Err(error) => match classify_accept_error(&error) {
                kind @ (AcceptErrorKind::WouldBlock | AcceptErrorKind::TransientConnection) => {
                    on_retry(kind, &error);
                    wait(IPC_ACCEPT_POLL_INTERVAL);
                }
                AcceptErrorKind::Fatal => {
                    on_fatal(error);
                    break;
                }
            },
        }
    }
}

fn join_threads(handles: &mut Vec<JoinHandle<()>>, description: &str) {
    for handle in handles.drain(..) {
        if let Err(error) = handle.join() {
            error!("could not join {description}: {error:?}");
        }
    }
}

#[cfg(test)]
pub(crate) fn handle_test_client(stream: UnixStream) -> anyhow::Result<()> {
    handle_client(&stream, &Arc::new(AtomicBool::new(false)))
}

enum CommandRead {
    Command(Vec<u8>),
    Eof,
}

#[derive(Debug)]
enum CommandReadError {
    Io(io::Error),
    TooLarge,
    Deadline(&'static str),
}

impl std::fmt::Display for CommandReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "could not read ipc command: {error}"),
            Self::TooLarge => formatter.write_str("ipc command exceeds the byte limit"),
            Self::Deadline(name) => write!(formatter, "ipc {name} deadline exceeded"),
        }
    }
}

impl std::error::Error for CommandReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::TooLarge | Self::Deadline(_) => None,
        }
    }
}

fn read_bounded_command<R, F, P>(
    reader: &mut R,
    max_bytes: usize,
    mut before_read: F,
    mut after_progress: P,
) -> Result<CommandRead, CommandReadError>
where
    R: BufRead,
    F: FnMut() -> Result<(), CommandReadError>,
    P: FnMut(),
{
    let mut command = Vec::with_capacity(max_bytes.min(1024));

    loop {
        before_read()?;
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if is_socket_timeout(&error) => continue,
            Err(error) => return Err(CommandReadError::Io(error)),
        };
        if available.is_empty() {
            return if command.is_empty() {
                Ok(CommandRead::Eof)
            } else {
                Ok(CommandRead::Command(command))
            };
        }

        let newline = available.iter().position(|byte| *byte == b'\n');
        let segment_len = newline.unwrap_or(available.len());
        let new_len = command
            .len()
            .checked_add(segment_len)
            .ok_or(CommandReadError::TooLarge)?;
        if new_len > max_bytes {
            return Err(CommandReadError::TooLarge);
        }
        command.extend_from_slice(&available[..segment_len]);
        if segment_len != 0 {
            after_progress();
        }
        reader.consume(segment_len + usize::from(newline.is_some()));

        if newline.is_some() {
            return Ok(CommandRead::Command(command));
        }
    }
}

fn is_socket_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) || matches!(error.raw_os_error(), Some(10035 | 10060))
}

fn remaining(deadline: Instant, name: &'static str) -> Result<Duration, CommandReadError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(CommandReadError::Deadline(name))
}

fn handle_client(stream: &UnixStream, stop: &Arc<AtomicBool>) -> anyhow::Result<()> {
    handle_client_with_limits(stream, IpcLimits::default(), stop)
}

fn handle_client_with_limits(
    stream: &UnixStream,
    limits: IpcLimits,
    stop: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    handle_client_with_processor(stream, limits, stop, process_command)
}

fn handle_client_with_processor(
    stream: &UnixStream,
    limits: IpcLimits,
    stop: &Arc<AtomicBool>,
    mut process: impl FnMut(&str, &CommandContext) -> CommandResponse,
) -> anyhow::Result<()> {
    let connection_deadline = Instant::now()
        .checked_add(limits.connection_timeout)
        .context("ipc connection deadline overflow")?;
    let mut reader = BufReader::with_capacity(8 * 1024, stream);

    for _ in 0..limits.max_commands_per_connection {
        let idle_deadline = Cell::new(
            Instant::now()
                .checked_add(limits.idle_timeout)
                .context("ipc idle deadline overflow")?,
        );
        let command_deadline = Instant::now()
            .checked_add(limits.command_timeout)
            .context("ipc command deadline overflow")?;
        let command = read_bounded_command(
            &mut reader,
            limits.max_command_bytes,
            || {
                let timeout = IPC_SOCKET_POLL_INTERVAL
                    .min(remaining(idle_deadline.get(), "idle")?)
                    .min(remaining(command_deadline, "command")?)
                    .min(remaining(connection_deadline, "connection")?);
                stream
                    .set_read_timeout(timeout)
                    .map_err(CommandReadError::Io)
            },
            || {
                idle_deadline.set(
                    Instant::now()
                        .checked_add(limits.idle_timeout)
                        .unwrap_or_else(Instant::now),
                );
            },
        );

        let command = match command {
            Ok(CommandRead::Command(command)) => command,
            Ok(CommandRead::Eof) => return Ok(()),
            Err(CommandReadError::TooLarge) => {
                write_protocol_error(
                    stream,
                    "command byte limit exceeded",
                    limits,
                    connection_deadline,
                    stop,
                )
                .log_if_err();
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };

        let command = match std::str::from_utf8(&command) {
            Ok(command) => command,
            Err(_) => {
                write_protocol_error(
                    stream,
                    "command is not valid UTF-8",
                    limits,
                    connection_deadline,
                    stop,
                )
                .log_if_err();
                return Ok(());
            }
        };
        let trimmed = command.trim();
        if trimmed.is_empty() {
            continue;
        }

        remaining(connection_deadline, "connection")?;
        let context = CommandContext::new(connection_deadline, stop.clone());
        let response = process(trimmed, &context);
        let write_result = write_response(stream, response.body, limits, connection_deadline, stop);
        if let Some(acknowledgement) = response.acknowledgement {
            let _ = acknowledgement.send(());
        }
        write_result?;
    }

    write_protocol_error(
        stream,
        "command count limit exceeded",
        limits,
        connection_deadline,
        stop,
    )
    .log_if_err();
    Ok(())
}

fn write_protocol_error(
    stream: &UnixStream,
    message: &str,
    limits: IpcLimits,
    connection_deadline: Instant,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    write_response(
        stream,
        json!({"ok": false, "error": message}).to_string(),
        limits,
        connection_deadline,
        stop,
    )
}

trait ResponseWriter {
    fn set_write_timeout(&self, timeout: Duration) -> io::Result<()>;
    fn write_bytes(&self, bytes: &[u8]) -> io::Result<usize>;
}

impl ResponseWriter for UnixStream {
    fn set_write_timeout(&self, timeout: Duration) -> io::Result<()> {
        UnixStream::set_write_timeout(self, timeout)
    }

    fn write_bytes(&self, bytes: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut &*self, bytes)
    }
}

fn write_response<W: ResponseWriter>(
    writer: &W,
    mut response: String,
    limits: IpcLimits,
    connection_deadline: Instant,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    anyhow::ensure!(!stop.load(Ordering::Acquire), "ipc server is stopping");
    remaining(connection_deadline, "connection")?;
    if response.len() > limits.max_response_bytes {
        anyhow::bail!("ipc response exceeds the byte limit");
    }
    response.push('\n');
    let write_deadline = Instant::now()
        .checked_add(limits.write_timeout)
        .context("ipc write deadline overflow")?;
    let mut remaining_bytes = response.as_bytes();
    while !remaining_bytes.is_empty() {
        anyhow::ensure!(!stop.load(Ordering::Acquire), "ipc server is stopping");
        let timeout = IPC_SOCKET_POLL_INTERVAL
            .min(remaining(write_deadline, "write")?)
            .min(remaining(connection_deadline, "connection")?);
        writer
            .set_write_timeout(timeout)
            .context("could not set ipc client write timeout")?;
        match writer.write_bytes(remaining_bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "ipc socket write returned zero bytes",
                )
                .into());
            }
            Ok(written) => remaining_bytes = &remaining_bytes[written..],
            Err(error) if is_socket_timeout(&error) => {
                anyhow::ensure!(!stop.load(Ordering::Acquire), "ipc server is stopping");
            }
            Err(error) => return Err(error).context("could not write response to ipc client"),
        }
    }

    Ok(())
}

/// All commands that can be sent through the IPC mechanism. When serialized,
/// the enum variant is in snake case and denoted with "cmd".
/// Example JSON format: {"cmd":"set_color","active":<color>,"inactive":<color>}
#[derive(Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum IpcCommand {
    SetColor {
        #[serde(default)]
        active: Option<ColorBrushConfig>,
        #[serde(default)]
        inactive: Option<ColorBrushConfig>,
        /// When true, only the currently focused window's border is updated.
        #[serde(default)]
        focused: bool,
    },
    SetWidth {
        width: WidthConfig,
        #[serde(default)]
        focused: bool,
    },
    SetOffset {
        offset: OffsetConfig,
        #[serde(default)]
        focused: bool,
    },
    SetRadius {
        radius: RadiusConfig,
        #[serde(default)]
        focused: bool,
    },
    Reload,
    GetState,
}

struct CommandResponse {
    body: String,
    acknowledgement: Option<std::sync::mpsc::SyncSender<()>>,
}

impl CommandResponse {
    fn plain(body: String) -> Self {
        Self {
            body,
            acknowledgement: None,
        }
    }
}

fn process_command(raw: &str, context: &CommandContext) -> CommandResponse {
    match execute_command(raw, context) {
        Ok(response) => response,
        Err(error) => {
            CommandResponse::plain(json!({"ok": false, "error": format!("{error:#}")}).to_string())
        }
    }
}

fn execute_command(raw: &str, context: &CommandContext) -> anyhow::Result<CommandResponse> {
    context.check()?;
    let command: IpcCommand = match serde_json::from_str(raw) {
        Ok(command) => command,
        Err(err) => {
            return Ok(CommandResponse::plain(
                json!({"ok": false, "error": format!("invalid command: {err}")}).to_string(),
            ));
        }
    };

    context.check()?;
    Ok(match command {
        IpcCommand::SetColor {
            active,
            inactive,
            focused,
        } => {
            if active.is_none() && inactive.is_none() {
                return Ok(CommandResponse::plain(
                    json!({"ok": false, "error": "no colors provided"}).to_string(),
                ));
            }
            CommandResponse::plain(apply_update(
                IpcUpdate::Colors { active, inactive },
                focused,
                context,
            ))
        }
        IpcCommand::SetWidth { width, focused } => {
            CommandResponse::plain(apply_update(IpcUpdate::Width(width), focused, context))
        }
        IpcCommand::SetOffset { offset, focused } => {
            CommandResponse::plain(apply_update(IpcUpdate::Offset(offset), focused, context))
        }
        IpcCommand::SetRadius { radius, focused } => {
            CommandResponse::plain(apply_update(IpcUpdate::Radius(radius), focused, context))
        }
        IpcCommand::Reload => reload_from_host(context),
        IpcCommand::GetState => {
            let (active_color, inactive_color, border_width, border_offset, border_radius) = {
                let config = context.read(&APP_STATE.config, "configuration")?;
                (
                    config.global.active_color.clone(),
                    config.global.inactive_color.clone(),
                    config.global.border_width,
                    config.global.border_offset,
                    config.global.border_radius,
                )
            };
            let active_window = {
                let isize = *context.lock(&APP_STATE.active_window, "active window")?;
                format!("{isize:#x}")
            };
            let border_count = APP_STATE.border_windows(context)?.len();
            context.check()?;

            CommandResponse::plain(
                json!({
                    "ok": true,
                    "active_window": active_window,
                    "border_count": border_count,
                    "active_color": active_color,
                    "inactive_color": inactive_color,
                    "border_width": border_width,
                    "border_offset": border_offset,
                    "border_radius": border_radius,
                })
                .to_string(),
            )
        }
    })
}

fn reload_from_host(context: &CommandContext) -> CommandResponse {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    if let Err(error) = crate::submit_ipc_reload(sender, context) {
        return CommandResponse::plain(
            json!({"ok": false, "changed": false, "error": format!("reload request rejected: {error:#}")})
                .to_string(),
        );
    }

    match wait_for_reload(&receiver, context) {
        Ok(reply) => {
            let body = match reply.outcome {
                ReloadOutcome::Applied {
                    changed,
                    generation,
                } => json!({"ok": true, "changed": changed, "generation": generation}),
                ReloadOutcome::Rejected {
                    stage,
                    error,
                    ..
                } => {
                    json!({"ok": false, "changed": false, "stage": format!("{stage:?}"), "error": format!("reload rejected: {error:#}")})
                }
            }
            .to_string();
            CommandResponse {
                body,
                acknowledgement: Some(reply.acknowledgement),
            }
        }
        Err(error) => CommandResponse::plain(
            json!({"ok": false, "changed": false, "error": format!("reload coordinator unavailable: {error}")})
                .to_string(),
        ),
    }
}

fn wait_for_reload(
    receiver: &std::sync::mpsc::Receiver<crate::IpcReloadResponse>,
    context: &CommandContext,
) -> anyhow::Result<crate::IpcReloadResponse> {
    loop {
        match receiver.recv_timeout(Duration::from_millis(5)) {
            Ok(reply) => break Ok(reply),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(anyhow::anyhow!("reload owner disconnected"));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Err(error) = context.check()
                    && (context.cancel_before_commit() || !context.is_committed())
                {
                    return Err(error);
                }
            }
        }
    }
}

#[derive(Default, Debug, Serialize)]
struct IpcDeliveryReport {
    config_accepted: bool,
    config_changed: bool,
    intended_targets: usize,
    accepted_targets: usize,
    failed_targets: usize,
    selection_failed: bool,
}

impl IpcDeliveryReport {
    fn response(&self) -> String {
        let accepted = self.config_accepted || self.accepted_targets != 0;
        let ok = accepted && self.failed_targets == 0 && !self.selection_failed;
        let mut value = serde_json::to_value(self).unwrap();
        value["ok"] = json!(ok);
        value["partial"] = json!(accepted && !ok);
        if !ok {
            value["error"] =
                json!("appearance update was not accepted by every intended destination");
        }
        value.to_string()
    }
}

enum TargetSelection {
    All,
    Focused(Option<TrackingWindowFingerprint>),
}

fn apply_update(update: IpcUpdate, focused: bool, context: &CommandContext) -> String {
    let selection = if focused {
        TargetSelection::Focused(TrackingWindowFingerprint::capture(get_foreground_window()))
    } else {
        TargetSelection::All
    };
    match broadcast_update_with_generation(
        &APP_STATE.appearance,
        &APP_STATE.config,
        &APP_STATE.config_generation,
        &APP_STATE.border_registry,
        selection,
        update,
        context,
    ) {
        Ok(report) => report.response(),
        Err(error) => json!({"ok": false, "partial": false, "config_changed": false, "intended_targets": 0, "accepted_targets": 0, "failed_targets": 0, "error": format!("setter rejected: {error:#}")}).to_string(),
    }
}

#[cfg(test)]
fn broadcast_update(
    gate: &Mutex<AppearanceSequencer>,
    config: &RwLock<RuntimeConfig>,
    registry: &Mutex<BorderRegistry>,
    selection: TargetSelection,
    update: IpcUpdate,
) -> anyhow::Result<IpcDeliveryReport> {
    broadcast_update_inner(
        gate,
        config,
        None,
        registry,
        selection,
        update,
        &CommandContext::default(),
    )
}

fn broadcast_update_with_generation(
    gate: &Mutex<AppearanceSequencer>,
    config: &RwLock<RuntimeConfig>,
    config_generation: &AtomicU64,
    registry: &Mutex<BorderRegistry>,
    selection: TargetSelection,
    update: IpcUpdate,
    context: &CommandContext,
) -> anyhow::Result<IpcDeliveryReport> {
    broadcast_update_inner(
        gate,
        config,
        Some(config_generation),
        registry,
        selection,
        update,
        context,
    )
}

fn broadcast_update_inner(
    gate: &Mutex<AppearanceSequencer>,
    config: &RwLock<RuntimeConfig>,
    config_generation: Option<&AtomicU64>,
    registry: &Mutex<BorderRegistry>,
    selection: TargetSelection,
    update: IpcUpdate,
    context: &CommandContext,
) -> anyhow::Result<IpcDeliveryReport> {
    let mut sequence = context.lock(gate, "appearance gate")?;
    let mut config = if matches!(selection, TargetSelection::All) {
        Some(context.write(config, "configuration")?)
    } else {
        None
    };
    let mut registry_guard = context.lock(registry, "border registry")?;
    let targets = registry_guard
        .entries
        .values()
        .filter(|entry| match selection {
            TargetSelection::All => true,
            TargetSelection::Focused(fingerprint) => {
                fingerprint == Some(entry.tracking.fingerprint)
                    && registry_guard
                        .current_by_hwnd
                        .get(&entry.tracking.fingerprint.hwnd)
                        == Some(&entry.id)
            }
        })
        .map(|entry| entry.updates.clone())
        .collect::<Vec<_>>();
    let admissions = targets
        .iter()
        .map(|mailbox| mailbox.admit(context))
        .collect::<anyhow::Result<Vec<_>>>()?;
    context.begin_commit()?;
    let command = sequence.next_command().ok_or_else(|| {
        registry_guard.quarantine();
        anyhow::anyhow!("appearance command IDs exhausted")
    })?;
    let mut report = IpcDeliveryReport::default();
    if matches!(selection, TargetSelection::All) {
        let revision = sequence.config_revision.checked_add(1).ok_or_else(|| {
            registry_guard.quarantine();
            anyhow::anyhow!("configuration revisions exhausted")
        })?;
        if let Some(config_generation) = config_generation {
            config_generation
                .try_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
                    generation.checked_add(1)
                })
                .map_err(|_| anyhow::anyhow!("configuration generations exhausted"))?;
        }
        let global = config
            .as_mut()
            .expect("global selection holds config")
            .global_mut();
        match &update {
            IpcUpdate::Colors { active, inactive } => {
                if let Some(color) = active {
                    report.config_changed |= global.active_color != *color;
                    global.active_color = color.clone();
                    sequence.global_stamps.active = command;
                }
                if let Some(color) = inactive {
                    report.config_changed |= global.inactive_color != *color;
                    global.inactive_color = color.clone();
                    sequence.global_stamps.inactive = command;
                }
            }
            IpcUpdate::Width(value) => {
                report.config_changed = global.border_width != *value;
                global.border_width = *value;
                sequence.global_stamps.width = command;
            }
            IpcUpdate::Offset(value) => {
                report.config_changed = global.border_offset != *value;
                global.border_offset = *value;
                sequence.global_stamps.offset = command;
            }
            IpcUpdate::Radius(value) => {
                report.config_changed = global.border_radius != *value;
                global.border_radius = *value;
                sequence.global_stamps.radius = command;
            }
        }
        sequence.config_revision = revision;
        report.config_accepted = true;
    }
    report.intended_targets = targets.len();
    drop(registry_guard);
    for admission in admissions {
        if admission.is_some_and(|admission| admission.enqueue(command, update.clone()).is_ok()) {
            report.accepted_targets += 1;
        } else {
            report.failed_targets += 1;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::border_registry::{BorderAdmission, BorderControl, ReserveResult};
    use crate::border_updates::{BorderUpdateMailbox, MailboxDrain};
    use std::io::{Read, Write};
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    static NEXT_SOCKET_PATH: AtomicU64 = AtomicU64::new(0);

    type IpcFixture = (
        Arc<Mutex<AppearanceSequencer>>,
        Arc<RwLock<RuntimeConfig>>,
        Arc<Mutex<BorderRegistry>>,
        Arc<BorderUpdateMailbox>,
    );

    fn fixture() -> IpcFixture {
        let mailbox =
            Arc::new(BorderUpdateMailbox::new(Arc::new(BorderControl::new().unwrap())).unwrap());
        let mut registry = BorderRegistry::default();
        registry.admission = BorderAdmission::Open;
        let identity = registry
            .observe(TrackingWindowFingerprint {
                hwnd: 10,
                process_id: 11,
                thread_id: 12,
            })
            .unwrap();
        assert!(matches!(
            registry
                .reserve(identity, mailbox.clone(), Instant::now())
                .unwrap(),
            ReserveResult::Reserved(_)
        ));
        (
            Arc::new(Mutex::new(AppearanceSequencer::default())),
            Arc::new(RwLock::new(RuntimeConfig::fallback())),
            Arc::new(Mutex::new(registry)),
            mailbox,
        )
    }

    fn test_socket_path(name: &str) -> PathBuf {
        let id = NEXT_SOCKET_PATH.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "tacky-borders-ipc-{name}-{}-{id}.sock",
            std::process::id()
        ))
    }

    #[test]
    fn accept_loop_retries_reset_connections_before_dispatching_next_client() {
        let stop = AtomicBool::new(false);
        let attempts = AtomicUsize::new(0);
        let retry_kinds = Mutex::new(Vec::new());
        let dispatched = AtomicUsize::new(0);

        run_accept_loop(
            &stop,
            || match attempts.fetch_add(1, Ordering::Relaxed) {
                0 => Err(io::Error::from_raw_os_error(WSAECONNRESET.0)),
                1 => Err(io::Error::from_raw_os_error(WSAECONNABORTED.0)),
                _ => Ok("healthy client"),
            },
            |client| {
                assert_eq!(client, "healthy client");
                dispatched.fetch_add(1, Ordering::Relaxed);
                stop.store(true, Ordering::Release);
            },
            |kind, _| retry_kinds.lock().unwrap().push(kind),
            |error| panic!("transient accept error became fatal: {error}"),
            |_| {},
        );

        assert_eq!(attempts.load(Ordering::Relaxed), 3);
        assert_eq!(dispatched.load(Ordering::Relaxed), 1);
        assert_eq!(
            *retry_kinds.lock().unwrap(),
            [
                AcceptErrorKind::TransientConnection,
                AcceptErrorKind::TransientConnection
            ]
        );
    }

    #[test]
    fn accept_loop_waits_after_would_block_and_stops_on_fatal_error() {
        let stop = AtomicBool::new(false);
        let attempts = AtomicUsize::new(0);
        let waits = Mutex::new(Vec::new());
        let fatal = Mutex::new(None);

        run_accept_loop(
            &stop,
            || match attempts.fetch_add(1, Ordering::Relaxed) {
                0 => Err(io::Error::from(io::ErrorKind::WouldBlock)),
                _ => Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "listener failure",
                )),
            },
            |_: ()| panic!("unexpected accepted client"),
            |kind, _| waits.lock().unwrap().push(kind),
            |error| *fatal.lock().unwrap() = Some(error.to_string()),
            |delay| assert_eq!(delay, IPC_ACCEPT_POLL_INTERVAL),
        );

        assert_eq!(attempts.load(Ordering::Relaxed), 2);
        assert_eq!(*waits.lock().unwrap(), [AcceptErrorKind::WouldBlock]);
        assert_eq!(fatal.lock().unwrap().as_deref(), Some("listener failure"));
    }

    #[test]
    fn setter_lock_deadlines_leave_configuration_and_delivery_unchanged() {
        for blocked_lock in 0..4 {
            let (gate, config, registry, mailbox) = fixture();
            let original = config.read().unwrap().global.border_width;
            let generation = AtomicU64::new(7);
            let context = CommandContext::new(
                Instant::now() + Duration::from_millis(40),
                Arc::new(AtomicBool::new(false)),
            );
            let attempt = || {
                let error = broadcast_update_inner(
                    &gate,
                    &config,
                    Some(&generation),
                    &registry,
                    TargetSelection::All,
                    IpcUpdate::Width(WidthConfig::new(23.0)),
                    &context,
                )
                .unwrap_err();
                assert!(error.to_string().contains("deadline"), "{error:#}");
                assert!(!context.is_committed());
            };
            let started = Instant::now();
            match blocked_lock {
                0 => {
                    let _guard = gate.lock().unwrap();
                    attempt();
                }
                1 => {
                    let _guard = config.read().unwrap();
                    attempt();
                }
                2 => {
                    let _guard = registry.lock().unwrap();
                    attempt();
                }
                _ => mailbox.while_locked(attempt),
            }
            assert!(started.elapsed() < Duration::from_secs(2));
            assert_eq!(config.read().unwrap().global.border_width, original);
            assert_eq!(gate.lock().unwrap().config_revision, 0);
            assert_eq!(generation.load(Ordering::Acquire), 7);
            let MailboxDrain::Updates(pending) = mailbox.try_drain() else {
                panic!("mailbox unavailable after expired setter");
            };
            assert!(pending.width.is_none());
        }
    }

    #[test]
    #[serial_test::serial]
    fn get_state_deadline_covers_every_snapshot_lock() {
        let generation = APP_STATE.config_generation();
        for blocked_lock in 0..3 {
            let context = CommandContext::new(
                Instant::now() + Duration::from_millis(40),
                Arc::new(AtomicBool::new(false)),
            );
            let attempt = || {
                let result = execute_command(r#"{"cmd":"get_state"}"#, &context);
                let Err(error) = result else {
                    panic!("contended state snapshot succeeded");
                };
                assert!(error.to_string().contains("deadline"), "{error:#}");
            };
            let started = Instant::now();
            match blocked_lock {
                0 => {
                    let _guard = APP_STATE.config.write().unwrap();
                    attempt();
                }
                1 => {
                    let _guard = APP_STATE.active_window.lock().unwrap();
                    attempt();
                }
                _ => {
                    let _guard = APP_STATE.border_registry.lock().unwrap();
                    attempt();
                }
            }
            assert!(started.elapsed() < Duration::from_secs(2));
        }
        assert_eq!(APP_STATE.config_generation(), generation);
    }

    #[test]
    fn poisoned_mailbox_remains_a_partial_delivery_failure() {
        let (gate, config, registry, mailbox) = fixture();
        let poisoned = mailbox.clone();
        assert!(
            thread::spawn(move || poisoned.while_locked(|| panic!("poison mailbox")))
                .join()
                .is_err()
        );
        let report = broadcast_update(
            &gate,
            &config,
            &registry,
            TargetSelection::All,
            IpcUpdate::Width(WidthConfig::new(23.0)),
        )
        .unwrap();
        assert!(report.config_accepted);
        assert_eq!(report.failed_targets, 1);
        assert!(mailbox.control().is_stopped());
        assert!(mailbox.is_closed());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&report.response()).unwrap()["partial"],
            true
        );
    }

    #[test]
    fn expired_queued_reload_is_rejected_when_owner_eventually_dequeues_it() {
        let (_sender, receiver) = std::sync::mpsc::sync_channel(1);
        let context = CommandContext::new(
            Instant::now() + Duration::from_millis(40),
            Arc::new(AtomicBool::new(false)),
        );
        let (host_sender, host_receiver) = std::sync::mpsc::sync_channel(1);
        host_sender
            .send(crate::HostAction::ReloadConfig {
                origin: crate::ReloadOrigin::Ipc,
                response: Some(_sender),
                context: Some(context.clone()),
            })
            .unwrap();
        assert!(
            wait_for_reload(&receiver, &context)
                .unwrap_err()
                .to_string()
                .contains("deadline")
        );
        let crate::HostAction::ReloadConfig {
            origin,
            context: Some(owner),
            ..
        } = host_receiver.recv().unwrap()
        else {
            panic!("reload context was not queued");
        };
        let completion = Config::reload_with_context(origin, &owner);
        let ReloadOutcome::Rejected { error, .. } = completion.outcome else {
            panic!("expired queued reload was accepted");
        };
        assert!(error.to_string().contains("cancelled"));
        assert!(!owner.is_committed());
        assert!(!completion.retired.retires_ipc_server());
    }

    #[test]
    fn reload_wait_returns_committed_outcome_even_after_caller_expiry() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let context = CommandContext::new(
            Instant::now() + Duration::from_millis(40),
            Arc::new(AtomicBool::new(false)),
        );
        context.begin_commit().unwrap();
        let owner = thread::spawn(move || {
            thread::sleep(Duration::from_millis(80));
            let (acknowledgement, _received) = std::sync::mpsc::sync_channel(1);
            sender
                .send(crate::IpcReloadResponse {
                    outcome: ReloadOutcome::Applied {
                        changed: true,
                        generation: 12,
                    },
                    acknowledgement,
                })
                .unwrap();
        });
        assert!(matches!(
            wait_for_reload(&receiver, &context).unwrap().outcome,
            ReloadOutcome::Applied {
                changed: true,
                generation: 12
            }
        ));
        owner.join().unwrap();
    }

    #[test]
    fn reload_wait_distinguishes_owner_disconnection() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        drop(sender);
        assert!(
            wait_for_reload(&receiver, &CommandContext::default())
                .unwrap_err()
                .to_string()
                .contains("disconnected")
        );
    }

    fn exercise_blocked_worker_pool(cancel: bool) -> anyhow::Result<()> {
        let path = test_socket_path("command-workers");
        let listener = UnixListener::bind(&path)?;
        let queue = Arc::new(ClientQueue::new(IPC_QUEUE_CAPACITY));
        let active = Arc::new(ActiveClients::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (gate, config, registry, _) = fixture();
        let original = config.read().unwrap().global.border_width;
        let held_gate = gate.lock().unwrap();
        let (entered, entries) = std::sync::mpsc::channel();
        let (done, completions) = std::sync::mpsc::channel();
        let limits = IpcLimits {
            connection_timeout: if cancel {
                Duration::from_secs(30)
            } else {
                Duration::from_millis(250)
            },
            ..IpcLimits::default()
        };
        let mut workers = Vec::new();
        for _ in 0..IPC_WORKER_COUNT {
            let (queue, active, stop) = (queue.clone(), active.clone(), stop.clone());
            let (gate, config, registry) = (gate.clone(), config.clone(), registry.clone());
            let (entered, done) = (entered.clone(), done.clone());
            workers.push(thread::spawn(move || {
                run_worker_with(queue, active, stop, |stream, stop| {
                    handle_client_with_processor(stream, limits, stop, |raw, context| {
                        if matches!(
                            serde_json::from_str::<IpcCommand>(raw),
                            Ok(IpcCommand::SetWidth { .. })
                        ) {
                            entered.send(()).unwrap();
                            let result = broadcast_update_inner(
                                &gate,
                                &config,
                                None,
                                &registry,
                                TargetSelection::All,
                                IpcUpdate::Width(WidthConfig::new(23.0)),
                                context,
                            );
                            assert!(result.is_err());
                            CommandResponse::plain(json!({"ok": false}).to_string())
                        } else {
                            process_command(raw, context)
                        }
                    })
                });
                done.send(()).unwrap();
            }));
        }
        let mut clients = Vec::new();
        for _ in 0..IPC_WORKER_COUNT {
            let client = UnixStream::connect(&path)?;
            let stream = listener.accept()?;
            assert!(matches!(queue.try_push(stream), PushResult::Queued));
            (&client).write_all(b"{\"cmd\":\"set_width\",\"width\":23}\n")?;
            clients.push(client);
        }
        for _ in 0..IPC_WORKER_COUNT {
            entries.recv_timeout(Duration::from_secs(2))?;
        }
        assert_eq!(active.state.lock().unwrap().sockets.len(), IPC_WORKER_COUNT);
        let queued = UnixStream::connect(&path)?;
        let stream = listener.accept()?;
        assert!(matches!(queue.try_push(stream), PushResult::Queued));
        (&queued).write_all(b"{\"cmd\":\"unknown\"}\n")?;
        if !cancel {
            queued.set_read_timeout(Duration::from_secs(2))?;
            let mut response = String::new();
            BufReader::new(&queued).read_line(&mut response)?;
            assert!(response.contains("invalid command"));
        }
        let shutdown = Instant::now();
        stop.store(true, Ordering::Release);
        queue.close_and_drain();
        active.shutdown_all();
        for _ in 0..IPC_WORKER_COUNT {
            completions.recv_timeout(Duration::from_secs(2))?;
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(shutdown.elapsed() < Duration::from_secs(2));
        assert_eq!(config.read().unwrap().global.border_width, original);
        assert_eq!(held_gate.config_revision, 0);
        assert!(active.state.lock().unwrap().sockets.is_empty());
        drop((clients, queued, listener, held_gate));
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn command_deadlines_release_all_four_workers_and_service_queued_client() -> anyhow::Result<()>
    {
        exercise_blocked_worker_pool(false)
    }

    #[test]
    fn shutdown_joins_all_four_command_workers_while_application_gate_is_still_locked()
    -> anyhow::Result<()> {
        exercise_blocked_worker_pool(true)
    }

    #[test]
    fn cli_deadline_is_absolute_even_with_response_trickle() -> anyhow::Result<()> {
        let path = test_socket_path("cli-trickle");
        let listener = UnixListener::bind(&path)?;
        let peer = thread::spawn(move || {
            let stream = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(&stream).read_line(&mut request).unwrap();
            while (&stream).write_all(b" ").is_ok() {
                thread::sleep(Duration::from_millis(10));
            }
        });
        let start = Instant::now();
        let result = send_command_to(&path, r#"{"cmd":"get_state"}"#, Duration::from_millis(100));
        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(2));
        peer.join().unwrap();
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn cli_accepts_normal_reply_and_reports_empty_eof_separately() -> anyhow::Result<()> {
        for reply in [Some("{\"ok\":true}\n"), None] {
            let path = test_socket_path("cli-reply");
            let listener = UnixListener::bind(&path)?;
            let peer = thread::spawn(move || {
                let stream = listener.accept().unwrap();
                let mut request = String::new();
                BufReader::new(&stream).read_line(&mut request).unwrap();
                if let Some(reply) = reply {
                    (&stream).write_all(reply.as_bytes()).unwrap();
                }
            });
            let result = send_command_to(&path, r#"{"cmd":"get_state"}"#, Duration::from_secs(2));
            match reply {
                Some(_) => assert_eq!(result?, "{\"ok\":true}"),
                None => assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("closed without a response")
                ),
            }
            peer.join().unwrap();
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    #[test]
    fn client_queue_enforces_capacity_and_fifo_order() {
        let queue = ClientQueue::new(2);
        assert!(matches!(queue.try_push(1), PushResult::Queued));
        assert!(matches!(queue.try_push(2), PushResult::Queued));
        assert!(matches!(queue.try_push(3), PushResult::Full(3)));
        assert_eq!(queue.pop(), Some(1));
        assert_eq!(queue.pop(), Some(2));
    }

    #[test]
    fn client_queue_rejects_pushes_after_close() {
        let queue = ClientQueue::new(1);
        queue.close_and_drain();

        assert!(matches!(queue.try_push(1), PushResult::Closed(1)));
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn client_queue_close_wakes_waiter() {
        let queue = Arc::new(ClientQueue::<usize>::new(1));
        let waiter_queue = queue.clone();
        let waiter = thread::spawn(move || waiter_queue.pop());

        queue.close_and_drain();

        assert_eq!(waiter.join().unwrap(), None);
    }

    fn read_test_command(
        input: &[u8],
        capacity: usize,
        limit: usize,
    ) -> Result<CommandRead, CommandReadError> {
        let cursor = io::Cursor::new(input);
        let mut reader = BufReader::with_capacity(capacity, cursor);
        read_bounded_command(&mut reader, limit, || Ok(()), || {})
    }

    #[test]
    fn bounded_reader_preserves_newline_and_eof_commands() {
        let CommandRead::Command(command) = read_test_command(b"first\n", 8, 16).unwrap() else {
            panic!("newline command was not returned");
        };
        assert_eq!(command, b"first");

        let CommandRead::Command(command) = read_test_command(b"final", 8, 16).unwrap() else {
            panic!("EOF command was not returned");
        };
        assert_eq!(command, b"final");
        assert!(matches!(
            read_test_command(b"", 8, 16).unwrap(),
            CommandRead::Eof
        ));
    }

    #[test]
    fn bounded_reader_enforces_exact_payload_limit() {
        let CommandRead::Command(command) = read_test_command(b"1234\n", 2, 4).unwrap() else {
            panic!("limit-sized command was not returned");
        };
        assert_eq!(command, b"1234");
        assert!(matches!(
            read_test_command(b"12345\n", 2, 4),
            Err(CommandReadError::TooLarge)
        ));
    }

    #[test]
    fn bounded_reader_stops_at_each_buffered_newline() {
        let cursor = io::Cursor::new(b"one\ntwo\n");
        let mut reader = BufReader::with_capacity(16, cursor);
        let CommandRead::Command(first) =
            read_bounded_command(&mut reader, 16, || Ok(()), || {}).unwrap()
        else {
            panic!("first command was not returned");
        };
        let CommandRead::Command(second) =
            read_bounded_command(&mut reader, 16, || Ok(()), || {}).unwrap()
        else {
            panic!("second command was not returned");
        };
        assert_eq!(first, b"one");
        assert_eq!(second, b"two");
    }

    #[test]
    fn server_bounds_clients_and_rebinds_after_active_shutdown() -> anyhow::Result<()> {
        let socket_path = test_socket_path("bounds");
        let server = IpcServer::new(&socket_path)?;
        assert_eq!(server.workers.len(), IPC_WORKER_COUNT);

        let mut clients = Vec::new();
        for _ in 0..IPC_WORKER_COUNT {
            clients.push(UnixStream::connect(&socket_path)?);
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        while server
            .active
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .sockets
            .len()
            != IPC_WORKER_COUNT
        {
            anyhow::ensure!(
                Instant::now() < deadline,
                "server did not assign one client to each worker"
            );
            thread::yield_now();
        }

        for _ in 0..IPC_QUEUE_CAPACITY {
            clients.push(UnixStream::connect(&socket_path)?);
        }
        while server
            .queue
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clients
            .len()
            != IPC_QUEUE_CAPACITY
        {
            anyhow::ensure!(
                Instant::now() < deadline,
                "server did not fill its bounded client queue"
            );
            thread::yield_now();
        }

        clients.push(UnixStream::connect(&socket_path)?);
        while server.rejected.load(Ordering::Relaxed) == 0 {
            anyhow::ensure!(
                Instant::now() < deadline,
                "server did not reject a client beyond its bounded capacity"
            );
            thread::yield_now();
        }

        assert_eq!(
            server
                .active
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .sockets
                .len(),
            IPC_WORKER_COUNT
        );
        assert_eq!(
            server
                .queue
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clients
                .len(),
            IPC_QUEUE_CAPACITY
        );

        let shutdown_started = Instant::now();
        drop(server);
        anyhow::ensure!(
            shutdown_started.elapsed() < Duration::from_secs(2),
            "server shutdown exceeded its bounded wait"
        );
        anyhow::ensure!(!socket_path.exists(), "ipc socket path was not removed");
        drop(clients);

        let restarted = IpcServer::new(&socket_path)?;
        drop(restarted);
        anyhow::ensure!(
            !socket_path.exists(),
            "restarted server left its socket path"
        );
        Ok(())
    }

    #[test]
    fn server_rejects_oversized_command_and_closes_client() -> anyhow::Result<()> {
        let socket_path = test_socket_path("oversized");
        let server = IpcServer::new(&socket_path)?;
        let mut client = UnixStream::connect(&socket_path)?;
        client.set_read_timeout(Duration::from_secs(2))?;
        let mut command = vec![b'x'; MAX_COMMAND_BYTES + 1];
        command.push(b'\n');

        client.write_all(&command)?;
        let mut response = String::new();
        client.read_to_string(&mut response)?;

        assert!(response.contains("command byte limit exceeded"));
        drop(server);
        Ok(())
    }

    #[test]
    fn blank_commands_count_toward_connection_limit() -> anyhow::Result<()> {
        let socket_path = test_socket_path("command-count");
        let server = IpcServer::new(&socket_path)?;
        let mut client = UnixStream::connect(&socket_path)?;
        client.set_read_timeout(Duration::from_secs(2))?;

        client.write_all(&[b'\n'; MAX_COMMANDS_PER_CONNECTION])?;
        let mut response = String::new();
        client.read_to_string(&mut response)?;

        assert!(response.contains("command count limit exceeded"));
        drop(server);
        Ok(())
    }

    #[test]
    fn idle_deadline_releases_handler() -> anyhow::Result<()> {
        let socket_path = test_socket_path("idle-deadline");
        remove_file_if_exists(&socket_path)?;
        let listener = UnixListener::bind(&socket_path)?;
        let mut client = UnixStream::connect(&socket_path)?;
        client.set_read_timeout(Duration::from_secs(2))?;
        let server_stream = listener.accept()?;
        let limits = IpcLimits {
            idle_timeout: Duration::from_millis(100),
            command_timeout: Duration::from_secs(1),
            connection_timeout: Duration::from_secs(1),
            ..IpcLimits::default()
        };
        let handler = thread::spawn(move || {
            handle_client_with_limits(&server_stream, limits, &Arc::new(AtomicBool::new(false)))
        });
        let started = Instant::now();

        let mut buffer = [0u8; 1];
        assert_eq!(client.read(&mut buffer)?, 0);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(handler.join().unwrap().is_err());

        drop(listener);
        remove_file_if_exists(&socket_path)?;
        Ok(())
    }

    #[test]
    fn response_limit_rejects_before_writing() -> anyhow::Result<()> {
        let socket_path = test_socket_path("response-limit");
        remove_file_if_exists(&socket_path)?;
        let listener = UnixListener::bind(&socket_path)?;
        let _client = UnixStream::connect(&socket_path)?;
        let server_stream = listener.accept()?;
        let limits = IpcLimits {
            max_response_bytes: 3,
            ..IpcLimits::default()
        };
        let connection_deadline = Instant::now() + Duration::from_secs(1);

        let error = write_response(
            &server_stream,
            "1234".to_owned(),
            limits,
            connection_deadline,
            &AtomicBool::new(false),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("response exceeds the byte limit")
        );
        drop(listener);
        remove_file_if_exists(&socket_path)?;
        Ok(())
    }

    struct TimeoutWriter<'a> {
        stop_on_write: Option<&'a AtomicBool>,
        writes: AtomicUsize,
    }

    impl ResponseWriter for TimeoutWriter<'_> {
        fn set_write_timeout(&self, timeout: Duration) -> io::Result<()> {
            assert!(!timeout.is_zero());
            assert!(timeout <= IPC_SOCKET_POLL_INTERVAL);
            Ok(())
        }

        fn write_bytes(&self, _bytes: &[u8]) -> io::Result<usize> {
            self.writes.fetch_add(1, Ordering::Relaxed);
            if let Some(stop) = self.stop_on_write {
                stop.store(true, Ordering::Release);
            } else {
                thread::sleep(Duration::from_millis(1));
            }
            Err(io::Error::new(io::ErrorKind::TimedOut, "test timeout"))
        }
    }

    #[test]
    fn write_timeouts_end_at_write_deadline() {
        let stop = AtomicBool::new(false);
        let writer = TimeoutWriter {
            stop_on_write: None,
            writes: AtomicUsize::new(0),
        };
        let limits = IpcLimits {
            write_timeout: Duration::from_millis(20),
            connection_timeout: Duration::from_secs(1),
            ..IpcLimits::default()
        };

        let error = write_response(
            &writer,
            "response".to_owned(),
            limits,
            Instant::now() + limits.connection_timeout,
            &stop,
        )
        .unwrap_err();

        assert!(error.to_string().contains("write deadline exceeded"));
        assert!(writer.writes.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn stop_cancels_write_after_bounded_timeout() {
        let stop = AtomicBool::new(false);
        let writer = TimeoutWriter {
            stop_on_write: Some(&stop),
            writes: AtomicUsize::new(0),
        };
        let limits = IpcLimits {
            write_timeout: Duration::from_secs(5),
            connection_timeout: Duration::from_secs(10),
            ..IpcLimits::default()
        };

        let error = write_response(
            &writer,
            "response".to_owned(),
            limits,
            Instant::now() + limits.connection_timeout,
            &stop,
        )
        .unwrap_err();

        assert!(error.to_string().contains("ipc server is stopping"));
        assert_eq!(writer.writes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn slow_trickle_cannot_extend_command_deadline() -> anyhow::Result<()> {
        let socket_path = test_socket_path("command-deadline");
        remove_file_if_exists(&socket_path)?;
        let listener = UnixListener::bind(&socket_path)?;
        let mut client = UnixStream::connect(&socket_path)?;
        client.set_read_timeout(Duration::from_secs(2))?;
        let server_stream = listener.accept()?;
        let limits = IpcLimits {
            idle_timeout: Duration::from_millis(500),
            command_timeout: Duration::from_millis(200),
            connection_timeout: Duration::from_secs(1),
            ..IpcLimits::default()
        };
        let handler = thread::spawn(move || {
            handle_client_with_limits(&server_stream, limits, &Arc::new(AtomicBool::new(false)))
        });

        for _ in 0..3 {
            client.write_all(b"x")?;
            thread::sleep(Duration::from_millis(50));
        }

        let mut buffer = [0u8; 1];
        assert_eq!(client.read(&mut buffer)?, 0);
        let error = handler.join().unwrap().unwrap_err();
        assert!(error.to_string().contains("command deadline exceeded"));

        drop(listener);
        remove_file_if_exists(&socket_path)?;
        Ok(())
    }

    #[test]
    fn activity_cannot_extend_connection_deadline() -> anyhow::Result<()> {
        let socket_path = test_socket_path("connection-deadline");
        remove_file_if_exists(&socket_path)?;
        let listener = UnixListener::bind(&socket_path)?;
        let mut client = UnixStream::connect(&socket_path)?;
        client.set_read_timeout(Duration::from_secs(2))?;
        let server_stream = listener.accept()?;
        let limits = IpcLimits {
            idle_timeout: Duration::from_millis(500),
            command_timeout: Duration::from_millis(500),
            connection_timeout: Duration::from_millis(200),
            ..IpcLimits::default()
        };
        let handler = thread::spawn(move || {
            handle_client_with_limits(&server_stream, limits, &Arc::new(AtomicBool::new(false)))
        });

        for _ in 0..3 {
            client.write_all(b"\n")?;
            thread::sleep(Duration::from_millis(50));
        }

        let mut buffer = [0u8; 1];
        assert_eq!(client.read(&mut buffer)?, 0);
        let error = handler.join().unwrap().unwrap_err();
        assert!(error.to_string().contains("connection deadline exceeded"));

        drop(listener);
        remove_file_if_exists(&socket_path)?;
        Ok(())
    }

    #[test]
    fn unchanged_global_with_zero_targets_is_complete_acceptance() {
        let config = RwLock::new(RuntimeConfig::fallback());
        let width = config.read().unwrap().global.border_width;
        let report = broadcast_update(
            &Mutex::default(),
            &config,
            &Mutex::new(BorderRegistry::default()),
            TargetSelection::All,
            IpcUpdate::Width(width),
        )
        .unwrap();
        assert!(report.config_accepted);
        assert!(!report.config_changed);
        assert_eq!(report.intended_targets, 0);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&report.response()).unwrap()["ok"],
            true
        );
    }

    #[test]
    fn focused_missing_is_total_failure_without_config_mutation() {
        let (gate, config, registry, _) = fixture();
        let original = config.read().unwrap().global.border_width;
        let report = broadcast_update(
            &gate,
            &config,
            &registry,
            TargetSelection::Focused(None),
            IpcUpdate::Width(WidthConfig::new(19.0)),
        )
        .unwrap();
        assert!(!report.config_accepted);
        assert_eq!(config.read().unwrap().global.border_width, original);
        let response: serde_json::Value = serde_json::from_str(&report.response()).unwrap();
        assert_eq!(response["ok"], false);
        assert_eq!(response["partial"], false);
    }

    #[test]
    fn revision_exhaustion_quarantines_without_mutation_or_delivery() {
        let (gate, config, registry, mailbox) = fixture();
        let original = config.read().unwrap().global.border_width;
        gate.lock().unwrap().config_revision = u64::MAX;
        assert!(
            broadcast_update(
                &gate,
                &config,
                &registry,
                TargetSelection::All,
                IpcUpdate::Width(WidthConfig::new(22.0))
            )
            .is_err()
        );
        assert_eq!(config.read().unwrap().global.border_width, original);
        assert_eq!(gate.lock().unwrap().config_revision, u64::MAX);
        assert!(registry.lock().unwrap().quarantined);
        assert!(mailbox.control().is_stopped());
    }

    #[test]
    fn focused_components_survive_unrelated_global_snapshot_refresh() {
        let (gate, config, registry, mailbox) = fixture();
        let fingerprint = TrackingWindowFingerprint {
            hwnd: 10,
            process_id: 11,
            thread_id: 12,
        };
        let original_active = config.read().unwrap().global.active_color.clone();
        broadcast_update(
            &gate,
            &config,
            &registry,
            TargetSelection::Focused(Some(fingerprint)),
            IpcUpdate::Colors {
                active: Some(ColorBrushConfig::Solid("red".into())),
                inactive: None,
            },
        )
        .unwrap();
        broadcast_update(
            &gate,
            &config,
            &registry,
            TargetSelection::All,
            IpcUpdate::Colors {
                active: None,
                inactive: Some(ColorBrushConfig::Solid("blue".into())),
            },
        )
        .unwrap();
        broadcast_update(
            &gate,
            &config,
            &registry,
            TargetSelection::Focused(Some(fingerprint)),
            IpcUpdate::Width(WidthConfig::new(11.0)),
        )
        .unwrap();
        let MailboxDrain::Updates(mut pending) = mailbox.try_drain() else {
            panic!("mailbox did not drain");
        };
        let config = config.read().unwrap();
        assert_eq!(config.global.active_color, original_active);
        let sequence = gate.lock().unwrap();
        let resolved = crate::border_config::BorderConfig::resolve(
            &crate::config::WindowRule::default(),
            &config.global,
            config.render_backend,
            false,
        );
        pending.merge(crate::border_updates::PendingIpcUpdates::from_config(
            &resolved,
            sequence.global_stamps,
        ));
        let colors = pending.colors.unwrap();
        assert_eq!(
            colors.active.unwrap().value,
            ColorBrushConfig::Solid("red".into())
        );
        assert_eq!(
            colors.inactive.unwrap().value,
            ColorBrushConfig::Solid("blue".into())
        );
        assert_eq!(pending.width.unwrap().value, WidthConfig::new(11.0));
        assert_eq!(sequence.config_revision, 1);
    }

    #[test]
    fn reused_tracking_identity_cannot_retarget_old_mailbox() {
        let (gate, config, registry, old_mailbox) = fixture();
        let fingerprint = TrackingWindowFingerprint {
            hwnd: 10,
            process_id: 11,
            thread_id: 12,
        };
        let new_mailbox =
            Arc::new(BorderUpdateMailbox::new(Arc::new(BorderControl::new().unwrap())).unwrap());
        {
            let mut registry = registry.lock().unwrap();
            registry.retire_observation(fingerprint.hwnd, None, Instant::now());
            let identity = registry.observe(fingerprint).unwrap();
            registry
                .reserve(identity, new_mailbox.clone(), Instant::now())
                .unwrap();
        }
        let report = broadcast_update(
            &gate,
            &config,
            &registry,
            TargetSelection::Focused(Some(fingerprint)),
            IpcUpdate::Width(WidthConfig::new(17.0)),
        )
        .unwrap();
        assert_eq!(
            (
                report.intended_targets,
                report.accepted_targets,
                report.failed_targets
            ),
            (1, 1, 0)
        );
        assert!(old_mailbox.control().is_stopped());
        assert!(matches!(old_mailbox.try_drain(), MailboxDrain::Failed));
        let MailboxDrain::Updates(pending) = new_mailbox.try_drain() else {
            panic!("replacement did not drain");
        };
        assert_eq!(pending.width.unwrap().value, WidthConfig::new(17.0));
    }

    #[test]
    fn reserved_target_accepts_and_stopping_target_counts_as_rejected() {
        let (gate, config, registry, mailbox) = fixture();
        let update = IpcUpdate::Width(WidthConfig::new(9.0));
        let report = broadcast_update(
            &gate,
            &config,
            &registry,
            TargetSelection::All,
            update.clone(),
        )
        .unwrap();
        assert_eq!(
            (
                report.intended_targets,
                report.accepted_targets,
                report.failed_targets
            ),
            (1, 1, 0)
        );
        mailbox.control().record_stop();
        let report =
            broadcast_update(&gate, &config, &registry, TargetSelection::All, update).unwrap();
        assert_eq!(
            (
                report.intended_targets,
                report.accepted_targets,
                report.failed_targets
            ),
            (1, 0, 1)
        );
        assert!(report.config_accepted && !report.config_changed);
        let response: serde_json::Value = serde_json::from_str(&report.response()).unwrap();
        assert_eq!(response["partial"], true);
        assert_eq!(response["ok"], false);
    }

    #[test]
    fn two_clients_commit_and_enqueue_in_the_same_order() {
        let (gate, config, registry, mailbox) = fixture();
        let mut workers = Vec::with_capacity(2);
        mailbox.while_locked(|| {
            let first_gate = gate.clone();
            let first_config = config.clone();
            let first_registry = registry.clone();
            workers.push(thread::spawn(move || {
                broadcast_update(
                    &first_gate,
                    &first_config,
                    &first_registry,
                    TargetSelection::All,
                    IpcUpdate::Width(WidthConfig::new(9.0)),
                )
                .unwrap()
            }));
            let deadline = Instant::now() + Duration::from_secs(2);
            while gate.try_lock().is_ok() {
                assert!(Instant::now() < deadline);
                thread::yield_now();
            }
            while config.try_read().is_ok() {
                assert!(Instant::now() < deadline);
                thread::yield_now();
            }
            let second_gate = gate.clone();
            let second_config = config.clone();
            let second_registry = registry.clone();
            workers.push(thread::spawn(move || {
                broadcast_update(
                    &second_gate,
                    &second_config,
                    &second_registry,
                    TargetSelection::All,
                    IpcUpdate::Width(WidthConfig::new(13.0)),
                )
                .unwrap()
            }));
        });
        for worker in workers {
            assert_eq!(worker.join().unwrap().accepted_targets, 1);
        }
        assert_eq!(
            config.read().unwrap().global.border_width,
            WidthConfig::new(13.0)
        );
        let MailboxDrain::Updates(pending) = mailbox.try_drain() else {
            panic!("mailbox did not drain");
        };
        let width = pending.width.unwrap();
        assert_eq!(width.value, WidthConfig::new(13.0));
        let sequence = gate.lock().unwrap();
        assert_eq!(width.command_id, sequence.global_stamps.width);
        assert_eq!(sequence.config_revision, 2);
    }
}
