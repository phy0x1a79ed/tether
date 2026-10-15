#!/usr/bin/env bash
# Cut a release: gate what build-clients.sh staged, bake the checksums into the
# launchers, and publish every artifact to a GitHub release.
#
#   ./build-clients.sh            # stage every client, the relay and the operator
#   ./release.sh v0.1.0           # gate, render, tag, publish
#
# # What a release is
#
# One tag, one set of artifacts, one place the owner's launcher fetches from.
# The clients, the relay and the operator are built by build-clients.sh into the
# stage; this script checks that set is whole and untampered, writes the sha256
# of each client into the two launchers so a second host can serve the bytes
# without being able to change them, and uploads the lot to the release named by
# the tag. From then on the launcher tries that release first and the relay only
# as a fallback.
#
# # Why a draft first
#
# A release is public the moment it exists, and an upload that fails partway
# leaves a release that lists some assets and not others — which an owner's
# launcher would fetch from. So the release is created as a draft, every asset
# is uploaded while nobody can see it, and only a complete upload is published.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/artifacts.sh"

TAG="${1:-}"
[ -n "$TAG" ] || { echo "usage: $0 <tag>   e.g. $0 v0.1.0" >&2; exit 1; }

step() { echo; echo "== $*"; }

# The clients the launchers may fetch, each as "<sha256>  <name>". The launchers
# verify what they download against exactly these, so they are the clients and
# only the clients — the relay and operator are fetched by a node, not a
# launcher, and are checked against SHA256SUMS instead.
client_sums() {
    local name
    for name in $(tether_names); do
        if tether_is_client "$name"; then
            printf '%s  %s\n' "$(sha256_of "$TETHER_DIST/$name")" "$name"
        fi
    done
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

# The whole set, in the `<sha256>  <name>` format `sha256sum -c` reads back.
sha256_sum_all() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$@"
    else
        shasum -a 256 "$@"
    fi
}

# Render one launcher: copy the source up to and including the start marker,
# write the baked block, then copy from the end marker onward. Building the file
# this way rather than editing in place keeps the multi-line checksum block out
# of sed's way.
render_launcher() {
    local src="$1" out="$2" block="$3"
    awk '/^# --- baked by release.sh/ { print; exit } { print }' "$src" > "$out"
    printf '%s\n' "$block" >> "$out"
    awk '/^# --- end baked ---/ { f = 1 } f { print }' "$src" >> "$out"
}

step "the staged set"
[ -f "$TETHER_DIST/BUILD_KIND" ] || { echo "nothing staged in $TETHER_DIST; run ./build-clients.sh first" >&2; exit 1; }
KIND="$(cat "$TETHER_DIST/BUILD_KIND")"
[ "$KIND" = cross ] || { echo "the stage is '$KIND', not a full cross build; release only a set build-clients.sh built whole" >&2; exit 1; }
STAMP="$(cat "$TETHER_DIST/BUILD_STAMP" 2>/dev/null || echo unknown)"
echo "   $KIND build, stamped $STAMP"

step "a clean tree"
# A release names a commit. A dirty tree means the artifacts were built from
# something no commit records, so the tag would point at the wrong bytes.
[ -z "$(git -C "$HERE" status --porcelain)" ] || { echo "the tree is dirty; commit or stash before releasing" >&2; exit 1; }

step "the gate"
# The same gate ship used to run: a real binary of the right format, big enough
# to be a build, with no consent bypass compiled into any client.
# shellcheck disable=SC2046
tether_gate $(tether_names) || exit 1

step "the checksums"
SUMS_TEXT="$(client_sums)"
echo "$SUMS_TEXT"

step "render the launchers"
SH_BLOCK="$(cat <<EOF
# Filled by release.sh at publish time; the source carries empty values and
# refuses to run. See the top of this file for why the checksum is here.
TETHER_REPO="phy0x1a79ed/tether"
TETHER_TAG="$TAG"
TETHER_SUMS="
$SUMS_TEXT
"
EOF
)"
PS_BLOCK="$(cat <<EOF
# Filled by release.sh at publish time; the source carries empty values and
# refuses to run. See the top of this file for why the checksum is here.
\$TetherRepo = 'phy0x1a79ed/tether'
\$TetherTag = '$TAG'
\$TetherSums = @'
$SUMS_TEXT
'@
EOF
)"
render_launcher "$HERE/launcher.sh" "$TETHER_DIST/tether" "$SH_BLOCK"
render_launcher "$HERE/launcher.ps1" "$TETHER_DIST/tether.ps1" "$PS_BLOCK"
chmod 755 "$TETHER_DIST/tether"
echo "   rendered tether and tether.ps1 for $TAG"

step "SHA256SUMS"
# Every published asset, so a node or a curious owner can check the whole set.
ASSETS=""
for name in $(tether_names) tether tether.ps1; do
    ASSETS="$ASSETS $name"
done
( cd "$TETHER_DIST" && sha256_sum_all $ASSETS > SHA256SUMS )
cat "$TETHER_DIST/SHA256SUMS"

step "tag $TAG"
if git -C "$HERE" rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
    echo "   tag $TAG already exists locally; reusing it"
else
    git -C "$HERE" tag -a "$TAG" -m "tether $TAG"
fi
git -C "$HERE" push origin "$TAG"

step "the draft release"
PATHS=""
for name in $ASSETS SHA256SUMS; do
    PATHS="$PATHS $TETHER_DIST/$name"
done
# --draft, so the release is invisible until every asset has landed.
# shellcheck disable=SC2086
gh release create "$TAG" --draft --title "tether $TAG" \
    --notes "tether $TAG — built $STAMP. Clients, the relay and the operator, with SHA256SUMS." \
    $PATHS

step "publish"
gh release edit "$TAG" --draft=false
echo
echo "released $TAG"
gh release view "$TAG" --json assets --jq '.assets[].name' | sed 's/^/   /'
