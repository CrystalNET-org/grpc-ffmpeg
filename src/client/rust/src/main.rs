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
//! Configuration is primarily done through environment variables:
//! - `GRPC_HOST`: Hostname or IP of the gRPC server (default: "ffmpeg-workers")
//! - `GRPC_PORT`: Port of the gRPC server (default: "50051")
//! - `USE_SSL`: "true" or "false" to enable/disable SSL/TLS (default: "false")
//! - `CERTIFICATE_PATH`: Path to the server's TLS certificate if `USE_SSL` is "true" (default: "server.crt")
//! - `AUTH_TOKEN`: Bearer token for authentication (default: "my_secret_token1")

use futures_util::stream::{self, Stream, StreamExt};
use std::env;
use std::io::{self, Read, Write};
use std::sync::Arc;
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
async fn build_channel(use_ssl: bool) -> Result<Channel, anyhow::Error> {
    let grpc_host = env::var("GRPC_HOST").unwrap_or_else(|_| "ffmpeg-workers".to_string());
    let grpc_port = env::var("GRPC_PORT").unwrap_or_else(|_| "50051".to_string());
    let scheme = if use_ssl { "https" } else { "http" };
    let target_uri: Uri = format!("{}://{}:{}", scheme, grpc_host, grpc_port).parse()?;

    let mut endpoint = Channel::builder(target_uri)
        .connect_timeout(Duration::from_secs(10))
        .tcp_keepalive(Some(Duration::from_secs(60)))
        // Detect a dead server on long-running, quiet streams. The interval
        // matches the minimum gRPC servers accept by default.
        .http2_keep_alive_interval(Duration::from_secs(300))
        .keep_alive_timeout(Duration::from_secs(20));

    if use_ssl {
        let cert_path = env::var("CERTIFICATE_PATH").unwrap_or_else(|_| "server.crt".to_string());
        let pem = tokio::fs::read(&cert_path)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read certificate {}: {}", cert_path, e))?;
        let tls_config = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(pem))
            .domain_name(grpc_host);
        endpoint = endpoint.tls_config(tls_config)?;
    }

    Ok(endpoint.connect_lazy())
}

/// Local stdin, read on a background thread and shared by all attempts.
type StdinReceiver = Arc<Mutex<mpsc::Receiver<Vec<u8>>>>;

/// Reads stdin on a dedicated thread (a blocking read cannot be cancelled)
/// and forwards it with backpressure. EOF closes the channel.
fn spawn_stdin_reader() -> StdinReceiver {
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
    Arc::new(Mutex::new(rx))
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
        let data = stdin.lock().await.recv().await?;
        Some((
            ExecuteRequest {
                request: None,
                stdin: data,
            },
            stdin,
        ))
    }))
}

/// Executes the command on the remote server and returns its exit code.
///
/// Retries with exponential backoff while the server is unavailable or all
/// its ffmpeg slots are busy, but only until the command has started:
/// re-running a partially streamed command would duplicate its output. Each
/// attempt opens a new connection, so behind a load balancer a retry can land
/// on a different worker.
async fn run_command(args: Vec<String>, use_ssl: bool) -> Result<i32, anyhow::Error> {
    let auth_token = env::var("AUTH_TOKEN").unwrap_or_else(|_| "my_secret_token1".to_string());
    let token: MetadataValue<_> = format!("Bearer {}", auth_token).parse()?;
    let auth = move |mut req: Request<()>| {
        req.metadata_mut().insert("authorization", token.clone());
        Ok(req)
    };

    let command = args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
    let stdin = spawn_stdin_reader();
    // Servers without the Execute RPC do not support stdin forwarding.
    let mut forward_stdin = true;

    let max_retries = 5;
    let base_delay = Duration::from_secs(1);
    let mut attempt = 0;

    loop {
        let channel = build_channel(use_ssl).await?;
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
                    Ok(Some(first)) => return stream_output(first, &mut stream).await,
                    Ok(None) => {
                        eprintln!("Server closed the stream without reporting an exit code");
                        return Ok(1);
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
            let delay = base_delay * 2u32.pow(attempt as u32);
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
        eprintln!(
            "gRPC error after {} attempts: {:?}: {}",
            attempt + 1,
            result.code(),
            result.message()
        );
        return Ok(1);
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

    let use_ssl = env::var("USE_SSL").unwrap_or_else(|_| "false".to_string()).to_lowercase() == "true";

    // BusyBox-like command detection: the name the client is invoked as
    // (e.g. a symlink named "ffmpeg" or "ffprobe") is the remote binary.
    // argv[0] is used rather than current_exe(), which resolves symlinks.
    let command_from_exe = std::path::Path::new(&argv0)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("grpc-ffmpeg-client")
        .to_string();

    let mut full_command = vec![command_from_exe];
    full_command.extend(argv);

    // Execute the remote command and exit with the received exit code.
    match run_command(full_command, use_ssl).await {
        Ok(exit_code) if exit_code < 0 => exit_by_signal(-exit_code),
        Ok(exit_code) => std::process::exit(exit_code),
        Err(e) => {
            eprintln!("An unexpected error occurred: {}", e);
            std::process::exit(1);
        }
    }
}

/// The remote process was killed by a signal; die the same way so the caller
/// sees the same status as for a local process.
fn exit_by_signal(signal: i32) -> ! {
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
    use super::shell_quote;

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
