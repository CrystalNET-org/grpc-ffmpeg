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
//!   no worker is reachable (default: none)
//! - `RETRIES`: Attempts while no worker is reachable or all are busy (default: 5)
//! - `CONNECT_TIMEOUT`: Seconds to wait for a connection per attempt (default: 10)

use futures_util::stream::{self, Stream, StreamExt};
use std::collections::HashMap;
use std::env;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
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

/// Client settings.
struct Config {
    host: String,
    port: String,
    use_ssl: bool,
    certificate_path: String,
    auth_token: String,
    fallback_dir: Option<PathBuf>,
    retries: u32,
    connect_timeout: Duration,
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
        Config {
            host: get_or("GRPC_HOST", "ffmpeg-workers"),
            port: get_or("GRPC_PORT", "50051"),
            use_ssl: get_or("USE_SSL", "false").eq_ignore_ascii_case("true"),
            certificate_path: get_or("CERTIFICATE_PATH", "server.crt"),
            auth_token: get_or("AUTH_TOKEN", "my_secret_token1"),
            fallback_dir: get("FALLBACK_DIR").map(PathBuf::from),
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
    /// No worker could be reached; the command never started.
    Unreachable,
}

/// Executes the command on the remote server.
///
/// Retries with exponential backoff while the server is unavailable or all
/// its ffmpeg slots are busy, but only until the command has started:
/// re-running a partially streamed command would duplicate its output. Each
/// attempt opens a new connection, so behind a load balancer a retry can land
/// on a different worker.
async fn run_command(args: Vec<String>, config: &Config) -> Result<Outcome, anyhow::Error> {
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
    let base_delay = Duration::from_secs(1);
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
                        return stream_output(first, &mut stream).await.map(Outcome::Exited)
                    }
                    Ok(None) => {
                        eprintln!("Server closed the stream without reporting an exit code");
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
            let delay = base_delay * 2u32.pow(attempt);
            eprintln!(
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
        if result.code() == tonic::Code::Unavailable && config.fallback_dir.is_some() {
            return Ok(Outcome::Unreachable);
        }
        eprintln!(
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
fn run_locally(dir: &Path, name: &str, args: &[String]) -> i32 {
    let mut path = dir.join(name);
    if cfg!(windows) && !path.is_file() {
        path.set_extension("exe");
    }
    if !path.is_file() {
        eprintln!(
            "No worker reachable and no local fallback at {}",
            path.display()
        );
        return 1;
    }
    eprintln!("No worker reachable, running {} locally", path.display());
    let mut command = Command::new(&path);
    command.args(args);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        eprintln!("Failed to run {}: {}", path.display(), error);
        1
    }
    #[cfg(not(unix))]
    {
        match command.status() {
            Ok(status) => status.code().unwrap_or(1),
            Err(error) => {
                eprintln!("Failed to run {}: {}", path.display(), error);
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
                eprintln!(
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

    let config = Config::load(Path::new(&argv0));

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

    // Execute the remote command and exit with the received exit code.
    match run_command(full_command, &config).await {
        Ok(Outcome::Exited(exit_code)) if exit_code < 0 => exit_by_signal(-exit_code),
        Ok(Outcome::Exited(exit_code)) => std::process::exit(exit_code),
        Ok(Outcome::Unreachable) => {
            let dir = config.fallback_dir.as_deref().unwrap_or(Path::new("."));
            std::process::exit(run_locally(dir, &command_from_exe, &args))
        }
        Err(e) => {
            eprintln!("An unexpected error occurred: {}", e);
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
    use super::{parse_config, shell_quote};

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
