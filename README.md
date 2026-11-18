# llama-server-launcher-rs

一个最小、独立的 Rust 示例：从 JSON 配置读取模型和 server 路径，并通过子进程启动 `llama-server`。项目名描述它实际做的事情；它不是 LocModel 的源码，也不声称准确复刻 LocModel 3.3.0。

## 背景与边界

本示例的起点是对一个 LocModel 3.3.0 Windows EXE 所做的只读静态字符串检查。可见字符串包括 `llama-server.exe`、`engine.json` 和 `--model`、`--ctx-size`、`--n-gpu-layers` 等参数名；这只能说明二进制中存在这些线索，不能证明实际调用链、配置 schema、参数值或运行行为。未将该 EXE 或任何 LocModel 文件复制进本仓库。

下文中的 JSON schema、路径解析约定和参数映射均为本项目自行设计的示例假设。项目与 LocModel 或 llama.cpp 项目无官方关联或背书。

## 配置格式（本项目定义）

复制 [`engine.example.json`](engine.example.json) 为 `engine.json`，并按本地文件位置修改路径：

```json
{
  "server_executable": "bin/llama-server.exe",
  "model": "models/example.gguf",
  "ctx_size": 4096,
  "n_gpu_layers": 0
}
```

`server_executable`、`model`、`ctx_size`、`n_gpu_layers` 是这个示例的字段，不是已确认的 LocModel 字段。相对路径以 `engine.json` 所在目录为基准；绝对路径也可用。程序要求 `ctx_size` 大于 0，并在示例层面允许 `n_gpu_layers` 为 `-1` 或非负数。

程序只传递 `--model`、`--ctx-size`、`--n-gpu-layers` 这三组选项，不设置 host 或 port，也不加入额外参数。实际选项支持情况及语义取决于所使用的 llama.cpp/server 版本，请先核对该二进制的 `--help`。

## 本地运行

需要安装 Rust 和 Cargo，并准备可信、与本机系统匹配的 `llama-server` 和 GGUF 模型。Windows PowerShell 示例：

```powershell
Copy-Item .\engine.example.json .\engine.json
# 编辑 engine.json 中的 server_executable 和 model 路径
cargo test --locked
cargo run --locked -- .\engine.json
```

Linux/macOS shell 示例：

```sh
cp engine.example.json engine.json
# 编辑 engine.json 中的 server_executable 和 model 路径
cargo test --locked
cargo run --locked -- ./engine.json
```

相对路径由配置文件所在目录解析，不依赖启动器的工作目录。`cargo run` 会以前台方式等待 `llama-server` 退出，并返回其退出码（无法取得退出码时返回失败）。

## Windows x64 构建

在 Windows x64 机器上本地构建：

```powershell
cargo build --release --locked
```

生成的程序位于 `target\release\llama-server-launcher-rs.exe`。仓库还包含 GitHub Actions workflow：Windows x64 MSVC runner 会运行测试、构建 release EXE，并将 EXE 作为 workflow artifact 上传；它不会创建 GitHub Release，也不会自动发布二进制附件。

## 安全说明与限制

- Rust `Command` 逐项传递参数，不经过 shell；带空格的模型路径不会被拆成多个参数。
- `server_executable` 仍可指向任意本地程序。只配置并启动你信任的可执行文件；配置和模型文件也应来自可信来源。
- 本程序不下载、验证签名或扫描 `llama-server` 和模型文件；也不提供服务健康检查、停止/重启管理、GUI、打包或网络访问控制。
- 示例未设置监听地址和端口，服务监听行为由实际 server 版本的默认值决定。对外开放或在共享网络中运行前，应先核对并限制监听设置。
- `engine.example.json` 是演示配置，不是 LocModel 配置文件；本项目没有测试或执行 LocModel EXE。

## 许可

本项目采用 MIT License，详见 [`LICENSE`](LICENSE)。
