# CI and releases

## CI

Pushes to `main` and pull requests only validate. Nothing is tagged or published.

| Job     | Runs                                                 | When           |
| ------- | ---------------------------------------------------- | -------------- |
| Linux   | fmt, clippy, tests against PostgreSQL and MariaDB    | every push     |
| Windows | build, tests                                         | every push     |
| macOS   | build, tests                                         | every push     |

Docs-only changes skip CI.

## Releasing

Start a release by hand, from the Actions tab or with:

```sh
gh workflow run release.yml -f version=0.2.0
```

The workflow then:

1. sets the workspace version, commits `Release v0.2.0` to `main` and tags `v0.2.0`;
2. builds macOS (Apple Silicon), Windows (x64) and Linux (x64);
3. publishes a GitHub Release with the installers, `SHA256SUMS` and generated notes.

Installed apps read this repo's latest release to notice new versions.

## Known limits

- **Not code-signed:** macOS and Windows warn on first launch, see [Install](INSTALL.md).
  Signing needs an Apple Developer ID and a Windows signing certificate.
- **Apple Silicon only** on macOS; there is no Intel build.
- **No license notices yet:** the installers bundle open-source crates whose licenses
  require attribution. Generate third-party notices before distributing widely.
