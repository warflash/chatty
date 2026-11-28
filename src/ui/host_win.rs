//! Windows host of the main window: Win32 window, message handling,
//! Direct2D target, offscreen PNG rendering.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, PAINTSTRUCT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, ReleaseCapture, SetCapture, TRACKMOUSEEVENT, TME_LEAVE, TrackMouseEvent, VK_CONTROL};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, w};


use super::gfx::Gfx;
use super::{APP, Action, App, Page, frame, init, reload_report, run_actions, with_app};
use crate::{config, stats};

static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
static NOTIFY_PENDING: AtomicBool = AtomicBool::new(false);
static STANDALONE: AtomicBool = AtomicBool::new(false);

const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_STATUS: u32 = WM_APP + 30;
const WM_STATS: u32 = WM_APP + 31;
const TIMER_ANIM: usize = 1;

fn hwnd() -> Option<HWND> {
    let h = HWND_RAW.load(Ordering::Relaxed);
    (h != 0).then(|| HWND(h as *mut _))
}

fn visible() -> bool {
    hwnd().is_some_and(|h| unsafe { IsWindowVisible(h).as_bool() })
}

/// Live status changed (any thread).
pub fn notify() {
    super::flyout::notify();
    if let Some(h) = hwnd() {
        if visible() && !NOTIFY_PENDING.swap(true, Ordering::AcqRel) {
            unsafe {
                let _ = PostMessageW(Some(h), WM_STATUS, WPARAM(0), LPARAM(0));
            }
        }
    }
}

/// The statistics database changed (any thread).
pub fn stats_changed() {
    if let Some(h) = hwnd() {
        unsafe {
            let _ = PostMessageW(Some(h), WM_STATS, WPARAM(0), LPARAM(0));
        }
    }
}

/// Repaint request from any thread (e.g. a finished report).
pub fn redraw() {
    if let Some(h) = hwnd() {
        unsafe {
            let _ = InvalidateRect(Some(h), None, false);
        }
    }
}

pub fn open_path(p: &std::path::Path) {
    unsafe {
        ShellExecuteW(None, w!("open"), &HSTRING::from(p.as_os_str()), None, None, SW_SHOWNORMAL);
    }
}

pub fn play(p: &str) {
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_NODEFAULT};
    unsafe {
        let _ = PlaySoundW(&HSTRING::from(p), None, SND_FILENAME | SND_ASYNC | SND_NODEFAULT);
    }
}

fn frame_window(app: &mut App, hwnd: HWND) -> Vec<Action> {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96) as f32;
    if app.gfx.is_none() {
        match Gfx::new(hwnd, rc.right as u32, rc.bottom as u32, dpi) {
            Ok(g) => app.gfx = Some(g),
            Err(e) => {
                log::error!("Direct2D: {e}");
                return Vec::new();
            }
        }
    }
    let scale = dpi / 96.0;
    frame(app, rc.right as f32 / scale, rc.bottom as f32 / scale, dpi)
}

pub(super) fn mouse_pos(hwnd: HWND, lparam: LPARAM) -> (f32, f32) {
    let x = (lparam.0 & 0xFFFF) as u16 as i16 as f32;
    let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as f32;
    let scale = unsafe { GetDpiForWindow(hwnd) }.max(96) as f32 / 96.0;
    (x / scale, y / scale)
}

fn invalidate(hwnd: HWND) {
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut ps);
                let result = with_app(|a| {
                    let actions = frame_window(a, hwnd);
                    (actions, a.animating, a.mem.hand, a.mem.ibeam)
                });
                let _ = EndPaint(hwnd, &ps);
                if let Some((actions, animating, _, _)) = result {
                    let had_actions = !actions.is_empty();
                    run_actions(actions);
                    if animating || had_actions {
                        SetTimer(Some(hwnd), TIMER_ANIM, 16, None);
                    } else {
                        let _ = KillTimer(Some(hwnd), TIMER_ANIM);
                    }
                }
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == TIMER_ANIM => {
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_STATUS => {
                NOTIFY_PENDING.store(false, Ordering::Release);
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_STATS => {
                let r = with_app(|a| (a.report.clone(), a.loading.clone()));
                if let Some((r, l)) = r {
                    reload_report(r, l);
                }
                LRESULT(0)
            }
            WM_SIZE => {
                let (wpx, hpx) = ((lparam.0 & 0xFFFF) as u32, ((lparam.0 >> 16) & 0xFFFF) as u32);
                with_app(|a| {
                    if let Some(g) = a.gfx.as_mut() {
                        g.resize(wpx, hpx);
                    }
                });
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let p = mouse_pos(hwnd, lparam);
                with_app(|a| a.input.mouse = p);
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                with_app(|a| a.input.mouse = (-1.0, -1.0));
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let p = mouse_pos(hwnd, lparam);
                with_app(|a| {
                    a.input.mouse = p;
                    a.input.down = true;
                    a.input.pressed = true;
                });
                SetCapture(hwnd);
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let p = mouse_pos(hwnd, lparam);
                with_app(|a| {
                    a.input.mouse = p;
                    a.input.down = false;
                    a.input.released = true;
                });
                let _ = ReleaseCapture();
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as f32;
                with_app(|a| a.input.wheel += delta);
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_CHAR => {
                if let Some(c) = char::from_u32(wparam.0 as u32) {
                    with_app(|a| a.input.chars.push(c));
                    invalidate(hwnd);
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                let vk = wparam.0 as u16;
                let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
                with_app(|a| {
                    a.input.keys.push(vk);
                    if a.mem.focus == 0 {
                        // Page navigation keys.
                        let page = match vk {
                            0x74 => {
                                a.input.keys.clear();
                                Some(a.page) // F5 reloads
                            }
                            _ => None,
                        };
                        if page.is_some() {
                            reload_report(a.report.clone(), a.loading.clone());
                        }
                        match vk {
                            0x21 => a.scroll_target -= a.viewport_h * 0.9,
                            0x22 => a.scroll_target += a.viewport_h * 0.9,
                            0x24 if !ctrl => a.scroll_target = 0.0,
                            0x23 if !ctrl => a.scroll_target = a.content_h,
                            _ => {}
                        }
                    }
                });
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_SETCURSOR if (lparam.0 & 0xFFFF) as u32 == HTCLIENT => {
                let (hand, ibeam) = with_app(|a| (a.mem.hand, a.mem.ibeam)).unwrap_or((false, false));
                let cursor = if ibeam { IDC_IBEAM } else if hand { IDC_HAND } else { IDC_ARROW };
                SetCursor(LoadCursorW(None, cursor).ok());
                LRESULT(1)
            }
            WM_GETMINMAXINFO => {
                let info = &mut *(lparam.0 as *mut MINMAXINFO);
                let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
                info.ptMinTrackSize.x = (860.0 * scale) as i32;
                info.ptMinTrackSize.y = (560.0 * scale) as i32;
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let r = &*(lparam.0 as *const RECT);
                let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_CLOSE => {
                if STANDALONE.load(Ordering::Relaxed) {
                    let _ = DestroyWindow(hwnd);
                } else {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    // Free the GPU surface while hidden.
                    with_app(|a| a.gfx = None);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                if STANDALONE.load(Ordering::Relaxed) {
                    PostQuitMessage(0);
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

fn apply_backdrop(hwnd: HWND) {
    unsafe {
        // Opaque, custom-painted content; the title bar matches the sidebar.
        let dark: i32 = 1;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as *const _, 4);
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corner as *const _ as *const _, 4);
        let caption: u32 = 0x000E0C0C; // COLORREF of theme::SIDEBAR
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CAPTION_COLOR, &caption as *const _ as *const _, 4);
        let border: u32 = 0x00262222;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &border as *const _ as *const _, 4);
    }
}

/// Shows the window (creating it on first use) on `page`. Main thread only.
pub fn open(page: Option<Page>) {
    if let Some(h) = hwnd() {
        with_app(|a| {
            if let Some(p) = page {
                a.page = p;
                a.scroll = 0.0;
                a.scroll_target = 0.0;
            }
            reload_report(a.report.clone(), a.loading.clone());
        });
        unsafe {
            let _ = ShowWindow(h, SW_SHOWNORMAL);
            let _ = SetForegroundWindow(h);
        }
        invalidate(h);
        return;
    }
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("chatty_window"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: crate::tray::app_icon(),
            ..Default::default()
        };
        RegisterClassW(&class);
        APP.with(|a| *a.borrow_mut() = Some(App::new()));
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("chatty_window"),
            w!("chatty"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1200,
            820,
            None,
            None,
            Some(instance.into()),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                log::error!("window: {e}");
                return;
            }
        };
        HWND_RAW.store(hwnd.0 as isize, Ordering::Relaxed);
        apply_backdrop(hwnd);
        let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
        let _ = SetWindowPos(hwnd, None, 0, 0, (1200.0 * scale) as i32, (820.0 * scale) as i32, SWP_NOMOVE | SWP_NOZORDER);
        if let Some(p) = page {
            with_app(|a| a.page = p);
        }
        with_app(|a| reload_report(a.report.clone(), a.loading.clone()));
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(hwnd);
    }
}

/// `chatty --render <page> <out.png> [w h scale]`: draws a page offscreen
/// (Direct2D into a WIC bitmap) with the real statistics, no window.
pub fn render_png(cfg: config::Shared, page: Page, path: &std::path::Path, w: u32, h: u32, scale: f32) -> Result<(), String> {
    use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT};
    use windows::Win32::Graphics::Direct2D::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::*;
    use windows::core::Interface;
    let e = |e: windows::core::Error| e.to_string();
    init(cfg.clone(), None);
    crate::status::set_phase(crate::status::Phase::Idle);
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).map_err(e)?;
        let (pw, ph) = ((w as f32 * scale) as u32, (h as f32 * scale) as u32);
        let bmp = wic.CreateBitmap(pw, ph, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnDemand).map_err(e)?;
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).map_err(e)?;
        let dpi = 96.0 * scale;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: dpi,
            dpiY: dpi,
            ..Default::default()
        };
        let rt = factory.CreateWicBitmapRenderTarget(&bmp, &props).map_err(e)?;
        let mut app = App::new();
        app.gfx = Some(Gfx::from_target(rt.cast().map_err(e)?, dpi).map_err(e)?);
        app.page = page;
        let c = cfg.get();
        app.report = Arc::new(Mutex::new(Some(stats::open().and_then(|db| stats::report(&db, c.typing_wpm as f64, 60)))));
        app.devices = crate::audio::device_names();
        let _ = frame(&mut app, w as f32, h as f32, dpi);

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

/// `chatty --stats`: the window is the whole process; closing exits.
pub fn run_standalone(cfg: config::Shared, page: Page) -> Result<(), String> {
    STANDALONE.store(true, Ordering::Relaxed);
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    init(cfg, None);
    crate::status::set_phase(crate::status::Phase::Idle);
    open(Some(page));
    let Some(h) = hwnd() else { return Err("cannot create the window".into()) };
    crate::clipboard::OWNER.store(h.0 as isize, Ordering::Relaxed);
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
