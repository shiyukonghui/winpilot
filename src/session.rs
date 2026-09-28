use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

use crate::capture::CaptureWorker;
use crate::geometry::TargetGeometry;
use crate::input::InputWorker;
use crate::types::{CaptureEventReceiver, FrameSlot, WindowInfo};
use crate::window_list::list_capturable_windows;
use crossbeam_channel::Sender;

/// GUI、无头模式与 MCP server 共享的会话状态。
///
/// 抓取会话的生命周期由这里统一管理：`ensure_capture` 是唯一的启动入口，
/// 三种来源（GUI 按钮、MCP select_window、--auto）走同一条路。
/// GUI 是"观看者"——通过 `generation` 感知会话变化并认领新的帧接收端。
pub struct SharedSession {
    pub input: InputWorker,
    /// 产生注入回执的通道：GUI 收进界面日志，无头模式打进 tracing
    pub notes: Sender<String>,

    selected: Mutex<Option<WindowInfo>>,
    geometry: Mutex<Option<TargetGeometry>>,
    frame_slot: Mutex<Option<FrameSlot>>,
    /// 抓取端实测帧率，由内建的采样线程每秒更新
    fps: Mutex<f32>,

    capture: Mutex<CaptureState>,
    generation: AtomicU64,
}

#[derive(Default)]
struct CaptureState {
    worker: Option<CaptureWorker>,
    receiver: Option<CaptureEventReceiver>,
    hwnd: Option<isize>,
}

impl SharedSession {
    /// 构建会话并启动抓取帧率采样线程。
    pub fn start(input: InputWorker, notes: Sender<String>) -> Arc<Self> {
        let session = Arc::new(Self {
            input,
            notes,
            selected: Mutex::new(None),
            geometry: Mutex::new(None),
            frame_slot: Mutex::new(None),
            fps: Mutex::new(0.0),
            capture: Mutex::new(CaptureState::default()),
            generation: AtomicU64::new(0),
        });
        session.spawn_fps_sampler();
        session
    }

    /// 每秒读一次帧计数差值，得到真实的抓取帧率。
    /// GUI 的显示帧率与这里无关；无头模式下它是唯一的帧率来源。
    fn spawn_fps_sampler(self: &Arc<Self>) {
        let session = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("fps-sampler".to_owned())
            .spawn(move || {
                let mut last_count = 0_u64;
                let mut last_at = Instant::now();
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    let Some(session) = session.upgrade() else { return };
                    let count = session.frame_slot().map(|slot| slot.count()).unwrap_or(0);
                    let elapsed = last_at.elapsed().as_secs_f32();
                    let fps = if elapsed > 0.0 {
                        (count.saturating_sub(last_count)) as f32 / elapsed
                    } else {
                        0.0
                    };
                    last_count = count;
                    last_at = Instant::now();
                    if let Ok(mut guard) = session.fps.lock() {
                        *guard = fps;
                    }
                }
            })
            .expect("fps 采样线程创建失败");
    }

    /// 启动（或重启）对 `hwnd` 的抓取会话，返回帧接收端与目标几何。
    ///
    /// 同一窗口、会话存活且画面新鲜（帧龄 <1s）时直接复用当前会话；
    /// 其余情况（换了目标、上次失败、目标最小化导致断帧）重新走一遍启动，
    /// 启动过程会自动恢复最小化的窗口。
    pub fn ensure_capture(&self, hwnd: isize) -> Result<(CaptureEventReceiver, TargetGeometry)> {
        let mut state = self
            .capture
            .lock()
            .map_err(|_| anyhow::anyhow!("会话状态锁中毒"))?;

        if state.hwnd == Some(hwnd)
            && state.worker.as_ref().is_some_and(|worker| worker.is_alive())
            && self
                .frame_slot()
                .and_then(|slot| slot.age_ms())
                .is_some_and(|age| age < 1000)
        {
            if let (Some(receiver), Some(geometry)) = (&state.receiver, self.geometry()) {
                return Ok((receiver.clone(), geometry));
            }
        }

        if let Some(worker) = state.worker.take() {
            worker.stop();
        }
        state.receiver = None;

        let (worker, receiver, frame_slot) = CaptureWorker::start(hwnd).context("启动抓取失败")?;
        let geometry = TargetGeometry::query(hwnd).context("读取窗口几何失败")?;
        let info = list_capturable_windows()
            .ok()
            .and_then(|windows| windows.into_iter().find(|w| w.hwnd == hwnd))
            .unwrap_or_else(|| WindowInfo {
                hwnd,
                title: String::new(),
                process_name: String::new(),
                pid: 0,
                width: geometry.window_w,
                height: geometry.window_h,
                minimized: false,
            });

        state.worker = Some(worker);
        state.receiver = Some(receiver.clone());
        state.hwnd = Some(hwnd);
        self.set_selected(Some(info));
        self.set_geometry(Some(geometry));
        self.set_frame_slot(Some(frame_slot));
        self.generation.fetch_add(1, Ordering::Relaxed);

        let _ = self.notes.send(format!("抓取已启动 hwnd=0x{hwnd:X}"));
        Ok((receiver, geometry))
    }

    /// 停止抓取会话并清空画面槽位。
    pub fn stop_capture(&self) {
        let mut state = match self.capture.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        if let Some(worker) = state.worker.take() {
            worker.stop();
            let _ = self.notes.send("抓取已停止".to_owned());
        }
        state.receiver = None;
        state.hwnd = None;
        self.set_frame_slot(None);
        self.set_selected(None);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// 会话代数：每次抓取会话启动/停止后 +1，GUI 据此感知外部切换。
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// 当前会话的帧接收端克隆，供 GUI 认领。
    pub fn receiver(&self) -> Option<CaptureEventReceiver> {
        self.capture
            .lock()
            .ok()
            .and_then(|state| state.receiver.clone())
    }

    /// 抓取会话是否存活。
    pub fn capture_alive(&self) -> bool {
        self.capture
            .lock()
            .ok()
            .and_then(|state| state.worker.as_ref().map(|w| w.is_alive()))
            .unwrap_or(false)
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

    pub fn fps(&self) -> f32 {
        self.fps.lock().map(|fps| *fps).unwrap_or(0.0)
    }
}
