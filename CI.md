# Client CI/CD

This repo now carries `pulstart/protocol` as a Git submodule under `./protocol`. That matches
the Rust path dependency in [Cargo.toml](./Cargo.toml):

```toml
st-protocol = { path = "protocol" }
```

## Workflows

- `.github/workflows/ci.yml`
  - Runs on every pull request and on pushes to `main`
  - Checks out the repo with submodules
  - Builds release binaries on Linux, macOS x64, macOS arm64, and Windows x64
  - Uploads packaged artifacts to the workflow run
- `.github/workflows/release.yml`
  - Runs on `v*` tags and on manual dispatch
  - Builds the same platform matrix
  - Publishes the packaged artifacts to the GitHub release when the run is tag-based

## Cloning

Clone with submodules:

```bash
git clone --recurse-submodules git@github.com:pulstart/client.git
```

If you already cloned the repo without submodules:

```bash
git submodule update --init --recursive
```

## Private Submodule Access

Both GitHub Actions workflows use `actions/checkout` with `submodules: recursive`.

If `pulstart/protocol` is private, add this repository secret in `pulstart/client`:

- `PROTOCOL_CHECKOUT_TOKEN`
  - Personal access token or fine-grained token with read access to `pulstart/protocol`

Without that secret, the workflows fall back to the standard `GITHUB_TOKEN`, which only works when
the protocol repo is public or otherwise accessible to the workflow token.

## macOS Release Signing

macOS release archives now build unsigned by default. If you want a proper
Developer ID signed and notarized download, provide the Apple secrets below and
the packaging script will use them automatically.

Optional signing secrets:

- `MACOS_CERTIFICATE_P12_BASE64`
  - Base64-encoded Developer ID Application certificate export (`.p12`)
- `MACOS_CERTIFICATE_PASSWORD`
  - Password for that `.p12`
- `MACOS_CODESIGN_IDENTITY`
  - Full signing identity, for example `Developer ID Application: Example, Inc. (TEAMID)`

Optional notarization secrets, choose one set:

- App Store Connect API key:
  - `MACOS_NOTARY_KEY_ID`
  - `MACOS_NOTARY_ISSUER`
  - `MACOS_NOTARY_API_KEY_BASE64`
- Apple ID / app-specific password:
  - `MACOS_NOTARY_APPLE_ID`
  - `MACOS_NOTARY_APP_PASSWORD`
  - `MACOS_TEAM_ID`

Without those secrets, both CI and tagged releases still produce macOS `.app`
archives, but Gatekeeper may require a manual allow/open step on the target Mac.

## Release Artifacts

Tagging `v0.1.0` produces these files:

- `st-client-v0.1.0-linux-x64.tar.gz`
- `st-client-v0.1.0-macos-x64.zip`
- `st-client-v0.1.0-macos-arm64.zip`
- `st-client-v0.1.0-windows-x64.zip`

The Windows artifact already includes the staged runtime DLLs from the existing
`build-windows-msvc.ps1` flow.

The Linux and macOS archives currently package the built client binary or `.app` bundle, but
they do not bundle every runtime multimedia dependency yet. The package README files call that
out explicitly:

- Linux expects the target machine to provide FFmpeg, Opus, and the usual desktop OpenGL/audio stack
- macOS release archives are unsigned unless the Apple secrets above are configured
- macOS still expects the target machine to provide FFmpeg and Opus runtime libraries for now

## Releasing

1. Push a version tag from the client repo, for example `v0.1.0`
2. GitHub Actions runs `.github/workflows/release.yml`
3. The workflow builds all platforms, uploads temporary build artifacts, and publishes the final
   archives to the GitHub release for that tag
