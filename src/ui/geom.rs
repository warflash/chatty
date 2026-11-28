//! Geometry, colors and font descriptions shared by both renderers
//! (Direct2D on Windows, CoreGraphics on macOS). Coordinates are DIPs.

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

