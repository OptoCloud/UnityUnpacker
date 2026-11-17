//! Command-line front end.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;

use unity_unpacker::settings::{Mode, Settings};
use unity_unpacker::{
    Error, LoadEvents, LowMemory, OnLowMemory, Progress, Stats, format_size, load,
};

const USAGE: &str = "\
Usage: unity-unpacker [options] <package> [output_folder]
       unity-unpacker [options] <package> <package>...

You can also drag one or more .unitypackage files onto the executable. Each package
is then extracted into a folder named after it, next to the package.

Options:
  --mode <mode>  How to read the package. Defaults to the mode saved in the settings
                 file (shared with the GUI), or single-pass if there is none.
                   two-pass     Read the index, then read again and write each file
                                straight to its final path. Least disk and memory.
                   single-pass  Decompress once. Small files wait in memory (500 MB
                                by default), the rest in a temp folder inside the
                                output folder; all are put in place at the end.
                   in-memory    Decompress once, holding everything in memory, then
                                write it out. Switches to a temp folder if the
                                machine runs low on memory.
  --two-pass, --single-pass, --in-memory
                 Shorthand for --mode <mode>.
  -h, --help     Show this help.";

fn main() -> ExitCode {
    // Started from Explorer (double-click or drag-and-drop) rather than a terminal:
    // the console window closes on exit and the working directory is arbitrary.
    let standalone = owns_console();
    let code = run(standalone);
    if standalone {
        println!("\nPress Enter to close this window.");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    code
}

fn run(standalone: bool) -> ExitCode {
    let mut mode = None;
    let mut positional: Vec<PathBuf> = Vec::new();
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("-h" | "--help") => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            Some("--mode") => {
                let value = args.next().unwrap_or_default();
                match value.to_str().and_then(Mode::from_name) {
                    Some(m) => mode = Some(m),
                    None => return usage_error(&format!("Unknown mode: {}", value.display())),
                }
            }
            Some(flag) if flag.starts_with("--") => match Mode::from_name(&flag[2..]) {
                Some(m) => mode = Some(m),
                None => return usage_error(&format!("Unknown option: {flag}")),
            },
            _ => positional.push(PathBuf::from(arg)),
        }
    }

    let mut settings = Settings::load().unwrap_or_default();
    if let Some(mode) = mode {
        settings.mode = mode;
    }
    settings.preload_previews = false; // nothing to preview here

    // Everything after the first argument being an existing file means a batch of
    // packages (e.g. several dropped on the executable), not "package + output folder".
    let batch = positional.len() >= 2 && positional[1..].iter().all(|p| p.is_file());
    let jobs: Vec<(PathBuf, PathBuf)> = match positional.as_slice() {
        [input, output] if !batch => vec![(input.clone(), output.clone())],
        inputs if !inputs.is_empty() && (batch || inputs.len() == 1) => inputs
            .iter()
            .map(|input| (input.clone(), default_output(input, standalone)))
            .collect(),
        _ => {
            if standalone && positional.is_empty() {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            return usage_error("Expected a package and an optional output folder.");
        }
    };

    let mut failed = 0;
    for (i, (input, output)) in jobs.iter().enumerate() {
        if i > 0 {
            println!();
        }
        println!("[INFO] Input file: {}", input.display());
        println!("[INFO] Output directory: {}", output.display());
        println!("[INFO] Mode: {}", settings.mode.name());

        match unpack(input, output, &settings) {
            Ok((stats, rejected)) => {
                for bad in &rejected {
                    println!("[WARN] Skipped unsafe path: {bad}");
                }
                println!(
                    "[SUCCESS] Unpacked {} files ({}), {} folders into {}",
                    stats.files,
                    format_size(stats.bytes),
                    stats.folders,
                    output.display()
                );
            }
            Err(e) => {
                eprintln!("[ERROR] {e}");
                failed += 1;
            }
        }
    }
    if jobs.len() > 1 {
        println!(
            "\n[INFO] {} of {} packages unpacked",
            jobs.len() - failed,
            jobs.len()
        );
    }
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn usage_error(message: &str) -> ExitCode {
    eprintln!("{message}\n\n{USAGE}");
    ExitCode::FAILURE
}

fn unpack(input: &Path, output: &Path, settings: &Settings) -> Result<(Stats, Vec<String>), Error> {
    let never = AtomicBool::new(false);
    // Temp files go inside the output folder so putting them in place is a rename.
    let mut loaded = load(input, output, settings.load_policy(), &mut Events, &never)?;
    println!(
        "[INFO] Found {} assets ({} in memory, {} in temp files)",
        loaded.entries.len(),
        format_size(loaded.memory_bytes()),
        format_size(loaded.temp_bytes())
    );

    let mut log = |p: Progress| {
        if let Progress::Writing(path) = p {
            println!("[INFO] Writing {path}");
        }
    };
    let stats = loaded.extract(None, output, &mut log, &never)?;
    Ok((stats, std::mem::take(&mut loaded.rejected)))
}

struct Events;

impl LoadEvents for Events {
    /// Nobody may be there to answer a prompt, so running low on memory in in-memory
    /// mode falls back to temp files, which always works.
    fn low_memory(&mut self, low: LowMemory) -> OnLowMemory {
        println!(
            "[WARN] Low on memory with {} loaded; continuing in a temp folder",
            format_size(low.in_memory)
        );
        OnLowMemory::UseTemp
    }
}

/// A folder named after the package: in the working directory when run from a terminal
/// (as the original tool did), next to the package when started from Explorer, where
/// the working directory is not somewhere the user chose.
fn default_output(input: &Path, standalone: bool) -> PathBuf {
    let stem: OsString = input.file_stem().unwrap_or(input.as_os_str()).to_owned();
    match input.parent() {
        Some(parent) if standalone => parent.join(stem),
        _ => PathBuf::from(stem),
    }
}

/// True if this process is the only one attached to its console, i.e. Windows created
/// the console for it instead of it inheriting a terminal.
#[cfg(windows)]
fn owns_console() -> bool {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetConsoleProcessList(process_list: *mut u32, process_count: u32) -> u32;
    }
    let mut pids = [0u32; 2];
    // SAFETY: the buffer is valid for the two entries its length is given as.
    unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) == 1 }
}

#[cfg(not(windows))]
fn owns_console() -> bool {
    false
}
