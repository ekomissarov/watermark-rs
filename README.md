# watermark-rs

A Rust implementation of invisible digital watermarking for images.

`watermark-rs` allows you to embed and extract hidden information in images using frequency-domain techniques. The watermark is designed to remain recoverable after common image transformations, including JPEG compression, resizing, and minor rotations.

The project focuses on robustness, performance, and interoperability with a compatible Python implementation.

## Features

- **Invisible watermarking** — Embed information without introducing obvious visual changes.
- **DCT-based embedding** — Uses the Discrete Cosine Transform to modify image frequency components.
- **Robust extraction** — Designed to tolerate common image processing operations.
- **Error correction** — Uses Hamming coding to improve watermark recovery.
- **Integrity verification** — Includes checksum validation to detect corrupted payloads.
- **Synchronization** — Uses pilot signals to assist watermark recovery.
- **Parallel processing** — Leverages Rust's Rayon library for efficient computation.
- **Cross-language compatibility** — Supports interoperability with the corresponding Python implementation.
- **Command-line interface** — Provides a CLI for embedding and extracting watermarks.

## How It Works

The watermarking algorithm operates primarily in the frequency domain.

During embedding, the image is divided into blocks and transformed using the Discrete Cosine Transform (DCT). The watermark is encoded into selected frequency coefficients to balance visual quality and robustness.

During extraction, the algorithm analyzes the image's frequency components to reconstruct the embedded information.

Several mechanisms improve reliability:

1. **Frequency-domain embedding:** Reduces sensitivity to certain pixel-level modifications.
2. **Error-correcting codes:** Help recover information affected by image transformations.
3. **Pilot signals:** Assist synchronization during extraction.
4. **Checksums:** Verify the integrity of recovered data.
5. **Deterministic processing:** Enables compatibility between Rust and Python implementations.

The algorithm is designed for robust watermark recovery rather than cryptographic protection of image contents.

## Requirements

- Rust toolchain (Cargo and rustc)
- macOS, Linux, or Windows
- A supported image format

The project uses the following Rust crates:

| Crate | Purpose |
|---|---|
| `image` | Image loading, processing, and saving |
| `rustfft` | Fast Fourier Transform operations |
| `sha2` | SHA-2 hashing |
| `blake2` | BLAKE2 hashing |
| `clap` | Command-line argument parsing |
| `rayon` | Parallel processing |

## Installation

Clone the repository:

```bash
git clone git@github.com:ekomissarov/watermark-rs.git
cd watermark-rs
```

Build the release version:

```bash
cargo build --release
```

The executable will be available at:

```text
target/release/watermark-rs
```

To explore the available commands and options:

```bash
./target/release/watermark-rs --help
```

## Usage

The application provides command-line operations for embedding and extracting invisible watermarks.

### Embed a Watermark

Use the embedding operation to insert a hidden payload into an image.

The application processes the source image and produces a watermarked output image.

### Extract a Watermark

Use the extraction operation to recover the embedded payload from an image.

The extractor attempts to reconstruct the watermark even when the image has undergone supported transformations.

For the exact command syntax and available parameters, run:

```bash
./target/release/watermark-rs --help
```

## Robustness

The watermarking algorithm is designed to tolerate several common image transformations.

The following scenarios have been successfully verified in cross-language interoperability tests:

| Test | Result |
|---|---|
| Python embedding → Rust extraction | Passed |
| Rust embedding → Python extraction | Passed |
| Rust extraction after 125% resizing | Passed |
| Rust extraction after 80% resizing | Passed |
| Rust extraction after 0.5° rotation | Passed |
| Rust extraction after JPEG compression (quality 70) | Passed |

All eight scenarios in the cross-language test suite passed.

These results demonstrate compatibility under the tested conditions. They do not guarantee successful recovery for every image, watermark payload, or transformation.

## Python Compatibility

The Rust implementation is designed to be compatible with a corresponding Python implementation of the watermarking algorithm.

This enables workflows such as:

- Embedding a watermark in Python and extracting it in Rust.
- Embedding a watermark in Rust and extracting it in Python.
- Comparing implementations for correctness and performance.
- Validating watermark recovery after image transformations.

Cross-language testing can be performed using the companion Python project's `crosstest.py` script.

Example:

```bash
python crosstest.py \
  --py ./watermark.py \
  --rs ../watermark-rs/target/release/watermark-rs
```

This command assumes that the Python and Rust repositories are located in adjacent directories and that the Rust release executable has already been built.

## Development

Build the project:

```bash
cargo build
```

Build an optimized release version:

```bash
cargo build --release
```

Check the project:

```bash
cargo check
```

Run tests:

```bash
cargo test
```

Check formatting:

```bash
cargo fmt --check
```

Run Clippy:

```bash
cargo clippy
```

## Project Structure

```text
watermark-rs/
├── Cargo.toml
├── Cargo.lock
├── .gitignore
├── README.md
└── src/
    └── main.rs
```

- `Cargo.toml` — Project metadata and dependencies.
- `Cargo.lock` — Locked dependency versions for reproducible builds.
- `src/main.rs` — Main application and watermarking implementation.
- `.gitignore` — Git exclusions for generated files and build artifacts.

## Limitations

- Watermark recovery depends on image content, payload characteristics, and transformation severity.
- Heavy cropping, extreme geometric transformations, or aggressive compression may prevent successful extraction.
- Invisible watermarking does not prevent copying, modification, or redistribution of an image.
- The watermark should not be treated as cryptographic proof of ownership or authenticity without additional verification mechanisms.

## License

No license has been specified yet.

Until a license is added, the repository's source code remains subject to applicable copyright law.

## Author

Eugene Komissarov
