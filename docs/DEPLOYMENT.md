# 部署与运行手册

本文说明 v0.2.9 桌面版的系统依赖、便携包启动、首次配置和常见故障。应用基于 Tauri 2：窗口由操作系统提供的 WebView 绘制界面，因此它和应用本身是两回事。Windows 使用 Microsoft Edge WebView2 Runtime；Linux 使用 GTK 3 与 WebKitGTK 4.1。缺少相应运行库时，程序可能无法打开窗口。

## 获取的包是什么

当前 Tauri 配置关闭了安装包生成（`bundle.active: false`）。本项目提供的是压缩包中的便携测试程序，不是 MSI、`.deb`、AppImage 或带安装向导的安装器。v0.2.9 发布包名称与主要内容为：

| 平台 | 压缩包 | 解压后的主要文件 |
| --- | --- | --- |
| Linux x86_64 | `locm-0.2.9-linux-x86_64.tar.gz` | `linux-x86_64/local-engine-manager`、`LICENSE`、`README.txt`、`icons/`（PNG、SVG、ICO） |
| Windows x64 GNU | `locm-0.2.9-windows-x64-gnu-test.zip` | `windows-x64-gnu/local-engine-manager.exe`、`WebView2Loader.dll`、`LICENSE`、`README.txt`、`icons/`（PNG、SVG、ICO） |

这些名称对应仓库本地 `target/delivery/locm-v0.2.9/` 的构建产物；该目录是本地交付目录，不是 Git 跟踪的 `dist/` 发布文件。也可从[项目 v0.2.9 发布页](https://github.com/sincalaway/locm/releases/tag/v0.2.9)下载。Windows ZIP 名称中的 `test` 是有意保留的：该版本尚未在真实 Windows 设备启动验证。

## Windows x64

1. 确认系统为 64 位 Windows。先查看“设置 → 应用 → 已安装的应用”中是否已有 Microsoft Edge WebView2 Runtime；若没有，从 [Microsoft 官方 WebView2 Runtime 页面](https://developer.microsoft.com/en-us/microsoft-edge/webview2)安装 Evergreen Bootstrapper。需要离线安装时，该页面也提供 Evergreen Standalone Installer。
2. 将 ZIP **完整解压**到一个文件夹。不要单独移动或删除 `WebView2Loader.dll`；它必须与 `local-engine-manager.exe` 保持在同一个 `windows-x64-gnu` 文件夹内。
3. 双击 `local-engine-manager.exe` 启动。它是便携程序，不会安装 Windows 服务或自动安装模型、引擎。

WebView2 Runtime 是负责显示应用界面的浏览器运行时；包内的 `WebView2Loader.dll` 只是让程序连接到该 Runtime 的加载库，不能代替 Runtime，也不包含完整 Edge 浏览器运行环境。

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
tar -xzf locm-0.2.9-linux-x86_64.tar.gz
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

- **Windows 提示 WebView2 缺失、窗口空白或程序无法显示界面：** 安装/修复 Microsoft WebView2 Evergreen Runtime；确认 ZIP 已完整解压，并且 `WebView2Loader.dll` 与 EXE 同目录。不要把 loader DLL 当成 Runtime。
- **Linux 报 `libgtk-3.so.0`、`libwebkit2gtk-4.1.so.0` 或其他共享库缺失：** 按当前发行版安装 GTK 3、WebKitGTK 4.1 运行库及报错指出的依赖。可在终端运行 `ldd ./local-engine-manager` 查找标记为 `not found` 的库；该命令只用于定位，不会自动安装依赖。
- **Linux 终端提示权限不足：** 在解压目录运行 `chmod +x local-engine-manager`，再从图形桌面会话运行 `./local-engine-manager`。这是桌面 GUI 程序，不适用于没有图形显示会话的纯 headless 环境。
- **引擎安装或模型下载失败：** 检查网络/代理连接、目标磁盘余量和下载地址可达性；区分“引擎未安装”和“模型未导入/下载”。应用包本身不会补齐这些文件。
- **Linux 提示 `GLIBC_x.y` 版本缺失：** 这表示程序所需的系统 ABI 高于当前系统提供的版本，单装 GTK/WebKitGTK 未必能解决。Tauri 也提醒 Linux 二进制与构建基线、glibc 版本有关；本项目没有承诺兼容所有发行版和旧版本。

## 验证范围与限制

v0.2.9 验证通过 Rust 格式检查、23 项默认 Rust 测试、70 项桌面 Rust 测试、29 项 JavaScript 回归测试、JavaScript 语法检查和 `git diff --check`，并分别构建 Linux x86_64 桌面程序与 Windows x64 GNU GUI EXE。Linux 包为 Linux 原生构建；Windows 包从 Linux 使用 `x86_64-pc-windows-gnu` 目标交叉构建，且 PE 文件格式检查确认其为 x86-64 Windows 可执行文件；包内包含与 EXE 同目录所需的 WebView2 Loader。Windows 包**没有在真实 Windows 设备启动验证**，本次也没有进行真实模型桌面端到端推理测试；不能将交叉构建称为 Windows 实机测试或据此承诺 Windows 兼容性。项目当前也没有生成 MSI/`.deb` 等安装包。

Rust 的 Windows `raw-dylib` 链接会将 `kernel32.dll` 解析为 `-lKernel32`；Linux MinGW 的标准导入库名称为小写 `libkernel32.a`，而 GNU ld 在 Linux 大小写敏感的文件系统上无法用大写名称找到它。此仓库用 `./scripts/build-windows-gnu.sh` 选择专用链接包装器，仅对该库名做小写规范化；它不创建临时导入库别名，也不修改全局或系统 linker 配置。Windows 原生 MSVC 工作流与普通 Cargo 构建保持不变。

引擎和模型权重均需另行安装/导入；未使用真实模型权重验证聊天推理或媒体生成。Linux 与 Windows 的具体系统库、硬件加速能力、模型兼容性和下载环境都可能影响实际结果。
