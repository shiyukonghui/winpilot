mod app;
mod capture;
mod geometry;
mod input;
mod keys;
mod types;
mod window_list;

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use app::WinPilotApp;
use capture::CaptureWorker;
use crossbeam_channel::unbounded;
use types::{Action, CaptureEvent, CaptureEventReceiver, FramePacket, InputRequest};
use window_list::list_capturable_windows;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .init();

    let args: Vec<String> = std::env::args().collect();
    if let Some(pattern) = flag_value(&args, "--capture") {
        return capture_self_test(pattern);
    }
    if let Some(pattern) = flag_value(&args, "--map") {
        return mapping_self_test(pattern, &args);
    }
    if let Some(pattern) = flag_value(&args, "--send") {
        let specs: Vec<String> = args
            .iter()
            .skip_while(|a| *a != "--send")
            .skip(2)
            .cloned()
            .collect();
        return send_self_test(pattern, &specs);
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 780.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("WinPilot"),
        ..Default::default()
    };

    let auto = flag_value(&args, "--auto").map(str::to_owned);
    eframe::run_native(
        "WinPilot",
        options,
        Box::new(move |cc| Ok(Box::new(WinPilotApp::new(&cc.egui_ctx, auto.as_deref())))),
    )
    .context("界面初始化失败")?;

    Ok(())
}

fn flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// 无界面验证抓取链路：按标题/进程名匹配窗口，启动会话并统计收到的帧。
fn capture_self_test(pattern: &str) -> Result<()> {
    let windows = list_capturable_windows()?;
    let lowered = pattern.to_lowercase();
    let target = windows.iter().find(|w| {
        w.title.contains(pattern) || w.process_name.to_lowercase().contains(&lowered)
    });

    let Some(info) = target else {
        println!("[selftest] 未匹配到 {pattern:?}，当前可抓取窗口：");
        for w in &windows {
            println!("  {}", w.label());
        }
        return Ok(());
    };

    println!(
        "[selftest] 目标 {} hwnd=0x{:X} 窗口 {}×{}",
        info.label(),
        info.hwnd,
        info.width,
        info.height
    );

    let (worker, receiver) = CaptureWorker::start(info.hwnd)?;
    let started = Instant::now();
    let mut frames = 0_u32;

    while frames < 20 && started.elapsed() < Duration::from_secs(6) {
        match receiver.recv_timeout(Duration::from_millis(500)) {
            Ok(CaptureEvent::Frame(packet)) => {
                frames += 1;
                if frames <= 3 || frames % 5 == 0 {
                    println!(
                        "[selftest] 帧 {frames}: {}×{} 字节={} 用时={:.2}s",
                        packet.width,
                        packet.height,
                        packet.rgba.len(),
                        started.elapsed().as_secs_f32()
                    );
                }
            }
            Ok(CaptureEvent::Closed) => {
                println!("[selftest] 目标窗口关闭，会话结束");
                break;
            }
            Err(_) => {}
        }
    }

    println!(
        "[selftest] 共收到 {frames} 帧，耗时 {:.2}s，会话存活={}",
        started.elapsed().as_secs_f32(),
        worker.is_alive()
    );
    worker.stop();
    Ok(())
}

/// 注入自检：向目标发送一串指令，用画面变化证明输入真的被目标接收并生效。
///
/// 指令形式：`Left` / `Ctrl+S` 等按键；`click:fx,fy` 归一化点击；`text:内容` 文本；
/// `rmove:dx,dy` 相对移动。
fn send_self_test(pattern: &str, specs: &[String]) -> Result<()> {
    let windows = list_capturable_windows()?;
    let lowered = pattern.to_lowercase();
    let Some(info) = windows.iter().find(|w| {
        w.title.contains(pattern) || w.process_name.to_lowercase().contains(&lowered)
    }) else {
        println!("[send] 未匹配到 {pattern:?}");
        return Ok(());
    };
    println!("[send] 目标 {}", info.label());
    let hwnd = info.hwnd;
    let geometry = geometry::TargetGeometry::query(hwnd)?;

    let (worker, receiver) = CaptureWorker::start(hwnd)?;
    let first = latest_frame(&receiver).context("没收到画面，无法换算点击坐标")?;
    let frame_size = (first.width, first.height);
    println!("[send] 画面 {frame_size:?} 客户区 {}×{}", geometry.client_w, geometry.client_h);

    let mut actions = Vec::new();
    for spec in specs {
        actions.push(build_action(spec, hwnd, &geometry, frame_size).map_err(anyhow::Error::msg)?);
    }
    println!("[send] 发送 {} 条指令", actions.len());

    let (notes_tx, notes_rx) = unbounded();
    let input = input::InputWorker::spawn(notes_tx);
    drop(first);

    let before = latest_frame(&receiver);
    println!("[send] 发送前 {}", describe(before.as_ref()));

    for action in actions {
        input.send(InputRequest {
            hwnd,
            activate: true,
            action,
        });
        std::thread::sleep(Duration::from_millis(160));
    }

    let settle = Instant::now() + Duration::from_millis(800);
    while Instant::now() < settle {
        while let Ok(note) = notes_rx.try_recv() {
            println!("[send]   {note}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    while let Ok(note) = notes_rx.try_recv() {
        println!("[send]   {note}");
    }

    let after = latest_frame(&receiver);
    println!("[send] 发送后 {}", describe(after.as_ref()));
    if let (Some(before), Some(after)) = (&before, &after) {
        let (changed, bbox) = diff_frames(before, after);
        println!("[send] 变化像素 {changed}，包围盒 {bbox:?}");
    }

    worker.stop();
    Ok(())
}

/// 把命令行指令翻译成动作。
fn build_action(
    spec: &str,
    hwnd: isize,
    geometry: &geometry::TargetGeometry,
    frame: (usize, usize),
) -> Result<Action, String> {
    if let Some(text) = spec.strip_prefix("text:") {
        return Ok(Action::Text {
            text: text.to_owned(),
        });
    }

    if let Some(pair) = spec.strip_prefix("click:") {
        let (fx, fy) = pair.split_once(',').ok_or("click 格式应为 click:fx,fy")?;
        let fx: f32 = fx.trim().parse().map_err(|_| "fx 不是数字")?;
        let fy: f32 = fy.trim().parse().map_err(|_| "fy 不是数字")?;
        let image = (fx * frame.0 as f32, fy * frame.1 as f32);
        let client = geometry::image_to_client(image, frame, geometry).ok_or("换算失败")?;
        let screen = geometry
            .client_to_screen(hwnd, client.0, client.1)
            .map_err(|error| error.to_string())?;
        println!("[send] click ({fx},{fy}) → 客户区({},{}) → 屏幕{screen:?}", client.0, client.1);
        return Ok(Action::Click {
            screen,
            button: types::MouseButton::Left,
            count: 1,
        });
    }

    if let Some(pair) = spec.strip_prefix("rmove:") {
        let (dx, dy) = pair.split_once(',').ok_or("rmove 格式应为 rmove:dx,dy")?;
        let dx: i32 = dx.trim().parse().map_err(|_| "dx 不是整数")?;
        let dy: i32 = dy.trim().parse().map_err(|_| "dy 不是整数")?;
        return Ok(Action::MoveRel { dx, dy });
    }

    keys::parse_combo(spec).map(|keys| Action::Combo { keys })
}

/// 取一小段时间内的最后一帧，代表该时刻的稳定画面。
fn latest_frame(receiver: &CaptureEventReceiver) -> Option<FramePacket> {
    let mut last = None;
    let deadline = Instant::now() + Duration::from_millis(400);
    while Instant::now() < deadline {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(CaptureEvent::Frame(packet)) => last = Some(packet),
            Ok(CaptureEvent::Closed) => break,
            Err(_) => {}
        }
    }
    last
}

fn describe(frame: Option<&FramePacket>) -> String {
    match frame {
        Some(frame) => format!(
            "{}×{} 校验和={:016x}",
            frame.width,
            frame.height,
            fnv(&frame.rgba)
        ),
        None => "无画面".to_owned(),
    }
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn diff_frames(a: &FramePacket, b: &FramePacket) -> (u64, Option<(usize, usize, usize, usize)>) {
    if a.width != b.width || a.height != b.height {
        return (u64::MAX, None);
    }

    let mut changed = 0_u64;
    let (mut x0, mut y0) = (usize::MAX, usize::MAX);
    let (mut x1, mut y1) = (0_usize, 0_usize);

    for y in 0..a.height {
        for x in 0..a.width {
            let offset = (y * a.width + x) * 4;
            if a.rgba[offset..offset + 4] != b.rgba[offset..offset + 4] {
                changed += 1;
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }

    let bbox = if changed > 0 {
        Some((x0, y0, x1, y1))
    } else {
        None
    };
    (changed, bbox)
}
/// 验证坐标映射：抓一帧 -> 换算出客户区/屏幕坐标 -> 把真实光标移过去并回读。
fn mapping_self_test(pattern: &str, args: &[String]) -> Result<()> {
    let frac = |name: &str| -> f32 {
        flag_value(args, name)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.5)
    };
    let (fx, fy) = (frac("--fx"), frac("--fy"));

    let windows = list_capturable_windows()?;
    let lowered = pattern.to_lowercase();
    let Some(info) = windows.iter().find(|w| {
        w.title.contains(pattern) || w.process_name.to_lowercase().contains(&lowered)
    }) else {
        println!("[map] 未匹配到 {pattern:?}");
        return Ok(());
    };
    println!("[map] 目标 {}", info.label());

    let geometry = geometry::TargetGeometry::query(info.hwnd)?;
    println!(
        "[map] 客户区 {}×{} 窗口 {}×{} DPI={} 感知={}",
        geometry.client_w,
        geometry.client_h,
        geometry.window_w,
        geometry.window_h,
        geometry.dpi,
        geometry.awareness
    );

    let (worker, receiver) = CaptureWorker::start(info.hwnd)?;
    let mut frame = (0_usize, 0_usize);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(CaptureEvent::Frame(packet)) = receiver.recv_timeout(Duration::from_millis(400)) {
            frame = (packet.width, packet.height);
            break;
        }
    }
    if frame.0 == 0 {
        println!("[map] 5s 内未收到帧（目标静止），无法验证");
        worker.stop();
        return Ok(());
    }
    println!(
        "[map] 画面 {}×{}，画面==客户区：{}",
        frame.0,
        frame.1,
        geometry.frame_is_client(frame)
    );

    let image = (fx * frame.0 as f32, fy * frame.1 as f32);
    let Some(client) = geometry::image_to_client(image, frame, &geometry) else {
        println!("[map] 换算失败");
        worker.stop();
        return Ok(());
    };
    let expected = geometry.client_to_screen(info.hwnd, client.0, client.1)?;
    println!(
        "[map] 归一化({fx},{fy}) → 画面({:.1},{:.1}) → 客户区({},{}) → 屏幕({},{})",
        image.0, image.1, client.0, client.1, expected.0, expected.1
    );

    let aimed = geometry::aim_cursor(info.hwnd, &geometry, frame, image)?;
    let actual = geometry::cursor_pos();
    println!(
        "[map] 光标移到 {aimed:?}，系统回读 {actual:?}，一致={}",
        aimed == actual && aimed == expected
    );
    worker.stop();
    Ok(())
}
