use std::ffi::c_void;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
    BringWindowToTop, GetForegroundWindow, GetGUIThreadInfo, GetWindowTextW,
    GetWindowThreadProcessId, GUITHREADINFO, IsIconic, IsWindow, SetCursorPos, SetForegroundWindow,
    ShowWindow, SwitchToThisWindow, SW_RESTORE,
};

use crate::keys::is_extended;
use crate::types::{Action, InputRequest, MouseButton};

/// 按下到抬起之间的保持时间。目标可能按帧轮询输入状态，保持时间过短会让一次
/// 操作被合并成"没按过"。8ms 远小于 60fps 的 16.7ms 帧长，够用又不至于拖慢队列。
const HOLD: Duration = Duration::from_millis(8);
/// 连续点击之间的间隔。要小于系统双击判定间隔（默认 500ms）才会被认成双击，
/// 又要留出 down/up 分离的时间，30ms 是两者之间的安全值。
const MULTI_CLICK_GAP: Duration = Duration::from_millis(30);
/// 确实切换过前台之后，最多再等这么久让键盘焦点落到目标上；已经在前台时完全不等待。
const FOCUS_WAIT: Duration = Duration::from_millis(40);
/// 文本走 WM_CHAR 队列，不需要保持时间，只留一点间隔避免事件洪泛。
const TEXT_GAP: Duration = Duration::from_millis(4);

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
                // rx.len() 是本条执行完之前还排着多少条，用于界面上观察积压
                execute(request, &notes_tx, rx.len() as usize);
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

    /// 还没开始执行的请求条数。持续非零说明注入速度跟不上发送速度。
    pub fn backlog(&self) -> usize {
        self.tx.len() as usize
    }
}

fn note(notes: &Sender<String>, message: impl Into<String>) {
    let _ = notes.send(message.into());
}

fn execute(request: InputRequest, notes: &Sender<String>, pending: usize) {
    let started = Instant::now();
    let hwnd = HWND(request.hwnd as *mut c_void);
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        note(notes, "目标窗口已不存在，操作已取消");
        return;
    }

    if request.activate {
        match activate(hwnd) {
            // 已经在前台，不需要任何等待，这是连点时最常见的路径
            Activation::Already => {}
            Activation::Switched => wait_focus(hwnd),
            Activation::Failed => {
                note(notes, "激活失败：目标未拿到前台焦点，注入可能被忽略");
                return;
            }
        }
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

    let took = started.elapsed().as_millis();
    match outcome {
        Ok(()) => note(
            notes,
            format!(
                "已发送 {} 用时{took}ms 队列剩{pending}（此刻前台={}）",
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

enum Activation {
    /// 本来就是前台，不需要等待
    Already,
    /// 这次调用把它带到了前台，需要等一下键盘焦点跟上
    Switched,
    /// 系统不允许抢前台
    Failed,
}

/// 把窗口抢到前台。SetForegroundWindow 有系统限制，需要先把输入队列挂到当前前台线程上。
fn activate(hwnd: HWND) -> Activation {
    unsafe {
        // 已经是前台就不用折腾，直接省下激活等待
        if GetForegroundWindow() == hwnd && !IsIconic(hwnd).as_bool() {
            return Activation::Already;
        }

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

        if GetForegroundWindow() == hwnd {
            Activation::Switched
        } else {
            Activation::Failed
        }
    }
}

/// 抢过前台之后，目标还要在自己的消息队列里处理 WM_ACTIVATE，键盘焦点才会真的落上去。
/// 这里轮询到就绪就立刻返回，最多等 FOCUS_WAIT——比盲等固定 40ms 快得多，连点时体感明显。
fn wait_focus(hwnd: HWND) {
    let thread = unsafe { GetWindowThreadProcessId(hwnd, None) };
    if thread == 0 {
        return;
    }
    let deadline = Instant::now() + FOCUS_WAIT;
    loop {
        let mut info = GUITHREADINFO::default();
        info.cbSize = size_of::<GUITHREADINFO>() as u32;
        let ready = unsafe { GetGUIThreadInfo(thread, &mut info) }
            .map(|()| info.hwndFocus == hwnd || info.hwndActive == hwnd)
            .unwrap_or(false);
        if ready || Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
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

    for index in 0..count.max(1) {
        if index > 0 {
            std::thread::sleep(MULTI_CLICK_GAP);
        }
        press(button, true)?;
        std::thread::sleep(HOLD);
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

/// 组合键：正序按下、逆序抬起。按下与抬起之间只保持 HOLD，让目标至少有一帧
/// 能轮询到按键状态；步骤之间不再插等待，否则连点会排成长队。
fn combo(keys: &[u16], layout: Option<HKL>) -> Result<(), String> {
    for vk in keys {
        key(*vk, true, layout)?;
    }
    std::thread::sleep(HOLD);
    for vk in keys.iter().rev() {
        key(*vk, false, layout)?;
    }
    Ok(())
}

/// 逐 UTF-16 码元走 KEYEVENTF_UNICODE，绕开键盘布局和 scan code 差异。
fn send_text(text: &str, _layout: Option<HKL>) -> Result<(), String> {
    for unit in text.encode_utf16() {
        key_event(0, unit, KEYEVENTF_UNICODE)?;
        key_event(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)?;
        std::thread::sleep(TEXT_GAP);
    }
    Ok(())
}

pub type NotesReceiver = Receiver<String>;
