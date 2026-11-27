//! Direct2D + DirectWrite drawing for the main window. Coordinates are DIPs;
//! the render target's DPI maps them to pixels.

use std::collections::HashMap;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::core::{HSTRING, Interface, w};
use windows_numerics::Vector2;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Rect { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn contains(&self, p: (f32, f32)) -> bool {
        p.0 >= self.x && p.0 < self.right() && p.1 >= self.y && p.1 < self.bottom()
    }
    pub fn inset(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, (self.w - 2.0 * dx).max(0.0), (self.h - 2.0 * dy).max(0.0))
    }
    fn d2d(&self) -> D2D_RECT_F {
        D2D_RECT_F { left: self.x, top: self.y, right: self.right(), bottom: self.bottom() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color(pub f32, pub f32, pub f32, pub f32);

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0)
    }
    pub const fn white(a: f32) -> Color {
        Color(1.0, 1.0, 1.0, a)
    }
    pub fn alpha(self, a: f32) -> Color {
        Color(self.0, self.1, self.2, self.3 * a)
    }
    pub fn mix(self, o: Color, t: f32) -> Color {
        Color(self.0 + (o.0 - self.0) * t, self.1 + (o.1 - self.1) * t, self.2 + (o.2 - self.2) * t, self.3 + (o.3 - self.3) * t)
    }
    fn d2d(self) -> D2D1_COLOR_F {
        D2D1_COLOR_F { r: self.0, g: self.1, b: self.2, a: self.3 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    Text,
    Display,
    Icons,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Font {
    pub size: f32,
    pub weight: u16,
    pub family: Family,
}

impl Font {
    pub const fn text(size: f32) -> Font {
        Font { size, weight: 400, family: Family::Text }
    }
    pub const fn semibold(size: f32) -> Font {
        Font { size, weight: 600, family: Family::Text }
    }
    pub const fn display(size: f32) -> Font {
        Font { size, weight: 600, family: Family::Display }
    }
    pub const fn icon(size: f32) -> Font {
        Font { size, weight: 400, family: Family::Icons }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FormatKey {
    size10: u32,
    weight: u16,
    family: Family,
    wrap: bool,
    align: Align,
}

pub struct Gfx {
    pub rt: ID2D1RenderTarget,
    hwnd_rt: Option<ID2D1HwndRenderTarget>,
    brush: ID2D1SolidColorBrush,
    dw: IDWriteFactory,
    formats: HashMap<FormatKey, IDWriteTextFormat>,
    icon_family: HSTRING,
    pub dpi: f32,
}

fn text_family_name(f: Family, icons: &HSTRING) -> HSTRING {
    match f {
        Family::Text => HSTRING::from("Segoe UI Variable Text"),
        Family::Display => HSTRING::from("Segoe UI Variable Display"),
        Family::Icons => icons.clone(),
    }
}

impl Gfx {
    pub fn new(hwnd: HWND, width: u32, height: u32, dpi: f32) -> windows::core::Result<Gfx> {
        unsafe {
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: dpi,
                dpiY: dpi,
                ..Default::default()
            };
            let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: D2D_SIZE_U { width: width.max(1), height: height.max(1) },
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            };
            let hwnd_rt = factory.CreateHwndRenderTarget(&props, &hwnd_props)?;
            let rt: ID2D1RenderTarget = hwnd_rt.cast()?;
            let mut g = Gfx::from_target(rt, dpi)?;
            g.hwnd_rt = Some(hwnd_rt);
            Ok(g)
        }
    }

    /// Offscreen: a WIC bitmap target for `chatty --render`.
    pub fn from_target(rt: ID2D1RenderTarget, dpi: f32) -> windows::core::Result<Gfx> {
        unsafe {
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE);
            let brush = rt.CreateSolidColorBrush(&Color::white(1.0).d2d(), None)?;
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            // Segoe Fluent Icons ships with Windows 11; MDL2 with Windows 10.
            let mut icon_family = HSTRING::from("Segoe Fluent Icons");
            let mut collection: Option<IDWriteFontCollection> = None;
            if dw.GetSystemFontCollection(&mut collection, false).is_ok() {
                if let Some(collection) = collection {
                    let mut index = 0u32;
                    let mut exists = windows::core::BOOL(0);
                    let _ = collection.FindFamilyName(w!("Segoe Fluent Icons"), &mut index, &mut exists);
                    if !exists.as_bool() {
                        icon_family = HSTRING::from("Segoe MDL2 Assets");
                    }
                }
            }
            Ok(Gfx { rt, hwnd_rt: None, brush, dw, formats: HashMap::new(), icon_family, dpi })
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if let Some(h) = &self.hwnd_rt {
            unsafe {
                let _ = h.Resize(&D2D_SIZE_U { width: width.max(1), height: height.max(1) });
            }
        }
    }

    /// Vertical gradient fill.
    pub fn gradient(&self, r: Rect, radius: f32, top: Color, bottom: Color) {
        unsafe {
            let stops = [
                D2D1_GRADIENT_STOP { position: 0.0, color: top.d2d() },
                D2D1_GRADIENT_STOP { position: 1.0, color: bottom.d2d() },
            ];
            let Ok(coll) = self.rt.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP) else { return };
            let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                startPoint: Vector2 { X: r.x, Y: r.y },
                endPoint: Vector2 { X: r.x, Y: r.bottom() },
            };
            let Ok(b) = self.rt.CreateLinearGradientBrush(&props, None, &coll) else { return };
            if radius > 0.0 {
                self.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius }, &b);
            } else {
                self.rt.FillRectangle(&r.d2d(), &b);
            }
        }
    }

    /// Radial glow centred at (cx, cy), fading to transparent.
    pub fn glow(&self, cx: f32, cy: f32, radius: f32, c: Color) {
        unsafe {
            let stops = [
                D2D1_GRADIENT_STOP { position: 0.0, color: c.d2d() },
                D2D1_GRADIENT_STOP { position: 1.0, color: c.alpha(0.0).d2d() },
            ];
            let Ok(coll) = self.rt.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP) else { return };
            let props = D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES {
                center: Vector2 { X: cx, Y: cy },
                gradientOriginOffset: Vector2 { X: 0.0, Y: 0.0 },
                radiusX: radius,
                radiusY: radius,
            };
            let Ok(b) = self.rt.CreateRadialGradientBrush(&props, None, &coll) else { return };
            self.rt.FillEllipse(&D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: radius, radiusY: radius }, &b);
        }
    }

    /// Soft drop shadow under a rounded rect (stacked translucent layers).
    pub fn shadow(&self, r: Rect, radius: f32, strength: f32) {
        for i in 1..=4 {
            let d = i as f32 * 2.0;
            let rr = Rect::new(r.x - d * 0.5, r.y + d * 0.6, r.w + d, r.h + d);
            self.fill(rr, radius + d * 0.5, Color(0.0, 0.0, 0.0, strength / (i as f32 + 1.0)));
        }
    }

    pub fn set_dpi(&mut self, dpi: f32) {
        self.dpi = dpi;
        unsafe { self.rt.SetDpi(dpi, dpi) };
    }

    fn format(&mut self, font: Font, wrap: bool, align: Align) -> IDWriteTextFormat {
        let key = FormatKey { size10: (font.size * 10.0) as u32, weight: font.weight, family: font.family, wrap, align };
        if let Some(f) = self.formats.get(&key) {
            return f.clone();
        }
        unsafe {
            let family = text_family_name(font.family, &self.icon_family);
            let f = self
                .dw
                .CreateTextFormat(
                    &family,
                    None,
                    DWRITE_FONT_WEIGHT(font.weight as i32),
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    font.size,
                    w!("en-us"),
                )
                .expect("text format");
            let _ = f.SetWordWrapping(if wrap { DWRITE_WORD_WRAPPING_WRAP } else { DWRITE_WORD_WRAPPING_NO_WRAP });
            let _ = f.SetTextAlignment(match align {
                Align::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
                Align::Right => DWRITE_TEXT_ALIGNMENT_TRAILING,
            });
            if !wrap {
                if let Ok(sign) = self.dw.CreateEllipsisTrimmingSign(&f) {
                    let trimming = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
                    let _ = f.SetTrimming(&trimming, &sign);
                }
            }
            self.formats.insert(key, f.clone());
            f
        }
    }

    pub fn layout(&mut self, s: &str, font: Font, max_w: f32, wrap: bool, align: Align) -> IDWriteTextLayout {
        let format = self.format(font, wrap, align);
        let wide: Vec<u16> = s.encode_utf16().collect();
        unsafe { self.dw.CreateTextLayout(&wide, &format, max_w.max(1.0), 10_000.0).expect("text layout") }
    }

    /// (width, height) of `s`.
    pub fn measure(&mut self, s: &str, font: Font, max_w: f32, wrap: bool) -> (f32, f32) {
        let l = self.layout(s, font, max_w, wrap, Align::Left);
        let mut m = DWRITE_TEXT_METRICS::default();
        unsafe {
            let _ = l.GetMetrics(&mut m);
        }
        (m.widthIncludingTrailingWhitespace, m.height)
    }

    /// (line count, height) of wrapped `s`.
    pub fn wrapped(&mut self, s: &str, font: Font, max_w: f32) -> (u32, f32) {
        let l = self.layout(s, font, max_w, true, Align::Left);
        let mut m = DWRITE_TEXT_METRICS::default();
        unsafe {
            let _ = l.GetMetrics(&mut m);
        }
        (m.lineCount, m.height)
    }

    /// Horizontal gradient fill.
    pub fn hgradient(&self, r: Rect, radius: f32, left: Color, right: Color) {
        unsafe {
            let stops = [
                D2D1_GRADIENT_STOP { position: 0.0, color: left.d2d() },
                D2D1_GRADIENT_STOP { position: 1.0, color: right.d2d() },
            ];
            let Ok(coll) = self.rt.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP) else { return };
            let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                startPoint: Vector2 { X: r.x, Y: r.y },
                endPoint: Vector2 { X: r.right(), Y: r.y },
            };
            let Ok(b) = self.rt.CreateLinearGradientBrush(&props, None, &coll) else { return };
            if radius > 0.0 {
                self.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius }, &b);
            } else {
                self.rt.FillRectangle(&r.d2d(), &b);
            }
        }
    }

    fn set(&self, c: Color) {
        unsafe { self.brush.SetColor(&c.d2d()) };
    }

    pub fn clear(&self, c: Color) {
        unsafe { self.rt.Clear(Some(&c.d2d())) };
    }

    pub fn fill(&self, r: Rect, radius: f32, c: Color) {
        self.set(c);
        unsafe {
            if radius > 0.0 {
                self.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius }, &self.brush);
            } else {
                self.rt.FillRectangle(&r.d2d(), &self.brush);
            }
        }
    }

    pub fn stroke(&self, r: Rect, radius: f32, c: Color, width: f32) {
        self.set(c);
        let r = r.inset(width / 2.0, width / 2.0);
        unsafe {
            self.rt.DrawRoundedRectangle(
                &D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius },
                &self.brush,
                width,
                None,
            );
        }
    }

    pub fn circle(&self, cx: f32, cy: f32, r: f32, c: Color) {
        self.set(c);
        unsafe {
            self.rt.FillEllipse(&D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: r, radiusY: r }, &self.brush);
        }
    }

    pub fn ring(&self, cx: f32, cy: f32, r: f32, c: Color, width: f32) {
        self.set(c);
        unsafe {
            self.rt.DrawEllipse(&D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: r, radiusY: r }, &self.brush, width, None);
        }
    }

    pub fn line(&self, x0: f32, y0: f32, x1: f32, y1: f32, c: Color, width: f32) {
        self.set(c);
        unsafe {
            self.rt.DrawLine(Vector2 { X: x0, Y: y0 }, Vector2 { X: x1, Y: y1 }, &self.brush, width, None);
        }
    }

    /// Draws text into `r` (top-aligned); returns the text height.
    pub fn text(&mut self, s: &str, font: Font, r: Rect, c: Color, wrap: bool, align: Align) -> f32 {
        if s.is_empty() {
            return 0.0;
        }
        let l = self.layout(s, font, r.w, wrap, align);
        let mut m = DWRITE_TEXT_METRICS::default();
        unsafe {
            let _ = l.GetMetrics(&mut m);
            self.set(c);
            self.rt.DrawTextLayout(
                Vector2 { X: r.x, Y: r.y },
                &l,
                &self.brush,
                D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
            );
        }
        m.height
    }

    /// Single line, vertically centred in `r`.
    pub fn label(&mut self, s: &str, font: Font, r: Rect, c: Color, align: Align) {
        if s.is_empty() {
            return;
        }
        let (_, h) = self.measure(s, font, r.w, false);
        self.text(s, font, Rect::new(r.x, r.y + (r.h - h) / 2.0, r.w, h), c, false, align);
    }

    /// X offset of the caret before character index `pos` (UTF-16 units).
    pub fn caret_x(&mut self, s: &str, font: Font, pos: usize) -> f32 {
        let l = self.layout(s, font, 10_000.0, false, Align::Left);
        let (mut x, mut y) = (0.0f32, 0.0f32);
        let mut m = DWRITE_HIT_TEST_METRICS::default();
        unsafe {
            let _ = l.HitTestTextPosition(pos as u32, false, &mut x, &mut y, &mut m);
        }
        x
    }

    /// Clips to `r` and fades content out over `top` DIPs at the top and
    /// `bottom` DIPs at the bottom (an opacity-mask layer). Pair with
    /// `pop_fade`.
    pub fn push_fade(&self, r: Rect, top: f32, bottom: f32) {
        unsafe {
            let h = r.h.max(1.0);
            let stops = [
                D2D1_GRADIENT_STOP { position: 0.0, color: Color::white(if top > 0.0 { 0.0 } else { 1.0 }).d2d() },
                D2D1_GRADIENT_STOP { position: (top / h).clamp(0.0, 1.0), color: Color::white(1.0).d2d() },
                D2D1_GRADIENT_STOP { position: (1.0 - bottom / h).clamp(0.0, 1.0), color: Color::white(1.0).d2d() },
                D2D1_GRADIENT_STOP { position: 1.0, color: Color::white(if bottom > 0.0 { 0.0 } else { 1.0 }).d2d() },
            ];
            let brush = self
                .rt
                .CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)
                .and_then(|coll| {
                    let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                        startPoint: Vector2 { X: r.x, Y: r.y },
                        endPoint: Vector2 { X: r.x, Y: r.bottom() },
                    };
                    self.rt.CreateLinearGradientBrush(&props, None, &coll)
                })
                .ok()
                .and_then(|b| b.cast::<ID2D1Brush>().ok());
            let Ok(layer) = self.rt.CreateLayer(None) else {
                self.push_clip(r);
                return;
            };
            let params = D2D1_LAYER_PARAMETERS {
                contentBounds: r.d2d(),
                geometricMask: std::mem::ManuallyDrop::new(None),
                maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                maskTransform: windows_numerics::Matrix3x2::identity(),
                opacity: 1.0,
                opacityBrush: std::mem::ManuallyDrop::new(brush),
                layerOptions: D2D1_LAYER_OPTIONS_NONE,
            };
            self.rt.PushLayer(&params, &layer);
            // The layer holds its own references.
            let _ = std::mem::ManuallyDrop::into_inner(params.opacityBrush);
        }
    }

    pub fn pop_fade(&self) {
        unsafe { self.rt.PopLayer() };
    }

    pub fn push_clip(&self, r: Rect) {
        unsafe { self.rt.PushAxisAlignedClip(&r.d2d(), D2D1_ANTIALIAS_MODE_ALIASED) };
    }

    pub fn pop_clip(&self) {
        unsafe { self.rt.PopAxisAlignedClip() };
    }

    pub fn begin(&self) {
        unsafe { self.rt.BeginDraw() };
    }

    /// False when the device was lost and the target must be recreated.
    pub fn end(&self) -> bool {
        match unsafe { self.rt.EndDraw(None, None) } {
            Ok(()) => true,
            // D2DERR_RECREATE_TARGET
            Err(e) => e.code() != windows::core::HRESULT(0x8899_000C_u32 as i32),
        }
    }
}
