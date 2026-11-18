use serde::Deserialize;
use std::{
    env,
    error::Error,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EngineConfig {
    server_executable: PathBuf,
    model: PathBuf,
    ctx_size: u32,
    n_gpu_layers: i32,
}

type AppResult<T> = Result<T, Box<dyn Error>>;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("启动失败：{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> AppResult<ExitCode> {
    let mut args = env::args_os();
    let _program = args.next();
    let config_arg = args.next().unwrap_or_else(|| OsString::from("engine.json"));
    if args.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "用法：llama-server-launcher-rs [engine.json 路径]",
        )
        .into());
    }

    let config_arg = PathBuf::from(config_arg);
    let config_path = if config_arg.is_absolute() {
        config_arg
    } else {
        env::current_dir()?.join(config_arg)
    };
    let config_path = config_path.canonicalize()?;
    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));

    let config_text = fs::read_to_string(&config_path)?;
    let config: EngineConfig = serde_json::from_str(&config_text)?;
    if config.ctx_size == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "ctx_size 必须大于 0").into());
    }
    if config.n_gpu_layers < -1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "n_gpu_layers 示例校验只接受 -1 或非负整数",
        )
        .into());
    }

    let server = resolve_existing_file(config_dir, &config.server_executable, "server_executable")?;
    let model = resolve_existing_file(config_dir, &config.model, "model")?;
    let child_args = build_args(&config, &model);

    eprintln!("启动子进程：{:?} {:?}", server, child_args);
    // 直接传入参数，不经过 shell；含空格的路径也会作为单个参数传递。
    let status = Command::new(&server).args(&child_args).status()?;

    let code = status
        .code()
        .and_then(|value| u8::try_from(value).ok())
        .unwrap_or(1);
    Ok(ExitCode::from(code))
}

fn resolve_existing_file(config_dir: &Path, configured: &Path, field: &str) -> io::Result<PathBuf> {
    let candidate = if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        config_dir.join(configured)
    };
    let resolved = candidate.canonicalize().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("{field} 路径不可用（{}）：{error}", candidate.display()),
        )
    })?;
    if !resolved.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{field} 必须指向一个文件：{}", resolved.display()),
        ));
    }
    Ok(resolved)
}

fn build_args(config: &EngineConfig, model: &Path) -> Vec<OsString> {
    vec![
        OsString::from("--model"),
        model.as_os_str().to_os_string(),
        OsString::from("--ctx-size"),
        OsString::from(config.ctx_size.to_string()),
        OsString::from("--n-gpu-layers"),
        OsString::from(config.n_gpu_layers.to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_path_with_spaces_remains_one_argument() {
        let config = EngineConfig {
            server_executable: PathBuf::from("bin/llama-server.exe"),
            model: PathBuf::from("models/model with spaces.gguf"),
            ctx_size: 4096,
            n_gpu_layers: 12,
        };
        let model = Path::new("/fixture/models/model with spaces.gguf");
        let args = build_args(&config, model);
        let rendered: Vec<String> = args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            rendered,
            [
                "--model",
                "/fixture/models/model with spaces.gguf",
                "--ctx-size",
                "4096",
                "--n-gpu-layers",
                "12"
            ]
        );
    }
}
