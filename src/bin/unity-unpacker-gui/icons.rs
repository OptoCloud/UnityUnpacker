//! The icon set (`assets/icons`), embedded in the executable and scaled for the display.

use image::imageops::{self, FilterType};
use image::{ImageFormat, RgbaImage};
use wxdragon::prelude::*;

macro_rules! icons {
    ($($variant:ident => $file:literal),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Icon { $($variant),* }

        impl Icon {
            const ALL: &'static [Icon] = &[$(Icon::$variant),*];

            fn png(self) -> &'static [u8] {
                match self {
                    $(Icon::$variant => include_bytes!(concat!("../../../assets/icons/", $file)),)*
                }
            }
        }
    };
}

icons! {
    AnimationClip => "animation-clip.png",
    AnimatorController => "animator-controller.png",
    Assembly => "assembly.png",
    Audio => "audio.png",
    AvatarMask => "avatar-mask.png",
    Folder => "folder.png",
    Help => "help.png",
    Image => "image.png",
    Material => "material.png",
    Mesh => "mesh.png",
    PrefabModel => "prefab-model.png",
    Scene => "scene.png",
    Script => "script.png",
    ScriptableObject => "scriptable-object.png",
    Shader => "shader.png",
    Text => "text.png",
}

impl Icon {
    /// The icon the Unity editor would show for an asset with this file name.
    pub fn for_file(name: &str) -> Icon {
        let ext = name
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "cs" => Icon::Script,
            "dll" | "asmdef" | "asmref" => Icon::Assembly,
            "anim" => Icon::AnimationClip,
            "controller" | "overridecontroller" => Icon::AnimatorController,
            "mask" => Icon::AvatarMask,
            "wav" | "mp3" | "ogg" | "aif" | "aiff" | "flac" | "mixer" => Icon::Audio,
            "png" | "jpg" | "jpeg" | "tga" | "psd" | "tif" | "tiff" | "bmp" | "gif" | "exr"
            | "hdr" | "dds" | "svg" => Icon::Image,
            "mat" | "physicmaterial" | "physicsmaterial2d" => Icon::Material,
            "mesh" => Icon::Mesh,
            "prefab" | "fbx" | "obj" | "blend" | "dae" | "3ds" | "max" | "ma" | "mb" => {
                Icon::PrefabModel
            }
            "unity" => Icon::Scene,
            "asset" | "playable" | "preset" => Icon::ScriptableObject,
            "shader" | "cginc" | "hlsl" | "glslinc" | "compute" | "shadergraph"
            | "shadersubgraph" | "shadervariants" => Icon::Shader,
            _ => Icon::Text,
        }
    }

    pub fn from_index(index: usize) -> Icon {
        Icon::ALL.get(index).copied().unwrap_or(Icon::Text)
    }

    fn image(self) -> RgbaImage {
        image::load_from_memory_with_format(self.png(), ImageFormat::Png)
            .expect("embedded icon is a valid PNG")
            .into_rgba8()
    }

    /// The icon as a `px` x `px` bitmap. The set is drawn for Unity's dark theme, so
    /// with `on_light` its greys are darkened to stay visible on a light background.
    pub fn bitmap(self, px: u32, on_light: bool) -> Option<Bitmap> {
        let mut img = self.image();
        if on_light {
            for p in img.pixels_mut() {
                let [r, g, b, _] = p.0;
                let grey = r.max(g).max(b) - r.min(g).min(b) < 40;
                let factor = if grey { 0.5 } else { 0.85 };
                for c in &mut p.0[..3] {
                    *c = (f32::from(*c) * factor) as u8;
                }
            }
        }
        if img.width() != px || img.height() != px {
            // Resample with premultiplied alpha so transparent pixels don't bleed
            // their (arbitrary) colour into the edges.
            for p in img.pixels_mut() {
                let a = u16::from(p.0[3]);
                for c in &mut p.0[..3] {
                    *c = (u16::from(*c) * a / 255) as u8;
                }
            }
            img = imageops::resize(&img, px, px, FilterType::Lanczos3);
            for p in img.pixels_mut() {
                let a = u16::from(p.0[3]);
                for c in &mut p.0[..3] {
                    // Fully transparent pixels have no colour to restore.
                    let straight = (u16::from(*c) * 255).checked_div(a).unwrap_or(0);
                    *c = straight.min(255) as u8;
                }
            }
        }
        Bitmap::from_rgba(img.as_raw(), px, px)
    }
}

/// Every icon at the size used in lists and on buttons, ready to draw.
pub struct Icons {
    bitmaps: Vec<Option<Bitmap>>,
    /// Edge length in pixels (16 at 100% scaling).
    pub size: i32,
}

impl Icons {
    /// Loads the set at the right size for the display `window` is on.
    pub fn load(window: &dyn WxWidget) -> Icons {
        let size = WindowRef(window.handle_ptr()).from_dip_int(16).max(16);
        let background = SystemSettings::get_colour(SystemColour::ListBox);
        let on_light =
            u32::from(background.r) + u32::from(background.g) + u32::from(background.b) > 384;
        Icons {
            bitmaps: Icon::ALL
                .iter()
                .map(|icon| icon.bitmap(size as u32, on_light))
                .collect(),
            size,
        }
    }

    pub fn get(&self, icon: Icon) -> Option<&Bitmap> {
        self.bitmaps.get(icon as usize)?.as_ref()
    }
}

/// Lets a `&dyn WxWidget` be passed where a sized `W: WxWidget` is needed.
pub struct WindowRef(pub *mut wxdragon::ffi::wxd_Window_t);

impl WxWidget for WindowRef {
    fn handle_ptr(&self) -> *mut wxdragon::ffi::wxd_Window_t {
        self.0
    }
}
