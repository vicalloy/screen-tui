#!/usr/bin/env bash
#
# 打包发布物（tech-design §7 / T0.1）
#
#   dist/stui-<triple>              ← make build-linux / build-macos 的产物
#   ↓
#   dist/screen-tui-v<ver>/stui-<arch>-<os>.tar.gz
#   dist/screen-tui-v<ver>/SHA256SUMS
#
# 包名带 -linux/-macos 与架构，**不暴露 triple 术语** —— 在手机上下载的人
# 不需要知道 musl 是什么。tar.gz 内路径平铺，解压得一个 stui 文件。
#
# 用法：VERSION=0.1.0 scripts/package.sh

set -euo pipefail

VERSION="${VERSION:-$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' Cargo.toml | head -1)}"
if [ -z "$VERSION" ]; then
  echo "package.sh: cannot determine VERSION (set VERSION=x.y.z)" >&2
  exit 1
fi

DIST="${DIST:-dist}"
OUT="${DIST}/screen-tui-v${VERSION}"
mkdir -p "$OUT"

# triple → 发布用友好名（不含 musl/unknown 这类术语）
friendly_name() {
  case "$1" in
    x86_64-unknown-linux-musl)  echo "x86_64-linux" ;;
    aarch64-unknown-linux-musl) echo "aarch64-linux" ;;
    aarch64-apple-darwin)       echo "aarch64-macos" ;;
    x86_64-apple-darwin)        echo "x86_64-macos" ;;
    *) echo "" ;;
  esac
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

staged=0
for triple in \
  x86_64-unknown-linux-musl \
  aarch64-unknown-linux-musl \
  aarch64-apple-darwin \
  x86_64-apple-darwin
do
  src="${DIST}/stui-${triple}"
  name="$(friendly_name "$triple")"

  # macOS 本机构建的产物落在 target/<triple>/release/stui，也接受这个位置
  if [ ! -f "$src" ] && [ -f "target/${triple}/release/stui" ]; then
    src="target/${triple}/release/stui"
  fi

  if [ ! -f "$src" ]; then
    echo "  skip  ${name}  (no artifact for ${triple})"
    continue
  fi

  work="$(mktemp -d)"
  cp "$src" "${work}/stui"
  chmod 755 "${work}/stui"

  # 解压即用的四步说明（README 片段）
  cat > "${work}/INSTALL.txt" <<EOF
stui ${VERSION} — install
========================

1. tar xzf stui-${name}.tar.gz
2. mkdir -p ~/.local/bin && mv stui ~/.local/bin/
3. Make sure ~/.local/bin is in your PATH.
4. stui --version

Requires GNU Screen 4.00.03 or newer (already present on most servers).
Uninstall: delete the stui binary and ~/.config/screen-tui/
EOF

  # 路径平铺：解压得 stui + INSTALL.txt
  tarball="${OUT}/stui-${name}.tar.gz"
  ( cd "$work" && tar cf - stui INSTALL.txt | gzip -9 > "${OLDPWD}/${tarball}" )
  rm -rf "$work"

  echo "  pack  ${name}  ($(wc -c < "$tarball" | tr -d ' ') bytes)"
  staged=$((staged + 1))
done

if [ "$staged" -eq 0 ]; then
  echo "package.sh: no artifacts found under ${DIST}/ or target/<triple>/release/" >&2
  echo "            run \`make build-linux\` and/or \`make build-macos\` first." >&2
  exit 1
fi

# SHA256SUMS：相对包内目录的裸文件名，便于 `shasum -c` 直接使用
( cd "$OUT" && : > SHA256SUMS
  for f in *.tar.gz; do
    printf '%s  %s\n' "$(sha256_of "$f")" "$f" >> SHA256SUMS
  done
)

echo ""
echo "staged ${staged} package(s) in ${OUT}:"
( cd "$OUT" && ls -1 && echo "" && cat SHA256SUMS )
