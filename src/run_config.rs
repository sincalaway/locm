use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    net::{IpAddr, Ipv4Addr, UdpSocket},
    path::Path,
    process::Command,
};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunPreferences {
    pub context: String,
    pub flash_attention: String,
    pub kv_cache_q8: bool,
    pub lan_access: bool,
    pub auto_restart: bool,
}

impl Default for RunPreferences {
    fn default() -> Self {
        Self {
            context: "auto".to_string(),
            flash_attention: "auto".to_string(),
            kv_cache_q8: false,
            lan_access: false,
            auto_restart: false,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    pub model: bool,
    pub host: bool,
    pub port: bool,
    pub ctx_size: bool,
    pub flash_attention: bool,
    pub cache_type_k: bool,
    pub cache_type_v: bool,
    pub n_gpu_layers: bool,
    pub reasoning: bool,
}

fn has_option(help: &str, option: &str) -> bool {
    help.lines().any(|line| {
        line.split(|character: char| character.is_whitespace() || character == ',')
            .any(|token| token.trim_matches(['[', ']', '=']) == option)
    })
}

pub fn parse_capabilities(help: &str) -> ServerCapabilities {
    ServerCapabilities {
        model: has_option(help, "--model"),
        host: has_option(help, "--host"),
        port: has_option(help, "--port"),
        ctx_size: has_option(help, "--ctx-size"),
        flash_attention: has_option(help, "--flash-attn"),
        cache_type_k: has_option(help, "--cache-type-k"),
        cache_type_v: has_option(help, "--cache-type-v"),
        n_gpu_layers: has_option(help, "--n-gpu-layers"),
        reasoning: has_option(help, "--reasoning"),
    }
}

pub fn probe_capabilities(executable: &Path) -> Result<ServerCapabilities, String> {
    let mut command = Command::new(executable);
    command.arg("--help");
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = command
        .output()
        .map_err(|error| format!("无法读取 llama-server 参数能力：{error}"))?;
    let mut help = String::from_utf8_lossy(&output.stdout).into_owned();
    help.push_str(&String::from_utf8_lossy(&output.stderr));
    if help.trim().is_empty() {
        return Err(format!(
            "llama-server --help 未返回参数说明（退出状态：{}）",
            output.status
        ));
    }
    let capabilities = parse_capabilities(&help);
    if !capabilities.model || !capabilities.host || !capabilities.port {
        return Err(
            "当前 llama-server 未在 --help 中确认 --model、--host 与 --port；为避免错误启动，不能启动服务"
                .to_string(),
        );
    }
    Ok(capabilities)
}

pub fn build_server_args(
    model_path: &Path,
    host: &str,
    port: u16,
    preferences: &RunPreferences,
    capabilities: &ServerCapabilities,
) -> Result<Vec<OsString>, String> {
    if !capabilities.model || !capabilities.host || !capabilities.port {
        return Err("当前 llama-server 不支持已核实的 --model/--host/--port 参数".to_string());
    }
    let mut args = vec![
        OsString::from("--model"),
        model_path.as_os_str().to_os_string(),
        OsString::from("--host"),
        OsString::from(host),
        OsString::from("--port"),
        OsString::from(port.to_string()),
    ];
    if capabilities.n_gpu_layers {
        args.extend([OsString::from("--n-gpu-layers"), OsString::from("0")]);
    }

    match preferences.context.as_str() {
        "auto" => {}
        "8192" | "16384" | "32768" | "65536" => {
            if !capabilities.ctx_size {
                return Err("当前 llama-server 不支持 --ctx-size；请改用“自动”上下文".to_string());
            }
            args.extend([
                OsString::from("--ctx-size"),
                OsString::from(&preferences.context),
            ]);
        }
        _ => return Err("上下文选项无效".to_string()),
    }

    match preferences.flash_attention.as_str() {
        "auto" => {}
        "on" | "off" => {
            if !capabilities.flash_attention {
                return Err(
                    "当前 llama-server 不支持 --flash-attn；此选项已禁用，未传递该参数".to_string(),
                );
            }
            args.extend([
                OsString::from("--flash-attn"),
                OsString::from(&preferences.flash_attention),
            ]);
        }
        _ => return Err("Flash Attention 选项无效".to_string()),
    }

    if preferences.kv_cache_q8 {
        if !capabilities.cache_type_k || !capabilities.cache_type_v {
            return Err(
                "当前 llama-server 未同时支持 --cache-type-k 与 --cache-type-v；Q8 KV 缓存不可用"
                    .to_string(),
            );
        }
        args.extend([
            OsString::from("--cache-type-k"),
            OsString::from("q8_0"),
            OsString::from("--cache-type-v"),
            OsString::from("q8_0"),
        ]);
    }
    Ok(args)
}

pub fn private_ipv4_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => ip.is_private() && !ip.is_loopback() && !ip.is_link_local(),
        IpAddr::V6(_) => false,
    }
}

/// Finds the private IPv4 interface chosen by the OS route table without sending a packet.
pub fn lan_bind_host() -> Result<String, String> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .map_err(|error| format!("无法查找局域网接口：{error}"))?;
    socket
        .connect((Ipv4Addr::new(192, 0, 2, 1), 9))
        .map_err(|error| format!("无法选择局域网接口：{error}"))?;
    let address = socket
        .local_addr()
        .map_err(|error| format!("无法读取局域网接口地址：{error}"))?
        .ip();
    if !private_ipv4_address(address) {
        return Err(
            "未找到可用的私有 IPv4 局域网地址；为避免绑定公网或所有网卡，服务仍保持未启动"
                .to_string(),
        );
    }
    Ok(address.to_string())
}

pub fn args_as_strings(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process::Command};
    use tempfile::tempdir;

    const FULL_HELP: &str = "--model FNAME\n--host HOST\n--port PORT\n-c, --ctx-size N\n-fa, --flash-attn [on|off|auto]\n-ctk, --cache-type-k TYPE\n-ctv, --cache-type-v TYPE\n-ngl, --n-gpu-layers N\n-rea, --reasoning [on|off|auto]\n";

    #[test]
    fn only_exact_help_options_are_detected() {
        let capabilities = parse_capabilities(FULL_HELP);
        assert!(capabilities.host && capabilities.port && capabilities.ctx_size);
        assert!(capabilities.flash_attention && capabilities.cache_type_k);
        assert!(capabilities.cache_type_v && capabilities.n_gpu_layers && capabilities.reasoning);
        let missing =
            parse_capabilities("--host HOST\n--port PORT\n--ctx-size N\n--flash-attention on\n");
        assert!(!missing.flash_attention);
        assert!(!missing.cache_type_k && !missing.cache_type_v);
    }

    #[test]
    fn launch_arguments_include_only_supported_requested_options() {
        let capabilities = parse_capabilities(FULL_HELP);
        let preferences = RunPreferences {
            context: "32768".to_string(),
            flash_attention: "on".to_string(),
            kv_cache_q8: true,
            lan_access: true,
            auto_restart: true,
        };
        let args = build_server_args(
            Path::new("/models/model with spaces.gguf"),
            "192.168.1.17",
            8686,
            &preferences,
            &capabilities,
        )
        .unwrap();
        assert_eq!(
            args_as_strings(&args),
            [
                "--model",
                "/models/model with spaces.gguf",
                "--host",
                "192.168.1.17",
                "--port",
                "8686",
                "--n-gpu-layers",
                "0",
                "--ctx-size",
                "32768",
                "--flash-attn",
                "on",
                "--cache-type-k",
                "q8_0",
                "--cache-type-v",
                "q8_0",
            ]
        );
    }

    #[test]
    fn default_preferences_stay_loopback_and_leave_context_to_the_model() {
        let preferences = RunPreferences::default();
        assert!(!preferences.lan_access);
        assert_eq!(preferences.context, "auto");
        assert!(!preferences.kv_cache_q8);
        let capabilities = parse_capabilities(FULL_HELP);
        let args = build_server_args(
            Path::new("/models/default.gguf"),
            "127.0.0.1",
            8686,
            &preferences,
            &capabilities,
        )
        .unwrap();
        let args = args_as_strings(&args);
        assert!(args.windows(2).any(|pair| pair == ["--host", "127.0.0.1"]));
        assert!(!args.iter().any(|arg| arg == "--ctx-size"));
        assert!(!args.iter().any(|arg| arg == "--flash-attn"));
        assert!(!args.iter().any(|arg| arg == "--cache-type-k"));
        assert!(!args.iter().any(|arg| arg == "--cache-type-v"));
    }

    #[test]
    fn unsupported_flags_are_never_silently_passed() {
        let capabilities = parse_capabilities("--model FNAME\n--host HOST\n--port PORT\n");
        let mut preferences = RunPreferences::default();
        preferences.context = "8192".to_string();
        assert!(build_server_args(
            Path::new("m.gguf"),
            "127.0.0.1",
            8686,
            &preferences,
            &capabilities
        )
        .unwrap_err()
        .contains("不支持 --ctx-size"));
        preferences.context = "auto".to_string();
        preferences.flash_attention = "on".to_string();
        assert!(build_server_args(
            Path::new("m.gguf"),
            "127.0.0.1",
            8686,
            &preferences,
            &capabilities
        )
        .unwrap_err()
        .contains("不支持 --flash-attn"));
        preferences.flash_attention = "auto".to_string();
        preferences.kv_cache_q8 = true;
        assert!(build_server_args(
            Path::new("m.gguf"),
            "127.0.0.1",
            8686,
            &preferences,
            &capabilities
        )
        .unwrap_err()
        .contains("Q8 KV 缓存不可用"));
    }

    #[cfg(unix)]
    #[test]
    fn fake_server_help_probe_and_process_capture_the_exact_launch_arguments() {
        let directory = tempdir().unwrap();
        let fake_server = directory.path().join("fake-llama-server.sh");
        let captured = directory.path().join("captured-args.txt");
        fs::write(
            &fake_server,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--help\" ]; then\n  printf '%s\\n' '{}'\n  exit 0\nfi\nprintf '%s\\n' \"$@\" > \"$CAPTURE_ARGS\"\n",
                FULL_HELP.lines().collect::<Vec<_>>().join("' '")
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake_server, fs::Permissions::from_mode(0o755)).unwrap();

        let capabilities = probe_capabilities(&fake_server).unwrap();
        let preferences = RunPreferences {
            context: "16384".to_string(),
            flash_attention: "off".to_string(),
            kv_cache_q8: true,
            lan_access: false,
            auto_restart: false,
        };
        let args = build_server_args(
            Path::new("/tmp/fake model.gguf"),
            "127.0.0.1",
            8686,
            &preferences,
            &capabilities,
        )
        .unwrap();
        let status = Command::new(&fake_server)
            .args(&args)
            .env("CAPTURE_ARGS", &captured)
            .status()
            .unwrap();
        assert!(status.success());
        let observed: Vec<String> = fs::read_to_string(captured)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(observed, args_as_strings(&args));
        assert!(observed
            .windows(2)
            .any(|pair| pair == ["--ctx-size", "16384"]));
        assert!(observed
            .windows(2)
            .any(|pair| pair == ["--flash-attn", "off"]));
        assert!(observed
            .windows(2)
            .any(|pair| pair == ["--cache-type-k", "q8_0"]));
        assert!(observed
            .windows(2)
            .any(|pair| pair == ["--cache-type-v", "q8_0"]));
    }

    #[test]
    fn lan_binding_accepts_only_private_ipv4() {
        assert!(private_ipv4_address("192.168.1.2".parse().unwrap()));
        assert!(private_ipv4_address("10.1.2.3".parse().unwrap()));
        assert!(!private_ipv4_address("127.0.0.1".parse().unwrap()));
        assert!(!private_ipv4_address("8.8.8.8".parse().unwrap()));
        assert!(!private_ipv4_address("fe80::1".parse().unwrap()));
    }
}
