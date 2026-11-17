# Unity Unpacker

Extracts `.unitypackage` files into a regular folder tree (assets plus their `.meta` files) without needing Unity.

Two binaries:

- **`unity-unpacker-gui`**: native desktop app (wxWidgets: Win32 controls on Windows, GTK on Linux). Open or drag-and-drop a package, browse its contents as a tree, tick what you want, and extract. You can pass a package path as the first argument, so it also works as an "Open with" handler.
- **`unity-unpacker`**: command line.

  ```
  unity-unpacker [--single-pass] <input.unitypackage> [output_folder]
  ```

  If no output folder is given, it extracts into a folder named after the package in the current directory.

  By default the package is read twice: once for the file list, then again to write each file straight to its final path. `--single-pass` decompresses it only once instead. Files are staged in a temporary `.unity-unpacker-*` folder inside the output folder, then moved into place (a cheap rename on the same disk). That's roughly 25% faster on large packages. The temp folder is removed on success, error, or cancel.

Paths inside a package that would escape the output folder (`../`, absolute, or drive paths) are skipped.

## Building

Requires Rust (stable), CMake, Ninja and a C++ compiler; the first build compiles wxWidgets and takes a few minutes.

- **Windows**: Visual Studio Build Tools (C++ workload). Visual Studio ships Ninja under `Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja`; put it on `PATH`, or `winget install Ninja-build.Ninja`.
- **Linux (Debian/Ubuntu)**:
  ```
  sudo apt install ninja-build cmake libclang-dev pkg-config libgtk-3-dev libpng-dev libjpeg-dev \
    libgl1-mesa-dev libglu1-mesa-dev libxkbcommon-dev libexpat1-dev libtiff-dev
  ```

```
cargo build --release
```

To build only the CLI (no C++ toolchain needed):

```
cargo build --release --no-default-features
```

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
