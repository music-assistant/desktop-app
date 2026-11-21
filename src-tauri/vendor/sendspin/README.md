# sendspin-rs

> [!WARNING]
> THIS IS A WIP. Please help!

Hyper-efficient Rust implementation of the [Sendspin Protocol](https://github.com/Sendspin/spec) for synchronized multi-room audio streaming.

## Features

- **Zero-copy audio pipeline** - Minimal allocations, maximum performance
- **Lock-free concurrency** - No contention on audio thread
- **Async I/O** - Efficient WebSocket handling with Tokio
- **Type-safe protocol** - Leverage Rust's type system for correctness

## Performance Targets

- Audio latency: <10ms end-to-end jitter
- CPU usage: <2% on modern hardware (4-core)
- Memory: <20MB stable
- Thread synchronization: Lock-free audio pipeline

## Current Status

**Phase 1: Foundation** ✅ (Complete)
- Core audio types (Sample, AudioFormat, AudioBuffer)
- Protocol message types with serde serialization
- WebSocket client with handshake
- PCM decoder (16-bit and 24-bit)
- Clock synchronization (NTP-style)

**Phase 2: Audio Pipeline** 🚧 (Next)
- Audio output (cpal integration)
- Lock-free scheduler
- End-to-end player


## Installing build dependencies

### Debian/Ubuntu/Mint

```
sudo apt install libasound2-dev
```

### Fedora/Centos

```
dnf install alsa-lib-devel
```

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
sendspin = "0.1"
```

### Basic Client Example

```rust
use sendspin::ProtocolClientBuilder;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = ProtocolClientBuilder::builder()
        .client_id(uuid::Uuid::new_v4().to_string())
        .name("My Player".to_string())
        .build()
        .connect("ws://localhost:8080/sendspin")
        .await?;

    // Client is now connected and ready to receive audio

    Ok(())
}
```

See `examples/` directory for more examples.

## Architecture

See [docs/rust-thoughts.md](docs/rust-thoughts.md) for detailed architecture and implementation notes.

## Development

```bash
# Build
cargo build

# Run tests
cargo test

# Run examples
cargo run --example basic_client

# Build with optimizations
cargo build --release
```

### Verification (cargo-make)

```bash
# Install cargo-make (one time)
cargo install cargo-make

# Run the full verification suite
cargo make verify
```

`cargo make verify` runs: tests, Clippy, doc build, doctests, and formatting checks.

## Testing

```bash
# Run all tests
cargo test

# Run specific test
cargo test test_sample_from_i16

# Run with logging
RUST_LOG=debug cargo test
```

## License

MIT OR Apache-2.0

## Contributing

Contributions welcome! Please open an issue or PR.
