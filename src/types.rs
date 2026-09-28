use std::sync::{Arc, Mutex};
use std::time::Instant;

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
    /// 窗口是否处于最小化状态。最小化的窗口收不到 WGC 帧，抓取前要先恢复。
    pub minimized: bool,
}

impl WindowInfo {
    pub fn label(&self) -> String {
        if self.minimized {
            format!(
                "{} · {}  ({}×{}, 已最小化)",
                self.process_name, self.title, self.width, self.height
            )
        } else {
            format!(
                "{} · {}  ({}×{})",
                self.process_name, self.title, self.width, self.height
            )
        }
    }
}

pub struct FramePacket {
    pub rgba: Vec<u8>,
    pub width: usize,
    pub height: usize,
    /// 抓取回调收到该帧的时刻，用于测量抓取→上传的端到端延迟
    pub captured_at: Instant,
}

/// 最新一帧的共享槽位。抓取回调写入，MCP 的 screenshot 等工具读取，
/// 与预览信箱互不干扰：UI 走信箱，外部工具走槽位。
#[derive(Clone, Default)]
pub struct FrameSlot(Arc<Mutex<Option<Arc<FramePacket>>>>);

impl FrameSlot {
    pub fn get(&self) -> Option<Arc<FramePacket>> {
        self.0.lock().ok().and_then(|slot| slot.clone())
    }

    /// 帧龄（毫秒），调用方据此判断画面是否新鲜。
    pub fn age_ms(&self) -> Option<u128> {
        self.get()
            .map(|packet| packet.captured_at.elapsed().as_millis())
    }

    pub fn set(&self, packet: Arc<FramePacket>) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(packet);
        }
    }

}

pub enum CaptureEvent {
    Frame(Arc<FramePacket>),
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

/// 帧内容的 FNV-1a 校验和。两次读数不同即画面发生了变化，
/// 供 MCP 工具用很小的代价做"操作是否生效"的判定。
pub fn frame_checksum(rgba: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in rgba {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 预览事件通道：容量 1，并且投递前先把压在里面的旧帧挤掉。
///
/// 抓取端比 UI 快时，丢的必须是旧帧而不是新帧——否则界面上永远显示过期画面，
/// 操作起来就像"画面慢半拍"。同时抓取线程绝不能阻塞（WGC 回调一停整条链路就停），
/// 所以这里只用 try_* 系列接口，靠多持有的一份接收端来腾位置。
#[derive(Clone)]
pub struct FrameMailbox {
    tx: CaptureEventSender,
    stale: CaptureEventReceiver,
    slot: FrameSlot,
}

impl FrameMailbox {
    pub fn bounded() -> (Self, CaptureEventReceiver) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mailbox = Self {
            tx: tx.clone(),
            stale: rx.clone(),
            slot: FrameSlot::default(),
        };
        (mailbox, rx)
    }

    /// 本信箱对应的最新帧槽位，交给需要旁路读帧的组件（例如 MCP 的 screenshot）。
    pub fn slot(&self) -> FrameSlot {
        self.slot.clone()
    }

    pub fn push(&self, event: CaptureEvent) {
        if let CaptureEvent::Frame(packet) = &event {
            self.slot.set(packet.clone());
        }
        let retry = match self.tx.try_send(event) {
            Ok(()) => return,
            Err(crossbeam_channel::TrySendError::Full(event)) => event,
            // 通道已断开，无需再投递
            Err(_) => return,
        };
        let _ = self.stale.try_recv();
        let _ = self.tx.try_send(retry);
    }
}
