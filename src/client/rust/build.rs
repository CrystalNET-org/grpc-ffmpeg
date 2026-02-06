fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_server(false) // We only need to generate client code
        .compile_protos(
            &["../../proto/ffmpeg.proto"],
            &["../../proto"], // Include the directory where ffmpeg.proto is located
        )?;
    Ok(())
}