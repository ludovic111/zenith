#!/usr/bin/env bash
# Release workflow, macOS runners: import the Developer ID certificate into a throwaway keychain
# and write the App Store Connect API key, then hand scripts/mac/package.sh the identity and the
# key through $GITHUB_ENV. The six secrets are the lsuite ones (set them with
# ../lsuite/scripts/set-apple-secrets.sh; the API key is "zenith notarization"). With none set
# the app is ad hoc signed and not notarized; with only some, the release stops.
set -euo pipefail
names=(APPLE_CERTIFICATE_P12_BASE64 APPLE_CERTIFICATE_PASSWORD APPLE_SIGNING_IDENTITY
  APPLE_API_KEY_P8_BASE64 APPLE_API_KEY_ID APPLE_API_ISSUER)
set_count=0
for name in "${names[@]}"; do
  test -n "${!name:-}" && set_count=$((set_count + 1))
done
if [ "$set_count" -eq 0 ]; then
  echo '::warning::No Apple signing secrets: the macOS app is ad hoc signed and not notarized.'
  exit 0
fi
if [ "$set_count" -ne "${#names[@]}" ]; then
  for name in "${names[@]}"; do test -n "${!name:-}" || echo "Missing secret $name" >&2; done
  echo 'Set all six Apple secrets or none.' >&2
  exit 1
fi
keychain="$RUNNER_TEMP/signing.keychain-db"
password=$(uuidgen)
security create-keychain -p "$password" "$keychain"
security set-keychain-settings -lut 21600 "$keychain"
security unlock-keychain -p "$password" "$keychain"
umask 077
printf '%s' "$APPLE_CERTIFICATE_P12_BASE64" | base64 --decode > "$RUNNER_TEMP/developer-id.p12"
security import "$RUNNER_TEMP/developer-id.p12" -k "$keychain" -P "$APPLE_CERTIFICATE_PASSWORD" \
  -f pkcs12 -T /usr/bin/codesign
rm -f "$RUNNER_TEMP/developer-id.p12"
security set-key-partition-list -S apple-tool:,apple: -s -k "$password" "$keychain" > /dev/null
existing=$(security list-keychains -d user | sed -e 's/^ *"//' -e 's/"$//')
# shellcheck disable=SC2086
security list-keychains -d user -s "$keychain" $existing
if ! security find-identity -v -p codesigning "$keychain" | grep -qF "$APPLE_SIGNING_IDENTITY"; then
  echo "The certificate does not hold the identity \"$APPLE_SIGNING_IDENTITY\"." >&2
  exit 1
fi
printf '%s' "$APPLE_API_KEY_P8_BASE64" | base64 --decode > "$RUNNER_TEMP/notary.p8"
{
  echo "APPLE_SIGNING_IDENTITY=$APPLE_SIGNING_IDENTITY"
  echo "APPLE_API_KEY_PATH=$RUNNER_TEMP/notary.p8"
  echo "APPLE_API_KEY_ID=$APPLE_API_KEY_ID"
  echo "APPLE_API_ISSUER=$APPLE_API_ISSUER"
} >> "$GITHUB_ENV"
echo "Developer ID signing and notarization are ready."
