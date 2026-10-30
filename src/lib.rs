//! Document-aware public blind watermark (experimental print/scan prototype).
//!
//! Rust port of `watermark.py`. The on-image format is identical: adaptive DCT pair
//! modulation, repeated 320x320-pixel tiles, public synchronisation pilots,
//! Hamming(15,11), 48-bit checksum. No secrets, signatures, or proof of authorship;
//! the layout seed only selects the layout and is not a cryptographic key.
//!
//! Core library shared by the `watermark-rs` CLI and the `watermark-gui` window.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::f64::consts::PI;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use blake2::digest::{Update, VariableOutput};
use blake2::Blake2sVar;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageReader};
use rayon::prelude::*;
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use sha2::{Digest, Sha256};

pub type Res<T> = Result<T, Box<dyn std::error::Error>>;
type C = Complex<f32>;

// --------------------------------------------------------------------------- //
// Format constants (changing any of these breaks compatibility with marked files)
// --------------------------------------------------------------------------- //
pub const BLOCK: usize = 16;
pub const TILE: usize = 20;
pub const MAX_MESSAGE_BYTES: usize = 16;
pub const CHECKSUM_BYTES: usize = 6;
pub const FRAME_BYTES: usize = 1 + MAX_MESSAGE_BYTES + CHECKSUM_BYTES; // 23
pub const FRAME_BITS: usize = FRAME_BYTES * 8; // 184
pub const CODE_BITS: usize = 15 * ((FRAME_BITS + 10) / 11); // 255
pub const PILOTS: usize = TILE * TILE - CODE_BITS; // 145
pub const COEFF_A: (usize, usize) = (2, 3);
pub const COEFF_B: (usize, usize) = (3, 2);
pub const STRENGTH: f32 = 115.0;
/// Default value of the public layout seed (pilot positions, pilot signs, payload mask).
/// Files marked with a different seed can only be read with that same seed.
/// The seed selects the layout; it is not a cryptographic key.
pub const DEFAULT_SEED: u64 = 0x57A73;

// --------------------------------------------------------------------------- //
// Operational limits
// --------------------------------------------------------------------------- //
pub const MIN_EMBED_SIDE: usize = BLOCK * TILE;
pub const MIN_DETECT_SIDE: usize = 130;
pub const MAX_PIXELS: usize = 50_000_000;
pub const MAX_EXTRACT_SIDE: usize = 3600;
pub const SEARCH_SECONDS: u64 = 110;
pub const EMBED_ACTIVE_WEIGHT: f32 = 0.20;
pub const DETECT_MIN_WEIGHT: f32 = 0.15;
pub const PROGRESS_INTERVAL: u64 = 12;
pub const JPEG_QUALITY: u8 = 95;

// --------------------------------------------------------------------------- //
// Deterministic public layout
// --------------------------------------------------------------------------- //
pub fn random_bytes(n: usize, domain: &[u8], seed: u64) -> Vec<u8> {
    let seed = seed.to_be_bytes();
    let mut out = Vec::with_capacity(n + 32);
    for counter in 0..((n + 31) / 32) as u64 {
        let mut h = Sha256::new();
        Digest::update(&mut h, domain);
        Digest::update(&mut h, seed);
        Digest::update(&mut h, counter.to_be_bytes());
        out.extend_from_slice(&h.finalize());
    }
    out.truncate(n);
    out
}

pub fn random_bits(n: usize, domain: &[u8], seed: u64) -> Vec<u8> {
    let bytes = random_bytes((n + 7) / 8, domain, seed);
    (0..n).map(|i| (bytes[i / 8] >> (7 - i % 8)) & 1).collect()
}

/// Fisher-Yates with fixed SHA256-derived words (identical to the Python version).
pub fn shuffle(n: usize, seed: u64) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..n).collect();
    let source = random_bytes(4 * n, b"layout-v5", seed);
    for i in (1..n).rev() {
        let word = u32::from_be_bytes([source[4 * i], source[4 * i + 1], source[4 * i + 2], source[4 * i + 3]]);
        let j = word as usize % (i + 1);
        idx.swap(i, j);
    }
    idx
}

/// Public layout of one tile for a given seed: which cells carry data, the pilot signs and the payload mask.
pub struct Layout {
    data_idx: Vec<usize>,
    pilot_template: Vec<f32>,
    mask_bits: Vec<u8>,
    pilot_fft: Vec<C>,
    pilot_abs_fft: Vec<C>,
}

static FFT_FWD: LazyLock<Arc<dyn Fft<f32>>> = LazyLock::new(|| FftPlanner::<f32>::new().plan_fft_forward(TILE));
static FFT_INV: LazyLock<Arc<dyn Fft<f32>>> = LazyLock::new(|| FftPlanner::<f32>::new().plan_fft_inverse(TILE));

/// 2-D FFT of a TILE x TILE grid (unnormalised, like rustfft; the caller scales the inverse).
fn fft2(data: &mut [C], inverse: bool) {
    let fft: &Arc<dyn Fft<f32>> = if inverse { &FFT_INV } else { &FFT_FWD };
    for r in 0..TILE {
        fft.process(&mut data[r * TILE..(r + 1) * TILE]);
    }
    let mut col = [C::new(0., 0.); TILE];
    for c in 0..TILE {
        for r in 0..TILE {
            col[r] = data[r * TILE + c];
        }
        fft.process(&mut col);
        for r in 0..TILE {
            data[r * TILE + c] = col[r];
        }
    }
}

fn fft2_real(v: &[f32]) -> Vec<C> {
    let mut buf: Vec<C> = v.iter().map(|&x| C::new(x, 0.)).collect();
    fft2(&mut buf, false);
    buf
}

/// Circular cross-correlation of `a` with a template, given conj(FFT(template)).
fn circ_corr(a: &[f32], kernel_fft: &[C]) -> Vec<f32> {
    let mut buf = fft2_real(a);
    for (x, k) in buf.iter_mut().zip(kernel_fft) {
        *x *= *k;
    }
    fft2(&mut buf, true);
    let norm = (TILE * TILE) as f32;
    buf.iter().map(|c| c.re / norm).collect()
}

impl Layout {
    /// Builds the layout for `seed` (deterministic, no other secret involved).
    pub fn new(seed: u64) -> Self {
        let perm = shuffle(TILE * TILE, seed);
        let data_idx = perm[PILOTS..].to_vec();
        let mut pilot_template = vec![0f32; TILE * TILE];
        let pbits = random_bits(PILOTS, b"pilots-v5", seed);
        for (k, &pos) in perm[..PILOTS].iter().enumerate() {
            pilot_template[pos] = if pbits[k] != 0 { 1. } else { -1. };
        }
        let mask_bits = random_bits(CODE_BITS, b"mask-v5", seed);
        let conj = |v: Vec<C>| -> Vec<C> { v.into_iter().map(|c| c.conj()).collect() };
        let pilot_fft = conj(fft2_real(&pilot_template));
        let abs_template: Vec<f32> = pilot_template.iter().map(|v| v.abs()).collect();
        let pilot_abs_fft = conj(fft2_real(&abs_template));
        Layout { data_idx, pilot_template, mask_bits, pilot_fft, pilot_abs_fft }
    }
}

static LAYOUTS: LazyLock<Mutex<HashMap<u64, Arc<Layout>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Layout for `seed`, computed once and cached (the GUI lets the user try different seeds).
pub fn layout_for(seed: u64) -> Arc<Layout> {
    let mut cache = LAYOUTS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(layout) = cache.get(&seed) {
        return layout.clone();
    }
    if cache.len() >= 16 {
        cache.clear();
    }
    let layout = Arc::new(Layout::new(seed));
    cache.insert(seed, layout.clone());
    layout
}

/// Parses a seed written as 1-16 hexadecimal digits (an optional `0x` prefix is allowed).
pub fn parse_seed(text: &str) -> Result<u64, String> {
    let t = text.trim();
    let t = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    if t.is_empty() || t.len() > 16 {
        return Err("Seed must be 1-16 hexadecimal digits.".into());
    }
    if !t.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("Seed may contain only hexadecimal digits (0-9, A-F).".into());
    }
    u64::from_str_radix(t, 16).map_err(|e| e.to_string())
}

// --------------------------------------------------------------------------- //
// Message framing and Hamming(15,11)
// --------------------------------------------------------------------------- //
pub const PARITY_POSITIONS: [usize; 4] = [1, 2, 4, 8];
pub const HAMMING_DATA: [usize; 11] = [3, 5, 6, 7, 9, 10, 11, 12, 13, 14, 15];

pub fn checksum(header: &[u8]) -> [u8; CHECKSUM_BYTES] {
    let mut h = Blake2sVar::new(CHECKSUM_BYTES).expect("valid blake2s size");
    h.update(header);
    let mut out = [0u8; CHECKSUM_BYTES];
    h.finalize_variable(&mut out).expect("output size matches");
    out
}

pub fn frame(message: &str) -> Res<Vec<u8>> {
    let text = message.as_bytes();
    if text.is_empty() || text.len() > MAX_MESSAGE_BYTES {
        return Err(format!("Message must contain 1–{MAX_MESSAGE_BYTES} UTF-8 bytes (not characters).").into());
    }
    let mut header = vec![text.len() as u8];
    header.extend_from_slice(text);
    header.resize(1 + MAX_MESSAGE_BYTES, 0);
    let mut raw = header.clone();
    raw.extend_from_slice(&checksum(&header));
    Ok(raw.iter().flat_map(|b| (0..8).map(move |i| (b >> (7 - i)) & 1)).collect())
}

pub fn unframe(bits: &[u8]) -> Option<String> {
    let mut raw = vec![0u8; (bits.len() + 7) / 8];
    for (i, &b) in bits.iter().enumerate() {
        raw[i / 8] |= (b & 1) << (7 - i % 8);
    }
    raw.truncate(FRAME_BYTES);
    if raw.len() < FRAME_BYTES {
        return None;
    }
    let (header, check) = raw.split_at(FRAME_BYTES - CHECKSUM_BYTES);
    if checksum(header) != check {
        return None;
    }
    let length = header[0] as usize;
    if !(1..=MAX_MESSAGE_BYTES).contains(&length) || header[1 + length..].iter().any(|&b| b != 0) {
        return None;
    }
    String::from_utf8(header[1..1 + length].to_vec()).ok()
}

pub fn encode_hamming(bits: &[u8]) -> Vec<u8> {
    let nwords = (bits.len() + 10) / 11;
    let mut out = Vec::with_capacity(nwords * 15);
    for w in 0..nwords {
        let mut word = [0u8; 16];
        for (k, &pos) in HAMMING_DATA.iter().enumerate() {
            let i = w * 11 + k;
            word[pos] = if i < bits.len() { bits[i] } else { 0 };
        }
        for &p in &PARITY_POSITIONS {
            let mut x = 0u8;
            for q in 1..16 {
                if q & p != 0 {
                    x ^= word[q];
                }
            }
            word[p] = x;
        }
        out.extend_from_slice(&word[1..]);
    }
    out
}

pub fn decode_hamming(bits: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bits.len() / 15 * 11);
    for chunk in bits.chunks_exact(15) {
        let mut word = [0u8; 16];
        word[1..].copy_from_slice(chunk);
        let mut syndrome = 0usize;
        for &p in &PARITY_POSITIONS {
            let mut x = 0u8;
            for q in 1..16 {
                if q & p != 0 {
                    x ^= word[q];
                }
            }
            syndrome += p * x as usize;
        }
        if syndrome != 0 {
            word[syndrome] ^= 1;
        }
        for &pos in &HAMMING_DATA {
            out.push(word[pos]);
        }
    }
    out.truncate(FRAME_BITS);
    out
}

pub fn decode_bits(bits: &[u8]) -> Option<String> {
    unframe(&decode_hamming(bits))
}

/// +/-1 sign for each block of one tile: pilots plus masked, coded payload.
pub fn pattern(message: &str, layout: &Layout) -> Res<Vec<f32>> {
    let mut signs = layout.pilot_template.clone();
    let coded: Vec<u8> = encode_hamming(&frame(message)?)
        .iter()
        .zip(&layout.mask_bits)
        .map(|(a, b)| a ^ b)
        .collect();
    for (i, &pos) in layout.data_idx.iter().enumerate() {
        signs[pos] = if coded[i] != 0 { 1. } else { -1. };
    }
    Ok(signs)
}

// --------------------------------------------------------------------------- //
// Planes, resampling
// --------------------------------------------------------------------------- //
#[derive(Clone)]
pub struct Plane {
    pub w: usize,
    pub h: usize,
    pub d: Vec<f32>,
}

pub fn luma(rgb: &[u8], w: usize, h: usize) -> Plane {
    let d = rgb
        .chunks_exact(3)
        .map(|p| 0.299f32 * p[0] as f32 + 0.587f32 * p[1] as f32 + 0.114f32 * p[2] as f32)
        .collect();
    Plane { w, h, d }
}

/// OpenCV BORDER_REFLECT_101.
#[inline]
pub fn reflect(mut i: isize, n: isize) -> usize {
    if n == 1 {
        return 0;
    }
    loop {
        if i < 0 {
            i = -i;
        } else if i >= n {
            i = 2 * (n - 1) - i;
        } else {
            return i as usize;
        }
    }
}

/// Correlation along x (like cv2.sepFilter2D's kernelX) with BORDER_REFLECT_101.
pub fn filter_h(src: &Plane, k: &[f32], anchor: usize) -> Plane {
    let (w, h, kl) = (src.w, src.h, k.len());
    let mut out = vec![0f32; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let s = &src.d[y * w..(y + 1) * w];
        let mut pad = vec![0f32; w + kl - 1];
        for (i, p) in pad.iter_mut().enumerate() {
            *p = s[reflect(i as isize - anchor as isize, w as isize)];
        }
        for x in 0..w {
            let mut acc = 0f32;
            for j in 0..kl {
                acc += k[j] * pad[x + j];
            }
            row[x] = acc;
        }
    });
    Plane { w, h, d: out }
}

/// Correlation along y (cv2.sepFilter2D's kernelY) with BORDER_REFLECT_101.
pub fn filter_v(src: &Plane, k: &[f32], anchor: usize) -> Plane {
    let (w, h) = (src.w, src.h);
    let mut out = vec![0f32; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (j, &kj) in k.iter().enumerate() {
            let sy = reflect(y as isize + j as isize - anchor as isize, h as isize);
            let s = &src.d[sy * w..(sy + 1) * w];
            for x in 0..w {
                row[x] += kj * s[x];
            }
        }
    });
    Plane { w, h, d: out }
}

pub fn box_filter(p: &Plane) -> Plane {
    let k = [1.0f32 / BLOCK as f32; BLOCK];
    filter_v(&filter_h(p, &k, BLOCK / 2), &k, BLOCK / 2)
}

/// OpenCV's bicubic weights (A = -0.75).
pub fn cubic_w(t: f64) -> [f64; 4] {
    let a = -0.75;
    let c0 = ((a * (t + 1.) - 5. * a) * (t + 1.) + 8. * a) * (t + 1.) - 4. * a;
    let c1 = ((a + 2.) * t - (a + 3.)) * t * t + 1.;
    let u = 1. - t;
    let c2 = ((a + 2.) * u - (a + 3.)) * u * u + 1.;
    [c0, c1, c2, 1. - c0 - c1 - c2]
}

type Taps = Vec<Vec<(usize, f32)>>;

pub fn cubic_taps(src_n: usize, dst_n: usize) -> Taps {
    let scale = src_n as f64 / dst_n as f64;
    (0..dst_n)
        .map(|i| {
            let sx = (i as f64 + 0.5) * scale - 0.5;
            let i0 = sx.floor();
            let wts = cubic_w(sx - i0);
            (0..4)
                .map(|t| {
                    let idx = (i0 as isize - 1 + t as isize).clamp(0, src_n as isize - 1) as usize;
                    (idx, wts[t] as f32)
                })
                .collect()
        })
        .collect()
}

pub fn area_taps(src_n: usize, dst_n: usize) -> Taps {
    let scale = src_n as f64 / dst_n as f64;
    (0..dst_n)
        .map(|i| {
            let lo = i as f64 * scale;
            let hi = (lo + scale).min(src_n as f64);
            let span = hi - lo;
            let first = lo.floor() as usize;
            let last = (hi.ceil() as usize).min(src_n);
            (first..last)
                .filter_map(|j| {
                    let overlap = (j as f64 + 1.).min(hi) - (j as f64).max(lo);
                    (overlap > 0.).then(|| (j, (overlap / span) as f32))
                })
                .collect()
        })
        .collect()
}

pub fn apply_taps(src: &Plane, tx: &Taps, ty: &Taps) -> Plane {
    let (nw, nh) = (tx.len(), ty.len());
    let mut tmp = vec![0f32; nw * src.h];
    tmp.par_chunks_mut(nw).enumerate().for_each(|(y, row)| {
        let s = &src.d[y * src.w..(y + 1) * src.w];
        for (x, taps) in tx.iter().enumerate() {
            row[x] = taps.iter().map(|&(i, w)| s[i] * w).sum();
        }
    });
    let mut out = vec![0f32; nw * nh];
    out.par_chunks_mut(nw).enumerate().for_each(|(y, row)| {
        for &(j, w) in &ty[y] {
            let s = &tmp[j * nw..(j + 1) * nw];
            for x in 0..nw {
                row[x] += s[x] * w;
            }
        }
    });
    Plane { w: nw, h: nh, d: out }
}

/// cv2.resize(..., INTER_CUBIC)
pub fn resize_cubic(src: &Plane, nw: usize, nh: usize) -> Plane {
    apply_taps(src, &cubic_taps(src.w, nw), &cubic_taps(src.h, nh))
}

/// cv2.resize(..., INTER_AREA) for downscaling
pub fn resize_area(src: &Plane, nw: usize, nh: usize) -> Plane {
    apply_taps(src, &area_taps(src.w, nw), &area_taps(src.h, nh))
}

pub fn sample_cubic(p: &Plane, sx: f64, sy: f64) -> f32 {
    let (x0, y0) = (sx.floor(), sy.floor());
    let wx = cubic_w(sx - x0);
    let wy = cubic_w(sy - y0);
    let (x0, y0) = (x0 as isize, y0 as isize);
    let mut acc = 0f64;
    for j in 0..4 {
        let yy = reflect(y0 - 1 + j as isize, p.h as isize);
        let mut r = 0f64;
        for i in 0..4 {
            let xx = reflect(x0 - 1 + i as isize, p.w as isize);
            r += wx[i] * p.d[yy * p.w + xx] as f64;
        }
        acc += wy[j] * r;
    }
    acc as f32
}

/// cv2.warpAffine with getRotationMatrix2D(centre, angle, 1.0), INTER_CUBIC, REFLECT_101.
pub fn rotate(y: &Plane, angle: f64) -> Plane {
    if angle == 0. {
        return y.clone();
    }
    let (w, h) = (y.w, y.h);
    let (cx, cy) = ((w as f64 - 1.) / 2., (h as f64 - 1.) / 2.);
    let (al, be) = (angle.to_radians().cos(), angle.to_radians().sin());
    let (m00, m01, m02) = (al, be, (1. - al) * cx - be * cy);
    let (m10, m11, m12) = (-be, al, be * cx + (1. - al) * cy);
    let det = m00 * m11 - m01 * m10;
    let (i00, i01, i10, i11) = (m11 / det, -m01 / det, -m10 / det, m00 / det);
    let i02 = -(i00 * m02 + i01 * m12);
    let i12 = -(i10 * m02 + i11 * m12);
    let mut out = vec![0f32; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(py, row)| {
        for px in 0..w {
            let (fx, fy) = (px as f64, py as f64);
            row[px] = sample_cubic(y, i00 * fx + i01 * fy + i02, i10 * fx + i11 * fy + i12);
        }
    });
    Plane { w, h, d: out }
}

// --------------------------------------------------------------------------- //
// Embedding
// --------------------------------------------------------------------------- //
pub fn dct_basis(index: usize) -> [f32; BLOCK] {
    let mut out = [0f32; BLOCK];
    for (x, o) in out.iter_mut().enumerate() {
        *o = ((2.0 / BLOCK as f64).sqrt() * (PI * index as f64 * (x as f64 + 0.5) / BLOCK as f64).cos()) as f32;
    }
    out
}

/// Per-block weight in [0, 1] from the 256 luma samples of one block.
pub fn block_weight(block: &[f32]) -> f32 {
    let n = block.len() as f64;
    let mean = block.iter().map(|&v| v as f64).sum::<f64>() / n;
    let var = block.iter().map(|&v| (v as f64 - mean).powi(2)).sum::<f64>() / n;
    let (mean, std) = (mean as f32, var.sqrt() as f32);
    let not_paper = ((246.0 - mean) / 32.0).clamp(0., 1.);
    let textured = ((std - 5.0) / 20.0).clamp(0., 1.);
    let white = block.iter().filter(|&&v| v >= 235.0).count() as f32 / n as f32;
    let black = block.iter().filter(|&&v| v <= 100.0).count() as f32 / n as f32;
    let text_like = white > 0.40 && black > 0.015;
    if text_like {
        0.
    } else {
        not_paper * textured
    }
}

pub struct Embedded {
    pub rgb: Vec<u8>,
    pub modified: Vec<(usize, usize)>,
}

pub fn embed(rgb: &[u8], w: usize, h: usize, message: &str, seed: u64) -> Res<Embedded> {
    if h < MIN_EMBED_SIDE || w < MIN_EMBED_SIDE {
        return Err(format!("Image must be at least {MIN_EMBED_SIDE}x{MIN_EMBED_SIDE} pixels.").into());
    }
    let layout = layout_for(seed);
    let pat = pattern(message, &layout)?;
    let (bh, bw) = (h / BLOCK, w / BLOCK);
    let y = luma(rgb, w, h);
    let (b2, b3) = (dct_basis(COEFF_A.0), dct_basis(COEFF_A.1));
    debug_assert_eq!((COEFF_B.0, COEFF_B.1), (COEFF_A.1, COEFF_A.0));

    // Per-block corrections, computed in parallel over block rows.
    struct BlockOut {
        corr: f32,
        active: bool,
    }
    let rows: Vec<Vec<BlockOut>> = (0..bh)
        .into_par_iter()
        .map(|by| {
            let mut block = [0f32; BLOCK * BLOCK];
            (0..bw)
                .map(|bx| {
                    for i in 0..BLOCK {
                        let src = (by * BLOCK + i) * w + bx * BLOCK;
                        block[i * BLOCK..(i + 1) * BLOCK].copy_from_slice(&y.d[src..src + BLOCK]);
                    }
                    let weight = block_weight(&block);
                    if weight < EMBED_ACTIVE_WEIGHT {
                        return BlockOut { corr: 0., active: false };
                    }
                    // DCT coefficients (2,3) and (3,2) of this block (orthonormal 2-D DCT).
                    let (mut a, mut b) = (0f32, 0f32);
                    for i in 0..BLOCK {
                        let (mut ra, mut rb) = (0f32, 0f32);
                        for j in 0..BLOCK {
                            let v = block[i * BLOCK + j];
                            ra += v * b3[j];
                            rb += v * b2[j];
                        }
                        a += ra * b2[i];
                        b += rb * b3[i];
                    }
                    let wanted = pat[(by % TILE) * TILE + bx % TILE];
                    let shortfall = STRENGTH * weight - (a - b) * wanted;
                    BlockOut { corr: shortfall.max(0.) * wanted / 2., active: true }
                })
                .collect()
        })
        .collect();

    if !rows.iter().flatten().any(|b| b.active) {
        return Err("No suitable blocks: watermark was not embedded.".into());
    }

    let mut out = rgb.to_vec();
    let mut modified = Vec::new();
    for (by, row) in rows.iter().enumerate() {
        for (bx, blk) in row.iter().enumerate() {
            if !blk.active || blk.corr.abs() <= 1e-6 {
                continue;
            }
            modified.push((by, bx));
            for i in 0..BLOCK {
                for j in 0..BLOCK {
                    // a += corr on (2,3), b -= corr on (3,2)  =>  pixel delta in the spatial domain
                    let delta = blk.corr * (b2[i] * b3[j] - b3[i] * b2[j]);
                    let base = ((by * BLOCK + i) * w + bx * BLOCK + j) * 3;
                    for c in 0..3 {
                        let v = (out[base + c] as f32 + delta).round_ties_even().clamp(0., 255.);
                        out[base + c] = v as u8;
                    }
                }
            }
        }
    }
    Ok(Embedded { rgb: out, modified })
}

pub fn debug_overlay(rgb: &[u8], w: usize, modified: &[(usize, usize)]) -> Vec<u8> {
    let mut img = rgb.to_vec();
    let mut put = |x: usize, y: usize| {
        let o = (y * w + x) * 3;
        img[o] = 255;
        img[o + 1] = 0;
        img[o + 2] = 0;
    };
    for &(by, bx) in modified {
        let (x1, y1) = (bx * BLOCK, by * BLOCK);
        for k in 0..BLOCK {
            put(x1 + k, y1);
            put(x1 + k, y1 + BLOCK - 1);
            put(x1, y1 + k);
            put(x1 + BLOCK - 1, y1 + k);
        }
    }
    img
}

// --------------------------------------------------------------------------- //
// Detection
// --------------------------------------------------------------------------- //
/// DCT(2,3) minus DCT(3,2) of the 16x16 window centred at every pixel.
pub fn differences(y: &Plane) -> Plane {
    let (b2, b3) = (dct_basis(2), dct_basis(3));
    let a = filter_v(&filter_h(y, &b3, BLOCK / 2), &b2, BLOCK / 2);
    let b = filter_v(&filter_h(y, &b2, BLOCK / 2), &b3, BLOCK / 2);
    let d = a.d.iter().zip(&b.d).map(|(p, q)| p - q).collect();
    Plane { w: y.w, h: y.h, d }
}

/// Same 16x16 statistics as `block_weight`, evaluated at every candidate centre.
pub fn weight_map(y: &Plane) -> Plane {
    let mean = box_filter(y);
    let sq = box_filter(&Plane { w: y.w, h: y.h, d: y.d.iter().map(|v| v * v).collect() });
    let white = box_filter(&Plane { w: y.w, h: y.h, d: y.d.iter().map(|&v| (v >= 235.0) as u8 as f32).collect() });
    let black = box_filter(&Plane { w: y.w, h: y.h, d: y.d.iter().map(|&v| (v <= 100.0) as u8 as f32).collect() });
    let d = (0..y.d.len())
        .map(|i| {
            let m = mean.d[i];
            let std = (sq.d[i] - m * m).max(0.).sqrt();
            let not_paper = ((246. - m) / 32.).clamp(0., 1.);
            let textured = ((std - 5.) / 20.).clamp(0., 1.);
            let text_like = white.d[i] > 0.40 && black.d[i] > 0.015;
            let wgt = if text_like { 0. } else { not_paper * textured };
            if wgt >= DETECT_MIN_WEIGHT {
                wgt
            } else {
                0.
            }
        })
        .collect();
    Plane { w: y.w, h: y.h, d }
}

pub fn decode_tile(sums: &[f32], counts: &[f32], ty: usize, tx: usize, deep: bool, layout: &Layout) -> Option<String> {
    let mut s = vec![0f32; TILE * TILE];
    let mut c = vec![0f32; TILE * TILE];
    for i in 0..TILE {
        for j in 0..TILE {
            let src = ((i + ty) % TILE) * TILE + (j + tx) % TILE;
            s[i * TILE + j] = sums[src];
            c[i * TILE + j] = counts[src];
        }
    }
    let soft: Vec<f32> = layout.data_idx.iter().map(|&k| s[k] / c[k].max(1.)).collect();
    let bits: Vec<u8> = soft.iter().zip(&layout.mask_bits).map(|(&v, &m)| (v > 0.) as u8 ^ m).collect();
    if let Some(r) = decode_bits(&bits) {
        return Some(r);
    }

    let mut order: Vec<usize> = (0..soft.len()).collect();
    order.sort_by(|&a, &b| soft[a].abs().partial_cmp(&soft[b].abs()).unwrap_or(Ordering::Equal));
    let uncertain = &order[..if deep { 16 } else { 8 }];
    for &i in uncertain {
        let mut alt = bits.clone();
        alt[i] ^= 1;
        if let Some(r) = decode_bits(&alt) {
            return Some(r);
        }
    }
    if deep {
        for (n, &i) in uncertain[..12].iter().enumerate() {
            for &j in &uncertain[n + 1..12] {
                let mut alt = bits.clone();
                alt[i] ^= 1;
                alt[j] ^= 1;
                if let Some(r) = decode_bits(&alt) {
                    return Some(r);
                }
            }
        }
    }
    None
}

struct Cand {
    score: f32,
    sums: Vec<f32>,
    counts: Vec<f32>,
    sy: usize,
    sx: usize,
}

/// Search DCT block alignment and periodic pilot phase.
/// Returns (message or None, best pilot correlation score).
pub fn detect(y: &Plane, deep: bool, layout: &Layout) -> (Option<String>, f32) {
    if y.w.min(y.h) < MIN_DETECT_SIDE {
        return (None, 0.);
    }
    let diff = differences(y);
    let wm = weight_map(y);
    let (h, w) = (y.h, y.w);
    let phases = if deep { 10 } else { 2 };

    let per_oy: Vec<Vec<Cand>> = (0..BLOCK)
        .into_par_iter()
        .map(|oy| {
            let mut out = Vec::new();
            let rows: Vec<usize> = (BLOCK / 2 + oy..h - BLOCK / 2).step_by(BLOCK).collect();
            if rows.len() < 2 {
                return out;
            }
            for ox in 0..BLOCK {
                let cols: Vec<usize> = (BLOCK / 2 + ox..w - BLOCK / 2).step_by(BLOCK).collect();
                if cols.len() < 2 {
                    continue;
                }
                let mut sums = vec![0f32; TILE * TILE];
                let (mut sum_v, mut sum_sq) = (0f64, 0f64);
                for (i, &r) in rows.iter().enumerate() {
                    for (j, &c) in cols.iter().enumerate() {
                        let v = diff.d[r * w + c] * wm.d[r * w + c];
                        sums[(i % TILE) * TILE + j % TILE] += v;
                        sum_v += v as f64;
                        sum_sq += (v as f64) * (v as f64);
                    }
                }
                let n = (rows.len() * cols.len()) as f64;
                let mean = sum_v / n;
                let std = (sum_sq / n - mean * mean).max(0.).sqrt() as f32;

                let mut per_row = [0f32; TILE];
                let mut per_col = [0f32; TILE];
                for i in 0..rows.len() {
                    per_row[i % TILE] += 1.;
                }
                for j in 0..cols.len() {
                    per_col[j % TILE] += 1.;
                }
                let mut counts = vec![0f32; TILE * TILE];
                for r in 0..TILE {
                    for c in 0..TILE {
                        counts[r * TILE + c] = per_row[r] * per_col[c];
                    }
                }

                let corr = circ_corr(&sums, &layout.pilot_fft);
                let obs = circ_corr(&counts, &layout.pilot_abs_fft);
                let z: Vec<f32> = (0..TILE * TILE)
                    .map(|k| corr[k] / ((std + 1e-4) * obs[k].max(1.).sqrt()))
                    .collect();
                let mut idx: Vec<usize> = (0..TILE * TILE).collect();
                idx.sort_by(|&a, &b| z[b].partial_cmp(&z[a]).unwrap_or(Ordering::Equal));
                idx.truncate(phases);
                for &k in &idx {
                    if deep || z[k] > 2.5 {
                        out.push(Cand { score: z[k], sums: sums.clone(), counts: counts.clone(), sy: k / TILE, sx: k % TILE });
                    }
                }
            }
            out
        })
        .collect();

    let mut cands: Vec<Cand> = per_oy.into_iter().flatten().collect();
    cands.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
    let limit = if deep { 120 } else { 40 };
    for c in cands.iter().take(limit) {
        if let Some(r) = decode_tile(&c.sums, &c.counts, c.sy, c.sx, deep, layout) {
            return (Some(r), c.score);
        }
    }
    (None, cands.first().map(|c| c.score).unwrap_or(0.))
}

// --------------------------------------------------------------------------- //
// Extraction search over scale / rotation hypotheses
// --------------------------------------------------------------------------- //
pub const COMMON_SCALES: [f64; 13] = [
    1., 2., 1.333333333, 1.25, 1.428571429, 1.666666667, 1.176470588, 1.111111111, 1.5, 0.8, 0.666666667, 0.5, 1.2,
];
pub const ANGLES: [f64; 5] = [0., -0.5, 0.5, -1., 1.];
pub const MAX_SELECTED_SCALES: usize = 8;
pub const MIN_SCALE_SEPARATION: f64 = 0.02;

pub fn fine_scales() -> Vec<f64> {
    // np.arange(.75, 1.551, .005)
    let n = ((1.551f64 - 0.75) / 0.005).ceil() as usize;
    let mut v: Vec<f64> = (0..n).map(|i| 0.75 + i as f64 * 0.005).collect();
    v.sort_by(|a, b| a.ln().abs().partial_cmp(&b.ln().abs()).unwrap_or(Ordering::Equal));
    v
}

struct Searcher {
    y: Plane,
    deadline: Instant,
    last_progress: Instant,
    ranked: Vec<(f32, f64)>, // (pilot score, scale) from the fine scan
    cancel: Arc<AtomicBool>,
    layout: Arc<Layout>,
}

impl Searcher {
    fn new(y: Plane, seconds: u64, started: Instant, cancel: Arc<AtomicBool>, layout: Arc<Layout>) -> Self {
        Searcher { y, deadline: started + Duration::from_secs(seconds), last_progress: started, ranked: Vec::new(), cancel, layout }
    }

    fn expired(&self) -> bool {
        Instant::now() > self.deadline || self.cancel.load(AtomicOrdering::Relaxed)
    }

    fn check(&mut self, upright: &Plane, scale: f64, deep: bool, collect: bool) -> Option<String> {
        if self.expired() {
            return None;
        }
        let now = Instant::now();
        if now.duration_since(self.last_progress) >= Duration::from_secs(PROGRESS_INTERVAL) {
            eprintln!("[extract] searching watermark...");
            self.last_progress = now;
        }
        let width = (upright.w as f64 * scale).round_ties_even() as usize;
        let height = (upright.h as f64 * scale).round_ties_even() as usize;
        if height.min(width) < 128
            || (scale > 1. && height.max(width) > MAX_EXTRACT_SIDE)
            || height * width > MAX_PIXELS
        {
            return None;
        }
        let owned: Plane;
        let scaled: &Plane = if scale == 1. {
            upright
        } else {
            owned = if scale > 1. { resize_cubic(upright, width, height) } else { resize_area(upright, width, height) };
            &owned
        };
        let (found, strength) = detect(scaled, deep, &self.layout);
        if collect {
            self.ranked.push((strength, scale));
        }
        found
    }

    /// Try every (angle, scale, offset) combination in order; stop at the first hit.
    fn sweep(&mut self, scales: &[f64], angles: &[f64], deep: bool, collect: bool, offsets: &[f64]) -> Option<String> {
        for &angle in angles {
            if self.expired() {
                return None;
            }
            let upright = rotate(&self.y, angle);
            for &scale in scales {
                for &offset in offsets {
                    if self.expired() {
                        return None;
                    }
                    if let Some(r) = self.check(&upright, scale + offset, deep, collect) {
                        return Some(r);
                    }
                }
            }
        }
        None
    }

    fn best_scales(&self) -> Vec<f64> {
        let mut ranked = self.ranked.clone();
        ranked.sort_by(|a, b| b.partial_cmp(a).unwrap_or(Ordering::Equal));
        let mut selected: Vec<f64> = Vec::new();
        for (_, scale) in ranked {
            if selected.iter().all(|old| (scale - old).abs() >= MIN_SCALE_SEPARATION) {
                selected.push(scale);
            }
            if selected.len() == MAX_SELECTED_SCALES {
                break;
            }
        }
        selected
    }
}

pub fn extract(rgb: &[u8], w: usize, h: usize, seed: u64) -> Res<Option<String>> {
    extract_with_cancel(rgb, w, h, seed, Arc::new(AtomicBool::new(false)))
}

/// Like `extract`, but the search stops early (returning `Ok(None)`) once `cancel` is set.
pub fn extract_with_cancel(rgb: &[u8], w: usize, h: usize, seed: u64, cancel: Arc<AtomicBool>) -> Res<Option<String>> {
    if w * h > MAX_PIXELS {
        return Err(format!("Image exceeds {MAX_PIXELS} pixels.").into());
    }
    let started = Instant::now();
    let mut y = luma(rgb, w, h);
    if y.w.max(y.h) > MAX_EXTRACT_SIDE {
        let f = MAX_EXTRACT_SIDE as f64 / y.w.max(y.h) as f64;
        let (nw, nh) = ((y.w as f64 * f).round_ties_even() as usize, (y.h as f64 * f).round_ties_even() as usize);
        y = resize_area(&y, nw, nh);
    }
    let mut s = Searcher::new(y, SEARCH_SECONDS, started, cancel, layout_for(seed));

    macro_rules! stage {
        ($e:expr) => {
            if let Some(r) = $e {
                return Ok(Some(r));
            }
            if s.expired() {
                return Ok(None);
            }
        };
    }

    // fast: common ratios, deskew
    stage!(s.sweep(&COMMON_SCALES, &ANGLES, false, false, &[0.]));
    // deep: more candidates
    stage!(s.sweep(&COMMON_SCALES, &ANGLES, true, false, &[0.]));
    // fine scale scan, no rotation
    let fine: Vec<f64> = fine_scales()
        .into_iter()
        .filter(|f| COMMON_SCALES.iter().map(|c| (f - c).abs()).fold(f64::INFINITY, f64::min) >= 0.003)
        .collect();
    stage!(s.sweep(&fine, &[0.], false, true, &[0.]));
    // promising scales, deep
    let best = s.best_scales();
    stage!(s.sweep(&best, &[0.], true, false, &[0.]));
    stage!(s.sweep(&best, &ANGLES[1..], true, false, &[-0.01, 0., 0.01]));
    Ok(None)
}

// --------------------------------------------------------------------------- //
// Image I/O
// --------------------------------------------------------------------------- //
#[derive(Clone, Copy)]
pub enum Fmt {
    Png,
    Jpeg,
}

pub fn output_format(path: &Path) -> Res<Fmt> {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("png") => Ok(Fmt::Png),
        Some("jpg") | Some("jpeg") => Ok(Fmt::Jpeg),
        _ => Err("Output must be a PNG or JPEG file.".into()),
    }
}

pub fn load(path: &Path) -> Res<(Vec<u8>, usize, usize)> {
    let reader = ImageReader::open(path)?.with_guessed_format()?;
    let mut decoder = reader.into_decoder()?;
    let (w, h) = decoder.dimensions();
    if w as u64 * h as u64 > MAX_PIXELS as u64 {
        return Err(format!("Image exceeds {MAX_PIXELS} pixels.").into());
    }
    let orientation = decoder.orientation()?;
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    let (w, h) = (img.width() as usize, img.height() as usize);
    let rgb = if img.color().has_alpha() {
        // Flatten transparency onto white.
        let rgba = img.to_rgba8();
        let mut out = Vec::with_capacity(w * h * 3);
        for px in rgba.pixels() {
            let a = px[3] as u32;
            for c in 0..3 {
                out.push(((px[c] as u32 * a + 255 * (255 - a) + 127) / 255) as u8);
            }
        }
        out
    } else {
        img.to_rgb8().into_raw()
    };
    Ok((rgb, w, h))
}

pub fn encode_image(rgb: &[u8], w: usize, h: usize, fmt: Fmt) -> Res<Vec<u8>> {
    let mut buf = Vec::new();
    match fmt {
        Fmt::Png => PngEncoder::new(&mut buf).write_image(rgb, w as u32, h as u32, ExtendedColorType::Rgb8)?,
        Fmt::Jpeg => {
            JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY).write_image(rgb, w as u32, h as u32, ExtendedColorType::Rgb8)?
        }
    }
    Ok(buf)
}

/// Write `data` to a file that must not exist yet; remove it again on failure.
pub fn write_new(data: &[u8], output: &Path) -> Res<()> {
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut file = OpenOptions::new().write(true).create_new(true).open(output)?;
    if let Err(e) = file.write_all(data) {
        drop(file);
        let _ = std::fs::remove_file(output);
        return Err(e.into());
    }
    Ok(())
}

// --------------------------------------------------------------------------- //
// Tests
// --------------------------------------------------------------------------- //
#[cfg(test)]
mod tests {
    use super::*;

    fn textured(w: usize, h: usize) -> Vec<u8> {
        let mut state = 12345u32;
        let mut out = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                let noise = ((state >> 24) as f32 / 255.0 - 0.5) * 30.0;
                let base = 128.0 + 40.0 * (x as f32 * 0.13).sin() * (y as f32 * 0.11).cos();
                let v = (base + noise).clamp(0., 255.) as u8;
                out.extend_from_slice(&[v, v.saturating_add(5), v.saturating_sub(5)]);
            }
        }
        out
    }

    #[test]
    fn layout_sizes() {
        let l = layout_for(DEFAULT_SEED);
        assert_eq!(l.data_idx.len(), CODE_BITS);
        assert_eq!(l.mask_bits.len(), CODE_BITS);
        assert_eq!(l.pilot_template.iter().filter(|v| **v != 0.).count(), PILOTS);
    }

    #[test]
    fn frame_hamming_roundtrip() {
        for msg in ["a", "Short text", "0123456789abcdef", "Привет"] {
            let coded = encode_hamming(&frame(msg).unwrap());
            assert_eq!(coded.len(), CODE_BITS);
            assert_eq!(decode_bits(&coded).as_deref(), Some(msg));
            let mut damaged = coded.clone();
            for k in 0..17 {
                damaged[k * 15 + 3] ^= 1; // one error per codeword is corrected
            }
            assert_eq!(decode_bits(&damaged).as_deref(), Some(msg));
        }
    }

    #[test]
    fn embed_then_extract() {
        let (w, h) = (640, 480);
        let src = textured(w, h);
        let emb = embed(&src, w, h, "Short text", DEFAULT_SEED).unwrap();
        let (found, _) = detect(&luma(&emb.rgb, w, h), true, &layout_for(DEFAULT_SEED));
        assert_eq!(found.as_deref(), Some("Short text"));
        assert_eq!(extract(&emb.rgb, w, h, DEFAULT_SEED).unwrap().as_deref(), Some("Short text"));
    }

    #[test]
    fn extract_after_resize() {
        let (w, h) = (640, 480);
        let emb = embed(&textured(w, h), w, h, "scaled", DEFAULT_SEED).unwrap();
        let y = luma(&emb.rgb, w, h);
        let big = resize_cubic(&y, 800, 600); // x1.25 is one of the common scales
        let (found, _) = detect(&resize_area(&big, w, h), true, &layout_for(DEFAULT_SEED));
        assert_eq!(found.as_deref(), Some("scaled"));
    }

    #[test]
    fn custom_seed_roundtrip_and_wrong_seed() {
        let (w, h) = (640, 480);
        let seed = 0xC0FFEE;
        let emb = embed(&textured(w, h), w, h, "seeded", seed).unwrap();
        let y = luma(&emb.rgb, w, h);
        assert_eq!(detect(&y, true, &layout_for(seed)).0.as_deref(), Some("seeded"));
        // A different seed selects a different layout, so nothing is found.
        assert_eq!(detect(&y, true, &layout_for(DEFAULT_SEED)).0, None);
    }

    #[test]
    fn seed_parsing() {
        assert_eq!(parse_seed("57A73"), Ok(DEFAULT_SEED));
        assert_eq!(parse_seed(" 0x57a73 "), Ok(DEFAULT_SEED));
        assert_eq!(parse_seed("FFFFFFFFFFFFFFFF"), Ok(u64::MAX));
        assert!(parse_seed("").is_err());
        assert!(parse_seed("12345678901234567").is_err());
        assert!(parse_seed("+5").is_err());
        assert!(parse_seed("xyz").is_err());
    }
}

