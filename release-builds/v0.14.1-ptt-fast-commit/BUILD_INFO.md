# VoiceX v0.14.1 PTT Fast Commit

- Version: 0.14.1
- Platform: Windows x64 (x86_64-pc-windows-msvc)
- Source commit: 7abc5c1e57ecaa3da284edb09b5564dab9459638
- Successful CI run: https://github.com/breathi3552/VoiceX/actions/runs/33325822304

## Main changes

- Push-To-Talk key release is an explicit commit signal.
- Existing provider final text is preferred; otherwise the latest HUD/interim transcript is committed.
- Empty PTT snapshots are not injected.
- Late ASR final/partial/failure/finished messages are ignored after a PTT release commit, preventing duplicate input.
- Hands-Free keeps its existing ASR completion behavior.
- Gemini 3.5 transcription, Google Cloud STT, proxy transport, local ASR, and the existing text-injection pipeline remain enabled.

## Validation

- Standalone pure-Rust PTT state-machine tests executed successfully on Windows x64.
- Full VoiceX Rust test targets compiled successfully with cargo test --no-run.
- Tauri Windows x64 MSI and NSIS bundles built successfully.
