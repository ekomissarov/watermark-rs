# watermark-rs

A Rust implementation of invisible digital watermarking for images, with a command-line tool and a desktop window.

`watermark-rs` allows you to embed and extract hidden information in images using frequency-domain techniques. The watermark is designed to remain recoverable after common image transformations, including JPEG compression, resizing, and minor rotations.

The project focuses on robustness, performance, and interoperability with a compatible Python implementation.

## Features

- **Invisible watermarking** — Embed information without introducing obvious visual changes.
- **DCT-based embedding** — Uses the Discrete Cosine Transform to modify image frequency components.
- **Document-aware embedding** — Modifies only textured, non-paper, non-text regions of the image.
- **Robust extraction** — Searches over scale and small rotation hypotheses to tolerate common image processing operations.
- **Error correction** — Uses Hamming coding to improve watermark recovery.
- **Integrity verification** — Includes checksum validation to detect corrupted payloads.
- **Self-check on embedding** — The result is decoded again before it is saved; a file whose watermark cannot be read back is never written.
- **Synchronization** — Uses pilot signals to assist watermark recovery.
- **Configurable layout seed** — The pilot positions, pilot signs and payload mask are derived from a seed (default `57A73`, up to 16 hex digits) that can be set in the GUI and on the command line.
- **Parallel processing** — Leverages Rust's Rayon library for efficient computation.
- **Cross-language compatibility** — Supports interoperability with the corresponding Python implementation.
- **Command-line interface** — Provides a CLI for embedding and extracting watermarks.
- **Desktop GUI (Linux and macOS, optional)** — A single-window application with zoomable preview and highlighting of the regions that carry the watermark.

## How It Works

The watermarking algorithm operates primarily in the frequency domain.

During embedding, the image is divided into 16×16 blocks and transformed using the Discrete Cosine Transform (DCT). A bit is stored as the sign of the difference between two mid-frequency coefficients, `(2,3)` and `(3,2)`, of each block. The sign pattern is a 20×20-block tile (320×320 pixels) that is repeated across the whole image, so the payload survives cropping and can be recovered from any part of the picture that is large enough.

During extraction, the algorithm analyzes the same coefficients at every possible block alignment, locates the tile phase using public pilot signals, and reconstructs the embedded information.

Several mechanisms improve reliability:

1. **Frequency-domain embedding:** Reduces sensitivity to certain pixel-level modifications.
2. **Error-correcting codes:** Hamming(15,11) helps recover information affected by image transformations.
3. **Pilot signals:** 145 of the 400 cells of a tile are known signs that assist synchronization during extraction.
4. **Checksums:** A truncated BLAKE2s checksum verifies the integrity of recovered data.
5. **Deterministic processing:** The layout is derived from a seed, so the same seed gives the same layout. With the default seed it is bit-identical to the Python implementation.
6. **Adaptive strength:** Blocks are weighted by local texture; paper-white areas, flat areas and text-like areas are left untouched.

The payload is up to 16 UTF-8 bytes (not characters).

The algorithm is designed for robust watermark recovery rather than cryptographic protection of image contents. The seed only selects the layout, so a watermark embedded with one seed is not found with another one, but the seed is **not a cryptographic key**: it is at most 64 bits (the default has about 20), the checksum is not keyed, and anyone who knows the algorithm can read, overwrite or intentionally disrupt the watermark.

## Requirements

- Rust toolchain (Cargo and rustc)
- Linux, macOS, or Windows for the command-line tool; Linux or macOS for the GUI
- Input and output images in PNG or JPEG format

The project uses the following Rust crates:

| Crate | Purpose |
|---|---|
| `image` | Image loading, processing, and saving (PNG, JPEG) |
| `rustfft` | Fast Fourier Transform operations |
| `sha2` | SHA-2 hashing (public layout generation) |
| `blake2` | BLAKE2 hashing (payload checksum) |
| `clap` | Command-line argument parsing |
| `rayon` | Parallel processing |
| `eframe` / `egui` | Desktop window (optional, `gui` feature) |
| `rfd` | File dialogs (optional, `gui` feature) |

## Installation

Clone the repository:

```bash
git clone git@github.com:ekomissarov/watermark-rs.git
cd watermark-rs
```

Build the command-line tool:

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

The GUI is built separately, see [Desktop GUI](#desktop-gui).

## Usage (command line)

### Embed a Watermark

```bash
./target/release/watermark-rs embed photo.jpg marked.png "Short text"
```

The application processes the source image and produces a watermarked output image. The output must be a `.png` or `.jpg`/`.jpeg` file and must not exist yet. Add `--debug` to also write `marked_debug.png`, a copy of the result with red borders around every modified DCT block:

```bash
./target/release/watermark-rs embed photo.jpg marked.png "Short text" --debug
```

The layout seed is given with `--seed` as 1–16 hexadecimal digits (default `57A73`):

```bash
./target/release/watermark-rs embed photo.jpg marked.png "Short text" --seed C0FFEE
```

Before the file is saved, the exact bytes that will be written are decoded again. If the watermark cannot be recovered, no file is created and the program exits with an error.

### Extract a Watermark

```bash
./target/release/watermark-rs extract marked.png
./target/release/watermark-rs extract marked.png --seed C0FFEE
```

The seed must match the one used for embedding; with another seed the watermark is simply not found.

The extractor attempts to reconstruct the watermark even when the image has undergone supported transformations (rescaling, small rotation, recompression). No original image is required. It prints the hidden text, or `Watermark not found.` and exits with status `1`. On an image without a watermark the search can take up to about two minutes (the search budget is 110 seconds).

### Exit codes

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | `extract`: watermark not found |
| `2` | Error (bad arguments, unreadable image, existing output file, failed verification, ...) |

## Desktop GUI

A single-window application for Linux and macOS. It is developed and tested on Ubuntu; the macOS build uses the same cross-platform libraries (`eframe`/`egui` and `rfd`) and needs no platform-specific code, but it has not been tested on a Mac yet. The console tool is not affected: the GUI is an optional feature and a separate executable, `watermark-gui`.

### Build and run

Install the system libraries once.

Ubuntu / Debian:

```bash
sudo apt install build-essential pkg-config libgtk-3-dev libxkbcommon-dev
```

macOS: no extra libraries are needed, only the Xcode Command Line Tools (and Rust, e.g. via [rustup](https://rustup.rs)):

```bash
xcode-select --install
```

Then:

```bash
cargo run --release --features gui --bin watermark-gui
```

or build it and start the executable:

```bash
cargo build --release --features gui
./target/release/watermark-gui
```

The first build takes several minutes because of the GUI dependencies.

### Workflow

1. **Open** an image with the *Open…* button or by dragging a PNG/JPEG file into the window.
2. **Look at the preview.** Use the mouse wheel (or pinch) to zoom around the cursor, drag to pan, double-click to fit the image into the window. The toolbar has *−*, *+*, *Fit* and *100%*.
3. *(Optional)* Press **🔍 Find watermark in file** to check whether the opened file already contains a watermark. The search runs in the background, can be stopped with *Stop*, and a found text can be copied into the input field with *Copy to field*. It is **not** started automatically, because on an image without a watermark it can run for up to a couple of minutes and keeps the CPU busy.
4. **Type the label** into the text field (up to 16 bytes of UTF-8; the counter shows the current size). The **Seed (hex)** field next to it holds the layout seed and is pre-filled with the default (`57A73`; the *Default* button restores it). Use the same seed to read the watermark later; the field is also used by *Find watermark in file*. An invalid seed is shown in red and blocks processing.
5. Press **⚙ Process.** The watermark is embedded and immediately read back. The modified DCT blocks are highlighted with red squares on the preview. Switch between *Original* and *Watermarked*, or hide the squares with *Show embedded regions*. The status bar shows how many blocks were modified, how many of the 400 pattern cells are covered, and whether the read-back check passed.
6. Press **💾 Save…** to write the watermarked image (PNG is recommended; JPEG is possible). Saving is only available after a successful check with the current label, and the saved bytes are verified once more before they are written.

All file dialogs and heavy operations run in background threads, so the window stays responsive.

### Notes

- Images larger than 4096 pixels on a side are downscaled for the preview only; embedding and saving always use the full resolution.
- If the label or the seed is changed after processing, press *Process* again before saving.

## Robustness

The watermarking algorithm is designed to tolerate several common image transformations.

The following scenarios have been successfully verified in cross-language interoperability tests (`crosstest.py`):

| Test | Result |
|---|---|
| Python embedding → Rust extraction | Passed |
| Rust embedding → Python extraction | Passed |
| Rust extraction after 125% resizing | Passed |
| Rust extraction after 80% resizing | Passed |
| Rust extraction after 0.5° rotation | Passed |
| Rust extraction after JPEG compression (quality 70) | Passed |

All eight checks of the cross-language test suite passed (these six scenarios plus the two plain embedding steps).

These results demonstrate compatibility under the tested conditions. They do not guarantee successful recovery for every image, watermark payload, or transformation.

## Python Compatibility

The Rust implementation is designed to be compatible with a corresponding Python implementation of the watermarking algorithm: the tile layout, pilots, mask, Hamming code and checksum are bit-identical, so a file marked by one implementation can be read by the other. This holds for the default seed only; the Python version has no seed option, so files marked with another seed can be read by the Rust version alone.

This enables workflows such as:

- Embedding a watermark in Python and extracting it in Rust.
- Embedding a watermark in Rust and extracting it in Python.
- Comparing implementations for correctness and performance.
- Validating watermark recovery after image transformations.

Cross-language testing can be performed using the `crosstest.py` script.

Example:

```bash
python crosstest.py \
  --py ./watermark.py \
  --rs ../watermark-rs/target/release/watermark-rs
```

This command assumes that the Python and Rust repositories are located in adjacent directories and that the Rust release executable has already been built.

Note that the resampling used by the extraction search (bicubic, area and rotation) is implemented in pure Rust and may differ slightly from OpenCV's, so results on heavily processed images can differ between the two implementations.

## Development

Build the command-line tool:

```bash
cargo build
```

Build an optimized release version (recommended: debug builds are much slower at image processing):

```bash
cargo build --release
```

Build everything, including the GUI:

```bash
cargo build --release --features gui
```

Check the project:

```bash
cargo check
cargo check --features gui
```

Run tests:

```bash
cargo test --release
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
├── crosstest.py
└── src/
    ├── lib.rs
    ├── main.rs
    └── bin/
        └── watermark-gui.rs
```

- `Cargo.toml` — Project metadata, dependencies and the optional `gui` feature.
- `Cargo.lock` — Locked dependency versions for reproducible builds.
- `src/lib.rs` — The watermarking algorithm: format constants, layout, Hamming coding, embedding, detection, scale/rotation search, image I/O.
- `src/main.rs` — Command-line interface (`watermark-rs`).
- `src/bin/watermark-gui.rs` — Desktop window (`watermark-gui`, built with `--features gui`).
- `crosstest.py` — Cross-checks the Rust and Python implementations.
- `.gitignore` — Git exclusions for generated files and build artifacts.

## Limitations

- Watermark recovery depends on image content, payload characteristics, and transformation severity.
- **Image size and texture matter.** The pattern is only written into textured, non-white, non-text blocks, and every one of its 400 cells needs at least one such block. Small images (for example about 640×640 px), screenshots, documents and pictures with large flat or white areas may fail the read-back check. As a rule of thumb, use photographs of roughly 1500×1500 px or more with texture across the whole frame. In that case the tools report the failure instead of saving an unreadable file.
- The embedded image must be at least 320×320 px; detection needs at least 130 px per side. Images above 50 megapixels are rejected.
- Heavy cropping, extreme geometric transformations, or aggressive compression may prevent successful extraction.
- Searching for a watermark in an image that has none is slow by design (it tries many scale and rotation hypotheses).
- Invisible watermarking does not prevent copying, modification, or redistribution of an image.
- The seed selects the layout but is not a secret key: the algorithm has no signatures or keyed authentication, so the watermark should not be treated as cryptographic proof of ownership or authenticity without additional verification mechanisms.

## License

No license has been specified yet.

Until a license is added, the repository's source code remains subject to applicable copyright law.

## Author

Eugene Komissarov
