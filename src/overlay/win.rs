//! Windows host of the overlay: a layered, non-activating window updated
//! with UpdateLayeredWindow from a Direct2D DC render target.

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Instant;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT};
use windows::Win32::Graphics::Direct2D::{
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, ID2D1RenderTarget,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, w};


use super::*;

const WM_UPDATE: u32 = WM_APP + 1;
const TIMER_HIDE: usize = 1;
const TIMER_ANIM: usize = 2;
const FRAME_MS: u32 = 33;

static HWND_RAW: AtomicIsize = AtomicIsize::new(0);

pub(super) fn post() {
    let hwnd = HWND_RAW.load(Ordering::Relaxed);
    if hwnd != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_UPDATE, WPARAM(0), LPARAM(0));
        }
    }
}

// --------------------------------------------------------------- renderer --

struct Surface {
    rt: ID2D1DCRenderTarget,
    g: Gfx,
    mem: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    size: (i32, i32),
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.mem, self.old);
            let _ = DeleteObject(HGDIOBJ(self.bmp.0));
            let _ = DeleteDC(self.mem);
        }
    }
}

struct Anim {
    surface: Option<Surface>,
    visible: bool,
    shown_at: Instant,
    hiding_since: Option<Instant>,
    last: Option<Frame>,
    level: f32,
    timer: bool,
    /// Displayed geometry, easing toward the layout.
    geo: Option<Geo>,
    /// While recording the card only grows (no jumping as words change).
    min_w: f32,
    min_h: f32,
}

thread_local! {
    static ANIM: RefCell<Anim> = RefCell::new(Anim {
        surface: None,
        visible: false,
        shown_at: Instant::now(),
        hiding_since: None,
        last: None,
        level: 0.0,
        timer: false,
        geo: None,
        min_w: 0.0,
        min_h: 0.0,
    });
}

fn dc_target(dpi: f32) -> windows::core::Result<ID2D1DCRenderTarget> {
    unsafe {
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: dpi,
            dpiY: dpi,
            ..Default::default()
        };
        factory.CreateDCRenderTarget(&props)
    }
}

fn surface(anim: &mut Anim, w: i32, h: i32, dpi: f32) -> Option<&mut Surface> {
    let fits = anim.surface.as_ref().is_some_and(|s| s.size == (w, h));
    if !fits {
        let keep = anim.surface.take();
        let rt = match keep {
            Some(s) => s.rt.clone(),
            None => dc_target(dpi).ok()?,
        };
        unsafe {
            let mem = CreateCompatibleDC(None);
            let bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bmp = CreateDIBSection(Some(mem), &bi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            let old = SelectObject(mem, HGDIOBJ(bmp.0));
            let target: ID2D1RenderTarget = rt.cast().ok()?;
            let g = Gfx::from_target(target, dpi).ok()?;
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            anim.surface = Some(Surface { rt, g, mem, bmp, old, bits: bits as *mut u8, size: (w, h) });
        }
    }
    let s = anim.surface.as_mut()?;
    if s.g.dpi != dpi {
        s.g.set_dpi(dpi);
    }
    Some(s)
}

fn set_timer(hwnd: HWND, anim: &mut Anim, on: bool) {
    if on != anim.timer {
        anim.timer = on;
        unsafe {
            if on {
                SetTimer(Some(hwnd), TIMER_ANIM, FRAME_MS, None);
            } else {
                let _ = KillTimer(Some(hwnd), TIMER_ANIM);
            }
        }
    }
}

/// Lays out, draws and shows (or hides) the window. Main thread only.
unsafe fn render(hwnd: HWND) {
    ANIM.with(|cell| {
        let mut anim = cell.borrow_mut();
        let anim = &mut *anim;
        let (mode, live, split, hover) = {
            let v = VIEW.lock().unwrap();
            (v.mode.clone(), v.live.clone(), v.split, v.hover)
        };
        let wanted = match &mode {
            Mode::Live(_) | Mode::Transcribing => ENABLED.load(Ordering::Relaxed),
            Mode::Hidden => false,
            _ => true,
        };
        let now = Instant::now();
        let status = crate::status::get();

        let frame = if wanted {
            if !anim.visible || anim.hiding_since.is_some() {
                anim.shown_at = now;
                anim.hiding_since = None;
            }
            anim.visible = true;
            let target = if matches!(mode, Mode::Live(_)) { status.level } else { 0.0 };
            let k = if target > anim.level { 0.55 } else { 0.18 };
            anim.level += (target - anim.level) * k;
            let elapsed = if status.phase == crate::status::Phase::Recording { status.phase_since.elapsed().as_secs_f32() } else { 0.0 };
            let f = Frame { mode: mode.clone(), live, split, level: anim.level, elapsed, t: (now - anim.shown_at).as_secs_f32(), hover };
            anim.last = Some(f.clone());
            f
        } else {
            if !anim.visible {
                set_timer(hwnd, anim, false);
                return;
            }
            let since = *anim.hiding_since.get_or_insert(now);
            if (now - since).as_secs_f32() >= FADE_OUT || anim.last.is_none() {
                anim.visible = false;
                anim.hiding_since = None;
                anim.last = None;
                anim.level = 0.0;
                anim.geo = None;
                anim.min_w = 0.0;
                anim.min_h = 0.0;
                set_timer(hwnd, anim, false);
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                return;
            }
            let mut f = anim.last.clone().unwrap();
            f.t = (now - anim.shown_at).as_secs_f32();
            f
        };

        // Monitor of the foreground window, its DPI and work area.
        let (dpi, work) = unsafe {
            let fg = GetForegroundWindow();
            let monitor = MonitorFromWindow(if fg.is_invalid() { hwnd } else { fg }, MONITOR_DEFAULTTOPRIMARY);
            let (mut dx, mut dy) = (96u32, 96u32);
            let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
            let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(monitor, &mut mi);
            (dx.max(96) as f32, mi.rcWork)
        };
        let scale = dpi / 96.0;
        // Lay out with whatever surface exists, then size the bitmap.
        if anim.surface.is_none() && surface(anim, 1, 1, dpi).is_none() {
            return;
        }
        let mut target = {
            let g = &mut anim.surface.as_mut().unwrap().g;
            if g.dpi != dpi {
                g.set_dpi(dpi);
            }
            layout(g, &frame)
        };
        let recording = matches!(frame.mode, Mode::Live(_) | Mode::Transcribing);
        if recording && wanted {
            if target.w < anim.min_w {
                // Keep the width; wrap the text to it.
                let g = &mut anim.surface.as_mut().unwrap().g;
                let mut f2 = frame.clone();
                f2.t = 0.0;
                target = layout_at(g, &f2, anim.min_w);
            }
            target.h = target.h.max(anim.min_h);
            anim.min_w = target.w;
            anim.min_h = target.h;
        } else if wanted {
            anim.min_w = 0.0;
            anim.min_h = 0.0;
        }
        // Ease the displayed geometry toward the target.
        let geo = match anim.geo {
            Some(cur) if anim.hiding_since.is_none() => {
                let k = 0.34;
                let step = |a: f32, b: f32| if (b - a).abs() < 0.4 { b } else { a + (b - a) * k };
                Geo { w: step(cur.w, target.w), h: step(cur.h, target.h), scroll: step(cur.scroll, target.scroll), ..target }
            }
            Some(cur) => cur,
            None => target,
        };
        let settling = (geo.w - target.w).abs() > 0.01 || (geo.h - target.h).abs() > 0.01 || (geo.scroll - target.scroll).abs() > 0.01;
        anim.geo = Some(geo);
        let px = |v: f32| (v * scale).round() as i32;
        let (w, h) = (px(MAX_W + 2.0 * MARGIN), px(geo.h + 2.0 * MARGIN));
        let Some(s) = surface(anim, w, h, dpi) else { return };
        let buttons = unsafe {
            let r = RECT { left: 0, top: 0, right: w, bottom: h };
            if s.rt.BindDC(s.mem, &r).is_err() {
                return;
            }
            s.g.begin();
            s.g.clear(Color(0.0, 0.0, 0.0, 0.0));
            let b = draw(&mut s.g, &frame, &geo);
            if !s.g.end() {
                anim.surface = None;
                return;
            }
            b
        };

        // Fade and slide.
        let fade = if let Some(since) = anim.hiding_since {
            1.0 - ease((now - since).as_secs_f32() / FADE_OUT)
        } else {
            ease((now - anim.shown_at).as_secs_f32() / FADE_IN)
        };
        let slide = px(8.0 * (1.0 - fade));
        let x = work.left + (work.right - work.left - w) / 2;
        let y = work.bottom - px(LIFT) - h + px(MARGIN) + slide;
        let s = anim.surface.as_ref().unwrap();
        unsafe {
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: (fade * 255.0).round() as u8,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let screen = GetDC(None);
            let _ = UpdateLayeredWindow(
                hwnd,
                Some(screen),
                Some(&POINT { x, y }),
                Some(&SIZE { cx: w, cy: h }),
                Some(s.mem),
                Some(&POINT { x: 0, y: 0 }),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            ReleaseDC(None, screen);
            if !IsWindowVisible(hwnd).as_bool() {
                let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
            }
            // Clickable only as a card.
            let card = matches!(frame.mode, Mode::Card { .. }) && anim.hiding_since.is_none();
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let want = if card { ex & !(WS_EX_TRANSPARENT.0 as isize) } else { ex | WS_EX_TRANSPARENT.0 as isize };
            if want != ex {
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, want);
            }
        }
        {
            let to_px = |r: Rect| Px { left: px(r.x), top: px(r.y), right: px(r.right()), bottom: px(r.bottom()) };
            let mut v = VIEW.lock().unwrap();
            match buttons {
                Some((copy, close)) => {
                    v.copy_btn = to_px(copy);
                    v.close_btn = to_px(close);
                }
                None => {
                    v.copy_btn = ZERO;
                    v.close_btn = ZERO;
                }
            }
        }
        let moving = matches!(frame.mode, Mode::Live(_) | Mode::Transcribing | Mode::Progress(..))
            || anim.hiding_since.is_some()
            || fade < 1.0
            || settling;
        set_timer(hwnd, anim, moving);
    });
}

/// Hide timers for notices and cards, restarted when the mode changes.
unsafe fn arm_hide(hwnd: HWND) {
    let v = VIEW.lock().unwrap();
    let ms = match &v.mode {
        Mode::Notice(_) => Some(3500),
        Mode::Card { copied: true, .. } => Some(1200),
        Mode::Card { .. } => Some(120_000),
        _ => None,
    };
    drop(v);
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_HIDE);
        if let Some(ms) = ms {
            SetTimer(Some(hwnd), TIMER_HIDE, ms, None);
        }
    }
}

fn set_hover(hwnd: HWND, h: Option<Button>) {
    let changed = {
        let mut v = VIEW.lock().unwrap();
        let c = v.hover != h;
        v.hover = h;
        c
    };
    if changed {
        unsafe { render(hwnd) };
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_UPDATE => {
                arm_hide(hwnd);
                render(hwnd);
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == TIMER_ANIM => {
                render(hwnd);
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == TIMER_HIDE => {
                let _ = KillTimer(Some(hwnd), TIMER_HIDE);
                let mut v = VIEW.lock().unwrap();
                if matches!(v.mode, Mode::Notice(_) | Mode::Card { .. }) {
                    v.mode = Mode::Hidden;
                    drop(v);
                    render(hwnd);
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let x = (lparam.0 & 0xFFFF) as u16 as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
                set_hover(hwnd, hover_at(x, y));
                LRESULT(0)
            }
            0x02A3 /* WM_MOUSELEAVE */ => {
                set_hover(hwnd, None);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let x = (lparam.0 & 0xFFFF) as u16 as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                click(x, y);
                LRESULT(0)
            }
            WM_SETCURSOR => {
                let mut p = POINT::default();
                let _ = GetCursorPos(&mut p);
                let _ = ScreenToClient(hwnd, &mut p);
                let over = hover_at(p.x, p.y).is_some();
                SetCursor(LoadCursorW(None, if over { IDC_HAND } else { IDC_ARROW }).ok());
                LRESULT(1)
            }
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Releases the Direct2D surface. Call on the main thread before exiting:
/// dropping it during process teardown corrupts the exit.
pub fn shutdown() {
    ANIM.with(|cell| cell.borrow_mut().surface = None);
}

/// Creates the window. Call on the main (message loop) thread.
pub fn create(instance: windows::Win32::Foundation::HINSTANCE) {
    unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: w!("chatty_overlay"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&class);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("chatty_overlay"),
            w!("chatty"),
            WS_POPUP,
            0,
            0,
            10,
            10,
            None,
            None,
            Some(instance),
            None,
        )
        .expect("overlay window");
        HWND_RAW.store(hwnd.0 as isize, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------- preview --

/// Draws a card through the live path (DC render target into the DIB that
/// UpdateLayeredWindow would show) and checks the pixels. No window.
pub fn selftest() -> Result<(), String> {
    ANIM.with(|cell| {
        let mut anim = cell.borrow_mut();
        let frame = Frame {
            mode: Mode::Card { text: "Self-test".into(), reason: "Not pasted".into(), copied: false },
            live: String::new(),
            split: 0,
            level: 0.0,
            elapsed: 0.0,
            t: 0.0,
            hover: None,
        };
        let dpi = 144.0;
        let s = surface(&mut anim, 1, 1, dpi).ok_or("surface")?;
        let geo = layout(&mut s.g, &frame);
        let (w, h) = (((MAX_W + 2.0 * MARGIN) * 1.5) as i32, ((geo.h + 2.0 * MARGIN) * 1.5) as i32);
        let s = surface(&mut anim, w, h, dpi).ok_or("surface")?;
        unsafe {
            s.rt.BindDC(s.mem, &RECT { left: 0, top: 0, right: w, bottom: h }).map_err(|e| e.to_string())?;
            s.g.begin();
            s.g.clear(Color(0.0, 0.0, 0.0, 0.0));
            draw(&mut s.g, &frame, &geo);
            if !s.g.end() {
                return Err("EndDraw failed".into());
            }
            let px = |x: i32, y: i32| {
                let p = s.bits.add(((y * w + x) * 4) as usize);
                [*p, *p.add(1), *p.add(2), *p.add(3)]
            };
            let corner = px(1, 1);
            let inside = px(w / 2, (MARGIN * 1.5) as i32 + 4);
            println!("corner bgra {corner:?}, card bgra {inside:?}, size {w}x{h}");
            if corner[3] != 0 || inside[3] < 240 {
                return Err("unexpected alpha".into());
            }
        }
        anim.surface = None;
        Ok(())
    })
}

/// Draws one overlay state to a PNG over a sample backdrop, for design
/// review: `chatty --render-overlay <live|long|transcribing|notice|warning|progress|card|copied> out.png [scale]`.
pub fn render_png(kind: &str, path: &std::path::Path, scale: f32) -> Result<(), String> {
    use windows::Win32::Graphics::Direct2D::*;
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::*;
    use windows::core::HSTRING;
    let e = |e: windows::core::Error| e.to_string();
    let long = "So I was thinking we could move the review to Thursday afternoon, because the numbers from the \
                second test run won't be ready before Wednesday evening, and I'd rather walk everyone through the \
                final version than a draft that changes again the next morning.";
    let short = "Let's ship the overlay redesign today and";
    let mut split = usize::MAX;
    let (mode, live, level, elapsed, hover) = match kind {
        "live" => {
            split = short[..20].rfind(' ').unwrap_or(0);
            (Mode::Live(short.into()), short.to_string(), 0.7, 7.0, None)
        }
        "listening" => (Mode::Live(String::new()), String::new(), 0.1, 1.0, None),
        "long" => {
            split = long[..long.len() - 40].rfind(' ').unwrap_or(0);
            (Mode::Live(long.into()), long.to_string(), 0.55, 23.0, None)
        }
        "transcribing" => (Mode::Transcribing, long.to_string(), 0.0, 0.0, None),
        "notice" => (Mode::Notice("Downloaded Parakeet Unified EN 0.6B".into()), String::new(), 0.0, 0.0, None),
        "warning" => (Mode::Notice("Download failed: Parakeet Unified EN 0.6B".into()), String::new(), 0.0, 0.0, None),
        "progress" => (Mode::Progress("Downloading Parakeet Unified EN 0.6B".into(), 0.42), String::new(), 0.0, 0.0, None),
        "copied" => (
            Mode::Card { text: long.into(), reason: "Not pasted · no text field had focus".into(), copied: true },
            String::new(),
            0.0,
            0.0,
            None,
        ),
        _ => (
            Mode::Card { text: long.into(), reason: "Not pasted · no text field had focus".into(), copied: false },
            String::new(),
            0.0,
            0.0,
            Some(Button::Copy),
        ),
    };
    let frame = Frame { mode, live, split, level, elapsed, t: 1.3, hover };
    if let Ok(f) = std::env::var("CHATTY_FONT") {
        crate::ui::gfx::set_font(&f);
    }
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).map_err(e)?;
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).map_err(e)?;
        let dpi = 96.0 * scale;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: dpi,
            dpiY: dpi,
            ..Default::default()
        };
        // Measure first on a scratch target.
        let scratch = wic.CreateBitmap(4, 4, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnDemand).map_err(e)?;
        let rt = factory.CreateWicBitmapRenderTarget(&scratch, &props).map_err(e)?;
        let mut g = Gfx::from_target(rt.cast().map_err(e)?, dpi).map_err(e)?;
        let geo = layout(&mut g, &frame);
        let (bw, bh) = (MAX_W + 2.0 * MARGIN + 80.0, geo.h + 2.0 * MARGIN + 60.0);
        let (pw, ph) = ((bw * scale) as u32, (bh * scale) as u32);
        let bmp = wic.CreateBitmap(pw, ph, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnDemand).map_err(e)?;
        let rt = factory.CreateWicBitmapRenderTarget(&bmp, &props).map_err(e)?;
        rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        let mut g = Gfx::from_target(rt.cast().map_err(e)?, dpi).map_err(e)?;
        rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        g.begin();
        // A busy, light-and-dark backdrop to judge contrast and the shadow.
        g.gradient(Rect::new(0.0, 0.0, bw, bh), 0.0, Color::rgb(58, 74, 96), Color::rgb(214, 220, 228));
        g.fill(Rect::new(0.0, bh * 0.55, bw * 0.45, bh), 0.0, Color::rgb(245, 245, 247));
        g.rt.SetTransform(&windows_numerics::Matrix3x2::translation(40.0, 30.0));
        draw(&mut g, &frame, &geo);
        g.rt.SetTransform(&windows_numerics::Matrix3x2::identity());
        g.end();

        let stream = wic.CreateStream().map_err(e)?;
        stream.InitializeFromFilename(&HSTRING::from(path.as_os_str()), 0x4000_0000).map_err(e)?;
        let enc = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null()).map_err(e)?;
        enc.Initialize(&stream, WICBitmapEncoderNoCache).map_err(e)?;
        let mut fe: Option<IWICBitmapFrameEncode> = None;
        enc.CreateNewFrame(&mut fe, std::ptr::null_mut()).map_err(e)?;
        let fe = fe.ok_or("no frame")?;
        fe.Initialize(None).map_err(e)?;
        fe.SetSize(pw, ph).map_err(e)?;
        let mut fmt = GUID_WICPixelFormat32bppPBGRA;
        fe.SetPixelFormat(&mut fmt).map_err(e)?;
        fe.WriteSource(&bmp, std::ptr::null()).map_err(e)?;
        fe.Commit().map_err(e)?;
        enc.Commit().map_err(e)?;
    }
    Ok(())
}
