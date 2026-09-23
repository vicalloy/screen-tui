# Screen TUI —— 构建入口（T0.1）
#
# 目标平台与工具链（design/tech-design.md §6）：
#   linux/amd64, linux/arm64  → Docker + cargo-zigbuild，musl 静态
#   macOS  arm64, x86_64      → 本机 cargo
#
# 与设计文档的一处有意偏离：文档写「产物直接落在宿主机 target/<triple>/release/」，
# 但同时又把 stui-target 命名卷挂在 /work/target 上 —— 命名卷不是宿主机目录，
# 产物会留在卷里。更关键的是容器以 root 运行，直接写宿主机 target/ 会留下 root 属主文件，
# 让后续本机 `cargo build` 撞权限错误。
# 因此这里把产物**提取**到 dist/（同样 root 属主，但只由本流水线写入），
# 并提供 clean-linux 用容器删掉它们。见 `make help`。

CARGO   ?= cargo
DOCKER  ?= docker

# 镜像 tag 锁死，升级走显式变更（tech-design §10 风险 7）
#
# 注：设计文档原本写的 ghcr.io/rust-cross/cargo-zigbuild:v0.19.8 **不存在**（实测
# `docker pull` 报 not found；GHCR 上的 tag 不带 v 前缀，最高稳定版为 0.17.1）。
# 这里改为实际存在的 0.17.1，并在 ci.yml/release.yml 中同步。
IMG     := ghcr.io/rust-cross/cargo-zigbuild:0.17.1
ALPINE  := alpine:3

LINUX_TARGETS := x86_64-unknown-linux-musl aarch64-unknown-linux-musl
MACOS_TARGETS := aarch64-apple-darwin x86_64-apple-darwin

BIN   := stui
DIST  := dist
CARGO_VOL  := stui-cargo
TARGET_VOL := stui-target

CARGO_TARGET_DIR ?= target

.DEFAULT_GOAL := help

.PHONY: help test lint fmt build build-linux extract-linux verify-linux build-macos package release linux macos clean clean-linux

help: ## 显示可用目标
	@echo "stui build targets"
	@echo ""
	@echo "  make test          跑单元测试（含多版本 -ls fixture 回归）"
	@echo "  make lint          rustfmt --check + clippy -D warnings（CI 门禁）"
	@echo "  make build         本机 debug 构建"
	@echo "  make build-linux   Docker 交叉编译 linux amd64+arm64（musl 静态）→ dist/"
	@echo "  make verify-linux  在 Alpine 容器内验证产物可执行（接受标准 ②）"
	@echo "  make build-macos   本机编译 macOS arm64+x86_64"
	@echo "  make package       归档 + SHA256SUMS"
	@echo "  make release       linux + macos + package"
	@echo "  make clean-linux   用容器删掉 dist/ 里 root 属主的产物"

# ---------------------------------------------------------------- 质量门禁

test: ## 跑测试
	$(CARGO) test

lint: ## fmt + clippy
	$(CARGO) fmt --check
	$(CARGO) clippy --all-targets -- -D warnings

fmt: ## 直接格式化（不检查）
	$(CARGO) fmt

build: ## 本机构建
	$(CARGO) build

# ---------------------------------------------------------------- Linux（musl 静态）

build-linux: ## Docker 交叉编译双架构
	$(DOCKER) run --rm \
	  -v "$(CURDIR)":/work -w /work \
	  -v $(CARGO_VOL):/usr/local/cargo \
	  -v $(TARGET_VOL):/work/target \
	  $(IMG) bash -c 'set -e; \
	    for t in $(LINUX_TARGETS); do \
	      echo "==> cargo zigbuild --release --target $$t"; \
	      cargo zigbuild --release --target $$t; \
	    done'
	$(MAKE) extract-linux

extract-linux: ## 把容器卷里的产物提取到 dist/
	@mkdir -p $(DIST)
	$(DOCKER) run --rm \
	  -v $(TARGET_VOL):/src:ro \
	  -v "$(CURDIR)/$(DIST)":/dst \
	  $(ALPINE) sh -c 'set -e; \
	    for t in $(LINUX_TARGETS); do \
	      cp /src/$$t/release/$(BIN) /dst/$(BIN)-$$t; \
	      chmod 755 /dst/$(BIN)-$$t; \
	    done; \
	    ls -l /dst'

verify-linux: ## 在 Alpine 里验证 amd64 产物可执行 + 两者均为静态 ELF
	@test -f "$(DIST)/$(BIN)-x86_64-unknown-linux-musl" || { echo "先跑 make build-linux"; exit 1; }
	@echo "== amd64: 直接在 alpine 内执行（若为 glibc 动态链则必然失败）=="
	$(DOCKER) run --rm -v "$(CURDIR)/$(DIST)":/d:ro $(ALPINE) \
	  /d/$(BIN)-x86_64-unknown-linux-musl --version
	@echo "== 双架构: DT_NEEDED 必须为 0（静态）=="
	$(DOCKER) run --rm -v "$(CURDIR)/$(DIST)":/d:ro $(ALPINE) sh -c 'set -e; \
	  apk add --no-cache binutils >/dev/null 2>&1; \
	  for t in $(LINUX_TARGETS); do \
	    n=$$(readelf -d /d/$(BIN)-$$t | grep -c NEEDED || true); \
	    echo "  $$t: DT_NEEDED=$$n (0 = static)"; \
	    [ "$$n" = "0" ] || { echo "FAIL: $$t 不是静态链接"; exit 1; }; \
	  done; \
	  echo "OK: both linux artifacts are static"'

# ---------------------------------------------------------------- macOS（本机）

build-macos: ## 本机编译 macOS 双架构
	@for t in $(MACOS_TARGETS); do \
	  echo "==> cargo build --release --target $$t"; \
	  $(CARGO) build --release --target $$t || exit 1; \
	done
	@echo ""
	@echo "提示：x86_64-apple-darwin 需要先装标准库 —— rustup target add x86_64-apple-darwin"
	@echo "产物：$(CARGO_TARGET_DIR)/<triple>/release/$(BIN)"

# ---------------------------------------------------------------- 发布物

package: ## 归档 + SHA256SUMS
	@VERSION=$$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' Cargo.toml | head -1); \
	  VERSION="$$VERSION" scripts/package.sh

release: build-linux build-macos package ## 全量发布

linux: build-linux ## build-linux 的别名（CI 用）

macos: build-macos ## build-macos 的别名（CI 用）

# ---------------------------------------------------------------- 清理

clean: ## 清理本机构建产物
	$(CARGO) clean
	rm -rf $(DIST)

clean-linux: ## 用容器删除 dist/（文件可能是 root 属主）
	$(DOCKER) run --rm -v "$(CURDIR)/$(DIST)":/dst $(ALPINE) sh -c 'rm -rf /dst/* /dst/.[!.]* 2>/dev/null; true'
