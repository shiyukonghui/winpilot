use std::sync::Arc;
use std::time::Instant;

use crate::capture::capability_report;
use crate::geometry::{TargetGeometry, aim_cursor, image_to_client};
use crate::input::NotesReceiver;
use crate::keys::{parse_combo, parse_sequence};
use crate::session::SharedSession;
use crate::types::{Action, CaptureEvent, CaptureEventReceiver, FramePacket, InputRequest, MouseButton, WindowInfo};
use crate::window_list::list_capturable_windows;

const CONTROLS_WIDTH: f32 = 360.0;
const FONT_KEY: &str = "cjk";

/// 预览画面上的一个点及其换算结果。
#[derive(Clone, Copy, Debug, Default)]
struct MappedPoint {
    /// 抓取画面中的像素坐标
    image: (f32, f32),
    /// 目标窗口客户区坐标，注入时使用
    client: (i32, i32),
    /// 屏幕物理坐标
    screen: Option<(i32, i32)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PanelTab {
    Mouse,
    Keyboard,
    Log,
}

pub struct WinPilotApp {
    windows: Vec<WindowInfo>,
    selected: Option<isize>,
    /// 上一次向 session 认领的会话代数，变了说明有外部（MCP）切换了目标
    generation: u64,
    events_rx: Option<CaptureEventReceiver>,
    texture: Option<egui::TextureHandle>,
    frame_size: (usize, usize),
    total_frames: u64,
    fps: f32,
    fps_count: u32,
    fps_since: Instant,
    /// 帧从抓取回调到达到本界面取用的平均耗时（指数滑动平均），单位毫秒
    frame_latency_ms: f32,
    log: Vec<String>,
    capabilities: Vec<(&'static str, Option<bool>)>,
    geometry: Option<TargetGeometry>,
    frame_is_client: Option<bool>,
    hover: Option<MappedPoint>,
    picked: Option<MappedPoint>,
    session: Arc<SharedSession>,
    notes_rx: NotesReceiver,
    activate_first: bool,
    key_spec: String,
    text_input: String,
    relative_move: String,
    tab: PanelTab,
}

/// egui 自带字体不含中日韩字形，需要注入一个系统中文字体作为回退，否则界面全是方块。
fn install_cjk_font(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\NotoSansSC-VF.ttf",
    ];

    for path in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };

        let mut fonts = egui::FontDefinitions::default();
        fonts
            .font_data
            .insert(FONT_KEY.to_owned(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push(FONT_KEY.to_owned());
        }
        ctx.set_fonts(fonts);
        tracing::info!("已加载中文字体 {path}");
        return;
    }

    tracing::warn!("未找到可用的系统中文字体，界面中文可能显示为方块");
}

impl WinPilotApp {
    pub fn new(
        ctx: &egui::Context,
        auto_select: Option<&str>,
        session: Arc<SharedSession>,
        notes_rx: NotesReceiver,
    ) -> Self {
        install_cjk_font(ctx);

        let mut app = Self {
            windows: Vec::new(),
            selected: None,
            generation: 0,
            events_rx: None,
            texture: None,
            frame_size: (0, 0),
            total_frames: 0,
            fps: 0.0,
            fps_count: 0,
            fps_since: Instant::now(),
            frame_latency_ms: 0.0,
            log: Vec::new(),
            capabilities: capability_report(),
            geometry: None,
            frame_is_client: None,
            hover: None,
            picked: None,
            session,
            notes_rx,
            activate_first: true,
            key_spec: String::new(),
            text_input: String::new(),
            relative_move: "0,0".to_owned(),
            tab: PanelTab::Mouse,
        };
        app.refresh_windows();

        if let Some(pattern) = auto_select {
            let lowered = pattern.to_lowercase();
            let found = app
                .windows
                .iter()
                .find(|w| w.title.contains(pattern) || w.process_name.to_lowercase().contains(&lowered))
                .map(|w| w.hwnd);
            match found {
                Some(hwnd) => {
                    app.selected = Some(hwnd);
                    app.start_capture();
                }
                None => app.push_log(format!("--auto 未匹配到 {pattern:?}")),
            }
        }

        app
    }

    fn push_log(&mut self, message: impl Into<String>) {
        let message = message.into();
        tracing::info!(target: "ui_log", "{message}");
        self.log.push(message);
        if self.log.len() > 200 {
            self.log.remove(0);
        }
    }

    /// 输入线程的执行结果回灌到界面日志。
    fn drain_notes(&mut self) {
        while let Ok(message) = self.notes_rx.try_recv() {
            self.push_log(message);
        }
    }

    /// 动作作用在绿色选点上；没有选点时按钮保持禁用。
    fn target_point(&self) -> Option<(i32, i32)> {
        self.picked.and_then(|point| point.screen)
    }

    fn dispatch(&mut self, action: Action) {
        let Some(hwnd) = self.selected else {
            self.push_log("还没有选择目标窗口");
            return;
        };
        let activate = self.activate_first;
        self.session.input.send(InputRequest {
            hwnd,
            activate,
            action,
        });
    }

    fn send_combo(&mut self, spec: &str) {
        match parse_combo(spec) {
            Ok(keys) => self.dispatch(Action::Combo { keys }),
            Err(error) => self.push_log(format!("按键解析失败：{error}")),
        }
    }

    /// 空格分隔的多个组合键会排成队列，由输入线程串行执行。
    fn send_key_spec(&mut self) {
        match parse_sequence(&self.key_spec) {
            Ok(combos) => {
                let count = combos.len();
                for keys in combos {
                    self.dispatch(Action::Combo { keys });
                }
                self.push_log(format!("已排队 {count} 次按键"));
            }
            Err(error) => self.push_log(format!("按键解析失败：{error}")),
        }
    }

    fn send_relative(&mut self) {
        let mut parts = self.relative_move.split(|c| c == ',' || c == ' ');
        let dx = parts.next().and_then(|v| v.trim().parse::<i32>().ok());
        let dy = parts.next().and_then(|v| v.trim().parse::<i32>().ok());
        match (dx, dy) {
            (Some(dx), Some(dy)) => self.dispatch(Action::MoveRel { dx, dy }),
            _ => self.push_log("相对移动格式应为 \"dx,dy\"，例如 \"40,0\""),
        }
    }

    fn refresh_windows(&mut self) {
        match list_capturable_windows() {
            Ok(windows) => {
                self.push_log(format!("枚举到 {} 个可抓取窗口", windows.len()));
                self.selected = self
                    .selected
                    .filter(|hwnd| windows.iter().any(|w| w.hwnd == *hwnd));
                self.windows = windows;
            }
            Err(error) => self.push_log(format!("窗口枚举失败: {error:#}")),
        }
    }

    /// 启动按钮/自动选中：走 session 的唯一入口，并重置本地预览状态。
    fn start_capture(&mut self) {
        let Some(hwnd) = self.selected else { return };
        match self.session.ensure_capture(hwnd) {
            Ok((receiver, geometry)) => {
                self.adopt_receiver(receiver);
                self.geometry = Some(geometry);
                self.generation = self.session.generation();
                self.push_log(
                    self.session
                        .selected()
                        .map(|info| format!("目标 {}", info.label()))
                        .unwrap_or_else(|| format!("抓取已启动 hwnd=0x{hwnd:X}")),
                );
            }
            Err(error) => {
                self.geometry = None;
                self.push_log(format!("启动抓取失败: {error:#}"));
            }
        }
    }

    fn stop_capture(&mut self) {
        self.session.stop_capture();
        self.generation = self.session.generation();
        self.events_rx = None;
        self.geometry = None;
    }

    /// 会话代数变化（MCP 切换了目标或停止）时，向 session 认领新状态。
    /// 抓取会话的生命周期由 session 统一管理，GUI 只是跟随者。
    fn sync_session(&mut self) {
        let generation = self.session.generation();
        if generation == self.generation {
            return;
        }
        self.generation = generation;

        if let Some(info) = self.session.selected() {
            if self.selected != Some(info.hwnd) {
                self.selected = Some(info.hwnd);
                self.push_log(format!("外部切换目标：{}", info.label()));
            }
        }
        match self.session.receiver() {
            Some(receiver) => {
                self.adopt_receiver(receiver);
                self.geometry = self.session.geometry();
                self.push_log("已跟随外部会话变更");
            }
            None => {
                self.events_rx = None;
                self.geometry = None;
            }
        }
    }

    /// 认领一份新的帧接收端：预览与坐标映射状态全部归零重来。
    fn adopt_receiver(&mut self, receiver: CaptureEventReceiver) {
        self.events_rx = Some(receiver);
        self.texture = None;
        self.frame_size = (0, 0);
        self.total_frames = 0;
        self.fps = 0.0;
        self.fps_count = 0;
        self.fps_since = Instant::now();
        self.frame_latency_ms = 0.0;
        self.frame_is_client = None;
        self.hover = None;
        self.picked = None;
    }

    /// 只取最新一帧，中间帧全部丢弃，保证预览延迟最低。返回本次是否用上了新画面。
    fn poll_frames(&mut self, ctx: &egui::Context) -> bool {
        let Some(receiver) = self.events_rx.clone() else {
            return false;
        };

        let mut latest: Option<Arc<FramePacket>> = None;
        let mut target_closed = false;
        let mut applied = false;
        while let Ok(event) = receiver.try_recv() {
            match event {
                CaptureEvent::Frame(packet) => latest = Some(packet),
                CaptureEvent::Closed => target_closed = true,
            }
        }

        if let Some(packet) = latest {
            applied = true;
            let latency_ms = packet.captured_at.elapsed().as_secs_f32() * 1000.0;
            self.frame_latency_ms += (latency_ms - self.frame_latency_ms) * 0.2;

            let (width, height) = (packet.width, packet.height);
            let image = egui::ColorImage::from_rgba_unmultiplied([width, height], &packet.rgba);
            if let Some(texture) = &mut self.texture {
                texture.set(image, egui::TextureOptions::LINEAR);
            } else {
                self.texture =
                    Some(ctx.load_texture("preview", image, egui::TextureOptions::LINEAR));
            }

            self.frame_size = (width, height);
            if self.frame_is_client.is_none() {
                if let Some(geometry) = self.geometry {
                    let aligned = geometry.frame_is_client((width, height));
                    self.frame_is_client = Some(aligned);
                    self.push_log(if aligned {
                        "画面尺寸与客户区一致，坐标映射为恒等换算".to_owned()
                    } else {
                        format!(
                            "画面 {width}×{height} 与客户区 {}×{} 不一致，改用比例换算",
                            geometry.client_w, geometry.client_h
                        )
                    });
                }
            }
            self.total_frames += 1;
            self.fps_count += 1;
            let elapsed = self.fps_since.elapsed().as_secs_f32();
            if elapsed >= 0.5 {
                self.fps = self.fps_count as f32 / elapsed;
                self.fps_count = 0;
                self.fps_since = Instant::now();
            }
        }

        if target_closed {
            self.push_log("目标窗口已关闭，抓取结束");
            self.events_rx = None;
            self.texture = None;
            self.frame_size = (0, 0);
        }

        applied
    }

    /// 把预览控件上的一个点换算成画面像素 / 客户区 / 屏幕三套坐标。
    fn map_point(
        geometry: &TargetGeometry,
        hwnd: isize,
        frame: (usize, usize),
        rect: egui::Rect,
        on_screen: egui::Pos2,
    ) -> Option<MappedPoint> {
        if rect.width() <= 0.0 || rect.height() <= 0.0 {
            return None;
        }
        let image = (
            (on_screen.x - rect.min.x) / rect.width() * frame.0 as f32,
            (on_screen.y - rect.min.y) / rect.height() * frame.1 as f32,
        );
        let client = image_to_client(image, frame, geometry)?;
        let screen = geometry.client_to_screen(hwnd, client.0, client.1).ok();
        Some(MappedPoint {
            image,
            client,
            screen,
        })
    }

    fn preview_ui(&mut self, ui: &mut egui::Ui) {
        self.hover = None;

        let frame = self.frame_size;
        let rect = match self.texture.as_ref() {
            Some(texture) if frame.0 > 0 && frame.1 > 0 => {
                let available = ui.available_size();
                let scale = (available.x / frame.0 as f32)
                    .min(available.y / frame.1 as f32)
                    .max(0.01);
                let size = egui::vec2(frame.0 as f32 * scale, frame.1 as f32 * scale);
                let offset = (available - size) * 0.5;

                ui.add_space(offset.y.max(0.0));
                ui.horizontal(|ui| {
                    ui.add_space(offset.x.max(0.0));
                    ui.add(egui::Image::from_texture(texture).fit_to_exact_size(size)).rect
                })
                .inner
            }
            _ => {
                ui.vertical_centered(|ui| {
                    ui.add_space(40.0);
                    ui.label("左侧为窗口画面预览区。");
                    ui.label("在右侧选择目标窗口，然后点击「开始抓取」。");
                });
                return;
            }
        };

        let pointer = ui.input(|state| state.pointer.hover_pos());
        let clicked = ui.input(|state| state.pointer.button_clicked(egui::PointerButton::Primary));

        if let (Some(geometry), Some(hwnd), Some(point)) = (self.geometry, self.selected, pointer) {
            if rect.contains(point) {
                let mapped = Self::map_point(&geometry, hwnd, frame, rect, point);
                self.hover = mapped;
                if clicked {
                    self.picked = mapped;
                    if let Some(mapped) = mapped {
                        self.push_log(format!(
                            "选点：画面({:.0},{:.0}) → 客户区({},{}) → 屏幕{:?}",
                            mapped.image.0, mapped.image.1, mapped.client.0, mapped.client.1,
                            mapped.screen
                        ));
                    }
                }
            }
        }

        let painter = ui.painter();
        let crosshair = |point: egui::Pos2, color: egui::Color32| {
            let stroke = egui::Stroke::new(1.0, color);
            painter.line_segment([egui::pos2(rect.min.x, point.y), egui::pos2(rect.max.x, point.y)], stroke);
            painter.line_segment([egui::pos2(point.x, rect.min.y), egui::pos2(point.x, rect.max.y)], stroke);
            painter.circle_stroke(point, 5.0, stroke);
        };

        if let (Some(picked), Some(_)) = (self.picked, self.geometry) {
            crosshair(
                egui::pos2(
                    rect.min.x + picked.image.0 / frame.0 as f32 * rect.width(),
                    rect.min.y + picked.image.1 / frame.1 as f32 * rect.height(),
                ),
                egui::Color32::from_rgb(60, 220, 120),
            );
        }
        if let (Some(hover), Some(_)) = (self.hover, self.geometry) {
            crosshair(
                egui::pos2(
                    rect.min.x + hover.image.0 / frame.0 as f32 * rect.width(),
                    rect.min.y + hover.image.1 / frame.1 as f32 * rect.height(),
                ),
                egui::Color32::from_rgb(255, 80, 80),
            );
        }
    }

    fn aim_picked(&mut self) {
        let (Some(picked), Some(geometry), Some(hwnd)) =
            (self.picked, self.geometry, self.selected)
        else {
            return;
        };
        match aim_cursor(hwnd, &geometry, self.frame_size, picked.image) {
            Ok(screen) => self.push_log(format!(
                "光标移到屏幕 {screen:?}，对应客户区 ({},{})（只移动，未点击）",
                picked.client.0, picked.client.1
            )),
            Err(error) => self.push_log(format!("光标定位失败: {error:#}")),
        }
    }

    fn point_row(ui: &mut egui::Ui, label: &str, point: Option<MappedPoint>) {
        let Some(point) = point else {
            ui.monospace(format!("{label}：—"));
            return;
        };
        let screen = match point.screen {
            Some((x, y)) => format!("({x},{y})"),
            None => "换算失败".to_owned(),
        };
        ui.monospace(format!(
            "{label}：客户区 ({},{})  屏幕 {}",
            point.client.0, point.client.1, screen
        ));
    }

    /// 分组框默认按内容收缩，这里显式撑满面板宽度，避免左右边缘参差不齐。
    fn section(ui: &mut egui::Ui, title: &str, add_contents: impl FnOnce(&mut egui::Ui)) {
        let inner_width = (ui.available_width() - 24.0).max(140.0);
        ui.group(|ui| {
            ui.set_min_width(inner_width);
            ui.label(egui::RichText::new(title).strong());
            ui.separator();
            add_contents(ui);
        });
        ui.add_space(6.0);
    }

    fn controls_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("WinPilot");
        ui.separator();

        // 各分区加起来超过窗口高度，整体可滚动，任何分区都不会被裁掉
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.status_section(ui);
                self.target_section(ui);
                self.mapping_section(ui);
                self.action_section(ui);
            });
    }

    /// 运行状态读数。延迟与积压是判断"点了没反应"到底卡在哪一环节的依据。
    fn status_section(&mut self, ui: &mut egui::Ui) {
        let capturing = self.session.capture_alive() || self.events_rx.is_some();
        let alive = self.session.capture_alive();
        let backlog = self.session.input.backlog();
        let (width, height) = self.frame_size;

        Self::section(ui, "状态", |ui| {
            let state = if !capturing {
                ("未抓取", egui::Color32::GRAY)
            } else if alive {
                ("抓取中", egui::Color32::from_rgb(70, 200, 130))
            } else {
                ("会话已终止", egui::Color32::RED)
            };
            ui.colored_label(state.1, state.0);

            if capturing {
                ui.monospace(format!(
                    "画面 {:>4}×{:<4} 帧率 {:>5.1}fps",
                    width, height, self.fps
                ));
                let latency = self.frame_latency_ms;
                let color = if latency > 80.0 {
                    egui::Color32::RED
                } else if latency > 40.0 {
                    egui::Color32::YELLOW
                } else {
                    egui::Color32::from_rgb(70, 200, 130)
                };
                ui.horizontal(|ui| {
                    ui.monospace("画面延迟");
                    ui.colored_label(color, format!("{latency:>5.1} ms"));
                    ui.weak("抓取→界面");
                });
                ui.monospace(format!("已显示帧数 {:>6}", self.total_frames));
            }

            ui.monospace(format!(
                "注入待执行 {backlog:>3}{}",
                if backlog > 3 { "  ← 发送过快" } else { "" }
            ));
        });
    }

    fn target_section(&mut self, ui: &mut egui::Ui) {
        let selected_label = self
            .selected
            .and_then(|hwnd| self.windows.iter().find(|w| w.hwnd == hwnd).map(|w| w.label()));
        let capturing = self.session.capture_alive();

        Self::section(ui, "目标窗口", |ui| {
            ui.horizontal(|ui| {
                if ui.button("刷新列表").clicked() {
                    self.refresh_windows();
                }
                ui.weak(selected_label.unwrap_or_else(|| "未选择".to_owned()));
            });

            let count = self.windows.len();
            egui::CollapsingHeader::new(format!("窗口列表（{count}）"))
                .default_open(true)
                .show(ui, |ui| {
                    // 取出 vec 再遍历，避免与 self.selected 的可变借用冲突
                    let windows = std::mem::take(&mut self.windows);
                    egui::ScrollArea::vertical()
                        .max_height(170.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if windows.is_empty() {
                                ui.weak("没有可用窗口");
                            }
                            for info in &windows {
                                let label = info.label();
                                ui.selectable_value(&mut self.selected, Some(info.hwnd), label);
                            }
                        });
                    self.windows = windows;
                });

            ui.add_space(4.0);
            if capturing {
                if ui.button("停止抓取").clicked() {
                    self.stop_capture();
                }
            } else {
                let button = egui::Button::new("开始抓取").min_size(egui::vec2(120.0, 26.0));
                if ui.add_enabled(self.selected.is_some(), button).clicked() {
                    self.start_capture();
                }
                if self.selected.is_none() {
                    ui.weak("先在上方列表里选一个窗口");
                }
            }
        });
    }

    fn mapping_section(&mut self, ui: &mut egui::Ui) {
        let geometry = self.geometry;
        let frame_is_client = self.frame_is_client;
        let hover = self.hover;
        let picked = self.picked;

        Self::section(ui, "坐标映射", |ui| {
            match geometry {
                Some(geometry) => {
                    ui.label(format!(
                        "客户区 {}×{} · 窗口 {}×{} · DPI {} · {}",
                        geometry.client_w,
                        geometry.client_h,
                        geometry.window_w,
                        geometry.window_h,
                        geometry.dpi,
                        geometry.awareness
                    ));
                    match frame_is_client {
                        Some(true) => {
                            ui.colored_label(
                                egui::Color32::from_rgb(70, 200, 130),
                                "画面 = 客户区，恒等换算",
                            );
                        }
                        Some(false) => {
                            ui.colored_label(
                                egui::Color32::YELLOW,
                                "画面 ≠ 客户区，已改用比例换算",
                            );
                        }
                        None => {
                            ui.weak("等待首帧…");
                        }
                    }
                }
                None => {
                    ui.weak("开始抓取后可用");
                }
            }

            ui.separator();
            Self::point_row(ui, "指向", hover);
            Self::point_row(ui, "选点", picked);

            ui.horizontal(|ui| {
                if ui
                    .add_enabled(picked.is_some(), egui::Button::new("清除选点"))
                    .clicked()
                {
                    self.picked = None;
                }
                let can_aim = picked.is_some() && geometry.is_some();
                if ui
                    .add_enabled(can_aim, egui::Button::new("光标移到选点"))
                    .on_disabled_hover_text("先在左侧预览区点一个点")
                    .clicked()
                {
                    self.aim_picked();
                }
            });
        });
    }

    fn action_section(&mut self, ui: &mut egui::Ui) {
        let log_count = self.log.len();
        Self::section(ui, "操作", |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, PanelTab::Mouse, "鼠标");
                ui.selectable_value(&mut self.tab, PanelTab::Keyboard, "键盘");
                ui.selectable_value(&mut self.tab, PanelTab::Log, format!("日志({log_count})"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.checkbox(&mut self.activate_first, "先激活");
                });
            });
            ui.separator();

            match self.tab {
                PanelTab::Mouse => self.mouse_rows(ui),
                PanelTab::Keyboard => self.keyboard_rows(ui),
                PanelTab::Log => self.log_rows(ui),
            }
        });
    }

    fn mouse_rows(&mut self, ui: &mut egui::Ui) {
        let has_point = self.target_point().is_some();
        if !has_point {
            ui.weak("先在左侧预览区点一个点作为动作位置");
        }

        ui.horizontal_wrapped(|ui| {
            for (label, button, count) in [
                ("左键单击", MouseButton::Left, 1_u32),
                ("左键双击", MouseButton::Left, 2),
                ("右键单击", MouseButton::Right, 1),
                ("中键单击", MouseButton::Middle, 1),
            ] {
                let widget = egui::Button::new(label).min_size(egui::vec2(82.0, 24.0));
                if ui.add_enabled(has_point, widget).clicked()
                    && let Some(screen) = self.target_point()
                {
                    self.dispatch(Action::Click {
                        screen,
                        button,
                        count,
                    });
                }
            }
        });

        ui.horizontal_wrapped(|ui| {
            if ui.button("滚轮上").clicked() {
                self.dispatch(Action::Scroll { ticks: 3 });
            }
            if ui.button("滚轮下").clicked() {
                self.dispatch(Action::Scroll { ticks: -3 });
            }
            if ui.button("按下左键").clicked() {
                self.dispatch(Action::Button {
                    button: MouseButton::Left,
                    down: true,
                });
            }
            if ui.button("抬起左键").clicked() {
                self.dispatch(Action::Button {
                    button: MouseButton::Left,
                    down: false,
                });
            }
        });

        ui.horizontal(|ui| {
            ui.label("相对移动");
            ui.add(
                egui::TextEdit::singleline(&mut self.relative_move)
                    .desired_width(70.0)
                    .hint_text("dx,dy"),
            );
            if ui.button("发送").clicked() {
                self.send_relative();
            }
        });
        ui.weak("相对移动走 raw input，供鼠标被游戏捕获时使用");
    }

    fn keyboard_rows(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.key_spec)
                    .desired_width(200.0)
                    .hint_text("Ctrl+Left 或 Left Left Space"),
            );
            if ui.button("发送").clicked() {
                self.send_key_spec();
            }
        });

        ui.horizontal_wrapped(|ui| {
            for key in ["Left", "Right", "Up", "Down", "Space", "Enter", "Esc"] {
                if ui.small_button(key).clicked() {
                    self.send_combo(key);
                }
            }
        });

        ui.separator();
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.text_input)
                    .desired_width(200.0)
                    .hint_text("要输入的文本"),
            );
            if ui.button("输入").clicked() {
                let text = std::mem::take(&mut self.text_input);
                if text.is_empty() {
                    self.push_log("文本为空，未发送");
                } else {
                    self.dispatch(Action::Text { text });
                }
            }
        });
        ui.weak("文本逐码元走 KEYEVENTF_UNICODE，不受键盘布局影响");
    }

    fn log_rows(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("系统自检")
            .default_open(false)
            .show(ui, |ui| {
                for (name, supported) in &self.capabilities {
                    ui.horizontal(|ui| {
                        let mark = match supported {
                            Some(true) => "✔",
                            Some(false) => "✘",
                            None => "?",
                        };
                        ui.label(mark);
                        ui.label(*name);
                    });
                }
            });

        if ui.button("清空日志").clicked() {
            self.log.clear();
        }

        egui::ScrollArea::vertical()
            .max_height(260.0)
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if self.log.is_empty() {
                    ui.weak("暂无日志");
                }
                for line in &self.log {
                    ui.label(line);
                }
            });
    }
}

impl eframe::App for WinPilotApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.sync_session();
        let fresh = self.poll_frames(&ctx);
        self.drain_notes();

        egui::Panel::right("controls")
            .exact_size(CONTROLS_WIDTH)
            .resizable(false)
            .show(ui, |ui| self.controls_ui(ui));

        egui::CentralPanel::default_margins().show(ui, |ui| self.preview_ui(ui));

        // 重绘请求放在最后发：刚用上新一帧就立刻再来一次，让预览贴着抓取节奏走；
        // 其余情况退到 250ms 心跳——除了刷新状态读数，还要及时处理 MCP 的 select_window，
        // 空闲时界面没有其他重绘来源，漏掉心跳会让外部请求永远等不到执行。
        if fresh {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }
}
