# 部署与运行手册

本文说明 v0.2.11 桌面版的系统依赖、安装包启动、首次配置和常见故障。应用基于 Tauri 2：窗口由操作系统提供的 WebView 绘制界面，因此它和应用本身是两回事。Windows 使用 Microsoft Edge WebView2 Runtime；Linux 使用 GTK 3 与 WebKitGTK 4.1。缺少相应运行库时，程序可能无法打开窗口。

## 获取的包是什么

v0.2.11 发布包含 Linux x86_64 便携桌面包、真正的 Windows x64 NSIS 安装程序，以及独立的 Windows GNU 便携测试 ZIP：

| 平台 | Release 文件 | 类型与主要内容 |
| --- | --- | --- |
| Linux x86_64 | `locm-0.2.11-linux-x86_64.tar.gz` | `linux-x86_64/local-engine-manager`、`LICENSE`、`README.txt`、`icons/`（PNG、SVG、ICO） |
| Windows x64 | `locm-0.2.11-windows-x64-nsis-setup.exe` | 带安装向导的 NSIS 安装程序，内含 Windows x64 应用 EXE；本版 NSIS 清单未单独列出 `WebView2Loader.dll` |
| Windows x64 GNU | `locm-0.2.11-windows-x64-gnu-portable-test.zip` | 手动解压运行的交叉构建测试包；含 `local-engine-manager.exe`、同目录 `WebView2Loader.dll`、`LICENSE`、说明和图标 |

以上名称对应 v0.2.11 GitHub Release 资产。Linux 包为 Linux 原生构建；Windows NSIS 安装器从 Linux 使用 Tauri 官方文档所述的 `cargo-xwin` 与 `x86_64-pc-windows-msvc` 目标交叉构建；GNU 便携包由维护中的 `scripts/build-windows-gnu.sh` 构建。两个 Windows 包均未在真实 Windows 设备启动验证，不能把交叉构建当作实机测试。

## Windows x64

1. 确认系统为 64 位 Windows。应用运行仍需要 Microsoft Edge WebView2 Runtime。此 NSIS 安装器配置为：如果 Runtime 缺失，就联网下载并运行微软 WebView2 Bootstrapper；首次安装因此需要互联网连接。若电脑离线，可先从 [Microsoft 官方 WebView2 Runtime 页面](https://developer.microsoft.com/en-us/microsoft-edge/webview2)下载并安装 Evergreen Standalone Runtime。
2. 双击 `locm-0.2.11-windows-x64-nsis-setup.exe` 并按安装向导操作。安装器会部署 Windows x64 应用。静态检查本版 NSIS 安装器内容时，没有发现单独的 `WebView2Loader.dll`。
3. 从安装器创建的入口启动应用。安装包不包含模型、推理引擎或模型权重，也不会自动下载这些大型文件。

WebView2 Runtime 是负责显示应用界面的浏览器运行时；本版应用运行仍以它为前提，NSIS 配置会在缺少 Runtime 时联网启动微软 Bootstrapper。Windows GNU 便携 ZIP 会附带 `WebView2Loader.dll`；它只是加载库，不是 Runtime，也不包含完整 Edge 浏览器运行环境。NSIS 安装器清单中没有单独的 Loader DLL。

### Windows x64 GNU 便携测试包

下载并完整解压 `locm-0.2.11-windows-x64-gnu-portable-test.zip`，然后运行 `windows-x64-gnu/local-engine-manager.exe`。不要移走或删除同目录的 `WebView2Loader.dll`。此压缩包不会安装快捷方式或 Windows 服务；运行它仍需要 WebView2 Runtime。该包从 Linux 交叉构建，未在真实 Windows 设备启动验证。

## Linux x86_64

此便携程序面向 Linux x86_64，并且是动态链接程序，不是自包含的静态二进制文件。它需要图形桌面会话，以及 GTK 3（原生窗口/控件）和 WebKitGTK 4.1（Tauri 界面渲染）及其系统依赖。Tauri 的[官方 Debian 运行依赖说明](https://v2.tauri.app/distribute/debian/)列出 `libgtk-3-0` 和 `libwebkit2gtk-4.1-0`；[官方 Linux 前置条件](https://v2.tauri.app/start/prerequisites/)按发行版列出编译用依赖。下列是常见运行库包名示例，并非适用于所有 Linux 发行版或版本的通用命令：

```sh
# Debian / Ubuntu：先确认当前发行版仓库提供这些包
sudo apt update
sudo apt install libgtk-3-0 libwebkit2gtk-4.1-0

# Fedora：不同 Fedora 版本或衍生版的包名/仓库可能有差异
sudo dnf install gtk3 webkit2gtk4.1

# Arch Linux：需启用提供对应包的仓库
sudo pacman -S --needed gtk3 webkit2gtk-4.1
```

如果包名不存在或依赖无法安装，请按自己的发行版和版本查询其软件仓库，并参考上面的 Tauri 官方清单；不要把 Debian/Ubuntu 命令直接当成其他发行版的安装命令。**从源代码编译**还需要编译器及 WebKitGTK 开发头文件等额外开发依赖；它们不同于运行此便携包所需的运行库，具体请看官方前置条件页面。

解压并从图形桌面会话启动：

```sh
tar -xzf locm-0.2.11-linux-x86_64.tar.gz
cd linux-x86_64
./local-engine-manager
```

若系统提示没有执行权限，可在解压目录运行 `chmod +x local-engine-manager` 后再试。该包没有桌面安装器或系统菜单快捷方式。

## 首次使用：引擎、模型分别准备

压缩包**不含** `llama.cpp`、`stable-diffusion.cpp`、GGUF/Safetensors 模型文件或任何模型/引擎权重；启动程序也不会默默下载它们。它们是独立的大文件，下载会占用模型目录所在磁盘的空间，部分模型/组件可能很大。下载前先查看应用显示的文件大小、目标目录和剩余空间，并确认模型来源与许可证。

建议按以下顺序操作：

1. 打开应用的“概览”，按需单独安装聊天用的 `llama.cpp` 引擎，或画图/视频共用的 `stable-diffusion.cpp` 引擎。安装需要网络，并由你在应用中主动点击。
2. 聊天：进入“模型”，从 Hugging Face 选择单文件 GGUF 下载，或点“导入本地 GGUF”；随后进入“运行”，选择模型并启动，再打开“对话”。模型文件不会因为安装了引擎而自动出现。
3. 画图/视频：在“模型 → 画图 / 视频模型”中明确选择并下载所需权重组件；也可选择 ComfyUI 目录进行只读扫描。扫描不会搬移或修改原文件。引擎和权重都必须分别准备。
4. 可在概览或设置中的模型目录选项查看/调整模型存放位置。确认该磁盘有足够余量；下载权重不属于便携包的一部分。

当前媒体引擎为 CPU 构建，不调用 GPU；项目尚未用真实模型权重验证生成兼容性。模型文件大小也不能代表所需内存或运行性能。

## 常见启动问题

- **Windows 提示 WebView2 缺失、窗口空白或程序无法显示界面：** 安装/修复 Microsoft WebView2 Evergreen Runtime；本版 NSIS 安装器缺少 Runtime 时会尝试联网运行微软 Bootstrapper。安装器中的 `WebView2Loader.dll` 只负责连接 Runtime，不是 Runtime 本身。
- **Agent 提示回复不符合 JSON 协议：** Agent 会尝试一次有限格式纠正；如果再次失败，任务安全暂停，且没有执行文件操作或命令。明确创建空文件夹时，Agent 应在展示计划后及时调用原生 `create_folder`，不通过 shell；它只接受所选工作区的相对路径、拒绝路径穿越和符号链接跳出，目标已存在时返回“已存在”且不修改。明确修改已有文件时，Agent 应先读取文件、再用 `edit_file` 提交唯一精确的 `old_text`/`new_text`；缺少匹配或多处匹配会失败，不会整文件覆盖。修改会先显示应用审批卡，再弹系统原生确认；确认后后端重新检查文件并在工作区内原子替换。任何 `run_command` 仍需你审阅并批准。
- **Linux 报 `libgtk-3.so.0`、`libwebkit2gtk-4.1.so.0` 或其他共享库缺失：** 按当前发行版安装 GTK 3、WebKitGTK 4.1 运行库及报错指出的依赖。可在终端运行 `ldd ./local-engine-manager` 查找标记为 `not found` 的库；该命令只用于定位，不会自动安装依赖。
- **Linux 终端提示权限不足：** 在解压目录运行 `chmod +x local-engine-manager`，再从图形桌面会话运行 `./local-engine-manager`。这是桌面 GUI 程序，不适用于没有图形显示会话的纯 headless 环境。
- **引擎安装或模型下载失败：** 检查网络/代理连接、目标磁盘余量和下载地址可达性；区分“引擎未安装”和“模型未导入/下载”。应用包本身不会补齐这些文件。
- **Linux 提示 `GLIBC_x.y` 版本缺失：** 这表示程序所需的系统 ABI 高于当前系统提供的版本，单装 GTK/WebKitGTK 未必能解决。Tauri 也提醒 Linux 二进制与构建基线、glibc 版本有关；本项目没有承诺兼容所有发行版和旧版本。

## 验证范围与限制

v0.2.11 发布检查包括 Rust 格式检查、默认与桌面 Rust 测试、JavaScript 回归测试及语法检查、Linux x86_64 原生桌面构建，以及 Windows x64 NSIS 和 GNU 便携测试包构建。NSIS 安装器由 Linux 使用 `cargo-xwin` 和 `x86_64-pc-windows-msvc` 目标交叉构建；检查安装器资产与其中的 Windows x64 应用文件，并确认没有模型权重。Agent 对不合规 JSON 最多进行一次格式纠正，连续失败时不执行工具或命令；有效的 `run_command` 仍只会进入应用确认卡，不会由解析器直接启动。Windows 包**没有在真实 Windows 设备启动验证**，也没有在用户的 Windows 电脑运行 MiniCPM5-1B 或真实模型端到端推理测试；不能将交叉构建称为 Windows 实机测试或据此承诺 Windows 兼容性。Linux 包为 Linux 原生构建。Agent 关闭清理只使用 LocM 持有的子进程句柄，不按进程名称终止用户自行运行的服务。

`create_folder` 与 `edit_file` 都通过原生工作区后端执行；前者是幂等空目录创建，后者只修改唯一精确匹配的既有 UTF-8 普通文件。`edit_file` 的首次调用不写入，仅显示待确认状态；用户批准后还必须通过系统原生确认，后端会重新读取、验证匹配与路径，再用同目录临时文件原子替换。Rust 与前端回归覆盖工具意图路由、工具结果续接、创建/编辑成功、确认边界、缺失或重复匹配、路径穿越和符号链接逃逸。两个工具均未在用户的 Windows 电脑实机运行验证。

本次回归结果：默认 Rust `cargo test --locked` 23/23、desktop Rust `cargo test --locked --features desktop --bin local-engine-manager` 84/84、JavaScript `node --test ui/*.test.js` 44/44 均通过。`create_folder` 覆盖新建空多级目录、幂等、目标为文件时拒绝、路径穿越/绝对路径及 Unix 符号链接逃逸拒绝；`edit_file` 覆盖需批准才修改、唯一精确替换、文件缺失、无匹配/重复/重叠匹配、路径穿越/绝对路径及 Unix 符号链接逃逸拒绝，并验证前端原生派发与确认文案。还验证两类用户意图路由和工具结果保持在当前计划中。上述 Agent 工具做了本机单元与协议测试，**没有在用户的 Windows 电脑上运行验证**。

Rust 的 Windows `raw-dylib` 链接会将 `kernel32.dll` 解析为 `-lKernel32`；Linux MinGW 的标准导入库名称为小写 `libkernel32.a`，而 GNU ld 在 Linux 大小写敏感的文件系统上无法用大写名称找到它。此仓库用 `./scripts/build-windows-gnu.sh` 选择专用链接包装器，仅对该库名做小写规范化；它不创建临时导入库别名，也不修改全局或系统 linker 配置。Windows 原生 MSVC 工作流与普通 Cargo 构建保持不变。

引擎和模型权重均需另行安装/导入；未使用真实模型权重验证聊天推理或媒体生成。Linux 与 Windows 的具体系统库、硬件加速能力、模型兼容性和下载环境都可能影响实际结果。
