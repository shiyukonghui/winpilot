use std::sync::Mutex;

use crate::geometry::TargetGeometry;
use crate::input::InputWorker;
use crate::types::{FrameSlot, WindowInfo};

/// GUI 与 MCP server 共享的会话状态。
///
/// 挂靠模式下两边操作同一个实例：输入共用一条注入队列（串行）；selected/geometry/
/// frame_slot/fps 是 GUI 维护的事实镜像，MCP 只读；MCP 的 select_window 写入
/// pending_select 并唤醒 GUI，由 GUI 统一管理抓取会话的生命周期。
pub struct SharedSession {
    pub input: InputWorker,
    /// 当前目标窗口（GUI 与 MCP 共同维护，后写者生效）
    pub selected: Mutex<Option<WindowInfo>>,
    /// 目标窗口几何，抓取启动时写入
    pub geometry: Mutex<Option<TargetGeometry>>,
    /// 抓取启动时由 GUI 换成新会话的槽位，停止时清空
    pub frame_slot: Mutex<Option<FrameSlot>>,
    /// 预览实测帧率，GUI 周期性更新
    pub fps: Mutex<f32>,
    /// MCP 的 select_window 在这里排队，GUI 每次重绘开头取走并执行
    pub pending_select: Mutex<Option<isize>>,
}

impl SharedSession {
    pub fn new(input: InputWorker) -> Self {
        Self {
            input,
            selected: Mutex::new(None),
            geometry: Mutex::new(None),
            frame_slot: Mutex::new(None),
            fps: Mutex::new(0.0),
            pending_select: Mutex::new(None),
        }
    }

    pub fn set_selected(&self, info: Option<WindowInfo>) {
        if let Ok(mut guard) = self.selected.lock() {
            *guard = info;
        }
    }

    pub fn selected(&self) -> Option<WindowInfo> {
        self.selected.lock().ok().and_then(|guard| guard.clone())
    }

    pub fn set_geometry(&self, geometry: Option<TargetGeometry>) {
        if let Ok(mut guard) = self.geometry.lock() {
            *guard = geometry;
        }
    }

    pub fn geometry(&self) -> Option<TargetGeometry> {
        self.geometry.lock().ok().and_then(|guard| *guard)
    }

    pub fn set_frame_slot(&self, slot: Option<FrameSlot>) {
        if let Ok(mut guard) = self.frame_slot.lock() {
            *guard = slot;
        }
    }

    pub fn frame_slot(&self) -> Option<FrameSlot> {
        self.frame_slot.lock().ok().and_then(|guard| guard.clone())
    }
}
