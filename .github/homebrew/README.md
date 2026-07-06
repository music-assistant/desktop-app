# Homebrew Tap Setup for Music Assistant Desktop App

This directory documents the Homebrew tap integration for the Music Assistant desktop app.

The public Homebrew cask token is `music-assistant`. The older `companion` token was internal-facing and should be kept only as a Homebrew rename in `cask_renames.json` for existing users.

## Tap repository structure

The tap repository is `music-assistant/homebrew-tap` and should contain:

```
homebrew-tap/
├── Casks/
│   └── music-assistant.rb
├── cask_renames.json
├── .github/
│   └── workflows/
│       └── update-cask.yml
└── README.md
```

## Cask template

Use `music-assistant.rb.template` as the shape for `Casks/music-assistant.rb`. The cask is macOS-only and installs the signed macOS `.app.tar.gz` release assets:

- `Music.Assistant_<version>_aarch64.app.tar.gz`
- `Music.Assistant_<version>_x64.app.tar.gz`

## Update workflow

The desktop-app `Build Release` workflow sends a `repository_dispatch` event to the tap repository after release assets are uploaded:

```json
{
  "event_type": "update-cask",
  "client_payload": {
    "version": "<release-version>"
  }
}
```

The tap workflow should listen for `repository_dispatch` type `update-cask`, download the two macOS `.app.tar.gz` assets, compute SHA256 checksums, rewrite `Casks/music-assistant.rb`, and commit the result.

For compatibility while rolling out the rename, the tap workflow may also listen for the legacy `update-formula` event type.

## Configure secrets

In the **desktop-app** repository, add this secret:

- `HOMEBREW_TAP_TOKEN`: a Personal Access Token (PAT) with `repo` scope that can dispatch workflows in `music-assistant/homebrew-tap`.

## Usage

Users can install the app via:

```bash
brew tap music-assistant/tap
brew install --cask music-assistant/tap/music-assistant
```

## How it works

1. A maintainer manually triggers the `Build Release` workflow with a tag like `0.5.0`.
2. The workflow builds and uploads release assets.
3. The workflow sends `repository_dispatch` event `update-cask` to `music-assistant/homebrew-tap`.
4. The tap workflow downloads the release assets, computes checksums, and updates `Casks/music-assistant.rb`.
5. Users running `brew update && brew upgrade` receive the new version.
