mod app;
mod capture;
mod geometry;
mod input;
mod keys;
mod mcp_server;
mod session;
mod types;
mod window_list;

use std::sync::Arc;
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
    if let Some(pattern) = flag_value(&args, "--latency") {
        let specs: Vec<String> = args
            .iter()
            .skip_while(|a| *a != "--latency")
            .skip(2)
            .cloned()
            .collect();
        return latency_self_test(pattern, &specs);
    }
    if let Some(index) = args.iter().position(|a| a == "--burst") {
        let rest: Vec<&str> = args[index + 1..].iter().map(String::as_str).collect();
        let Some(pattern) = rest.first().copied() else {
            println!("[burst] 用法：--burst <窗口关键字> <按键> <条数>");
            return Ok(());
        };
        let spec = rest.get(1).copied().unwrap_or("A");
        let count = rest.get(2).and_then(|v| v.parse().ok()).unwrap_or(10);
        return burst_self_test(pattern, spec, count);
    }

    let auto = flag_value(&args, "--auto").map(str::to_owned);
    let headless = args.iter().any(|a| a == "--serve");

    // 共享会话：GUI、无头模式与 MCP server 操作同一个实例
    let (notes_tx, notes_rx) = unbounded();
    let input = input::InputWorker::spawn(notes_tx.clone());
    let session = session::SharedSession::start(input, notes_tx);

    if headless {
        // 无头模式：不启动界面，MCP server 是唯一操控入口；--serve 默认开启 8100 端口
        let port = mcp_port(&args).unwrap_or(8100);
        if let Some(pattern) = auto.as_deref() {
            headless_autostart(&session, pattern)?;
        }
        mcp_server::start(session, Some(port));
        let logger = std::thread::spawn(move || {
            while let Ok(note) = notes_rx.recv() {
                tracing::info!(target: "input", "{note}");
            }
        });
        let _ = logger;
        tracing::info!("WinPilot 无头模式运行中（无界面），Ctrl+C 退出");
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 780.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("WinPilot"),
        ..Default::default()
    };

    eframe::run_native(
        "WinPilot",
        options,
        Box::new(move |cc| {
            if let Some(port) = mcp_port(&args) {
                mcp_server::start(session.clone(), Some(port));
            }
            Ok(Box::new(WinPilotApp::new(
                &cc.egui_ctx,
                auto.as_deref(),
                session.clone(),
                notes_rx,
            )))
        }),
    )
    .context("界面初始化失败")?;

    Ok(())
}

/// `--mcp` 开启 MCP server；可带端口参数（`--mcp 9100`），默认 8100。
fn mcp_port(args: &[String]) -> Option<u16> {
    args.iter().position(|a| a == "--mcp").map(|index| {
        args.get(index + 1)
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(8100)
    })
}

/// 无头模式的 --auto：直接启动对目标的抓取会话。
fn headless_autostart(session: &session::SharedSession, pattern: &str) -> Result<()> {
    let windows = list_capturable_windows()?;
    let lowered = pattern.to_lowercase();
    let Some(info) = windows.iter().find(|w| {
        w.title.contains(pattern) || w.process_name.to_lowercase().contains(&lowered)
    }) else {
        println!("[serve] --auto 未匹配到 {pattern:?}，当前可抓取窗口：");
        for w in &windows {
            println!("  {}", w.label());
        }
        return Ok(());
    };
    let (_, geometry) = session.ensure_capture(info.hwnd)?;
    println!(
        "[serve] 目标 {} 客户区 {}×{}",
        info.label(),
        geometry.client_w,
        geometry.client_h
    );
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

    let (worker, receiver, _frame_slot) = CaptureWorker::start(info.hwnd)?;
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

    let (worker, receiver, _frame_slot) = CaptureWorker::start(hwnd)?;
    let first = latest_frame(&receiver).context("没收到画面，无法换算点击坐标")?;
    let frame_size = (first.width, first.height);
    println!("[send] 画面 {frame_size:?} 客户区 {}×{}", geometry.client_w, geometry.client_h);

    let mut actions = Vec::new();
    for spec in specs {
        actions.push(build_action(spec, hwnd, &geometry, frame_size).map_err(anyhow::Error::msg)?);
    }
    println!("[send] 发送 {} 条指令", actions.len());

    let (notes_tx, notes_rx) = unbounded();
    let input = input::InputWorker::spawn(notes_tx.clone());
    drop(first);

    let before = latest_frame(&receiver);
    println!("[send] 发送前 {}", describe(before.as_deref()));

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
    println!("[send] 发送后 {}", describe(after.as_deref()));
    if let (Some(before), Some(after)) = (&before, &after) {
        let (changed, bbox) = diff_frames(before, after);
        println!("[send] 变化像素 {changed}，包围盒 {bbox:?}");
    }

    worker.stop();
    Ok(())
}

/// 实测端到端延迟：一条指令发出，到画面里出现第一处变化，中间隔了多久。
///
/// 数字包含注入、目标自身处理、重绘与 WGC 抓取，就是用户体感的"按下到看见"。
/// 目标画面本身在动（比如过关动画）时首处变化可能早于真实响应，所以同时给出
/// 无操作状态下画面自身的变化量作为参照。
fn latency_self_test(pattern: &str, specs: &[String]) -> Result<()> {
    let windows = list_capturable_windows()?;
    let lowered = pattern.to_lowercase();
    let Some(info) = windows.iter().find(|w| {
        w.title.contains(pattern) || w.process_name.to_lowercase().contains(&lowered)
    }) else {
        println!("[latency] 未匹配到 {pattern:?}");
        return Ok(());
    };
    let hwnd = info.hwnd;
    let geometry = geometry::TargetGeometry::query(hwnd)?;

    let (worker, receiver, _frame_slot) = CaptureWorker::start(hwnd)?;
    let first = latest_frame(&receiver).context("没收到画面，目标可能完全静止")?;
    let frame_size = (first.width, first.height);
    drop(first);
    println!(
        "[latency] 目标 {} 画面 {}×{}",
        info.label(),
        frame_size.0,
        frame_size.1
    );

    let (notes_tx, _notes_rx) = unbounded();
    let input = input::InputWorker::spawn(notes_tx.clone());

    let mut stale_max = Duration::ZERO;
    let mut totals = Vec::new();

    for spec in specs {
        let action = build_action(spec, hwnd, &geometry, frame_size).map_err(anyhow::Error::msg)?;

        // 400ms 静置窗口里的最后一帧作为基准，顺带量出画面自身的变化速度
        let baseline = steady_frame(&receiver, Duration::from_millis(400), &mut stale_max);
        let Some(baseline) = baseline else {
            println!("[latency] {spec}: 目标画面静止，收不到基准帧，跳过");
            continue;
        };
        let baseline_hash = types::frame_checksum(&baseline.rgba);

        let sent = Instant::now();
        input.send(InputRequest {
            hwnd,
            activate: true,
            action,
        });

        let mut hit = None;
        while sent.elapsed() < Duration::from_millis(1200) {
            if let Ok(CaptureEvent::Frame(packet)) = receiver.recv_timeout(Duration::from_millis(20)) {
                stale_max = stale_max.max(packet.captured_at.elapsed());
                if types::frame_checksum(&packet.rgba) != baseline_hash {
                    // 以该帧的抓取时刻为准，而不是本进程轮到它的时刻，否则把轮询间隔也算成了延迟
                    let since = packet.captured_at.saturating_duration_since(sent);
                    hit = Some((since, packet));
                    break;
                }
            }
        }

        match hit {
            Some((delay, packet)) => {
                let (changed, bbox) = diff_frames(&baseline, &packet);
                let ms = delay.as_secs_f64() * 1000.0;
                println!(
                    "[latency] {spec:<14} 画面变化 {ms:>7.1}ms  变化像素 {changed} 包围盒 {bbox:?}"
                );
                totals.push(delay);
            }
            None => println!("[latency] {spec:<14} 1.2s 内画面没有变化"),
        }
    }

    if !totals.is_empty() {
        let n = totals.len();
        let avg = totals.iter().map(|d| d.as_secs_f64() * 1000.0).sum::<f64>() / n as f64;
        let slowest = totals
            .iter()
            .map(|d| d.as_secs_f64() * 1000.0)
            .fold(0.0_f64, f64::max);
        let stale_ms = stale_max.as_secs_f64() * 1000.0;
        println!("[latency] {n} 条平均 {avg:.1}ms，最慢 {slowest:.1}ms");
        println!("[latency] 参考：帧在信箱里最长滞留 {stale_ms:.1}ms（本进程取帧节奏，不计入上面的数字）");
    }

    worker.stop();
    Ok(())
}

/// 在给定时间窗内持续取帧，返回最后一帧（代表该时刻的稳定画面）。
fn steady_frame(
    receiver: &CaptureEventReceiver,
    window: Duration,
    stale_max: &mut Duration,
) -> Option<Arc<FramePacket>> {
    let deadline = Instant::now() + window;
    let mut last: Option<Arc<FramePacket>> = None;
    while Instant::now() < deadline {
        if let Ok(CaptureEvent::Frame(packet)) = receiver.recv_timeout(Duration::from_millis(40)) {
            *stale_max = (*stale_max).max(packet.captured_at.elapsed());
            last = Some(packet);
        }
    }
    last
}

/// 排队压测：把 N 条指令瞬间全部丢进队列，只看注入线程多久消化完。
///
/// 不启动抓取，测出来的就是注入这一段的开销，可以直接和界面上"连点没反应"对应起来。
fn burst_self_test(pattern: &str, spec: &str, count: usize) -> Result<()> {
    let windows = list_capturable_windows()?;
    let lowered = pattern.to_lowercase();
    let Some(info) = windows.iter().find(|w| {
        w.title.contains(pattern) || w.process_name.to_lowercase().contains(&lowered)
    }) else {
        println!("[burst] 未匹配到 {pattern:?}");
        return Ok(());
    };
    let hwnd = info.hwnd;
    let keys = keys::parse_combo(spec).map_err(anyhow::Error::msg)?;
    println!("[burst] 目标 {}，指令 {spec} ×{count}", info.label());

    let (notes_tx, notes_rx) = unbounded();
    let input = input::InputWorker::spawn(notes_tx.clone());

    let started = Instant::now();
    for _ in 0..count {
        input.send(InputRequest {
            hwnd,
            activate: true,
            action: Action::Combo { keys: keys.clone() },
        });
    }
    println!(
        "[burst] {count} 条入队耗时 {:.2}ms",
        started.elapsed().as_secs_f64() * 1000.0
    );

    let mut times: Vec<f64> = Vec::with_capacity(count);
    let mut last_note = String::new();
    while times.len() < count {
        match notes_rx.recv_timeout(Duration::from_millis(2000)) {
            Ok(note) => {
                last_note = note;
                times.push(started.elapsed().as_secs_f64() * 1000.0);
            }
            Err(_) => break,
        }
    }

    if times.is_empty() {
        println!("[burst] 没有任何指令完成");
        return Ok(());
    }
    let head: Vec<String> = times
        .iter()
        .take(6)
        .map(|t| format!("{t:.1}"))
        .collect();
    let total = times.last().copied().unwrap_or(0.0);
    let first = times[0];
    let per_item = (total - first) / (times.len().saturating_sub(1)).max(1) as f64;
    println!("[burst] 完成时刻(ms)：{}", head.join(" "));
    println!(
        "[burst] 全部完成 {total:.1}ms，平均每条 {per_item:.1}ms，首条 {first:.1}ms 含首次抢前台"
    );
    println!("[burst] 最后一条回执：{last_note}");
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
fn latest_frame(receiver: &CaptureEventReceiver) -> Option<Arc<FramePacket>> {
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
            types::frame_checksum(&frame.rgba)
        ),
        None => "无画面".to_owned(),
    }
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

    let (worker, receiver, _frame_slot) = CaptureWorker::start(info.hwnd)?;
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
