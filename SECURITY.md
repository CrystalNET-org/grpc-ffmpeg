# Security

## Reporting a vulnerability

Please report security problems privately through
[GitHub's vulnerability reporting](https://github.com/CrystalNET-org/grpc-ffmpeg/security/advisories/new),
not as a public issue. Include what is affected, how to reproduce it, and the versions of the
worker image and the client.

You will get an answer as soon as possible. Fixes are released as a new version and
announced in a security advisory.

## Supported versions

Only the latest release receives security fixes.

## Security model

A worker runs `ffmpeg`, `ffprobe`, `mediainfo` and `vainfo` with any arguments a client sends.
ffmpeg can read and write any file the worker can access, so access to the worker's gRPC port
means access to those files. The worker relies on:

- the token (`VALID_TOKEN`), which every call must carry,
- TLS (`USE_SSL`) or a trusted network, as the token is sent with every call,
- running as an unprivileged user with only the directories it needs mounted.

Problems within this model, such as calls accepted without a valid token or binaries other
than the four above being run, are vulnerabilities. That ffmpeg can access files the worker
can access is expected behaviour.
