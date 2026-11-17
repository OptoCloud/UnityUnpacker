# Unity Unpacker

Extracts `.unitypackage` files into a regular folder tree (assets plus their `.meta` files) without needing Unity.

Two binaries:

- **`unity-unpacker-gui`**: native desktop app (wxWidgets: Win32 controls on Windows, GTK on Linux). Open a package, or drag and drop one onto the window or onto the executable, browse its contents as a tree, preview files, tick what you want, and extract. You can pass a package path as the first argument, so it also works as an "Open with" handler.
- **`unity-unpacker`**: command line.

  ```
  unity-unpacker [options] <package> [output_folder]
  unity-unpacker [options] <package> <package>...
  ```

  If no output folder is given, it extracts into a folder named after the package in the current directory.

  On Windows you can also drag one or more packages onto `unity-unpacker.exe`. Each is extracted into a folder named after it, next to the package, and the console window stays open until you press Enter.

Paths inside a package that would escape the output folder (`../`, absolute, or drive paths) are skipped.

## Extraction modes

A package is a compressed archive that lists each file's name *after* its contents, so the tool cannot know where a file belongs while reading it. Opening a package therefore reads all of it once. What differs between the modes is what is kept from that read: each file's contents end up in memory, in a temp folder, or not loaded at all.

| Mode | What opening a package keeps | Good for |
| --- | --- | --- |
| `two-pass` | Only the file list. Extracting reads the package a second time and writes each chosen file straight to its final path. | Least disk space and memory. |
| `single-pass` (default) | Files up to 8 MB in memory, up to 500 MB in total; larger files, and everything after that limit is reached, in a temporary `.unity-unpacker-*` folder. | Large packages: decompressing only once is roughly 25% faster. |
| `in-memory` | Everything, in RAM. Nothing touches the disk before you extract. | Speed, when the package fits in memory. |

Extracting writes what is ticked: loaded files are written or moved into place, and anything that is not loaded is read from the package. Whatever was loaded but not ticked is then discarded, and the temp folder is removed. That also happens on cancel, when another package is opened, and on exit. In the GUI the temp folder sits next to the package, so moving files into the default output folder is a cheap rename; on the command line it goes inside the output folder.

**Running low on memory.** Before a file is read into memory, the tool checks that doing so would still leave 10% of the PC's memory free. If not, the GUI pauses loading and asks:

- **Use temp folder**: what is in memory is written to the temp folder and the rest of the package is loaded there too.
- **Switch to two pass**: what is in memory stays, the rest is left in the package and read from it when you extract.
- **Cancel loading**.

The command line does not ask and continues in the temp folder.

On the command line, pick a mode with `--mode <name>` (or `--two-pass`, `--single-pass`, `--in-memory`).

## Previews

Selecting a file in the GUI shows it in the panel on the right:

- **Images** (PNG, JPEG, BMP, GIF, TGA, TIFF) are shown scaled to the panel.
- **Audio** (WAV, OGG, MP3, FLAC, AIFF) gets Play and Stop buttons.
- **Text** (scripts, shaders, JSON, YAML assets and anything else that turns out to be text) shows its first 256 KB. **Open copy in editor** saves a copy to the system temp folder and opens it in the program registered for its type; the copies are deleted when the app closes. Files whose extension is not a known text or code type get `.txt` added to the copy's name, so a package cannot have something run by being "previewed".

A file that is loaded previews instantly. One that is not (everything in two pass mode) has to be found by reading through the package again, every time. To avoid that in two pass mode, turn on **Also load images and sounds into memory** in the settings. With a pixel limit set, images are kept as reduced copies instead of the original files, which uses less memory but makes opening the package slower, since every image is decoded.

## Settings

The GUI shows the settings the first time it starts; change them later with the **Settings...** button. They are saved as JSON and shared with the command line, where `--mode` overrides the saved mode:

- Windows: `%APPDATA%\UnityUnpacker\settings.json`
- Linux: `$XDG_CONFIG_HOME/unity-unpacker/settings.json` (default `~/.config`)

```json
{
  "mode": "single-pass",
  "memory_limit_mb": 500,
  "small_file_mb": 8,
  "memory_reserve_percent": 10,
  "preload_previews": false,
  "preview_max_px": 0
}
```

| Setting | Meaning |
| --- | --- |
| `mode` | `two-pass`, `single-pass` or `in-memory`. |
| `memory_limit_mb` | Single pass: most memory the loaded files may use in total. |
| `small_file_mb` | Single pass: files larger than this go straight to the temp folder. |
| `memory_reserve_percent` | Share of the PC's memory that loading leaves free before it pauses and asks. |
| `preload_previews` | Two pass, GUI only: also keep images and sounds in memory for instant previews. |
| `preview_max_px` | With `preload_previews`: keep images as copies of at most this many pixels on the longer side. `0` keeps the original files. |

## Building

Requires Rust (stable), CMake, Ninja and a C++ compiler; the first build compiles wxWidgets and takes a few minutes.

- **Windows**: Visual Studio Build Tools (C++ workload). Visual Studio ships Ninja under `Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja`; put it on `PATH`, or `winget install Ninja-build.Ninja`.
- **Linux (Debian/Ubuntu)**:
  ```
  sudo apt install ninja-build cmake libclang-dev pkg-config libgtk-3-dev libpng-dev libjpeg-dev \
    libgl1-mesa-dev libglu1-mesa-dev libxkbcommon-dev libexpat1-dev libtiff-dev libasound2-dev
  ```

```
cargo build --release
```

To build only the CLI (no C++ toolchain needed):

```
cargo build --release --no-default-features
```

The icons in `assets/icons` are embedded in the GUI; `assets/app.ico` is the executables' icon on Windows.

## Releasing

1. On a branch, bump `version` in `Cargo.toml`, run `cargo check` to refresh `Cargo.lock`, and merge it to `master` through a PR so CI runs.
2. Tag the merge commit and push the tag:
   ```
   git switch master && git pull
   git tag -a v0.2.0 -m "v0.2.0"
   git push origin v0.2.0
   ```
3. The `Release` workflow checks that the tag matches `Cargo.toml` and is on `master`, builds and tests on Linux and Windows, and creates a **draft** release with the archives, `SHA256SUMS.txt` and generated notes.
4. Download the archives from the draft, smoke-test the GUI, edit the notes if needed, then click **Publish**.

If a release build fails, fix it on `master`, then delete and re-push the tag (`git push --delete origin v0.2.0`, `git tag -d v0.2.0`), plus the draft if one was created.
