# rewrite-it

A self-contained, locally-running "Help me rewrite" service for Linux desktops,
inspired by Microsoft Edge's built-in AI writing assistant.

It exposes a **DBus session-bus service** so any application, shell script, or
keyboard shortcut can request AI-powered text rewriting without sending data to
the cloud.

---

## Features

| | |
|---|---|
| **100 % local** | Phi-4-mini runs entirely on your machine — no API key, no telemetry |
| **Self-contained** | llama.cpp is compiled from source at build time (no external llama binaries) |
| **DBus integration** | First-class Linux desktop integration (KDE Plasma + GNOME) |
| **DBus activation** | Daemon auto-starts on first request; no background service manager needed |
| **Lazy model startup** | Model download and load happen on first real rewrite request |
| **Streaming** | `StartRewrite` emits token-by-token DBus signals for live UX |
| **Clipboard helper** | Rewrites the current selection and updates the clipboard |
| **Wayland paste** | On Plasma Wayland, can send a portal-approved `Ctrl+V` after rewriting |
| **GPU-optional** | Build with `--features cuda` or `--features vulkan` for GPU acceleration |

---

## Quick start

```bash
# 1. Install (builds and registers keyboard shortcut)
bash install.sh

# 2. Start the daemon (auto-starts via DBus activation too)
rewrite-it

# 3. Check readiness/status
rewrite-it status

# 4. Rewrite from the terminal
echo "the cat eat the mouse" | rewrite-it rewrite
# → "The cat eats the mouse."

# 4a. Smoke-test the local LLM directly without DBus
echo "the cat eat the mouse" | cargo run --bin llm-test -- --style grammar
# → "The cat eats the mouse."

# 5. Or use the keyboard shortcut (select text first)
#    KDE:   Meta+Shift+G
#    GNOME: Super+Shift+G
```

On KDE Plasma Wayland, the first shortcut-triggered paste may show an
`xdg-desktop-portal-kde` permission dialog because keyboard injection is done
through the XDG Remote Desktop portal. If permission is denied or the portal is
unavailable, rewrite-it still succeeds and leaves the rewritten text on the
clipboard for manual paste.

---

## Default model

[Phi-4-mini-instruct](https://huggingface.co/unsloth/Phi-4-mini-instruct-GGUF)
in Unsloth's `Q4_K_M` GGUF (~2.49 GB), chosen as a lighter CPU-friendly default.

The model is automatically downloaded on first rewrite request to
`~/.local/share/rewrite-it/models/`.

To use a different GGUF model, edit `~/.config/rewrite-it/config.toml`:

```toml
model_path  = "/path/to/your/model.gguf"
hf_repo     = "unsloth/Phi-4-mini-instruct-GGUF"   # used only when model_path is absent
hf_filename = "Phi-4-mini-instruct-Q4_K_M.gguf"
```

---

## Build options

```bash
# CPU only (default)
cargo build --release

# NVIDIA GPU
cargo build --release --features cuda

# Vulkan (AMD / Intel / NVIDIA)
cargo build --release --features vulkan
```

Then set `n_gpu_layers` in the config to the number of transformer layers
to offload (`999` = offload all):

```toml
n_gpu_layers = 999
```

---

## DBus interface

**Service:** `org.rewriteit.Rewriter1`
**Object:** `/org/rewriteit/Rewriter`
**Interface:** `org.rewriteit.Rewriter1`

### Methods

| Method | Signature | Description |
|--------|-----------|-------------|
| `Rewrite` | `(s, s) → s` | Blocking rewrite (text, style → result) |
| `StartRewrite` | `(s, s) → s` | Streaming rewrite → returns job_id |
| `RewriteSelection` | `(s) → ()` | Rewrite the current selection/clipboard using `style`, then copy the result back |
| `ListStyles` | `() → as` | List available styles |
| `IsReady` | `() → b` | True when model is loaded |

### Signals

| Signal | Signature | Description |
|--------|-----------|-------------|
| `Chunk` | `(s, s)` | `(job_id, text_piece)` |
| `Done` | `(s)` | `(job_id)` — generation finished |
| `Error` | `(s, s)` | `(job_id, error_message)` |

### Rewriting styles

| Name | Description |
|------|-------------|
| `grammar` | Fix grammar, spelling, punctuation (default) |
| `formal` | Elevate to formal/professional tone |
| `casual` | Relax to conversational tone |
| `concise` | Shorten, remove filler |
| `elaborate` | Expand with detail |
| `creative` | Creative rewrite, same meaning |

### Example: call from the terminal

```bash
# Full blocking rewrite
busctl --user call org.rewriteit.Rewriter1 /org/rewriteit/Rewriter \
    org.rewriteit.Rewriter1 Rewrite ss "the cat eat the mouse" "grammar"

# Rewrite the current selection / clipboard using the "grammar" style
busctl --user call org.rewriteit.Rewriter1 /org/rewriteit/Rewriter \
    org.rewriteit.Rewriter1 RewriteSelection s "grammar"

# Or use the built-in CLI client
echo "the cat eat the mouse" | rewrite-it rewrite --style grammar
```

---

## Configuration

`~/.config/rewrite-it/config.toml` (created with defaults on first run):

```toml
model_path   = "~/.local/share/rewrite-it/models/Phi-4-mini-instruct-Q4_K_M.gguf"
hf_repo      = "unsloth/Phi-4-mini-instruct-GGUF"
hf_filename  = "Phi-4-mini-instruct-Q4_K_M.gguf"
context_size = 4096
max_tokens   = 1024
temperature  = 0.3
n_gpu_layers = 0
seed         = 42
# n_threads  = 8   # uncomment to pin CPU thread count
```

Use `rewrite-it status` to inspect whether the daemon is `idle`, `downloading`,
`loading`, `ready`, or `failed`.

---

## Dependencies

| Package | Purpose |
|---------|---------|
| `cmake` | Build llama.cpp (required) |
| `cc` / `g++` | C/C++ compiler for llama.cpp |
| `wl-clipboard` | Wayland clipboard (optional, for keyboard shortcut) |
| `xclip` | X11 clipboard (optional, for keyboard shortcut) |
| `xdg-desktop-portal` + `xdg-desktop-portal-kde` | Wayland keyboard injection permission for portal-backed paste (optional) |
| `libnotify` | Desktop notifications via `notify-send` (optional) |

The portal packages are only needed for automatic paste on Wayland. Clipboard
rewrite continues to work without them.

---

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│  Desktop (KDE / GNOME)                                  │
│    keyboard shortcut ──→ KWin / desktop shortcut        │
│    Dolphin right-click → rewrite-it-selection (shell)   │
└────────────────────┬────────────────────────────────────┘
                     │ DBus method call / rewrite-it rewrite --style … (CLI)
                     │
┌────────────────────▼────────────────────────────────────┐
│  DBus session bus                                       │
│    org.rewriteit.Rewriter1 @ /org/rewriteit/Rewriter    │
└────────────────────┬────────────────────────────────────┘
                     │
┌────────────────────▼────────────────────────────────────┐
│  rewrite-it daemon (Rust + tokio + zbus)                │
│    selection read → LLM rewrite → clipboard update      │
│    Wayland paste via XDG Remote Desktop portal          │
│    ┌──────────────────────────────────────────────────┐ │
│    │  LLM Engine (llama-cpp-2)                        │ │
│    │    Phi-4-mini-instruct Q4_K_M (≈2.49 GB)         │ │
│    │    CPU / CUDA / Vulkan                           │ │
│    └──────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────┘
```

---

## License

MIT OR Apache-2.0
