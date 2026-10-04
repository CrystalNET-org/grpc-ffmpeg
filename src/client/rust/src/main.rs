//! A gRPC client for executing FFmpeg commands remotely.
//!
//! This client connects to an `FFmpegService` gRPC server, allowing users to
//! execute FFmpeg commands as if they were running locally. It supports SSL/TLS
//! for secure communication, authentication via a bearer token, and a retry
//! mechanism for transient network issues.
//!
//! The arguments are sent as an exact argv list, so values containing spaces,
//! quotes or other special characters reach the remote FFmpeg unchanged. A
//! shell-quoted command string is sent alongside for older servers.
//!
//! The client's stdin is forwarded to the remote process, so e.g. the "q"
//! Jellyfin writes to stop ffmpeg gracefully works as with a local ffmpeg.
//!
//! Settings are read from a `grpc-ffmpeg.conf` file (`KEY=VALUE` lines) next
//! to the client (see [`config_file`]), or the file named by
//! `GRPC_FFMPEG_CONFIG`. Environment variables override the file:
//! - `GRPC_HOST`: Hostname or IP of the gRPC server (default: "ffmpeg-workers")
//! - `GRPC_PORT`: Port of the gRPC server (default: "50051")
//! - `USE_SSL`: "true" or "false" to enable/disable SSL/TLS (default: "false")
//! - `CERTIFICATE_PATH`: Path to the server's TLS certificate if `USE_SSL` is "true" (default: "server.crt")
//! - `AUTH_TOKEN`: Bearer token for authentication (default: "my_secret_token1")
//! - `FALLBACK_DIR`: Directory with local binaries of the same names; used when
//!   no worker is reachable or the token is rejected (default: none)
//! - `RETRIES`: Attempts while no worker is reachable or all are busy (default: 5)
//! - `CONNECT_TIMEOUT`: Seconds to wait for a connection per attempt (default: 10)
//! - `LOG_FILE`: File or named pipe (FIFO) to write an activity log to: each
//!   command with its exit code, the client's own messages, and the end of
//!   ffmpeg's stderr for failed commands (default: none). A FIFO is written
//!   without blocking; lines are dropped while nobody reads it.
//! - `CLASS_ADDRESSES` (experimental): worker pools per hardware class, e.g.
//!   `nvidia=workers-nvidia:50051;intel=workers-intel:50051`. Each command is
//!   classified from its own arguments (see [`hardware_class`]) and sent to its
//!   class's address; commands without hardware arguments, or of a class
//!   without an entry, go to `GRPC_HOST`/`GRPC_PORT` (default: none).

use futures_util::stream::{self, Stream, StreamExt};
use std::collections::HashMap;
use std::env;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, Mutex};
use tokio::time::sleep;
use tonic::metadata::MetadataValue;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Uri};
use tonic::Request;

// Import the generated protobuf and gRPC service definitions.
use ffmpeg::f_fmpeg_service_client::FFmpegServiceClient;
use ffmpeg::{CommandRequest, ExecuteRequest};

/// The `ffmpeg` module contains the generated Rust code from `ffmpeg.proto`.
pub mod ffmpeg {
    tonic::include_proto!("ffmpeg");
}

const CONFIG_FILE_NAME: &str = "grpc-ffmpeg.conf";

/// Size at which the activity log is rotated to `<file>.1`.
const LOG_MAX_BYTES: u64 = 1024 * 1024;
/// How much of ffmpeg's stderr is kept for the activity log.
const STDERR_TAIL_BYTES: usize = 4096;

/// The activity log (see `LOG_FILE`), set up once in `main`.
static ACTIVITY_LOG: OnceLock<Option<ActivityLog>> = OnceLock::new();
/// The end of the remote process's stderr, for the activity log.
static STDERR_TAIL: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());

struct ActivityLog {
    path: PathBuf,
    /// "[pid] ffmpeg", so lines of concurrent commands can be told apart.
    prefix: String,
}

/// Longest wait between two attempts.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(5);
/// After a run found no worker reachable, later runs within this time go to
/// the fallback right away instead of waiting through the retries again (e.g.
/// the dozens of ffmpeg calls of Jellyfin's startup while the workers are down).
const UNREACHABLE_GRACE: Duration = Duration::from_secs(20);

/// Longest line written to the activity log. Writes of up to PIPE_BUF (4096)
/// bytes to a FIFO are atomic, so lines of concurrent commands never mix.
const LOG_MAX_LINE_BYTES: usize = 4000;

/// Appends a line to the activity log, if one is configured. Failures are
/// ignored: logging must never break or slow down a command.
fn log_activity(message: &str) {
    let Some(Some(log)) = ACTIVITY_LOG.get() else {
        return;
    };
    let mut line = format!("{} {} {}", utc_timestamp(), log.prefix, message);
    if line.len() > LOG_MAX_LINE_BYTES {
        let mut cut = LOG_MAX_LINE_BYTES - 3;
        while !line.is_char_boundary(cut) {
            cut -= 1;
        }
        line.truncate(cut);
        line.push('…');
    }
    line.push('\n');

    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
        if std::fs::metadata(&log.path).is_ok_and(|m| m.file_type().is_fifo()) {
            // Non-blocking: fails right away if nobody reads the pipe (ENXIO)
            // or its buffer is full (EAGAIN); the line is dropped then.
            if let Ok(mut pipe) = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&log.path)
            {
                let _ = pipe.write_all(line.as_bytes());
            }
            return;
        }
    }

    if std::fs::metadata(&log.path).is_ok_and(|m| m.len() > LOG_MAX_BYTES) {
        let mut rotated = log.path.clone().into_os_string();
        rotated.push(".1");
        let _ = std::fs::rename(&log.path, rotated);
    }
    // One write per line in append mode keeps concurrent writers' lines whole
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&log.path) {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Prints a message of the client itself to stderr and the activity log.
macro_rules! diag {
    ($($arg:tt)*) => {{
        let message = format!($($arg)*);
        eprintln!("{}", message);
        log_activity(&message);
    }};
}

/// Keeps the last `STDERR_TAIL_BYTES` of the remote stderr.
fn remember_stderr(data: &[u8]) {
    if !matches!(ACTIVITY_LOG.get(), Some(Some(_))) {
        return;
    }
    let mut tail = STDERR_TAIL.lock().unwrap_or_else(|e| e.into_inner());
    tail.extend_from_slice(data);
    let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
    tail.drain(..excess);
}

/// Logs the last lines of the remote stderr, one log line each.
fn log_stderr_tail() {
    let tail = STDERR_TAIL.lock().unwrap_or_else(|e| e.into_inner());
    let text = String::from_utf8_lossy(&tail);
    // ffmpeg ends progress lines with \r
    let lines: Vec<&str> = text
        .split(['\n', '\r'])
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect();
    for line in &lines[lines.len().saturating_sub(10)..] {
        log_activity(&format!("  stderr: {}", line));
    }
}

/// Current UTC time as "YYYY-MM-DD HH:MM:SS".
fn utc_timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        year,
        month,
        day,
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Shortens long command lines (transcodes) for the activity log.
fn preview(args: &[String]) -> String {
    const MAX: usize = 400;
    let line = args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}

/// Client settings.
struct Config {
    host: String,
    port: String,
    use_ssl: bool,
    certificate_path: String,
    auth_token: String,
    /// Whether AUTH_TOKEN was configured, rather than the default being used.
    auth_token_set: bool,
    fallback_dir: Option<PathBuf>,
    log_file: Option<PathBuf>,
    retries: u32,
    connect_timeout: Duration,
    /// Worker address (host, port) per hardware class, see `CLASS_ADDRESSES`.
    class_addresses: HashMap<String, (String, String)>,
}

impl Config {
    /// Reads the config file (if any), with environment variables taking
    /// precedence over its values.
    fn load(argv0: &Path) -> Config {
        let file_values = config_file(argv0)
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(|text| parse_config(&text))
            .unwrap_or_default();
        let get = |key: &str| {
            env::var(key)
                .ok()
                .or_else(|| file_values.get(key).cloned())
                .filter(|value| !value.is_empty())
        };
        let get_or = |key: &str, default: &str| get(key).unwrap_or_else(|| default.to_string());
        let port = get_or("GRPC_PORT", "50051");
        Config {
            host: get_or("GRPC_HOST", "ffmpeg-workers"),
            class_addresses: get("CLASS_ADDRESSES")
                .map(|value| parse_class_addresses(&value, &port))
                .unwrap_or_default(),
            port,
            use_ssl: get_or("USE_SSL", "false").eq_ignore_ascii_case("true"),
            certificate_path: get_or("CERTIFICATE_PATH", "server.crt"),
            auth_token_set: get("AUTH_TOKEN").is_some(),
            auth_token: get_or("AUTH_TOKEN", "my_secret_token1"),
            fallback_dir: get("FALLBACK_DIR").map(PathBuf::from),
            log_file: get("LOG_FILE").map(PathBuf::from),
            retries: get("RETRIES").and_then(|v| v.parse().ok()).unwrap_or(5).max(1),
            connect_timeout: Duration::from_secs(
                get("CONNECT_TIMEOUT").and_then(|v| v.parse().ok()).unwrap_or(10),
            ),
        }
    }
}

/// Finds the config file: `GRPC_FFMPEG_CONFIG` if set, otherwise
/// `grpc-ffmpeg.conf` in the directory the client was invoked from (where e.g.
/// the `ffmpeg` symlink lives), then in the directory of the binary itself.
fn config_file(argv0: &Path) -> Option<PathBuf> {
    if let Some(path) = env::var_os("GRPC_FFMPEG_CONFIG") {
        return Some(PathBuf::from(path));
    }
    let invoked_dir = argv0
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(Path::to_path_buf);
    let binary_dir = env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    invoked_dir
        .into_iter()
        .chain(binary_dir)
        .map(|dir| dir.join(CONFIG_FILE_NAME))
        .find(|path| path.is_file())
}

/// Parses `KEY=VALUE` lines; blank lines and lines starting with `#` are
/// ignored, and values may be wrapped in single or double quotes.
fn parse_config(text: &str) -> HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| {
            let value = value.trim();
            let unquoted = ['"', '\'']
                .iter()
                .find_map(|q| value.strip_prefix(*q).and_then(|v| v.strip_suffix(*q)))
                .unwrap_or(value);
            (key.trim().to_string(), unquoted.to_string())
        })
        .collect()
}

/// Parses `CLASS_ADDRESSES`: `class=host[:port]` entries separated by `;` (or
/// `,`). Class names are case-insensitive; a missing port means `default_port`.
/// Malformed entries are skipped.
fn parse_class_addresses(value: &str, default_port: &str) -> HashMap<String, (String, String)> {
    value
        .split([';', ','])
        .filter_map(|entry| entry.split_once('='))
        .filter_map(|(class, address)| {
            let (class, address) = (class.trim().to_ascii_lowercase(), address.trim());
            // "[::1]:50051", "[::1]", "host:50051", "host"
            let (host, port) = match address.rsplit_once(':') {
                Some((host, port))
                    if !port.is_empty()
                        && port.chars().all(|c| c.is_ascii_digit())
                        && (!host.contains(':') || host.ends_with(']')) =>
                {
                    (host, port)
                }
                _ => (address, default_port),
            };
            let host = host.trim_start_matches('[').trim_end_matches(']');
            (!class.is_empty() && !host.is_empty()).then(|| (class, (host.to_string(), port.to_string())))
        })
        .collect()
}

/// The hardware class a command needs, from the devices it initializes:
/// `-init_hw_device cuda=…` is "nvidia"; `qsv=…`, or `vaapi=…` with an Intel
/// driver (`driver=iHD`/`i965`, as Jellyfin derives QSV from VAAPI on Linux), is
/// "intel". Without a hardware device, an `*_nvenc`/`*_qsv` encoder decides.
/// Anything else (software commands, probes, AMD VAAPI) has no class.
fn hardware_class(args: &[String]) -> Option<&'static str> {
    let mut class = None;
    for pair in args.windows(2) {
        if pair[0] != "-init_hw_device" {
            continue;
        }
        let (kind, options) = pair[1].split_once('=').unwrap_or((pair[1].as_str(), ""));
        match kind {
            // The CUDA device is what needs the NVIDIA GPU, whatever else is set up
            "cuda" => return Some("nvidia"),
            "qsv" => class = Some("intel"),
            "vaapi" if options.contains("driver=iHD") || options.contains("driver=i965") => {
                class = Some("intel")
            }
            _ => {}
        }
    }
    class.or_else(|| {
        let is_codec_option = |option: &str| {
            option == "-c" || option == "-vcodec" || option.starts_with("-c:") || option.starts_with("-codec")
        };
        args.windows(2).find_map(|pair| {
            let (option, arg) = (&pair[0], &pair[1]);
            if !is_codec_option(option) {
                None
            } else if arg.ends_with("_nvenc") || arg.ends_with("_cuvid") {
                Some("nvidia")
            } else if arg.ends_with("_qsv") {
                Some("intel")
            } else {
                None
            }
        })
    })
}

/// Quotes an argument for a POSIX shell, identical to Python's `shlex.quote`,
/// so the server's `shlex.split` recovers the exact argument.
fn shell_quote(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }
    let is_safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if arg.chars().all(is_safe) {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\"'\"'"))
}

/// Marker file whose modification time records when the workers at this
/// address were last found unreachable.
fn unreachable_marker(config: &Config) -> PathBuf {
    let address: String = format!("{}_{}", config.host, config.port)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect();
    env::temp_dir().join(format!("grpc-ffmpeg-unreachable-{}", address))
}

/// How long ago the workers were found unreachable, if within `UNREACHABLE_GRACE`.
fn recently_unreachable(marker: &Path) -> Option<Duration> {
    let age = std::fs::metadata(marker).ok()?.modified().ok()?.elapsed().ok()?;
    (age <= UNREACHABLE_GRACE).then_some(age)
}

/// Delay before attempt `attempt + 1`: 1s, 2s, 4s, ..., at most `MAX_RETRY_DELAY`.
fn retry_delay(attempt: u32) -> Duration {
    (Duration::from_secs(1) * 2u32.saturating_pow(attempt)).min(MAX_RETRY_DELAY)
}

/// Builds a lazily connecting channel. Connection failures surface as
/// `Unavailable` on the first call, which lets the retry loop handle a server
/// that is not up yet.
async fn build_channel(config: &Config) -> Result<Channel, anyhow::Error> {
    let scheme = if config.use_ssl { "https" } else { "http" };
    let target_uri: Uri = format!("{}://{}:{}", scheme, config.host, config.port).parse()?;

    let mut endpoint = Channel::builder(target_uri)
        .connect_timeout(config.connect_timeout)
        .tcp_keepalive(Some(Duration::from_secs(60)))
        // Detect a dead server on long-running, quiet streams. The interval
        // matches the minimum gRPC servers accept by default.
        .http2_keep_alive_interval(Duration::from_secs(300))
        .keep_alive_timeout(Duration::from_secs(20));

    if config.use_ssl {
        let cert_path = &config.certificate_path;
        let pem = tokio::fs::read(cert_path)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read certificate {}: {}", cert_path, e))?;
        let tls_config = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(pem))
            .domain_name(config.host.clone());
        endpoint = endpoint.tls_config(tls_config)?;
    }

    Ok(endpoint.connect_lazy())
}

/// Local stdin, read on a background thread and shared by all attempts. The
/// thread only starts once a request body is actually sent, so nothing is
/// consumed from stdin if the command ends up running locally instead.
type StdinReceiver = Arc<OnceLock<Mutex<mpsc::Receiver<Vec<u8>>>>>;

/// Reads stdin on a dedicated thread (a blocking read cannot be cancelled)
/// and forwards it with backpressure. EOF closes the channel.
fn spawn_stdin_reader() -> Mutex<mpsc::Receiver<Vec<u8>>> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>(16);
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
    Mutex::new(rx)
}

/// The request stream for `Execute`: the command, followed by stdin data.
/// The stream ends (half-closing the call) when stdin reaches EOF.
fn execute_requests(
    request: CommandRequest,
    stdin: StdinReceiver,
) -> impl Stream<Item = ExecuteRequest> + Send + 'static {
    let first = ExecuteRequest {
        request: Some(request),
        stdin: Vec::new(),
    };
    stream::once(async move { first }).chain(stream::unfold(stdin, |stdin| async move {
        let data = stdin
            .get_or_init(spawn_stdin_reader)
            .lock()
            .await
            .recv()
            .await?;
        Some((
            ExecuteRequest {
                request: None,
                stdin: data,
            },
            stdin,
        ))
    }))
}

/// How a remote run ended.
enum Outcome {
    /// The command ran (or failed) remotely with this exit code.
    Exited(i32),
    /// The command never started remotely and should run on the local
    /// fallback, for the given reason.
    Fallback(String),
}

/// Executes the command on the remote server.
///
/// Retries with exponential backoff while the server is unavailable or all
/// its ffmpeg slots are busy, but only until the command has started:
/// re-running a partially streamed command would duplicate its output. Each
/// attempt opens a new connection, so behind a load balancer a retry can land
/// on a different worker.
async fn run_command(args: Vec<String>, config: &Config) -> Result<Outcome, anyhow::Error> {
    let marker = unreachable_marker(config);
    if config.fallback_dir.is_some() {
        if let Some(age) = recently_unreachable(&marker) {
            return Ok(Outcome::Fallback(format!(
                "workers unreachable {}s ago",
                age.as_secs()
            )));
        }
    }

    let token: MetadataValue<_> = format!("Bearer {}", config.auth_token).parse()?;
    let auth = move |mut req: Request<()>| {
        req.metadata_mut().insert("authorization", token.clone());
        Ok(req)
    };

    let command = args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
    let stdin: StdinReceiver = Arc::new(OnceLock::new());
    // Servers without the Execute RPC do not support stdin forwarding.
    let mut forward_stdin = true;

    let max_retries = config.retries;
    let mut attempt = 0;

    loop {
        let channel = build_channel(config).await?;
        let mut client = FFmpegServiceClient::with_interceptor(channel, auth.clone());
        let request = CommandRequest {
            command: command.clone(),
            args: args.clone(),
            raw_stderr: true,
        };

        let response = if forward_stdin {
            client.execute(execute_requests(request, stdin.clone())).await
        } else {
            client.execute_command(Request::new(request)).await
        };
        let result = match response {
            Ok(response) => {
                let mut stream = response.into_inner();
                match stream.message().await {
                    // The command has started; from here on errors are final.
                    Ok(Some(first)) => {
                        let _ = std::fs::remove_file(&marker);
                        return stream_output(first, &mut stream).await.map(Outcome::Exited);
                    }
                    Ok(None) => {
                        diag!("Server closed the stream without reporting an exit code");
                        return Ok(Outcome::Exited(1));
                    }
                    Err(status) => status,
                }
            }
            Err(status) => status,
        };

        if forward_stdin && result.code() == tonic::Code::Unimplemented {
            forward_stdin = false;
            continue;
        }
        let retryable = matches!(
            result.code(),
            tonic::Code::Unavailable | tonic::Code::ResourceExhausted
        );
        if retryable && attempt < max_retries - 1 {
            let delay = retry_delay(attempt);
            diag!(
                "{}, retrying in {:.1} seconds... (Attempt {}/{})",
                if result.code() == tonic::Code::Unavailable {
                    "Server unavailable"
                } else {
                    "Server busy"
                },
                delay.as_secs_f32(),
                attempt + 1,
                max_retries
            );
            sleep(delay).await;
            attempt += 1;
            continue;
        }
        if result.code() == tonic::Code::Unauthenticated && !config.auth_token_set {
            // e.g. a broken secret reference; easy to miss as callers like
            // Jellyfin do not show ffmpeg's stderr
            diag!("AUTH_TOKEN is not set, so the default token was sent");
        }
        if config.fallback_dir.is_some() {
            match result.code() {
                tonic::Code::Unavailable => {
                    let _ = std::fs::write(&marker, b"");
                    return Ok(Outcome::Fallback("workers unreachable".to_string()));
                }
                // A wrong token would otherwise keep Jellyfin from starting
                tonic::Code::Unauthenticated | tonic::Code::PermissionDenied => {
                    return Ok(Outcome::Fallback(format!(
                        "token rejected by the worker: {}",
                        result.message()
                    )));
                }
                _ => {}
            }
        }
        diag!(
            "gRPC error after {} attempts: {:?}: {}",
            attempt + 1,
            result.code(),
            result.message()
        );
        return Ok(Outcome::Exited(1));
    }
}

/// Runs the command with the local binary of the same name in `dir`. On Unix
/// the client process is replaced, so stdin, output, signals and the exit
/// status behave exactly as if the local binary had been run directly.
fn run_locally(dir: &Path, name: &str, args: &[String], reason: &str) -> i32 {
    let mut path = dir.join(name);
    if cfg!(windows) && !path.is_file() {
        path.set_extension("exe");
    }
    if !path.is_file() {
        diag!("fallback ({}): no local {} at {}", reason, name, path.display());
        return 1;
    }
    diag!("fallback ({}): running {} locally", reason, path.display());
    let mut command = Command::new(&path);
    command.args(args);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        diag!("Failed to run {}: {}", path.display(), error);
        1
    }
    #[cfg(not(unix))]
    {
        match command.status() {
            Ok(status) => status.code().unwrap_or(1),
            Err(error) => {
                diag!("Failed to run {}: {}", path.display(), error);
                1
            }
        }
    }
}

/// Writes the streamed responses to stdout/stderr and returns the exit code.
async fn stream_output(
    first: ffmpeg::CommandResponse,
    stream: &mut tonic::Streaming<ffmpeg::CommandResponse>,
) -> Result<i32, anyhow::Error> {
    // Stays 1 if the server never reports an exit code.
    let mut exit_code = 1;
    let mut next = Some(first);
    while let Some(res) = next {
        if !res.binary_output.is_empty() {
            if res.stream == "stderr" {
                remember_stderr(&res.binary_output);
                let mut stderr = io::stderr().lock();
                stderr.write_all(&res.binary_output)?;
                stderr.flush()?;
            } else {
                let mut stdout = io::stdout().lock();
                stdout.write_all(&res.binary_output)?;
                stdout.flush()?;
            }
        } else if !res.output.is_empty() {
            match res.stream.as_str() {
                "stdout" => {
                    let mut stdout = io::stdout().lock();
                    stdout.write_all(res.output.as_bytes())?;
                    stdout.flush()?;
                }
                "stderr" => {
                    remember_stderr(res.output.as_bytes());
                    let mut stderr = io::stderr().lock();
                    stderr.write_all(res.output.as_bytes())?;
                    stderr.flush()?;
                }
                _ => {} // Ignore unknown stream types.
            }
        } else if res.stream == "exit_code" {
            exit_code = res.exit_code;
        }

        next = match stream.message().await {
            Ok(message) => message,
            Err(status) => {
                diag!(
                    "gRPC stream failed: {:?}: {}",
                    status.code(),
                    status.message()
                );
                return Ok(1);
            }
        };
    }
    Ok(exit_code)
}

/// Main entry point of the gRPC FFmpeg client.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Every argument is passed through untouched; ffmpeg flags such as `-i`
    // or `-version` must not be interpreted by this client.
    let mut argv = env::args_os().map(|a| a.to_string_lossy().into_owned());
    let argv0 = argv.next().unwrap_or_default();

    let mut config = Config::load(Path::new(&argv0));

    // BusyBox-like command detection: the name the client is invoked as
    // (e.g. a symlink named "ffmpeg" or "ffprobe") is the remote binary.
    // argv[0] is used rather than current_exe(), which resolves symlinks.
    let command_from_exe = std::path::Path::new(&argv0)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("grpc-ffmpeg-client")
        .to_string();

    let args: Vec<String> = argv.collect();
    let mut full_command = vec![command_from_exe.clone()];
    full_command.extend(args.iter().cloned());

    let _ = ACTIVITY_LOG.set(config.log_file.clone().map(|path| ActivityLog {
        path,
        prefix: format!("[{}] {}", std::process::id(), command_from_exe),
    }));
    // Route to the worker pool of the command's hardware class, if one is set.
    // Retries, the unreachable marker and the fallback then apply to that address.
    let class = hardware_class(&args);
    if let Some((host, port)) = class.and_then(|class| config.class_addresses.get(class)).cloned() {
        config.host = host;
        config.port = port;
    }
    if config.class_addresses.is_empty() {
        log_activity(&format!("run: {}", preview(&full_command)));
    } else {
        log_activity(&format!(
            "run [{} {}:{}]: {}",
            class.unwrap_or("default"),
            config.host,
            config.port,
            preview(&full_command)
        ));
    }
    let started = Instant::now();

    // Execute the remote command and exit with the received exit code.
    match run_command(full_command, &config).await {
        Ok(Outcome::Exited(exit_code)) => {
            log_activity(&format!(
                "exit {} after {:.1}s",
                exit_code,
                started.elapsed().as_secs_f32()
            ));
            if exit_code != 0 {
                log_stderr_tail();
            }
            if exit_code < 0 {
                exit_by_signal(-exit_code)
            }
            std::process::exit(exit_code)
        }
        Ok(Outcome::Fallback(reason)) => {
            let dir = config.fallback_dir.as_deref().unwrap_or(Path::new("."));
            std::process::exit(run_locally(dir, &command_from_exe, &args, &reason))
        }
        Err(e) => {
            diag!("An unexpected error occurred: {}", e);
            std::process::exit(1);
        }
    }
}

/// The remote process was killed by a signal; die the same way so the caller
/// sees the same status as for a local process.
fn exit_by_signal(signal: i32) -> ! {
    #[cfg(unix)]
    // SAFETY: resetting a signal to its default action and raising it has no
    // memory-safety preconditions.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
    // Not reached for fatal signals; mirror the shell's convention otherwise.
    std::process::exit(128 + signal)
}

#[cfg(test)]
mod tests {
    use super::{
        hardware_class, parse_class_addresses, parse_config, retry_delay, shell_quote, MAX_RETRY_DELAY,
    };
    use std::time::Duration;

    fn argv(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn classifies_jellyfin_nvenc_commands() {
        // EncodingHelper.GetCudaDeviceArgs + GetFilterHwDeviceArgs
        let args = argv("-analyzeduration 200M -init_hw_device cuda=cu:0 -filter_hw_device cu -hwaccel cuda -hwaccel_output_format cuda -i file:/media/a.mkv -c:v h264_nvenc out.m3u8");
        assert_eq!(hardware_class(&args), Some("nvidia"));
        // CUDA next to a Vulkan device (tonemapping) is still NVIDIA
        let args = argv("-init_hw_device cuda=cu:0 -init_hw_device vulkan=vk@cu -i a.mkv");
        assert_eq!(hardware_class(&args), Some("nvidia"));
    }

    #[test]
    fn classifies_jellyfin_qsv_commands() {
        // EncodingHelper.GetQsvDeviceArgs on Linux, with and without the render node
        let args = argv("-init_hw_device vaapi=va:/dev/dri/renderD128,driver=iHD -init_hw_device qsv=qs@va -filter_hw_device qs -hwaccel vaapi -i a.mkv -c:v h264_qsv out.m3u8");
        assert_eq!(hardware_class(&args), Some("intel"));
        let args = argv("-init_hw_device vaapi=va:,vendor_id=0x8086,driver=iHD -init_hw_device qsv=qs@va -i a.mkv");
        assert_eq!(hardware_class(&args), Some("intel"));
        // Intel VAAPI with an explicit Intel driver
        let args = argv("-init_hw_device vaapi=va:/dev/dri/renderD128,driver=i965 -i a.mkv -c:v h264_vaapi o.ts");
        assert_eq!(hardware_class(&args), Some("intel"));
    }

    #[test]
    fn leaves_other_commands_unclassified() {
        // Probes, software transcodes and AMD VAAPI (no driver= given) go to the default address
        assert_eq!(hardware_class(&argv("-version")), None);
        assert_eq!(hardware_class(&argv("-hide_banner -encoders")), None);
        assert_eq!(hardware_class(&argv("-i file:/media/a_qsv.mkv -c:v libx264 out.mp4")), None);
        assert_eq!(
            hardware_class(&argv("-init_hw_device vaapi=va:/dev/dri/renderD128 -i a.mkv -c:v h264_vaapi o.ts")),
            None
        );
        assert_eq!(hardware_class(&argv("-init_hw_device")), None);
    }

    #[test]
    fn classifies_by_encoder_without_device() {
        assert_eq!(hardware_class(&argv("-i a.mkv -c:v hevc_nvenc o.mp4")), Some("nvidia"));
        assert_eq!(hardware_class(&argv("-c:v h264_cuvid -i a.mkv o.mp4")), Some("nvidia"));
        assert_eq!(hardware_class(&argv("-i a.mkv -vcodec mjpeg_qsv o.jpg")), Some("intel"));
    }

    #[test]
    fn parses_class_addresses() {
        let map = parse_class_addresses(
            " NVIDIA = workers-nvidia:50052 ; intel=workers-intel,amd=[fd00::1]:7000;v6=[::1];broken;=x:1;empty=",
            "50051",
        );
        assert_eq!(map["nvidia"], ("workers-nvidia".to_string(), "50052".to_string()));
        assert_eq!(map["intel"], ("workers-intel".to_string(), "50051".to_string()));
        assert_eq!(map["amd"], ("fd00::1".to_string(), "7000".to_string()));
        assert_eq!(map["v6"], ("::1".to_string(), "50051".to_string()));
        assert_eq!(map.len(), 4);
    }

    #[test]
    fn retry_delay_doubles_up_to_the_maximum() {
        assert_eq!(retry_delay(0), Duration::from_secs(1));
        assert_eq!(retry_delay(1), Duration::from_secs(2));
        assert_eq!(retry_delay(2), Duration::from_secs(4));
        assert_eq!(retry_delay(3), MAX_RETRY_DELAY);
        assert_eq!(retry_delay(40), MAX_RETRY_DELAY);
    }

    #[test]
    fn parses_config_file() {
        let config = parse_config(
            "# comment\n\nGRPC_HOST = worker.lan\nAUTH_TOKEN=\"a=b c\"\nFALLBACK_DIR='/usr/lib/jellyfin-ffmpeg'\ninvalid line\n",
        );
        assert_eq!(config["GRPC_HOST"], "worker.lan");
        assert_eq!(config["AUTH_TOKEN"], "a=b c");
        assert_eq!(config["FALLBACK_DIR"], "/usr/lib/jellyfin-ffmpeg");
        assert_eq!(config.len(), 3);
    }

    #[test]
    fn quotes_like_python_shlex() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("file:/media/a.mkv"), "file:/media/a.mkv");
        assert_eq!(shell_quote("-c:v"), "-c:v");
        assert_eq!(shell_quote("/a b/c.mkv"), "'/a b/c.mkv'");
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
        assert_eq!(shell_quote("[0:v]scale=1:2"), "'[0:v]scale=1:2'");
    }
}
