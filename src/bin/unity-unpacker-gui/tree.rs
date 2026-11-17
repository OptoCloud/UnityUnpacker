//! The package contents tree: a checkbox column, a name column with a file-type icon,
//! and a size column.
//!
//! Built on `DataViewCtrl` with a custom model. Ticking a folder ticks everything in
//! it; a folder shows as ticked when all of its contents are, and its name is dimmed
//! when none are.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;

use unity_unpacker::{Entry, format_size};
use wxdragon::dc::DeviceContext;
use wxdragon::prelude::*;
use wxdragon::widgets::dataview::{
    CustomDataViewTreeModel, DataViewAlign, DataViewCellMode, DataViewColumn, DataViewColumnFlags,
    DataViewCtrl, DataViewCustomRenderer, DataViewEventHandler, DataViewItem, DataViewStyle,
    DataViewToggleRenderer, Variant, VariantType,
};

use crate::icons::{Icon, Icons};

const COL_CHECK: u32 = 0;
const COL_NAME: u32 = 1;
const COL_SIZE: u32 = 2;
/// `wxDATAVIEW_CELL_SELECTED`
const CELL_SELECTED: i32 = 1;

/// One row. Nodes are heap-allocated and referred to by raw pointer because that is
/// how wxWidgets names the rows of a custom model; [`Data`] owns them all.
struct Node {
    parent: *mut Node,
    children: Vec<*mut Node>,
    name: String,
    /// Index into the package's entries. Folders the package only implies have none.
    entry: Option<usize>,
    is_file: bool,
    icon: Icon,
    /// Payload bytes of this row and everything beneath it.
    size: u64,
    /// Rows without children at or beneath this one (1 for a file or an empty folder),
    /// and how many of them are ticked.
    leaves: u32,
    ticked: Cell<u32>,
}

impl Node {
    fn set_ticked(&self, ticked: bool) -> u32 {
        let count = if self.children.is_empty() {
            u32::from(ticked)
        } else {
            // SAFETY: child pointers are live for as long as their parent is.
            (self.children.iter())
                .map(|&c| unsafe { &*c }.set_ticked(ticked))
                .sum()
        };
        self.ticked.set(count);
        count
    }
}

/// The rows currently shown. Shared between the model's callbacks and [`Tree`].
#[derive(Default)]
struct Data {
    /// Every node, parents before their children. Each was made by `Box::into_raw`.
    nodes: RefCell<Vec<*mut Node>>,
    roots: RefCell<Vec<*mut Node>>,
    on_change: RefCell<Option<Rc<dyn Fn()>>>,
}

impl Data {
    fn each(&self, mut f: impl FnMut(&Node)) {
        for &node in self.nodes.borrow().iter() {
            // SAFETY: `nodes` only holds live nodes.
            f(unsafe { &*node });
        }
    }

    fn free(&self) {
        self.roots.borrow_mut().clear();
        for node in self.nodes.borrow_mut().drain(..) {
            // SAFETY: made by `Box::into_raw` in `build` and dropped exactly once here.
            drop(unsafe { Box::from_raw(node) });
        }
    }

    /// Replaces the rows with the tree `entries` describe: folders first, then files,
    /// sorted case-insensitively, everything ticked.
    fn build(&self, entries: &[Entry]) {
        #[derive(Default)]
        struct Draft {
            entry: Option<usize>,
            children: BTreeMap<String, Draft>,
        }

        fn is_folder(draft: &Draft, entries: &[Entry]) -> bool {
            !draft.children.is_empty() || draft.entry.is_none_or(|i| entries[i].is_folder())
        }

        fn realize(
            name: &str,
            draft: &Draft,
            parent: *mut Node,
            entries: &[Entry],
            nodes: &mut Vec<*mut Node>,
        ) -> *mut Node {
            let folder = is_folder(draft, entries);
            let node = Box::into_raw(Box::new(Node {
                parent,
                children: Vec::new(),
                name: name.to_owned(),
                entry: draft.entry,
                is_file: !folder,
                icon: if folder {
                    Icon::Folder
                } else {
                    Icon::for_file(name)
                },
                size: draft.entry.map_or(0, |i| entries[i].size),
                leaves: 0,
                ticked: Cell::new(0),
            }));
            nodes.push(node);

            let mut kids: Vec<(&String, &Draft)> = draft.children.iter().collect();
            kids.sort_by_cached_key(|(name, d)| (!is_folder(d, entries), name.to_lowercase()));
            let children: Vec<*mut Node> = kids
                .into_iter()
                .map(|(name, d)| realize(name, d, node, entries, nodes))
                .collect();

            // SAFETY: `node` was just allocated and nothing else refers to it yet; the
            // children are live.
            unsafe {
                let below = children.iter().map(|&c| (&*c).size).sum::<u64>();
                let leaves = children.iter().map(|&c| (&*c).leaves).sum::<u32>();
                (*node).size += below;
                (*node).leaves = if children.is_empty() { 1 } else { leaves };
                (*node).ticked.set((*node).leaves);
                (*node).children = children;
            }
            node
        }

        let mut root = Draft::default();
        for (i, e) in entries.iter().enumerate() {
            let mut draft = &mut root;
            for part in e.path.split('/') {
                draft = draft.children.entry(part.to_owned()).or_default();
            }
            draft.entry = Some(i);
        }

        self.free();
        let mut nodes = Vec::with_capacity(entries.len());
        let mut top: Vec<(&String, &Draft)> = root.children.iter().collect();
        top.sort_by_cached_key(|(name, d)| (!is_folder(d, entries), name.to_lowercase()));
        let roots = top
            .into_iter()
            .map(|(name, d)| realize(name, d, std::ptr::null_mut(), entries, &mut nodes))
            .collect();
        *self.nodes.borrow_mut() = nodes;
        *self.roots.borrow_mut() = roots;
    }

    fn tick(&self, node: &Node, ticked: bool) {
        node.set_ticked(ticked);
        let mut parent = node.parent;
        // SAFETY: parent pointers are live for as long as their children are.
        while let Some(p) = unsafe { parent.as_ref() } {
            let sum = p
                .children
                .iter()
                .map(|&c| unsafe { &*c }.ticked.get())
                .sum();
            p.ticked.set(sum);
            parent = p.parent;
        }
    }
}

impl Drop for Data {
    fn drop(&mut self) {
        self.free();
    }
}

pub struct Summary {
    pub files: usize,
    pub ticked_files: usize,
    pub ticked_bytes: u64,
}

pub struct Tree {
    pub ctrl: DataViewCtrl,
    model: CustomDataViewTreeModel,
    data: Rc<Data>,
}

impl Tree {
    pub fn new(parent: &dyn WxWidget, icons: Rc<Icons>) -> Tree {
        let data = Rc::new(Data::default());
        // SAFETY: every pointer the callbacks hand out comes from `Data`, which keeps
        // the nodes alive until `set_entries` swaps them and reports `cleared()` before
        // returning to the event loop.
        let model = unsafe {
            CustomDataViewTreeModel::new(
                data.clone(),
                |_: &Rc<Data>, node: Option<&Node>| node.map(|n| n.parent).filter(|p| !p.is_null()),
                |_: &Rc<Data>, node: Option<&Node>| node.is_none_or(|n| !n.children.is_empty()),
                |data: &Rc<Data>, node: Option<&Node>| match node {
                    Some(n) => n.children.clone(),
                    None => data.roots.borrow().clone(),
                },
                |_: &Rc<Data>, node: Option<&Node>, col: u32| match (node, col) {
                    (Some(n), COL_CHECK) => Variant::from_bool(n.ticked.get() == n.leaves),
                    (Some(n), COL_NAME) => {
                        // "<icon>\t<dimmed>\t<name>", unpacked again by the renderer.
                        let dimmed = u8::from(n.ticked.get() == 0);
                        Variant::from_string(format!("{}\t{dimmed}\t{}", n.icon as usize, n.name))
                    }
                    (Some(n), COL_SIZE) => Variant::from_string(format_size(n.size)),
                    _ => Variant::new(),
                },
                Some(
                    |data: &Rc<Data>, node: Option<&Node>, col: u32, value: &Variant| {
                        let (Some(node), COL_CHECK, Some(ticked)) = (node, col, value.get_bool())
                        else {
                            return false;
                        };
                        data.tick(node, ticked);
                        let on_change = data.on_change.borrow().clone();
                        if let Some(on_change) = on_change {
                            on_change();
                        }
                        true
                    },
                ),
                None::<fn(&Rc<Data>, Option<&Node>, u32) -> bool>,
                None::<fn(&Rc<Data>, &Node, &Node, u32, bool) -> i32>,
            )
        };
        // Folders show their tick box and size too, not just a name.
        model.set_container_columns(true);

        let ctrl = DataViewCtrl::builder(parent)
            .with_style(DataViewStyle::Single)
            .build();
        ctrl.associate_model(&model);

        let dip = |v: i32| ctrl.from_dip_int(v);
        let none = DataViewColumnFlags::DefaultNone;
        ctrl.append_column(DataViewColumn::new(
            "",
            DataViewToggleRenderer::new(
                VariantType::Bool,
                DataViewCellMode::Activatable,
                DataViewAlign::Center,
            ),
            COL_CHECK as usize,
            dip(36),
            DataViewAlign::Center,
            none,
        ));
        ctrl.append_column(DataViewColumn::new(
            "Name",
            name_renderer(icons),
            COL_NAME as usize,
            dip(480),
            DataViewAlign::Left,
            DataViewColumnFlags::Resizable,
        ));
        ctrl.append_column(DataViewColumn::new(
            "Size",
            wxdragon::widgets::dataview::DataViewTextRenderer::new(
                VariantType::String,
                DataViewCellMode::Inert,
                DataViewAlign::Right,
            ),
            COL_SIZE as usize,
            dip(100),
            DataViewAlign::Right,
            DataViewColumnFlags::Resizable,
        ));
        // Indent the names, not the tick boxes, so the boxes line up in a gutter.
        if let Some(name_column) = ctrl.get_column(COL_NAME as usize) {
            ctrl.set_expander_column(&name_column);
        }

        Tree { ctrl, model, data }
    }

    /// Called when the selected row changes, with the index of the entry it stands
    /// for if that is a file (folders and no selection give `None`).
    pub fn on_select(&self, callback: impl Fn(Option<usize>) + 'static) {
        let ctrl = self.ctrl;
        ctrl.on_selection_changed(move |_| {
            let node = ctrl.get_selection().and_then(|item| item.get_id::<Node>());
            // SAFETY: a selected item names a live node of the current contents.
            let node = node.and_then(|ptr| unsafe { ptr.as_ref() });
            callback(node.filter(|n| n.is_file).and_then(|n| n.entry));
        });
    }

    /// Called after the user ticks or unticks a row.
    pub fn on_change(&self, callback: impl Fn() + 'static) {
        *self.data.on_change.borrow_mut() = Some(Rc::new(callback));
    }

    /// Shows `entries` (all ticked), or nothing if empty. Expands the top two levels,
    /// which are usually "Assets" and the package's own folder.
    pub fn set_entries(&self, entries: &[Entry]) {
        self.data.build(entries);
        self.model.cleared();
        for &root in self.data.roots.borrow().iter() {
            self.expand(root);
            // SAFETY: just built.
            if let [only] = unsafe { &*root }.children[..] {
                self.expand(only);
            }
        }
    }

    fn expand(&self, node: *mut Node) {
        // SAFETY: callers pass live nodes of the current model contents.
        self.ctrl
            .expand(&unsafe { DataViewItem::from_id_ptr(node) });
    }

    pub fn expand_all(&self) {
        let nodes = self.data.nodes.borrow().clone();
        for node in nodes {
            // SAFETY: `nodes` only holds live nodes.
            if !unsafe { &*node }.children.is_empty() {
                self.expand(node);
            }
        }
    }

    pub fn tick_all(&self, ticked: bool) {
        for &root in self.data.roots.borrow().iter() {
            // SAFETY: roots are live.
            unsafe { &*root }.set_ticked(ticked);
        }
        self.redraw();
    }

    /// Repaints the rows; cell values are read from the model as they are drawn.
    pub fn redraw(&self) {
        self.ctrl.refresh(false, None);
    }

    /// GUIDs to extract. Files must be ticked. Folders count if anything beneath them
    /// is, so their `.meta` comes along with the selected contents.
    pub fn selected_guids(&self, entries: &[Entry]) -> HashSet<String> {
        let mut out = HashSet::new();
        self.data.each(|n| {
            if let Some(i) = n.entry
                && n.ticked.get() > 0
            {
                out.insert(entries[i].guid.clone());
            }
        });
        out
    }

    pub fn summary(&self, entries: &[Entry]) -> Summary {
        let mut s = Summary {
            files: 0,
            ticked_files: 0,
            ticked_bytes: 0,
        };
        self.data.each(|n| {
            if let (true, Some(i)) = (n.is_file, n.entry) {
                s.files += 1;
                if n.ticked.get() > 0 {
                    s.ticked_files += 1;
                    s.ticked_bytes += entries[i].size;
                }
            }
        });
        s
    }
}

/// Draws a row's icon followed by its name, dimmed if nothing in the row is ticked.
fn name_renderer(icons: Rc<Icons>) -> DataViewCustomRenderer {
    let gap = icons.size / 4 + 2;
    let width_icons = icons.clone();
    DataViewCustomRenderer::builder()
        .variant_type(VariantType::String)
        .mode(DataViewCellMode::Inert)
        .align(DataViewAlign::Left)
        .with_get_size(move |value, default| {
            let chars = value.get_string().map_or(0, |s| s.chars().count()) as i32;
            Size::new(
                width_icons.size + gap + chars * width_icons.size / 2,
                default.height.max(width_icons.size + 2),
            )
        })
        .with_render(move |rect, dc, state, value| {
            let text = value.get_string().unwrap_or_default();
            let mut parts = text.splitn(3, '\t');
            let icon = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let dimmed = parts.next() == Some("1");
            let name = parts.next().unwrap_or_default();

            if let Some(bitmap) = icons.get(Icon::from_index(icon)) {
                let y = rect.y + (rect.height - icons.size) / 2;
                dc.draw_bitmap(bitmap, rect.x, y, true);
            }
            dc.set_text_foreground(SystemSettings::get_colour(if state & CELL_SELECTED != 0 {
                SystemColour::ListBoxHighlightText
            } else if dimmed {
                SystemColour::GrayText
            } else {
                SystemColour::ListBoxText
            }));
            let (_, text_height) = dc.get_text_extent(name);
            let y = rect.y + (rect.height - text_height) / 2;
            dc.draw_text(name, rect.x + icons.size + gap, y);
            true
        })
        .build()
}
