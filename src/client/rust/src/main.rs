//! A gRPC client for executing FFmpeg commands remotely.
//!
//! This client connects to an `FFmpegService` gRPC server, allowing users to
//! execute FFmpeg commands as if they were running locally. It supports SSL/TLS
//! for secure communication, authentication via a bearer token, and a retry
//! mechanism for transient network issues.
//!
//! The client also includes logic to properly quote FFmpeg arguments that
//! might contain spaces or special characters, mirroring the behavior of
//! the Python client.
//!
//! Configuration is primarily done through environment variables:
//! - `GRPC_HOST`: Hostname or IP of the gRPC server (default: "ffmpeg-workers")
//! - `GRPC_PORT`: Port of the gRPC server (default: "50051")
//! - `USE_SSL`: "true" or "false" to enable/disable SSL/TLS (default: "false")
//! - `CERTIFICATE_PATH`: Path to the server's TLS certificate if `USE_SSL` is "true" (default: "server.crt")
//! - `AUTH_TOKEN`: Bearer token for authentication (default: "my_secret_token1")

use clap::Parser;
use std::env;
use std::io::{self, Write};
use std::time::Duration;
use tokio::time::sleep;
use tonic::transport::{Certificate, Channel, ClientTlsConfig};
use tonic::metadata::MetadataValue;
use tonic::Request;

// Import the generated protobuf and gRPC service definitions.
use ffmpeg::f_fmpeg_service_client::FFmpegServiceClient;
use ffmpeg::CommandRequest;

/// The `ffmpeg` module contains the generated Rust code from `ffmpeg.proto`.
pub mod ffmpeg {
    tonic::include_proto!("ffmpeg");
}

/// Command-line arguments for the gRPC FFmpeg client.
///
/// The `command` field captures all arguments passed after the client executable itself,
/// which are then treated as the FFmpeg command and its parameters.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The FFmpeg command and its arguments to be executed on the remote server.
    #[arg(required = true, num_args = 1..)]
    command: Vec<String>,
}

/// Handles quoting of specific FFmpeg arguments that might contain special characters.
///
/// This function iterates through the provided command arguments and, for certain
/// known FFmpeg parameters (like `-filter_complex` or `file:` inputs), it
/// encloses their values in double quotes if they contain spaces or other
/// problematic characters. This prevents misinterpretation by the remote FFmpeg
/// process.
///
/// This logic mirrors the `handle_quoted_arguments` function in the Python client.
fn handle_quoted_arguments(command_args: &[String]) -> Vec<String> {
    // List of FFmpeg parameters whose values might need quoting.
    let parameters_to_quote = &["-filter_complex", "-vf", "-hls_segment_filename", "-user_agent"];
    // Characters that necessitate quoting if present in an argument's value.
    let chars_that_need_quoting: &[char] = &[' ', ',', ':', '(', ')'];
    
    let mut rffmpeg_command = Vec::new();
    let mut i = 0;

    // Iterate through the command arguments.
    while i < command_args.len() {
        let arg = &command_args[i];

        // Special handling for `-i file:` arguments.
        // If the argument is `-i` and the next argument starts with `file:`,
        // extract the file path and quote it if necessary.
        if *arg == "-i" && i + 1 < command_args.len() && command_args[i + 1].starts_with("file:") {
            let file_path_arg = &command_args[i + 1];
            let file_path = &file_path_arg[5..]; // Extract path after "file:"

            if file_path.chars().any(|c| chars_that_need_quoting.contains(&c)) {
                rffmpeg_command.push(arg.to_string());
                rffmpeg_command.push(format!("file:\"{}\"", file_path));
            } else {
                rffmpeg_command.push(arg.to_string());
                rffmpeg_command.push(file_path_arg.to_string());
            }
            i += 2; // Skip the next argument as it's part of the `-i file:` pair.
        } 
        // Handling for other parameters that require quoting their values.
        // If the current argument is in `parameters_to_quote` and there's a subsequent argument,
        // quote the subsequent argument if it contains special characters.
        else if parameters_to_quote.contains(&arg.as_str()) && i + 1 < command_args.len() {
            let next_arg = &command_args[i + 1];

            if next_arg.chars().any(|c| chars_that_need_quoting.contains(&c)) {
                rffmpeg_command.push(arg.to_string());
                rffmpeg_command.push(format!("\"{}\"", next_arg));
            } else {
                rffmpeg_command.push(arg.to_string());
                rffmpeg_command.push(next_arg.to_string());
            }
            i += 2; // Skip the next argument as it's the value for the current parameter.
        } 
        // For all other arguments, just append them as they are.
        else {
            rffmpeg_command.push(arg.to_string());
            i += 1;
        }
    }

    rffmpeg_command
}

/// Establishes a gRPC connection to the FFmpeg service and executes the given command.
///
/// This function handles:
/// - Reading gRPC host, port, SSL settings, and authentication token from environment variables.
/// - Setting up secure (TLS) or insecure channels.
/// - Adding an authorization interceptor for the bearer token *only* when SSL is used.
/// - Implementing a retry mechanism with exponential backoff for `Unavailable` errors.
/// - Streaming command responses and writing stdout/stderr/binary output to the console.
/// - Returning the exit code received from the remote FFmpeg process.
async fn run_command(command: String, use_ssl: bool) -> Result<i32, anyhow::Error> {
    // Retrieve configuration from environment variables.
    let grpc_host = env::var("GRPC_HOST").unwrap_or_else(|_| "ffmpeg-workers".to_string());
    let grpc_port = env::var("GRPC_PORT").unwrap_or_else(|_| "50051".to_string());
    // Construct the target URI for the gRPC service.
    let target = format!("https://{}:{}", grpc_host, grpc_port);

    // Retrieve authentication token. It will only be used if SSL is active.
    let auth_token = env::var("AUTH_TOKEN").unwrap_or_else(|_| "my_secret_token1".to_string());
    let token: MetadataValue<_> = format!("Bearer {}", auth_token).parse()?;

    // Configure the gRPC channel based on SSL settings.
    let channel = if use_ssl {
        // If SSL is enabled, load the server certificate.
        let cert_path = env::var("CERTIFICATE_PATH").unwrap_or_else(|_| "server.crt".to_string());
        let pem = tokio::fs::read(cert_path).await?;
        let ca = Certificate::from_pem(pem);
        let tls_config = ClientTlsConfig::new().ca_certificate(ca);
        Channel::from_static(&target) // Use `from_static` for HTTPS targets.
            .tls_config(tls_config)?
            .connect()
            .await?
    } else {
        // If SSL is disabled, use an insecure channel.
        Channel::from_shared(target.replace("https://", "http://"))? // Replace https with http for insecure.
            .connect()
            .await?
    };
    
    // Retry mechanism parameters.
    let max_retries = 5;
    let base_delay = Duration::from_secs_f32(1.0);

    // Loop for retrying the gRPC call.
    for attempt in 0..max_retries {
        // Create a gRPC client with an interceptor to add the authorization header.
        // The token is now sent regardless of SSL status, as per user's request.
        let mut client = FFmpegServiceClient::with_interceptor(channel.clone(), move |mut req: Request<()>| {
            req.metadata_mut().insert("authorization", token.clone());
            Ok(req)
        });

        // Create the gRPC request with the command string.
        let request = Request::new(CommandRequest {
            command: command.clone(),
        });

        // Execute the gRPC command.
        match client.execute_command(request).await {
            Ok(response) => {
                let mut stream = response.into_inner();
                let mut exit_code = 0;

                // Process the streaming responses from the server.
                while let Some(res) = stream.message().await? {
                    // Handle binary output (e.g., raw media data).
                    if !res.binary_output.is_empty() {
                        io::stdout().write_all(&res.binary_output)?;
                        io::stdout().flush()?;
                    } 
                    // Handle text output (stdout/stderr).
                    else if !res.output.is_empty() {
                        match res.stream.as_str() {
                            "stdout" => {
                                print!("{}", res.output);
                                io::stdout().flush()?;
                            }
                            "stderr" => {
                                eprint!("{}", res.output);
                                io::stderr().flush()?;
                            }
                            _ => {} // Ignore unknown stream types.
                        }
                    }
                    // Capture the final exit code.
                    if res.stream == "exit_code" {
                        exit_code = res.exit_code;
                    }
                }
                return Ok(exit_code); // Command completed successfully.
            }
            Err(e) => {
                // Handle gRPC errors, especially "Unavailable" for retries.
                if e.code() == tonic::Code::Unavailable && attempt < max_retries - 1 {
                    let delay = base_delay * 2u32.pow(attempt as u32); // Exponential backoff.
                    eprintln!(
                        "Server unavailable, retrying in {:.1} seconds... (Attempt {}/{})",
                        delay.as_secs_f32(),
                        attempt + 1,
                        max_retries
                    );
                    sleep(delay).await;
                    continue; // Retry the command.
                } else {
                    // For other errors or after max retries, print the error and exit.
                    eprintln!("gRPC error after {} attempts: {}", attempt + 1, e.message());
                    return Ok(1); // Indicate failure.
                }
            }
        }
    }

    eprintln!("Command failed after reaching max retries.");
    Ok(1) // Indicate failure if all retries are exhausted.
}

/// Main entry point of the gRPC FFmpeg client.
///
/// Parses command-line arguments, determines SSL usage, processes arguments
/// for quoting, and then executes the remote FFmpeg command via gRPC.
#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // Parse command-line arguments using clap.
    let args = Args::parse();
    
    // Determine if SSL should be used based on the USE_SSL environment variable.
    let use_ssl = env::var("USE_SSL").unwrap_or_else(|_| "false".to_string()).to_lowercase() == "true";

    // --- BusyBox-like command detection (matches Python client behavior) ---
    
    // 1. Get the current executable's name. This allows the client to be symlinked
    //    to names like "ffmpeg", "ffprobe", etc., and use that name as the command.
    let current_exe_path = env::current_exe()?;
    let command_from_exe = current_exe_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("grpc-ffmpeg-client") // Default if no file stem found
        .to_string();

    // 2. Process only the user-provided arguments for proper quoting.
    let rffmpeg_command_args = handle_quoted_arguments(&args.command);
    
    // 3. Prepend the executable name to the processed arguments.
    let mut full_command = vec![command_from_exe];
    full_command.extend(rffmpeg_command_args);

    // 4. Join the final list of arguments into a single command string.
    let command_str = full_command.join(" ");

    // Execute the remote command and exit with the received exit code.
    match run_command(command_str, use_ssl).await {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(e) => {
            eprintln!("An unexpected error occurred: {}", e);
            std::process::exit(1);
        }
    }
}
