# llama-server-launcher-rs

一个独立的 Rust MVP：保留原有 `llama-server` 配置启动示例，并新增两个彼此独立的本地引擎安装流程。它不是 LocModel 的源码，也不包含 LocModel 品牌资产或官方背书。

## 两套引擎，各自管理

| ID | 官方引擎 | 用途 | 下载/状态 |
| --- | --- | --- | --- |
| `chat` | [llama.cpp](https://github.com/ggml-org/llama.cpp) | 对话推理运行时 | 独立查询、安装、更新和状态 |
| `media` | [stable-diffusion.cpp](https://github.com/leejet/stable-diffusion.cpp) | 图像与视频共用的运行时 | 独立查询、安装、更新和状态；图像与视频不会各装一份 |

两套引擎分别放在当前用户的数据目录下：Windows 默认是 `%LOCALAPPDATA%\LocM\engines\llama.cpp\<版本>` 和 `%LOCALAPPDATA%\LocM\engines\stable-diffusion.cpp\<版本>`；Linux 使用 `$XDG_DATA_HOME/locm/engines/...`，未设置 `XDG_DATA_HOME` 时使用 `$HOME/.local/share/locm/engines/...`。下载包按版本保存，升级不会覆盖旧版本。`--dir PATH` 可指定两套引擎的共同根目录，但应指定当前用户可信、不可被其他不可信进程写入的位置。

## 下载与校验

安装器在运行时读取官方 GitHub Release API，不把易变的 ZIP 文件名或版本写死。它按当前目标选择官方 CPU x64 资产：Windows 使用 `-bin-win-cpu-x64.zip`；Linux x86_64 的 llama.cpp 使用 `-bin-ubuntu-x64.tar.gz`，stable-diffusion.cpp 使用 `-bin-Linux-Ubuntu-26.04-x86_64.zip`。资产必须与仓库和 Release 路径匹配、大小合理且 GitHub API 提供 SHA-256 digest 的资产。下载通过 HTTPS，并限制到 GitHub API 和实测 Release 下载主机；下载进度显示为百分比，失败或校验错误最多重试 3 次。SHA-256 不匹配、digest 缺失、下载主机不符合约束或归档路径不安全时会停止，不解包。解压会拒绝路径穿越、Windows 盘符/设备保留名、非法文件名、符号链接/硬链接/特殊文件和 staging 根目录外路径；Linux tar.gz 中的普通文件只恢复为安全的 `0644` 或 `0755`。整个校验、解压流程不会启动包内程序。

安全边界：这些检查用于降低远程 Release 包或网络传输异常导致的风险，但不是针对同一用户下恶意进程的竞态隔离。文件系统路径检查与创建之间仍存在很小的并发替换窗口，不能替代基于受保护目录句柄的原子解包；因此默认只写当前用户的数据目录，不要把 `--dir` 指向共享可写目录。文件系统若拒绝清理临时下载/暂存文件会显示警告；进程被强制终止时可能留下隐藏 `.download-*` 或 `.staging-*` 文件。

截至 2026-11-18，核对到的官方资产如下（运行时会重新查询，而不是依赖这些快照）：

- stable-diffusion.cpp 最新 Release `master-1106-532d2c1` 的 Windows CPU x64 资产为 `sd-master-532d2c1-bin-win-cpu-x64.zip`，18,358,541 bytes；GitHub API digest 为 `sha256:abef332cfca7861cc567910e79bedb70bfb2920bb4eedac97fcbb6db477f1596`。见[官方 Release](https://github.com/leejet/stable-diffusion.cpp/releases/tag/master-1106-532d2c1)与[官方 Release API](https://api.github.com/repos/leejet/stable-diffusion.cpp/releases/latest)。
- llama.cpp 当前查到的带 Windows CPU x64 包为 `llama-b12498-bin-win-cpu-x64.zip`，25,520,902 bytes；GitHub API digest 为 `sha256:eeb67345805510a1a955430d559336a3c535345d9a19467bf89fa0a6a34d4173`。该版本是官方预览版；[官方 Release](https://github.com/ggml-org/llama.cpp/releases/tag/b12498)。稳定 `v1.0.0` Release 当前没有对应 CPU ZIP，因此默认安装不会悄悄退回预览版；若用户接受预览版，须显式传入 `--allow-prerelease`。稳定版搜索最多检查最新 500 个官方 Release 项。

CPU x64 是目前唯一自动选择的硬件变体；CUDA、Vulkan、ARM64 和其他平台暂不自动选择。

## 桌面 MVP（Tauri 2）

仓库现在包含一个独立桌面目标，UI 直接调用同一份 Rust 引擎管理器，不会另写一套下载逻辑。开发环境可用以下命令启动：

```sh
cargo run --features desktop --bin local-engine-manager
```

桌面页会自动读取处理器、逻辑核心数、内存和 NVIDIA/Windows 显卡信息；概览页按硬件检测、安装推理引擎、准备模型显示三步状态。两套官方引擎分别管理，安装进度覆盖下载、SHA-256 校验和安全解压。Linux x86_64 使用官方 CPU 包；llama.cpp 的 tar.gz 和 stable-diffusion.cpp 的 ZIP 都会经过路径与文件类型检查。llama.cpp 预览版会明确标注。stable-diffusion.cpp 是图像与视频共用的一份 CPU 运行时；媒体页可从 Hugging Face 下载权重、只读扫描 ComfyUI 权重并调用官方 CLI 生成图像或 Wan 视频。设置页可选择作品根目录，生成内容分别保存在 `images` 与 `videos` 子目录；旧配置未设置该项时仍使用默认目录。此 CPU 构建不调用 GPU。

目前桌面 UI 还支持：导入本地 GGUF、搜索 Hugging Face GGUF 仓库并下载选定文件；配置模型目录、作品目录、外观和本地 API 端口；选择已安装模型并启动/停止 `llama-server`；按当前服务器 `--help` 动态启用上下文、Flash Attention 与 Q8 K/V 缓存参数；可选在崩溃后重启一次。Agent 可用已启动的本地模型处理任务、查看工作区文件、在工作区内新建文件，并把任务记录保存在本机 WebView 存储中。Agent 读取/写入只限选定工作区，拒绝符号链接和 `.git` 元数据；覆盖、删除和命令执行必须逐项批准。命令以独立程序和参数启动，工作目录设为工作区，但程序仍以当前用户权限运行并继承环境变量，可能访问工作区以外的文件或网络。默认 API 只绑定 `127.0.0.1`；用户开启局域网访问时，仅尝试绑定操作系统选定的私有 IPv4 地址，不绑定所有网卡或公网地址。运行页还展示速度、首次加载和资源占用的粗略估算（不是实测）。Hugging Face 搜索和下载会联网；聊天和 Agent 模型请求只发往本地服务。

定时页支持持久化的一次性、每小时和每 24 小时提示词任务。任务只在应用保持打开且本地模型运行时执行；应用关闭期间错过的周期会跳过，不补跑、不自动启停模型，也不发送通知。

思考行为不提供强制开关，由模型模板/服务器默认逻辑决定；当前无法可靠判断每个 GGUF 是否支持强制思考。投机解码需要单独的草稿模型或解码配置，本项目未管理草稿模型，因此不提供该开关。启动时只传递安装版本 `llama-server --help` 确认支持的参数；没有确认的选项会禁用或拒绝启动。

桌面引擎安装流程支持 Windows x64 和 Linux x86_64 CPU 官方包；其他平台可查看状态，但自动安装按钮不可用。Windows 保留原有引擎安装/启动实现。Tauri 2 依赖操作系统 WebView（Windows 使用 WebView2；Linux 构建需要 GTK/WebKitGTK 开发库）。原 CLI 仍是默认构建目标；只有显式打开 `desktop` feature 才构建桌面窗口。

在目标平台的原生构建环境中，以下命令会生成对应平台的桌面可执行文件：

```sh
cargo build --locked --release --features desktop --bin local-engine-manager
```

当前 `tauri.conf.json` 关闭了安装包 bundle（`bundle.active: false`），因此该命令产出的是当前目标平台的可执行文件，不是 `.deb`、MSI 或 Windows 安装器。本轮 Linux x86_64 桌面程序已构建并在隔离图形环境启动检查；Windows x64 GUI EXE 已从 Linux 交叉构建，随包提供的 `WebView2Loader.dll` 需与 EXE 放在同一目录，Windows 仍需安装 WebView2 Runtime。Windows 未在真实 Windows 设备启动验证。

**尚未实现或验证的边界：** 操作系统级后台定时任务和通知、GPU 加速选择、流式聊天、`.deb`/MSI 等可分发安装器。Linux 定时任务由应用内调度，只在应用打开且本地模型运行时执行；关闭期间错过的周期会跳过。Agent 依赖本机模型按 JSON 工具协议返回动作；如果模型不遵循协议，任务会暂停而不会模拟执行。本轮测试通过 20 项默认测试、28 项桌面测试、Linux 桌面构建和隔离启动检查；Windows 通过 GNU x64 交叉链接构建，但没有在真实 Windows 设备启动验证。没有下载真实模型权重，也没有实测真实图片或视频生成。

## 使用命令

以下 CLI 命令应在目标 Windows x64 或 Linux x86_64 电脑上运行。`cargo run` 适用于安装了 Rust 的开发环境；桌面 MVP 的开发启动方式见上一节。

```powershell
# 查看两套引擎各自状态
cargo run --locked -- engine status

# 查看官方可用版本和包大小；不会下载
cargo run --locked -- engine latest chat
cargo run --locked -- engine latest media

# 分开安装；图像和视频都使用同一个 media 引擎
cargo run --locked -- engine install media
cargo run --locked -- engine install chat

# 仅当接受 llama.cpp 预览版时显式启用
cargo run --locked -- engine install chat --allow-prerelease

# 分开更新；旧版本保留
cargo run --locked -- engine update chat
cargo run --locked -- engine update media
```

安装或更新必须明确指定 `chat` 或 `media`，避免一次意外下载两个引擎。也可使用 `--dir C:\somewhere\engines` 覆盖共同根目录。`engine status` 不请求网络；`engine latest` 只查询官方 API。

旧的配置启动器仍可按原样使用：

```powershell
cargo run --locked -- .\engine.json
```

## 当前范围与限制

- **已实现：** 两套独立的状态/版本目录、官方 Release API 查询、引擎下载进度、3 次重试、SHA-256 校验和 ZIP/tar.gz 路径与文件类型检查；桌面版另含硬件检测、GGUF 导入/公开 Hugging Face 搜索与下载、`llama-server` 管理、本地聊天、按进程采样的系统监测和应用内定时提示词任务；Agent 支持本机任务记录、受限工作区浏览与读写，以及命令/覆盖/删除的逐项批准。
- **尚未实现：** 操作系统后台调度/推送、stable-diffusion.cpp 权重下载与图像/视频生成、GPU 自动选择、更新后的旧版本清理，以及可分发的安装包。
- 本地聊天模型目前以 CPU 参数启动，回复非流式。上下文默认为“自动”（不显式传入 `--ctx-size`，使用该服务器与模型的默认值），也可选择 8K/16K/32K/64K；大上下文或大模型可能无法加载。速度、首次加载与内存占用均为估算，不是本机基准测试。
- Hugging Face 文件下载使用 HTTPS 和安全文件名/域名检查，但当前未校验仓库提供的 LFS 哈希；请先确认仓库来源、许可证和文件后再下载。
- stable-diffusion.cpp 的引擎程序不等于模型权重；本项目目前没有可用的画图或视频生成工作流。
- 运行时更新流程会另存新版本，不覆盖旧版；安装完成后不会自动切换或启动。
- 本 MVP 的自动引擎安装支持 Windows x64 与 Linux x86_64 CPU 官方包；Linux llama.cpp 使用安全 tar.gz 解包，stable-diffusion.cpp 仍使用 ZIP。桌面目标以 Tauri 2 实现，默认 feature 仍保持 CLI，避免未显式选择桌面时要求本机具备 WebView 系统库。

## 开发与测试

```sh
cargo fmt --all -- --check
cargo test --locked
cargo build --release --locked
cargo check --features desktop --bin local-engine-manager
```

许可证见 [`LICENSE`](LICENSE)。
