# Releasing

Push a tag `vX.Y.Z` matching `version` in `Cargo.toml`. `.github/workflows/release.yml` builds
every platform and publishes a GitHub release with, per target, `byob-…` (browser UI and the
HTML app renderer), `byob-slim-…`, and on macOS `byob-<version>-<target>.dmg`, each with a
`.sha256`. Running the workflow by hand (`gh workflow run release.yml --ref BRANCH`) builds the
same files as run artifacts and publishes nothing.

Checklist:
1. Bump `version` in `Cargo.toml` (the workspace and its `byob-*` dependencies),
   `crates/artifact-wire/Cargo.toml`, `host/artifact-host/Cargo.toml` and `flake.nix`; update
   both lockfiles; bump `v=` in the README's macOS `curl` line and every version number
   in `site/index.html` (the byob.chat download links; `cargo xtask ci` fails until it matches).
2. `cargo xtask ci`, commit, push, and wait for CI.
3. Tag and push the tag; when the release is up, set its notes (`gh release edit vX.Y.Z
   --notes-file …`; say the macOS builds are unsigned while they are).
4. Publish to crates.io by hand: a release made by the workflow doesn't trigger
   `publish.yml` (GitHub doesn't start workflows from a workflow's own events), so run
   `gh workflow run publish.yml --ref vX.Y.Z -f dry_run=false`.

## The macOS app

`scripts/macos-bundle.sh` builds `byob.app` (bundle id `io.github.scronkfinkle.byob`) and the
dmg:

```text
byob.app/Contents/
  Info.plist                        version from Cargo.toml, LSMinimumSystemVersion from the binaries
  PkgInfo
  MacOS/byob
  MacOS/byob-artifact-host          byob finds the helper next to itself
  Resources/LICENSE-MIT, LICENSE-APACHE, README.md, NOTICE-byob-artifact-host
```

The dmg also holds a link to `/Applications`. There is no icon yet.

Started from the Finder, byob moves from `/` to the home folder and takes `PATH` from the
login shell (`$SHELL -l -i -c env`, plus Homebrew/Nix folders that exist), so `run_command`
finds the same tools as in a terminal (`apps/byob/src/launch_env.rs`). Its log goes nowhere
then; start `byob.app/Contents/MacOS/byob` from a terminal to see it.

## Signing and notarization

Off until the repository has these secrets (Settings → Secrets and variables → Actions);
until then the app is signed ad hoc, and users allow it once (see the README).

| Secret | What |
| --- | --- |
| `MACOS_CERTIFICATE` | base64 of a `.p12` with a "Developer ID Application" certificate and its private key (`base64 -i cert.p12 \| pbcopy`) |
| `MACOS_CERTIFICATE_PASSWORD` | the `.p12`'s password |
| `MACOS_NOTARY_KEY` | an App Store Connect API key: the text of the `AuthKey_XXXXXXXXXX.p8` file (Users and Access → Integrations → App Store Connect API, role Developer) |
| `MACOS_NOTARY_KEY_ID` | that key's ID |
| `MACOS_NOTARY_ISSUER_ID` | the issuer ID shown above the keys |

With `MACOS_CERTIFICATE` set, the workflow imports it into a temporary keychain, the script
signs (hardened runtime, secure timestamp) the helper with
`scripts/macos-artifact-host.entitlements` (`com.apple.security.cs.allow-jit`, for
SpiderMonkey's JIT), then byob, then the app, then the dmg; `xcrun notarytool submit --wait`
notarizes the dmg and `xcrun stapler staple` attaches the ticket.

## byob.chat

`site/` is the website; the README's pictures (`docs/media`) are served from it as `media/`.
The fonts are self-hosted (OFL, licences in `site/fonts/`), so the page loads nothing from
other sites. Publish it with:

```sh
rsync -avz --delete --chmod=D755,F644 site/ docs/media HOST:/var/www/byob/
```

Preview: `cp -r site /tmp/s && cp -r docs/media /tmp/s/media`, then open `/tmp/s/index.html`.
