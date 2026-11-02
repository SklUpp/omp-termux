#!/usr/bin/env bash
set -euo pipefail

RUNNER_TOKEN="$1"
RUNNER_VERSION="2.322.0"
ANDROID_NDK_VERSION="r27c"

echo "=== 1. Install Rust, Bun, NDK ==="

# Rust
if ! command -v rustc &>/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
fi
source "$HOME/.cargo/env"
rustup target add aarch64-linux-android

# Bun
if ! command -v bun &>/dev/null; then
    curl -fsSL https://bun.sh/install | bash
fi
export BUN_INSTALL="$HOME/.bun"
export PATH="$BUN_INSTALL/bin:$PATH"

# Android NDK
if [ ! -d "$HOME/android-ndk-${ANDROID_NDK_VERSION}" ]; then
    echo "Downloading Android NDK ${ANDROID_NDK_VERSION}..."
    curl -fsSL -o /tmp/ndk.zip "https://dl.google.com/android/repository/android-ndk-${ANDROID_NDK_VERSION}-linux.zip"
    unzip -q /tmp/ndk.zip -d "$HOME"
    rm /tmp/ndk.zip
fi

export ANDROID_NDK_HOME="$HOME/android-ndk-${ANDROID_NDK_VERSION}"

# Environment for non-interactive shells
cat << 'ENV_EOF' >> "$HOME/.bashrc"
export PATH="$HOME/.cargo/bin:$HOME/.bun/bin:$PATH"
export ANDROID_NDK_HOME="$HOME/android-ndk-r27c"
export RUSTC_WRAPPER=sccache
export SCCACHE_DIR=/var/cache/sccache
ENV_EOF

sudo mkdir -p /var/cache/sccache
sudo chown -R $USER:$USER /var/cache/sccache

echo "=== 2. Setup GitHub Actions Runner ==="
mkdir -p "$HOME/actions-runner" && cd "$HOME/actions-runner"

if [ ! -f config.sh ]; then
    curl -o actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz -L "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz"
    tar xzf ./actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz
    rm ./actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz
fi

# Configure runner
./config.sh --url https://github.com/sasazemzulin058-debug/omp-termux \
            --token "$RUNNER_TOKEN" \
            --name "vps-64gb-runner" \
            --labels "self-hosted,linux,x64,vps-build" \
            --unattended \
            --replace

# Install and start systemd service
sudo ./svc.sh install $USER
sudo ./svc.sh start

echo "=== Runner setup complete and active! ==="
