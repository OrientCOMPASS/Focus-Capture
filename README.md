# 🎯 FocusCapture — MicYou 焦点应用声音混入插件

把**当前焦点应用**的声音一键混入 MicYou 麦克风流：通话 / 直播时，对方能直接听到你正在玩的游戏、正在放的音乐、浏览器里的视频声——不需要虚拟声卡，不需要立体声混音。

基于各平台官方的**进程级音频捕获**能力（Windows WASAPI Process Loopback / Linux PipeWire 流间链接 / macOS CoreAudio Process Tap），只捕获你按下快捷键那一刻位于前台的那个进程（及其子进程树），其他应用的声音与系统提示音不会混入。

| | |
|---|---|
| 插件 id | `opss.focus-capture` |
| 运行时 | Native（cdylib，C ABI v1 / apiVersion 1） |
| 平台 | **Windows 10 2004+（x86_64）· Linux（PipeWire + X11，x86_64）· macOS 14.4+（arm64）**；三平台动态库同包分发，宿主按平台自选 |
| 插件类型 | `dsp`（实时处理链节点，`realtimeSafe: true`） |
| 权限 | `dsp.node`、`config.read`、`config.write`（最小化，无网络/剪贴板/文件权限） |
| 构建 | GitHub Actions · windows-latest · **原生 MSVC**（见 `.github/workflows/release.yml`） |
| 依赖 | 无外部运行库（仅链接 Windows 系统 DLL；单文件 ~360 KB） |
| 许可 | Unlicense（公有领域） |

## 安装

1. 下载 Release 中的 `plugin.zip`（或版本化 `opss.focus-capture-v*.zip`，内容相同）；开发版见每次 push 构建的 Actions artifact `focus-capture-development`
2. 把 zip 内容解压到插件目录的 `opss.focus-capture/` 子目录（Windows `%APPDATA%\micyou\plugins\`，Linux/macOS `~/.config/micyou/plugins/`）；包内含三个动态库（`.dll/.so/.dylib`），**宿主按平台自动选用**
3. 回到插件页点「刷新」，启用 **FocusCapture**
4. 启用后宿主会自动把合成节点 `Plugins` 插入处理链（AEC 之后）
5. 应用内「检查更新」走 `updateUrl`（release 资产 `plugin.json`；宿主按 `.json→.zip` 推导下载 `plugin.zip`）

### 平台要求与已知不可用情形

| 平台 | 要求 | 已知不可用 |
|---|---|---|
| Windows | 10 2004（build 19041）及以上，x86_64 | 系统版本过旧（捕获 API 自 19041 起提供）；WASAPI 独占模式播放的应用；DRM 保护内容 |
| Linux | PipeWire 会话 + X11，x86_64 | **纯 PulseAudio 会话不支持**（不做兜底）；Wayland 不支持（合成器不暴露前台 pid，且宿主快捷键仅 X11） |
| macOS | 14.4 及以上（Process Tap API），arm64 | 更早版本无 `AudioHardwareCreateProcessTap`（插件给出明确报错）；Intel Mac 未提供构建 |

> ⚠️ **Linux 与 macOS 后端未经真机测试**：Windows 后端已经多轮真机回归；Linux 仅在容器内
> 真实 PipeWire 会话中验证到节点发现/归属/报错/回收路径（音频数据通路待真机）；macOS 仅通过
> `aarch64-apple-darwin` 编译级检查。**欢迎在这两个平台上实机试用，并在
> [Issues](https://github.com/OrientCOMPASS/Focus-Capture/issues) 积极反馈实机运行遇到的
> 问题（附 `FC_TRACE=1` 追踪日志最佳）；也欢迎直接提交 PR 修复。**

## 使用

1. 把要共享声音的应用切到**前台**（游戏、播放器、浏览器……）
2. 按 **`Ctrl+Shift+F8`**（默认，可改）→ 系统通知「🎯 正在捕获焦点应用」，该应用的声音即刻混入麦克风
3. 在**任何**应用里再按同一快捷键 → 「已停止捕获」
4. 目标应用退出时自动停止；MicYou 侧边栏面板（🎯 FocusCapture）可随时查看状态、调增益、手动停止

> **混音只在串流运行时送达对方**：MicYou 需要已连接手机/Web 端（音频线程在处理链上调用本插件）。未串流时按快捷键同样会进入捕获状态，声音在开始串流后混入。

## 快捷键

设置表单 / 面板均可修改，**改完立即生效**（无需重新启用插件；旧组合自动失效，切回曾用过的组合会直接复用宿主里的注册）：

- **格式**：`ctrl/alt/shift/win` 修饰键 + 字母、数字、`f1–f24`、`space/enter/up/...` 等，例如 `ctrl+shift+f8`。
- 面板提供**按键捕获**：点「按键捕获」按钮后直接按下键盘组合，自动写入配置。
- 字母/数字主键必须带修饰键（防止系统级劫持正常打字）；F 键可以裸用。
- **鼠标侧键暂不支持**。技术上插件侧可用低级钩子（`WH_MOUSE_LL`）实现，但实测发现两个结构性问题：低级钩子按进程各自生效，MicYou GUI 与 CLI 同时运行时会**双重触发**；持有钩子的进程退出时系统需重建全局钩子链，会造成**全系统输入卡顿**。因此鼠标监听计划在宿主 `register_hotkey` API 层统一实现后再开放（详见 docs/TECHNICAL.md §6 复盘）。

## 配置

| 键 | 默认 | 说明 |
|---|---|---|
| `hotkey` | `ctrl+shift+f8` | 见上节；修改后立即生效 |
| `gain` | `1.0` | 混音线性增益 0–2，运行期即时生效 |
| `notify` | `true` | 开始/停止/出错时弹系统通知 |

状态快照写在 `fc_status`（面板轮询用）：`phase / app / pid / rate / channels / error / hotkey / hotkeyOk`。

## 常见问题

| 现象 | 原因与处理 |
|---|---|
| 通知「激活失败 (0x80070005) 拒绝访问」 | 目标以**管理员权限**运行而 MicYou 没有 → 用管理员运行 MicYou |
| 通知「激活失败 (0x80070490)」 | 音频引擎暂未找到目标端点，稍后重试；持续失败请确认系统 ≥ Win10 2004 |
| DRM 保护内容无声 | 系统禁止捕获受保护流（设计使然） |
| 对方听到的游戏声发闷 / 被削弱 | 到「设置 → 音频 → 处理链」把 `Plugins` 节点保持在 **AEC 之后**（或拖到链尾），否则混入的声音会被回声消除/降噪处理 |
| 按快捷键偶发无反应 | 宿主总线在插件实例忙时会跳过消息投递（约 20ms 音频帧窗口）；再按一次即可，也可用面板「停止捕获」兜底 |
| 独占模式（WASAPI Exclusive）播放的应用 | 独占流绕过音频引擎，进程级捕获拿不到；把该应用改回共享模式 |
| 换了新快捷键后旧组合仍触发？ | 不会：旧宿主注册按句柄 id 过滤；但该注册要到插件禁用/重启 MicYou 才真正释放（宿主 ABI 无注销接口） |

## 构建与发布（GitHub Actions）

`.github/workflows/release.yml`（各平台原生工具链，参考 Mambo-RVC-ONNX 的方式）：

- **每次 push（main/dev，含直接改版本号）**：仅自动构建 development build——三平台矩阵（`windows-latest` MSVC / `ubuntu-latest` + `libpipewire-0.3-dev libspa-0.2-dev libclang-dev` / `macos-latest` arm64）各跑 `cargo test` → 产物去 `lib` 前缀归一化 → 打包**单包含三库**的 `plugin.zip` → Actions artifact `focus-capture-development`；**不创建 release / tag**；
- **发 release = 手动**：Actions 页 Run workflow（可选 bump 档位 none/patch/minor/major，默认 none 用 main 上当前版本）→ 防重复检查 → 打 tag `v<ver>` + Release（资产：版本化 zip、`plugin.zip` 别名、`plugin.json` 快照、`market-entry.json`）；选 bump 档位时新版本号回提交 main（`[skip ci]`）；
- 仓库：`https://github.com/OrientCOMPASS/Focus-Capture`。

本地验证：Windows 目标可交叉编译（mingw `x86_64-pc-windows-gnu` 或 `cargo xwin build --target x86_64-pc-windows-msvc`）；macOS 目标 `cargo check --target aarch64-apple-darwin` 类型检查；Linux 原生编译需上述三个 dev 包。

## 现场诊断（FC_TRACE）

设置环境变量 `FC_TRACE=1` 后运行 MicYou（CLI：`set FC_TRACE=1 && micyou-cli …`；GUI 直接设系统环境变量后启动），插件会把**全链路追踪**（每次状态迁移、worker 激活/拆解的每一步、Host API 调用、线程 id、墙钟毫秒）打印到 stderr，并**同时追加写入 `%TEMP%\focuscapture-trace.log`**（GUI 无控制台，靠文件回收）。若进程崩溃，内置的 vectored exception handler 会在最后一行打印**异常码、崩溃地址与所属模块名**（可区分插件 DLL / AudioSes / mmdevapi / ntdll），用于 issue 反馈定位。正常使用时不开启则零开销。

## 测试

单元测试随 CI 执行（`cargo test --release`：环形缓冲、重采样、快捷键解析）；三平台动态库由 CI 矩阵构建产出。实现细节、实时安全约束与平台验证状态见 `docs/TECHNICAL.md`。

## 许可

Unlicense — 见 [LICENSE](LICENSE)。实现细节见 [docs/TECHNICAL.md](docs/TECHNICAL.md)。
