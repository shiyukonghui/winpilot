use anyhow::Result;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowPlacement, IsIconic, WINDOWPLACEMENT,
};
use windows_capture::window::Window;

use crate::types::WindowInfo;

/// 列出所有可作为抓取目标的顶层窗口。
///
/// [`Window::enumerate`] 已过滤不可见、工具窗口与子窗口，这里再排除无标题窗口和本进程自身。
/// 最小化的窗口保留（rect 是 -32000 占位值，改用还原位置的尺寸），否则外部模型
/// 在目标最小化时会发现整个目标列表里没有它。
pub fn list_capturable_windows() -> Result<Vec<WindowInfo>> {
    let own_pid = std::process::id();
    let mut out = Vec::new();

    for window in Window::enumerate()? {
        if window.process_id().is_ok_and(|pid| pid == own_pid) {
            continue;
        }

        let title = window.title().unwrap_or_default();
        let title = title.trim().to_owned();
        if title.is_empty() {
            continue;
        }

        let hwnd = HWND(window.as_raw_hwnd());
        let iconic = unsafe { IsIconic(hwnd).as_bool() };
        let (width, height) = if iconic {
            let mut placement = WINDOWPLACEMENT::default();
            placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
            match unsafe { GetWindowPlacement(hwnd, &mut placement) } {
                Ok(()) => {
                    let rc = placement.rcNormalPosition;
                    ((rc.right - rc.left).max(0), (rc.bottom - rc.top).max(0))
                }
                Err(_) => (0, 0),
            }
        } else {
            let Ok(rect) = window.rect() else { continue };
            (rect.right - rect.left, rect.bottom - rect.top)
        };
        // 许多程序会常驻一个几十像素的隐藏辅助窗口（微信、资源管理器都有），
        // 它们从不重绘，选中后永远收不到帧，直接排除
        if width < 64 || height < 64 {
            continue;
        }

        out.push(WindowInfo {
            hwnd: window.as_raw_hwnd() as isize,
            process_name: window.process_name().unwrap_or_default(),
            pid: window.process_id().unwrap_or(0),
            title,
            width,
            height,
            minimized: iconic,
        });
    }

    out.sort_by(|a, b| {
        (&a.process_name, &a.title).cmp(&(&b.process_name, &b.title))
    });
    Ok(out)
}
