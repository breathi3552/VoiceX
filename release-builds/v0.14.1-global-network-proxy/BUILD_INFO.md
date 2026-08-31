# VoiceX v0.14.1 Global Network Proxy

- Version: 0.14.1
- Platform: Windows x64 (x86_64-pc-windows-msvc)
- Source commit: 7c239889c93ee22b555a85d4760481532749286b
- Successful CI run: https://github.com/breathi3552/VoiceX/actions/runs/33354861103

## Main changes

- Proxy settings moved out of ASR into the global Network settings page.
- Explicit No proxy, System proxy, and Custom proxy modes.
- Legacy explicit proxy URLs migrate to Custom mode without losing the saved URL.
- System mode reads the current Windows user proxy configuration for new connections.
- Direct mode explicitly bypasses system/environment proxy discovery.
- Gemini Live WebSocket and Google Cloud STT / Chirp 3 OAuth and gRPC share the same global proxy configuration.
- Local ASR remains independent of the global network proxy.
- Existing PTT release-to-commit source is unchanged and its standalone regression tests still pass.

## Validation

- Vue/TypeScript frontend type-check and Vite production build completed successfully.
- Global proxy persistence, UI placement, Windows proxy wiring, and cloud transport contract checks executed successfully.
- VoiceX Rust test targets, including the network proxy unit tests, compiled successfully with cargo test --no-run.
- Standalone pure-Rust PTT state-machine regression tests executed successfully on Windows x64.
- Tauri Windows x64 MSI and NSIS bundles built successfully.

Note: the full VoiceX lib-test executable is not launched in CI because the repository native test binary currently fails at process startup with STATUS_ENTRYPOINT_NOT_FOUND before Rust tests run; compilation still covers those tests.
