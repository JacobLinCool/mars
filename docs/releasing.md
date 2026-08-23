# Releasing MARS

MARS publishes a native Apple Silicon (`arm64`) macOS installer package from
version tags. Every executable is signed with Developer ID and hardened
runtime, and every installer is submitted with `notarytool`, stapled, checked
with Gatekeeper, and uploaded with checksums and notarization logs.

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

## Publish a release

1. Update `crates/mars-daemon/Cargo.toml` to the release version and merge the
   verified change into `main`.
2. Create and push an annotated matching tag:

   ```bash
   git tag -a v0.1.0 -m "MARS v0.1.0"
   git push origin v0.1.0
   ```

The tag must be `vX.Y.Z`, match the daemon crate version exactly, and point to
a commit contained in `main`. The workflow publishes:

- `mars-X.Y.Z-arm64.pkg`
- per-package checksums and notarization logs
- `SHA256SUMS`

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
