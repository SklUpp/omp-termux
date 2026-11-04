# CI/CD: Android ARM64 / Termux

Android release automation uses official stable Bun. CI does not compile Bun
from source.

## Workflows

| Workflow | Trigger | Purpose |
|---|---|---|
| `sync-upstream.yml` | Scheduled/manual | Track the official upstream SHA, then dispatch the release-tree job. |
| `port-patches.yml` | Sync completion/manual | Build the standard and the extended tree independently. Publish the standard `mod`, create variant tags, dispatch releases. |
| `ci.yml` | Push/PR/manual/reusable | Run repository checks and Android overlay checks. |
| `android-release.yml` | Termux tag/manual | Build one variant (standard or `-ex` extended). Verify, package, and publish the release. |

`android/versions.env` is the source of truth for the Bun, NDK, Android API,
and Rust values. The OMP version comes from
`packages/coding-agent/package.json`.

```sh
cat android/versions.env
python3 -c 'import json; print(json.load(open("packages/coding-agent/package.json"))["version"])'
```

Manifest fields:

- `BUN_VERSION` — the stable Bun version that ships in the release.
- `BUN_BOOTSTRAP_VERSION` — the CI-only Bun that the JS and native jobs use.
- `BUN_ARCHIVE_NAME` — the official Bun Android ARM64 release asset.
- `BUN_SHA256` — the exact checksum of that asset.
- `NDK_VERSION`, `ANDROID_API`, `RUST_TOOLCHAIN` — the Android build inputs.

## Release flow

1. `sync-upstream.yml` tracks `main` in `can1357/oh-my-pi`.
2. `port-patches.yml` applies the sequential raw Git patch queue.
3. CI validates the patch queue, the release inputs, and the frozen lockfile.
4. `release-tree` builds two variants independently: standard (`01`) and
   extended (`01+02`). Standard publishes `mod`. Each variant creates its
   immutable tag (`v<ver>-termux[-ex]`) and dispatches `android-release.yml`.
   A failed extended build does not block the standard build.
5. `android-release.yml` runs repository checks, Android overlay checks, the
   native addon build, and the JS bundle build.
6. `release-publish` downloads official stable Bun:
```text
https://github.com/oven-sh/bun/releases/download/bun-v${BUN_VERSION}/${BUN_ARCHIVE_NAME}
```

This job verifies `BUN_SHA256`, extracts the executable `bun`, checks the
aarch64 ELF, checks the runtime version, and packages the binary as `./bun`.
It never uses npm Bun or Termux `pkg bun`, and it has no silent fallback.

Standard releases run one at a time. Before promotion, CI compares the
candidate against the current `isLatest` tag. An older standard release is
published, but it does not become latest. Extended releases use
`prerelease: true` and `make_latest: false`, so only a standard release can be
latest.

Release output:

- `omp-termux.tar.gz`
- `omp-termux.tar.gz.sha256`
- `pi_natives.android-arm64.node`
- `pi_natives.android-arm64.node.sha256`
- `verify-browser.sh`
- `smoke-browser.sh`

## Local checks

```sh
python3 android/scripts/verify-overlay.py
OMP_PATCH_PROFILE=standard python3 android/scripts/check-release-inputs.py
OMP_PATCH_PROFILE=extended python3 android/scripts/check-release-inputs.py
```

## Failure behavior

When sync fails, it fails loudly and opens or updates a GitHub Issue. CI never
overwrites an existing release tag. If the Bun asset is missing, corrupt, for
the wrong architecture, or has a checksum mismatch, the release fails closed.
