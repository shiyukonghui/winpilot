use std::ffi::c_void;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{IsIconic, ShowWindow, SW_RESTORE};
use windows_capture::{
    capture::{Context, GraphicsCaptureApiHandler},
    frame::Frame,
    graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl},
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },
    window::Window,
};

use crate::types::{
    CaptureEvent, CaptureEventReceiver, FrameMailbox, FramePacket, FrameSlot,
};

/// 抓取线程上的回调：把每帧 RGBA 像素推给 UI，不持有任何窗口句柄以外的状态。
struct PreviewHandler {
    events: FrameMailbox,
    scratch: Vec<u8>,
    logged_first: bool,
}

impl GraphicsCaptureApiHandler for PreviewHandler {
    type Flags = FrameMailbox;
    type Error = anyhow::Error;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            events: ctx.flags,
            scratch: Vec::new(),
            logged_first: false,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame<'_>,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        // 时间戳打在回调入口，拷贝与投递的耗时也计入预览延迟
        let arrived_at = Instant::now();

        // 去掉标题栏，使抓取画面与目标窗口的客户区一一对应，后续坐标映射只需一次等比缩放
        let buffer = match frame.buffer_without_title_bar() {
            Ok(buffer) => buffer,
            Err(error) => {
                // 返回 Err 会让整个会话终止，这里只记录并跳过该帧
                tracing::error!("取帧失败: {error}");
                return Ok(());
            }
        };

        let (width, height) = (buffer.width() as usize, buffer.height() as usize);
        if !self.logged_first {
            self.logged_first = true;
            tracing::info!(
                "首帧到达 {width}×{height} 格式={:?} 有填充={}",
                buffer.color_format(),
                buffer.has_padding()
            );
        }

        // frame buffer 在回调返回后即被 unmap，必须拷贝出来；scratch 复用避免每帧重新分配
        let mut packed = std::mem::take(&mut self.scratch);
        packed.clear();
        let rgba = buffer.as_nopadding_buffer(&mut packed).to_vec();
        self.scratch = packed;

        self.events.push(CaptureEvent::Frame(Arc::new(FramePacket {
            rgba,
            width,
            height,
            captured_at: arrived_at,
        })));
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        tracing::info!("抓取会话已关闭（目标窗口销毁或不可再抓取）");
        self.events.push(CaptureEvent::Closed);
        Ok(())
    }
}

pub struct CaptureWorker {
    control: windows_capture::capture::CaptureControl<PreviewHandler, anyhow::Error>,
}

impl CaptureWorker {
    /// 启动抓取会话，返回控制句柄、帧事件接收端和最新帧槽位（供旁路读帧）。
    pub fn start(hwnd: isize) -> Result<(Self, CaptureEventReceiver, FrameSlot)> {
        let (events, events_rx) = FrameMailbox::bounded();
        let frame_slot = events.slot();
        let window = Window::from_raw_hwnd(hwnd as *mut c_void);

        if !window.is_valid() {
            anyhow::bail!("目标窗口当前不可抓取（已关闭或为工具窗口）");
        }

        // 最小化的窗口不产生 WGC 帧，先恢复并等布局完成，否则会话起来后只有一张旧帧
        let target = HWND(hwnd as *mut c_void);
        if unsafe { IsIconic(target) }.as_bool() {
            let _ = unsafe { ShowWindow(target, SW_RESTORE) };
            std::thread::sleep(Duration::from_millis(150));
            tracing::info!("目标窗口处于最小化，已自动恢复");
        }

        let settings = Settings::new(
            window,
            CursorCaptureSettings::WithoutCursor,
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Exclude,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Rgba8,
            events,
        );

        let control = PreviewHandler::start_free_threaded(settings)
            .with_context(|| format!("WGC 会话启动失败 (hwnd=0x{hwnd:X})"))?;

        Ok((Self { control }, events_rx, frame_slot))
    }

    pub fn stop(self) {
        if let Err(error) = self.control.stop() {
            tracing::warn!("停止抓取会话失败: {error}");
        }
    }

    /// 抓取线程是否仍在运行。会话内部异常终止后会变为 false。
    pub fn is_alive(&self) -> bool {
        !self.control.is_finished()
    }
}

/// 本机对 WGC 各项能力的支持情况，用于启动时自检。
/// `None` 表示查询失败（通常是 WinRT 尚未在本线程初始化），不代表系统不支持。
pub fn capability_report() -> Vec<(&'static str, Option<bool>)> {
    let check = |result: std::result::Result<bool, windows_capture::graphics_capture_api::Error>| result.ok();
    vec![
        (
            "Windows Graphics Capture 可用",
            check(GraphicsCaptureApi::is_supported()),
        ),
        (
            "支持不绘制光标",
            check(GraphicsCaptureApi::is_cursor_settings_supported()),
        ),
        (
            "支持关闭捕获边框",
            check(GraphicsCaptureApi::is_border_settings_supported()),
        ),
    ]
}
