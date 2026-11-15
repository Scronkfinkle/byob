# Releasing

Push a tag `vX.Y.Z` matching `version` in `Cargo.toml`. `.github/workflows/release.yml` builds
every platform and publishes a GitHub release with, per target, `byob-…` (browser UI and the
HTML app renderer), `byob-slim-…`, and on macOS `byob-<version>-<target>.dmg`, each with a
`.sha256`. Running the workflow by hand (`gh workflow run release.yml --ref BRANCH`) builds the
same files as run artifacts and publishes nothing.

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
