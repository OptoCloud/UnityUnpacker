//! Native desktop front end (wxWidgets: Win32 on Windows, GTK on Linux).
//!
//! `unity-unpacker-gui [package.unitypackage]` opens the window, optionally preloading a
//! package (so it works as a file-association handler).

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};

use unity_unpacker::{Entry, Progress, ScanResult, Stats, extract, format_size, scan};
use wxdragon::prelude::*;
use wxdragon::timer::Timer;
use wxdragon::widgets::treelistctrl::{
    CheckboxState, TreeListCtrl, TreeListCtrlStyle, TreeListItem,
};

const GAUGE_RANGE: i32 = 1000;
const PACKAGE_WILDCARD: &str = "Unity packages (*.unitypackage)|*.unitypackage|All files (*.*)|*.*";

/// Messages from worker threads to the UI thread.
enum Msg {
    Progress {
        fraction: f64,
        writing: Option<String>,
    },
    Scanned {
        package: PathBuf,
        result: Result<ScanResult, String>,
    },
    Extracted {
        out_dir: PathBuf,
        result: Result<Stats, String>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Busy {
    Idle,
    Scanning,
    Extracting,
}

struct State {
    package: Option<PathBuf>,
    entries: Vec<Entry>,
    /// Tree item id -> index into `entries`. Synthesized folders have no entry.
    item_entry: HashMap<i64, usize>,
    busy: Busy,
    cancel: Arc<AtomicBool>,
    rx: Option<Receiver<Msg>>,
}

#[derive(Clone, Copy)]
struct Ui {
    frame: Frame,
    package_text: TextCtrl,
    package_browse: Button,
    output_text: TextCtrl,
    output_browse: Button,
    tree: TreeListCtrl,
    select_all: Button,
    select_none: Button,
    expand_all: Button,
    summary: StaticText,
    gauge: Gauge,
    status: StaticText,
    open_folder: Button,
    extract: Button,
}

fn main() {
    let initial = std::env::args_os().nth(1).map(PathBuf::from);
    let _ = wxdragon::main(move |_| {
        let ui = build_ui();
        let state = Rc::new(RefCell::new(State {
            package: None,
            entries: Vec::new(),
            item_entry: HashMap::new(),
            busy: Busy::Idle,
            cancel: Arc::new(AtomicBool::new(false)),
            rx: None,
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

        if let Some(path) = initial {
            start_scan(ui, &state, path);
        }
    });
}

fn build_ui() -> Ui {
    let frame = Frame::builder()
        .with_title("Unity Package Unpacker")
        .with_size(Size::new(760, 620))
        .build();
    let panel = Panel::builder(&frame).build();

    let label = |text: &str| StaticText::builder(&panel).with_label(text).build();
    let package_text = TextCtrl::builder(&panel).build();
    let package_browse = Button::builder(&panel).with_label("Browse...").build();
    let output_text = TextCtrl::builder(&panel).build();
    let output_browse = Button::builder(&panel).with_label("Browse...").build();

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

    let tree = TreeListCtrl::builder(&panel)
        .with_style(TreeListCtrlStyle::Checkbox | TreeListCtrlStyle::Three_State)
        .build();
    tree.append_column("Name", 520, ListColumnFormat::Left);
    tree.append_column("Size", 110, ListColumnFormat::Right);

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
    let open_folder = Button::builder(&panel)
        .with_label("Open output folder")
        .build();
    let extract = Button::builder(&panel).with_label("Extract").build();
    let action_row = BoxSizer::builder(Orientation::Horizontal).build();
    action_row.add(&status, 1, center | SizerFlag::Right, 12);
    action_row.add(&open_folder, 0, SizerFlag::Right, 6);
    action_row.add(&extract, 0, SizerFlag::empty(), 0);

    let root = BoxSizer::builder(Orientation::Vertical).build();
    root.add_sizer(&paths, 0, SizerFlag::Expand | SizerFlag::All, 12);
    root.add(
        &tree,
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
    frame.set_min_size(Size::new(560, 420));

    Ui {
        frame,
        package_text,
        package_browse,
        output_text,
        output_browse,
        tree,
        select_all,
        select_none,
        expand_all,
        summary,
        gauge,
        status,
        open_folder,
        extract,
    }
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
            start_scan(ui, &s, PathBuf::from(path));
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
            start_scan(ui, &s, PathBuf::from(text.trim()));
        }
    });

    let s = state.clone();
    ui.tree.on_item_checked(move |event| {
        if let Some(item) = event.get_item() {
            // Clicking cycles through checked/unchecked; propagate down and up.
            let checked = ui.tree.get_checked_state(&item);
            ui.tree.check_item_recursively(&item, checked);
            ui.tree.update_item_parent_state(&item);
        }
        update_summary(ui, &s.borrow());
    });

    let s = state.clone();
    ui.select_all
        .on_click(move |_| set_all(ui, &s.borrow(), CheckboxState::Checked));
    let s = state.clone();
    ui.select_none
        .on_click(move |_| set_all(ui, &s.borrow(), CheckboxState::Unchecked));
    ui.expand_all.on_click(move |_| {
        let mut item = ui.tree.get_first_item();
        while let Some(it) = item {
            ui.tree.expand(&it);
            item = ui.tree.get_next_item(&it);
        }
    });

    ui.open_folder
        .on_click(move |_| open_in_file_manager(Path::new(&ui.output_text.get_value())));

    let s = state.clone();
    ui.extract.on_click(move |_| {
        let busy = s.borrow().busy;
        match busy {
            Busy::Idle => start_extract(ui, &s),
            Busy::Scanning | Busy::Extracting => {
                s.borrow().cancel.store(true, Ordering::Relaxed);
                ui.status.set_label("Cancelling...");
            }
        }
    });

    // Drop a package anywhere on the window (or the tree) to open it.
    for target in [&ui.frame as &dyn WxWidget, &ui.tree as &dyn WxWidget] {
        let s = state.clone();
        FileDropTarget::builder(&WindowRef(target.handle_ptr()))
            .with_on_drop_files(move |files, _, _| {
                let Some(first) = files.into_iter().next() else {
                    return false;
                };
                if s.borrow().busy != Busy::Idle {
                    return false;
                }
                start_scan(ui, &s, PathBuf::from(first));
                true
            })
            .build();
    }
}

/// Lets a `&dyn WxWidget` be passed where a sized `W: WxWidget` is needed.
struct WindowRef(*mut wxdragon::ffi::wxd_Window_t);

impl WxWidget for WindowRef {
    fn handle_ptr(&self) -> *mut wxdragon::ffi::wxd_Window_t {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Background work

fn start_scan(ui: Ui, state: &Rc<RefCell<State>>, package: PathBuf) {
    let mut st = state.borrow_mut();
    if st.busy != Busy::Idle {
        return;
    }
    ui.package_text.set_value(&package.to_string_lossy());
    ui.output_text
        .set_value(&default_output(&package).to_string_lossy());
    ui.tree.delete_all_items();
    st.entries.clear();
    st.item_entry.clear();
    st.package = None;
    ui.status.set_label("Reading package...");
    ui.gauge.set_value(0);

    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    st.rx = Some(rx);
    st.cancel = cancel.clone();
    st.busy = Busy::Scanning;
    refresh_controls(ui, &st);
    drop(st);

    std::thread::spawn(move || {
        let mut report = progress_reporter(tx.clone());
        let result = scan(&package, &mut report, &cancel).map_err(|e| e.to_string());
        let _ = tx.send(Msg::Scanned { package, result });
    });
}

fn start_extract(ui: Ui, state: &Rc<RefCell<State>>) {
    let mut st = state.borrow_mut();
    let Some(package) = st.package.clone() else {
        return;
    };
    let out_text = ui.output_text.get_value();
    if out_text.trim().is_empty() {
        error_box(ui, "Choose an output folder first.");
        return;
    }
    let out_dir = PathBuf::from(out_text.trim());
    let selected = selected_guids(ui, &st);
    if selected.is_empty() {
        error_box(ui, "Nothing is selected.");
        return;
    }

    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    st.rx = Some(rx);
    st.cancel = cancel.clone();
    st.busy = Busy::Extracting;
    ui.status.set_label("Extracting...");
    ui.gauge.set_value(0);
    refresh_controls(ui, &st);
    let entries = st.entries.clone();
    drop(st);

    std::thread::spawn(move || {
        let mut report = progress_reporter(tx.clone());
        let result = extract(
            &package,
            &entries,
            Some(&selected),
            &out_dir,
            &mut report,
            &cancel,
        )
        .map_err(|e| e.to_string());
        let _ = tx.send(Msg::Extracted { out_dir, result });
    });
}

/// Forwards library progress to the UI thread, throttled so a package with tens of
/// thousands of tiny files doesn't flood the channel.
fn progress_reporter(tx: Sender<Msg>) -> impl FnMut(Progress) {
    let mut last_fraction = -1.0;
    let mut last_writing = std::time::Instant::now();
    move |p| match p {
        Progress::Bytes { read, total } if total > 0 => {
            let fraction = read as f64 / total as f64;
            if fraction - last_fraction >= 0.002 {
                last_fraction = fraction;
                let _ = tx.send(Msg::Progress {
                    fraction,
                    writing: None,
                });
            }
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
    let messages: Vec<Msg> = match &state.borrow().rx {
        Some(rx) => rx.try_iter().collect(),
        None => return,
    };
    for msg in messages {
        match msg {
            Msg::Progress { fraction, writing } => {
                ui.gauge.set_value((fraction * GAUGE_RANGE as f64) as i32);
                if let Some(path) = writing {
                    ui.status.set_label(&path);
                }
            }
            Msg::Scanned { package, result } => finish_scan(ui, state, package, result),
            Msg::Extracted { out_dir, result } => finish_extract(ui, state, out_dir, result),
        }
    }
}

fn finish_scan(
    ui: Ui,
    state: &Rc<RefCell<State>>,
    package: PathBuf,
    result: Result<ScanResult, String>,
) {
    let mut st = state.borrow_mut();
    st.busy = Busy::Idle;
    st.rx = None;
    ui.gauge.set_value(0);
    match result {
        Ok(scanned) => {
            let item_entry = populate_tree(ui.tree, &scanned.entries);
            st.entries = scanned.entries;
            st.item_entry = item_entry;
            st.package = Some(package);
            let files = st.entries.iter().filter(|e| !e.is_folder()).count();
            ui.status.set_label(&format!("Loaded {files} files."));
            if !scanned.rejected.is_empty() {
                let shown: Vec<_> = scanned
                    .rejected
                    .iter()
                    .take(10)
                    .map(String::as_str)
                    .collect();
                let more = scanned.rejected.len().saturating_sub(shown.len());
                let mut msg = format!(
                    "{} entries were skipped because their paths would land outside the output folder:\n\n{}",
                    scanned.rejected.len(),
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
        }
        Err(e) if st.cancel.load(Ordering::Relaxed) => {
            let _ = e;
            ui.status.set_label("Cancelled.");
        }
        Err(e) => {
            ui.status.set_label("Could not read package.");
            error_box(ui, &format!("Could not read the package:\n\n{e}"));
        }
    }
    refresh_controls(ui, &st);
}

fn finish_extract(
    ui: Ui,
    state: &Rc<RefCell<State>>,
    out_dir: PathBuf,
    result: Result<Stats, String>,
) {
    let mut st = state.borrow_mut();
    st.busy = Busy::Idle;
    st.rx = None;
    match result {
        Ok(stats) => {
            ui.gauge.set_value(GAUGE_RANGE);
            ui.status.set_label(&format!(
                "Extracted {} files ({}) to {}",
                stats.files,
                format_size(stats.bytes),
                out_dir.display()
            ));
        }
        Err(_) if st.cancel.load(Ordering::Relaxed) => {
            ui.gauge.set_value(0);
            ui.status
                .set_label("Cancelled. Files already written were kept.");
        }
        Err(e) => {
            ui.gauge.set_value(0);
            ui.status.set_label("Extraction failed.");
            error_box(ui, &format!("Extraction failed:\n\n{e}"));
        }
    }
    refresh_controls(ui, &st);
}

// ---------------------------------------------------------------------------
// Tree

#[derive(Default)]
struct Node {
    entry: Option<usize>,
    children: BTreeMap<String, Node>,
    size: u64,
}

impl Node {
    fn is_folder(&self, entries: &[Entry]) -> bool {
        !self.children.is_empty() || self.entry.is_none_or(|i| entries[i].is_folder())
    }
}

/// Fills the tree (folders first, then files, case-insensitively sorted), checks
/// everything, and returns the item-id -> entry-index map.
fn populate_tree(tree: TreeListCtrl, entries: &[Entry]) -> HashMap<i64, usize> {
    let mut root = Node::default();
    for (i, e) in entries.iter().enumerate() {
        let mut node = &mut root;
        for part in e.path.split('/') {
            node.size += e.size;
            node = node.children.entry(part.to_owned()).or_default();
        }
        node.size += e.size;
        node.entry = Some(i);
    }

    fn insert(
        tree: TreeListCtrl,
        parent: &TreeListItem,
        node: &Node,
        entries: &[Entry],
        map: &mut HashMap<i64, usize>,
    ) {
        let mut kids: Vec<(&String, &Node)> = node.children.iter().collect();
        kids.sort_by_cached_key(|(name, n)| (!n.is_folder(entries), name.to_lowercase()));
        for (name, child) in kids {
            let Some(item) = tree.append_item(parent, name) else {
                continue;
            };
            tree.set_item_text(&item, 1, &format_size(child.size));
            if let Some(i) = child.entry {
                map.insert(item.id(), i);
            }
            insert(tree, &item, child, entries, map);
        }
    }

    let mut map = HashMap::with_capacity(entries.len());
    tree.freeze();
    let tree_root = tree.get_root_item();
    insert(tree, &tree_root, &root, entries, &mut map);
    // Expand the top two levels (usually "Assets" and the package's own folder).
    let mut item = tree.get_first_child(&tree_root);
    while let Some(it) = item {
        tree.expand(&it);
        if let Some(only) = tree.get_first_child(&it)
            && tree.get_next_sibling(&only).is_none()
        {
            tree.expand(&only);
        }
        item = tree.get_next_sibling(&it);
    }
    tree.check_item_recursively(&tree_root, CheckboxState::Checked);
    tree.thaw();
    map
}

fn set_all(ui: Ui, st: &State, state: CheckboxState) {
    let mut item = ui.tree.get_first_child(&ui.tree.get_root_item());
    while let Some(it) = item {
        ui.tree.check_item_recursively(&it, state);
        item = ui.tree.get_next_sibling(&it);
    }
    update_summary(ui, st);
}

/// Files must be fully checked. Folders count if anything beneath them is, so their
/// `.meta` comes along with the selected contents.
fn selected_guids(ui: Ui, st: &State) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut item = ui.tree.get_first_item();
    while let Some(it) = item {
        if let Some(&i) = st.item_entry.get(&it.id()) {
            let entry = &st.entries[i];
            let take = match ui.tree.get_checked_state(&it) {
                CheckboxState::Checked => true,
                CheckboxState::Undetermined => entry.is_folder(),
                CheckboxState::Unchecked => false,
            };
            if take {
                out.insert(entry.guid.clone());
            }
        }
        item = ui.tree.get_next_item(&it);
    }
    out
}

fn update_summary(ui: Ui, st: &State) {
    if st.entries.is_empty() {
        ui.summary.set_label("");
        return;
    }
    let (mut total, mut chosen, mut bytes) = (0usize, 0usize, 0u64);
    let mut item = ui.tree.get_first_item();
    while let Some(it) = item {
        if let Some(&i) = st.item_entry.get(&it.id()) {
            let entry = &st.entries[i];
            if !entry.is_folder() {
                total += 1;
                if ui.tree.get_checked_state(&it) == CheckboxState::Checked {
                    chosen += 1;
                    bytes += entry.size;
                }
            }
        }
        item = ui.tree.get_next_item(&it);
    }
    ui.summary.set_label(&format!(
        "{chosen} of {total} files selected ({})",
        format_size(bytes)
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
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program).arg(dir).spawn();
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
