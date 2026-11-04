# omp-termux

Android/arm64 port of [oh-my-pi](https://github.com/can1357/oh-my-pi) for [Termux](https://github.com/termux/termux-app).

The port runs OMP with Android Bun and an arm64 native addon. It does not require root or a Linux compatibility layer.
The standard release supports core OMP tools. The extended release adds retained isolated workspaces and deferred integration.
Chromium is optional. Audio, desktop automation, and full display capture are not supported.

## Requirements

Termux on Android arm64 (aarch64), API 24 or later. Use [F-Droid](https://f-droid.org/packages/com.termux/) or [GitHub](https://github.com/termux/termux-app/releases).

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/SklUpp/omp-termux/main/install.sh -o "$PREFIX/tmp/omp-install.sh"
less "$PREFIX/tmp/omp-install.sh"
sh "$PREFIX/tmp/omp-install.sh"
omp --version
```

In a terminal, the installer asks separately about each choice that is not already set.
Without a terminal or an explicit choice, it selects the standard release and skips Chromium.
Use `-ex` for extended and `--browser` for Chromium. See [setup.md](android/docs/setup.md) for all options and recovery.
The Chromium choice installs `x11-repo` and updates Chromium and `libc++` through Termux packages.
`libc++` is a shared library. Its update can affect other Termux programs.

## Documentation

- [Setup](android/docs/setup.md) and [verification](android/docs/verification.md).
- [Port architecture](android/docs/port-architecture.md), [port changes](android/docs/port-changes.md), and [CI](android/docs/ci-cd.md).

## License

MIT. See [LICENSE](LICENSE) and [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt).
