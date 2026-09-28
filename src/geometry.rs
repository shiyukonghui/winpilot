use anyhow::{Context as _, Result};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE,
    DPI_AWARENESS_CONTEXT_UNAWARE, DPI_AWARENESS_CONTEXT_UNAWARE_GDISCALED, GetDpiForWindow,
    GetWindowDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetCursorPos, GetWindowRect, SetCursorPos,
};

fn as_hwnd(hwnd: isize) -> HWND {
    HWND(hwnd as *mut core::ffi::c_void)
}

/// 目标窗口的几何信息，用于把预览画面的像素坐标映射回目标窗口的客户区坐标。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetGeometry {
    pub client_w: i32,
    pub client_h: i32,
    pub window_w: i32,
    pub window_h: i32,
    pub dpi: u32,
    pub awareness: &'static str,
}

/// 只能比对句柄，无法直接读取数值，因此逐个已知 context 试。
/// 注意 Per-Monitor 与 Per-Monitor V2 是两个不同的 context，必须分开判断。
fn awareness_name(hwnd: HWND) -> &'static str {
    let context = unsafe { GetWindowDpiAwarenessContext(hwnd) };
    let same = |other| unsafe { AreDpiAwarenessContextsEqual(context, other).as_bool() };

    if same(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
        "Per-Monitor V2"
    } else if same(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE) {
        "Per-Monitor"
    } else if same(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE) {
        "System"
    } else if same(DPI_AWARENESS_CONTEXT_UNAWARE_GDISCALED) {
        "Unaware(GDIScaled)"
    } else if same(DPI_AWARENESS_CONTEXT_UNAWARE) {
        "Unaware"
    } else {
        "未知"
    }
}

impl TargetGeometry {
    pub fn query(hwnd: isize) -> Result<Self> {
        let window = as_hwnd(hwnd);
        let mut client_rect = RECT::default();
        let mut window_rect = RECT::default();

        let awareness = awareness_name(window);

        unsafe {
            GetClientRect(window, &mut client_rect).context("GetClientRect 失败")?;
            GetWindowRect(window, &mut window_rect).context("GetWindowRect 失败")?;
        }

        Ok(Self {
            client_w: client_rect.right - client_rect.left,
            client_h: client_rect.bottom - client_rect.top,
            window_w: window_rect.right - window_rect.left,
            window_h: window_rect.bottom - window_rect.top,
            dpi: unsafe { GetDpiForWindow(window) },
            awareness,
        })
    }

    /// 客户区坐标 -> 屏幕物理坐标。
    pub fn client_to_screen(&self, hwnd: isize, x: i32, y: i32) -> Result<(i32, i32)> {
        let mut point = POINT { x, y };
        unsafe {
            ClientToScreen(as_hwnd(hwnd), &mut point).ok().context("ClientToScreen 失败")?;
        }
        Ok((point.x, point.y))
    }

    /// 抓取画面是否恰好等于客户区尺寸（相等时映射就是纯比例换算，最可靠）。
    pub fn frame_is_client(&self, frame: (usize, usize)) -> bool {
        frame.0 as i32 == self.client_w && frame.1 as i32 == self.client_h
    }
}

/// 画面像素坐标 -> 目标窗口客户区坐标。
///
/// 抓取到的就是客户区，两者通常尺寸相同；但 DPI 虚拟化或标题栏裁剪差异会让尺寸不一致，
/// 因此按尺寸比例换算，而不是假定像素恒等。
pub fn image_to_client(
    image: (f32, f32),
    frame: (usize, usize),
    geometry: &TargetGeometry,
) -> Option<(i32, i32)> {
    if frame.0 == 0 || frame.1 == 0 || geometry.client_w <= 0 || geometry.client_h <= 0 {
        return None;
    }

    let scale_x = geometry.client_w as f32 / frame.0 as f32;
    let scale_y = geometry.client_h as f32 / frame.1 as f32;
    let x = (image.0 * scale_x).clamp(0.0, (geometry.client_w - 1) as f32).round() as i32;
    let y = (image.1 * scale_y).clamp(0.0, (geometry.client_h - 1) as f32).round() as i32;
    Some((x, y))
}

/// 只移动系统光标、不做任何点击，用于在接入注入之前肉眼校验映射是否准确。
pub fn aim_cursor(
    hwnd: isize,
    geometry: &TargetGeometry,
    frame: (usize, usize),
    image: (f32, f32),
) -> Result<(i32, i32)> {
    let (client_x, client_y) =
        image_to_client(image, frame, geometry).context("画面或客户区尺寸为 0，无法换算")?;
    let screen = geometry.client_to_screen(hwnd, client_x, client_y)?;
    unsafe {
        SetCursorPos(screen.0, screen.1).ok().context("SetCursorPos 失败")?;
    }
    Ok(screen)
}

/// 回读系统光标当前的屏幕物理坐标，用于校验 SetCursorPos 是否真的到位。
pub fn cursor_pos() -> (i32, i32) {
    let mut point = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut point);
    }
    (point.x, point.y)
}
