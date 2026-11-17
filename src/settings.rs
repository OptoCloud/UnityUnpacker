//! User settings shared by the GUI and the command line, stored as JSON.
//!
//! The file lives in the per-user config directory (see [`Settings::path`]). A missing or
//! unreadable file simply means "not configured yet": the GUI then shows the settings
//! dialog on startup and the CLI falls back to [`Settings::default`].

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::LoadPolicy;

const MIB: u64 = 1024 * 1024;

/// How a package is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Read the index, then read the package again to write the chosen files directly.
    TwoPass,
    /// Read once; small files wait in memory (up to a limit), the rest in a temp folder.
    #[default]
    SinglePass,
    /// Read once and hold everything in memory until it is written out. If memory runs
    /// low part-way, the user is asked whether to continue in the temp folder or to
    /// leave the rest in the package.
    InMemory,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::TwoPass, Mode::SinglePass, Mode::InMemory];

    /// The name used in the settings file and on the command line.
    pub fn name(self) -> &'static str {
        match self {
            Mode::TwoPass => "two-pass",
            Mode::SinglePass => "single-pass",
            Mode::InMemory => "in-memory",
        }
    }

    pub fn from_name(name: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|m| m.name() == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub mode: Mode,
    /// Single pass: most memory the staged files may use in total, in MiB.
    pub memory_limit_mb: u64,
    /// Single pass: files larger than this many MiB always go to the temp folder.
    pub small_file_mb: u64,
    /// Share of physical memory, in percent, that loading into memory leaves free.
    /// Loading pauses and asks how to continue rather than cut into it.
    pub memory_reserve_percent: u64,
    /// Two pass: also keep images and sounds in memory, so previewing them doesn't
    /// have to search the package. Only the GUI, which has previews, uses this.
    pub preload_previews: bool,
    /// With `preload_previews`: keep images as copies of at most this many pixels on
    /// the longer side instead of the original files. 0 keeps the originals.
    pub preview_max_px: u64,
    /// GUI: use dark colours. Takes effect the next time the app starts.
    pub dark_mode: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mode: Mode::default(),
            memory_limit_mb: 500,
            small_file_mb: 8,
            memory_reserve_percent: LoadPolicy::DEFAULT_RESERVE_PERCENT as u64,
            preload_previews: false,
            preview_max_px: 0,
            dark_mode: false,
        }
    }
}

impl Settings {
    /// What loading a package reads up front in this mode, and where it puts it.
    pub fn load_policy(&self) -> LoadPolicy {
        let policy = match self.mode {
            Mode::TwoPass if self.preload_previews => LoadPolicy::TWO_PASS_WITH_MEDIA,
            Mode::TwoPass => LoadPolicy::TWO_PASS,
            Mode::SinglePass => LoadPolicy::single_pass(
                self.small_file_mb.saturating_mul(MIB),
                self.memory_limit_mb.saturating_mul(MIB),
            ),
            Mode::InMemory => LoadPolicy::IN_MEMORY,
        };
        policy.with_reserve_percent(self.memory_reserve_percent.min(100) as u8)
    }

    /// Location of the settings file:
    ///
    /// * Windows: `%APPDATA%\UnityUnpacker\settings.json`
    /// * macOS: `~/Library/Application Support/UnityUnpacker/settings.json`
    /// * elsewhere: `$XDG_CONFIG_HOME/unity-unpacker/settings.json`, defaulting to
    ///   `~/.config`
    ///
    /// `None` if the environment doesn't say where that is.
    pub fn path() -> Option<PathBuf> {
        let dir = |var: &str| {
            std::env::var_os(var)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let folder = if cfg!(windows) {
            dir("APPDATA")?.join("UnityUnpacker")
        } else if cfg!(target_os = "macos") {
            dir("HOME")?.join("Library/Application Support/UnityUnpacker")
        } else {
            dir("XDG_CONFIG_HOME")
                .or_else(|| Some(dir("HOME")?.join(".config")))?
                .join("unity-unpacker")
        };
        Some(folder.join("settings.json"))
    }

    /// Loads the saved settings, or `None` if there are none yet (or they are invalid).
    pub fn load() -> Option<Settings> {
        Self::load_from(&Self::path()?)
    }

    pub fn load_from(path: &Path) -> Option<Settings> {
        serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
    }

    /// Saves to [`Settings::path`], creating the folder if needed.
    pub fn save(&self) -> io::Result<()> {
        let path = Self::path().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no per-user config directory")
        })?;
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut json = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        json.push('\n');
        fs::write(path, json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/settings.json");
        assert_eq!(Settings::load_from(&path), None);

        let settings = Settings {
            mode: Mode::InMemory,
            memory_limit_mb: 123,
            small_file_mb: 4,
            memory_reserve_percent: 25,
            preload_previews: true,
            preview_max_px: 512,
            dark_mode: true,
        };
        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), Some(settings));
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains(r#""mode": "in-memory""#)
        );
    }

    #[test]
    fn missing_fields_use_defaults_and_garbage_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");

        fs::write(&path, r#"{ "mode": "two-pass", "future_option": true }"#).unwrap();
        assert_eq!(
            Settings::load_from(&path),
            Some(Settings {
                mode: Mode::TwoPass,
                ..Settings::default()
            })
        );

        fs::write(&path, r#"{ "mode": "warp-speed" }"#).unwrap();
        assert_eq!(Settings::load_from(&path), None);
        fs::write(&path, "not json").unwrap();
        assert_eq!(Settings::load_from(&path), None);
    }

    #[test]
    fn policies() {
        let mut settings = Settings::default();
        assert_eq!(
            settings.load_policy(),
            LoadPolicy::single_pass(8 * MIB, 500 * MIB)
        );
        settings.mode = Mode::InMemory;
        assert_eq!(settings.load_policy(), LoadPolicy::IN_MEMORY);
        settings.memory_reserve_percent = 250;
        assert_eq!(
            settings.load_policy(),
            LoadPolicy::IN_MEMORY.with_reserve_percent(100)
        );
        settings.memory_reserve_percent = 10;
        settings.mode = Mode::TwoPass;
        assert_eq!(settings.load_policy(), LoadPolicy::TWO_PASS);
        settings.preload_previews = true;
        assert_eq!(settings.load_policy(), LoadPolicy::TWO_PASS_WITH_MEDIA);
        for mode in Mode::ALL {
            assert_eq!(Mode::from_name(mode.name()), Some(mode));
        }
    }
}
