package main

import (
	"context"
	"fmt"
	"io"
	"log"
	"os"
	"os/exec"
	"strings"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/metadata"

	ffmpeg "src/client/go/ffmpegpb"
)

const (
	defaultGrpcHost = "ffmpeg-workers"
	defaultGrpcPort = "50051"
	maxRetries      = 5
	baseDelay       = 1 * time.Second
)

func getEnv(key, defaultValue string) string {
	if value, exists := os.LookupEnv(key); exists {
		return value
	}
	return defaultValue
}

func handleQuotedArguments(commandArgs []string) []string {
	charsThatNeedQuoting := []string{" ", ",", ":", "(", ")"}
	parametersToQuote := []string{"-filter_complex", "-vf", "-hls_segment_filename", "-user_agent"}

	var rffmpegCommand []string
	i := 0

	for i < len(commandArgs) {
		arg := commandArgs[i]

		// Handle -i file: argument separately
		if arg == "-i" && i+1 < len(commandArgs) && strings.HasPrefix(commandArgs[i+1], "file:") {
			filePathArg := commandArgs[i+1]
			filePath := strings.TrimPrefix(filePathArg, "file:")

			// Quote the file path if it contains spaces or special characters
			needsQuote := false
			for _, char := range charsThatNeedQuoting {
				if strings.Contains(filePath, char) {
					needsQuote = true
					break
				}
			}
			if needsQuote {
				filePath = fmt.Sprintf(`"%s"`, filePath)
			}

			// Reassemble the -i file: argument
			rffmpegCommand = append(rffmpegCommand, arg)
			rffmpegCommand = append(rffmpegCommand, fmt.Sprintf("file:%s", filePath))
			i += 2 // Skip the next argument as it's part of -i file:
		} else if containsString(parametersToQuote, arg) && i+1 < len(commandArgs) {
			nextArg := commandArgs[i+1]

			// Quote the argument value if it contains spaces, commas, or colons
			needsQuote := false
			for _, char := range charsThatNeedQuoting {
				if strings.Contains(nextArg, char) {
					needsQuote = true
					break
				}
			}
			if needsQuote {
				nextArg = fmt.Sprintf(`"%s"`, nextArg)
			}

			rffmpegCommand = append(rffmpegCommand, arg)
			rffmpegCommand = append(rffmpegCommand, nextArg)
			i += 2 // Skip the next argument as it's part of this argument set
		} else {
			rffmpegCommand = append(rffmpegCommand, arg)
			i += 1
		}
	}
	return rffmpegCommand
}

func containsString(slice []string, item string) bool {
	for _, s := range slice {
		if s == item {
			return true
		}
	}
	return false
}

var (
	version = "dev" // populated by go build -ldflags="-X main.version=$CI_COMMIT_TAG"
)

func main() {
	log.SetFlags(0) // Don't print timestamps
	log.SetOutput(os.Stderr)

	// Check for --version flag
	if len(os.Args) > 1 && (os.Args[1] == "--version" || os.Args[1] == "-v") {
		fmt.Printf("grpc-ffmpeg-client version %s\n", version)
		os.Exit(0)
	}

	certificatePath := getEnv("CERTIFICATE_PATH", "server.crt")
	authToken := getEnv("AUTH_TOKEN", "my_secret_token1")
	grpcHost := getEnv("GRPC_HOST", defaultGrpcHost)
	grpcPort := getEnv("GRPC_PORT", defaultGrpcPort)
	useSSL := strings.ToLower(getEnv("USE_SSL", "false")) == "true"

	target := fmt.Sprintf("%s:%s", grpcHost, grpcPort)

	var opts []grpc.DialOption
	if useSSL {
		log.Printf("Using SSL with certificate: %s", certificatePath)
		creds, err := credentials.NewClientTLSFromFile(certificatePath, "")
		if err != nil {
			log.Fatalf("Failed to load client TLS credentials: %v", err)
		}
		opts = append(opts, grpc.WithTransportCredentials(creds))
	} else {
		log.Println("Using insecure connection")
		opts = append(opts, grpc.WithTransportCredentials(insecure.NewCredentials()))
	}

	// Add authentication token
	opts = append(opts, grpc.WithUnaryInterceptor(authUnaryInterceptor(authToken)))
	opts = append(opts, grpc.WithStreamInterceptor(authStreamInterceptor(authToken)))

	clientArgs := os.Args[1:]
	processedArgs := handleQuotedArguments(clientArgs)

	// Reconstruct the command string for the gRPC request
	// Mimics Python client's behavior of including the script name as the first arg in the command string
	executableName := os.Args[0] // Full path to the executable
	// Extract base name, similar to Python's os.path.basename(sys.argv[0])
	baseExecutableName := executableName[strings.LastIndex(executableName, "/")+1:]
	commandStr := strings.Join(append([]string{baseExecutableName}, processedArgs...), " ")


	var conn *grpc.ClientConn
	var err error
	var exitCode int32 = 1 // Default to error

	for attempt := 0; attempt < maxRetries; attempt++ {
		conn, err = grpc.Dial(target, opts...)
		if err != nil {
			log.Printf("Failed to connect to gRPC server (attempt %d/%d): %v", attempt+1, maxRetries, err)
			time.Sleep(baseDelay * time.Duration(1<<attempt)) // Exponential backoff
			continue
		}
		defer conn.Close() // Close connection after each attempt, will be reopened on retry

		client := ffmpeg.NewFFmpegServiceClient(conn)
		ctx, cancel := context.WithTimeout(context.Background(), 30*time.Minute) // Long timeout for ffmpeg commands
		defer cancel()

		req := &ffmpeg.CommandRequest{Command: commandStr}
		stream, err := client.ExecuteCommand(ctx, req)
		if err != nil {
			log.Printf("Failed to execute command (attempt %d/%d): %v", attempt+1, maxRetries, err)
			if strings.Contains(err.Error(), "unavailable") || strings.Contains(err.Error(), "connection refused") {
				time.Sleep(baseDelay * time.Duration(1<<attempt))
				continue // Retry on unavailable or connection refused
			} else {
				exitCode = 1
				return // Non-retryable error
			}
		}

		// Success connecting and starting stream, break retry loop
		exitCode = 0 // Assume success unless an exit_code is received
		for {
			resp, err := stream.Recv()
			if err == io.EOF {
				break // Stream ended
			}
			if err != nil {
				log.Printf("Error receiving stream response: %v", err)
				exitCode = 1
				break
			}

			if resp.BinaryOutput != nil && len(resp.BinaryOutput) > 0 {
				_, writeErr := os.Stdout.Write(resp.BinaryOutput)
				if writeErr != nil {
					log.Printf("Error writing binary output to stdout: %v", writeErr)
					exitCode = 1
					break
				}
			} else if resp.Output != "" {
				if resp.Stream == "stdout" {
					_, writeErr := os.Stdout.WriteString(resp.Output)
					if writeErr != nil {
						log.Printf("Error writing stdout to stdout: %v", writeErr)
						exitCode = 1
						break
					}
				} else if resp.Stream == "stderr" {
					_, writeErr := os.Stderr.WriteString(resp.Output)
					if writeErr != nil {
						log.Printf("Error writing stderr to stderr: %v", writeErr)
						exitCode = 1
						break
					}
				}
			} else if resp.Stream == "exit_code" {
				exitCode = resp.ExitCode
				break // Exit code indicates command completion
			}
		}
		// If we reached here, the stream finished or an exit_code was received.
		// If exitCode is still 0 (meaning no explicit exit_code was sent but stream ended), it's a success.
		// If it's 1, it's an error.
		os.Exit(int(exitCode))
		return // Exit after processing stream
	}

	log.Printf("Command failed after %d retries.", maxRetries)
	os.Exit(int(exitCode))
}

// authUnaryInterceptor adds the authorization token to unary RPC calls.
func authUnaryInterceptor(token string) grpc.UnaryClientInterceptor {
	return func(ctx context.Context, method string, req, reply interface{}, cc *grpc.ClientConn, invoker grpc.UnaryInvoker, opts ...grpc.CallOption) error {
		md := metadata.Pairs("authorization", token)
		newCtx := metadata.NewOutgoingContext(ctx, md)
		return invoker(newCtx, method, req, reply, cc, opts...)
	}
}

// authStreamInterceptor adds the authorization token to stream RPC calls.
func authStreamInterceptor(token string) grpc.StreamClientInterceptor {
	return func(ctx context.Context, desc *grpc.StreamDesc, cc *grpc.ClientConn, method string, streamer grpc.StreamInvoker, opts ...grpc.CallOption) (grpc.ClientStream, error) {
		md := metadata.Pairs("authorization", token)
		newCtx := metadata.NewOutgoingContext(ctx, md)
		return streamer(newCtx, desc, cc, method, opts...)
	}
}
