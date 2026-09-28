use std::time::Instant;

use crate::capture::{CaptureWorker, capability_report};
use crate::geometry::{TargetGeometry, aim_cursor, image_to_client};
use crate::input::{InputWorker, NotesReceiver};
use crate::keys::{parse_combo, parse_sequence};
use crate::types::{Action, CaptureEvent, CaptureEventReceiver, FramePacket, InputRequest, MouseButton, WindowInfo};
use crate::window_list::list_capturable_windows;
use crossbeam_channel::unbounded;

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

pub struct WinPilotApp {
    windows: Vec<WindowInfo>,
    selected: Option<isize>,
    worker: Option<CaptureWorker>,
    events_rx: Option<CaptureEventReceiver>,
    texture: Option<egui::TextureHandle>,
    frame_size: (usize, usize),
    total_frames: u64,
    fps: f32,
    fps_count: u32,
    fps_since: Instant,
    log: Vec<String>,
    capabilities: Vec<(&'static str, Option<bool>)>,
    geometry: Option<TargetGeometry>,
    frame_is_client: Option<bool>,
    hover: Option<MappedPoint>,
    picked: Option<MappedPoint>,
    input: InputWorker,
    notes_rx: NotesReceiver,
    activate_first: bool,
    key_spec: String,
    text_input: String,
    relative_move: String,
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
    pub fn new(ctx: &egui::Context, auto_select: Option<&str>) -> Self {
        install_cjk_font(ctx);

        let (notes_tx, notes_rx) = unbounded();
        let input = InputWorker::spawn(notes_tx);

        let mut app = Self {
            windows: Vec::new(),
            selected: None,
            worker: None,
            events_rx: None,
            texture: None,
            frame_size: (0, 0),
            total_frames: 0,
            fps: 0.0,
            fps_count: 0,
            fps_since: Instant::now(),
            log: Vec::new(),
            capabilities: capability_report(),
            geometry: None,
            frame_is_client: None,
            hover: None,
            picked: None,
            input,
            notes_rx,
            activate_first: true,
            key_spec: String::new(),
            text_input: String::new(),
            relative_move: "0,0".to_owned(),
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
        self.input.send(InputRequest {
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

    fn start_capture(&mut self) {
        self.stop_capture();

        let Some(hwnd) = self.selected else { return };
        match CaptureWorker::start(hwnd) {
            Ok((worker, events_rx)) => {
                self.worker = Some(worker);
                self.events_rx = Some(events_rx);
                self.texture = None;
                self.frame_size = (0, 0);
                self.total_frames = 0;
                self.fps = 0.0;
                self.fps_count = 0;
                self.fps_since = Instant::now();
                self.frame_is_client = None;
                self.hover = None;
                self.picked = None;
                self.push_log(format!("抓取已启动 hwnd=0x{hwnd:X}"));

                self.geometry = match TargetGeometry::query(hwnd) {
                    Ok(geometry) => {
                        self.push_log(format!(
                            "客户区 {}×{} 窗口 {}×{} DPI={} 感知={}",
                            geometry.client_w,
                            geometry.client_h,
                            geometry.window_w,
                            geometry.window_h,
                            geometry.dpi,
                            geometry.awareness
                        ));
                        Some(geometry)
                    }
                    Err(error) => {
                        self.push_log(format!("读取窗口几何失败: {error:#}"));
                        None
                    }
                };
            }
            Err(error) => {
                self.geometry = None;
                self.push_log(format!("启动抓取失败: {error:#}"));
            }
        }
    }

    fn stop_capture(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.stop();
            self.push_log("抓取已停止");
        }
        self.events_rx = None;
    }

    /// 只取最新一帧，中间帧全部丢弃，保证预览延迟最低。
    fn poll_frames(&mut self, ctx: &egui::Context) {
        let Some(receiver) = self.events_rx.clone() else {
            return;
        };

        let mut latest: Option<FramePacket> = None;
        let mut target_closed = false;
        while let Ok(event) = receiver.try_recv() {
            match event {
                CaptureEvent::Frame(packet) => latest = Some(packet),
                CaptureEvent::Closed => target_closed = true,
            }
        }

        if let Some(FramePacket {
            rgba,
            width,
            height,
        }) = latest
        {
            let image = egui::ColorImage::from_rgba_unmultiplied([width, height], &rgba);
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

            ctx.request_repaint();
        }

        if target_closed {
            self.push_log("目标窗口已关闭，抓取结束");
            self.worker = None;
            self.events_rx = None;
            self.texture = None;
            self.frame_size = (0, 0);
        }
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

    fn point_row(&self, ui: &mut egui::Ui, label: &str, point: Option<MappedPoint>) {
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

    fn controls_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("WinPilot");
        ui.separator();

        ui.group(|ui| {
            ui.label("目标窗口");
            if ui.button("刷新列表").clicked() {
                self.refresh_windows();
            }

            // 取出 vec 再遍历，避免与 self.selected 的可变借用冲突
            let windows = std::mem::take(&mut self.windows);
            egui::ScrollArea::vertical()
                .max_height(220.0)
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

        ui.group(|ui| {
            ui.label("画面抓取");
            if self.worker.is_some() {
                if ui.button("停止抓取").clicked() {
                    self.stop_capture();
                }
            } else {
                let button = ui.add_enabled(
                    self.selected.is_some(),
                    egui::Button::new("开始抓取").min_size(egui::vec2(200.0, 26.0)),
                );
                if button.clicked() {
                    self.start_capture();
                }
                if self.selected.is_none() {
                    ui.weak("请先在上方选择一个窗口");
                }
            }
        });

        ui.group(|ui| {
            ui.label("坐标映射");
            match self.geometry {
                Some(geometry) => {
                    ui.label(format!(
                        "客户区 {}×{}  窗口 {}×{}",
                        geometry.client_w, geometry.client_h, geometry.window_w, geometry.window_h
                    ));
                    ui.label(format!("DPI {}  感知 {}", geometry.dpi, geometry.awareness));
                    match self.frame_is_client {
                        Some(true) => {
                            ui.colored_label(
                                egui::Color32::from_rgb(70, 200, 130),
                                "画面尺寸 = 客户区，映射为恒等换算",
                            );
                        }
                        Some(false) => {
                            ui.colored_label(
                                egui::Color32::YELLOW,
                                "画面尺寸 ≠ 客户区，已改用比例换算",
                            );
                        }
                        None => {
                            ui.weak("等待首帧…");
                        }
                    }
                }
                None => { ui.weak("开始抓取后可用"); }
            }

            ui.separator();
            self.point_row(ui, "鼠标指向", self.hover);
            self.point_row(ui, "已选中  ", self.picked);

            ui.horizontal(|ui| {
                if ui
                    .add_enabled(self.picked.is_some(), egui::Button::new("清除选点"))
                    .clicked()
                {
                    self.picked = None;
                }

                let can_aim =
                    self.picked.is_some() && self.geometry.is_some() && self.selected.is_some();
                if ui
                    .add_enabled(can_aim, egui::Button::new("把光标移到选点"))
                    .on_disabled_hover_text("先在左侧预览区点击一个点")
                    .clicked()
                {
                    if let (Some(picked), Some(geometry), Some(hwnd)) =
                        (self.picked, self.geometry, self.selected)
                    {
                        match aim_cursor(hwnd, &geometry, self.frame_size, picked.image) {
                            Ok(screen) => self.push_log(format!(
                                "光标已移到屏幕 ({},{})，对应客户区 ({},{})（只移动，未点击）",
                                screen.0, screen.1, picked.client.0, picked.client.1
                            )),
                            Err(error) => {
                                self.push_log(format!("光标定位失败: {error:#}"));
                            }
                        }
                    }
                }
            });
        });

        ui.group(|ui| {
            ui.label("鼠标动作");
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
                    let button_widget =
                        egui::Button::new(label).min_size(egui::vec2(82.0, 24.0));
                    if ui
                        .add_enabled(has_point, button_widget)
                        .clicked()
                    {
                        if let Some(screen) = self.target_point() {
                            self.dispatch(Action::Click {
                                screen,
                                button,
                                count,
                            });
                        }
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
        });

        ui.group(|ui| {
            ui.label("键盘动作");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.key_spec)
                        .desired_width(210.0)
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

            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.text_input)
                        .desired_width(210.0)
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
        });

        ui.group(|ui| {
            ui.checkbox(&mut self.activate_first, "动作前先激活目标窗口（前台模式）");
        });

        ui.group(|ui| {
            let session_alive = self.worker.as_ref().is_some_and(|worker| worker.is_alive());
            let state = match (&self.worker, session_alive) {
                (None, _) => "空闲",
                (Some(_), true) => "抓取中",
                (Some(_), false) => "会话已终止",
            };
            ui.label(format!("状态：{state}"));
            ui.label(format!(
                "画面尺寸：{}×{}",
                self.frame_size.0, self.frame_size.1
            ));
            ui.label(format!("帧率：{:.1} fps", self.fps));
            ui.label(format!("累计帧数：{}", self.total_frames));
        });

        ui.collapsing("系统自检", |ui| {
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

        ui.collapsing("日志", |ui| {
            egui::ScrollArea::vertical()
                .max_height(160.0)
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
        });
    }
}

impl eframe::App for WinPilotApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_frames(&ctx);
        self.drain_notes();
        if self.worker.is_some() {
            // 目标画面静止时 WGC 不再产帧，这里保持低频重绘以更新帧率与会话状态
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }

        egui::Panel::right("controls")
            .exact_size(CONTROLS_WIDTH)
            .resizable(false)
            .show(ui, |ui| self.controls_ui(ui));

        egui::CentralPanel::default_margins().show(ui, |ui| self.preview_ui(ui));
    }
}
