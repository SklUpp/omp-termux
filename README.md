# omp-termux

Android/arm64 порт [oh-my-pi](https://github.com/can1357/oh-my-pi) для [Termux](https://github.com/termux/termux-app).

## Требования

- Termux на Android arm64 (aarch64), API 24+; root не нужен.
- Рекомендуемые сборки Termux: [F-Droid](https://f-droid.org/packages/com.termux/) или [GitHub](https://github.com/termux/termux-app/releases).

## Установка

```sh
curl -fsSL https://raw.githubusercontent.com/SklUpp/omp-termux/main/install.sh -o "$PREFIX/tmp/omp-install.sh"
less "$PREFIX/tmp/omp-install.sh"
sh "$PREFIX/tmp/omp-install.sh"
omp --version
```

Установка: `$PREFIX/lib/omp-termux`, launcher: `$PREFIX/bin/omp`. Обновление и выбор версии — повторный запуск: `OMP_VERSION='<release-tag>' sh "$PREFIX/tmp/omp-install.sh"` (по умолчанию `latest`).

## Браузер (опционально)

Chromium не входит в bundle и по умолчанию не устанавливается. Включить инструмент `browser` вместе с `x11-repo` и `chromium`: `OMP_INSTALL_BROWSER=1 sh "$PREFIX/tmp/omp-install.sh"`. Проверки — [verification.md](android/docs/verification.md).

## Документация

- [setup.md](android/docs/setup.md) — установка и удаление; [verification.md](android/docs/verification.md) — проверки.
- [port-architecture.md](android/docs/port-architecture.md), [ci-cd.md](android/docs/ci-cd.md) — устройство порта и CI.

## Лицензия

MIT. См. [LICENSE](LICENSE) и [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt).
