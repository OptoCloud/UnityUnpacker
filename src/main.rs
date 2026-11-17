//! Command-line front end.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;

use unity_unpacker::{Progress, Stats, extract, extract_single_pass, format_size, scan};

const USAGE: &str = "\
Usage: unity-unpacker [--single-pass] <input_file> [output_folder]

Options:
  --single-pass  Decompress the package once, staging files in a temp folder inside
                 the output folder and moving them into place at the end. Faster on
                 large packages; needs room for the extracted files plus the move.
                 Default: read the index first, then write files directly.
  -h, --help     Show this help.";

fn main() -> ExitCode {
    let mut single_pass = false;
    let mut positional = Vec::new();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--single-pass" => single_pass = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            flag if flag.starts_with("--") => {
                eprintln!("Unknown option: {flag}\n\n{USAGE}");
                return ExitCode::FAILURE;
            }
            _ => positional.push(arg),
        }
    }
    let (input, output) = match positional.as_slice() {
        [input] => (PathBuf::from(input), default_output(Path::new(input))),
        [input, output] => (PathBuf::from(input), PathBuf::from(output)),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    println!("[INFO] Input file: {}", input.display());
    println!("[INFO] Output directory: {}", output.display());

    let never = AtomicBool::new(false);
    let mut log = |p: Progress| {
        if let Progress::Writing(path) = p {
            println!("[INFO] Writing {path}");
        }
    };

    let result = if single_pass {
        extract_single_pass(&input, &output, &mut log, &never).map(|r| (r.stats, r.rejected))
    } else {
        two_pass(&input, &output, &mut log, &never)
    };

    match result {
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
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[ERROR] {e}");
            ExitCode::FAILURE
        }
    }
}

fn two_pass(
    input: &Path,
    output: &Path,
    log: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<(Stats, Vec<String>), unity_unpacker::Error> {
    let scanned = scan(input, &mut |_| {}, cancel)?;
    println!("[INFO] Found {} assets", scanned.entries.len());
    let stats = extract(input, &scanned.entries, None, output, log, cancel)?;
    Ok((stats, scanned.rejected))
}

/// Matches the original tool: a folder named after the package, in the working directory.
fn default_output(input: &Path) -> PathBuf {
    PathBuf::from(input.file_stem().unwrap_or(input.as_os_str()))
}
