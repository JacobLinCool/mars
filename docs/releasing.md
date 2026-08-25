# Releasing MARS

MARS publishes a native Apple Silicon (`arm64`) macOS installer package from
version tags. Every executable is signed with Developer ID and hardened
runtime. The signed driver bundle is submitted in a ZIP archive, stapled, and
validated before it is added to the runtime payload. The resulting installer
is then separately submitted with `notarytool`, stapled, checked with
Gatekeeper, and uploaded with checksums and both notarization logs.

The release build must contain the complete Official Driver Binary License in
both `mars.driver/Contents/Resources/MARS-OFFICIAL-DRIVER-BINARY-LICENSE.txt`
and the driver executable's `__TEXT,__mars_license` Mach-O section. The driver
bundle is signed only after both copies are present, so the signature covers
the license shipped with the official binary. The driver is notarized and its
ticket is stapled only after that final signature; packaging uses the stapled
bundle without rebuilding or re-signing it.

## One-time GitHub configuration

Configure these repository Actions secrets:

- `DEVELOPER_ID_APPLICATION_CERT_BASE64`: base64-encoded `.p12` containing the
  Developer ID Application certificate and private key
- `DEVELOPER_ID_APPLICATION_CERT_PASSWORD`: password of that `.p12`
- `DEVELOPER_ID_INSTALLER_CERT_BASE64`: base64-encoded `.p12` containing the
  Developer ID Installer certificate and private key
- `DEVELOPER_ID_INSTALLER_CERT_PASSWORD`: password of that `.p12`
- `APP_STORE_CONNECT_API_KEY_BASE64`: base64-encoded App Store Connect `.p8`
  API key with notarization access
- `APP_STORE_CONNECT_KEY_ID`: API key ID
- `APP_STORE_CONNECT_ISSUER_ID`: API issuer ID

The release workflow has no unsigned or password-based fallback. A missing or
invalid secret fails the release before an artifact is published.

Create two GitHub Actions environments in the repository:

- `npm`
- `pypi`

They do not contain registry tokens. Registry publication uses GitHub OIDC.

### PyPI trusted publisher

Create a pending GitHub publisher for `mars-sdk` with these exact values:

| Field | Value |
| --- | --- |
| PyPI project name | `mars-sdk` |
| GitHub owner | `JacobLinCool` |
| Repository name | `mars` |
| Workflow filename | `release.yml` |
| Environment name | `pypi` |

PyPI creates the project on the first successful trusted publication. Do not
add a PyPI API token to GitHub.

### npm trusted publisher

The public `mars-audio` npm scope must exist and the first version of
`@mars-audio/sdk` must be published interactively before npm exposes its
trusted-publisher settings:

```bash
cd sdks/typescript
pnpm install --frozen-lockfile
pnpm run build:native
pnpm test
pnpm run test:native
pnpm login --registry https://registry.npmjs.org/
pnpm publish --access public
```

Then configure the package's GitHub Actions trusted publisher:

| Field | Value |
| --- | --- |
| Organization or user | `JacobLinCool` |
| Repository | `mars` |
| Workflow filename | `release.yml` |
| Environment name | `npm` |
| Allowed action | `npm publish` |

The workflow uses pnpm for dependency management, build, test, and packaging.
Its final registry upload invokes a pinned npm CLI because npm's OIDC exchange
is implemented by npm CLI 11.5.1 and newer. After the trusted publisher works,
disable token-based publishing for the package.

## Publish a release

1. Update the runtime, SDK, native-binding, TypeScript, and Python package
   manifests to the same release version and merge the verified change into
   `main`.
2. Create and push an annotated matching tag:

   ```bash
   git tag -a v0.1.0 -m "MARS v0.1.0"
   git push origin v0.1.0
   ```

The tag must be `vX.Y.Z`, match the daemon crate version exactly, and point to
a commit contained in `main`. The workflow verifies that all shipping package
versions match before it publishes:

- `mars-X.Y.Z-arm64.pkg`
- per-package checksums and installer notarization result/log
- driver notarization result/log
- `SHA256SUMS`
- `@mars-audio/sdk@X.Y.Z` on npm
- `mars-sdk==X.Y.Z` as a CPython 3.11+ `abi3` arm64 macOS wheel on PyPI

Users can double-click the package or install it from Terminal:

```bash
sudo installer -pkg mars-0.1.0-arm64.pkg -target /
mars doctor
```

Installation requires an active logged-in macOS user because MARS installs a
per-user LaunchAgent while placing the CLI, daemon, and HAL driver in their
system locations.

Apple references:

- [Notarizing macOS software before distribution](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)
- [Customizing the notarization workflow](https://developer.apple.com/documentation/security/customizing-the-notarization-workflow)
