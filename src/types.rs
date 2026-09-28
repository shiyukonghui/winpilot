use crossbeam_channel::{Receiver, Sender};

/// 目标窗口的快照。用 `isize` 保存 HWND 而不是窗口句柄类型，保证本结构 `Send + Sync`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub title: String,
    pub process_name: String,
    pub pid: u32,
    pub width: i32,
    pub height: i32,
}

impl WindowInfo {
    pub fn label(&self) -> String {
        format!(
            "{} · {}  ({}×{})",
            self.process_name, self.title, self.width, self.height
        )
    }
}

pub struct FramePacket {
    pub rgba: Vec<u8>,
    pub width: usize,
    pub height: usize,
}

pub enum CaptureEvent {
    Frame(FramePacket),
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    pub fn label(self) -> &'static str {
        match self {
            Self::Left => "左键",
            Self::Right => "右键",
            Self::Middle => "中键",
        }
    }
}

/// 一次要执行的输入动作。
#[derive(Clone, Debug)]
pub enum Action {
    /// 移到屏幕坐标后点击，count=2 为双击
    Click {
        screen: (i32, i32),
        button: MouseButton,
        count: u32,
    },
    /// 在当前光标位置按下或抬起
    Button {
        button: MouseButton,
        down: bool,
    },
    /// 相对位移，鼠标被游戏捕获时只有这个有效
    MoveRel { dx: i32, dy: i32 },
    /// 滚轮格数，正数向上
    Scroll { ticks: i32 },
    /// 组合键：按顺序按下，逆序抬起
    Combo { keys: Vec<u16> },
    /// 逐码元 Unicode 文本
    Text { text: String },
}

impl Action {
    pub fn describe(&self) -> String {
        match self {
            Self::Click { screen, button, count } => {
                format!("{}×{} ({},{})", button.label(), count, screen.0, screen.1)
            }
            Self::Button { button, down } => {
                format!("{}{}", button.label(), if *down { "按下" } else { "抬起" })
            }
            Self::MoveRel { dx, dy } => format!("相对移动 ({dx},{dy})"),
            Self::Scroll { ticks } => format!("滚轮 {ticks} 格"),
            Self::Combo { keys } => format!("组合键 {:?}", keys),
            Self::Text { text } => format!("文本 {text:?}"),
        }
    }
}

/// 投递给输入线程的一次完整请求。hwnd 用 isize 传递以保证 Send。
#[derive(Clone, Debug)]
pub struct InputRequest {
    pub hwnd: isize,
    pub activate: bool,
    pub action: Action,
}

pub type CaptureEventSender = Sender<CaptureEvent>;
pub type CaptureEventReceiver = Receiver<CaptureEvent>;

/// 有界通道：容量 2，抓取端满则丢帧，保证预览延迟不随 UI 卡顿而堆积。
pub fn event_channel() -> (CaptureEventSender, CaptureEventReceiver) {
    crossbeam_channel::bounded(2)
}
