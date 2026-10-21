//! A bounded image cache for the timeline.
//!
//! Without one, gpui keeps every decoded image for the life of the app. A
//! decoded photo is width × height × 4 bytes (about 48 MB for 12 megapixels),
//! so scrolling through a photo-heavy group would grow memory without limit.
//! This cache keeps the most recently drawn images within a byte budget and
//! drops the rest; each conversation view owns one, so everything it decoded
//! is released when you switch away.

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use gpui_kit::{App, AppContext as _, Entity, ImageCache, ImageCacheError, ImageCacheItem, RenderImage, Resource, Window};

/// Decoded bytes kept before evicting the least recently drawn image.
const BUDGET_BYTES: usize = 256 * 1024 * 1024;

pub struct LruImageCache {
    items: HashMap<u64, ImageCacheItem>,
    /// Least recently used first.
    order: VecDeque<u64>,
    budget: usize,
}

impl LruImageCache {
    pub fn new(cx: &mut App) -> Entity<Self> {
        Self::with_budget(BUDGET_BYTES, cx)
    }

    pub fn with_budget(budget: usize, cx: &mut App) -> Entity<Self> {
        let cache = cx.new(|_| Self { items: HashMap::new(), order: VecDeque::new(), budget });
        cx.observe_release(&cache, |cache, cx| {
            for (_, item) in cache.items.drain() {
                if let Some(Ok(image)) = item.get() {
                    cx.drop_image(image, None);
                }
            }
        })
        .detach();
        cache
    }

    fn decoded_bytes(&self) -> usize {
        self.items.values().filter_map(|item| item.get()?.ok()).map(|image| image_bytes(&image)).sum()
    }

    /// Evicts least recently used images until within budget, never the one
    /// just requested.
    fn evict(&mut self, keep: u64, window: &mut Window, cx: &mut App) {
        let mut total = self.decoded_bytes();
        while total > self.budget {
            let Some(position) = self.order.iter().position(|key| *key != keep) else { break };
            let Some(key) = self.order.remove(position) else { break };
            if let Some(item) = self.items.remove(&key)
                && let Some(Ok(image)) = item.get()
            {
                total -= image_bytes(&image);
                cx.drop_image(image, Some(window));
            }
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[cfg(test)]
    pub fn loaded(&self) -> usize {
        self.items.values().filter(|item| matches!(item.get(), Some(Ok(_)))).count()
    }

    /// Whether the image at `path` has decoded.
    #[cfg(test)]
    pub fn has_loaded(&self, path: &std::path::Path) -> bool {
        let key = Self::key(&Resource::Path(path.into()));
        self.items.get(&key).is_some_and(|item| matches!(item.get(), Some(Ok(_))))
    }

    fn key(resource: &Resource) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        resource.hash(&mut hasher);
        hasher.finish()
    }
}

fn image_bytes(image: &RenderImage) -> usize {
    (0..image.frame_count())
        .map(|frame| {
            let size = image.size(frame);
            size.width.0 as usize * size.height.0 as usize * 4
        })
        .sum()
}

impl ImageCache for LruImageCache {
    fn load(
        &mut self,
        resource: &Resource,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        let key = Self::key(resource);
        if let Some(position) = self.order.iter().position(|k| *k == key) {
            self.order.remove(position);
        }
        self.order.push_back(key);
        let result = self.items.entry(key).or_insert_with(|| ImageCacheItem::new(resource, cx)).use_image(window);
        if result.is_some() {
            self.evict(key, window, cx);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{Context, IntoElement, ParentElement as _, Render, Styled as _, TestAppContext, div, img, px};

    use super::*;

    /// A 1×1 PNG: 4 bytes once decoded.
    const PIXEL_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
        0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00,
        0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xF0, 0x1F, 0x00, 0x05, 0x00, 0x01, 0xFF,
        0x89, 0x99, 0x3D, 0x1D, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    /// Draws one image through the cache.
    struct Show {
        path: std::path::PathBuf,
        cache: Entity<LruImageCache>,
    }

    impl Render for Show {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().image_cache(self.cache.clone()).child(img(self.path.clone()).size(px(10.)))
        }
    }

    #[gpui_kit::test]
    fn evicts_least_recently_drawn_images_over_budget(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let [a, b] = ["a.png", "b.png"].map(|name| {
            let path = dir.path().join(name);
            std::fs::write(&path, PIXEL_PNG).unwrap();
            path
        });
        // Room for one decoded pixel image, not two.
        let cache = cx.update(|cx| LruImageCache::with_budget(6, cx));
        let window = cx.add_window(|_, _| Show { path: a.clone(), cache: cache.clone() });
        let draw_until_loaded = |cx: &mut TestAppContext| {
            for _ in 0..200 {
                window.update(cx, |_, window, _| window.refresh()).unwrap();
                cx.run_until_parked();
                if cx.read(|cx| cache.read(cx).loaded()) >= 1 {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("image never loaded");
        };

        draw_until_loaded(cx);
        assert_eq!(cx.read(|cx| cache.read(cx).len()), 1);
        // Drawing b pushes the total over budget: a, the older one, goes.
        window.update(cx, |show, _, _| show.path = b.clone()).unwrap();
        for _ in 0..200 {
            window.update(cx, |_, window, _| window.refresh()).unwrap();
            cx.run_until_parked();
            if cx.read(|cx| cache.read(cx).len()) == 1 && cx.read(|cx| cache.read(cx).loaded()) == 1 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(cx.read(|cx| (cache.read(cx).len(), cache.read(cx).loaded())), (1, 1));
    }
}
