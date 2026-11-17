//! The preview pane: shows the selected file as an image, as text, or as audio that
//! can be played.
//!
//! Contents come from a [`Source`], which says where the file's bytes are right now
//! (memory, the temp folder, or still in the package). Reading and decoding happen on
//! a worker thread; a newer selection cancels the one before it.

use std::cell::RefCell;
use std::io::Cursor;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::Duration;

use image::RgbaImage;
use image::imageops::{self, FilterType};
use rodio::Source as _;
use unity_unpacker::{Error, Source, format_size};
use wxdragon::font::FontFamily;
use wxdragon::prelude::*;

/// Text previews show at most this much of a file.
const TEXT_LIMIT: u64 = 256 * 1024;
/// Images and audio larger than this are not read for a preview.
const MEDIA_LIMIT: u64 = 256 * 1024 * 1024;
/// Decoded images are kept at most this many pixels wide or tall; the pane scales
/// that copy to fit, so resizing the window doesn't decode again.
const IMAGE_MAX: u32 = 1024;
const MARGIN: i32 = 8;

/// Text files larger than this are not copied out for an external editor.
const EDITOR_LIMIT: u64 = 64 * 1024 * 1024;

/// What to preview.
pub struct Request {
    pub name: String,
    pub size: u64,
    pub source: Source,
    /// A copy of the image made while the package was loaded, if there is one.
    pub thumb: Option<Arc<Thumb>>,
}

/// A decoded image, shrunk to fit a size limit.
pub struct Thumb {
    pub picture: RgbaImage,
    /// Size of the original image.
    pub width: u32,
    pub height: u32,
}

impl Thumb {
    /// Decodes `bytes`, keeping at most `max_px` pixels on the longer side.
    pub fn decode(bytes: &[u8], max_px: u32) -> Result<Thumb, image::ImageError> {
        let decoded = image::load_from_memory(bytes)?;
        let (width, height) = (decoded.width(), decoded.height());
        let picture = if width.max(height) > max_px {
            decoded.thumbnail(max_px, max_px).into_rgba8()
        } else {
            decoded.into_rgba8()
        };
        Ok(Thumb {
            picture,
            width,
            height,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Image,
    Audio,
    /// Anything else is shown as text if that is what it turns out to be.
    Other,
}

fn kind_of(name: &str) -> Kind {
    let ext = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "bmp" | "gif" | "tga" | "tif" | "tiff" => Kind::Image,
        "wav" | "mp3" | "ogg" | "flac" | "aif" | "aiff" => Kind::Audio,
        _ => Kind::Other,
    }
}

/// Audio bytes shared between the pane and the decoder playing them.
#[derive(Clone)]
struct Bytes(Arc<Vec<u8>>);

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

enum Content {
    Image(Thumb),
    Text {
        text: String,
        truncated: bool,
    },
    Audio {
        bytes: Bytes,
        duration: Option<Duration>,
    },
    /// Nothing to show, and why.
    Nothing(String),
}

/// Runs on the worker thread.
fn produce(request: &Request, cancel: &AtomicBool) -> Result<Content, Error> {
    let kind = kind_of(&request.name);
    if kind != Kind::Other && request.size > MEDIA_LIMIT {
        return Ok(Content::Nothing(format!(
            "Too large to preview ({}).",
            format_size(request.size)
        )));
    }
    Ok(match kind {
        Kind::Image => {
            let bytes = request.source.read(MEDIA_LIMIT, cancel)?;
            match Thumb::decode(&bytes, IMAGE_MAX) {
                Ok(thumb) => Content::Image(thumb),
                Err(e) => Content::Nothing(format!("Could not decode this image: {e}")),
            }
        }
        Kind::Audio => {
            let bytes = Bytes(Arc::new(request.source.read(MEDIA_LIMIT, cancel)?));
            match rodio::Decoder::try_from(Cursor::new(bytes.clone())) {
                Ok(decoder) => Content::Audio {
                    duration: decoder.total_duration(),
                    bytes,
                },
                Err(e) => Content::Nothing(format!("Could not decode this audio file: {e}")),
            }
        }
        Kind::Other => {
            let mut bytes = request.source.read(TEXT_LIMIT + 1, cancel)?;
            let truncated = bytes.len() as u64 > TEXT_LIMIT;
            bytes.truncate(TEXT_LIMIT as usize);
            match decode_text(&bytes, truncated) {
                Some(text) => Content::Text { text, truncated },
                None => Content::Nothing("No preview for this type of file.".to_owned()),
            }
        }
    })
}

/// `bytes` as text, or `None` if it looks like binary data. A cut-off file may end in
/// the middle of a character, which is not held against it.
fn decode_text(bytes: &[u8], truncated: bool) -> Option<String> {
    if bytes.contains(&0) {
        return None;
    }
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(text.to_owned()),
        Err(e) if truncated && bytes.len() - e.valid_up_to() < 4 => {
            Some(String::from_utf8_lossy(&bytes[..e.valid_up_to()]).into_owned())
        }
        Err(_) => None,
    }
}

/// Where copies for external editors are written; one folder per running app.
fn copies_dir() -> PathBuf {
    std::env::temp_dir().join(format!("unity-unpacker-preview-{}", std::process::id()))
}

/// File name for the editor copy of an asset called `name`.
///
/// The copy is opened with whatever program is registered for its extension, and the
/// package is untrusted: a "text file" called `setup.bat` must not be run. So only
/// extensions known to open in an editor are kept; anything else gets `.txt` added.
fn editor_file_name(name: &str) -> String {
    const EDITOR_EXTENSIONS: [&str; 17] = [
        "cs", "txt", "json", "xml", "yaml", "yml", "md", "csv", "shader", "cginc", "hlsl",
        "compute", "uxml", "uss", "ini", "cfg", "log",
    ];
    let mut safe: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let ext = safe
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase());
    if !ext.is_some_and(|ext| EDITOR_EXTENSIONS.contains(&ext.as_str())) {
        safe.push_str(".txt");
    }
    safe
}

fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

struct Reply {
    generation: u64,
    name: String,
    size: u64,
    content: Result<Content, Error>,
}

#[derive(Default)]
struct State {
    /// Bumped for every request, so replies to superseded ones can be recognised.
    generation: u64,
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    rx: Option<Receiver<Reply>>,
    /// The file being shown, for opening it in an external editor.
    current: Option<(String, Source)>,
    /// Problems from background jobs, to show in the pane.
    notices: Option<(mpsc::Sender<String>, Receiver<String>)>,
    /// The image being shown, before scaling to the pane.
    picture: Option<RgbaImage>,
    audio: Option<Bytes>,
    /// Opened the first time something is played, then kept.
    output: Option<rodio::MixerDeviceSink>,
    player: Option<rodio::Player>,
}

#[derive(Clone, Copy)]
struct Widgets {
    panel: Panel,
    title: StaticText,
    info: StaticText,
    message: StaticText,
    canvas: Panel,
    image: StaticBitmap,
    text: TextCtrl,
    audio: Panel,
    open: Button,
}

pub struct Preview {
    w: Widgets,
    state: RefCell<State>,
}

impl Preview {
    pub fn new(parent: &dyn WxWidget) -> Rc<Preview> {
        let panel = Panel::builder(parent).build();
        let label = |text: &str| StaticText::builder(&panel).with_label(text).build();
        let title = label("");
        if let Some(font) = title.get_font() {
            let mut bold = font;
            bold.make_bold();
            title.set_font(&bold);
        }
        let info = label("");
        let message = label("");

        // The image sits centred in a canvas that takes all the spare room; the canvas
        // size is what the image is scaled to.
        let canvas = Panel::builder(&panel).build();
        // A StaticBitmap must be created with a bitmap; start with an invisible one.
        let blank = Bitmap::from_rgba(&[0; 4], 1, 1).expect("1x1 bitmap");
        let image = StaticBitmap::new_with_bitmap(&canvas, ID_ANY as Id, &blank);
        let canvas_sizer = BoxSizer::builder(Orientation::Vertical).build();
        canvas_sizer.add_stretch_spacer(1);
        canvas_sizer.add(&image, 0, SizerFlag::AlignCenterHorizontal, 0);
        canvas_sizer.add_stretch_spacer(1);
        canvas.set_sizer(canvas_sizer, true);

        let text = TextCtrl::builder(&panel)
            .with_style(
                TextCtrlStyle::MultiLine | TextCtrlStyle::ReadOnly | TextCtrlStyle::DontWrap,
            )
            .build();
        if let Some(font) = Font::builder()
            .with_family(FontFamily::Teletype)
            .with_point_size(9)
            .build()
        {
            text.set_font(&font);
        }

        let open = Button::builder(&panel)
            .with_label("Open copy in editor")
            .build();
        open.set_tooltip(
            "Saves a copy of this file to the temp folder and opens it in the program \
             registered for it. Changes to the copy are not written anywhere else.",
        );

        let audio = Panel::builder(&panel).build();
        let play = Button::builder(&audio).with_label("Play").build();
        let stop = Button::builder(&audio).with_label("Stop").build();
        let audio_sizer = BoxSizer::builder(Orientation::Horizontal).build();
        audio_sizer.add(&play, 0, SizerFlag::Right, 6);
        audio_sizer.add(&stop, 0, SizerFlag::empty(), 0);
        audio.set_sizer(audio_sizer, true);

        let sides = SizerFlag::Left | SizerFlag::Right;
        let root = BoxSizer::builder(Orientation::Vertical).build();
        root.add(
            &title,
            0,
            sides | SizerFlag::Top | SizerFlag::Expand,
            MARGIN,
        );
        root.add(&info, 0, sides | SizerFlag::Expand, MARGIN);
        root.add(&message, 0, SizerFlag::All | SizerFlag::Expand, MARGIN);
        root.add(&audio, 0, SizerFlag::All, MARGIN);
        root.add(&open, 0, sides | SizerFlag::Top, MARGIN);
        root.add(&canvas, 1, SizerFlag::All | SizerFlag::Expand, MARGIN);
        root.add(&text, 1, SizerFlag::Top | SizerFlag::Expand, MARGIN);
        panel.set_sizer(root, true);

        let preview = Rc::new(Preview {
            w: Widgets {
                panel,
                title,
                info,
                message,
                canvas,
                image,
                text,
                audio,
                open,
            },
            state: RefCell::new(State::default()),
        });
        preview.show(None);

        let p = preview.clone();
        play.on_click(move |_| p.play());
        let p = preview.clone();
        stop.on_click(move |_| p.stop_audio());
        let p = preview.clone();
        open.on_click(move |_| p.open_copy());
        let p = preview.clone();
        canvas.on_size(move |event| {
            p.fit_image();
            event.skip(true);
        });
        preview
    }

    pub fn panel(&self) -> Panel {
        self.w.panel
    }

    /// Starts previewing `request`, or empties the pane for `None`. Whatever was being
    /// read or played before is abandoned.
    pub fn show(&self, request: Option<Request>) {
        self.halt();
        let w = self.w;
        let Some(request) = request else {
            w.title.set_label("Preview");
            w.info.set_label("");
            self.present(None, "Select a file to preview it.");
            return;
        };

        w.title.set_label(&request.name);
        w.info.set_label(&format_size(request.size));
        self.state.borrow_mut().current = Some((request.name.clone(), request.source.clone()));
        if let Some(thumb) = &request.thumb {
            self.show_image(thumb, request.size);
            return;
        }
        self.present(
            None,
            if request.source.needs_scan() {
                "Reading through the package to find this file...\n\n\
                 Its contents are not loaded, so every preview has to search the \
                 package again. Single pass and in-memory mode keep them at hand."
            } else {
                "Loading preview..."
            },
        );

        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut st = self.state.borrow_mut();
        st.generation += 1;
        st.cancel = cancel.clone();
        st.rx = Some(rx);
        let generation = st.generation;
        st.worker = Some(std::thread::spawn(move || {
            let content = produce(&request, &cancel);
            let _ = tx.send(Reply {
                generation,
                name: request.name,
                size: request.size,
                content,
            });
        }));
    }

    /// Stops reading and playing, and waits for the worker so it no longer has any
    /// temp file open. Cancellation is checked every 64 KB, so the wait is short.
    pub fn halt(&self) {
        self.stop_audio();
        let worker = {
            let mut st = self.state.borrow_mut();
            st.cancel.store(true, Ordering::Relaxed);
            st.rx = None;
            st.current = None;
            st.picture = None;
            st.audio = None;
            st.worker.take()
        };
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }

    fn show_image(&self, thumb: &Thumb, size: u64) {
        let (width, height) = (thumb.width, thumb.height);
        let shrunk = thumb.picture.width() < width;
        self.w.info.set_label(&format!(
            "{width} x {height} px, {}{}",
            format_size(size),
            if shrunk { " (reduced copy)" } else { "" }
        ));
        self.state.borrow_mut().picture = Some(thumb.picture.clone());
        self.present(Some(&self.w.canvas), "");
        self.fit_image();
    }

    /// Picks up a finished preview. Call regularly from the UI thread.
    pub fn pump(&self) {
        let notice = {
            let st = self.state.borrow();
            st.notices.as_ref().and_then(|(_, rx)| rx.try_recv().ok())
        };
        if let Some(notice) = notice {
            self.present(Some(&self.w.text), &notice);
        }
        let reply = {
            let st = self.state.borrow();
            match st.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
                Some(reply) if reply.generation == st.generation => reply,
                _ => return,
            }
        };
        let size = format_size(reply.size);
        let w = self.w;
        match reply.content {
            Ok(Content::Image(thumb)) => self.show_image(&thumb, reply.size),
            Ok(Content::Text { text, truncated }) => {
                w.info.set_label(&if truncated {
                    format!("{size}, showing the first {}", format_size(TEXT_LIMIT))
                } else {
                    size
                });
                w.text.set_value(&text);
                self.present(Some(&w.text), "");
            }
            Ok(Content::Audio { bytes, duration }) => {
                w.info.set_label(&match duration {
                    Some(d) => format!("{}, {size}", format_duration(d)),
                    None => size,
                });
                self.state.borrow_mut().audio = Some(bytes);
                self.present(Some(&w.audio), "");
            }
            Ok(Content::Nothing(why)) => self.present(None, &why),
            Err(Error::Cancelled) => {}
            Err(e) => self.present(None, &format!("Could not read {}:\n\n{e}", reply.name)),
        }
    }

    /// Shows `content` (one of the content widgets) and hides the others; `message`
    /// goes above it, or stands alone.
    fn present(&self, content: Option<&dyn WxWidget>, message: &str) {
        let w = self.w;
        for widget in [&w.canvas as &dyn WxWidget, &w.text, &w.audio] {
            let shown = content.is_some_and(|c| c.handle_ptr() == widget.handle_ptr());
            crate::icons::WindowRef(widget.handle_ptr()).show(shown);
        }
        // Only text is worth handing to an editor.
        w.open
            .show(content.is_some_and(|c| c.handle_ptr() == w.text.handle_ptr()));
        w.message.set_label(message);
        w.message.show(!message.is_empty());
        w.panel.layout();
        // Wrap to the pane after layout has given the label its width.
        if !message.is_empty() {
            w.message
                .wrap((w.panel.get_client_size().width - 2 * MARGIN).max(80));
            w.panel.layout();
        }
    }

    /// Scales the current image to the canvas, never beyond its own size.
    fn fit_image(&self) {
        let st = self.state.borrow();
        let Some(picture) = &st.picture else {
            return;
        };
        let room = self.w.canvas.get_client_size();
        let (max_w, max_h) = (room.width.max(1) as u32, room.height.max(1) as u32);
        let scale = (f64::from(max_w) / f64::from(picture.width()))
            .min(f64::from(max_h) / f64::from(picture.height()))
            .min(1.0);
        let width = ((f64::from(picture.width()) * scale) as u32).max(1);
        let height = ((f64::from(picture.height()) * scale) as u32).max(1);
        let scaled;
        let shown = if (width, height) == picture.dimensions() {
            picture
        } else {
            scaled = imageops::resize(picture, width, height, FilterType::Triangle);
            &scaled
        };
        if let Some(bitmap) = Bitmap::from_rgba(shown.as_raw(), width, height) {
            self.w.image.set_bitmap(&bitmap);
            self.w.canvas.layout();
        }
    }

    fn play(&self) {
        let mut st = self.state.borrow_mut();
        let Some(bytes) = st.audio.clone() else {
            return;
        };
        if let Some(player) = st.player.take() {
            player.stop();
        }
        if st.output.is_none() {
            match rodio::DeviceSinkBuilder::open_default_sink() {
                Ok(mut output) => {
                    output.log_on_drop(false);
                    st.output = Some(output);
                }
                Err(e) => {
                    drop(st);
                    self.present(Some(&self.w.audio), &format!("No audio output device: {e}"));
                    return;
                }
            }
        }
        let Some(output) = &st.output else {
            return;
        };
        if let Ok(decoder) = rodio::Decoder::try_from(Cursor::new(bytes)) {
            let player = rodio::Player::connect_new(output.mixer());
            player.append(decoder);
            st.player = Some(player);
        }
    }

    /// Saves a copy of the file being shown to the temp folder and opens it with the
    /// program registered for its type.
    fn open_copy(&self) {
        let mut st = self.state.borrow_mut();
        let Some((name, source)) = st.current.clone() else {
            return;
        };
        let (notices, _) = st.notices.get_or_insert_with(mpsc::channel);
        let notices = notices.clone();
        // Not tied to the preview's cancel flag: selecting another row shouldn't stop
        // a copy that was asked for.
        std::thread::spawn(move || {
            let result = (|| -> Result<(), String> {
                let never = AtomicBool::new(false);
                let bytes = source
                    .read(EDITOR_LIMIT + 1, &never)
                    .map_err(|e| e.to_string())?;
                if bytes.len() as u64 > EDITOR_LIMIT {
                    return Err("this file is too large".to_owned());
                }
                let dir = copies_dir();
                std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                let path = dir.join(editor_file_name(&name));
                std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
                crate::open_path(&path).map_err(|e| e.to_string())
            })();
            if let Err(e) = result {
                let _ = notices.send(format!("Could not open a copy in an editor: {e}"));
            }
        });
    }

    /// Deletes the copies made for external editors. Call when the app closes.
    pub fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(copies_dir());
    }

    fn stop_audio(&self) {
        if let Some(player) = self.state.borrow_mut().player.take() {
            player.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert!(kind_of("Hero.PNG") == Kind::Image);
        assert!(kind_of("Jump.ogg") == Kind::Audio);
        assert!(kind_of("Player.cs") == Kind::Other);
        assert!(kind_of("noextension") == Kind::Other);
    }

    #[test]
    fn text_sniffing() {
        assert_eq!(
            decode_text(b"\xEF\xBB\xBFhello", false).as_deref(),
            Some("hello")
        );
        assert_eq!(decode_text(b"bin\0ary", false), None);
        assert_eq!(decode_text(b"\xFF\xFE\xFD", false), None);
        // "é" cut in half by the size limit is fine; in a complete file it is not.
        assert_eq!(decode_text(b"caf\xC3", true).as_deref(), Some("caf"));
        assert_eq!(decode_text(b"caf\xC3", false), None);
    }

    #[test]
    fn editor_copies_never_keep_a_runnable_extension() {
        assert_eq!(editor_file_name("Player.cs"), "Player.cs");
        assert_eq!(editor_file_name("Notes.TXT"), "Notes.TXT");
        assert_eq!(editor_file_name("setup.bat"), "setup.bat.txt");
        assert_eq!(editor_file_name("Main.unity"), "Main.unity.txt");
        assert_eq!(editor_file_name("README"), "README.txt");
        assert_eq!(editor_file_name("a:b?.cs"), "a_b_.cs");
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(Duration::from_secs(5)), "0:05");
        assert_eq!(format_duration(Duration::from_secs(125)), "2:05");
    }
}
