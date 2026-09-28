# WinPilot 使用指南

WinPilot 是一个 Windows 桌面自动化工具：它用 Windows Graphics Capture 实时抓取任意窗口的画面，用 SendInput 向该窗口定向注入鼠标和键盘操作，并提供图形界面和 **MCP server** 两种操控方式。典型用途是让外部 AI 模型通过 MCP 协议"看着画面操作"一个 Windows 程序（例如游戏）。

- 抓取：WGC 硬件通路，60fps，无需目标配合，被遮挡/后台的窗口也能抓
- 注入：SendInput，走原始输入通道，目标捕获鼠标或没有窗口焦点时依然有效
- 操控：egui 图形界面（人工）+ MCP server（外部模型），两者共享同一条注入队列

---

## 1. 构建与启动

```powershell
cd winpilot
cargo build --release
.\target\release\winpilot.exe            # 只开图形界面
.\target\release\winpilot.exe --mcp      # 图形界面 + MCP server（默认端口 8100）
.\target\release\winpilot.exe --mcp 9100 # 指定 MCP 端口
.\target\release\winpilot.exe --auto tetris --mcp   # 启动时自动选中标题/进程含 "tetris" 的窗口并开始抓取
.\target\release\winpilot.exe --serve --auto tetris # 无头模式：无界面，纯 MCP server 后台运行
```

### 运行形态

| 形态 | 启动方式 | 适用场景 |
|---|---|---|
| 图形界面 | 默认 | 人工操作，看预览、选点、手动注入 |
| 图形界面 + MCP | `--mcp` | 人和模型操作同一个实例，预览实时跟随 MCP 的切换 |
| **无头模式** | `--serve [--auto 关键字]` | 纯给外部模型当自动化后端：无窗口、不抢焦点、省 ~10% CPU，可挂后台长期运行 |

无头模式下 MCP server 默认开启（端口 8100），工具行为与挂靠模式完全一致；注入回执改为打进日志（tracing）。延迟与挂靠模式相同——抓取与注入链路本来就不经过 GUI。Ctrl+C 退出。

说明：

- 依赖已在 `Cargo.toml` 里配置了 `[profile.dev.package."*"] opt-level = 2`，因此 `cargo run` 的 debug 构建也能跑满 60fps；日常使用建议 release 构建
- 首次启动会加载 `C:\Windows\Fonts\msyh.ttc` 作为中文字体，缺失时依次尝试 simhei / NotoSansSC
- 程序嵌入了 PerMonitorV2 DPI 清单，多显示器/缩放环境下坐标换算自动处理

### 命令行自检模式（不打开界面）

| 模式 | 用法 | 作用 |
|---|---|---|
| 抓取自检 | `--capture <关键字>` | 对目标启动抓取并统计 20 帧，验证 WGC 通路；不带匹配时打印全部可抓取窗口 |
| 坐标自检 | `--map <关键字> --fx 0.5 --fy 0.5` | 把归一化坐标换算成屏幕坐标并把真实光标移过去，回读验证 |
| 注入自检 | `--send <关键字> <指令...>` | 发送一串指令，用画面变化像素数证明注入生效。指令：`A`、`Ctrl+S`、`click:0.5,0.5`、`text:Hello`、`rmove:40,0` |
| 延迟测量 | `--latency <关键字> <按键...>` | 实测"发出指令 → 画面出现变化"的端到端毫秒数 |
| 注入压测 | `--burst <关键字> <按键> <条数>` | 把 N 条指令瞬间入队，测量注入线程的吞吐（正常约 8~9ms/条） |

---

## 2. 图形界面

左侧为实时画面预览，右侧 360px 面板从上到下四个分区：

1. **状态**：抓取状态、画面尺寸、帧率、画面延迟（抓取→界面，正常 ≈16ms）、已显示帧数、注入待执行数。这是判断"点了没反应卡在哪一环"的第一入口
2. **目标窗口**：刷新列表、点选窗口、开始/停止抓取
3. **坐标映射**：显示客户区尺寸与 DPI；鼠标悬停/点选预览区会给出"画面像素 → 客户区 → 屏幕"三套坐标；"清除选点"、"光标移到选点"（只移动不点击，用于标定）
4. **操作**（鼠标/键盘/日志三个选项卡）：
   - 鼠标：先在预览区点一个点，再用"左键单击/双击/右键/中键"、"滚轮上/下"、"按下左键/抬起左键"、"相对移动 dx,dy"；"先激活"勾选框控制注入前是否把目标抢到前台
   - 键盘：按键输入框（如 `Ctrl+Left` 或 `Left Left Space`）、方向键快捷按钮、文本输入（逐码元 UNICODE 事件）
   - 日志：操作回执（含用时、注入后队列余量、此刻前台窗口）、系统自检（WGC 能力列表）

---

## 3. 通过 MCP 操作（重点）

### 3.1 连接

启动带 `--mcp` 的 WinPilot 后，server 监听：

```
http://127.0.0.1:8100/mcp        （--mcp 9100 可改端口）
```

传输协议是 **Streamable HTTP**（MCP 当前规范的标准传输）。任何支持 HTTP 型 MCP 客户端都能直连，例如：

```jsonc
// 通用配置（Qoder / Claude Desktop / VS Code 等）
{
  "mcpServers": {
    "winpilot": { "url": "http://127.0.0.1:8100/mcp" }
  }
}
```

```powershell
# Claude Code 命令行
claude mcp add --transport http winpilot http://127.0.0.1:8100/mcp
```

> 为什么不是 WebSocket：WebSocket 传输在 MCP 2025-03-26 版规范中已被移除，标准客户端均无法连接；本机回环下两种传输的延迟差 <1ms，模型决策耗时才是瓶颈，因此选了兼容性最好的 Streamable HTTP。

### 3.2 工具总览

| 工具 | 参数 | 返回 | 用途 |
|---|---|---|---|
| `list_windows` | 无 | `{windows: [{hwnd, process, title, size, minimized}]}` | 枚举可抓取窗口，最小化窗口也包含 |
| `select_window` | `query`（标题/进程包含匹配） | `{selected: {...}, note}` | 选中目标并启动抓取；目标是 GUI 与 MCP 共同的"当前目标" |
| `capture_status` | 无 | `{selected, geometry, frame{size,checksum,age_ms,stale}, fps, input_backlog}` | 查询抓取是否就绪、画面是否新鲜 |
| `screenshot` | `region? [l,t,r,b]`（0..1 归一化）、`scale?`（0.05..1） | **PNG 图像** + 文本元数据 | 看画面；`scale: 0.5` 可把体积减到 1/4 |
| `wait_for_change` | `checksum`、`timeout_ms?`（默认 1500） | `{changed, checksum, note?}` | 判断"刚才那次操作有没有真的生效" |
| `click` | `fx, fy`（0..1 归一化画面坐标）、`button?`（left/right/middle）、`count?`（2=双击） | 结构化回执 | 定向点击 |
| `key` | `keys`（如 `"A"`、`"Ctrl+Shift+Left"`、`"Left Left Space"`） | `{sent, receipts[]}` | 组合键与按键序列 |
| `type_text` | `text` | 结构化回执 | 输入文本（UNICODE 事件，不受键盘布局影响） |
| `scroll` | `ticks`（正上负下） | 结构化回执 | 滚轮 |
| `move_rel` | `dx, dy` | 结构化回执 | 相对移动（raw input，鼠标被游戏捕获时用这个） |

### 3.3 推荐操作循环

外部模型操作一个目标的标准节奏：

```
list_windows ──► select_window ──► capture_status(等到 stale:false)
      │                                   │
      │                                   ▼
      │                              screenshot ◄──────────────┐
      │                                   │                    │
      │                              根据画面做决策              │
      │                                   │                    │
      │                    key / click / type_text / scroll     │
      │                                   │                    │
      │                     wait_for_change(操作前 checksum)    │
      │                                   │                    │
      └──────────────── 没变化就换一种操作 ──────────────────────┘
```

要点：

1. **select_window 即启动抓取**。它直接启动会话（内部自动恢复最小化窗口）并返回客户区几何，同步阻塞到会话就绪；返回后用 `capture_status` 看到 `frame` 非空且 `stale: false` 即可开始操作
2. **坐标换算**。`screenshot` 返回的画面即注入坐标系：画面宽 `W`、高 `H`，想点截图里 `(px, py)` 处，就调 `click {"fx": px/W, "fy": py/H}`。`screenshot` 带 `region` 裁剪时注意 `fx/fy` 仍是**整幅画面**的归一化坐标，需要把裁剪区内的位置还原回去：`fx = (l + px_local/W_local * (r-l))`
3. **用 checksum 判定生效**。每个动作回执都带 `frame_checksum`（动作完成瞬间的画面校验和），把它传给 `wait_for_change`；`changed: true` 返回新校验和，可继续作为下一次的基准
4. **`changed: false` 有两种含义**：画面本身静止（比如游戏暂停在菜单），或按键没有绑定任何功能。先换一个确定有效的键（如方向键）排除注入问题，再怀疑目标逻辑
5. **`stale: true` 说明画面是旧的**，通常因为目标被最小化（WGC 对最小化窗口不产帧）。此时重新调用 `select_window` 会自动恢复窗口并重启抓取

### 3.4 回执字段解读

动作类工具（click/key/type_text/scroll/move_rel）的返回：

```json
{
  "action": "组合键 [65]",
  "receipt": "组合键 [65] 用时9ms 队列剩0（此刻前台=tetris）",
  "frame_checksum": 3963034902599170722
}
```

- `用时`：注入这条指令花的毫秒数。首次抢前台会多几十毫秒，之后连续操作约 9ms/条
- `队列剩`：执行这条时还排着几条。持续 >0 说明发送速度超过了注入速度
- `此刻前台`：SendInput 完成瞬间真正持有焦点的窗口标题。**这是判断注入是否落到目标的关键证据**——如果不是目标窗口，按键会落到别处
- `frame_checksum`：动作完成瞬间的画面校验和（注意：画面变化往往滞后几十毫秒，判断生效请用 `wait_for_change`）

### 3.5 前台抢夺机制

键盘事件跟随前台焦点，所以注入前需要把目标带到前台。WinPilot 按强度递进尝试四级手段：

1. 常规 `SetForegroundWindow`（先把输入队列挂到当前前台线程）
2. 模拟一次 ALT 按键骗过"最近没处理过输入"的系统限制
3. **最小化再恢复**（系统直接放行，代价是目标窗口闪一下）
4. 直连目标线程设置键盘焦点（不改变可见前台）

对模型的影响：

- 目标已在前台时零开销；不在前台时首次操作会多几十毫秒并可能闪一下目标窗口
- 操作期间目标会抢走用户焦点，**人工不要在注入进行时打字**
- 若四级全部失败（例如 UAC 安全桌面挡住），动作返回错误"激活失败：目标未拿到前台焦点"

### 3.6 常见错误与排查

| 错误信息 | 原因 | 处理 |
|---|---|---|
| `没有匹配 "xxx" 的窗口` | 关键字不匹配，或目标已关闭 | 先 `list_windows` 看实际列表（注意标题是包含匹配） |
| `还没有目标窗口：先调用 select_window` | 未选目标就调了动作/截图 | 先 `select_window` |
| `还没有画面：先调用 select_window` | 抓取尚未启动完成 | 稍候再试，用 `capture_status` 确认 `stale: false` |
| `激活失败：目标未拿到前台焦点` | 系统拒绝抢前台（UAC、全屏独占等） | 手动把目标点一下到前台，或退出全屏独占模式 |
| `SendInput 返回 0…被更高级别的输入源拦截` | 存在 UAC 提示框/安全桌面 | 关闭遮挡的系统对话框 |
| 回执 `前台=` 不是目标 | 注入落到了别的窗口 | 重发一次；WinPilot 会自动重试抢前台 |

### 3.7 性能预期（本机 2080 Ti + 1080p 实测）

| 指标 | 数值 |
|---|---|
| 抓取帧率 | ~60fps（目标窗口静止时不产帧，属正常） |
| 画面延迟（抓取→界面/槽位） | ≈16ms（1 帧） |
| 单条注入耗时 | ≈9ms（已在前台）；首次抢前台 +40~170ms |
| 指令发出→画面出现变化 | 25~70ms（含目标自身处理，与 WinPilot 无关的部分占大头） |
| screenshot 体积 | 802×600 全幅 ≈6.6KB PNG（tetris 这类色块画面），`scale 0.5` 约 2.5KB |

---

## 4. 架构速览（给想改代码的人）

```
src/
  main.rs        入口、CLI 自检模式、--mcp 启动
  app.rs         egui 界面、预览、坐标映射、MCP select_window 的落地执行
  capture.rs     WGC 会话封装（windows-capture crate）
  input.rs       注入线程：串行队列、前台激活四级递进、SendInput 封装
  geometry.rs    客户区/DPI 几何与"画面像素→客户区→屏幕"换算
  keys.rs        按键名解析（含扩展键 scan code 处理）
  types.rs       共享类型：FramePacket / FrameMailbox / FrameSlot / Action
  session.rs     GUI 与 MCP 共享的会话状态（SharedSession）
  mcp_server.rs  rmcp Streamable HTTP server 与 10 个工具
  window_list.rs 可抓取窗口枚举（含最小化窗口）
```

关键设计：

- **帧通道**：容量 1 的信箱 + 投递前挤掉旧帧（`FrameMailbox`），UI 永远看最新画面且抓取线程永不阻塞；`FrameSlot` 另存一份最新帧供 MCP 的 screenshot 旁路读取
- **注入队列**：GUI 走异步队列（回执进日志），MCP 走同步 `execute_and_wait`（回执直接返回给模型），两者严格串行，不会互相穿插 down/up
- **抓取会话生命周期**由 `SharedSession::ensure_capture` 统一管理（GUI 按钮、MCP select_window、`--auto` 三种来源同一入口）；GUI 通过会话代数感知外部切换并认领新的帧接收端，因此挂靠与无头两种形态行为完全一致

---

## 5. 已知限制

- WGC 只在目标窗口**内容变化**时产帧：静止画面帧率降到 0 是正常现象，不代表抓取失败
- 最小化的窗口不产帧；WinPilot 会在抓取前自动恢复目标窗口
- UAC 安全桌面、全屏独占（FSE）模式下注入会被系统拦截
- 抢前台必然打断当前输入焦点，不适合在人工正在打字时后台运行
- 注入走 SendInput，会被反作弊系统识别为合成输入——只对自有/授权的程序使用
