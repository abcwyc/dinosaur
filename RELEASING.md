# Releasing Dinosaur

Dinosaur ships signed in-app updates on macOS, Linux, and Windows. Releases live
in the **GitHub releases of
[`abcwyc/dinosaur`](https://github.com/abcwyc/dinosaur/releases)**. macOS uses
[Sparkle](https://sparkle-project.org); the native Linux and Windows updaters
read architecture-specific feeds and verify artifacts with the same EdDSA key.
One release workflow produces all platform artifacts and feeds.

Once set up, cutting a release is pushing a tag and publishing the draft it
opens:

```sh
git tag v<version> && git push origin v<version>
```

- Updater code: [`src/updater.rs`](src/updater.rs) — loads the embedded
  Sparkle.framework on macOS and owns the signed native flows on Linux and
  Windows. Available updates appear in the sidebar footer; **Check for
  Updates…** lives in the app menu, and **Automatic updates** lives in
  Settings → General.
- Feed URL + public key: [`resources/Info.plist`](resources/Info.plist)
  (`SUFeedURL`, `SUPublicEDKey`). The Linux and Windows feed URLs are the
  `FEED_URL` constants in [`src/updater.rs`](src/updater.rs) and
  [`src/updater/linux.rs`](src/updater/linux.rs).
- Framework embedding + pinned Sparkle version:
  [`scripts/bundle.sh`](scripts/bundle.sh) (bump `sparkle_version` and
  `sparkle_sha256` together; the distribution is cached under
  `.waku-cache/sparkle/`).
- Release automation: [`scripts/release.ts`](scripts/release.ts),
  [`scripts/appcast.ts`](scripts/appcast.ts) (which also names the release
  repository), [`scripts/changelog.ts`](scripts/changelog.ts).
- GitHub Actions: [`.github/workflows/release.yml`](.github/workflows/release.yml)
  builds Linux (x86_64, arm64), Windows (x86_64, arm64), and macOS archives on
  a `v*` tag — or on a manual **Run workflow**, which takes the version from
  `Cargo.toml` — and opens a draft GitHub release.

### Where everything is served

GitHub serves two kinds of URL, and the release relies on both:

- **Versioned assets** —
  `https://github.com/abcwyc/dinosaur/releases/download/v<version>/<file>`.
  Every appcast item points at its own release this way, so an entry stays
  valid after newer releases ship.
- **Live pointers** —
  `https://github.com/abcwyc/dinosaur/releases/latest/download/<file>`
  resolves to the newest *published*, non-prerelease release. The update feeds
  (`appcast.xml`, `appcast-<platform>-<arch>.xml`) and `latest-*.txt` are read
  through it, which is why every release re-uploads them.

---

## One-time setup

Local builds run on [Bun](https://bun.sh) and need
[`create-dmg`](https://github.com/create-dmg/create-dmg)
(`brew install bun create-dmg`).

### 1. Sparkle signing keys

Updates are signed with an ed25519 key; the public half ships in Info.plist as
`SUPublicEDKey`, and the private half signs every feed.

The key lives in the login keychain under the **`dinosaur`** account (not
Sparkle's default one), and the same private key is stored as the
`SPARKLE_PRIVATE_KEY` repository secret. Local runs of `bun run release` sign
from the keychain; CI signs from the secret.

On a fresh machine, import the key from the password-manager backup with the
Sparkle tools (they land in `.waku-cache/sparkle/<version>/bin` after any
macOS build, or download the release from
[sparkle-project/Sparkle](https://github.com/sparkle-project/Sparkle/releases)):

```sh
./bin/generate_keys --account dinosaur -f sparkle_private_key.txt  # import
./bin/generate_keys --account dinosaur -p   # must print SUPublicEDKey
```

To back the key up, `./bin/generate_keys --account dinosaur -x <file>` exports
it; move that file into the password manager and delete it.

> ⚠️ Lose the private key and existing installs can never update again. Keep
> the backup current.

### 2. Developer ID signing + notarization

Copy `.env.example` to `.env` and replace the signing and analytics
placeholders. Bun loads these values before Cargo compiles the release, so the
analytics endpoint and website ID are embedded in the executable. The script
notarizes with the `NOTARY` keychain profile by default. On a fresh machine:

```sh
cp .env.example .env
xcrun notarytool store-credentials NOTARY \
  --apple-id you@example.com --team-id YOUR_APPLE_TEAM_ID
```

Override the environment with `--signing-identity`, or change the notary
profile with `--notary-profile` / `WAKU_NOTARY_PROFILE`.

### 3. Repository secrets

The Release workflow reads these from the repository's **Settings → Secrets
and variables → Actions**:

| Secret | Purpose |
| --- | --- |
| `WAKU_ANALYTICS_ENDPOINT` | embedded in every desktop CI build |
| `WAKU_ANALYTICS_WEBSITE_ID` | embedded in every desktop CI build |
| `WAKU_SIGNING_IDENTITY` | Developer ID identity selector |
| `APPLE_CERTIFICATE` | base64-encoded Developer ID Application `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | password for that `.p12` |
| `APPLE_ID` | Apple ID used by `notarytool` |
| `APPLE_APP_SPECIFIC_PASSWORD` | app-specific password for that Apple ID |
| `APPLE_TEAM_ID` | Developer Team ID |
| `SPARKLE_PRIVATE_KEY` | EdDSA private key for every update feed |
| `WINDOWS_CERTIFICATE` | optional; base64-encoded Authenticode `.pfx` |
| `WINDOWS_CERTIFICATE_PASSWORD` | optional; password for that `.pfx` |

---

## Cutting a release

1. **Bump `version` in `Cargo.toml`** — the single source of truth.
   `CFBundleShortVersionString` is the version, and `CFBundleVersion` is
   derived from it (`major*1e6 + minor*1e3 + patch`, so `0.2.0` → `2000`),
   which keeps Sparkle's build-number comparison monotonic without a manual
   counter. Release stable versions only: `releases/latest` skips GitHub
   prereleases, and the feeds serve one stable channel.
2. **Write the release notes** — add a `## [<version>]` section at the top of
   [`CHANGELOG.md`](CHANGELOG.md).
3. **Start the Release workflow**, either way:
   - **Push a `v*` tag** — the tag must match the `version` in `Cargo.toml`, or
     the run fails before anything builds.
   - **Actions → Release → Run workflow** — no tag needed. The run releases
     whatever `Cargo.toml` says and drafts it as `v<version>`; that tag is
     created at the built commit when you publish the draft.
4. **Publish the draft release.** Until then `releases/latest` still points at
   the previous release, so no one is offered the update and no download link
   moves.

Test by keeping an older build around, launching it, and choosing
**Check for Updates…**.

### What the workflow builds

macOS CI runs `bun run release`, which builds and signs the app via
`scripts/bundle.sh release`, verifies the bundled JS REPL and computer-use
helper, builds the styled DMG, notarizes and staples DMG + app, zips the app
for Sparkle, attaches the changelog section as release notes, and writes the
signed `appcast.xml`:

- `Dinosaur-<version>.dmg`
- `Dinosaur-<version>.zip`
- `Dinosaur-<version>.md` — the release notes Sparkle shows
- `appcast.xml` (Sparkle-signed)

The macOS appcast lists only the release it ships with. Sparkle needs nothing
more: it offers the newest item, and far-behind installs jump straight to it.

Linux CI adds:

- `waku-<version>-x86_64-unknown-linux-gnu.tar.gz`
- `waku-<version>-aarch64-unknown-linux-gnu.tar.gz`
- `appcast-linux-x86_64.xml`, `appcast-linux-aarch64.xml`
- `latest-linux.txt` — the version `install.sh` resolves "latest" to

Windows CI adds:

- `Dinosaur-<version>-x86_64-Setup.exe`
- `Dinosaur-<version>-aarch64-Setup.exe`
- `waku-<version>-x86_64-pc-windows-msvc.zip` (portable)
- `waku-<version>-aarch64-pc-windows-msvc.zip` (portable)
- `appcast-windows-x86_64.xml`, `appcast-windows-aarch64.xml`
- `latest-windows.txt` — the version the download page resolves "latest" to

[`scripts/bundle-windows.ts`](scripts/bundle-windows.ts) builds both, driving
[`resources/windows/waku.iss`](resources/windows/waku.iss) through Inno Setup's
`ISCC`. The installer is **per-user** (`PrivilegesRequired=lowest`,
`%LOCALAPPDATA%\Programs\Dinosaur`) — no elevation, which is exactly what lets
the updater re-run it silently. The script signs the two executables and the
installer with Authenticode when `WINDOWS_CERTIFICATE` and
`WINDOWS_CERTIFICATE_PASSWORD` are set, and packages them unsigned otherwise,
so a fork without a certificate can still cut a release at the cost of a
SmartScreen warning.

**Never change `AppId` in `waku.iss`.** It is how Windows recognizes an
existing install; a new one turns every update into a second copy in
Add/Remove Programs.

#### The native Windows and Linux update feeds

Windows and Linux have no Sparkle, so [`src/updater.rs`](src/updater.rs) runs
the same contract itself: fetch the appcast, compare versions, download, and
verify the EdDSA signature. Windows hands the installer to Inno Setup with
`/SILENT`. Linux safely unpacks the tarball beside the managed user-local
prefix, then `waku-updater` swaps it after the app's normal quit saves and
rolls back if the replacement cannot open its main window. The Linux updater
only downloads archives under `github.com/abcwyc/dinosaur/releases/download/`.

- **One feed per architecture.** A Sparkle appcast cannot say which binary an
  item is for, and the client picks its feed at compile time.
- **Same key as macOS.** `build.rs` reads `SUPublicEDKey` out of
  `resources/Info.plist` and compiles it in, so the three platforms cannot
  drift onto different keys.
- [`scripts/appcast-windows.ts`](scripts/appcast-windows.ts) and
  [`scripts/appcast-linux.ts`](scripts/appcast-linux.ts) sign the feeds in the
  draft-release job — the only one holding all native artifacts. They sign
  with Node's Ed25519 over the same `SPARKLE_PRIVATE_KEY`, and refuse to run
  when the key does not derive `SUPublicEDKey` (signing with the wrong key
  ships a feed the app rejects).
- The step pulls the live feeds from the latest published release first and
  merges, so previously published releases keep their entries.

Both Linux jobs run on **Ubuntu 22.04**, and that choice is load-bearing: the
binaries link against the build machine's glibc, so the runner sets the oldest
distribution Dinosaur can start on (2.35 — Ubuntu 22.04, Debian 12, Fedora 36).
Moving those jobs to a newer runner silently drops support for everything
older.

Linux users install through
[`website/public/install.sh`](website/public/install.sh), served from the
repository at
`https://raw.githubusercontent.com/abcwyc/dinosaur/main/website/public/install.sh`
— see [docs/linux.md](docs/linux.md).

### Local builds

`bun run release` on a Mac builds, notarizes, and writes the same macOS
artifacts into `dist/` without publishing anything; upload them to a release by
hand if you ever need to.

| Flag / Env | Default | Purpose |
| --- | --- | --- |
| `--adhoc`, `--skip-notarize` | — | local test builds |
| `--skip-build` | — | reuse existing release binaries |
| `--build-number <n>` / `WAKU_BUILD_NUMBER` | derived | `CFBundleVersion` override |
| `WAKU_DOWNLOAD_URL_PREFIX` | `…/releases/download/v<version>/` | base URL in the appcast |
| `SPARKLE_BIN` | the `.waku-cache` copy | Sparkle tools directory |

---

## Notes

- **Two artifacts per release:** the notarized `.dmg` (what people download)
  and a `.zip` (what Sparkle installs). Only the zip appears in the appcast;
  point download buttons at the DMG.
- **No binary deltas.** Deltas need earlier archives next to the new one when
  the appcast is generated, and each release only has its own assets, so every
  macOS update downloads the full zip.
- **Debug builds never update themselves.** `Updater::init` returns `None`
  under `debug_assertions`, so the dev watcher's app can't offer to replace
  itself with a production Dinosaur. Set `WAKU_FORCE_UPDATER=1` to exercise the
  real Sparkle flow from a debug bundle anyway. A bare `cargo run` binary has
  no embedded framework and also degrades to no updater. For UI-only testing,
  start the watcher with `WAKU_PREVIEW_UPDATE=1`; the sidebar immediately
  shows an available update and clicking it changes to the spinner without
  installing anything. The preview flag fakes only that sidebar result;
  **Check for Updates…** still uses the embedded Sparkle framework and its
  real standard window.
- **Automatic and explicit checks have separate presentation.** Scheduled
  checks stay silent until the sidebar update button appears. Choosing
  **Check for Updates…** promotes an existing silent result into Sparkle's
  standard updater window, or shows its checking progress while an automatic
  check finishes. With no automatic session active, it starts Sparkle's
  standard user-initiated check directly.
- **First-run consent:** Sparkle shows its one-time "check automatically?"
  prompt on the second launch. The Settings → General toggle reads and writes
  the same persisted value.
- **Dinosaur isn't sandboxed**, so Sparkle's XPC services are unnecessary;
  `bundle.sh` strips them (plus headers/modules) from the embedded framework
  and re-signs the rest with the app's identity — hardened-runtime library
  validation requires the identities to match.
- **Never delete a published release.** Its assets are what older appcast
  entries and download links point at.
- **Asset names are a contract** — `Dinosaur-<v>.dmg`, `Dinosaur-<v>.zip`,
  `appcast*.xml`, `latest-*.txt`, `waku-<v>-<target>.tar.gz`, and
  `Dinosaur-<v>-<arch>-Setup.exe` are what the feeds, `install.sh`, and the
  website construct. `src/updater.rs` is the per-platform seam, and
  everything mac-specific in the release pipeline lives behind the Darwin
  guard in `scripts/release.ts` plus `scripts/bundle.sh`.
