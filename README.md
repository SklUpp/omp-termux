# omp-termux

Android/arm64 порт [oh-my-pi](https://github.com/can1357/oh-my-pi) (OMP) — агента для кодинга — для [Termux](https://github.com/termux/termux-app).

OMP — форк [Pi](https://github.com/badlogic/pi-mono) от [Mario Zechner](https://github.com/mariozechner). Этот репозиторий содержит upstream-код плюс очередь Android-патчей; интерфейс, провайдеры и документация агента — в upstream.

## Что это и зачем

- Агентский workflow upstream без изменений: сессии, субагенты, слэш-команды, LSP/DAP, провайдеры.
- Native Bionic runtime: собранный под Android arm64 Bun + JS + `pi-natives`. Никаких glibc-прослоек и сборки из исходников на устройстве.
- Установка из готового релизного bundle в Termux; root не нужен.
- Поддерживается только arm64 (aarch64). Бинарники собраны под API 24; широкого охвата всех прошивок нет — см. [setup.md](android/docs/setup.md).

## Установка готового релиза

Требуется Termux на Android arm64. Рекомендуемые источники: [F-Droid](https://f-droid.org/packages/com.termux/) или [GitHub](https://github.com/termux/termux-app/releases).

```sh
curl -fsSL https://raw.githubusercontent.com/SklUpp/omp-termux/main/install.sh -o "$PREFIX/tmp/omp-install.sh"
less "$PREFIX/tmp/omp-install.sh"   # необязательно: просмотреть перед запуском
env OMP_REPO=SklUpp/omp-termux OMP_INSTALL_BROWSER=0 sh "$PREFIX/tmp/omp-install.sh"
omp --version
```

- `OMP_REPO=SklUpp/omp-termux` обязателен: значение по умолчанию в `install.sh` указывает на старый форк.
- `OMP_INSTALL_BROWSER=0` — минимальная установка. Уберите переменную, чтобы `install.sh` доустановил `x11-repo` и `chromium` для инструмента `browser`.
- Скрипт скачивает `omp-termux.tar.gz` с `sha256`, проверяет контрольную сумму, прогоняет smoke-тест и загрузку native-аддона до подмены, делает swap с откатом при ошибке.
- Ставится в `$PREFIX/lib/omp-termux`, launcher — `$PREFIX/bin/omp`.

Конкретная версия:

```sh
env OMP_REPO=SklUpp/omp-termux OMP_INSTALL_BROWSER=0 \
  OMP_VERSION='<release-tag>' sh "$PREFIX/tmp/omp-install.sh"
```

`<release-tag>` — точный тег со страницы [Releases](https://github.com/SklUpp/omp-termux/releases).

## Обновление

`omp update` на Android не обновляет на месте: повторите установку тем же скриптом (по умолчанию берётся `latest`, либо укажите `OMP_VERSION`).

```sh
env OMP_REPO=SklUpp/omp-termux OMP_INSTALL_BROWSER=0 sh "$PREFIX/tmp/omp-install.sh"
```

## Браузер (опционально)

Chromium в bundle не входит и ставится отдельно из репозитория Termux:X11; без него остальные инструменты работают. Проверка и smoke-тесты — [verification.md](android/docs/verification.md).

## Документация порта

- [android/docs/setup.md](android/docs/setup.md) — требования, установка, удаление.
- [android/docs/verification.md](android/docs/verification.md) — проверки устройства и релиза.
- [android/docs/port-architecture.md](android/docs/port-architecture.md) — устройство порта и содержимое bundle.
- [android/docs/ci-cd.md](android/docs/ci-cd.md) — sync с upstream и публикация релизов.

## Лицензия

MIT, как и upstream. См. [LICENSE](LICENSE) и [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt). Attribution upstream-проекта сохранена: [oh-my-pi](https://github.com/can1357/oh-my-pi).
