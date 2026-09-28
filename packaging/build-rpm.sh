#!/usr/bin/env bash
# pmgfan の RPM をビルドする。rpm-build と Rust toolchain が必要。
#
#   ./packaging/build-rpm.sh
#
# 成果物: ~/rpmbuild/RPMS/<arch>/pmgfan-<version>-<release>.<arch>.rpm
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$(pwd)

# --- バージョンは Cargo.toml の workspace.package から ---
VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
[ -n "$VERSION" ] || { echo "cannot read version from Cargo.toml" >&2; exit 1; }
NAME=pmgfan

# --- rpmbuild ツリー ---
TOPDIR="${RPMBUILD_TOPDIR:-$HOME/rpmbuild}"
mkdir -p "$TOPDIR"/{SOURCES,SPECS,RPMS,SRPMS,BUILD,BUILDROOT}

# --- ソース tarball（git archive が使えれば HEAD、なければ作業ツリー） ---
TARBALL="$TOPDIR/SOURCES/$NAME-$VERSION.tar.gz"
if git -C "$ROOT" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    git -C "$ROOT" archive --format=tar --prefix="$NAME-$VERSION/" HEAD \
        | gzip >"$TARBALL"
    echo "source: git HEAD"
else
    tar -czf "$TARBALL" \
        --exclude=target --exclude=.git \
        --transform "s,^,$NAME-$VERSION/," \
        -C "$ROOT" .
    echo "source: working tree"
fi

cp "$ROOT/packaging/rpm/$NAME.spec" "$TOPDIR/SPECS/"

# rustup の cargo は PATH に無いことがあるので拾う
case ":$PATH:" in
    *":$HOME/.cargo/bin:"*) ;;
    *) PATH="$HOME/.cargo/bin:$PATH" ;;
esac
export PATH

rpmbuild -bb \
    --define "_topdir $TOPDIR" \
    "$TOPDIR/SPECS/$NAME.spec"

echo
echo "built RPMs:"
find "$TOPDIR/RPMS" -name "$NAME-*.rpm" -newer "$TARBALL" -o \
     -name "$NAME-*.rpm" -mmin -2 | sort -u
