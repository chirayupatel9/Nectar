// Live blur for the glass styles. Windows only blurs behind a whole window, and the notch and dock
// windows are much bigger than the surfaces they draw, so each surface gets a borderless window of
// its own just below it, cut to the surface's outline, and Windows blurs whatever is under that.
// The page sends the outline as the surface moves and resizes (liveBlur.ts).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tauri::Window;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmEnableBlurBehindWindow, DwmSetWindowAttribute, DWMSBT_TRANSIENTWINDOW, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE,
    DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
    DWM_BB_BLURREGION, DWM_BB_ENABLE, DWM_BLURBEHIND,
};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, CreatePolygonRgn, CreateRectRgn, DeleteObject, GetStockObject, SetWindowRgn, BLACK_BRUSH, HBRUSH,
    WINDING,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW, GetWindow, GetWindowRect,
    IsIconic, IsWindow, GW_HWNDPREV,
    IsWindowVisible, PeekMessageW, PostThreadMessageW, RegisterClassW, SendMessageW, SetTimer, SetWindowPos,
    ShowWindow, MSG, PM_NOREMOVE, SET_WINDOW_POS_FLAGS, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_SHOWWINDOW, SW_HIDE,
    WM_APP, WM_MOUSEACTIVATE, WM_NCACTIVATE, WM_NCHITTEST, WM_TIMER, WM_USER, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

const CLASS: PCWSTR = w!("NectarLiveBlur");
const WM_LIVE_BLUR: u32 = WM_APP + 1;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Blur,
    Acrylic,
}

impl Kind {
    fn parse(name: &str) -> Option<Kind> {
        match name {
            "blur" => Some(Kind::Blur),
            "acrylic" => Some(Kind::Acrylic),
            _ => None,
        }
    }
}

struct Request {
    owner: isize,
    kind: Option<Kind>,
    dark: bool,
    seq: u64,
    points: Vec<POINT>,
}

struct Backdrop {
    hwnd: HWND,
    kind: Kind,
    dark: bool,
    seq: u64,
    wanted: bool,
    shown: bool,
}

static QUEUE: Mutex<Vec<Request>> = Mutex::new(Vec::new());
static THREAD: OnceLock<Option<u32>> = OnceLock::new();

fn windows_build() -> u32 {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    static BUILD: OnceLock<u32> = OnceLock::new();
    *BUILD.get_or_init(|| unsafe {
        let mut buf = [0u16; 16];
        let mut size = std::mem::size_of_val(&buf) as u32;
        let found = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
            w!("CurrentBuildNumber"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            Some(&mut size),
        )
        .is_ok();
        if !found {
            return 0;
        }
        let len = (size as usize / 2).min(buf.len());
        String::from_utf16_lossy(&buf[..len]).trim_end_matches('\0').trim().parse().unwrap_or(0)
    })
}

/// Windows 10 1809 and later can blur behind a window.
#[tauri::command]
pub fn live_blur_supported() -> bool {
    windows_build() >= 17763
}

/// Shows the blur behind the calling window's surface, cut to `points` (x, y pairs in CSS pixels
/// of the window), or hides it when `kind` is None or there are no points. `seq` keeps a late
/// update from replacing a newer one.
#[tauri::command]
pub async fn set_live_blur(window: Window, kind: Option<String>, dark: bool, seq: u64, points: Vec<f64>) -> bool {
    if !live_blur_supported() {
        return false;
    }
    let Ok(owner) = window.hwnd() else { return false };
    let scale = window.scale_factor().unwrap_or(1.0);
    let points = points
        .chunks_exact(2)
        .map(|p| POINT { x: (p[0] * scale).round() as i32, y: (p[1] * scale).round() as i32 })
        .collect();
    let request = Request { owner: owner.0 as isize, kind: kind.as_deref().and_then(Kind::parse), dark, seq, points };
    let Some(thread) = *THREAD.get_or_init(start_thread) else { return false };
    if let Ok(mut queue) = QUEUE.lock() {
        queue.push(request);
    }
    unsafe { PostThreadMessageW(thread, WM_LIVE_BLUR, WPARAM(0), LPARAM(0)) }.is_ok()
}

// The blur windows live on one thread of their own. It also checks on them four times a second,
// since a hidden notch or dock window can't tell the page to take its blur away.
fn start_thread() -> Option<u32> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || unsafe {
        let mut msg = MSG::default();
        // Creates the thread's message queue before anyone posts to it
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
        let _ = tx.send(windows::Win32::System::Threading::GetCurrentThreadId());
        run();
    });
    rx.recv_timeout(Duration::from_secs(2)).ok()
}

unsafe fn run() {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    let instance: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
    let wc = WNDCLASSW {
        lpfnWndProc: Some(backdrop_proc),
        hInstance: instance,
        lpszClassName: CLASS,
        hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
        ..Default::default()
    };
    if RegisterClassW(&wc) == 0 {
        return;
    }
    let _ = SetTimer(None, 0, 250, None);

    let mut backdrops: HashMap<isize, Backdrop> = HashMap::new();
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        if !msg.hwnd.is_invalid() {
            DispatchMessageW(&msg);
            continue;
        }
        match msg.message {
            WM_LIVE_BLUR => {
                let requests = QUEUE.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
                // Several frames can pile up; only the newest per window matters.
                let mut latest: HashMap<isize, Request> = HashMap::new();
                for r in requests {
                    if latest.get(&r.owner).is_none_or(|l| r.seq > l.seq) {
                        latest.insert(r.owner, r);
                    }
                }
                for r in latest.into_values() {
                    apply(&mut backdrops, r, instance);
                }
            }
            WM_TIMER => {
                backdrops.retain(|&owner, b| {
                    let alive = IsWindow(Some(HWND(owner as *mut _))).as_bool();
                    if !alive {
                        let _ = DestroyWindow(b.hwnd);
                    }
                    alive
                });
                for (&owner, b) in backdrops.iter_mut() {
                    sync(HWND(owner as *mut _), b);
                }
            }
            _ => {}
        }
    }
}

unsafe extern "system" fn backdrop_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // HTTRANSPARENT: clicks go to whatever is under it
        WM_NCHITTEST => LRESULT(-1),
        // MA_NOACTIVATE
        WM_MOUSEACTIVATE => LRESULT(3),
        // Acrylic turns into a flat color in an inactive window; this one is never active.
        WM_NCACTIVATE => DefWindowProcW(hwnd, msg, WPARAM(1), lparam),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn set_dark(hwnd: HWND, dark: bool) {
    let value = windows::core::BOOL::from(dark);
    let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &value as *const _ as *const _, 4);
}

// SetWindowCompositionAttribute is undocumented but is what Windows 10 and 11 blur with; the same
// call window-vibrancy makes for the Settings window.
unsafe fn set_accent(hwnd: HWND, state: u32, flags: u32, color: u32) {
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    #[repr(C)]
    struct AccentPolicy {
        state: u32,
        flags: u32,
        color: u32,
        animation: u32,
    }
    #[repr(C)]
    struct CompositionData {
        attribute: u32,
        data: *mut core::ffi::c_void,
        size: usize,
    }
    type SetWindowCompositionAttribute = unsafe extern "system" fn(HWND, *mut CompositionData) -> windows::core::BOOL;

    let Ok(user32) = GetModuleHandleW(w!("user32.dll")) else { return };
    let Some(proc) = GetProcAddress(user32, windows::core::s!("SetWindowCompositionAttribute")) else { return };
    let set: SetWindowCompositionAttribute = std::mem::transmute(proc);
    let mut policy = AccentPolicy { state, flags, color, animation: 0 };
    // WCA_ACCENT_POLICY
    let mut data = CompositionData { attribute: 19, data: &mut policy as *mut _ as *mut _, size: std::mem::size_of::<AccentPolicy>() };
    let _ = set(hwnd, &mut data);
}

unsafe fn create(instance: HINSTANCE, kind: Kind, dark: bool) -> Option<HWND> {
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
        CLASS,
        PCWSTR::null(),
        WS_POPUP,
        0,
        0,
        0,
        0,
        None,
        None,
        Some(instance),
        None,
    )
    .ok()?;

    // Per-pixel transparency, the way tao makes Tauri's transparent windows: anything the blur
    // doesn't cover stays see-through, so if Windows ignores the effect nothing shows at all.
    let empty = CreateRectRgn(0, 0, -1, -1);
    let bb = DWM_BLURBEHIND {
        dwFlags: DWM_BB_ENABLE | DWM_BB_BLURREGION,
        fEnable: true.into(),
        hRgnBlur: empty,
        fTransitionOnMaximized: false.into(),
    };
    let _ = DwmEnableBlurBehindWindow(hwnd, &bb);
    let _ = DeleteObject(empty.into());

    let none = DWMWA_COLOR_NONE;
    let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &none as *const _ as *const _, 4);
    let corners = DWMWCP_DONOTROUND;
    let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corners as *const _ as *const _, 4);
    set_dark(hwnd, dark);

    match kind {
        // ACCENT_ENABLE_BLURBEHIND: a plain blur, the surface on top adds the tint
        Kind::Blur => set_accent(hwnd, 3, 2, 0),
        // Windows 11's acrylic material
        Kind::Acrylic if windows_build() >= 22523 => {
            let backdrop = DWMSBT_TRANSIENTWINDOW;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &backdrop as *const _ as *const _, 4);
        }
        // ACCENT_ENABLE_ACRYLICBLURBEHIND, with the least tint it accepts
        Kind::Acrylic => set_accent(hwnd, 4, 0, 0x0100_0000),
    }
    let _ = SendMessageW(hwnd, WM_NCACTIVATE, Some(WPARAM(1)), Some(LPARAM(0)));
    Some(hwnd)
}

unsafe fn apply(backdrops: &mut HashMap<isize, Backdrop>, r: Request, instance: HINSTANCE) {
    if backdrops.get(&r.owner).is_some_and(|b| r.seq <= b.seq) {
        return;
    }
    let owner = HWND(r.owner as *mut _);
    let Some(kind) = r.kind.filter(|_| r.points.len() >= 3) else {
        if let Some(b) = backdrops.get_mut(&r.owner) {
            b.seq = r.seq;
            b.wanted = false;
            hide(b);
        }
        return;
    };

    // Switching between blur and acrylic gets a fresh window rather than undoing the old effect.
    if backdrops.get(&r.owner).is_some_and(|b| b.kind != kind) {
        if let Some(old) = backdrops.remove(&r.owner) {
            let _ = DestroyWindow(old.hwnd);
        }
    }
    if !backdrops.contains_key(&r.owner) {
        let Some(hwnd) = create(instance, kind, r.dark) else { return };
        backdrops.insert(r.owner, Backdrop { hwnd, kind, dark: r.dark, seq: 0, wanted: false, shown: false });
    }
    let Some(b) = backdrops.get_mut(&r.owner) else { return };
    b.seq = r.seq;
    b.wanted = true;
    if b.dark != r.dark {
        set_dark(b.hwnd, r.dark);
        b.dark = r.dark;
    }
    let region = CreatePolygonRgn(&r.points, WINDING);
    if !region.is_invalid() {
        // The window owns the region from here on.
        SetWindowRgn(b.hwnd, Some(region), b.shown);
    }
    sync(owner, b);
}

// Covers the owner's client area, right below it in the z-order, while the owner is showing.
unsafe fn sync(owner: HWND, b: &mut Backdrop) {
    let showing = IsWindowVisible(owner).as_bool() && !IsIconic(owner).as_bool();
    let mut rc = RECT::default();
    let mut origin = POINT::default();
    if !b.wanted || !showing || GetClientRect(owner, &mut rc).is_err() || !ClientToScreen(owner, &mut origin).as_bool() {
        hide(b);
        return;
    }
    let (x, y, width, height) = (origin.x, origin.y, rc.right - rc.left, rc.bottom - rc.top);
    let mut current = RECT::default();
    let in_place = GetWindowRect(b.hwnd, &mut current).is_ok()
        && current == RECT { left: x, top: y, right: x + width, bottom: y + height }
        && GetWindow(b.hwnd, GW_HWNDPREV).is_ok_and(|above| above == owner);
    if b.shown && in_place {
        return;
    }
    let show = if b.shown { SET_WINDOW_POS_FLAGS(0) } else { SWP_SHOWWINDOW };
    let _ = SetWindowPos(
        b.hwnd,
        Some(owner),
        x,
        y,
        width,
        height,
        SWP_NOACTIVATE | SWP_NOOWNERZORDER | show,
    );
    b.shown = true;
}

unsafe fn hide(b: &mut Backdrop) {
    if b.shown {
        let _ = ShowWindow(b.hwnd, SW_HIDE);
        b.shown = false;
    }
}
