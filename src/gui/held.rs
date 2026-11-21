//! Picture caches that count what they hold in the memory report (`alloc::Gauge`).

use std::collections::HashMap;
use std::hash::Hash;

use slint::Image;

use crate::alloc::{self, Gauge};

/// Bytes of decoded pixels a picture holds.
pub(crate) trait Measured {
    fn bytes(&self) -> usize;
}

impl Measured for Image {
    fn bytes(&self) -> usize {
        let s = self.size();
        s.width as usize * s.height as usize * 4
    }
}

impl Measured for Option<Image> {
    fn bytes(&self) -> usize {
        self.as_ref().map_or(0, Measured::bytes)
    }
}

pub(crate) type Pictures<const G: usize> = ImageMap<String, Image, G>;
pub(crate) const THUMBS: usize = Gauge::Thumbs as usize;
pub(crate) const AVATARS: usize = Gauge::Avatars as usize;
pub(crate) const SENDER_AVATARS: usize = Gauge::SenderAvatars as usize;
pub(crate) const COMPOSER: usize = Gauge::Composer as usize;
pub(crate) const STICKER_COVERS: usize = Gauge::StickerCovers as usize;
pub(crate) const GALLERY: usize = Gauge::Gallery as usize;

/// A map of pictures counted under gauge `G` (`Gauge as usize`). It is read like a `HashMap`;
/// changes go through its own methods, so the count follows them.
pub(crate) struct ImageMap<K, V: Measured, const G: usize> {
    map: HashMap<K, V>,
}

impl<K, V: Measured, const G: usize> Default for ImageMap<K, V, G> {
    fn default() -> Self {
        Self { map: HashMap::new() }
    }
}

impl<K, V: Measured, const G: usize> std::ops::Deref for ImageMap<K, V, G> {
    type Target = HashMap<K, V>;
    fn deref(&self) -> &HashMap<K, V> {
        &self.map
    }
}

impl<K, V: Measured, const G: usize> ImageMap<K, V, G> {
    const GAUGE: Gauge = Gauge::ALL[G];
}

impl<K: Eq + Hash, V: Measured, const G: usize> ImageMap<K, V, G> {
    pub(crate) fn insert(&mut self, k: K, v: V) {
        alloc::add(Self::GAUGE, v.bytes(), 1);
        if let Some(old) = self.map.insert(k, v) {
            alloc::sub(Self::GAUGE, old.bytes(), 1);
        }
    }

    pub(crate) fn remove<Q>(&mut self, k: &Q) -> Option<V>
    where
        K: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let old = self.map.remove(k)?;
        alloc::sub(Self::GAUGE, old.bytes(), 1);
        Some(old)
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        self.map.retain(|k, v| {
            let kept = keep(k, v);
            if !kept {
                alloc::sub(Self::GAUGE, v.bytes(), 1);
            }
            kept
        });
    }

    pub(crate) fn clear(&mut self) {
        self.retain(|_, _| false);
    }
}

impl<K, V: Measured, const G: usize> Drop for ImageMap<K, V, G> {
    fn drop(&mut self) {
        let bytes = self.map.values().map(Measured::bytes).sum();
        alloc::sub(Self::GAUGE, bytes, self.map.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_count_follows_the_map() {
        // a gauge no other test fills
        const G: usize = STICKER_COVERS;
        let img = |w, h| Image::from_rgba8(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h));
        let held = || alloc::held(Gauge::StickerCovers);
        let mut m = Pictures::<G>::default();
        m.insert("a".into(), img(10, 10));
        m.insert("b".into(), img(20, 10));
        assert_eq!(held(), (1200, 2));
        // replaced
        m.insert("a".into(), img(5, 5));
        assert_eq!(held(), (900, 2));
        m.remove("b");
        assert_eq!(held(), (100, 1));
        m.insert("c".into(), img(1, 1));
        m.retain(|k, _| k == "c");
        assert_eq!(held(), (4, 1));
        drop(m);
        assert_eq!(held(), (0, 0));
    }
}
