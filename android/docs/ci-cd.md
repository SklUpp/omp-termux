# CI/CD: Android ARM64 / Termux

Android release automation uses official stable Bun. No Bun source compilation exists in CI.

## Workflows

| Workflow | Trigger | Purpose |
|---|---|---|
| `sync-upstream.yml` | Scheduled/manual | Track official upstream SHA and dispatch release-tree. |
| `port-patches.yml` | Sync completion/manual | Apply patches, publish `mod`, create tag, dispatch release. |
| `ci.yml` | Push/PR/manual/reusable | Run repository and Android overlay checks. |
| `android-release.yml` | Termux tag/manual | Build addon and JS bundle, verify, package, publish release. |

`android/versions.env` is source of truth for Bun, NDK, Android API, and Rust values. OMP version comes from `packages/coding-agent/package.json`.

```sh
cat android/versions.env
python3 -c 'import json; print(json.load(open("packages/coding-agent/package.json"))["version"])'
```

Manifest fields:

- `BUN_VERSION` — shipped stable Bun version.
- `BUN_BOOTSTRAP_VERSION` — CI-only Bun used by JS/native jobs.
- `BUN_ARCHIVE_NAME` — official Bun Android ARM64 release asset.
- `BUN_SHA256` — exact asset checksum.
- `NDK_VERSION`, `ANDROID_API`, `RUST_TOOLCHAIN` — Android build inputs.

## Release flow

1. `sync-upstream.yml` tracks `can1357/oh-my-pi` `main`.
2. `port-patches.yml` applies the sequential raw Git patch queue.
3. Patch queue, release inputs, and frozen lockfile are validated.
4. `release-tree` publishes `mod`, creates the immutable tag, and dispatches `android-release.yml`.
5. `android-release.yml` runs repository checks, Android overlay checks, native addon build, and JS bundle build.
6. `release-publish` downloads official stable Bun:
```text
https://github.com/oven-sh/bun/releases/download/bun-v${BUN_VERSION}/${BUN_ARCHIVE_NAME}
```

The job verifies `BUN_SHA256`, extracts executable `bun`, checks aarch64 ELF, checks runtime version, and packages it as `./bun`. It has no source build, npm Bun, Termux `pkg bun`, or silent fallback.

Release output:

- `omp-termux.tar.gz`
- `omp-termux.tar.gz.sha256`
- `pi_natives.android-arm64.node`
- `pi_natives.android-arm64.node.sha256`

## Local checks

```sh
python3 android/scripts/verify-overlay.py
python3 android/scripts/check-release-inputs.py
```

## Failure behavior

Sync fails loudly and opens/updates a GitHub Issue. Existing release tags are not overwritten. Release fails closed on missing, corrupt, wrong-architecture, or checksum-mismatched Bun asset.
