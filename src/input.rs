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
    MapVirtualKeyExW, SendInput, SetActiveWindow, SetFocus, VIRTUAL_KEY, VK_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetForegroundWindow, GetGUIThreadInfo, GetWindowTextW,
    GetWindowThreadProcessId, GUITHREADINFO, IsIconic, IsWindow, SetCursorPos, SetForegroundWindow,
    ShowWindow, SwitchToThisWindow, SW_MINIMIZE, SW_SHOW, SW_RESTORE,
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
/// 最小化恢复后等窗口重排、焦点重建的时长；不等的话第一拍注入会被吞掉。
const RESTORE_SETTLE: Duration = Duration::from_millis(80);

/// 独立线程串行执行注入。SendInput 是同步调用且可能阻塞，绝不能跑在 UI 线程上。
/// GUI 走 `send`（结果进日志），MCP 工具走 `execute_and_wait`（同步拿回执），
/// 两条路径共用同一条队列，注入顺序始终串行。
struct Job {
    request: InputRequest,
    reply: Option<Sender<Result<String, String>>>,
}

pub struct InputWorker {
    tx: Sender<Job>,
    _join: JoinHandle<()>,
}

impl InputWorker {
    pub fn spawn(notes_tx: Sender<String>) -> Self {
        let (tx, rx) = unbounded::<Job>();
        let join = std::thread::spawn(move || {
            for job in rx.iter() {
                // rx.len() 是本条执行完之前还排着多少条，用于界面上观察积压
                let pending = rx.len() as usize;
                let result = execute(job.request, pending);
                match job.reply {
                    Some(reply) => {
                        let _ = reply.send(result);
                    }
                    None => match result {
                        Ok(text) | Err(text) => note(&notes_tx, text),
                    },
                }
            }
        });
        Self {
            tx,
            _join: join,
        }
    }

    pub fn send(&self, request: InputRequest) {
        let _ = self.tx.send(Job {
            request,
            reply: None,
        });
    }

    /// 同步执行并拿回执，供 MCP 工具向外部模型返回结构化结果。
    pub fn execute_and_wait(
        &self,
        request: InputRequest,
        timeout: Duration,
    ) -> Result<String, String> {
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        self.tx
            .send(Job {
                request,
                reply: Some(reply_tx),
            })
            .map_err(|_| "注入线程已退出".to_owned())?;
        reply_rx
            .recv_timeout(timeout)
            .unwrap_or_else(|_| Err("注入执行超时".to_owned()))
    }

    /// 还没开始执行的请求条数。持续非零说明注入速度跟不上发送速度。
    pub fn backlog(&self) -> usize {
        self.tx.len() as usize
    }
}

fn note(notes: &Sender<String>, message: impl Into<String>) {
    let _ = notes.send(message.into());
}

fn execute(request: InputRequest, pending: usize) -> Result<String, String> {
    let started = Instant::now();
    let hwnd = HWND(request.hwnd as *mut c_void);
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return Err("目标窗口已不存在，操作已取消".to_owned());
    }

    if request.activate {
        match activate(hwnd) {
            // 已经在前台，不需要任何等待，这是连点时最常见的路径
            Activation::Already => {}
            Activation::Switched => wait_focus(hwnd),
            Activation::Failed => {
                return Err("激活失败：目标未拿到前台焦点，注入可能被忽略".to_owned());
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
    let summary = format!(
        "{} 用时{took}ms 队列剩{pending}（此刻前台={}）",
        request.action.describe(),
        foreground_title()
    );
    outcome.map(|()| summary)
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

/// 把窗口抢到前台。Windows 对抢前台有多重限制，按强度递进尝试三级手段。
fn activate(hwnd: HWND) -> Activation {
    unsafe {
        // 已经是前台就不用折腾，直接省下激活等待
        if GetForegroundWindow() == hwnd && !IsIconic(hwnd).as_bool() {
            return Activation::Already;
        }

        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }

        // 第一级：常规的输入队列挂靠 + SetForegroundWindow
        if bring_to_front(hwnd) {
            return Activation::Switched;
        }

        // 第二级：按住 ALT 跨越激活调用。Windows 只允许"最近处理过输入"的进程抢前台，
        // 外部模型经 MCP 触发注入时本进程没有焦点，模拟一次按键即获得资格。
        let _ = key_event(VK_MENU.0, 0, KEYBD_EVENT_FLAGS(0));
        let ok = bring_to_front(hwnd);
        let _ = key_event(VK_MENU.0, 0, KEYEVENTF_KEYUP);
        if ok {
            return Activation::Switched;
        }

        // 第三级：最小化再恢复。窗口从最小化恢复时系统直接把它带到前台，
        // 不受前台锁限制；代价是目标窗口闪一下，所以放在 ALT 无效之后。
        // 恢复后目标还要重排窗口、重建输入焦点，立刻注入的第一拍会被吞掉，
        // 这里等它稳定再返回。
        let _ = ShowWindow(hwnd, SW_MINIMIZE);
        let _ = ShowWindow(hwnd, SW_RESTORE);
        std::thread::sleep(RESTORE_SETTLE);
        if GetForegroundWindow() == hwnd {
            return Activation::Switched;
        }

        // 第四级：系统仍拒绝时不再改变可见前台，直接把键盘焦点挂到目标窗口，
        // 键盘事件跟着焦点走，注入仍然有效。
        if steal_focus(hwnd) {
            return Activation::Switched;
        }

        Activation::Failed
    }
}

fn bring_to_front(hwnd: HWND) -> bool {
    unsafe {
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

/// 不改前台，直接把键盘焦点设到目标窗口。需要先把两个线程的输入队列挂在一起。
fn steal_focus(hwnd: HWND) -> bool {
    unsafe {
        let current_thread = GetCurrentThreadId();
        let target_thread = GetWindowThreadProcessId(hwnd, None);
        let attached = target_thread != 0
            && target_thread != current_thread
            && AttachThreadInput(current_thread, target_thread, true).as_bool();

        let active = SetActiveWindow(hwnd);
        let focused = SetFocus(Some(hwnd));

        if attached {
            let _ = AttachThreadInput(current_thread, target_thread, false);
        }
        active.is_ok() && focused.is_ok()
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
