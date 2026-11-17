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
//! unknown while its bytes stream past. [`scan`] makes a cheap first pass to collect the
//! index, then [`extract`] streams again and writes each selected file straight to its
//! final location. No temp directory, no second copy.
//!
//! [`extract_single_pass`] is the alternative when no preview or selection is needed: it
//! decompresses once, staging payloads in a temp directory inside the output folder,
//! then renames them into place (same filesystem, so the move is cheap).

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use flate2::read::GzDecoder;

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

#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    /// Entries sorted by path.
    pub entries: Vec<Entry>,
    /// Raw pathnames that were refused because they would escape the output folder.
    pub rejected: Vec<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub files: usize,
    pub folders: usize,
    pub metas: usize,
    pub bytes: u64,
}

/// Progress notifications emitted while reading a package.
pub enum Progress<'a> {
    /// Compressed bytes consumed so far, out of `total`.
    Bytes { read: u64, total: u64 },
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

type PackageArchive = tar::Archive<GzDecoder<Counting<BufReader<File>>>>;

/// Opens a package, returning the archive, a live compressed-bytes counter and the total.
fn open(package: &Path) -> Result<(PackageArchive, Rc<Cell<u64>>, u64), Error> {
    let file = File::open(package).ctx(|| format!("failed to open {}", package.display()))?;
    let total = file.metadata().map(|m| m.len()).unwrap_or(0);
    let count = Rc::new(Cell::new(0));
    let reader = Counting {
        inner: BufReader::with_capacity(1 << 16, file),
        count: count.clone(),
    };
    Ok((tar::Archive::new(GzDecoder::new(reader)), count, total))
}

/// Reads the package index: every GUID that has a valid `pathname`.
pub fn scan(
    package: &Path,
    progress: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<ScanResult, Error> {
    #[derive(Default)]
    struct Partial {
        path: Option<String>,
        size: u64,
        has_asset: bool,
        has_meta: bool,
    }

    let (mut archive, count, total) = open(package)?;
    let ctx = || format!("failed to read {}", package.display());
    let mut by_guid: HashMap<String, Partial> = HashMap::new();
    let mut rejected = Vec::new();

    for member in archive.entries().ctx(ctx)? {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let mut member = member.ctx(ctx)?;
        let name = String::from_utf8_lossy(&member.path_bytes()).into_owned();
        match classify(&name) {
            Member::Asset(guid) => {
                let p = by_guid.entry(guid.to_owned()).or_default();
                p.size = member.header().size().unwrap_or(0);
                p.has_asset = true;
            }
            Member::Meta(guid) => by_guid.entry(guid.to_owned()).or_default().has_meta = true,
            Member::PathName(guid) => {
                let mut raw = String::new();
                member.read_to_string(&mut raw).ctx(ctx)?;
                match sanitize_pathname(&raw) {
                    Some(path) => by_guid.entry(guid.to_owned()).or_default().path = Some(path),
                    None => rejected.push(raw.lines().next().unwrap_or_default().to_owned()),
                }
            }
            Member::Other => {}
        }
        progress(Progress::Bytes {
            read: count.get(),
            total,
        });
    }

    let mut entries: Vec<Entry> = by_guid
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
    Ok(ScanResult { entries, rejected })
}

/// Extracts the entries whose GUID is in `selected` (or all of `entries` if `None`)
/// into `out_dir`, writing each file directly to its final path. Existing files are
/// overwritten.
pub fn extract(
    package: &Path,
    entries: &[Entry],
    selected: Option<&HashSet<String>>,
    out_dir: &Path,
    progress: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<Stats, Error> {
    let wanted: HashMap<&str, &Entry> = entries
        .iter()
        .filter(|e| selected.is_none_or(|s| s.contains(&e.guid)))
        .map(|e| (e.guid.as_str(), e))
        .collect();

    fs::create_dir_all(out_dir).ctx(|| format!("failed to create {}", out_dir.display()))?;
    let mut stats = Stats::default();

    // Folders have no payload; create them up front so selected empty folders still appear.
    for e in wanted.values().filter(|e| e.is_folder()) {
        let dir = out_dir.join(&e.path);
        fs::create_dir_all(&dir).ctx(|| format!("failed to create {}", dir.display()))?;
        stats.folders += 1;
    }

    let (mut archive, count, total) = open(package)?;
    let ctx = || format!("failed to read {}", package.display());
    for member in archive.entries().ctx(ctx)? {
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
        let Some(entry) = wanted.get(guid) else {
            continue;
        };

        let mut dest: PathBuf = out_dir.join(&entry.path);
        if is_meta {
            dest.as_mut_os_string().push(".meta");
        } else {
            progress(Progress::Writing(&entry.path));
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).ctx(|| format!("failed to create {}", parent.display()))?;
        }
        let mut out = File::create(&dest).ctx(|| format!("failed to create {}", dest.display()))?;
        let n = io::copy(&mut member, &mut out)
            .ctx(|| format!("failed to write {}", dest.display()))?;
        if is_meta {
            stats.metas += 1;
        } else {
            stats.files += 1;
            stats.bytes += n;
        }
        progress(Progress::Bytes {
            read: count.get(),
            total,
        });
    }
    Ok(stats)
}

#[derive(Debug, Default, Clone)]
pub struct SinglePassResult {
    pub stats: Stats,
    /// Raw pathnames that were refused because they would escape the output folder.
    pub rejected: Vec<String>,
}

/// GUIDs become directory names in the staging area, so only accept plain
/// alphanumerics (Unity uses 32 hex digits). Anything else is ignored.
fn is_safe_guid(guid: &str) -> bool {
    !guid.is_empty() && guid.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Extracts the whole package in a single decompression pass.
///
/// Payloads are written to a temp directory (`<out_dir>/.unity-unpacker-*`) as they
/// stream past, since their destination is usually only known once the `pathname`
/// member that follows them is read. After the archive ends, everything is moved into
/// place. The temp directory is removed on success, error, and cancellation alike.
pub fn extract_single_pass(
    package: &Path,
    out_dir: &Path,
    progress: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<SinglePassResult, Error> {
    #[derive(Default)]
    struct Staged {
        path: Option<String>,
        size: u64,
        has_asset: bool,
        has_meta: bool,
    }

    fs::create_dir_all(out_dir).ctx(|| format!("failed to create {}", out_dir.display()))?;
    let staging = tempfile::Builder::new()
        .prefix(".unity-unpacker-")
        .tempdir_in(out_dir)
        .ctx(|| format!("failed to create temp directory in {}", out_dir.display()))?;

    // Phase 1: stream once, staging payloads by GUID.
    let (mut archive, count, total) = open(package)?;
    let ctx = || format!("failed to read {}", package.display());
    let mut by_guid: HashMap<String, Staged> = HashMap::new();
    let mut rejected = Vec::new();

    for member in archive.entries().ctx(ctx)? {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let mut member = member.ctx(ctx)?;
        let name = String::from_utf8_lossy(&member.path_bytes()).into_owned();
        let (guid, file_name) = match classify(&name) {
            Member::Asset(g) => (g, "asset"),
            Member::Meta(g) => (g, "asset.meta"),
            Member::PathName(guid) if is_safe_guid(guid) => {
                let mut raw = String::new();
                member.read_to_string(&mut raw).ctx(ctx)?;
                match sanitize_pathname(&raw) {
                    Some(path) => by_guid.entry(guid.to_owned()).or_default().path = Some(path),
                    None => rejected.push(raw.lines().next().unwrap_or_default().to_owned()),
                }
                continue;
            }
            _ => continue,
        };
        if !is_safe_guid(guid) {
            continue;
        }

        let dir = staging.path().join(guid);
        fs::create_dir_all(&dir).ctx(|| format!("failed to create {}", dir.display()))?;
        let dest = dir.join(file_name);
        let mut out = File::create(&dest).ctx(|| format!("failed to create {}", dest.display()))?;
        let n = io::copy(&mut member, &mut out)
            .ctx(|| format!("failed to write {}", dest.display()))?;

        let staged = by_guid.entry(guid.to_owned()).or_default();
        if file_name == "asset" {
            staged.has_asset = true;
            staged.size = n;
        } else {
            staged.has_meta = true;
        }
        progress(Progress::Bytes {
            read: count.get(),
            total,
        });
    }

    // Phase 2: move staged payloads to their destinations. Sorting by path puts each
    // folder before its contents.
    let mut ready: Vec<(String, Staged)> = by_guid
        .into_iter()
        .filter(|(_, s)| s.path.is_some())
        .collect();
    ready.sort_by(|a, b| a.1.path.cmp(&b.1.path));

    let mut stats = Stats::default();
    for (guid, staged) in &ready {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let rel = staged.path.as_deref().unwrap_or_default();
        let dest = out_dir.join(rel);
        let src_dir = staging.path().join(guid);

        if staged.has_asset {
            progress(Progress::Writing(rel));
            move_file(&src_dir.join("asset"), &dest)?;
            stats.files += 1;
            stats.bytes += staged.size;
        } else {
            fs::create_dir_all(&dest).ctx(|| format!("failed to create {}", dest.display()))?;
            stats.folders += 1;
        }
        if staged.has_meta {
            let mut meta = dest.clone();
            meta.as_mut_os_string().push(".meta");
            move_file(&src_dir.join("asset.meta"), &meta)?;
            stats.metas += 1;
        }
    }

    staging
        .close()
        .ctx(|| "failed to remove temp directory".to_owned())?;
    Ok(SinglePassResult { stats, rejected })
}

/// Renames `src` to `dest`, replacing an existing file. Falls back to copy + delete if
/// rename fails (e.g. `out_dir` contains a mount point).
fn move_file(src: &Path, dest: &Path) -> Result<(), Error> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).ctx(|| format!("failed to create {}", parent.display()))?;
    }
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
    fn scan_and_extract_all() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = make_package(tmp.path());
        let never = AtomicBool::new(false);

        let scanned = scan(&pkg, &mut |_| {}, &never).unwrap();
        let paths: Vec<_> = scanned.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "Assets/Scripts/B.cs",
                "Assets/Textures",
                "Assets/Textures/a.png"
            ]
        );
        assert_eq!(scanned.rejected, ["../../evil.txt"]);
        assert!(scanned.entries[1].is_folder());
        assert_eq!(scanned.entries[2].size, 7);

        let out = tmp.path().join("out");
        let stats = extract(&pkg, &scanned.entries, None, &out, &mut |_| {}, &never).unwrap();
        assert_eq!(
            stats,
            Stats {
                files: 2,
                folders: 1,
                metas: 2,
                bytes: 7 + 18
            }
        );
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
        assert!(out.join("Assets/Scripts/B.cs").exists());
        assert!(!tmp.path().join("evil.txt").exists());
    }

    #[test]
    fn extract_selection() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = make_package(tmp.path());
        let never = AtomicBool::new(false);
        let scanned = scan(&pkg, &mut |_| {}, &never).unwrap();

        let out = tmp.path().join("out");
        let only: HashSet<String> = ["cccc".to_owned()].into();
        let stats = extract(
            &pkg,
            &scanned.entries,
            Some(&only),
            &out,
            &mut |_| {},
            &never,
        )
        .unwrap();
        assert_eq!(stats.files, 1);
        assert!(out.join("Assets/Scripts/B.cs").exists());
        assert!(!out.join("Assets/Textures").exists());
    }

    #[test]
    fn single_pass_matches_two_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = make_package(tmp.path());
        let never = AtomicBool::new(false);

        let out = tmp.path().join("out");
        // Pre-existing file must be replaced, not cause an error.
        fs::create_dir_all(out.join("Assets/Textures")).unwrap();
        fs::write(out.join("Assets/Textures/a.png"), b"old").unwrap();

        let result = extract_single_pass(&pkg, &out, &mut |_| {}, &never).unwrap();
        assert_eq!(
            result.stats,
            Stats {
                files: 2,
                folders: 1,
                metas: 2,
                bytes: 7 + 18
            }
        );
        assert_eq!(result.rejected, ["../../evil.txt"]);
        assert_eq!(
            fs::read(out.join("Assets/Textures/a.png")).unwrap(),
            b"PNGDATA"
        );
        assert_eq!(
            fs::read(out.join("Assets/Textures.meta")).unwrap(),
            b"folder meta"
        );
        assert!(out.join("Assets/Scripts/B.cs").exists());
        assert!(!tmp.path().join("evil.txt").exists());

        // Only "Assets" remains: the staging directory is gone.
        let leftovers: Vec<_> = fs::read_dir(&out)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, ["Assets"]);
    }

    #[test]
    fn single_pass_cancel_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = make_package(tmp.path());
        let out = tmp.path().join("out");
        let result = extract_single_pass(&pkg, &out, &mut |_| {}, &AtomicBool::new(true));
        assert!(matches!(result, Err(Error::Cancelled)));
        assert_eq!(fs::read_dir(&out).unwrap().count(), 0);
    }

    #[test]
    fn guid_validation() {
        assert!(is_safe_guid("0123456789abcdef0123456789abcdef"));
        assert!(!is_safe_guid(".."));
        assert!(!is_safe_guid("a.b"));
        assert!(!is_safe_guid(""));
    }

    #[test]
    fn cancel_stops() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = make_package(tmp.path());
        assert!(matches!(
            scan(&pkg, &mut |_| {}, &AtomicBool::new(true)),
            Err(Error::Cancelled)
        ));
    }
}
