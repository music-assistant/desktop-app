# Homebrew Tap Setup for Music Assistant Desktop App

This directory contains templates for setting up a Homebrew tap for the Music Assistant desktop app.

## Setting up the Homebrew Tap

### 1. Create the Tap Repository

Create a new repository at `music-assistant/homebrew-tap` with the following structure:

```
homebrew-tap/
├── Formula/
│   └── music-assistant.rb
├── .github/
│   └── workflows/
│       └── update-formula.yml
└── README.md
```

### 2. Create the Formula

Use the template in `music-assistant.rb.template` as a starting point. The formula will be automatically updated when new releases are published.

### 3. Set up the Update Workflow

Create `.github/workflows/update-formula.yml` in the tap repository:

```yaml
name: Update Formula

on:
  repository_dispatch:
    types: [update-formula]
  workflow_dispatch:
    inputs:
      version:
        description: "Version to update to (without v prefix)"
        required: true

jobs:
  update:
    runs-on: ubuntu-latest
    permissions:
      contents: write
    steps:
      - uses: actions/checkout@v4

      - name: Get version
        id: version
        run: |
          if [ "${{ github.event_name }}" = "repository_dispatch" ]; then
            VERSION="${{ github.event.client_payload.version }}"
          else
            VERSION="${{ github.event.inputs.version }}"
          fi
          # Remove 'v' prefix if present
          VERSION="${VERSION#v}"
          echo "version=$VERSION" >> $GITHUB_OUTPUT

      - name: Download release assets and compute checksums
        run: |
          VERSION="${{ steps.version.outputs.version }}"

          # Download macOS ARM DMG
          curl -L -o macos-arm.dmg "https://github.com/music-assistant/desktop-app/releases/download/v${VERSION}/Music.Assistant_${VERSION}_aarch64.dmg"
          SHA_MACOS_ARM=$(shasum -a 256 macos-arm.dmg | cut -d ' ' -f 1)

          # Download macOS Intel DMG
          curl -L -o macos-intel.dmg "https://github.com/music-assistant/desktop-app/releases/download/v${VERSION}/Music.Assistant_${VERSION}_x64.dmg"
          SHA_MACOS_INTEL=$(shasum -a 256 macos-intel.dmg | cut -d ' ' -f 1)

          echo "SHA_MACOS_ARM=$SHA_MACOS_ARM" >> $GITHUB_ENV
          echo "SHA_MACOS_INTEL=$SHA_MACOS_INTEL" >> $GITHUB_ENV

      - name: Update formula
        run: |
          VERSION="${{ steps.version.outputs.version }}"

          cat > Formula/music-assistant.rb << 'EOF'
          class MusicAssistant < Formula
            desc "Desktop companion app for Music Assistant"
            homepage "https://music-assistant.io"
            version "${{ steps.version.outputs.version }}"
            license "Apache-2.0"

            on_macos do
              on_arm do
                url "https://github.com/music-assistant/desktop-app/releases/download/v${{ steps.version.outputs.version }}/Music.Assistant_${{ steps.version.outputs.version }}_aarch64.dmg"
                sha256 "${{ env.SHA_MACOS_ARM }}"
              end
              on_intel do
                url "https://github.com/music-assistant/desktop-app/releases/download/v${{ steps.version.outputs.version }}/Music.Assistant_${{ steps.version.outputs.version }}_x64.dmg"
                sha256 "${{ env.SHA_MACOS_INTEL }}"
              end
            end

            def install
              prefix.install Dir["*.app"].first
            end

            def caveats
              <<~EOS
                Music Assistant Desktop App has been installed.
                To run the app, open it from your Applications folder.
              EOS
            end

            test do
              assert_predicate prefix/"Music Assistant.app", :exist?
            end
          end
          EOF

      - name: Commit and push
        run: |
          git config user.name "github-actions[bot]"
          git config user.email "github-actions[bot]@users.noreply.github.com"
          git add Formula/music-assistant.rb
          git commit -m "Update music-assistant to v${{ steps.version.outputs.version }}"
          git push
```

### 4. Configure Secrets

In the **desktop-app** repository, add the following secret:

- `HOMEBREW_TAP_TOKEN`: A Personal Access Token (PAT) with `repo` scope that has access to the `homebrew-tap` repository

### 5. Usage

Once set up, users can install the app via:

```bash
brew tap music-assistant/tap
brew install music-assistant
```

## How it Works

1. When a new release is published on the desktop-app repository, the release workflow triggers
2. After all binaries are built and uploaded, it sends a `repository_dispatch` event to the homebrew-tap repository
3. The homebrew-tap workflow downloads the new release assets, computes SHA256 checksums, and updates the formula
4. Users running `brew upgrade` will get the new version
