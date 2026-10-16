//! Desktop's Data usage page: media auto-download, and this app's lossless
//! image optimization where Desktop has its sent media quality.

use gpui_kit::{AnyElement, App, Entity, SharedString};
use signal_backend::media::Optimization;

use gpui_kit::prelude::*;
use gpui_kit::div;
use signal_backend::auto_download::AutoDownload;

use super::{Choice, Settings, list, select, switch};
use crate::theme::Palette;

pub(super) fn render(settings: &Entity<Settings>, palette: Palette, cx: &mut App) -> AnyElement {
    let state = settings.read(cx).state.clone();
    let current = state.read(cx).image_optimization;
    let choice = |label: &'static str, level: Optimization| {
        let state = state.clone();
        Choice::new(label, current == level, move |_, cx| state.update(cx, |s, cx| s.set_image_optimization(level, cx)))
    };
    let explanation: SharedString = match current {
        Optimization::Max => {
            "Images you send are made as small as possible without changing a pixel (oxipng for PNG, mozjpeg for \
             JPEG). Large images can take a few seconds."
        }
        Optimization::Fast => "Images are made smaller without changing a pixel, trading some savings for speed.",
        Optimization::Off => "Images are sent as they are, apart from removing metadata such as location.",
    }
    .into();
    let auto = state.read(cx).prefs.auto_download;
    let kind = |id: &'static str, label: &'static str, on: bool, set: fn(&mut AutoDownload, bool)| {
        let state = state.clone();
        switch(id, label, None, on, false, palette, move |on, _, cx| {
            let mut choices = state.read(cx).prefs.auto_download;
            set(&mut choices, on);
            state.update(cx, |s, cx| s.set_auto_download(choices, cx));
        })
    };
    let downloads = list(
        Some("Media auto-download"),
        Some("Voice messages and stickers are always auto-downloaded.".into()),
        [
            kind("auto-download-photos", "Photos", auto.photos, |c, on| c.photos = on),
            kind("auto-download-videos", "Videos", auto.videos, |c, on| c.videos = on),
            kind("auto-download-audio", "Audio", auto.audio, |c, on| c.audio = on),
            kind("auto-download-documents", "Documents", auto.documents, |c, on| c.documents = on),
        ],
        palette,
    );
    let optimization = list(
        None,
        None,
        [select(
            "image-optimization",
            "Optimize images before sending",
            Some(explanation),
            vec![choice("Maximum", Optimization::Max), choice("Fast", Optimization::Fast), choice("Off", Optimization::Off)],
            false,
            palette,
        )],
        palette,
    );
    div().flex().flex_col().gap_5().child(downloads).child(optimization).into_any_element()
}
