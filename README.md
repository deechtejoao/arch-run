# arch-run

arch-run is a Rust-based command-line utility for executing Arch Linux applications in a sandboxed, ephemeral environment. It allows users to run software from the Arch Linux repositories without permanent system installation, utilizing Bubblewrap for isolation and a custom symlink-based filesystem overlay.

## Core Features

- **Dependency Resolution**: Leverages `pacman -Sp` to resolve full dependency trees without requiring root privileges.
- **Concurrent Retrieval**: Uses `tokio` and `reqwest` for parallel downloading of package layers from Arch Linux mirrors.
- **Filesystem Isolation**: Employs Bubblewrap (`bwrap`) to create a secure, read-only sandbox for application execution.
- **Dynamic Symlink Farm**: Merges multiple package layers (usr, etc, lib, etc.) into a unified filesystem view at runtime.
- **Path Rewriting**: Automatically rewrites absolute paths in shell scripts and symlink targets to point within the sandbox overlay.
- **GUI Support**: Optional forwarding of X11, Wayland, GPU (DRI), and audio (PulseAudio/PipeWire) into the sandbox.
- **High-Performance I/O**: Includes a `tokio-uring` backed I/O abstraction for Linux, providing asynchronous completion-queue based operations.
- **Capability-Based Security**: Uses `cap-std` for strict directory sandboxing, preventing path traversal in the I/O layer.

## Architecture

The system is composed of several key modules:

- **Core Engine (`src/main.rs`)**: Orchestrates dependency resolution, fetching, symlink farm construction, and sandbox execution.
- **Cache Manager (`src/cache.rs`)**: Manages the local package cache with a structured priority system (CLI > Env > Config > Default).
- **Sandbox I/O (`src/bin/sandbox_io.rs`)**: Provides a specialized I/O interaction layer using `tokio-uring` for Linux and standard `tokio` as a fallback.

## Usage

### Basic Execution

Execute a package by name:

```bash
arch-run <package_name> [arguments...]
```

Example:
```bash
arch-run fastfetch
```

### Options

- `--bin <name>`: Specify a particular binary to run if the package provides multiple.
- `--gui`: Enable support for graphical applications.
- `--verbose`, `-v`: Increase logging verbosity.

### Cache Management

- `arch-run list`: List all currently cached package layers.
- `arch-run prune --all`: Completely clear the local package cache.

## Technical Details

- **Language**: Rust (Edition 2021)
- **Primary Dependencies**: `tokio`, `serde`, `clap`, `reqwest`, `zstd`, `tar`, `cap-std`, `tokio-uring` (Linux).
- **Sandbox Engine**: Bubblewrap (`bwrap`).
- **Overlay Path**: `/tmp/arch-run/` (inside the sandbox).
