//! Console front-end of the watermark tool (see lib.rs for the algorithm).
//!
//! Usage:
//!   watermark-rs embed photo.jpg marked.png "Short text" [--seed HEX] [--debug]
//!   watermark-rs extract marked.png [--seed HEX]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use watermark_rs::*;

// --------------------------------------------------------------------------- //
// Command line
// --------------------------------------------------------------------------- //
#[derive(Parser)]
#[command(about = "Invisible image watermark (experimental print/scan resistance)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Embed up to 16 UTF-8 bytes
    Embed {
        input: PathBuf,
        output: PathBuf,
        message: String,
        /// Layout seed, 1-16 hex digits (the same seed is needed to read the watermark)
        #[arg(long, default_value = "57A73", value_parser = parse_seed)]
        seed: u64,
        /// Save a PNG with red borders around modified DCT blocks
        #[arg(long)]
        debug: bool,
    },
    /// Read the hidden text; no original required
    Extract {
        input: PathBuf,
        /// Layout seed, 1-16 hex digits (must match the one used for embedding)
        #[arg(long, default_value = "57A73", value_parser = parse_seed)]
        seed: u64,
    },
}

fn run_embed(input: &Path, output: &Path, message: &str, seed: u64, debug: bool) -> Res<()> {
    let fmt = output_format(output)?;
    if output.exists() {
        return Err(format!("Output already exists: {}", output.display()).into());
    }
    frame(message)?; // validate the message early
    let debug_path = debug.then(|| {
        let stem = output.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
        output.with_file_name(format!("{stem}_debug.png"))
    });
    if let Some(p) = &debug_path {
        if p.exists() {
            return Err(format!("Debug output already exists: {}", p.display()).into());
        }
    }

    let (rgb, w, h) = load(input)?;
    let emb = embed(&rgb, w, h, message, seed)?;

    // Verify the exact bytes that will be written; never claim success otherwise.
    let encoded = encode_image(&emb.rgb, w, h, fmt)?;
    let checked = image::load_from_memory(&encoded)?.to_rgb8();
    let (cw, ch) = (checked.width() as usize, checked.height() as usize);
    let (found, _) = detect(&luma(checked.as_raw(), cw, ch), true, &layout_for(seed));
    if found.as_deref() != Some(message) {
        return Err("Embedding failed verification: output watermark cannot be recovered. No file saved.".into());
    }

    write_new(&encoded, output)?;
    println!("Watermark embedded: {}", output.display());
    if let Some(p) = debug_path {
        let overlay = debug_overlay(&emb.rgb, w, &emb.modified);
        write_new(&encode_image(&overlay, w, h, Fmt::Png)?, &p)?;
        println!("Debug visualization: {}", p.display());
    }
    Ok(())
}

fn run_extract(input: &Path, seed: u64) -> Res<ExitCode> {
    let (rgb, w, h) = load(input)?;
    match extract(&rgb, w, h, seed)? {
        Some(msg) => {
            println!("{msg}");
            Ok(ExitCode::SUCCESS)
        }
        None => {
            eprintln!("Watermark not found.");
            Ok(ExitCode::from(1))
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Embed { input, output, message, seed, debug } => run_embed(&input, &output, &message, seed, debug).map(|_| ExitCode::SUCCESS),
        Cmd::Extract { input, seed } => run_extract(&input, seed),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::from(2)
        }
    }
}

