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

1. sets the workspace version, regenerates `THIRD-PARTY-NOTICES.txt`, commits
   `Release v0.2.0` to `main` and tags `v0.2.0`;
2. builds macOS (Apple Silicon), Windows (x64) and Linux (x64);
3. publishes a GitHub Release with the installers, `SHA256SUMS` and generated notes,
   which start with a note on installing on macOS.

The notices step stops the release if a new dependency uses a license not listed in
`about.toml`. Review that license before adding it there.

Installed apps read this repo's latest release to notice new versions.

After publishing, the workflow installs the new release with `install.sh` and
`install.ps1` on all three platforms to confirm the installers still work.

## Known limits

- **Not code-signed:** browser downloads warn on first launch. The command-line install
  avoids that, see [Install](INSTALL.md).
- **Apple Silicon only** on macOS; there is no Intel build.

## Checking the window on Linux and Windows

`gh workflow run gui-smoke.yml` opens the real app on both and saves a screenshot as a
build artifact. It isn't part of CI: run it after UI or dependency changes.
