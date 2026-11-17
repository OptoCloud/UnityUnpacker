//! Core `.unitypackage` reading and extraction.
//!
//! A `.unitypackage` is a gzipped tar archive with one directory per asset GUID:
//!
//! ```text
//! <guid>/asset        the file contents (absent for folders)
//! <guid>/asset.meta   Unity's .meta sidecar
//! <guid>/pathname     the project-relative destination, e.g. "Assets/Foo/Bar.png"
//! <guid>/preview.png  optional thumbnail (ignored)
//! ```
//!
//! Tar order usually puts `pathname` *after* `asset`, so the destination of an asset is
//! unknown while its bytes stream past. [`load`] therefore reads the whole package once
//! and returns a [`Loaded`]: the file list, plus each payload in one of three places:
//!
//! * in memory,
//! * in a temp directory, or
//! * not loaded, still only in the package.
//!
//! A [`LoadPolicy`] decides which, and that is all the extraction modes are: two pass
//! loads nothing, single pass keeps small files in memory and the rest in the temp
//! directory, in-memory keeps everything in RAM. [`Loaded::extract`] then writes the
//! chosen entries: loaded payloads are written or moved into place, and anything not
//! loaded is streamed out of the package in a second pass.

mod memory;
pub mod settings;

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use flate2::read::GzDecoder;
use tempfile::TempDir;

/// One asset (file or folder) described by the package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub guid: String,
    /// Sanitized, `/`-separated project-relative path (e.g. `Assets/Foo/Bar.png`).
    pub path: String,
    /// Size of the asset payload in bytes (0 for folders).
    pub size: u64,
    pub has_asset: bool,
    pub has_meta: bool,
}

impl Entry {
    /// Unity folders are stored as a GUID with a `.meta` but no `asset` payload.
    pub fn is_folder(&self) -> bool {
        !self.has_asset
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub files: usize,
    pub folders: usize,
    pub metas: usize,
    pub bytes: u64,
}

/// Progress notifications emitted while loading or extracting.
pub enum Progress<'a> {
    /// How much of the work is done, from 0 to 1.
    Fraction(f64),
    /// An asset file is about to be written (extract only).
    Writing(&'a str),
}

#[derive(Debug)]
pub enum Error {
    Io { context: String, source: io::Error },
    Cancelled,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { context, source } => write!(f, "{context}: {source}"),
            Error::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            Error::Cancelled => None,
        }
    }
}

trait Context<T> {
    fn ctx(self, context: impl FnOnce() -> String) -> Result<T, Error>;
}

impl<T> Context<T> for io::Result<T> {
    fn ctx(self, context: impl FnOnce() -> String) -> Result<T, Error> {
        self.map_err(|source| Error::Io {
            context: context(),
            source,
        })
    }
}

/// Turns a raw `pathname` payload into a safe relative path, or `None` if it would
/// escape the output directory (absolute, drive-qualified, or containing `..`).
pub fn sanitize_pathname(raw: &str) -> Option<String> {
    // Some exporters append extra lines (e.g. "00") after the path.
    let first = raw
        .lines()
        .next()?
        .trim_matches(|c: char| c == '\0' || c.is_whitespace());
    let normalized = first.replace('\\', "/");
    if normalized.starts_with('/') {
        return None;
    }
    let mut parts = Vec::new();
    for part in normalized.split('/') {
        match part {
            "" | "." => continue,
            ".." => return None,
            p if p.contains(':') => return None,
            p => parts.push(p),
        }
    }
    if parts.is_empty() {
        return None;
    }
    let joined = parts.join("/");
    // Defence in depth: let std's path parser confirm there are only normal components.
    Path::new(&joined)
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
        .then_some(joined)
}

/// What a tar member is, relative to its GUID directory.
enum Member<'a> {
    Asset(&'a str),
    Meta(&'a str),
    PathName(&'a str),
    Other,
}

fn classify(name: &str) -> Member<'_> {
    let name = name.strip_prefix("./").unwrap_or(name);
    let Some((guid, file)) = name.split_once('/') else {
        return Member::Other;
    };
    if guid.is_empty() {
        return Member::Other;
    }
    match file {
        "asset" => Member::Asset(guid),
        "asset.meta" => Member::Meta(guid),
        "pathname" => Member::PathName(guid),
        _ => Member::Other,
    }
}

/// Reader wrapper that tallies compressed bytes into a shared counter, so progress can
/// be reported against the package's size on disk while `tar` owns the reader.
struct Counting<R> {
    inner: R,
    count: Rc<Cell<u64>>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count.set(self.count.get() + n as u64);
        Ok(n)
    }
}

/// An open package and how far into its compressed bytes the reader is.
struct Archive {
    tar: tar::Archive<GzDecoder<Counting<BufReader<File>>>>,
    read: Rc<Cell<u64>>,
    total: u64,
}

impl Archive {
    fn open(package: &Path) -> Result<Archive, Error> {
        let file = File::open(package).ctx(|| format!("failed to open {}", package.display()))?;
        let total = file.metadata().map(|m| m.len()).unwrap_or(0);
        let read = Rc::new(Cell::new(0));
        let reader = Counting {
            inner: BufReader::with_capacity(1 << 16, file),
            count: read.clone(),
        };
        Ok(Archive {
            tar: tar::Archive::new(GzDecoder::new(reader)),
            read,
            total,
        })
    }

    /// A closure reporting how much of the package has been read, from 0 to 1.
    fn fraction(&self) -> impl Fn() -> f64 + 'static {
        let (read, total) = (self.read.clone(), self.total.max(1));
        move || read.get() as f64 / total as f64
    }
}

/// Size of the chunks payloads are copied in. Cancellation and progress are checked
/// between chunks, so a multi-gigabyte asset doesn't make either unresponsive.
const CHUNK: usize = 1 << 16;

/// Copies `src` to `dst`, calling `tick` after every chunk. Returns `None` if `cancel`
/// was raised part-way.
fn copy(
    src: &mut dyn Read,
    dst: &mut dyn Write,
    cancel: &AtomicBool,
    tick: &mut dyn FnMut(),
) -> io::Result<Option<u64>> {
    let mut buf = vec![0u8; CHUNK];
    let mut written = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let n = match src.read(&mut buf) {
            Ok(0) => return Ok(Some(written)),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        dst.write_all(&buf[..n])?;
        written += n as u64;
        tick();
    }
}

/// Accumulates the package index while members stream past in any order.
#[derive(Default)]
struct Index {
    by_guid: HashMap<String, Partial>,
    rejected: Vec<String>,
}

#[derive(Default)]
struct Partial {
    path: Option<String>,
    size: u64,
    has_asset: bool,
    has_meta: bool,
}

impl Index {
    fn asset(&mut self, guid: &str, size: u64) {
        let p = self.by_guid.entry(guid.to_owned()).or_default();
        p.size = size;
        p.has_asset = true;
    }

    fn meta(&mut self, guid: &str) {
        self.by_guid.entry(guid.to_owned()).or_default().has_meta = true;
    }

    fn pathname(&mut self, guid: &str, raw: &str) {
        match sanitize_pathname(raw) {
            Some(path) => self.by_guid.entry(guid.to_owned()).or_default().path = Some(path),
            None => self
                .rejected
                .push(raw.lines().next().unwrap_or_default().to_owned()),
        }
    }

    /// Every GUID that got a valid `pathname`, sorted by path (so each folder precedes
    /// its contents), and the pathnames that were refused.
    fn finish(self) -> (Vec<Entry>, Vec<String>) {
        let mut entries: Vec<Entry> = self
            .by_guid
            .into_iter()
            .filter_map(|(guid, p)| {
                Some(Entry {
                    guid,
                    path: p.path?,
                    size: p.size,
                    has_asset: p.has_asset,
                    has_meta: p.has_meta,
                })
            })
            .collect();
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        (entries, self.rejected)
    }
}

/// Where payloads that are not kept in memory go while loading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
    /// Write them to the temp directory.
    Temp,
    /// Leave them in the package; they are read when (and if) they are needed.
    Skip,
}

/// Decides where [`load`] puts each payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadPolicy {
    /// Payloads larger than this are never kept in memory.
    pub memory_file_max: u64,
    /// Cap on the total bytes kept in memory. A payload that would exceed it is not
    /// kept in memory, whatever its size.
    pub memory_budget: u64,
    /// What happens to the payloads that are not kept in memory.
    pub overflow: Overflow,
    /// Percentage of physical memory to leave free. A payload bound for memory that
    /// would cut into it makes [`load`] ask its `low_memory` callback how to go on.
    pub memory_reserve_percent: u8,
    /// Only images and sounds are candidates for memory; everything else overflows.
    pub media_only: bool,
}

impl LoadPolicy {
    /// Read only the file list. Extraction reads the package a second time and writes
    /// each file straight to its final path.
    pub const TWO_PASS: Self = Self {
        memory_file_max: 0,
        memory_budget: 0,
        overflow: Overflow::Skip,
        memory_reserve_percent: Self::DEFAULT_RESERVE_PERCENT,
        media_only: false,
    };

    /// Hold the whole package in memory; nothing touches the disk until extraction.
    /// If the machine runs low on memory, the `low_memory` callback of [`load`]
    /// decides how to go on.
    pub const IN_MEMORY: Self = Self {
        memory_file_max: u64::MAX,
        memory_budget: u64::MAX,
        overflow: Overflow::Temp,
        memory_reserve_percent: Self::DEFAULT_RESERVE_PERCENT,
        media_only: false,
    };

    /// Keep payloads of up to `memory_file_max` bytes in memory, `memory_budget` bytes
    /// in total, and write the rest to the temp directory.
    pub const fn single_pass(memory_file_max: u64, memory_budget: u64) -> Self {
        Self {
            memory_file_max,
            memory_budget,
            overflow: Overflow::Temp,
            memory_reserve_percent: Self::DEFAULT_RESERVE_PERCENT,
            media_only: false,
        }
    }

    /// Two pass, except that images and sounds are kept in memory so they can be
    /// previewed without searching the package for them.
    pub const TWO_PASS_WITH_MEDIA: Self = Self {
        memory_file_max: u64::MAX,
        memory_budget: u64::MAX,
        overflow: Overflow::Skip,
        memory_reserve_percent: Self::DEFAULT_RESERVE_PERCENT,
        media_only: true,
    };

    pub const DEFAULT_RESERVE_PERCENT: u8 = 10;

    /// The same policy with a different share of physical memory left free.
    pub const fn with_reserve_percent(mut self, percent: u8) -> Self {
        self.memory_reserve_percent = percent;
        self
    }
}

/// Passed to the `low_memory` callback of [`load`] when the next payload doesn't fit
/// in memory comfortably.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LowMemory {
    /// Size of the payload that was about to be read into memory.
    pub needed: u64,
    /// Payload bytes already held in memory.
    pub in_memory: u64,
    /// Physical memory still available, or `None` if allocating `needed` bytes failed.
    pub available: Option<u64>,
}

/// How to continue loading when memory runs low.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnLowMemory {
    /// Move everything held in memory to the temp directory and put the rest of the
    /// package there too.
    UseTemp,
    /// Keep what is in memory, but load nothing more: the rest stays in the package
    /// and is read from it when it is needed.
    StopLoading,
    /// Give up; [`load`] returns [`Error::Cancelled`].
    Cancel,
}

/// What kind of previewable media a payload is, judged by its first bytes. (Its name
/// can't be used: in a package the name usually comes after the contents.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Media {
    Image,
    Audio,
}

impl Media {
    /// How many leading bytes [`Media::sniff`] looks at.
    pub const SNIFF_LEN: usize = 12;

    /// Recognises PNG, JPEG, GIF, BMP and TIFF images and WAV, AIFF, Ogg, FLAC and MP3
    /// audio. Formats without a signature (TGA) are not recognised.
    pub fn sniff(head: &[u8]) -> Option<Media> {
        let container =
            |form: &[u8], kind: &[u8]| head.starts_with(form) && head.get(8..12) == Some(kind);
        let image = head.starts_with(b"\x89PNG")
            || head.starts_with(b"\xFF\xD8\xFF")
            || head.starts_with(b"GIF8")
            || head.starts_with(b"BM")
            || head.starts_with(b"II*\0")
            || head.starts_with(b"MM\0*");
        let audio = container(b"RIFF", b"WAVE")
            || container(b"FORM", b"AIFF")
            || container(b"FORM", b"AIFC")
            || head.starts_with(b"OggS")
            || head.starts_with(b"fLaC")
            || head.starts_with(b"ID3")
            || matches!(head, [0xFF, second, ..] if second & 0xE6 == 0xE2);
        match (image, audio) {
            (true, _) => Some(Media::Image),
            (false, true) => Some(Media::Audio),
            (false, false) => None,
        }
    }
}

/// What [`load`] reports and asks while it runs. Every method has a default, so an
/// implementation only spells out what it cares about.
pub trait LoadEvents {
    /// How much of the package has been read, from 0 to 1.
    fn progress(&mut self, _fraction: f64) {}

    /// A payload bound for memory would leave the machine short of it (or could not
    /// be allocated at all). Asked at most once per load.
    fn low_memory(&mut self, _low: LowMemory) -> OnLowMemory {
        OnLowMemory::UseTemp
    }

    /// An image or sound has been read into memory. Return `false` to not keep it
    /// there (it then overflows like any payload that doesn't fit), for instance
    /// after making a smaller preview of it.
    fn keep_media(&mut self, _guid: &str, _media: Media, _data: &[u8]) -> bool {
        true
    }
}

/// For callers with nothing to report to.
impl LoadEvents for () {}

/// Available memory is re-checked after this many bytes have been read into memory.
const MEMORY_CHECK_INTERVAL: u64 = 16 << 20;

enum Payload {
    Memory(Arc<Vec<u8>>),
    Temp(PathBuf),
}

#[derive(Default)]
struct Payloads {
    asset: Option<Payload>,
    meta: Option<Payload>,
}

/// The temp directory payloads are written to, created on first use.
struct TempStore {
    parent: PathBuf,
    dir: Option<TempDir>,
    files: u64,
    bytes: u64,
}

impl TempStore {
    fn new(parent: &Path) -> TempStore {
        TempStore {
            parent: parent.to_owned(),
            dir: None,
            files: 0,
            bytes: 0,
        }
    }

    /// Creates the next (empty) temp file.
    fn create(&mut self) -> Result<(PathBuf, File), Error> {
        let dir = match &self.dir {
            Some(dir) => dir,
            None => {
                let mut builder = tempfile::Builder::new();
                builder.prefix(".unity-unpacker-");
                // Prefer `parent` so the later move is a rename on the same filesystem;
                // fall back to the system temp directory if it can't be written to.
                let created = fs::create_dir_all(&self.parent)
                    .and_then(|()| builder.tempdir_in(&self.parent))
                    .or_else(|_| builder.tempdir())
                    .ctx(|| {
                        format!(
                            "failed to create temp directory in {}",
                            self.parent.display()
                        )
                    })?;
                self.dir.insert(created)
            }
        };
        // Numbered rather than named after the GUID, which is untrusted input.
        let path = dir.path().join(self.files.to_string());
        self.files += 1;
        let file = File::create(&path).ctx(|| format!("failed to create {}", path.display()))?;
        Ok((path, file))
    }
}

/// A package whose file list has been read, along with however much of its contents
/// the [`LoadPolicy`] asked for.
///
/// Dropping it removes the temp directory.
pub struct Loaded {
    package: PathBuf,
    /// Entries sorted by path.
    pub entries: Vec<Entry>,
    /// Raw pathnames that were refused because they would escape the output folder.
    pub rejected: Vec<String>,
    payloads: HashMap<String, Payloads>,
    temp: TempStore,
    memory_bytes: u64,
}

impl fmt::Debug for Loaded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Loaded")
            .field("package", &self.package)
            .field("entries", &self.entries.len())
            .field("memory_bytes", &self.memory_bytes)
            .field("temp_bytes", &self.temp.bytes)
            .field("temp_dir", &self.temp_dir())
            .finish()
    }
}

/// Reads the package once: the file list always, the payloads as `policy` says.
///
/// Payloads destined for disk are written to a `.unity-unpacker-*` directory inside
/// `temp_parent`, created on first use. Pick a `temp_parent` on the same filesystem as
/// the eventual output folder so [`Loaded::extract`] can rename them into place instead
/// of copying.
pub fn load(
    package: &Path,
    temp_parent: &Path,
    policy: LoadPolicy,
    events: &mut dyn LoadEvents,
    cancel: &AtomicBool,
) -> Result<Loaded, Error> {
    load_with(
        package,
        temp_parent,
        policy,
        events,
        cancel,
        &memory::status,
    )
}

/// Fills `head` from `src` as far as it goes and returns how many bytes that was.
fn read_head(src: &mut dyn Read, head: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < head.len() {
        match src.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

fn load_with(
    package: &Path,
    temp_parent: &Path,
    mut policy: LoadPolicy,
    events: &mut dyn LoadEvents,
    cancel: &AtomicBool,
    memory_status: &dyn Fn() -> Option<memory::Status>,
) -> Result<Loaded, Error> {
    let mut archive = Archive::open(package)?;
    let fraction = archive.fraction();
    let ctx = || format!("failed to read {}", package.display());

    let mut index = Index::default();
    let mut payloads: HashMap<String, Payloads> = HashMap::new();
    let mut temp = TempStore::new(temp_parent);
    let mut memory_bytes = 0u64;
    let mut unchecked_bytes = MEMORY_CHECK_INTERVAL; // check before the first payload

    for member in archive.tar.entries().ctx(ctx)? {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let mut member = member.ctx(ctx)?;
        let name = String::from_utf8_lossy(&member.path_bytes()).into_owned();
        let (guid, is_meta) = match classify(&name) {
            Member::Asset(g) => (g, false),
            Member::Meta(g) => (g, true),
            Member::PathName(guid) => {
                let mut raw = String::new();
                member.read_to_string(&mut raw).ctx(ctx)?;
                index.pathname(guid, &raw);
                events.progress(fraction());
                continue;
            }
            Member::Other => continue,
        };

        let size = member.size();
        // Look at the first bytes to tell images and sounds apart from the rest, then
        // read on from where that left off.
        let mut head = [0u8; Media::SNIFF_LEN];
        let head_len = read_head(&mut member, &mut head).ctx(ctx)?;
        let media = Media::sniff(&head[..head_len]).filter(|_| !is_meta);
        let mut reader = head[..head_len].chain(&mut member);

        let mut data = Vec::new();
        let mut in_memory = size <= policy.memory_file_max
            && (media.is_some() || !policy.media_only)
            && memory_bytes
                .checked_add(size)
                .is_some_and(|sum| sum <= policy.memory_budget);
        if in_memory {
            // Asking the OS is cheap, but not free: skip it for runs of small files.
            unchecked_bytes = unchecked_bytes.saturating_add(size);
            let status = if unchecked_bytes >= MEMORY_CHECK_INTERVAL {
                unchecked_bytes = 0;
                memory_status()
            } else {
                None
            };
            let reserve = |total: u64| total / 100 * u64::from(policy.memory_reserve_percent);
            let short = status.filter(|s| s.available < size.saturating_add(reserve(s.total)));
            // The size comes from an untrusted header, so a failed allocation must not
            // abort; it is treated like running low.
            let reserved = short.is_none()
                && usize::try_from(size).is_ok_and(|n| data.try_reserve_exact(n).is_ok());
            if !reserved {
                let decision = events.low_memory(LowMemory {
                    needed: size,
                    in_memory: memory_bytes,
                    available: short.map(|s| s.available),
                });
                match decision {
                    OnLowMemory::UseTemp => {
                        move_to_temp(&mut payloads, &mut temp, cancel)?;
                        memory_bytes = 0;
                        policy.overflow = Overflow::Temp;
                    }
                    OnLowMemory::StopLoading => policy.overflow = Overflow::Skip,
                    OnLowMemory::Cancel => return Err(Error::Cancelled),
                }
                // Either way nothing more goes to memory, so this is not asked again.
                policy.memory_budget = 0;
                in_memory = false;
            }
        }

        let mut tick = || events.progress(fraction());
        let (payload, size) = if in_memory {
            let n = copy(&mut reader, &mut data, cancel, &mut tick)
                .ctx(ctx)?
                .ok_or(Error::Cancelled)?;
            let keep = media.is_none_or(|media| events.keep_media(guid, media, &data));
            if keep {
                memory_bytes += n;
                (Some(Payload::Memory(Arc::new(data))), n)
            } else if policy.overflow == Overflow::Temp {
                let (path, mut file) = temp.create()?;
                file.write_all(&data)
                    .ctx(|| format!("failed to write {}", path.display()))?;
                temp.bytes += n;
                (Some(Payload::Temp(path)), n)
            } else {
                (None, n)
            }
        } else if policy.overflow == Overflow::Temp {
            let (path, mut file) = temp.create()?;
            let n = copy(&mut reader, &mut file, cancel, &mut tick)
                .ctx(|| format!("failed to write {}", path.display()))?
                .ok_or(Error::Cancelled)?;
            temp.bytes += n;
            (Some(Payload::Temp(path)), n)
        } else {
            (None, size)
        };

        let slot = payloads.entry(guid.to_owned()).or_default();
        if is_meta {
            index.meta(guid);
            slot.meta = payload;
        } else {
            index.asset(guid, size);
            slot.asset = payload;
        }
        events.progress(fraction());
    }

    let (entries, rejected) = index.finish();
    // Payloads whose pathname was missing or unsafe can never be used.
    let known: HashSet<&str> = entries.iter().map(|e| e.guid.as_str()).collect();
    payloads.retain(|guid, _| known.contains(guid.as_str()));
    Ok(Loaded {
        package: package.to_owned(),
        entries,
        rejected,
        payloads,
        temp,
        memory_bytes,
    })
}

/// Writes every payload held in memory to the temp directory, freeing the memory.
fn move_to_temp(
    payloads: &mut HashMap<String, Payloads>,
    temp: &mut TempStore,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    for slot in payloads
        .values_mut()
        .flat_map(|p| [&mut p.asset, &mut p.meta])
    {
        let Some(Payload::Memory(data)) = slot else {
            continue;
        };
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let (path, mut file) = temp.create()?;
        file.write_all(data)
            .ctx(|| format!("failed to write {}", path.display()))?;
        temp.bytes += data.len() as u64;
        *slot = Some(Payload::Temp(path));
    }
    Ok(())
}

/// Where the contents of one asset can be read from, for previews. Cheap to make and
/// safe to send to another thread.
#[derive(Debug, Clone)]
pub enum Source {
    Memory(Arc<Vec<u8>>),
    Temp(PathBuf),
    /// Not loaded: reading it means decompressing the package up to this asset.
    Package {
        package: PathBuf,
        guid: String,
    },
}

impl Source {
    /// True if reading has to search through the package, which can take a while.
    pub fn needs_scan(&self) -> bool {
        matches!(self, Source::Package { .. })
    }

    /// Reads up to the first `limit` bytes of the asset.
    pub fn read(&self, limit: u64, cancel: &AtomicBool) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        match self {
            Source::Memory(data) => {
                let n = usize::try_from(limit).map_or(data.len(), |l| l.min(data.len()));
                out.extend_from_slice(&data[..n]);
            }
            Source::Temp(path) => {
                let file = File::open(path).ctx(|| format!("failed to open {}", path.display()))?;
                copy(&mut file.take(limit), &mut out, cancel, &mut || {})
                    .ctx(|| format!("failed to read {}", path.display()))?
                    .ok_or(Error::Cancelled)?;
            }
            Source::Package { package, guid } => {
                let mut archive = Archive::open(package)?;
                let ctx = || format!("failed to read {}", package.display());
                let mut found = false;
                for member in archive.tar.entries().ctx(ctx)? {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(Error::Cancelled);
                    }
                    let member = member.ctx(ctx)?;
                    let name = String::from_utf8_lossy(&member.path_bytes()).into_owned();
                    if matches!(classify(&name), Member::Asset(g) if g == guid) {
                        copy(&mut member.take(limit), &mut out, cancel, &mut || {})
                            .ctx(ctx)?
                            .ok_or(Error::Cancelled)?;
                        found = true;
                        break;
                    }
                }
                if !found {
                    return Err(io::Error::from(io::ErrorKind::NotFound))
                        .ctx(|| format!("asset {guid} is no longer in {}", package.display()));
                }
            }
        }
        Ok(out)
    }
}

/// One selected entry on its way to the output folder.
struct Job<'a> {
    entry: &'a Entry,
    need_asset: bool,
    need_meta: bool,
}

impl Loaded {
    /// The package this was loaded from.
    pub fn package(&self) -> &Path {
        &self.package
    }

    /// Payload bytes currently held in memory.
    pub fn memory_bytes(&self) -> u64 {
        self.memory_bytes
    }

    /// Payload bytes currently held in the temp directory.
    pub fn temp_bytes(&self) -> u64 {
        self.temp.bytes
    }

    /// The temp directory, if any payload had to be written to disk.
    pub fn temp_dir(&self) -> Option<&Path> {
        self.temp.dir.as_ref().map(TempDir::path)
    }

    /// Where the contents of the asset with this GUID can be read from right now.
    pub fn source(&self, guid: &str) -> Source {
        match self.payloads.get(guid).and_then(|p| p.asset.as_ref()) {
            Some(Payload::Memory(data)) => Source::Memory(data.clone()),
            Some(Payload::Temp(path)) => Source::Temp(path.clone()),
            None => Source::Package {
                package: self.package.clone(),
                guid: guid.to_owned(),
            },
        }
    }

    /// Writes the entries whose GUID is in `selected` (or everything if `None`) into
    /// `out_dir`, replacing existing files.
    ///
    /// Loaded payloads are written or moved into place; the rest are read from the
    /// package in one more pass. Afterwards nothing is loaded any more: the payloads
    /// that were not selected are discarded and the temp directory is removed, on
    /// success, error and cancellation alike. Files already put in place before an
    /// error or cancellation are kept. The file list stays, so the same `Loaded` can be
    /// extracted again; that then reads everything from the package.
    pub fn extract(
        &mut self,
        selected: Option<&HashSet<String>>,
        out_dir: &Path,
        progress: &mut dyn FnMut(Progress),
        cancel: &AtomicBool,
    ) -> Result<Stats, Error> {
        // Taking these out is what discards them when this returns, however it does.
        let mut payloads = std::mem::take(&mut self.payloads);
        let fresh = TempStore::new(&self.temp.parent);
        let temp = std::mem::replace(&mut self.temp, fresh);
        self.memory_bytes = 0;

        fs::create_dir_all(out_dir).ctx(|| format!("failed to create {}", out_dir.display()))?;
        let wanted: Vec<(&Entry, Payloads)> = self
            .entries
            .iter()
            .filter(|e| selected.is_none_or(|s| s.contains(&e.guid)))
            .map(|e| (e, payloads.remove(&e.guid).unwrap_or_default()))
            .collect();
        drop(payloads);

        // Progress is weighted by payload bytes across both steps.
        let total: u64 = wanted.iter().map(|(e, _)| e.size).sum::<u64>().max(1);
        let unloaded: u64 = (wanted.iter())
            .filter(|(e, p)| e.has_asset && p.asset.is_none())
            .map(|(e, _)| e.size)
            .sum();
        let mut placed = 0u64;

        // Step 1: folders, and everything that is already loaded.
        let mut stats = Stats::default();
        let mut pending: HashMap<&str, Job> = HashMap::new();
        for (entry, payloads) in wanted {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let dest = out_dir.join(&entry.path);
            let mut job = Job {
                entry,
                need_asset: false,
                need_meta: false,
            };
            match payloads.asset {
                Some(asset) => {
                    progress(Progress::Writing(&entry.path));
                    place(asset, &dest)?;
                    stats.files += 1;
                    stats.bytes += entry.size;
                    placed += entry.size;
                    progress(Progress::Fraction(placed as f64 / total as f64));
                }
                None if entry.has_asset => job.need_asset = true,
                None => {
                    fs::create_dir_all(&dest)
                        .ctx(|| format!("failed to create {}", dest.display()))?;
                    stats.folders += 1;
                }
            }
            match payloads.meta {
                Some(meta) => {
                    let mut meta_dest = dest;
                    meta_dest.as_mut_os_string().push(".meta");
                    place(meta, &meta_dest)?;
                    stats.metas += 1;
                }
                None => job.need_meta = entry.has_meta,
            }
            if job.need_asset || job.need_meta {
                pending.insert(&entry.guid, job);
            }
        }

        // Step 2: stream whatever was not loaded straight out of the package.
        if !pending.is_empty() {
            let mut archive = Archive::open(&self.package)?;
            let fraction = archive.fraction();
            let tick = |progress: &mut dyn FnMut(Progress)| {
                let done = placed as f64 + fraction() * unloaded as f64;
                progress(Progress::Fraction(done / total as f64))
            };
            let ctx = || format!("failed to read {}", self.package.display());
            for member in archive.tar.entries().ctx(ctx)? {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                let mut member = member.ctx(ctx)?;
                let name = String::from_utf8_lossy(&member.path_bytes()).into_owned();
                let (guid, is_meta) = match classify(&name) {
                    Member::Asset(g) => (g, false),
                    Member::Meta(g) => (g, true),
                    _ => continue,
                };
                let Some(job) = pending.get(guid) else {
                    continue;
                };
                let needed = if is_meta {
                    job.need_meta
                } else {
                    job.need_asset
                };
                if !needed {
                    continue;
                }

                let mut dest = out_dir.join(&job.entry.path);
                if is_meta {
                    dest.as_mut_os_string().push(".meta");
                } else {
                    progress(Progress::Writing(&job.entry.path));
                }
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)
                        .ctx(|| format!("failed to create {}", parent.display()))?;
                }
                let mut out =
                    File::create(&dest).ctx(|| format!("failed to create {}", dest.display()))?;
                let n = copy(&mut member, &mut out, cancel, &mut || tick(progress))
                    .ctx(|| format!("failed to write {}", dest.display()))?
                    .ok_or(Error::Cancelled)?;
                if is_meta {
                    stats.metas += 1;
                } else {
                    stats.files += 1;
                    stats.bytes += n;
                }
            }
        }

        if let Some(dir) = temp.dir {
            dir.close()
                .ctx(|| "failed to remove temp directory".to_owned())?;
        }
        Ok(stats)
    }
}

/// Puts a loaded payload at `dest`, replacing an existing file.
fn place(payload: Payload, dest: &Path) -> Result<(), Error> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).ctx(|| format!("failed to create {}", parent.display()))?;
    }
    match payload {
        Payload::Memory(data) => {
            fs::write(dest, &*data).ctx(|| format!("failed to write {}", dest.display()))
        }
        Payload::Temp(src) => move_file(&src, dest),
    }
}

/// Renames `src` to `dest`, replacing an existing file. Falls back to copy + delete if
/// rename fails (e.g. the temp directory is on another filesystem).
fn move_file(src: &Path, dest: &Path) -> Result<(), Error> {
    if fs::rename(src, dest).is_ok() {
        return Ok(());
    }
    fs::copy(src, dest)
        .ctx(|| format!("failed to move {} to {}", src.display(), dest.display()))?;
    let _ = fs::remove_file(src);
    Ok(())
}

/// Human-readable byte count.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};

    fn add(builder: &mut tar::Builder<GzEncoder<File>>, name: &str, data: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name, data).unwrap();
    }

    fn make_package(dir: &Path) -> PathBuf {
        let path = dir.join("test.unitypackage");
        let mut b = tar::Builder::new(GzEncoder::new(
            File::create(&path).unwrap(),
            Compression::fast(),
        ));
        // Tar order mimics Unity exports: asset, asset.meta, pathname.
        add(&mut b, "aaaa/asset.meta", b"folder meta"); // folder: meta only
        add(&mut b, "aaaa/pathname", b"Assets/Textures");
        add(&mut b, "bbbb/asset", b"PNGDATA");
        add(&mut b, "bbbb/asset.meta", b"png meta");
        add(&mut b, "bbbb/pathname", b"Assets/Textures/a.png\n00");
        add(&mut b, "bbbb/preview.png", b"thumb");
        add(&mut b, "./cccc/asset", b"using UnityEngine;");
        add(&mut b, "./cccc/pathname", b"Assets\\Scripts\\B.cs");
        add(&mut b, "dddd/asset", b"evil");
        add(&mut b, "dddd/pathname", b"../../evil.txt");
        b.into_inner().unwrap().finish().unwrap();
        path
    }

    /// Nothing loaded, everything on disk, everything in memory, and a mix (the 18-byte
    /// script is spilled by the per-file limit, the rest stays in memory).
    const POLICIES: [LoadPolicy; 4] = [
        LoadPolicy::TWO_PASS,
        LoadPolicy::single_pass(0, 0),
        LoadPolicy::IN_MEMORY,
        LoadPolicy::single_pass(11, 1 << 20),
    ];

    const EXPECTED: Stats = Stats {
        files: 2,
        folders: 1,
        metas: 2,
        bytes: 7 + 18,
    };

    fn never() -> AtomicBool {
        AtomicBool::new(false)
    }

    /// Records the low-memory prompts and answers them; panics if none was expected.
    #[derive(Default)]
    struct Events {
        answer: Option<OnLowMemory>,
        asked: Vec<LowMemory>,
        /// Media offered to `keep_media`, and whether to keep it.
        media: Vec<(String, Media)>,
        discard_media: bool,
    }

    impl LoadEvents for Events {
        fn low_memory(&mut self, low: LowMemory) -> OnLowMemory {
            self.asked.push(low);
            self.answer
                .unwrap_or_else(|| panic!("unexpected low-memory prompt: {low:?}"))
        }

        fn keep_media(&mut self, guid: &str, media: Media, _data: &[u8]) -> bool {
            self.media.push((guid.to_owned(), media));
            !self.discard_media
        }
    }

    fn plenty() -> Option<memory::Status> {
        Some(memory::Status {
            total: 1 << 40,
            available: 1 << 40,
        })
    }

    /// Loads on a pretend machine with plenty of memory, whatever the real one has.
    fn load_plain(pkg: &Path, temp_parent: &Path, policy: LoadPolicy) -> Loaded {
        let mut events = Events::default();
        load_with(pkg, temp_parent, policy, &mut events, &never(), &plenty).unwrap()
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn sanitize() {
        assert_eq!(
            sanitize_pathname("Assets/a.png\n00").as_deref(),
            Some("Assets/a.png")
        );
        assert_eq!(
            sanitize_pathname("Assets\\x\\y.cs").as_deref(),
            Some("Assets/x/y.cs")
        );
        assert_eq!(
            sanitize_pathname("./Assets//a").as_deref(),
            Some("Assets/a")
        );
        assert_eq!(sanitize_pathname("../a"), None);
        assert_eq!(sanitize_pathname("Assets/../../a"), None);
        assert_eq!(sanitize_pathname("/etc/passwd"), None);
        assert_eq!(sanitize_pathname("C:/Windows/x"), None);
        assert_eq!(sanitize_pathname(""), None);
    }

    #[test]
    fn every_policy_extracts_the_same_files() {
        for policy in POLICIES {
            let tmp = tempfile::tempdir().unwrap();
            let pkg = make_package(tmp.path());
            let out = tmp.path().join("out");
            // Pre-existing file must be replaced, not cause an error.
            fs::create_dir_all(out.join("Assets/Textures")).unwrap();
            fs::write(out.join("Assets/Textures/a.png"), b"old").unwrap();

            let mut loaded = load_plain(&pkg, &out, policy);
            let paths: Vec<_> = loaded.entries.iter().map(|e| e.path.as_str()).collect();
            assert_eq!(
                paths,
                [
                    "Assets/Scripts/B.cs",
                    "Assets/Textures",
                    "Assets/Textures/a.png"
                ]
            );
            assert!(loaded.entries[1].is_folder());
            assert_eq!(loaded.entries[2].size, 7);
            assert_eq!(loaded.rejected, ["../../evil.txt"]);

            let stats = loaded.extract(None, &out, &mut |_| {}, &never()).unwrap();
            assert_eq!(stats, EXPECTED, "{policy:?}");
            assert_eq!(
                fs::read(out.join("Assets/Textures/a.png")).unwrap(),
                b"PNGDATA"
            );
            assert_eq!(
                fs::read(out.join("Assets/Textures/a.png.meta")).unwrap(),
                b"png meta"
            );
            assert_eq!(
                fs::read(out.join("Assets/Textures.meta")).unwrap(),
                b"folder meta"
            );
            assert_eq!(
                fs::read(out.join("Assets/Scripts/B.cs")).unwrap(),
                b"using UnityEngine;"
            );
            assert!(!tmp.path().join("evil.txt").exists());
            // Only "Assets" remains: the temp directory is gone.
            assert_eq!(listing(&out), ["Assets"]);

            // Nothing is loaded any more, and extracting again still works.
            assert_eq!((loaded.memory_bytes(), loaded.temp_bytes()), (0, 0));
            fs::remove_dir_all(&out).unwrap();
            let again = loaded.extract(None, &out, &mut |_| {}, &never()).unwrap();
            assert_eq!(again, EXPECTED, "{policy:?}");
            assert_eq!(listing(&out), ["Assets"]);
        }
    }

    #[test]
    fn policy_decides_placement() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = make_package(tmp.path());
        // Payloads in tar order: 11 (folder meta), 7 (png), 8 (png meta), 18 (script),
        // 4 ("evil", which is loaded before its pathname is seen to be unsafe).
        let load = |policy| load_plain(&pkg, tmp.path(), policy);

        let nothing = load(LoadPolicy::TWO_PASS);
        assert_eq!((nothing.memory_bytes(), nothing.temp_bytes()), (0, 0));
        assert_eq!(nothing.temp_dir(), None);

        let all_temp = load(LoadPolicy::single_pass(0, 0));
        assert_eq!((all_temp.memory_bytes(), all_temp.temp_bytes()), (0, 48));
        let dir = all_temp.temp_dir().unwrap().to_owned();
        assert!(dir.starts_with(tmp.path()));
        drop(all_temp);
        assert!(!dir.exists());

        let all_memory = load(LoadPolicy::IN_MEMORY);
        assert_eq!(
            (all_memory.memory_bytes(), all_memory.temp_bytes()),
            (48, 0)
        );
        assert_eq!(all_memory.temp_dir(), None);

        // Per-file limit: only the 18-byte script is too big.
        let mixed = load(LoadPolicy::single_pass(11, 1 << 20));
        assert_eq!((mixed.memory_bytes(), mixed.temp_bytes()), (30, 18));

        // Budget: 11 + 7 fit; after that even the 4-byte payload would pass 20.
        let capped = load(LoadPolicy::single_pass(100, 20));
        assert_eq!((capped.memory_bytes(), capped.temp_bytes()), (18, 30));
    }

    #[test]
    fn selection_discards_the_rest() {
        for policy in POLICIES {
            let tmp = tempfile::tempdir().unwrap();
            let pkg = make_package(tmp.path());
            let out = tmp.path().join("out");

            let mut loaded = load_plain(&pkg, tmp.path(), policy);
            let only: HashSet<String> = ["cccc".to_owned()].into();
            let stats = loaded
                .extract(Some(&only), &out, &mut |_| {}, &never())
                .unwrap();
            assert_eq!(
                stats,
                Stats {
                    files: 1,
                    folders: 0,
                    metas: 0,
                    bytes: 18
                },
                "{policy:?}"
            );
            assert!(out.join("Assets/Scripts/B.cs").exists());
            assert!(!out.join("Assets/Textures").exists());
            // The unselected payloads and the temp directory are gone.
            assert_eq!(listing(tmp.path()), ["out", "test.unitypackage"]);
        }
    }

    /// Loads in-memory on a pretend machine whose available memory is whatever
    /// `available(check_number)` says, out of `total`, answering the prompt with
    /// `answer`. Returns the result and the prompts that were shown.
    fn load_short(
        pkg: &Path,
        temp_parent: &Path,
        total: u64,
        available: impl Fn(u32) -> u64,
        answer: OnLowMemory,
    ) -> (Result<Loaded, Error>, Vec<LowMemory>) {
        let checks = Cell::new(0u32);
        let mut events = Events {
            answer: Some(answer),
            ..Events::default()
        };
        let result = load_with(
            pkg,
            temp_parent,
            LoadPolicy::IN_MEMORY,
            &mut events,
            &never(),
            &|| {
                checks.set(checks.get() + 1);
                Some(memory::Status {
                    total,
                    available: available(checks.get()),
                })
            },
        );
        (result, events.asked)
    }

    #[test]
    fn low_memory_at_the_start() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = make_package(tmp.path());
        let out = tmp.path().join("out");
        // 100 bytes of RAM, 12 available: the first payload needs 11 plus the 10-byte
        // (10%) reserve.
        let run = |answer| load_short(&pkg, tmp.path(), 100, |_| 12, answer);

        // With no reserve the same machine has room for it, and nobody is asked.
        let roomy = LoadPolicy::IN_MEMORY.with_reserve_percent(0);
        let status = || {
            Some(memory::Status {
                total: 100,
                available: 12,
            })
        };
        let mut events = Events::default();
        let loaded = load_with(&pkg, tmp.path(), roomy, &mut events, &never(), &status);
        assert_eq!(loaded.unwrap().memory_bytes(), 48);

        let (loaded, asked) = run(OnLowMemory::StopLoading);
        let mut loaded = loaded.unwrap();
        assert_eq!(
            asked,
            [LowMemory {
                needed: 11,
                in_memory: 0,
                available: Some(12)
            }]
        );
        assert_eq!((loaded.memory_bytes(), loaded.temp_bytes()), (0, 0));
        let stats = loaded.extract(None, &out, &mut |_| {}, &never()).unwrap();
        assert_eq!(stats, EXPECTED);
        fs::remove_dir_all(&out).unwrap();

        let (loaded, asked) = run(OnLowMemory::UseTemp);
        let mut loaded = loaded.unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!((loaded.memory_bytes(), loaded.temp_bytes()), (0, 48));
        let stats = loaded.extract(None, &out, &mut |_| {}, &never()).unwrap();
        assert_eq!(stats, EXPECTED);
        assert_eq!(listing(tmp.path()), ["out", "test.unitypackage"]);

        let (loaded, asked) = run(OnLowMemory::Cancel);
        assert!(matches!(loaded, Err(Error::Cancelled)));
        assert_eq!(asked.len(), 1);
        assert_eq!(listing(tmp.path()), ["out", "test.unitypackage"]);
    }

    #[test]
    fn low_memory_part_way_keeps_or_moves_what_is_loaded() {
        let tmp = tempfile::tempdir().unwrap();
        // Memory is only re-checked every MEMORY_CHECK_INTERVAL bytes, so use payloads
        // of exactly that size: one check per asset.
        let size = MEMORY_CHECK_INTERVAL;
        let pkg = tmp.path().join("big.unitypackage");
        let mut b = tar::Builder::new(GzEncoder::new(
            File::create(&pkg).unwrap(),
            Compression::fast(),
        ));
        let chunk = vec![7u8; size as usize];
        for guid in ["aaaa", "bbbb", "cccc"] {
            add(&mut b, &format!("{guid}/asset"), &chunk);
            add(&mut b, &format!("{guid}/asset.meta"), b"meta");
            add(&mut b, &format!("{guid}/pathname"), guid.as_bytes());
        }
        b.into_inner().unwrap().finish().unwrap();

        // Room for the first asset plus the reserve, but not for the second.
        let run = |answer| {
            let available = |check| if check == 1 { 3 * size } else { size };
            let (loaded, asked) = load_short(&pkg, tmp.path(), 10 * size, available, answer);
            assert_eq!(
                asked,
                [LowMemory {
                    needed: size,
                    in_memory: size + 4,
                    available: Some(size)
                }]
            );
            loaded.unwrap()
        };
        let expected = Stats {
            files: 3,
            folders: 0,
            metas: 3,
            bytes: 3 * size,
        };

        let mut kept = run(OnLowMemory::StopLoading);
        assert_eq!((kept.memory_bytes(), kept.temp_bytes()), (size + 4, 0));
        assert!(matches!(kept.source("aaaa"), Source::Memory(_)));
        assert!(kept.source("bbbb").needs_scan());
        let out = tmp.path().join("kept");
        let stats = kept.extract(None, &out, &mut |_| {}, &never()).unwrap();
        assert_eq!(stats, expected);
        assert_eq!(fs::read(out.join("cccc")).unwrap(), chunk);
        assert_eq!(fs::read(out.join("cccc.meta")).unwrap(), b"meta");

        let mut moved = run(OnLowMemory::UseTemp);
        assert_eq!(
            (moved.memory_bytes(), moved.temp_bytes()),
            (0, 3 * (size + 4))
        );
        assert!(matches!(moved.source("aaaa"), Source::Temp(_)));
        assert!(matches!(moved.source("cccc"), Source::Temp(_)));
        let out = tmp.path().join("moved");
        let stats = moved.extract(None, &out, &mut |_| {}, &never()).unwrap();
        assert_eq!(stats, expected);
        assert_eq!(fs::read(out.join("aaaa")).unwrap(), chunk);
        assert_eq!(listing(tmp.path()), ["big.unitypackage", "kept", "moved"]);
    }

    #[test]
    fn sniffing() {
        assert_eq!(Media::sniff(b"\x89PNG\r\n\x1a\n...."), Some(Media::Image));
        assert_eq!(
            Media::sniff(b"\xFF\xD8\xFF\xE0\0\x10JFIF"),
            Some(Media::Image)
        );
        assert_eq!(Media::sniff(b"RIFF\x24\0\0\0WAVEfmt "), Some(Media::Audio));
        assert_eq!(Media::sniff(b"RIFF\x24\0\0\0AVI LIST"), None);
        assert_eq!(Media::sniff(b"OggS\0\x02"), Some(Media::Audio));
        assert_eq!(Media::sniff(b"\xFF\xFB\x90\x64"), Some(Media::Audio));
        assert_eq!(Media::sniff(b"using UnityEngine;"), None);
        assert_eq!(Media::sniff(b""), None);
    }

    #[test]
    fn two_pass_with_media_keeps_only_images_and_sounds() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = tmp.path().join("media.unitypackage");
        let mut b = tar::Builder::new(GzEncoder::new(
            File::create(&pkg).unwrap(),
            Compression::fast(),
        ));
        let png = b"\x89PNG\r\n\x1a\n-not really a picture-";
        let wav = b"RIFF\x24\0\0\0WAVEfmt -not really a sound-";
        for (guid, data, path) in [
            ("aaaa", &png[..], "Assets/a.png"),
            ("bbbb", &wav[..], "Assets/b.wav"),
            ("cccc", b"using UnityEngine;", "Assets/c.cs"),
        ] {
            add(&mut b, &format!("{guid}/asset"), data);
            // A meta file is never media, whatever it starts with.
            add(&mut b, &format!("{guid}/asset.meta"), png);
            add(&mut b, &format!("{guid}/pathname"), path.as_bytes());
        }
        b.into_inner().unwrap().finish().unwrap();
        let media_bytes = (png.len() + wav.len()) as u64;
        let expected = Stats {
            files: 3,
            folders: 0,
            metas: 3,
            bytes: media_bytes + 18,
        };

        let mut events = Events::default();
        let policy = LoadPolicy::TWO_PASS_WITH_MEDIA;
        let mut loaded =
            load_with(&pkg, tmp.path(), policy, &mut events, &never(), &plenty).unwrap();
        assert_eq!(
            events.media,
            [
                ("aaaa".to_owned(), Media::Image),
                ("bbbb".to_owned(), Media::Audio)
            ]
        );
        assert_eq!(
            (loaded.memory_bytes(), loaded.temp_bytes()),
            (media_bytes, 0)
        );
        assert!(matches!(loaded.source("aaaa"), Source::Memory(_)));
        assert!(loaded.source("cccc").needs_scan());
        assert_eq!(loaded.source("bbbb").read(u64::MAX, &never()).unwrap(), wav);
        // The loaded halves are written from memory, the rest from the package.
        let out = tmp.path().join("out");
        let stats = loaded.extract(None, &out, &mut |_| {}, &never()).unwrap();
        assert_eq!(stats, expected);
        assert_eq!(fs::read(out.join("Assets/a.png")).unwrap(), png);
        assert_eq!(
            fs::read(out.join("Assets/c.cs")).unwrap(),
            b"using UnityEngine;"
        );

        // Declining the media leaves it in the package like everything else.
        let mut events = Events {
            discard_media: true,
            ..Events::default()
        };
        let mut loaded =
            load_with(&pkg, tmp.path(), policy, &mut events, &never(), &plenty).unwrap();
        assert_eq!(events.media.len(), 2);
        assert_eq!((loaded.memory_bytes(), loaded.temp_bytes()), (0, 0));
        assert!(loaded.source("aaaa").needs_scan());
        fs::remove_dir_all(&out).unwrap();
        let stats = loaded.extract(None, &out, &mut |_| {}, &never()).unwrap();
        assert_eq!(stats, expected);
        assert_eq!(fs::read(out.join("Assets/b.wav")).unwrap(), wav);
    }

    #[test]
    fn sources_read_the_same_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = make_package(tmp.path());
        for policy in POLICIES {
            let loaded = load_plain(&pkg, tmp.path(), policy);
            let source = loaded.source("cccc");
            assert_eq!(source.needs_scan(), policy == LoadPolicy::TWO_PASS);
            assert_eq!(
                source.read(u64::MAX, &never()).unwrap(),
                b"using UnityEngine;"
            );
            assert_eq!(source.read(5, &never()).unwrap(), b"using");
        }
        let gone = Source::Package {
            package: pkg,
            guid: "zzzz".to_owned(),
        };
        assert!(matches!(gone.read(10, &never()), Err(Error::Io { .. })));
    }

    #[test]
    fn cancel_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = make_package(tmp.path());
        let cancelled = AtomicBool::new(true);

        for policy in POLICIES {
            let result = load(&pkg, tmp.path(), policy, &mut (), &cancelled);
            assert!(matches!(result, Err(Error::Cancelled)));

            let mut loaded = load_plain(&pkg, tmp.path(), policy);
            let out = tmp.path().join("out");
            let result = loaded.extract(None, &out, &mut |_| {}, &cancelled);
            assert!(matches!(result, Err(Error::Cancelled)));
            assert!(listing(&out).is_empty());
            assert_eq!(listing(tmp.path()), ["out", "test.unitypackage"]);
        }
    }
}
