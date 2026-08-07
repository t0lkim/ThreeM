//! Offline fixture harness for the `ThreeM` integration suites.
//!
//! Everything here is synthesised at runtime from bytes we build ourselves —
//! there are no checked-in binary test assets and no network access. That
//! matters because the suites below drive destructive code paths against
//! real files, so the inputs have to be reproducible on any machine, in CI,
//! offline, forever.
//!
//! Three pieces:
//!
//! * [`MediaTree`] — a fluent builder over a [`TempDir`] that declares a
//!   media tree file by file.
//! * A minimal JPEG synthesiser — a genuinely byte-valid 1x1 baseline JPEG
//!   carrying a hand-built EXIF APP1 segment (TIFF header, IFD0, Exif
//!   `SubIFD` with `DateTimeOriginal`, optional GPS IFD).
//! * [`snapshot_tree`] / [`file_contents_by_marker`] — golden-tree
//!   assertion helpers.
//!
//! ## Markers
//!
//! Every file the builder creates carries an embedded marker of the form
//! `MMMTEST:<declared relative path>;`. For synthesised JPEGs it lives in a
//! JPEG `COM` segment (which keeps the file valid and leaves EXIF alone);
//! for the raw byte variants it is appended to the caller's bytes. The
//! marker is what lets a test prove that *this specific* source file landed
//! at *that specific* destination, rather than merely that a file with the
//! expected name exists there. See [`file_contents_by_marker`].
//!
//! [`TempDir`]: tempfile::TempDir

// This module is shared by several integration-test crates, and no single
// crate uses all of it. Without this, the unused half fails the `-D warnings`
// gate in whichever suite happens not to touch it.
#![allow(
    dead_code,
    reason = "shared across integration suites; each uses only the part it needs"
)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test fixture is a failing test, which is the desired signal"
)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;
use tempfile::TempDir;
use walkdir::WalkDir;

/// Prefix of the embedded provenance marker. The marker runs to the next `;`.
const MARKER_PREFIX: &[u8] = b"MMMTEST:";

// ---------------------------------------------------------------------------
// MediaTree
// ---------------------------------------------------------------------------

/// A temporary directory populated with synthetic media, declared fluently.
///
/// The directory is removed when the `MediaTree` is dropped, so tests must
/// keep it alive for as long as they need the files:
///
/// ```ignore
/// let tree = MediaTree::new()
///     .jpeg_with_exif("holiday/beach.jpg", naive(2024, 1, 15, 14, 30, 0), None)
///     .jpeg_with_exif("holiday/paris.jpg", naive(2024, 1, 15, 14, 30, 0), Some((48.8584, 2.2945)))
///     .non_media("notes.txt", b"do not touch me");
/// let files = snapshot_tree(tree.path());
/// ```
pub struct MediaTree {
    root: TempDir,
}

impl Default for MediaTree {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaTree {
    /// Create an empty tree backed by a fresh temporary directory.
    pub fn new() -> Self {
        Self {
            root: TempDir::new().expect("creating fixture TempDir"),
        }
    }

    /// The root of the tree. Pass this to the binary or to `scan_directories`.
    pub fn path(&self) -> &Path {
        self.root.path()
    }

    /// Absolute path of a file previously declared at `rel`.
    pub fn join(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }

    /// A byte-valid 1x1 baseline JPEG carrying EXIF `DateTimeOriginal`, and
    /// GPS latitude/longitude when `gps` is `Some`.
    ///
    /// The datetime is written verbatim as EXIF ASCII (`YYYY:MM:DD HH:MM:SS`),
    /// so a test can assert the exact value round-trips.
    pub fn jpeg_with_exif(
        self,
        rel: &str,
        datetime: NaiveDateTime,
        gps: Option<(f64, f64)>,
    ) -> Self {
        let bytes = synth_jpeg(Some((datetime, gps)), rel);
        self.write(rel, &bytes)
    }

    /// A `.jpg` (or any extension the caller picks) holding arbitrary bytes —
    /// no EXIF, not a decodable image. Use this for the "unparseable" paths.
    ///
    /// The provenance marker is appended to `bytes`.
    pub fn jpeg_raw(self, rel: &str, bytes: &[u8]) -> Self {
        let body = with_marker(bytes, rel);
        self.write(rel, &body)
    }

    /// A video file holding arbitrary bytes. Video-ness comes from the
    /// extension in `rel` (`.mov`, `.mp4`, …) — that is all the scanner reads.
    ///
    /// The provenance marker is appended to `bytes`.
    pub fn video(self, rel: &str, bytes: &[u8]) -> Self {
        let body = with_marker(bytes, rel);
        self.write(rel, &body)
    }

    /// A non-media file (`.txt`, `.pdf`, …) that the organiser must never touch.
    ///
    /// The provenance marker is appended to `bytes`.
    pub fn non_media(self, rel: &str, bytes: &[u8]) -> Self {
        let body = with_marker(bytes, rel);
        self.write(rel, &body)
    }

    /// A byte-identical copy of a file already declared at `existing`.
    ///
    /// Being byte-identical, it necessarily carries the *same* marker as its
    /// source — which is why [`file_contents_by_marker`] maps a marker to a
    /// list of paths rather than to one path. Two files that hash the same
    /// are genuinely indistinguishable; the harness does not pretend
    /// otherwise.
    pub fn duplicate_of(self, rel: &str, existing: &str) -> Self {
        let src = self.root.path().join(existing);
        let bytes = fs::read(&src)
            .unwrap_or_else(|e| panic!("reading fixture {} to duplicate: {e}", src.display()));
        self.write(rel, &bytes)
    }

    /// Create an empty directory (the scanner should walk straight past it).
    pub fn empty_dir(self, rel: &str) -> Self {
        let dir = self.root.path().join(rel);
        fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("creating fixture dir {}: {e}", dir.display()));
        self
    }

    fn write(self, rel: &str, bytes: &[u8]) -> Self {
        let path = self.root.path().join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|e| panic!("creating fixture dir {}: {e}", parent.display()));
        }
        fs::write(&path, bytes)
            .unwrap_or_else(|e| panic!("writing fixture {}: {e}", path.display()));
        self
    }
}

/// Terse `NaiveDateTime` constructor for fixture declarations.
///
/// Panics on an invalid date — which in a test is exactly the right outcome.
pub fn naive(year: i32, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(year, month, day)
        .unwrap_or_else(|| panic!("invalid fixture date {year}-{month}-{day}"))
        .and_hms_opt(hour, min, sec)
        .unwrap_or_else(|| panic!("invalid fixture time {hour}:{min}:{sec}"))
}

// ---------------------------------------------------------------------------
// Golden-tree helpers
// ---------------------------------------------------------------------------

/// Sorted, `/`-separated paths of every file under `root`, relative to `root`.
///
/// Directories are omitted — an empty directory is not an observable outcome
/// worth asserting on, and including them would make the snapshots noisy.
pub fn snapshot_tree(root: &Path) -> Vec<String> {
    let mut out: Vec<String> = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| relative_slug(root, e.path()))
        .collect();
    out.sort();
    out
}

/// Like [`snapshot_tree`], but each entry is `"<path>  <blake3 hex>"`.
///
/// `snapshot_tree` alone proves the *shape* of a tree; this proves its
/// *content*. Use it for the "a default run leaves the input byte-identical"
/// assertion, where equal path lists would not actually establish the claim.
pub fn snapshot_tree_hashed(root: &Path) -> Vec<String> {
    let mut out: Vec<String> = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| {
            let bytes = fs::read(e.path())
                .unwrap_or_else(|err| panic!("reading {}: {err}", e.path().display()));
            let hash = blake3::hash(&bytes).to_hex().to_string();
            format!("{}  {hash}", relative_slug(root, e.path()))
        })
        .collect();
    out.sort();
    out
}

/// Map every marked file under `root` to where it now lives.
///
/// The key is the marker — the relative path the file was *declared* at — and
/// the value is the sorted list of relative paths that currently carry it.
/// One entry means one file; more than one means duplicates of the same
/// bytes. Files with no marker (anything the harness did not create, such as
/// a `manifest.txt` written by the organiser) are absent.
///
/// This is how a test proves a *specific* file reached a destination:
///
/// ```ignore
/// let landed = file_contents_by_marker(out_dir);
/// assert_eq!(landed["holiday/beach.jpg"], ["2024/01/15/2024-01-15-143000.jpg"]);
/// ```
pub fn file_contents_by_marker(root: &Path) -> BTreeMap<String, Vec<String>> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
    {
        if let Some(marker) = marker_of(entry.path()) {
            map.entry(marker)
                .or_default()
                .push(relative_slug(root, entry.path()));
        }
    }

    for paths in map.values_mut() {
        paths.sort();
    }
    map
}

/// Read the embedded provenance marker out of a file, if it has one.
pub fn marker_of(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let start = find_subslice(&bytes, MARKER_PREFIX)? + MARKER_PREFIX.len();
    let end = start + bytes[start..].iter().position(|&b| b == b';')?;
    String::from_utf8(bytes[start..end].to_vec()).ok()
}

fn relative_slug(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn marker_bytes(rel: &str) -> Vec<u8> {
    let mut out = MARKER_PREFIX.to_vec();
    out.extend_from_slice(rel.as_bytes());
    out.push(b';');
    out
}

fn with_marker(bytes: &[u8], rel: &str) -> Vec<u8> {
    let mut out = bytes.to_vec();
    out.extend_from_slice(&marker_bytes(rel));
    out
}

// ---------------------------------------------------------------------------
// Permission helpers
// ---------------------------------------------------------------------------

/// Restores a path's permission bits when dropped.
///
/// A test that makes something unreadable has to put it back, or `TempDir`
/// cannot clean up — and it has to do so even when the assertion in the middle
/// panics, which is the normal outcome while a defect is unfixed.
#[cfg(unix)]
pub struct RestorePerms {
    path: PathBuf,
    mode: u32,
}

#[cfg(unix)]
impl Drop for RestorePerms {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(self.mode));
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> RestorePerms {
    use std::os::unix::fs::PermissionsExt as _;

    let original = fs::metadata(path)
        .unwrap_or_else(|e| panic!("reading permissions of {}: {e}", path.display()))
        .permissions()
        .mode();
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .unwrap_or_else(|e| panic!("setting mode {mode:o} on {}: {e}", path.display()));
    RestorePerms {
        path: path.to_path_buf(),
        mode: original,
    }
}

/// Make `dir` read-only, returning a guard that restores it — or `None` when
/// the mode does not actually deny writes.
///
/// Running as root (which some container-based CI images do) ignores the
/// permission bits entirely, and a test that silently asserts nothing is worse
/// than a test that says why it stood down. The probe is a measurement, not an
/// assumption about the runner.
#[cfg(unix)]
pub fn deny_writes(dir: &Path) -> Option<RestorePerms> {
    let guard = set_mode(dir, 0o555);

    let probe = dir.join(".write-probe");
    if fs::write(&probe, b"probe").is_ok() {
        let _ = fs::remove_file(&probe);
        return None;
    }
    Some(guard)
}

/// Make `path` — a file or a directory — unreadable, returning a guard that
/// restores it, or `None` when the mode does not actually deny reads.
///
/// Same measured stand-down as [`deny_writes`], with the probe matched to what
/// `path` is: opening a file, listing a directory.
#[cfg(unix)]
pub fn deny_reads(path: &Path) -> Option<RestorePerms> {
    let is_dir = path.is_dir();
    let guard = set_mode(path, 0o000);

    let still_readable = if is_dir {
        fs::read_dir(path).is_ok()
    } else {
        fs::File::open(path).is_ok()
    };
    if still_readable {
        return None;
    }
    Some(guard)
}

// ---------------------------------------------------------------------------
// JPEG synthesiser
// ---------------------------------------------------------------------------

/// Assemble a byte-valid 1x1 greyscale baseline JPEG.
///
/// Segment order: `SOI`, `APP1` (EXIF, when requested), `COM` (marker),
/// `DQT`, `SOF0`, `DHT` x2, `SOS`, one byte of entropy-coded data, `EOI`.
/// The image decodes; it is simply a single black pixel.
fn synth_jpeg(exif: Option<(NaiveDateTime, Option<(f64, f64)>)>, marker: &str) -> Vec<u8> {
    let mut jpeg: Vec<u8> = vec![0xFF, 0xD8]; // SOI

    if let Some((datetime, gps)) = exif {
        let tiff = build_tiff(datetime, gps);
        let payload_len = 6 + tiff.len(); // "Exif\0\0" + TIFF block
        let seg_len = u16::try_from(payload_len + 2).expect("EXIF segment fits in a JPEG marker");
        jpeg.extend_from_slice(&[0xFF, 0xE1]);
        jpeg.extend_from_slice(&seg_len.to_be_bytes());
        jpeg.extend_from_slice(b"Exif\0\0");
        jpeg.extend_from_slice(&tiff);
    }

    // COM — carries the provenance marker without disturbing EXIF.
    let comment = marker_bytes(marker);
    let com_len = u16::try_from(comment.len() + 2).expect("marker fits in a COM segment");
    jpeg.extend_from_slice(&[0xFF, 0xFE]);
    jpeg.extend_from_slice(&com_len.to_be_bytes());
    jpeg.extend_from_slice(&comment);

    // DQT — Annex K luminance quantisation table, table id 0.
    jpeg.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]);
    jpeg.extend_from_slice(&LUMA_QUANT_TABLE);

    // SOF0 — baseline, 8-bit, 1x1, one component, sampling 1x1, quant table 0.
    jpeg.extend_from_slice(&[
        0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01, 0x00, 0x01, 0x01, 0x01, 0x11, 0x00,
    ]);

    // DHT — DC table 0.
    let dc_len = u16::try_from(2 + 1 + 16 + DC_LUMA_VALUES.len()).unwrap();
    jpeg.extend_from_slice(&[0xFF, 0xC4]);
    jpeg.extend_from_slice(&dc_len.to_be_bytes());
    jpeg.push(0x00); // class 0 (DC), table id 0
    jpeg.extend_from_slice(&DC_LUMA_COUNTS);
    jpeg.extend_from_slice(&DC_LUMA_VALUES);

    // DHT — AC table 0.
    let ac_len = u16::try_from(2 + 1 + 16 + AC_LUMA_VALUES.len()).unwrap();
    jpeg.extend_from_slice(&[0xFF, 0xC4]);
    jpeg.extend_from_slice(&ac_len.to_be_bytes());
    jpeg.push(0x10); // class 1 (AC), table id 0
    jpeg.extend_from_slice(&AC_LUMA_COUNTS);
    jpeg.extend_from_slice(&AC_LUMA_VALUES);

    // SOS — one component, DC table 0 / AC table 0, spectral selection 0..63.
    jpeg.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]);

    // The single MCU: DC category 0 (code `00`) then EOB (code `1010`),
    // padded to a byte with 1-bits -> 0b0010_1011.
    jpeg.push(0x2B);

    jpeg.extend_from_slice(&[0xFF, 0xD9]); // EOI
    jpeg
}

// TIFF field types.
const TY_BYTE: u16 = 1;
const TY_ASCII: u16 = 2;
const TY_LONG: u16 = 4;
const TY_RATIONAL: u16 = 5;
const TY_UNDEFINED: u16 = 7;

/// Build the TIFF block that sits inside the `APP1` segment, little-endian.
///
/// Layout: TIFF header, IFD0 (pointers only), Exif `SubIFD`, optional GPS IFD,
/// then a data area holding the values too large to sit inline in a 4-byte
/// field. Pointer fields are written as zero and patched once their target
/// offset is known — hand-computing them is where these things go wrong.
fn build_tiff(datetime: NaiveDateTime, gps: Option<(f64, f64)>) -> Vec<u8> {
    let mut t: Vec<u8> = Vec::new();

    // --- TIFF header: little-endian, magic 42, IFD0 at offset 8 ---
    t.extend_from_slice(b"II");
    t.extend_from_slice(&0x002Au16.to_le_bytes());
    t.extend_from_slice(&8u32.to_le_bytes());

    // --- IFD0: nothing but pointers into the sub-IFDs ---
    let ifd0_entries: u16 = if gps.is_some() { 2 } else { 1 };
    t.extend_from_slice(&ifd0_entries.to_le_bytes());

    let exif_ptr_at = t.len() + 8;
    push_entry(&mut t, 0x8769, TY_LONG, 1, [0; 4]); // ExifIFDPointer

    let gps_ptr_at = gps.map(|_| {
        let at = t.len() + 8;
        push_entry(&mut t, 0x8825, TY_LONG, 1, [0; 4]); // GPSInfoIFDPointer
        at
    });

    t.extend_from_slice(&0u32.to_le_bytes()); // no IFD1

    // --- Exif SubIFD ---
    let exif_ifd_off = u32::try_from(t.len()).unwrap();
    patch_u32(&mut t, exif_ptr_at, exif_ifd_off);

    t.extend_from_slice(&3u16.to_le_bytes());
    push_entry(&mut t, 0x9000, TY_UNDEFINED, 4, *b"0231"); // ExifVersion 2.31
    let datetime_ptr_at = t.len() + 8;
    push_entry(&mut t, 0x9003, TY_ASCII, 20, [0; 4]); // DateTimeOriginal
    let offset_ptr_at = t.len() + 8;
    push_entry(&mut t, 0x9011, TY_ASCII, 7, [0; 4]); // OffsetTimeOriginal
    t.extend_from_slice(&0u32.to_le_bytes());

    // --- GPS IFD ---
    let mut gps_value_ptrs = None;
    if let Some((lat, lon)) = gps {
        let gps_ifd_off = u32::try_from(t.len()).unwrap();
        patch_u32(&mut t, gps_ptr_at.unwrap(), gps_ifd_off);

        let lat_ref = if lat < 0.0 { b'S' } else { b'N' };
        let lon_ref = if lon < 0.0 { b'W' } else { b'E' };

        t.extend_from_slice(&5u16.to_le_bytes());
        push_entry(&mut t, 0x0000, TY_BYTE, 4, [2, 3, 0, 0]); // GPSVersionID
        push_entry(&mut t, 0x0001, TY_ASCII, 2, [lat_ref, 0, 0, 0]); // GPSLatitudeRef
        let lat_ptr_at = t.len() + 8;
        push_entry(&mut t, 0x0002, TY_RATIONAL, 3, [0; 4]); // GPSLatitude
        push_entry(&mut t, 0x0003, TY_ASCII, 2, [lon_ref, 0, 0, 0]); // GPSLongitudeRef
        let lon_ptr_at = t.len() + 8;
        push_entry(&mut t, 0x0004, TY_RATIONAL, 3, [0; 4]); // GPSLongitude
        t.extend_from_slice(&0u32.to_le_bytes());

        gps_value_ptrs = Some((lat_ptr_at, lon_ptr_at, lat, lon));
    }

    // --- Data area ---
    let datetime_off = u32::try_from(t.len()).unwrap();
    patch_u32(&mut t, datetime_ptr_at, datetime_off);
    let stamp = datetime.format("%Y:%m:%d %H:%M:%S").to_string();
    debug_assert_eq!(stamp.len(), 19, "EXIF datetime is a fixed 19 chars + NUL");
    t.extend_from_slice(stamp.as_bytes());
    t.push(0); // NUL terminator -> 20 bytes, keeping the next offset even

    // OffsetTimeOriginal pins the naive stamp above to UTC. Without it,
    // `nom-exif` resolves the timestamp against the *machine's* local
    // timezone, so the same fixture would organise into a different
    // directory on a developer's laptop than in CI. See the module note.
    let offset_off = u32::try_from(t.len()).unwrap();
    patch_u32(&mut t, offset_ptr_at, offset_off);
    t.extend_from_slice(b"+00:00\0");
    t.push(0); // pad to 8 bytes so the following offsets stay even

    if let Some((lat_ptr_at, lon_ptr_at, lat, lon)) = gps_value_ptrs {
        let lat_off = u32::try_from(t.len()).unwrap();
        patch_u32(&mut t, lat_ptr_at, lat_off);
        t.extend_from_slice(&dms_rationals(lat));

        let lon_off = u32::try_from(t.len()).unwrap();
        patch_u32(&mut t, lon_ptr_at, lon_off);
        t.extend_from_slice(&dms_rationals(lon));
    }

    t
}

fn push_entry(buf: &mut Vec<u8>, tag: u16, ty: u16, count: u32, value: [u8; 4]) {
    buf.extend_from_slice(&tag.to_le_bytes());
    buf.extend_from_slice(&ty.to_le_bytes());
    buf.extend_from_slice(&count.to_le_bytes());
    buf.extend_from_slice(&value);
}

fn patch_u32(buf: &mut [u8], at: usize, value: u32) {
    buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// Encode a signed decimal degree as the three unsigned EXIF rationals
/// (degrees, minutes, seconds). The sign lives in the separate `Ref` field.
///
/// Seconds carry a denominator of 10000, so the round-trip error is under
/// 1e-7 degrees — three orders of magnitude tighter than the 0.0001 the
/// integration tests assert on.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "values are bounded by 180 degrees / 60 minutes / 600000 sec-ten-thousandths and taken from a non-negative float"
)]
fn dms_rationals(decimal: f64) -> [u8; 24] {
    let abs = decimal.abs();
    let degrees = abs.trunc();
    let minutes_f = (abs - degrees) * 60.0;
    let minutes = minutes_f.trunc();
    let seconds_f = (minutes_f - minutes) * 60.0;

    let deg = degrees as u32;
    let min = minutes as u32;
    let sec = (seconds_f * 10_000.0).round() as u32;

    let mut out = [0u8; 24];
    for (slot, (num, den)) in
        out.chunks_exact_mut(8)
            .zip([(deg, 1u32), (min, 1u32), (sec, 10_000u32)])
    {
        slot[..4].copy_from_slice(&num.to_le_bytes());
        slot[4..].copy_from_slice(&den.to_le_bytes());
    }
    out
}

// --- Baseline JPEG tables (ITU T.81 Annex K) -------------------------------

#[rustfmt::skip]
const LUMA_QUANT_TABLE: [u8; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61,
    12, 12, 14, 19, 26, 58, 60, 55,
    14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62,
    18, 22, 37, 56, 68, 109, 103, 77,
    24, 35, 55, 64, 81, 104, 113, 92,
    49, 64, 78, 87, 103, 121, 120, 101,
    72, 92, 95, 98, 112, 100, 103, 99,
];

const DC_LUMA_COUNTS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_LUMA_VALUES: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

const AC_LUMA_COUNTS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7D];

#[rustfmt::skip]
const AC_LUMA_VALUES: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12,
    0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xA1, 0x08,
    0x23, 0x42, 0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0A, 0x16,
    0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2A, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39,
    0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59,
    0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79,
    0x7A, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98,
    0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7,
    0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6,
    0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4, 0xC5,
    0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4,
    0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xE1, 0xE2,
    0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA,
    0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8,
    0xF9, 0xFA,
];
