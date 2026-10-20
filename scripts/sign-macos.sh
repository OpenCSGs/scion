#!/usr/bin/env bash
# Sign macOS binaries with the OpenCSG Developer ID (hardened runtime, secure timestamp) and check
# the result. macOS lets a signed command-line tool ask for local network access; an ad-hoc signed
# one is refused connections to other machines on the LAN.
#
#   APPLE_SIGNING_IDENTITY="Developer ID Application: …" scripts/sign-macos.sh BINARY…
#
# Each binary gets the identifier com.opencsg.<name>, so its permissions survive new versions.
set -euo pipefail

identity="${APPLE_SIGNING_IDENTITY:?Set APPLE_SIGNING_IDENTITY}"
team_id="${APPLE_TEAM_ID:-3S93B6Z434}"
[[ $# -gt 0 ]] || { echo "usage: $0 BINARY..." >&2; exit 2; }

for bin in "$@"; do
  [[ -f "${bin}" ]] || { echo "not a file: ${bin}" >&2; exit 1; }
  codesign --force --sign "${identity}" --options runtime --timestamp \
    --identifier "com.opencsg.$(basename "${bin}")" "${bin}"
  codesign --verify --strict --verbose=2 "${bin}"
  details="$(codesign -dv --verbose=4 "${bin}" 2>&1)"
  grep -q "Authority=Developer ID Application" <<<"${details}" || { echo "${bin}: not signed with a Developer ID" >&2; exit 1; }
  grep -q "TeamIdentifier=${team_id}" <<<"${details}" || { echo "${bin}: not signed by team ${team_id}" >&2; exit 1; }
  echo "signed ${bin}"
done
