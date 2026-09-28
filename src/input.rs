use std::ffi::c_void;
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardLayout, HKL, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT, MOUSE_EVENT_FLAGS,
    MapVirtualKeyExW, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
    IsWindow, SetCursorPos, SetForegroundWindow, ShowWindow, SwitchToThisWindow, SW_RESTORE,
};

use crate::keys::is_extended;
use crate::types::{Action, InputRequest, MouseButton};

/// 注入节流：Godot 按帧轮询输入，事件间隔太小会被合并或漏掉。
const GAP: Duration = Duration::from_millis(15);
const DOUBLE_CLICK_GAP: Duration = Duration::from_millis(60);
const ACTIVATE_SETTLE: Duration = Duration::from_millis(80);

/// 独立线程串行执行注入。SendInput 是同步调用且可能阻塞，绝不能跑在 UI 线程上。
pub struct InputWorker {
    tx: Sender<InputRequest>,
    _join: JoinHandle<()>,
}

impl InputWorker {
    pub fn spawn(notes_tx: Sender<String>) -> Self {
        let (tx, rx) = unbounded();
        let join = std::thread::spawn(move || {
            for request in rx.iter() {
                execute(request, &notes_tx);
            }
        });
        Self {
            tx,
            _join: join,
        }
    }

    pub fn send(&self, request: InputRequest) {
        let _ = self.tx.send(request);
    }
}

fn note(notes: &Sender<String>, message: impl Into<String>) {
    let _ = notes.send(message.into());
}

fn execute(request: InputRequest, notes: &Sender<String>) {
    let hwnd = HWND(request.hwnd as *mut c_void);
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        note(notes, "目标窗口已不存在，操作已取消");
        return;
    }

    if request.activate && !activate(hwnd) {
        note(notes, "激活失败：目标未拿到前台焦点，注入可能被忽略");
        return;
    }
    if request.activate {
        std::thread::sleep(ACTIVATE_SETTLE);
    }

    let layout = key_layout(hwnd);
    let outcome = match &request.action {
        Action::Click {
            screen,
            button,
            count,
        } => click(*screen, *button, *count),
        Action::Button { button, down } => press(*button, *down),
        Action::MoveRel { dx, dy } => move_relative(*dx, *dy),
        Action::Scroll { ticks } => scroll(*ticks),
        Action::Combo { keys } => combo(keys, layout),
        Action::Text { text } => send_text(text, layout),
    };

    match outcome {
        Ok(()) => note(
            notes,
            format!(
                "已发送 {}（此刻前台={}）",
                request.action.describe(),
                foreground_title()
            ),
        ),
        Err(error) => note(notes, format!("{} 失败：{error}", request.action.describe())),
    }
}

/// 回读真正的前台窗口标题。SendInput 返回成功只代表事件入队，
/// 键盘事件跟随前台焦点而非鼠标位置，所以这是判断注入是否落到目标的关键证据。
fn foreground_title() -> String {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return "<无>".to_owned();
    }
    let mut buffer = [0u16; 256];
    let len = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    if len <= 0 {
        return "<空标题>".to_owned();
    }
    String::from_utf16_lossy(&buffer[..len as usize])
}

/// 把窗口抢到前台。SetForegroundWindow 有系统限制，需要先把输入队列挂到当前前台线程上。
fn activate(hwnd: HWND) -> bool {
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }

        let current_thread = GetCurrentThreadId();
        let foreground_thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let attached = foreground_thread != 0
            && foreground_thread != current_thread
            && AttachThreadInput(current_thread, foreground_thread, true).as_bool();

        let _ = SetForegroundWindow(hwnd);
        let _ = BringWindowToTop(hwnd);

        if attached {
            let _ = AttachThreadInput(current_thread, foreground_thread, false);
        }

        if GetForegroundWindow() != hwnd {
            SwitchToThisWindow(hwnd, true);
        }
        GetForegroundWindow() == hwnd
    }
}

fn key_layout(hwnd: HWND) -> Option<HKL> {
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, None) };
    let layout = unsafe { GetKeyboardLayout(thread_id) };
    Some(layout)
}

fn send_inputs(inputs: &[INPUT]) -> u32 {
    unsafe { SendInput(inputs, size_of::<INPUT>() as i32) }
}

fn check(sent: u32, expected: u32) -> Result<(), String> {
    if sent == expected {
        Ok(())
    } else if sent == 0 {
        Err("SendInput 返回 0，可能被更高级别的输入源拦截（如 UAC 安全桌面）".to_owned())
    } else {
        Err(format!("SendInput 只接受了 {sent}/{expected} 个事件"))
    }
}

fn button_flags(button: MouseButton, down: bool) -> MOUSE_EVENT_FLAGS {
    match (button, down) {
        (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
        (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
        (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
        (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
        (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
        (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
    }
}

fn press(button: MouseButton, down: bool) -> Result<(), String> {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: button_flags(button, down),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    check(send_inputs(&[input]), 1)
}

/// 绝对定位用 SetCursorPos（物理像素，语义直白），点击事件本身不带 ABSOLUTE 标志，
/// 因此不受 0..65535 归一化和多屏原点的影响。
fn click(screen: (i32, i32), button: MouseButton, count: u32) -> Result<(), String> {
    unsafe { SetCursorPos(screen.0, screen.1) }.map_err(|error| error.to_string())?;
    std::thread::sleep(GAP);

    for index in 0..count.max(1) {
        if index > 0 {
            std::thread::sleep(DOUBLE_CLICK_GAP);
        }
        press(button, true)?;
        std::thread::sleep(GAP);
        press(button, false)?;
    }
    Ok(())
}

/// 相对位移会同时进入 raw input 通道，所以游戏把鼠标捕获后仍然有效。
fn move_relative(dx: i32, dy: i32) -> Result<(), String> {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    check(send_inputs(&[input]), 1)
}

fn scroll(ticks: i32) -> Result<(), String> {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: (ticks * 120) as u32,
                dwFlags: MOUSEEVENTF_WHEEL,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    check(send_inputs(&[input]), 1)
}

fn key_event(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> Result<(), String> {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    check(send_inputs(&[input]), 1)
}

fn key(vk: u16, down: bool, layout: Option<HKL>) -> Result<(), String> {
    let scan = unsafe { MapVirtualKeyExW(u32::from(vk), MAPVK_VK_TO_VSC, layout) } as u16;
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    if is_extended(vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    key_event(vk, scan, flags)
}

fn combo(keys: &[u16], layout: Option<HKL>) -> Result<(), String> {
    for vk in keys {
        key(*vk, true, layout)?;
        std::thread::sleep(GAP);
    }
    std::thread::sleep(GAP);
    for vk in keys.iter().rev() {
        key(*vk, false, layout)?;
        std::thread::sleep(GAP);
    }
    Ok(())
}

/// 逐 UTF-16 码元走 KEYEVENTF_UNICODE，绕开键盘布局和 scan code 差异。
fn send_text(text: &str, _layout: Option<HKL>) -> Result<(), String> {
    for unit in text.encode_utf16() {
        key_event(0, unit, KEYEVENTF_UNICODE)?;
        key_event(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)?;
        std::thread::sleep(Duration::from_millis(8));
    }
    Ok(())
}

pub type NotesReceiver = Receiver<String>;
