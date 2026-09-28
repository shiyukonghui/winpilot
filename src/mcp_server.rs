use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use base64::Engine as _;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, ErrorData, ServerCapabilities, ServerConfig,
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{schemars, ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::geometry;
use crate::types::InputRequest;
use crate::keys;
use crate::session::SharedSession;
use crate::types::{Action, MouseButton};
use crate::window_list::list_capturable_windows;

/// MCP 监听端口默认值
const DEFAULT_PORT: u16 = 8100;
/// 帧龄超过该值视为过期画面（目标最小化或静止时 WGC 不产帧），返回里用 stale 标出。
const STALE_FRAME_MS: u128 = 1000;

/// 在独立线程上启动 tokio runtime 与 Streamable HTTP 服务，挂靠当前 GUI 进程。
pub fn start(session: Arc<SharedSession>, gui: egui::Context, port: Option<u16>) {
    let port = port.unwrap_or(DEFAULT_PORT);
    let spawned = std::thread::Builder::new()
        .name("mcp-server".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!("MCP server tokio runtime 启动失败: {error}");
                    return;
                }
            };
            if let Err(error) = runtime.block_on(serve(session, gui, port)) {
                tracing::error!("MCP server 退出: {error:#}");
            }
        });
    match spawned {
        Ok(_) => tracing::info!("MCP server 线程已启动，地址 http://127.0.0.1:{port}/mcp"),
        Err(error) => tracing::error!("MCP server 线程创建失败: {error}"),
    }
}

async fn serve(
    session: Arc<SharedSession>,
    gui: egui::Context,
    port: u16,
) -> anyhow::Result<()> {
    let session_manager = Arc::new(LocalSessionManager::default());
    let service = StreamableHttpService::new(
        move || {
            Ok(WinPilotTools {
                session: session.clone(),
                gui: gui.clone(),
            })
        },
        session_manager,
        StreamableHttpServerConfig::default(),
    );

    let app = Router::new().route("/mcp", axum::routing::any_service(service));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!("MCP server 就绪：http://127.0.0.1:{port}/mcp");
    axum::serve(listener, app).await?;
    Ok(())
}

// ---------- 工具参数 ----------

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SelectWindowArgs {
    /// 窗口标题或进程名的包含匹配关键字，例如 "tetris"
    pub query: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ScreenshotArgs {
    /// 可选归一化裁剪区 [左, 上, 右, 下]，取值 0..1，默认整幅画面
    pub region: Option<[f32; 4]>,
    /// 可选缩放（0.5 = 长宽各减半），用于控制返回图像体积，默认 1.0
    pub scale: Option<f32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct WaitForChangeArgs {
    /// 上一次已知画面校验和（capture_status 或动作回执里返回）
    pub checksum: u64,
    /// 最长等待毫秒数，默认 1500
    pub timeout_ms: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ClickArgs {
    /// 归一化横坐标 0..1（相对画面）
    pub fx: f32,
    /// 归一化纵坐标 0..1（相对画面）
    pub fy: f32,
    /// left / right / middle，默认 left
    pub button: Option<String>,
    /// 连点次数，默认 1
    pub count: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct KeyArgs {
    /// 按键序列，空格分隔多条，例如 "Left Left Space" 或 "Ctrl+Shift+Left"
    pub keys: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TextArgs {
    /// 要输入的文本（逐码元走 UNICODE 事件）
    pub text: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ScrollArgs {
    /// 滚轮格数，正数向上
    pub ticks: i32,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct MoveRelArgs {
    pub dx: i32,
    pub dy: i32,
}

// ---------- 工具实现 ----------

#[derive(Clone)]
pub struct WinPilotTools {
    session: Arc<SharedSession>,
    gui: egui::Context,
}

#[tool_router]
impl WinPilotTools {
    #[tool(description = "列出当前所有可抓取的目标窗口（含标题、进程名、尺寸）")]
    async fn list_windows(&self) -> Result<CallToolResult, ErrorData> {
        let windows = list_capturable_windows().map_err(mcp_error)?;
        let items: Vec<serde_json::Value> = windows
            .iter()
            .map(|info| {
                serde_json::json!({
                    "hwnd": info.hwnd,
                    "process": info.process_name,
                    "title": info.title,
                    "size": [info.width, info.height],
                    "minimized": info.minimized,
                })
            })
            .collect();
        Ok(text_result(serde_json::json!({ "windows": items })))
    }

    #[tool(description = "选择目标窗口并开始抓取（由 GUI 在下一帧执行）。之后才能 screenshot 和 click。")]
    async fn select_window(
        &self,
        Parameters(args): Parameters<SelectWindowArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let windows = list_capturable_windows().map_err(mcp_error)?;
        let lowered = args.query.to_lowercase();
        let found = windows.iter().find(|info| {
            info.title.contains(&args.query)
                || info.process_name.to_lowercase().contains(&lowered)
        });
        let Some(info) = found else {
            return Err(mcp_error(anyhow::format_err!(
                "没有匹配 {:?} 的窗口",
                args.query
            )));
        };

        if let Ok(mut pending) = self.session.pending_select.lock() {
            *pending = Some(info.hwnd);
        }
        self.gui.request_repaint();
        Ok(text_result(serde_json::json!({
            "selected": { "hwnd": info.hwnd, "process": info.process_name, "title": info.title },
            "note": "抓取正在启动，稍等片刻后调用 capture_status 确认",
        })))
    }

    #[tool(description = "查询抓取状态：目标窗口、画面尺寸、帧率、最新帧校验和与帧龄、注入队列积压")]
    async fn capture_status(&self) -> Result<CallToolResult, ErrorData> {
        Ok(text_result(self.status_json()))
    }

    #[tool(description = "截取当前目标窗口画面，返回 PNG 图像。可先用 select_window 选定目标。")]
    async fn screenshot(
        &self,
        Parameters(args): Parameters<ScreenshotArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let packet = self
            .session
            .frame_slot()
            .and_then(|slot| slot.get())
            .ok_or_else(|| mcp_error(anyhow::format_err!("还没有画面：先调用 select_window")))?;

        let mut rgba = packet.rgba.clone();
        let (mut width, mut height) = (packet.width, packet.height);

        if let Some([fx0, fy0, fx1, fy1]) = args.region {
            let (x0, y0, x1, y1) = clamp_region(fx0, fy0, fx1, fy1, width, height);
            rgba = crop(&rgba, width, x0, y0, x1, y1);
            width = x1 - x0;
            height = y1 - y0;
        }
        if let Some(scale) = args.scale {
            if (0.05..1.0).contains(&scale) {
                let (sw, sh) = (
                    ((width as f32 * scale).round() as usize).max(1),
                    ((height as f32 * scale).round() as usize).max(1),
                );
                rgba = downscale(&rgba, width, height, sw, sh);
                width = sw;
                height = sh;
            }
        }

        let png = encode_png(&rgba, width, height).map_err(mcp_error)?;
        let data = base64::engine::general_purpose::STANDARD.encode(png);
        let age_ms = packet.captured_at.elapsed().as_millis();
        let meta = serde_json::json!({
            "size": [width, height],
            "checksum": crate::types::frame_checksum(&packet.rgba),
            "frame_age_ms": age_ms,
            "stale": age_ms > STALE_FRAME_MS,
        });
        Ok(CallToolResult::success(vec![
            ContentBlock::image(data, "image/png"),
            ContentBlock::text(meta.to_string()),
        ]))
    }

    #[tool(description = "等待画面发生变化（相对给定校验和），返回是否变化与新校验和。用于确认一次操作真的生效了。")]
    async fn wait_for_change(
        &self,
        Parameters(args): Parameters<WaitForChangeArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if self.session.frame_slot().is_none() {
            return Err(mcp_error(anyhow::format_err!("还没有画面：先调用 select_window")));
        }
        let timeout = Duration::from_millis(args.timeout_ms.unwrap_or(1500).clamp(50, 10_000));
        let deadline = std::time::Instant::now() + timeout;
        let mut last_checksum = self.current_checksum();
        loop {
            if last_checksum.is_some_and(|sum| sum != args.checksum) {
                return Ok(text_result(serde_json::json!({
                    "changed": true,
                    "checksum": last_checksum,
                })));
            }
            if std::time::Instant::now() >= deadline {
                return Ok(text_result(serde_json::json!({
                    "changed": false,
                    "checksum": last_checksum,
                    "note": "超时画面未变化：目标可能静止，也可能操作未生效",
                })));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
            last_checksum = self.current_checksum();
        }
    }

    #[tool(description = "在画面的归一化坐标 (fx, fy) 处点击。button 可选 left/right/middle，count 默认 1（2 为双击）。")]
    async fn click(
        &self,
        Parameters(args): Parameters<ClickArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let button = parse_button(args.button.as_deref())?;
        let action = self.mapped_click(args.fx, args.fy, button, args.count.unwrap_or(1).max(1))?;
        self.run_action(action).await
    }

    #[tool(description = "发送按键。支持单个组合键（如 \"Ctrl+Shift+Left\"）或空格分隔的序列（如 \"Left Left Space\"）。")]
    async fn key(
        &self,
        Parameters(args): Parameters<KeyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let combos = keys::parse_sequence(&args.keys).map_err(mcp_str)?;
        let mut results = Vec::new();
        for combo in combos {
            let action = Action::Combo { keys: combo };
            results.push(self.run_action(action).await?);
        }
        Ok(text_result(serde_json::json!({
            "sent": args.keys,
            "receipts": results,
        })))
    }

    #[tool(description = "向目标输入文本（走 UNICODE 键事件，不受键盘布局影响）。")]
    async fn type_text(
        &self,
        Parameters(args): Parameters<TextArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run_action(Action::Text { text: args.text })
            .await
    }

    #[tool(description = "滚动滚轮。ticks 正数向上、负数向下。")]
    async fn scroll(
        &self,
        Parameters(args): Parameters<ScrollArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run_action(Action::Scroll { ticks: args.ticks })
            .await
    }

    #[tool(description = "相对移动鼠标（走 raw input，鼠标被游戏捕获时仍然有效）。")]
    async fn move_rel(
        &self,
        Parameters(args): Parameters<MoveRelArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run_action(Action::MoveRel {
            dx: args.dx,
            dy: args.dy,
        })
        .await
    }
}

impl WinPilotTools {
    fn current_checksum(&self) -> Option<u64> {
        self.session
            .frame_slot()
            .and_then(|slot| slot.get())
            .map(|packet| {
                crate::types::frame_checksum(&packet.rgba)
            })
    }

    fn status_json(&self) -> serde_json::Value {
        let selected = self
            .session
            .selected()
            .map(|info| serde_json::json!({ "hwnd": info.hwnd, "process": info.process_name, "title": info.title }));
        let geometry = self.session.geometry().map(|geo| {
            serde_json::json!({
                "client_size": [geo.client_w, geo.client_h],
                "dpi": geo.dpi,
            })
        });
        let frame = self
            .session
            .frame_slot()
            .and_then(|slot| slot.get())
            .map(|packet| {
                let age_ms = packet.captured_at.elapsed().as_millis();
                serde_json::json!({
                    "size": [packet.width, packet.height],
                    "checksum": crate::types::frame_checksum(&packet.rgba),
                    "age_ms": age_ms,
                    "stale": age_ms > STALE_FRAME_MS,
                })
            });
        let fps = self.session.fps.lock().map(|fps| *fps).unwrap_or(0.0);
        let backlog = self.session.input.backlog();
        serde_json::json!({
            "selected": selected,
            "geometry": geometry,
            "frame": frame,
            "fps": fps,
            "input_backlog": backlog,
        })
    }

    /// 归一化画面坐标 → 客户区 → 屏幕坐标，与 GUI 预览的映射完全一致。
    fn mapped_click(
        &self,
        fx: f32,
        fy: f32,
        button: MouseButton,
        count: u32,
    ) -> Result<Action, ErrorData> {
        let geometry = self
            .session
            .geometry()
            .ok_or_else(|| mcp_error(anyhow::format_err!("还没有目标窗口：先调用 select_window")))?;
        let Some(info) = self.session.selected() else {
            return Err(mcp_error(anyhow::format_err!("还没有目标窗口：先调用 select_window")));
        };
        let Some(slot) = self.session.frame_slot() else {
            return Err(mcp_error(anyhow::format_err!("抓取未启动：select_window 后稍候再试")));
        };
        let Some(packet) = slot.get() else {
            return Err(mcp_error(anyhow::format_err!("还没有画面帧：稍候再试")));
        };

        let frame = (packet.width, packet.height);
        let image = (fx.clamp(0.0, 1.0) * frame.0 as f32, fy.clamp(0.0, 1.0) * frame.1 as f32);
        let client = geometry::image_to_client(image, frame, &geometry)
            .ok_or_else(|| mcp_error(anyhow::format_err!("坐标换算失败")))?;
        let screen = geometry
            .client_to_screen(info.hwnd, client.0, client.1)
            .map_err(mcp_error)?;
        Ok(Action::Click {
            screen,
            button,
            count,
        })
    }

    async fn run_action(&self, action: Action) -> Result<CallToolResult, ErrorData> {
        let Some(info) = self.session.selected() else {
            return Err(mcp_error(anyhow::format_err!("还没有目标窗口：先调用 select_window")));
        };
        let request = InputRequest {
            hwnd: info.hwnd,
            activate: true,
            action: action.clone(),
        };
        let session = self.session.clone();
        let receipt = tokio::task::spawn_blocking(move || {
            session
                .input
                .execute_and_wait(request, Duration::from_secs(5))
        })
        .await
        .map_err(|error| mcp_error(anyhow::format_err!("注入任务失败: {error}")))?
        .map_err(mcp_str)?;

        let checksum = self.current_checksum();
        Ok(text_result(serde_json::json!({
            "action": action.describe(),
            "receipt": receipt,
            "frame_checksum": checksum,
        })))
    }
}

#[tool_handler]
impl ServerHandler for WinPilotTools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "WinPilot：抓取 Windows 窗口画面并向它注入鼠标键盘操作。\
                 先 select_window 选目标，再用 screenshot 看画面，\
                 click/key/type_text 操作，wait_for_change 或再次 screenshot 确认效果。",
            )
    }
}

// ---------- 小工具 ----------

fn text_result(value: serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(value.to_string())])
}

fn mcp_error(error: anyhow::Error) -> ErrorData {
    ErrorData::internal_error(format!("{error:#}"), None)
}

fn mcp_str(message: String) -> ErrorData {
    ErrorData::internal_error(message, None)
}

fn parse_button(name: Option<&str>) -> Result<MouseButton, ErrorData> {
    match name.map(str::to_lowercase).as_deref() {
        None | Some("left") => Ok(MouseButton::Left),
        Some("right") => Ok(MouseButton::Right),
        Some("middle") => Ok(MouseButton::Middle),
        Some(other) => Err(mcp_error(anyhow::format_err!(
            "未知按键 {other:?}，可选 left/right/middle"
        ))),
    }
}

fn clamp_region(
    fx0: f32,
    fy0: f32,
    fx1: f32,
    fy1: f32,
    width: usize,
    height: usize,
) -> (usize, usize, usize, usize) {
    let clamp_x = |fx: f32| (fx.clamp(0.0, 1.0) * width as f32).round() as usize;
    let clamp_y = |fy: f32| (fy.clamp(0.0, 1.0) * height as f32).round() as usize;
    let (x0, x1) = (clamp_x(fx0), clamp_x(fx1));
    let (y0, y1) = (clamp_y(fy0), clamp_y(fy1));
    (
        x0.min(x1).min(width.saturating_sub(1)),
        y0.min(y1).min(height.saturating_sub(1)),
        x0.max(x1).clamp(1, width),
        y0.max(y1).clamp(1, height),
    )
}

fn crop(rgba: &[u8], src_width: usize, x0: usize, y0: usize, x1: usize, y1: usize) -> Vec<u8> {
    let row_bytes = (x1 - x0) * 4;
    let mut out = Vec::with_capacity(row_bytes * (y1 - y0));
    for y in y0..y1 {
        let start = (y * src_width + x0) * 4;
        out.extend_from_slice(&rgba[start..start + row_bytes]);
    }
    out
}

/// 最近邻缩放。预览画面缩小时细节损失可接受，实现简单且不引入新依赖。
fn downscale(rgba: &[u8], src_w: usize, src_h: usize, dst_w: usize, dst_h: usize) -> Vec<u8> {
    let mut out = vec![0u8; dst_w * dst_h * 4];
    for y in 0..dst_h {
        let src_y = (y as f32 / dst_h as f32 * src_h as f32) as usize % src_h;
        for x in 0..dst_w {
            let src_x = (x as f32 / dst_w as f32 * src_w as f32) as usize % src_w;
            let src = (src_y * src_w + src_x) * 4;
            let dst = (y * dst_w + x) * 4;
            out[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
        }
    }
    out
}

fn encode_png(rgba: &[u8], width: usize, height: usize) -> Result<Vec<u8>, anyhow::Error> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width as u32, height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(rgba)?;
    }
    Ok(out)
}
