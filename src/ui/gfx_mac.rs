//! CoreGraphics + CoreText drawing for the main window and the overlay
//! (macOS). Same API as the Direct2D renderer: coordinates are DIPs (points),
//! `dpi` / 96 is the backing scale. Everything is drawn into an offscreen
//! premultiplied BGRA bitmap that the host hands to a layer (or writes as a
//! PNG). The icon glyphs (Segoe Fluent Icons code points on Windows) are
//! drawn as SF Symbols.

use std::collections::HashMap;

use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::{NSRect, NSSize};

use crate::mac::sys::*;
use crate::mac::{Id, nsstring, rust_string};

pub use super::geom::{Align, Color, Family, Font, Rect};

/// The user's text font: empty (the system font), an installed family name,
/// or a path to a .ttf / .otf file. Bumping the generation makes every Gfx
/// rebuild its fonts.
static FONT_SPEC: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
static FONT_GEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn set_font(spec: &str) {
    let mut s = FONT_SPEC.lock().unwrap();
    if *s != spec.trim() {
        *s = spec.trim().to_string();
        FONT_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Installed font family names, sorted.
pub fn system_families() -> Vec<String> {
    let mut out = Vec::new();
    objc2::rc::autoreleasepool(|_| unsafe {
        let fm: Id = msg_send![class!(NSFontManager), sharedFontManager];
        let families: Option<Id> = msg_send![&*fm, availableFontFamilies];
        let Some(families) = families else { return };
        let n: usize = msg_send![&*families, count];
        for i in 0..n {
            let f: Id = msg_send![&*families, objectAtIndex: i];
            let name = rust_string(&f);
            if !name.starts_with('.') {
                out.push(name);
            }
        }
    });
    out.sort_by_key(|a| a.to_lowercase());
    out.dedup();
    out
}

/// SF Symbol for a Segoe Fluent Icons code point used by the UI.
fn symbol_name(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{E80F}' => "house",
        '\u{E81C}' => "clock.arrow.circlepath",
        '\u{E9D2}' => "chart.bar",
        '\u{E8F1}' => "square.stack.3d.up",
        '\u{E8D2}' => "textformat",
        '\u{E713}' => "gearshape",
        '\u{E720}' => "mic.fill",
        '\u{E768}' => "play.fill",
        '\u{E8C8}' => "doc.on.doc",
        '\u{E896}' => "arrow.down",
        '\u{E74D}' => "trash",
        '\u{E73E}' => "checkmark",
        '\u{E72C}' => "arrow.clockwise",
        '\u{E70D}' => "chevron.down",
        '\u{E710}' => "plus",
        '\u{E738}' => "minus",
        '\u{E721}' => "magnifyingglass",
        '\u{E8A7}' => "arrow.up.forward.square",
        '\u{E838}' => "folder",
        '\u{E823}' => "clock",
        '\u{E765}' => "keyboard",
        '\u{E945}' => "sparkles",
        '\u{E7BA}' => "exclamationmark.triangle",
        '\u{E711}' => "xmark",
        '\u{E946}' => "info.circle",
        '\u{E77F}' => "doc.on.clipboard",
        '\u{E8FB}' => "checkmark.circle",
        '\u{E9CE}' => "questionmark.circle",
        _ => return None,
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FontKey {
    size10: u32,
    weight: u16,
    family: Family,
}

/// A laid-out line and where its text starts.
struct Line {
    line: Cf,
    width: f64,
}

enum Custom {
    Family(String),
    File(Cf),
}

pub struct Gfx {
    ctx: CGContextRef,
    cs: CGColorSpaceRef,
    w_px: usize,
    h_px: usize,
    pub dpi: f32,
    fonts: HashMap<FontKey, Cf>,
    icons: HashMap<(char, u32, u16), Option<Id>>,
    font_gen: u32,
    custom: Option<Custom>,
    fades: Vec<(Rect, f32, f32)>,
    ns_ctx: Option<Id>,
    /// Vertical gradients as 1-pixel-wide strips, keyed by colours and pixel
    /// height: stretching a strip is far cheaper than CoreGraphics shading
    /// every pixel of every card on every frame.
    strips: std::cell::RefCell<HashMap<([u32; 8], usize), Cf>>,
    /// Large glows as images, keyed by colour and pixel radius.
    glows: std::cell::RefCell<HashMap<([u32; 4], usize), Cf>>,
    /// Card shadows as nine-slice images, keyed by radius, strength, scale.
    shadows: std::cell::RefCell<HashMap<[u32; 3], Option<Slices>>>,
    /// Decoded embedded PNGs keyed by data address; None when decoding
    /// failed, so it is not retried every frame.
    pngs: HashMap<usize, Option<Cf>>,
}

/// A shadow rendered once around a small card: eight pieces (corners and
/// edges, no centre) and where they are cut, in device pixels.
struct Slices {
    pieces: Vec<(usize, usize, Cf)>,
    /// Cut positions: left margin, corner, right margin, bottom margin.
    m_px: usize,
    cs_px: usize,
    bm_px: usize,
}

impl Drop for Gfx {
    fn drop(&mut self) {
        unsafe {
            if !self.ctx.is_null() {
                CGContextRelease(self.ctx);
            }
            CGColorSpaceRelease(self.cs);
        }
    }
}

fn path_rounded(r: Rect, radius: f32) -> Option<Cf> {
    if r.w <= 0.0 || r.h <= 0.0 {
        return None;
    }
    let rad = (radius.max(0.0)).min(r.w / 2.0).min(r.h / 2.0) as f64;
    let p = unsafe { CGPathCreateWithRoundedRect(cg(r), rad, rad, std::ptr::null()) };
    Cf::new(p)
}

/// The four shadow layers around `r`, each filled as a ring (even-odd, its
/// inner edge a pixel inside `r` so the shape's edge looks as before).
fn shadow_layers(ctx: CGContextRef, r: Rect, radius: f32, strength: f32) {
    let Some(inner) = path_rounded(r.inset(1.0, 1.0), (radius - 1.0).max(0.0)) else { return };
    for i in 1..=4 {
        let d = i as f32 * 2.0;
        let rr = Rect::new(r.x - d * 0.5, r.y + d * 0.6, r.w + d, r.h + d);
        let Some(outer) = path_rounded(rr, radius + d * 0.5) else { continue };
        let a = (strength / (i as f32 + 1.0)) as f64;
        unsafe {
            CGContextSetRGBFillColor(ctx, 0.0, 0.0, 0.0, a);
            CGContextBeginPath(ctx);
            CGContextAddPath(ctx, outer.as_ptr());
            CGContextAddPath(ctx, inner.as_ptr());
            CGContextEOFillPath(ctx);
        }
    }
}

/// Renders the shadow around a card just large enough to hold the corners
/// and cuts it into eight pieces (columns and rows: corner, edge, corner).
fn render_shadow(cs: CGColorSpaceRef, radius: f32, strength: f32, s: f32) -> Option<Slices> {
    // The layers reach 4 points left and right of the card and 12.8 below
    // it, and start below its top; corners hold the largest layer's curve.
    let m_px = (8.0 * s).ceil() as usize;
    let bm_px = (16.0 * s).ceil() as usize;
    let cs_px = ((radius.max(0.0) + 10.0) * s).ceil() as usize;
    let t_px = 2 * cs_px + 2;
    let (w_px, h_px) = (t_px + 2 * m_px, t_px + bm_px);
    unsafe {
        let ctx = CGBitmapContextCreate(std::ptr::null_mut(), w_px, h_px, 8, 0, cs, kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little);
        if ctx.is_null() {
            return None;
        }
        // y-down points, as in the frame; the card at (m, 0).
        CGContextTranslateCTM(ctx, 0.0, h_px as f64);
        CGContextScaleCTM(ctx, s as f64, -s as f64);
        let t = t_px as f32 / s;
        shadow_layers(ctx, Rect::new(m_px as f32 / s, 0.0, t, t), radius, strength);
        let img = CGBitmapContextCreateImage(ctx);
        CGContextRelease(ctx);
        let img = Cf::new(img as CFTypeRef)?;
        let xs = [0, m_px + cs_px, m_px + t_px - cs_px, w_px];
        let ys = [0, cs_px, t_px - cs_px, h_px];
        let mut pieces = Vec::new();
        for row in 0..3 {
            for col in 0..3 {
                if row == 1 && col == 1 {
                    continue;
                }
                let rect = CGRect::new(xs[col] as f64, ys[row] as f64, (xs[col + 1] - xs[col]) as f64, (ys[row + 1] - ys[row]) as f64);
                let piece = Cf::new(CGImageCreateWithImageInRect(img.as_ptr() as CGImageRef, rect) as CFTypeRef)?;
                pieces.push((col, row, piece));
            }
        }
        Some(Slices { pieces, m_px, cs_px, bm_px })
    }
}

fn color_bits(c: Color) -> [u32; 4] {
    [c.0.to_bits(), c.1.to_bits(), c.2.to_bits(), c.3.to_bits()]
}

fn cg(r: Rect) -> CGRect {
    CGRect::new(r.x as f64, r.y as f64, r.w as f64, r.h as f64)
}

fn pt(x: f32, y: f32) -> CGPoint {
    CGPoint { x: x as f64, y: y as f64 }
}

fn utf16_len(s: &str) -> CFIndex {
    s.encode_utf16().count() as CFIndex
}

impl Gfx {
    /// A `w_px` x `h_px` bitmap at `dpi` (96 = 1x).
    pub fn new(w_px: usize, h_px: usize, dpi: f32) -> Option<Gfx> {
        let cs = unsafe { CGColorSpaceCreateWithName(kCGColorSpaceSRGB) };
        let mut g = Gfx {
            ctx: std::ptr::null_mut(),
            cs,
            w_px: 0,
            h_px: 0,
            dpi,
            fonts: HashMap::new(),
            icons: HashMap::new(),
            font_gen: u32::MAX,
            custom: None,
            fades: Vec::new(),
            strips: Default::default(),
            glows: Default::default(),
            shadows: Default::default(),
            pngs: HashMap::new(),
            ns_ctx: None,
        };
        g.resize(w_px, h_px);
        (!g.ctx.is_null()).then_some(g)
    }

    /// Reallocates the bitmap when the pixel size changed.
    pub fn resize(&mut self, w_px: usize, h_px: usize) {
        let (w_px, h_px) = (w_px.max(1), h_px.max(1));
        if (w_px, h_px) == (self.w_px, self.h_px) && !self.ctx.is_null() {
            return;
        }
        unsafe {
            if !self.ctx.is_null() {
                CGContextRelease(self.ctx);
            }
            self.ctx = CGBitmapContextCreate(
                std::ptr::null_mut(),
                w_px,
                h_px,
                8,
                0,
                self.cs,
                kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little,
            );
        }
        self.ns_ctx = None;
        self.w_px = w_px;
        self.h_px = h_px;
    }

    fn scale(&self) -> f64 {
        self.dpi as f64 / 96.0
    }

    pub fn set_dpi(&mut self, dpi: f32) {
        self.dpi = dpi;
    }

    /// The current bitmap as an image (+1; release with CGImageRelease).
    pub fn snapshot(&self) -> CGImageRef {
        unsafe { CGBitmapContextCreateImage(self.ctx) }
    }

    /// BGRA of one pixel (top-left origin).
    pub fn pixel(&self, x: usize, y: usize) -> [u8; 4] {
        unsafe {
            let data = CGBitmapContextGetData(self.ctx) as *const u8;
            let stride = CGBitmapContextGetBytesPerRow(self.ctx);
            if data.is_null() || x >= self.w_px || y >= self.h_px {
                return [0; 4];
            }
            let p = data.add(y * stride + x * 4);
            [*p, *p.add(1), *p.add(2), *p.add(3)]
        }
    }

    pub fn write_png(&self, path: &std::path::Path) -> Result<(), String> {
        unsafe {
            let img = self.snapshot();
            if img.is_null() {
                return Err("no image".into());
            }
            let url = file_url(path);
            let ty = Cf::str("public.png");
            let dest = CGImageDestinationCreateWithURL(url.as_ptr(), ty.as_ptr(), 1, std::ptr::null());
            if dest.is_null() {
                CGImageRelease(img);
                return Err(format!("cannot write {}", path.display()));
            }
            CGImageDestinationAddImage(dest, img, std::ptr::null());
            let ok = CGImageDestinationFinalize(dest);
            CFRelease(dest as CFTypeRef);
            CGImageRelease(img);
            if ok { Ok(()) } else { Err(format!("cannot write {}", path.display())) }
        }
    }

    pub fn begin(&mut self) {
        let s = self.scale();
        unsafe {
            CGContextSaveGState(self.ctx);
            // DIPs, top-left origin, y down.
            CGContextTranslateCTM(self.ctx, 0.0, self.h_px as f64);
            CGContextScaleCTM(self.ctx, s, -s);
            // Upright glyphs in the flipped space.
            CGContextSetTextMatrix(self.ctx, CGAffineTransform { a: 1.0, b: 0.0, c: 0.0, d: -1.0, tx: 0.0, ty: 0.0 });
            // Grayscale antialiasing: the bitmap is often transparent.
            CGContextSetAllowsFontSmoothing(self.ctx, false);
            CGContextSetShouldSmoothFonts(self.ctx, false);
            CGContextSetInterpolationQuality(self.ctx, 3);
        }
    }

    /// Always true (no device to lose).
    pub fn end(&mut self) -> bool {
        while !self.fades.is_empty() {
            self.pop_fade();
        }
        unsafe { CGContextRestoreGState(self.ctx) };
        true
    }

    /// Draws an embedded PNG into `r`, resampled with high quality.
    pub fn image(&mut self, png: &'static [u8], r: Rect) {
        let img = self.pngs.entry(png.as_ptr() as usize).or_insert_with(|| unsafe {
            let provider = CGDataProviderCreateWithData(std::ptr::null_mut(), png.as_ptr().cast(), png.len(), None);
            if provider.is_null() {
                return None;
            }
            let img = CGImageCreateWithPNGDataProvider(provider, std::ptr::null(), true, 0);
            CGDataProviderRelease(provider);
            let img = Cf::new(img as CFTypeRef);
            if img.is_none() {
                log::warn!("image decode failed");
            }
            img
        });
        let Some(img) = img else { return };
        unsafe {
            CGContextSaveGState(self.ctx);
            CGContextSetInterpolationQuality(self.ctx, 3);
            // The context is flipped (y down); images draw bottom-up.
            CGContextTranslateCTM(self.ctx, 0.0, (r.y + r.h) as f64);
            CGContextScaleCTM(self.ctx, 1.0, -1.0);
            CGContextDrawImage(self.ctx, cg(Rect::new(r.x, 0.0, r.w, r.h)), img.as_ptr() as CGImageRef);
            CGContextRestoreGState(self.ctx);
        }
    }

    /// Moves the origin (DIPs) for the rest of the frame.
    pub fn translate(&self, dx: f32, dy: f32) {
        unsafe { CGContextTranslateCTM(self.ctx, dx as f64, dy as f64) };
    }

    fn set_fill(&self, c: Color) {
        unsafe { CGContextSetRGBFillColor(self.ctx, c.0 as f64, c.1 as f64, c.2 as f64, c.3 as f64) };
    }

    fn set_stroke(&self, c: Color) {
        unsafe { CGContextSetRGBStrokeColor(self.ctx, c.0 as f64, c.1 as f64, c.2 as f64, c.3 as f64) };
    }

    fn gradient_obj(&self, stops: &[(f32, Color)]) -> Option<Cf> {
        let comps: Vec<f64> = stops.iter().flat_map(|(_, c)| [c.0 as f64, c.1 as f64, c.2 as f64, c.3 as f64]).collect();
        let locs: Vec<f64> = stops.iter().map(|(p, _)| *p as f64).collect();
        let g = unsafe { CGGradientCreateWithColorComponents(self.cs, comps.as_ptr(), locs.as_ptr(), stops.len()) };
        Cf::new(g as CFTypeRef)
    }

    fn linear(&self, r: Rect, radius: f32, from: CGPoint, to: CGPoint, a: Color, b: Color) {
        let Some(grad) = self.gradient_obj(&[(0.0, a), (1.0, b)]) else { return };
        unsafe {
            CGContextSaveGState(self.ctx);
            match path_rounded(r, radius) {
                Some(p) if radius > 0.0 => {
                    CGContextBeginPath(self.ctx);
                    CGContextAddPath(self.ctx, p.as_ptr());
                    CGContextClip(self.ctx);
                }
                _ => CGContextClipToRect(self.ctx, cg(r)),
            }
            CGContextDrawLinearGradient(
                self.ctx,
                grad.as_ptr() as CGGradientRef,
                from,
                to,
                kCGGradientDrawsBeforeStartLocation | kCGGradientDrawsAfterEndLocation,
            );
            CGContextRestoreGState(self.ctx);
        }
    }

    /// Vertical gradient fill.
    pub fn gradient(&self, r: Rect, radius: f32, top: Color, bottom: Color) {
        if r.w <= 0.0 || r.h <= 0.0 {
            return;
        }
        let h_px = (r.h as f64 * self.scale()).ceil() as usize;
        let Some(strip) = self.strip(top, bottom, h_px.max(1)) else {
            return self.linear(r, radius, pt(r.x, r.y), pt(r.x, r.bottom()), top, bottom);
        };
        let img = strip.as_ptr() as CGImageRef;
        let rad = radius.max(0.0).min(r.w / 2.0).min(r.h / 2.0);
        // Under a rounded clip CoreGraphics builds a mask the size of the
        // shape, which for the cards cost a third of a frame. Large shapes
        // skip it: the strip fills the straight-edged cross through rect
        // clips (pixel-aligned, so the pieces meet without seams), and each
        // corner square gets the rounded shape in a flat colour. Across one
        // corner radius the cards' gradient changes by under half a level.
        let large = rad > 0.0 && r.w > 4.0 * rad && r.h > 4.0 * rad && r.w * r.h > 4000.0;
        let path = path_rounded(r, rad);
        // The strip covers whole pixels so only the clip antialiases the
        // shape's edges (an image edge on a fractional pixel would darken
        // them a second time).
        let s = self.scale() as f32;
        let (x0, y0) = ((r.x * s).floor() / s, (r.y * s).floor() / s);
        let outer = Rect::new(x0, y0, (r.right() * s).ceil() / s - x0, (r.bottom() * s).ceil() / s - y0);
        unsafe {
            if !large || path.is_none() {
                CGContextSaveGState(self.ctx);
                match &path {
                    Some(p) if rad > 0.0 => {
                        CGContextBeginPath(self.ctx);
                        CGContextAddPath(self.ctx, p.as_ptr());
                        CGContextClip(self.ctx);
                    }
                    _ => CGContextClipToRect(self.ctx, cg(r)),
                }
                // One row per device pixel: no filtering needed.
                CGContextSetInterpolationQuality(self.ctx, 1);
                CGContextDrawImage(self.ctx, cg(outer), img);
                CGContextRestoreGState(self.ctx);
                return;
            }
            let path = path.unwrap();
            let (inner_l, inner_r) = (((r.x + rad) * s).ceil() / s, ((r.right() - rad) * s).floor() / s);
            let (inner_t, inner_b) = (((r.y + rad) * s).ceil() / s, ((r.bottom() - rad) * s).floor() / s);
            let pieces = [
                Rect::new(inner_l, r.y, inner_r - inner_l, r.h),
                Rect::new(r.x, inner_t, inner_l - r.x, inner_b - inner_t),
                Rect::new(inner_r, inner_t, r.right() - inner_r, inner_b - inner_t),
            ];
            for piece in pieces {
                CGContextSaveGState(self.ctx);
                CGContextClipToRect(self.ctx, cg(piece));
                CGContextSetInterpolationQuality(self.ctx, 1);
                CGContextDrawImage(self.ctx, cg(outer), img);
                CGContextRestoreGState(self.ctx);
            }
            let corners = [
                (Rect::new(r.x, r.y, inner_l - r.x, inner_t - r.y), top),
                (Rect::new(inner_r, r.y, r.right() - inner_r, inner_t - r.y), top),
                (Rect::new(r.x, inner_b, inner_l - r.x, r.bottom() - inner_b), bottom),
                (Rect::new(inner_r, inner_b, r.right() - inner_r, r.bottom() - inner_b), bottom),
            ];
            for (corner, c) in corners {
                CGContextSaveGState(self.ctx);
                CGContextClipToRect(self.ctx, cg(corner));
                self.set_fill(c);
                CGContextBeginPath(self.ctx);
                CGContextAddPath(self.ctx, path.as_ptr());
                CGContextFillPath(self.ctx);
                CGContextRestoreGState(self.ctx);
            }
        }
    }

    /// Renders the glow image for `c` at `r_px` into the cache.
    fn glow_image(&self, c: Color, r_px: usize) -> Option<()> {
        let key = (color_bits(c), r_px);
        if self.glows.borrow().contains_key(&key) {
            return Some(());
        }
        let grad = self.gradient_obj(&[(0.0, c), (1.0, c.alpha(0.0))])?;
        let side = r_px * 2;
        let img = unsafe {
            let ctx = CGBitmapContextCreate(std::ptr::null_mut(), side, side, 8, 0, self.cs, kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little);
            if ctx.is_null() {
                return None;
            }
            let m = pt(r_px as f32, r_px as f32);
            CGContextDrawRadialGradient(ctx, grad.as_ptr() as CGGradientRef, m, 0.0, m, r_px as f64, 0);
            let img = CGBitmapContextCreateImage(ctx);
            CGContextRelease(ctx);
            Cf::new(img as CFTypeRef)?
        };
        let mut g = self.glows.borrow_mut();
        if g.len() > 16 {
            g.clear();
        }
        g.insert(key, img);
        Some(())
    }

    /// A 1 x `h_px` image of the gradient from `top` to `bottom`, cached.
    fn strip(&self, top: Color, bottom: Color, h_px: usize) -> Option<std::cell::Ref<'_, Cf>> {
        let b = |c: Color| [c.0.to_bits(), c.1.to_bits(), c.2.to_bits(), c.3.to_bits()];
        let (t, bo) = (b(top), b(bottom));
        let key = ([t[0], t[1], t[2], t[3], bo[0], bo[1], bo[2], bo[3]], h_px);
        if !self.strips.borrow().contains_key(&key) {
            let grad = self.gradient_obj(&[(0.0, top), (1.0, bottom)])?;
            let img = unsafe {
                let ctx = CGBitmapContextCreate(std::ptr::null_mut(), 1, h_px, 8, 0, self.cs, kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little);
                if ctx.is_null() {
                    return None;
                }
                // Bitmap space is y-up, the frame's is y-down, and an image
                // drawn under the frame's flip lands upside down: `top` at
                // y = 0 ends up at the top of the shape.
                CGContextDrawLinearGradient(ctx, grad.as_ptr() as CGGradientRef, pt(0.0, 0.0), pt(0.0, h_px as f32), kCGGradientDrawsBeforeStartLocation | kCGGradientDrawsAfterEndLocation);
                let img = CGBitmapContextCreateImage(ctx);
                CGContextRelease(ctx);
                Cf::new(img as CFTypeRef)?
            };
            let mut s = self.strips.borrow_mut();
            if s.len() > 512 {
                s.clear();
            }
            s.insert(key, img);
        }
        std::cell::Ref::filter_map(self.strips.borrow(), |s| s.get(&key)).ok()
    }

    /// Horizontal gradient fill.
    pub fn hgradient(&self, r: Rect, radius: f32, left: Color, right: Color) {
        self.linear(r, radius, pt(r.x, r.y), pt(r.right(), r.y), left, right);
    }

    /// Radial glow centred at (cx, cy), fading to transparent.
    ///
    /// Large ones (the page header's) are drawn from a cached image: shading
    /// a 520-point radial gradient on every frame cost a third of it.
    pub fn glow(&self, cx: f32, cy: f32, radius: f32, c: Color) {
        let r_px = (radius as f64 * self.scale()).ceil() as usize;
        if r_px >= 128 && self.glow_image(c, r_px).is_some() {
            let img = self.glows.borrow().get(&(color_bits(c), r_px)).map(|i| i.as_ptr());
            if let Some(img) = img {
                unsafe {
                    CGContextSaveGState(self.ctx);
                    CGContextSetInterpolationQuality(self.ctx, 1);
                    // Whole device pixels at 1:1, so the draw is a plain blend
                    // rather than a resample.
                    let s = self.scale() as f32;
                    let side = (r_px * 2) as f32 / s;
                    let (x, y) = (((cx * s).round() - r_px as f32) / s, ((cy * s).round() - r_px as f32) / s);
                    CGContextDrawImage(self.ctx, cg(Rect::new(x, y, side, side)), img as CGImageRef);
                    CGContextRestoreGState(self.ctx);
                }
                return;
            }
        }
        let Some(grad) = self.gradient_obj(&[(0.0, c), (1.0, c.alpha(0.0))]) else { return };
        unsafe {
            CGContextDrawRadialGradient(self.ctx, grad.as_ptr() as CGGradientRef, pt(cx, cy), 0.0, pt(cx, cy), radius as f64, 0);
        }
    }

    /// Soft drop shadow under a rounded rect (stacked translucent layers).
    ///
    /// The shape drawn on top is opaque, so only the ring around it shows.
    /// The ring is rendered once per radius into nine-slice pieces and
    /// stitched around each card: tracing four translucent layers under every
    /// card cost most of a frame at 2x.
    pub fn shadow(&self, r: Rect, radius: f32, strength: f32) {
        let s = self.scale() as f32;
        let key = [radius.to_bits(), strength.to_bits(), s.to_bits()];
        if !self.shadows.borrow().contains_key(&key) {
            let slices = render_shadow(self.cs, radius, strength, s);
            self.shadows.borrow_mut().insert(key, slices);
        }
        let shadows = self.shadows.borrow();
        let Some(Some(sl)) = shadows.get(&key) else { return self.shadow_rings(r, radius, strength) };
        let (m, cs, bm) = (sl.m_px as f32 / s, sl.cs_px as f32 / s, sl.bm_px as f32 / s);
        if r.w < 2.0 * cs + 1.0 || r.h < 2.0 * cs + 1.0 {
            return self.shadow_rings(r, radius, strength);
        }
        let xs = [r.x - m, r.x + cs, r.right() - cs, r.right() + m];
        let ys = [r.y, r.y + cs, r.bottom() - cs, r.bottom() + bm];
        unsafe {
            CGContextSaveGState(self.ctx);
            CGContextSetInterpolationQuality(self.ctx, 1);
            for (col, row, img) in &sl.pieces {
                let d = Rect::new(xs[*col], ys[*row], xs[col + 1] - xs[*col], ys[row + 1] - ys[*row]);
                // The pieces are cut from a y-down bitmap; the frame's flip
                // would draw them upside down, so flip back about the piece.
                CGContextSaveGState(self.ctx);
                CGContextTranslateCTM(self.ctx, 0.0, (d.y + d.bottom()) as f64);
                CGContextScaleCTM(self.ctx, 1.0, -1.0);
                CGContextDrawImage(self.ctx, cg(d), img.as_ptr() as CGImageRef);
                CGContextRestoreGState(self.ctx);
            }
            CGContextRestoreGState(self.ctx);
        }
    }

    /// The shadow traced directly, for shapes too small for the slices.
    fn shadow_rings(&self, r: Rect, radius: f32, strength: f32) {
        shadow_layers(self.ctx, r, radius, strength);
    }

    /// Rounded-rect outline with a vertical gradient.
    pub fn stroke_vgradient(&self, r: Rect, radius: f32, top: Color, bottom: Color, width: f32) {
        let Some(path) = path_rounded(r, radius) else { return };
        let Some(grad) = self.gradient_obj(&[(0.0, top), (1.0, bottom)]) else { return };
        unsafe {
            CGContextSaveGState(self.ctx);
            CGContextSetLineWidth(self.ctx, width as f64);
            CGContextBeginPath(self.ctx);
            CGContextAddPath(self.ctx, path.as_ptr());
            CGContextReplacePathWithStrokedPath(self.ctx);
            CGContextClip(self.ctx);
            CGContextDrawLinearGradient(
                self.ctx,
                grad.as_ptr() as CGGradientRef,
                pt(r.x, r.y - width),
                pt(r.x, r.bottom() + width),
                kCGGradientDrawsBeforeStartLocation | kCGGradientDrawsAfterEndLocation,
            );
            CGContextRestoreGState(self.ctx);
        }
    }

    pub fn clear(&self, c: Color) {
        let s = self.scale();
        let all = CGRect::new(-1.0, -1.0, self.w_px as f64 / s + 2.0, self.h_px as f64 / s + 2.0);
        unsafe {
            CGContextClearRect(self.ctx, all);
            if c.3 > 0.0 {
                self.set_fill(c);
                CGContextFillRect(self.ctx, all);
            }
        }
    }

    pub fn fill(&self, r: Rect, radius: f32, c: Color) {
        if r.w <= 0.0 || r.h <= 0.0 {
            return;
        }
        self.set_fill(c);
        unsafe {
            if radius > 0.0 {
                if let Some(p) = path_rounded(r, radius) {
                    CGContextBeginPath(self.ctx);
                    CGContextAddPath(self.ctx, p.as_ptr());
                    CGContextFillPath(self.ctx);
                }
            } else {
                CGContextFillRect(self.ctx, cg(r));
            }
        }
    }

    pub fn stroke(&self, r: Rect, radius: f32, c: Color, width: f32) {
        let r = r.inset(width / 2.0, width / 2.0);
        let Some(p) = path_rounded(r, radius) else { return };
        self.set_stroke(c);
        unsafe {
            CGContextSetLineWidth(self.ctx, width as f64);
            CGContextBeginPath(self.ctx);
            CGContextAddPath(self.ctx, p.as_ptr());
            CGContextStrokePath(self.ctx);
        }
    }

    pub fn circle(&self, cx: f32, cy: f32, r: f32, c: Color) {
        self.set_fill(c);
        unsafe { CGContextFillEllipseInRect(self.ctx, cg(Rect::new(cx - r, cy - r, 2.0 * r, 2.0 * r))) };
    }

    pub fn ring(&self, cx: f32, cy: f32, r: f32, c: Color, width: f32) {
        self.set_stroke(c);
        unsafe {
            CGContextSetLineWidth(self.ctx, width as f64);
            CGContextStrokeEllipseInRect(self.ctx, cg(Rect::new(cx - r, cy - r, 2.0 * r, 2.0 * r)));
        }
    }

    pub fn line(&self, x0: f32, y0: f32, x1: f32, y1: f32, c: Color, width: f32) {
        self.set_stroke(c);
        unsafe {
            CGContextSetLineWidth(self.ctx, width as f64);
            CGContextBeginPath(self.ctx);
            CGContextMoveToPoint(self.ctx, x0 as f64, y0 as f64);
            CGContextAddLineToPoint(self.ctx, x1 as f64, y1 as f64);
            CGContextStrokePath(self.ctx);
        }
    }

    /// Clips to `r` and fades content out over `top` DIPs at the top and
    /// `bottom` DIPs at the bottom (a transparency layer masked on pop).
    pub fn push_fade(&mut self, r: Rect, top: f32, bottom: f32) {
        unsafe {
            CGContextSaveGState(self.ctx);
            CGContextClipToRect(self.ctx, cg(r));
            CGContextBeginTransparencyLayerWithRect(self.ctx, cg(r), std::ptr::null());
        }
        self.fades.push((r, top, bottom));
    }

    pub fn pop_fade(&mut self) {
        let Some((r, top, bottom)) = self.fades.pop() else { return };
        let h = r.h.max(1.0);
        let stops = [
            (0.0, Color::white(if top > 0.0 { 0.0 } else { 1.0 })),
            ((top / h).clamp(0.0, 1.0), Color::white(1.0)),
            ((1.0 - bottom / h).clamp(0.0, 1.0), Color::white(1.0)),
            (1.0, Color::white(if bottom > 0.0 { 0.0 } else { 1.0 })),
        ];
        unsafe {
            if (top > 0.0 || bottom > 0.0)
                && let Some(grad) = self.gradient_obj(&stops)
            {
                CGContextSetBlendMode(self.ctx, kCGBlendModeDestinationIn);
                CGContextDrawLinearGradient(self.ctx, grad.as_ptr() as CGGradientRef, pt(r.x, r.y), pt(r.x, r.bottom()), 0);
                CGContextSetBlendMode(self.ctx, kCGBlendModeNormal);
            }
            CGContextEndTransparencyLayer(self.ctx);
            CGContextRestoreGState(self.ctx);
        }
    }

    pub fn push_clip(&self, r: Rect) {
        unsafe {
            CGContextSaveGState(self.ctx);
            CGContextClipToRect(self.ctx, cg(r));
        }
    }

    pub fn pop_clip(&self) {
        unsafe { CGContextRestoreGState(self.ctx) };
    }

    // ------------------------------------------------------------- text --

    fn refresh_fonts(&mut self) {
        let generation = FONT_GEN.load(std::sync::atomic::Ordering::Relaxed);
        if generation == self.font_gen {
            return;
        }
        self.font_gen = generation;
        self.fonts.clear();
        let spec = FONT_SPEC.lock().unwrap().clone();
        self.custom = resolve_custom(&spec);
    }

    /// The CTFont for `font` (borrowed from the cache).
    fn ct_font(&mut self, font: Font) -> CTFontRef {
        self.refresh_fonts();
        let key = FontKey { size10: (font.size * 10.0) as u32, weight: font.weight, family: font.family };
        if let Some(f) = self.fonts.get(&key) {
            return f.as_ptr();
        }
        let size = font.size as f64;
        let f = match &self.custom {
            Some(Custom::File(desc)) => unsafe {
                let base = CTFontCreateWithFontDescriptor(desc.as_ptr(), size, std::ptr::null());
                if font.weight >= 600 && !base.is_null() {
                    let bold = CTFontCreateCopyWithSymbolicTraits(base, size, std::ptr::null(), kCTFontBoldTrait, kCTFontBoldTrait);
                    if bold.is_null() {
                        Cf::new(base)
                    } else {
                        CFRelease(base);
                        Cf::new(bold)
                    }
                } else {
                    Cf::new(base)
                }
            },
            Some(Custom::Family(name)) => family_font(name, size, font.weight),
            None => None,
        };
        let f = f.or_else(|| system_font(size, font.weight)).expect("system font");
        let p = f.as_ptr();
        self.fonts.insert(key, f);
        p
    }

    fn line_height(&mut self, font: Font) -> (f64, f64) {
        let f = self.ct_font(font);
        unsafe {
            let (a, d, l) = (CTFontGetAscent(f), CTFontGetDescent(f), CTFontGetLeading(f));
            (a, (a + d + l).ceil())
        }
    }

    /// Attributed text in `c1`, with UTF-16 units from `split` on in `c2`.
    fn attributed(&mut self, s: &str, font: Font, c1: Color, split: Option<(usize, Color)>) -> Cf {
        let ct = self.ct_font(font);
        unsafe {
            // Text and attributes at once: characters inserted into an empty
            // mutable attributed string have no attribute dictionary, and
            // setting the first attribute on them crashes.
            let text = Cf::str(s);
            let col = Cf(CGColorCreateSRGB(c1.0 as f64, c1.1 as f64, c1.2 as f64, c1.3 as f64) as CFTypeRef);
            let attrs = dict(&[(kCTFontAttributeName, ct), (kCTForegroundColorAttributeName, col.as_ptr())]);
            let base = Cf(CFAttributedStringCreate(std::ptr::null(), text.as_ptr(), attrs.as_ptr()));
            let n = utf16_len(s);
            match split {
                // `split` is usize::MAX for "all in c1": compare unsigned.
                Some((at, c2)) if at < n as usize => {
                    let m = CFAttributedStringCreateMutableCopy(std::ptr::null(), 0, base.as_ptr());
                    let col2 = Cf(CGColorCreateSRGB(c2.0 as f64, c2.1 as f64, c2.2 as f64, c2.3 as f64) as CFTypeRef);
                    let at = at as CFIndex;
                    CFAttributedStringSetAttribute(m, CFRange { location: at, length: n - at }, kCTForegroundColorAttributeName, col2.as_ptr());
                    Cf(m as CFTypeRef)
                }
                _ => base,
            }
        }
    }

    fn lines(&mut self, s: &str, font: Font, c: Color, split: Option<(usize, Color)>, max_w: f32, wrap: bool) -> Vec<Line> {
        let attr = self.attributed(s, font, c, split);
        let mut out = Vec::new();
        unsafe {
            if !wrap {
                let Some(line) = Cf::new(CTLineCreateWithAttributedString(attr.as_ptr())) else { return out };
                let width = CTLineGetTypographicBounds(line.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
                if width > max_w as f64 + 0.5 {
                    let token_attr = self.attributed("…", font, c, None);
                    let token = Cf::new(CTLineCreateWithAttributedString(token_attr.as_ptr()));
                    let truncated = CTLineCreateTruncatedLine(
                        line.as_ptr(),
                        max_w as f64,
                        kCTLineTruncationEnd,
                        token.as_ref().map_or(std::ptr::null(), |t| t.as_ptr()),
                    );
                    if let Some(t) = Cf::new(truncated) {
                        let w = CTLineGetTypographicBounds(t.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
                        out.push(Line { line: t, width: w });
                        return out;
                    }
                }
                out.push(Line { line, width });
                return out;
            }
            let Some(ts) = Cf::new(CTTypesetterCreateWithAttributedString(attr.as_ptr())) else { return out };
            let n = utf16_len(s);
            let mut start: CFIndex = 0;
            while start < n {
                let count = CTTypesetterSuggestLineBreak(ts.as_ptr(), start, max_w.max(1.0) as f64);
                if count <= 0 {
                    break;
                }
                if let Some(line) = Cf::new(CTTypesetterCreateLine(ts.as_ptr(), CFRange { location: start, length: count })) {
                    let width = CTLineGetTypographicBounds(line.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
                    out.push(Line { line, width });
                }
                start += count;
            }
        }
        out
    }

    /// (width, height) of `s`.
    pub fn measure(&mut self, s: &str, font: Font, max_w: f32, wrap: bool) -> (f32, f32) {
        if font.family == Family::Icons {
            return (font.size, font.size);
        }
        let (_, lh) = self.line_height(font);
        if s.is_empty() {
            return (0.0, lh as f32);
        }
        let lines = self.lines(s, font, Color::white(1.0), None, max_w, wrap);
        let w = lines.iter().map(|l| l.width).fold(0.0, f64::max);
        (w as f32, (lines.len().max(1) as f64 * lh) as f32)
    }

    /// Line count of wrapped `s` (the fixed line height does not change it).
    pub fn line_count(&mut self, s: &str, font: Font, max_w: f32, _line_h: f32) -> u32 {
        (self.lines(s, font, Color::white(1.0), None, max_w, true).len() as u32).max(1)
    }

    fn draw_line(&self, l: &Line, x: f64, baseline: f64) {
        unsafe {
            CGContextSetTextPosition(self.ctx, x, baseline);
            CTLineDraw(l.line.as_ptr(), self.ctx);
        }
    }

    /// Wrapped text at a fixed line height: UTF-16 units before `split` in
    /// `c1`, the rest in `c2`.
    #[allow(clippy::too_many_arguments)]
    pub fn text_two(&mut self, s: &str, split: usize, font: Font, r: Rect, line_h: f32, c1: Color, c2: Color) {
        if s.is_empty() {
            return;
        }
        let lines = self.lines(s, font, c1, Some((split, c2)), r.w, true);
        for (i, l) in lines.iter().enumerate() {
            // DirectWrite's uniform spacing puts the baseline at 76%.
            let baseline = r.y as f64 + i as f64 * line_h as f64 + line_h as f64 * 0.76;
            self.draw_line(l, r.x as f64, baseline);
        }
    }

    /// Draws text into `r` (top-aligned); returns the text height.
    pub fn text(&mut self, s: &str, font: Font, r: Rect, c: Color, wrap: bool, align: Align) -> f32 {
        if s.is_empty() {
            return 0.0;
        }
        if font.family == Family::Icons {
            let ch = s.chars().next().unwrap_or(' ');
            self.icon(ch, font, Rect::new(r.x, r.y, r.w, font.size), c, align);
            return font.size;
        }
        let (ascent, lh) = self.line_height(font);
        let lines = self.lines(s, font, c, None, r.w, wrap);
        for (i, l) in lines.iter().enumerate() {
            let x = match align {
                Align::Left => r.x as f64,
                Align::Center => r.x as f64 + (r.w as f64 - l.width) / 2.0,
                Align::Right => r.right() as f64 - l.width,
            };
            self.draw_line(l, x, r.y as f64 + i as f64 * lh + ascent);
        }
        (lines.len() as f64 * lh) as f32
    }

    /// Single line, vertically centred in `r`.
    pub fn label(&mut self, s: &str, font: Font, r: Rect, c: Color, align: Align) {
        if s.is_empty() {
            return;
        }
        if font.family == Family::Icons {
            let ch = s.chars().next().unwrap_or(' ');
            self.icon(ch, font, r, c, align);
            return;
        }
        let (_, h) = self.measure(s, font, r.w, false);
        self.text(s, font, Rect::new(r.x, r.y + (r.h - h) / 2.0, r.w, h), c, false, align);
    }

    /// X offset of the caret before character index `pos` (UTF-16 units).
    pub fn caret_x(&mut self, s: &str, font: Font, pos: usize) -> f32 {
        let attr = self.attributed(s, font, Color::white(1.0), None);
        unsafe {
            let Some(line) = Cf::new(CTLineCreateWithAttributedString(attr.as_ptr())) else { return 0.0 };
            CTLineGetOffsetForStringIndex(line.as_ptr(), pos as CFIndex, std::ptr::null_mut()) as f32
        }
    }

    // ------------------------------------------------------------ icons --

    fn symbol(&mut self, ch: char, size: f32, weight: u16) -> Option<Id> {
        let key = (ch, (size * 10.0) as u32, weight);
        if let Some(i) = self.icons.get(&key) {
            return i.clone();
        }
        let img = objc2::rc::autoreleasepool(|_| unsafe {
            let name = symbol_name(ch)?;
            let base: Option<Id> =
                msg_send![class!(NSImage), imageWithSystemSymbolName: &*nsstring(name), accessibilityDescription: std::ptr::null::<AnyObject>()];
            let base = base?;
            let w = if weight >= 600 { 0.3f64 } else { 0.0 };
            let conf: Id = msg_send![class!(NSImageSymbolConfiguration), configurationWithPointSize: size as f64 * 0.92, weight: w];
            let img: Option<Id> = msg_send![&*base, imageWithSymbolConfiguration: &*conf];
            img
        });
        self.icons.insert(key, img.clone());
        img
    }

    fn ns_context(&mut self) -> Option<Id> {
        if self.ns_ctx.is_none() {
            self.ns_ctx = unsafe { msg_send![class!(NSGraphicsContext), graphicsContextWithCGContext: self.ctx as *mut AnyObject, flipped: true] };
        }
        self.ns_ctx.clone()
    }

    /// An SF Symbol tinted with `c`, at its natural aspect, aligned in `r`
    /// and vertically centred.
    fn icon(&mut self, ch: char, font: Font, r: Rect, c: Color, align: Align) {
        let Some(img) = self.symbol(ch, font.size, font.weight) else { return };
        let Some(gc) = self.ns_context() else { return };
        let size: NSSize = unsafe { msg_send![&*img, size] };
        let (w, h) = (size.width as f32, size.height as f32);
        let x = match align {
            Align::Left => r.x,
            Align::Center => r.x + (r.w - w) / 2.0,
            Align::Right => r.right() - w,
        };
        let dst = Rect::new(x, r.y + (r.h - h) / 2.0, w, h);
        let rect = NSRect::new(objc2_foundation::NSPoint::new(dst.x as f64, dst.y as f64), NSSize::new(w as f64, h as f64));
        objc2::rc::autoreleasepool(|_| unsafe {
            CGContextSaveGState(self.ctx);
            CGContextBeginTransparencyLayerWithRect(self.ctx, cg(dst.inset(-2.0, -2.0)), std::ptr::null());
            let _: () = msg_send![class!(NSGraphicsContext), saveGraphicsState];
            let _: () = msg_send![class!(NSGraphicsContext), setCurrentContext: &*gc];
            let zero = NSRect::new(objc2_foundation::NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
            // NSCompositingOperationSourceOver = 2
            let _: () = msg_send![&*img, drawInRect: rect, fromRect: zero, operation: 2usize, fraction: 1.0f64, respectFlipped: true, hints: std::ptr::null::<AnyObject>()];
            let _: () = msg_send![class!(NSGraphicsContext), restoreGraphicsState];
            CGContextSetBlendMode(self.ctx, kCGBlendModeSourceIn);
            self.set_fill(c);
            CGContextFillRect(self.ctx, cg(dst.inset(-2.0, -2.0)));
            CGContextEndTransparencyLayer(self.ctx);
            CGContextRestoreGState(self.ctx);
        });
    }
}

fn ns_weight(weight: u16) -> f64 {
    match weight {
        0..=450 => 0.0,
        451..=550 => 0.23,
        551..=650 => 0.3,
        _ => 0.4,
    }
}

fn system_font(size: f64, weight: u16) -> Option<Cf> {
    objc2::rc::autoreleasepool(|_| unsafe {
        let f: Option<Id> = msg_send![class!(NSFont), systemFontOfSize: size, weight: ns_weight(weight)];
        f.map(|f| Cf(CFRetain(&*f as *const AnyObject as CFTypeRef)))
    })
}

fn family_font(family: &str, size: f64, weight: u16) -> Option<Cf> {
    // NSFontManager weights: 5 regular, 6 medium, 8 semibold, 9 bold.
    let w: isize = match weight {
        0..=450 => 5,
        451..=550 => 6,
        551..=650 => 8,
        _ => 9,
    };
    objc2::rc::autoreleasepool(|_| unsafe {
        let fm: Id = msg_send![class!(NSFontManager), sharedFontManager];
        let f: Option<Id> = msg_send![&*fm, fontWithFamily: &*nsstring(family), traits: 0usize, weight: w, size: size];
        f.map(|f| Cf(CFRetain(&*f as *const AnyObject as CFTypeRef)))
    })
}

fn resolve_custom(spec: &str) -> Option<Custom> {
    if spec.is_empty() {
        return None;
    }
    let path = std::path::Path::new(spec);
    let is_file = path.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_lowercase().as_str(), "ttf" | "otf" | "ttc"));
    if !is_file {
        if family_font(spec, 13.0, 400).is_none() {
            log::warn!("font {spec}: not installed");
            return None;
        }
        return Some(Custom::Family(spec.to_string()));
    }
    let url = file_url(path);
    unsafe {
        let descs = CTFontManagerCreateFontDescriptorsFromURL(url.as_ptr());
        let Some(descs) = Cf::new(descs) else {
            log::warn!("font {spec}: cannot read");
            return None;
        };
        if CFArrayGetCount(descs.as_ptr()) == 0 {
            log::warn!("font {spec}: no fonts in file");
            return None;
        }
        let d = CFArrayGetValueAtIndex(descs.as_ptr(), 0);
        Some(Custom::File(Cf(CFRetain(d))))
    }
}
