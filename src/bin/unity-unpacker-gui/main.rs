//! Native desktop front end (wxWidgets: Win32 on Windows, GTK on Linux).
//!
//! `unity-unpacker-gui [package.unitypackage]` opens the window, optionally preloading a
//! package (so it works as a file-association handler).

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};

use std::thread::JoinHandle;

use unity_unpacker::settings::{Mode, Settings};
use unity_unpacker::{
    Entry, LoadEvents, Loaded, LowMemory, Media, OnLowMemory, Progress, Stats, format_size, load,
};
use wxdragon::appearance::{AppAppearance, Appearance};
use wxdragon::prelude::*;
use wxdragon::timer::Timer;
use wxdragon::widgets::button::ButtonBitmapPosition;
use wxdragon::widgets::dataview::DataViewCtrl;

mod icons;
mod preview;
mod tree;

use icons::{Icon, Icons, WindowRef};
use preview::{Preview, Thumb};
use tree::Tree;

const GAUGE_RANGE: i32 = 1000;
const PACKAGE_WILDCARD: &str = "Unity packages (*.unitypackage)|*.unitypackage|All files (*.*)|*.*";

/// Reduced copies of images made while loading, by GUID.
type Thumbs = HashMap<String, Arc<Thumb>>;

/// Messages from worker threads to the UI thread.
enum Msg {
    Progress {
        fraction: f64,
        writing: Option<String>,
    },
    /// Loading has paused because memory is short; the worker waits for the answer.
    LowMemory {
        low: LowMemory,
        reply: Sender<OnLowMemory>,
    },
    Loaded(Result<(Loaded, Thumbs), String>),
    Extracted {
        out_dir: PathBuf,
        result: Result<Stats, String>,
        /// Handed back so the package can be previewed and extracted again.
        loaded: Loaded,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Busy {
    Idle,
    Loading,
    Extracting,
}

struct State {
    /// The open package: its file list and whatever of its contents is loaded.
    /// Away on the worker thread while extracting. Dropping it deletes its temp folder.
    loaded: Option<Loaded>,
    /// The open package's file list (a copy, so it is there while `loaded` is away).
    entries: Vec<Entry>,
    /// Reduced copies of the package's images, when the settings ask for them.
    thumbs: Thumbs,
    tree: Rc<Tree>,
    preview: Rc<Preview>,
    icons: Rc<Icons>,
    settings: Settings,
    busy: Busy,
    cancel: Arc<AtomicBool>,
    rx: Option<Receiver<Msg>>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy)]
struct Ui {
    frame: Frame,
    panel: Panel,
    package_text: TextCtrl,
    package_browse: Button,
    output_text: TextCtrl,
    output_browse: Button,
    tree: DataViewCtrl,
    select_all: Button,
    select_none: Button,
    expand_all: Button,
    summary: StaticText,
    gauge: Gauge,
    status: StaticText,
    settings: Button,
    open_folder: Button,
    extract: Button,
}

fn main() {
    let initial = std::env::args_os().nth(1).map(PathBuf::from);
    let _ = wxdragon::main(move |_| {
        let saved = Settings::load();
        // Has to be decided before the first window exists.
        if saved.is_some_and(|s| s.dark_mode)
            && let Some(app) = wxdragon::app::get_app()
        {
            app.set_appearance(Appearance::Dark);
        }
        let (ui, tree, preview, icons) = build_ui();
        let state = Rc::new(RefCell::new(State {
            loaded: None,
            entries: Vec::new(),
            thumbs: Thumbs::new(),
            tree,
            preview,
            icons,
            settings: saved.unwrap_or_default(),
            busy: Busy::Idle,
            cancel: Arc::new(AtomicBool::new(false)),
            rx: None,
            worker: None,
        }));
        wire_events(ui, &state);

        // Poll worker messages on the UI thread. The timer must outlive this closure, so
        // it is leaked deliberately; there is exactly one for the life of the app.
        let timer = Box::leak(Box::new(Timer::new(&ui.frame)));
        let s = state.clone();
        timer.on_tick(move |_| pump(ui, &s));
        timer.start(50, false);

        ui.frame.show(true);
        ui.frame.centre();
        refresh_controls(ui, &state.borrow());

        // First run: let the user pick how packages are read before anything is loaded.
        if saved.is_none() {
            edit_settings(ui, &state, true);
        }
        if let Some(path) = initial {
            start_load(ui, &state, path);
        }
    });
}

fn build_ui() -> (Ui, Rc<Tree>, Rc<Preview>, Rc<Icons>) {
    let frame = Frame::builder()
        .with_title("Unity Package Unpacker")
        .with_size(Size::new(1040, 660))
        .build();
    let panel = Panel::builder(&frame).build();
    let icons = Rc::new(Icons::load(&frame));
    if let Some(app_icon) = Icon::PrefabModel.bitmap(64, false) {
        frame.set_icon(&app_icon);
    }
    let with_icon = |button: Button, icon: Icon| {
        if let Some(bitmap) = icons.get(icon) {
            button.set_bitmap(bitmap, ButtonBitmapPosition::Left);
        }
        button
    };

    let label = |text: &str| StaticText::builder(&panel).with_label(text).build();
    let package_text = TextCtrl::builder(&panel).build();
    let package_browse = with_icon(
        Button::builder(&panel).with_label("Browse...").build(),
        Icon::PrefabModel,
    );
    let output_text = TextCtrl::builder(&panel).build();
    let output_browse = with_icon(
        Button::builder(&panel).with_label("Browse...").build(),
        Icon::Folder,
    );

    let paths = FlexGridSizer::builder(2, 3)
        .with_hgap(8)
        .with_vgap(8)
        .build();
    paths.add_growable_col(1, 1);
    let center = SizerFlag::AlignCenterVertical;
    paths.add(&label("Package:"), 0, center, 0);
    paths.add(&package_text, 1, SizerFlag::Expand | center, 0);
    paths.add(&package_browse, 0, center, 0);
    paths.add(&label("Output folder:"), 0, center, 0);
    paths.add(&output_text, 1, SizerFlag::Expand | center, 0);
    paths.add(&output_browse, 0, center, 0);

    // File tree on the left, preview of the selected file on the right.
    let splitter = SplitterWindow::builder(&panel)
        .with_style(SplitterWindowStyle::LiveUpdate)
        .build();
    let tree = Tree::new(&splitter, icons.clone());
    let preview = Preview::new(&splitter);
    splitter.set_minimum_pane_size(frame.from_dip_int(160));
    splitter.split_vertically(&tree.ctrl, &preview.panel(), frame.from_dip_int(-340));
    // Window resizes go to the tree; the preview keeps the width the user gave it.
    let preview_width = Rc::new(std::cell::Cell::new(frame.from_dip_int(340)));
    let width = preview_width.clone();
    splitter.on_sash_position_changed(move |_| {
        width.set(splitter.get_client_size().width - splitter.sash_position());
    });
    splitter.on_size(move |event| {
        let total = splitter.get_client_size().width;
        if total > preview_width.get() {
            splitter.set_sash_position(total - preview_width.get(), true);
        }
        event.skip(true);
    });

    let select_all = Button::builder(&panel).with_label("Select all").build();
    let select_none = Button::builder(&panel).with_label("Select none").build();
    let expand_all = Button::builder(&panel).with_label("Expand all").build();
    let summary = StaticText::builder(&panel).with_label("").build();
    let selection_row = BoxSizer::builder(Orientation::Horizontal).build();
    selection_row.add(&select_all, 0, SizerFlag::Right, 6);
    selection_row.add(&select_none, 0, SizerFlag::Right, 6);
    selection_row.add(&expand_all, 0, SizerFlag::Right, 12);
    selection_row.add(&summary, 1, center, 0);

    let gauge = Gauge::builder(&panel).with_range(GAUGE_RANGE).build();
    let status = StaticText::builder(&panel)
        .with_label("Open or drop a .unitypackage to begin.")
        .build();
    let settings = with_icon(
        Button::builder(&panel).with_label("Settings...").build(),
        Icon::ScriptableObject,
    );
    let open_folder = with_icon(
        Button::builder(&panel)
            .with_label("Open output folder")
            .build(),
        Icon::Folder,
    );
    let extract = Button::builder(&panel).with_label("Extract").build();
    let action_row = BoxSizer::builder(Orientation::Horizontal).build();
    action_row.add(&status, 1, center | SizerFlag::Right, 12);
    action_row.add(&settings, 0, SizerFlag::Right, 6);
    action_row.add(&open_folder, 0, SizerFlag::Right, 6);
    action_row.add(&extract, 0, SizerFlag::empty(), 0);

    let root = BoxSizer::builder(Orientation::Vertical).build();
    root.add_sizer(&paths, 0, SizerFlag::Expand | SizerFlag::All, 12);
    root.add(
        &splitter,
        1,
        SizerFlag::Expand | SizerFlag::Left | SizerFlag::Right,
        12,
    );
    root.add_sizer(&selection_row, 0, SizerFlag::Expand | SizerFlag::All, 12);
    root.add(
        &gauge,
        0,
        SizerFlag::Expand | SizerFlag::Left | SizerFlag::Right,
        12,
    );
    root.add_sizer(&action_row, 0, SizerFlag::Expand | SizerFlag::All, 12);
    panel.set_sizer(root, true);

    let frame_sizer = BoxSizer::builder(Orientation::Vertical).build();
    frame_sizer.add(&panel, 1, SizerFlag::Expand, 0);
    frame.set_sizer(frame_sizer, true);
    frame.set_min_size(Size::new(640, 420));

    let ui = Ui {
        frame,
        panel,
        package_text,
        package_browse,
        output_text,
        output_browse,
        tree: tree.ctrl,
        select_all,
        select_none,
        expand_all,
        summary,
        gauge,
        status,
        settings,
        open_folder,
        extract,
    };
    (ui, Rc::new(tree), preview, icons)
}

fn wire_events(ui: Ui, state: &Rc<RefCell<State>>) {
    let s = state.clone();
    ui.package_browse.on_click(move |_| {
        let dialog = FileDialog::builder(&ui.frame)
            .with_message("Open Unity package")
            .with_wildcard(PACKAGE_WILDCARD)
            .with_style(FileDialogStyle::Open | FileDialogStyle::FileMustExist)
            .build();
        if dialog.show_modal() == ID_OK
            && let Some(path) = dialog.get_path()
        {
            start_load(ui, &s, PathBuf::from(path));
        }
    });

    ui.output_browse.on_click(move |_| {
        let current = ui.output_text.get_value();
        let dialog = DirDialog::builder(&ui.frame, "Choose output folder", &current).build();
        if dialog.show_modal() == ID_OK
            && let Some(path) = dialog.get_path()
        {
            ui.output_text.set_value(&path);
        }
    });

    // Enter in the package box loads whatever path was typed or pasted.
    let s = state.clone();
    ui.package_text.on_text_enter(move |_| {
        let text = ui.package_text.get_value();
        if !text.trim().is_empty() {
            start_load(ui, &s, PathBuf::from(text.trim()));
        }
    });

    let tree = state.borrow().tree.clone();
    let s = state.clone();
    tree.on_change(move || {
        // Ticking a folder changes the rows beneath and above it as well.
        ui.tree.refresh(false, None);
        update_summary(ui, &s.borrow());
    });

    for (button, ticked) in [(ui.select_all, true), (ui.select_none, false)] {
        let s = state.clone();
        button.on_click(move |_| {
            let st = s.borrow();
            st.tree.tick_all(ticked);
            update_summary(ui, &st);
        });
    }
    let t = tree.clone();
    ui.expand_all.on_click(move |_| t.expand_all());

    // Selecting a file previews it.
    let s = state.clone();
    tree.on_select(move |entry| {
        let st = s.borrow();
        let request = match (entry.and_then(|i| st.entries.get(i)), &st.loaded) {
            (Some(entry), Some(loaded)) => Some(preview::Request {
                name: entry.path.rsplit('/').next().unwrap_or_default().to_owned(),
                size: entry.size,
                source: loaded.source(&entry.guid),
                thumb: st.thumbs.get(&entry.guid).cloned(),
            }),
            _ => None,
        };
        st.preview.show(request);
    });

    let s = state.clone();
    ui.settings.on_click(move |_| edit_settings(ui, &s, false));

    ui.open_folder
        .on_click(move |_| open_in_file_manager(Path::new(&ui.output_text.get_value())));

    let s = state.clone();
    ui.extract.on_click(move |_| {
        let busy = s.borrow().busy;
        match busy {
            Busy::Idle => start_extract(ui, &s),
            Busy::Loading | Busy::Extracting => {
                s.borrow().cancel.store(true, Ordering::Relaxed);
                ui.status.set_label("Cancelling...");
            }
        }
    });

    // Drop a package anywhere on the window to open it.
    for target in [
        &ui.frame as &dyn WxWidget,
        &ui.panel as &dyn WxWidget,
        &ui.tree as &dyn WxWidget,
    ] {
        let s = state.clone();
        FileDropTarget::builder(&WindowRef(target.handle_ptr()))
            .with_on_drop_files(move |files, _, _| {
                // Folders and other strays can ride along in a multi-file drop.
                let Some(package) = files
                    .iter()
                    .map(PathBuf::from)
                    .find(|p| is_package(p))
                    .or_else(|| files.first().map(PathBuf::from))
                else {
                    return false;
                };
                if s.borrow().busy != Busy::Idle {
                    return false;
                }
                start_load(ui, &s, package);
                true
            })
            .build();
    }

    // The timer keeps `state` alive for the life of the process, so it is never
    // dropped: stop the workers and delete the temp folder by hand when closing.
    let s = state.clone();
    ui.frame.on_close(move |event| {
        let (worker, preview) = {
            let mut st = s.borrow_mut();
            st.cancel.store(true, Ordering::Relaxed);
            st.loaded = None;
            st.rx = None;
            (st.worker.take(), st.preview.clone())
        };
        // Cancellation is checked every 64 KB, so these return almost immediately; a
        // worker that still holds the package removes the temp folder on its way out.
        preview.halt();
        preview.cleanup();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
        event.skip(true);
    });
}

fn is_package(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("unitypackage"))
}

// ---------------------------------------------------------------------------
// Background work

fn start_load(ui: Ui, state: &Rc<RefCell<State>>, package: PathBuf) {
    let mut st = state.borrow_mut();
    if st.busy != Busy::Idle {
        return;
    }
    ui.package_text.set_value(&package.to_string_lossy());
    let out_dir = default_output(&package);
    ui.output_text.set_value(&out_dir.to_string_lossy());
    st.preview.set_package(None);
    st.loaded = None; // deletes the previous package's temp folder
    st.entries.clear();
    st.thumbs.clear();
    st.tree.set_entries(&[]);
    ui.gauge.set_value(0);
    let settings = st.settings;
    let policy = settings.load_policy();
    ui.status.set_label(match st.settings.mode {
        Mode::TwoPass => "Reading file list...",
        Mode::SinglePass => "Reading package...",
        Mode::InMemory => "Reading package into memory...",
    });

    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    st.rx = Some(rx);
    st.cancel = cancel.clone();
    st.busy = Busy::Loading;
    refresh_controls(ui, &st);

    st.worker = Some(std::thread::spawn(move || {
        let mut events = WorkerEvents {
            report: progress_reporter(tx.clone()),
            tx: tx.clone(),
            // Two pass only holds previews in memory; when those don't fit, the rest
            // simply stays in the package. Nothing to ask.
            ask: settings.mode != Mode::TwoPass,
            thumb_px: u32::try_from(settings.preview_max_px).unwrap_or(u32::MAX),
            thumbs: Thumbs::new(),
        };
        // Temp files go next to the default output folder (without creating it yet),
        // so moving them into place later is a rename, not a copy.
        let temp_parent = out_dir.parent().unwrap_or(Path::new("."));
        let result = load(&package, temp_parent, policy, &mut events, &cancel);
        let result = result
            .map(|loaded| (loaded, events.thumbs))
            .map_err(|e| e.to_string());
        let _ = tx.send(Msg::Loaded(result));
    }));
}

/// What the loading thread does with the library's questions and reports.
struct WorkerEvents<R> {
    report: R,
    tx: Sender<Msg>,
    ask: bool,
    /// Longest side of the copies kept of images, or 0 to keep the files themselves.
    thumb_px: u32,
    thumbs: Thumbs,
}

impl<R: FnMut(Progress)> LoadEvents for WorkerEvents<R> {
    fn progress(&mut self, fraction: f64) {
        (self.report)(Progress::Fraction(fraction));
    }

    /// Hands the question to the UI thread and waits for the answer. If the window
    /// goes away in the meantime, so does the reply channel.
    fn low_memory(&mut self, low: LowMemory) -> OnLowMemory {
        if !self.ask {
            return OnLowMemory::StopLoading;
        }
        let (reply, answer) = mpsc::channel();
        let _ = self.tx.send(Msg::LowMemory { low, reply });
        answer.recv().unwrap_or(OnLowMemory::Cancel)
    }

    /// With a size limit set, an image is replaced by a reduced copy of itself: the
    /// copy is kept here, and the file is left for the library to drop from memory.
    fn keep_media(&mut self, guid: &str, media: Media, data: &[u8]) -> bool {
        if media != Media::Image || self.thumb_px == 0 {
            return true;
        }
        if let Ok(thumb) = Thumb::decode(data, self.thumb_px) {
            self.thumbs.insert(guid.to_owned(), Arc::new(thumb));
        }
        false
    }
}

fn start_extract(ui: Ui, state: &Rc<RefCell<State>>) {
    let out_text = ui.output_text.get_value();
    let selected = {
        let st = state.borrow();
        if st.loaded.is_none() {
            return;
        }
        st.tree.selected_guids(&st.entries)
    };
    // Dialogs run a nested event loop in which the timer still fires, so `state` must
    // not be borrowed while one is open.
    let problem = if out_text.trim().is_empty() {
        Some("Choose an output folder first.")
    } else if selected.is_empty() {
        Some("Nothing is selected.")
    } else {
        None
    };
    if let Some(problem) = problem {
        error_box(ui, problem);
        return;
    }
    let out_dir = PathBuf::from(out_text.trim());
    let mut st = state.borrow_mut();
    let Some(mut loaded) = st.loaded.take() else {
        return;
    };
    // The preview may be reading a temp file that is about to be moved.
    st.preview.show(None);

    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    st.rx = Some(rx);
    st.cancel = cancel.clone();
    st.busy = Busy::Extracting;
    ui.status.set_label("Extracting...");
    ui.gauge.set_value(0);
    refresh_controls(ui, &st);

    st.worker = Some(std::thread::spawn(move || {
        let mut report = progress_reporter(tx.clone());
        let result = loaded
            .extract(Some(&selected), &out_dir, &mut report, &cancel)
            .map_err(|e| e.to_string());
        let _ = tx.send(Msg::Extracted {
            out_dir,
            result,
            loaded,
        });
    }));
}

/// Forwards library progress to the UI thread, throttled so a package with tens of
/// thousands of tiny files doesn't flood the channel.
fn progress_reporter(tx: Sender<Msg>) -> impl FnMut(Progress) {
    let mut last_fraction = -1.0;
    let mut last_writing = std::time::Instant::now();
    move |p| match p {
        Progress::Fraction(fraction) if (fraction - last_fraction).abs() >= 0.002 => {
            last_fraction = fraction;
            let _ = tx.send(Msg::Progress {
                fraction,
                writing: None,
            });
        }
        Progress::Writing(path) if last_writing.elapsed().as_millis() >= 40 => {
            last_writing = std::time::Instant::now();
            let _ = tx.send(Msg::Progress {
                fraction: last_fraction.max(0.0),
                writing: Some(path.to_owned()),
            });
        }
        _ => {}
    }
}

/// Drains worker messages. Runs on the UI thread from the timer.
fn pump(ui: Ui, state: &Rc<RefCell<State>>) {
    // A dialog opened below runs its own event loop, in which the timer fires again;
    // `try_borrow` makes that nested call wait its turn instead of panicking.
    let (messages, preview) = match state.try_borrow() {
        Ok(st) => (
            st.rx
                .as_ref()
                .map(|rx| rx.try_iter().collect::<Vec<Msg>>())
                .unwrap_or_default(),
            st.preview.clone(),
        ),
        Err(_) => return,
    };
    preview.pump();
    for msg in messages {
        match msg {
            Msg::Progress { fraction, writing } => {
                ui.gauge.set_value((fraction * GAUGE_RANGE as f64) as i32);
                if let Some(path) = writing {
                    ui.status.set_label(&path);
                }
            }
            Msg::LowMemory { low, reply } => {
                ui.status.set_label("Paused: low on memory.");
                let _ = reply.send(low_memory_dialog(ui, low));
                ui.status.set_label("Reading package...");
            }
            Msg::Loaded(result) => finish_load(ui, state, result),
            Msg::Extracted {
                out_dir,
                result,
                loaded,
            } => finish_extract(ui, state, out_dir, result, loaded),
        }
    }
}

/// Asks how to go on when in-memory loading runs short of memory.
fn low_memory_dialog(ui: Ui, low: LowMemory) -> OnLowMemory {
    const MARGIN: i32 = 12;
    let dialog = Dialog::builder(&ui.frame, "Low on memory").build();
    let situation = match low.available {
        Some(available) => format!(
            "The next file needs {} and only {} of memory is free.",
            format_size(low.needed),
            format_size(available)
        ),
        None => format!(
            "The next file needs {}, which could not be allocated.",
            format_size(low.needed)
        ),
    };
    let paragraphs = [
        format!(
            "Loading is paused: this PC is running low on memory. {} of the package is \
             in memory so far. {situation}",
            format_size(low.in_memory)
        ),
        "Use temp folder: write what is in memory to a temp folder next to the package \
         and load the rest there too. Previews stay instant."
            .to_owned(),
        "Switch to two pass: keep what is in memory, but leave the rest in the package. \
         Extracting then reads the package a second time, and previewing a file that \
         isn't loaded has to search the package for it."
            .to_owned(),
    ];
    let root = BoxSizer::builder(Orientation::Vertical).build();
    for paragraph in &paragraphs {
        let text = StaticText::builder(&dialog).with_label(paragraph).build();
        text.wrap(dialog.from_dip_int(460));
        root.add(
            &text,
            0,
            SizerFlag::Left | SizerFlag::Right | SizerFlag::Top,
            MARGIN,
        );
    }
    root.add_spacer(MARGIN);
    let buttons = BoxSizer::builder(Orientation::Horizontal).build();
    buttons.add_stretch_spacer(1);
    for (i, (label, answer)) in [
        ("Use temp folder", OnLowMemory::UseTemp),
        ("Switch to two pass", OnLowMemory::StopLoading),
        ("Cancel loading", OnLowMemory::Cancel),
    ]
    .into_iter()
    .enumerate()
    {
        // Escape and the close box count as the last button.
        let id = if answer == OnLowMemory::Cancel {
            ID_CANCEL
        } else {
            ID_HIGHEST + 1 + i as Id
        };
        let button = Button::builder(&dialog)
            .with_id(id)
            .with_label(label)
            .build();
        button.on_click(move |_| dialog.end_modal(id));
        if i == 0 {
            button.set_default();
        }
        buttons.add(&button, 0, SizerFlag::Left, 6);
    }
    root.add_sizer(
        &buttons,
        0,
        SizerFlag::Expand | SizerFlag::Left | SizerFlag::Right | SizerFlag::Bottom,
        MARGIN,
    );
    dialog.set_sizer_and_fit(root, true);
    dialog.centre();
    let answer = match dialog.show_modal() {
        id if id == ID_HIGHEST + 1 => OnLowMemory::UseTemp,
        id if id == ID_HIGHEST + 2 => OnLowMemory::StopLoading,
        _ => OnLowMemory::Cancel,
    };
    dialog.destroy();
    answer
}

fn finish_load(ui: Ui, state: &Rc<RefCell<State>>, result: Result<(Loaded, Thumbs), String>) {
    let mut st = state.borrow_mut();
    st.busy = Busy::Idle;
    st.rx = None;
    st.worker = None;
    ui.gauge.set_value(0);
    // Dialogs run a nested event loop, so the borrow is released before showing one.
    match result {
        Ok((mut loaded, thumbs)) => {
            st.thumbs = thumbs;
            st.tree.set_entries(&loaded.entries);
            st.entries = loaded.entries.clone();
            let files = st.entries.iter().filter(|e| !e.is_folder()).count();
            let total: u64 = st.entries.iter().map(|e| e.size).sum();
            let (memory, temp) = (loaded.memory_bytes(), loaded.temp_bytes());
            ui.status.set_label(&if memory + temp == 0 {
                format!("Loaded the list of {files} files; contents are read on extract.")
            } else if memory + temp < total {
                format!(
                    "Loaded {files} files ({} in memory, {} in temp files, {} not loaded).",
                    format_size(memory),
                    format_size(temp),
                    format_size(total - memory - temp)
                )
            } else {
                format!(
                    "Loaded {files} files ({} in memory, {} in temp files).",
                    format_size(memory),
                    format_size(temp)
                )
            });
            let rejected = std::mem::take(&mut loaded.rejected);
            let name = loaded.package().file_name().unwrap_or_default();
            let icon = loaded.package_icon.as_ref().and_then(|bytes| {
                let thumb = Thumb::decode(bytes, preview::IMAGE_MAX).ok()?;
                Some(Arc::new(thumb))
            });
            st.preview
                .set_package(Some((name.to_string_lossy().into_owned(), icon)));
            st.loaded = Some(loaded);
            refresh_controls(ui, &st);
            drop(st);
            warn_rejected(ui, &rejected);
        }
        Err(_) if st.cancel.load(Ordering::Relaxed) => {
            ui.status.set_label("Cancelled.");
            refresh_controls(ui, &st);
        }
        Err(e) if e == unity_unpacker::Error::Cancelled.to_string() => {
            // Cancelled from the low-memory prompt rather than the Cancel button.
            ui.status.set_label("Cancelled.");
            refresh_controls(ui, &st);
        }
        Err(e) => {
            ui.status.set_label("Could not read package.");
            refresh_controls(ui, &st);
            drop(st);
            error_box(ui, &format!("Could not read the package:\n\n{e}"));
        }
    }
}

fn finish_extract(
    ui: Ui,
    state: &Rc<RefCell<State>>,
    out_dir: PathBuf,
    result: Result<Stats, String>,
    loaded: Loaded,
) {
    let mut st = state.borrow_mut();
    st.busy = Busy::Idle;
    st.rx = None;
    st.worker = None;
    st.loaded = Some(loaded);
    let error = match result {
        Ok(stats) => {
            ui.gauge.set_value(GAUGE_RANGE);
            ui.status.set_label(&format!(
                "Extracted {} files ({}) to {}",
                stats.files,
                format_size(stats.bytes),
                out_dir.display()
            ));
            None
        }
        Err(_) if st.cancel.load(Ordering::Relaxed) => {
            ui.gauge.set_value(0);
            ui.status
                .set_label("Cancelled. Files already written were kept.");
            None
        }
        Err(e) => {
            ui.gauge.set_value(0);
            ui.status.set_label("Extraction failed.");
            Some(e)
        }
    };
    refresh_controls(ui, &st);
    drop(st);
    if let Some(e) = error {
        error_box(ui, &format!("Extraction failed:\n\n{e}"));
    }
}

// ---------------------------------------------------------------------------
// Settings

/// Label and description of each mode, in `Mode::ALL` order.
const MODE_TEXT: [(&str, &str); 3] = [
    (
        "Two pass",
        "Reads the file list first, then reads the package a second time to extract. \
         Uses the least disk space and memory.",
    ),
    (
        "Single pass",
        "Reads the package once. Small files wait in memory and large ones in a temp \
         folder until you extract; whatever you leave unticked is then deleted.",
    ),
    (
        "In memory",
        "Reads the package once and keeps all of it in RAM until you extract. If the \
         PC runs low on memory, loading pauses and asks how to continue.",
    ),
];

/// Shows the settings dialog and saves the result. On first run the defaults are saved
/// even if the dialog is dismissed, so it is only shown unprompted once.
fn edit_settings(ui: Ui, state: &Rc<RefCell<State>>, first_run: bool) {
    let (current, icons) = {
        let st = state.borrow();
        (st.settings, st.icons.clone())
    };
    let chosen = settings_dialog(ui, &icons, current, first_run);
    let Some(settings) = chosen.or(first_run.then_some(current)) else {
        return;
    };
    state.borrow_mut().settings = settings;
    if let Err(e) = settings.save() {
        error_box(ui, &format!("Could not save the settings:\n\n{e}"));
    }
}

fn settings_dialog(ui: Ui, icons: &Icons, current: Settings, first_run: bool) -> Option<Settings> {
    const MARGIN: i32 = 12;
    let dialog = Dialog::builder(&ui.frame, "Settings").build();
    let text_width = dialog.from_dip_int(440);
    let indent = dialog.from_dip_int(22);
    let label = |text: &str| StaticText::builder(&dialog).with_label(text).build();
    let wrapped = |text: &str| {
        let t = label(text);
        t.wrap(text_width);
        t
    };
    let center = SizerFlag::AlignCenterVertical;
    let sides = SizerFlag::Left | SizerFlag::Right;
    let root = BoxSizer::builder(Orientation::Vertical).build();
    // Adds `sizer` below the last row, indented to line up with the radio labels.
    let add_indented = |sizer: &dyn Fn(&BoxSizer)| {
        let row = BoxSizer::builder(Orientation::Horizontal).build();
        row.add_spacer(indent);
        sizer(&row);
        root.add_sizer(&row, 0, sides | SizerFlag::Top, MARGIN / 2);
    };

    if first_run {
        let intro = wrapped(
            "Welcome! Choose how packages are read. You can change this later with the \
             Settings button.",
        );
        root.add(&intro, 0, sides | SizerFlag::Top, MARGIN);
    }

    // Limits that only single pass uses.
    let spin = |value: u64, max: i32| {
        SpinCtrl::builder(&dialog)
            .with_range(0, max)
            .with_initial_value(value.min(max as u64) as i32)
            .build()
    };
    let memory_limit = spin(current.memory_limit_mb, 1_048_576);
    let small_file = spin(current.small_file_mb, 1_048_576);
    let limit_labels = [
        label("Keep up to"),
        label("MB of files in memory"),
        label("Files larger than"),
        label("MB go straight to the temp folder"),
    ];
    // Two pass: optionally keep what can be previewed at hand.
    let preload = CheckBox::builder(&dialog)
        .with_label("Also load images and sounds into memory, so their previews are instant")
        .build();
    preload.set_value(current.preload_previews);
    let thumb_px = spin(current.preview_max_px, 16_384);
    let thumb_labels = [
        label("Keep images as copies of at most"),
        label("px (0 keeps the original files)"),
    ];
    let thumb_row = BoxSizer::builder(Orientation::Horizontal).build();
    thumb_row.add(&thumb_labels[0], 0, center | SizerFlag::Right, 8);
    thumb_row.add(&thumb_px, 0, center | SizerFlag::Right, 8);
    thumb_row.add(&thumb_labels[1], 0, center, 0);

    // What makes in-memory loading pause and ask.
    let reserve = spin(current.memory_reserve_percent, 100);
    let reserve_labels = [
        label("Pause and ask when less than"),
        label("% of physical memory would be left free"),
    ];
    let reserve_row = BoxSizer::builder(Orientation::Horizontal).build();
    reserve_row.add(&reserve_labels[0], 0, center | SizerFlag::Right, 8);
    reserve_row.add(&reserve, 0, center | SizerFlag::Right, 8);
    reserve_row.add(&reserve_labels[1], 0, center, 0);

    let limits = FlexGridSizer::builder(2, 3)
        .with_hgap(8)
        .with_vgap(6)
        .build();
    limits.add(&limit_labels[0], 0, center, 0);
    limits.add(&memory_limit, 0, center, 0);
    limits.add(&limit_labels[1], 0, center, 0);
    limits.add(&limit_labels[2], 0, center, 0);
    limits.add(&small_file, 0, center, 0);
    limits.add(&limit_labels[3], 0, center, 0);

    let mut radios = Vec::new();
    for (i, (mode, (name, description))) in Mode::ALL.into_iter().zip(MODE_TEXT).enumerate() {
        let builder = RadioButton::builder(&dialog).with_label(name);
        let radio = if i == 0 {
            builder.first_in_group().build()
        } else {
            builder.build()
        };
        radio.set_value(mode == current.mode);
        root.add(&radio, 0, sides | SizerFlag::Top, MARGIN);
        let description = wrapped(description);
        add_indented(&|row| {
            row.add(&description, 0, SizerFlag::empty(), 0);
        });
        if mode == Mode::TwoPass {
            add_indented(&|row| {
                row.add(&preload, 0, SizerFlag::empty(), 0);
            });
            add_indented(&|row| {
                row.add_sizer(&thumb_row, 0, SizerFlag::empty(), 0);
            });
        }
        if mode == Mode::SinglePass {
            add_indented(&|row| {
                row.add_sizer(&limits, 0, SizerFlag::empty(), 0);
            });
        }
        if mode == Mode::InMemory {
            add_indented(&|row| {
                row.add_sizer(&reserve_row, 0, SizerFlag::empty(), 0);
            });
        }
        radios.push((mode, radio));
    }

    let dark_mode = CheckBox::builder(&dialog)
        .with_label("Dark mode (applies the next time the app starts)")
        .build();
    dark_mode.set_value(current.dark_mode);
    root.add(&dark_mode, 0, sides | SizerFlag::Top, MARGIN);

    let note = wrapped(&match Settings::path() {
        Some(path) => format!(
            "Changes apply to the next package you open.\nSaved to {}",
            path.display()
        ),
        None => "Changes apply to the next package you open.".to_owned(),
    });
    let note_row = BoxSizer::builder(Orientation::Horizontal).build();
    if let Some(help) = icons.get(Icon::Help) {
        let bitmap = StaticBitmap::new_with_bitmap(&dialog, ID_ANY as Id, help);
        note_row.add(&bitmap, 0, SizerFlag::Right, indent - icons.size);
    }
    note_row.add(&note, 0, SizerFlag::empty(), 0);
    root.add_sizer(&note_row, 0, SizerFlag::All, MARGIN);

    let ok = Button::builder(&dialog)
        .with_id(ID_OK)
        .with_label("OK")
        .build();
    let cancel = Button::builder(&dialog)
        .with_id(ID_CANCEL)
        .with_label("Cancel")
        .build();
    let buttons = StdDialogButtonSizerBuilder::new().build();
    buttons.add_button(&ok);
    buttons.add_button(&cancel);
    buttons.realize();
    root.add_sizer(
        &buttons,
        0,
        SizerFlag::Expand | sides | SizerFlag::Bottom,
        MARGIN,
    );
    ok.set_default();

    let (two_pass, single_pass, in_memory) = (radios[0].1, radios[1].1, radios[2].1);
    let enable_limits = move || {
        let enabled = two_pass.get_value();
        preload.enable(enabled);
        let enabled = enabled && preload.get_value();
        thumb_px.enable(enabled);
        for l in &thumb_labels {
            l.enable(enabled);
        }
        let enabled = single_pass.get_value();
        memory_limit.enable(enabled);
        small_file.enable(enabled);
        for l in &limit_labels {
            l.enable(enabled);
        }
        let enabled = in_memory.get_value();
        reserve.enable(enabled);
        for l in &reserve_labels {
            l.enable(enabled);
        }
    };
    enable_limits();
    for (_, radio) in &radios {
        radio.on_selected(move |_| enable_limits());
    }
    preload.on_toggled(move |_| enable_limits());

    dialog.set_sizer_and_fit(root, true);
    dialog.centre();
    let accepted = dialog.show_modal() == ID_OK;
    let settings = Settings {
        mode: radios
            .iter()
            .find(|(_, radio)| radio.get_value())
            .map_or(current.mode, |(mode, _)| *mode),
        memory_limit_mb: memory_limit.value().max(0) as u64,
        small_file_mb: small_file.value().max(0) as u64,
        memory_reserve_percent: reserve.value().clamp(0, 100) as u64,
        preload_previews: preload.get_value(),
        preview_max_px: thumb_px.value().max(0) as u64,
        dark_mode: dark_mode.get_value(),
    };
    dialog.destroy();
    accepted.then_some(settings)
}

fn update_summary(ui: Ui, st: &State) {
    if st.entries.is_empty() {
        ui.summary.set_label("");
        return;
    }
    let summary = st.tree.summary(&st.entries);
    ui.summary.set_label(&format!(
        "{} of {} files selected ({})",
        summary.ticked_files,
        summary.files,
        format_size(summary.ticked_bytes)
    ));
}

// ---------------------------------------------------------------------------
// Helpers

fn refresh_controls(ui: Ui, st: &State) {
    let idle = st.busy == Busy::Idle;
    let loaded = !st.entries.is_empty();
    for w in [
        &ui.package_text as &dyn WxWidget,
        &ui.package_browse,
        &ui.output_text,
        &ui.output_browse,
        &ui.settings,
    ] {
        WindowRef(w.handle_ptr()).enable(idle);
    }
    for w in [
        &ui.tree as &dyn WxWidget,
        &ui.select_all,
        &ui.select_none,
        &ui.expand_all,
    ] {
        WindowRef(w.handle_ptr()).enable(idle && loaded);
    }
    ui.extract
        .set_label(if idle { "Extract" } else { "Cancel" });
    ui.extract.enable(!idle || loaded);
    ui.open_folder
        .enable(idle && Path::new(&ui.output_text.get_value()).is_dir());
    update_summary(ui, st);
    ui.frame.layout();
}

/// Sibling folder named after the package, e.g. `Downloads/Foo.unitypackage` -> `Downloads/Foo`.
fn default_output(package: &Path) -> PathBuf {
    let stem = package.file_stem().unwrap_or(package.as_os_str());
    package.parent().unwrap_or(Path::new(".")).join(stem)
}

fn open_in_file_manager(dir: &Path) {
    let _ = open_path(dir);
}

/// Opens a folder in the file manager, or a file in the program registered for it.
fn open_path(path: &Path) -> std::io::Result<()> {
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program)
        .arg(path)
        .spawn()
        .map(drop)
}

/// Warns about package entries whose paths would have escaped the output folder.
fn warn_rejected(ui: Ui, rejected: &[String]) {
    if rejected.is_empty() {
        return;
    }
    let shown: Vec<_> = rejected.iter().take(10).map(String::as_str).collect();
    let more = rejected.len() - shown.len();
    let mut msg = format!(
        "{} entries were skipped because their paths would land outside the output folder:\n\n{}",
        rejected.len(),
        shown.join("\n")
    );
    if more > 0 {
        msg.push_str(&format!("\n... and {more} more"));
    }
    message_box(
        ui,
        &msg,
        "Unsafe paths skipped",
        MessageDialogStyle::IconWarning,
    );
}

fn message_box(ui: Ui, message: &str, caption: &str, icon: MessageDialogStyle) {
    MessageDialog::builder(&ui.frame, message, caption)
        .with_style(MessageDialogStyle::OK | icon)
        .build()
        .show_modal();
}

fn error_box(ui: Ui, message: &str) {
    message_box(
        ui,
        message,
        "Unity Package Unpacker",
        MessageDialogStyle::IconError,
    );
}
