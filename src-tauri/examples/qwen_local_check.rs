//! Manual check: exercise the real QwenLocalAsrClient end to end.
//!
//! Verifies the path a packaged build takes, not just the developer shell:
//! run it under a stripped PATH to reproduce what a bundled macOS app sees.
//!
//!   cargo run --example qwen_local_check -- <model_dir> <audio.wav>
//!   env -i HOME="$HOME" PATH=/usr/bin:/bin cargo run --example ...

use std::path::PathBuf;

use voicex_lib::asr::{qwen_local_client::resolve_qwen_local_command, AsrConfig, AsrProviderType};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let model_dir = args
        .next()
        .expect("usage: qwen_local_check <model_dir> <wav> [command_path]");
    let wav = args
        .next()
        .expect("usage: qwen_local_check <model_dir> <wav> [command_path]");
    // Optional: pass the exact value stored in settings to reproduce a report.
    let command_path = args.next().unwrap_or_default();

    println!("PATH     = {}", std::env::var("PATH").unwrap_or_default());
    println!("configured command path = {command_path:?}");
    match resolve_qwen_local_command(&command_path) {
        Ok(p) => println!("resolved = {}", p.display()),
        Err(e) => {
            println!("resolved = FAILED: {e}");
            std::process::exit(1);
        }
    }

    let config = AsrConfig {
        provider_type: AsrProviderType::QwenLocal,
        qwen_local_command_path: command_path,
        qwen_local_model_dir: model_dir,
        qwen_local_language: "Chinese".to_string(),
        qwen_local_use_dictionary: true,
        hotwords: vec!["Tauri".to_string(), "Pinia".to_string()],
        ..Default::default()
    };
    println!("is_valid = {}", config.is_valid());

    let client = voicex_lib::asr::QwenLocalAsrClient::new(config);
    let started = std::time::Instant::now();
    match client.transcribe_file(&PathBuf::from(&wav)).await {
        Ok(text) => println!("\n[{:?}] {text}", started.elapsed()),
        Err(e) => {
            println!("\nERROR: {e}");
            std::process::exit(1);
        }
    }
}
