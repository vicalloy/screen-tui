# Screen TUI —— 构建入口（T0.1）
#
# 不假定宿主机是 macOS：本机目标由 uname 自动推导。
#   任意宿主机 linux/amd64+arm64 → Docker + cargo-zigbuild，musl 静态
#   本机原生目标（mac/linux 均可）→ cargo build --release --target <NATIVE_TARGET>
#
# 与设计文档的一处有意偏离：文档写「产物直接落在宿主机 target/<triple>/release/」，
# 但同时又把 stui-target 命名卷挂在 /work/target 上 —— 命名卷不是宿主机目录，
# 产物会留在卷里。更关键的是容器以 root 运行，直接写宿主机 target/ 会留下 root 属主文件，
# 让后续本机 `cargo build` 撞权限错误。
# 因此这里把产物**提取**到 dist/（同样 root 属主，但只由本流水线写入），
# 并提供 clean-linux 用容器删掉它们。见 `make help`。
#
# Linux 构建用持久容器 stui-build（ensure-container 保证存在且运行，编译走 docker exec）：
# 镜像/工具链/cargo registry/target 的缓存全部跨次保留，二次构建零下载。
# 卷仍在：容器删了（如换镜像）重建后缓存不丢；make clean-container 只删容器。

# cargo 不在 PATH 时回落 rustup 标准位置（~/.cargo/bin，mac/linux 通用）；仍可 CARGO=... 覆盖
CARGO   ?= $(shell command -v cargo 2>/dev/null || echo "$$HOME/.cargo/bin/cargo")
DOCKER  ?= docker

# 镜像 tag 锁死，升级走显式变更（tech-design §10 风险 7）
#
# 注：设计文档原本写的 ghcr.io/rust-cross/cargo-zigbuild:v0.19.8 **不存在**（实测
# `docker pull` 报 not found；GHCR 上的 tag 不带 v 前缀，最高稳定版为 0.17.1）。
# 这里改为实际存在的 0.17.1，并在 ci.yml/release.yml 中同步。
IMG     := ghcr.io/rust-cross/cargo-zigbuild:0.17.1
ALPINE  := alpine:3

# 容器内工具链版本：0.17.1 镜像自带 Rust ~1.76，不认 edition 2024（代码用了
# let-chains，需要 Rust 1.88+，且 let-chains 仅 2024 edition 可用，不能降 edition）。
# 首次构建 rustup 下载 minimal 工具链 + 双 musl std（~100 MB），之后缓存在卷里。
RUST_PIN    := 1.88.0
RUSTUP_VOL  := stui-rustup

LINUX_TARGETS := x86_64-unknown-linux-musl aarch64-unknown-linux-musl

# 宿主机原生目标（不假定 mac）：uname 推导，可用 make NATIVE_TARGET=<triple> 覆盖
UNAME_S := $(shell uname -s)
UNAME_M := $(shell uname -m)
ifeq ($(UNAME_S),Darwin)
  ifeq ($(UNAME_M),arm64)
    NATIVE_TARGET ?= aarch64-apple-darwin
  else
    NATIVE_TARGET ?= x86_64-apple-darwin
  endif
else
  ifeq ($(UNAME_M),x86_64)
    NATIVE_TARGET ?= x86_64-unknown-linux-gnu
  else
    NATIVE_TARGET ?= aarch64-unknown-linux-gnu
  endif
endif

BIN   := stui
DIST  := dist
BUILD_CONTAINER := stui-build
CARGO_VOL  := stui-cargo
TARGET_VOL := stui-target

CARGO_TARGET_DIR ?= target

.DEFAULT_GOAL := help

.PHONY: help test lint fmt build build-linux ensure-container extract-linux verify-linux build-shell package release linux clean clean-linux clean-container

help: ## 显示可用目标
	@echo "stui build targets（宿主机：$(UNAME_S)/$(UNAME_M)，原生目标 $(NATIVE_TARGET)）"
	@echo ""
	@echo "  make test          跑单元测试（含多版本 -ls fixture 回归）"
	@echo "  make lint          rustfmt --check + clippy -D warnings（CI 门禁）"
	@echo "  make build         本机原生编译（cargo build --release --target $(NATIVE_TARGET)）"
	@echo "  make build-linux   Docker 交叉编译 linux amd64+arm64（musl 静态）→ dist/"
	@echo "                     （持久容器 $(BUILD_CONTAINER)：不存在则创建，缓存不重复下载）"
	@echo "  make build-shell   进入 $(BUILD_CONTAINER) 容器排查（bash）"
	@echo "  make verify-linux  在 Alpine 容器内验证产物可执行（接受标准 ②）"
	@echo "  make package       归档 + SHA256SUMS"
	@echo "  make release       linux + 本机原生 + package"
	@echo "  make clean-linux   删掉 dist/ 里 root 属主的产物"
	@echo "  make clean-container  删除 $(BUILD_CONTAINER) 容器（卷里的缓存保留）"

# ---------------------------------------------------------------- 质量门禁

test: ## 跑测试
	$(CARGO) test

lint: ## fmt + clippy
	$(CARGO) fmt --check
	$(CARGO) clippy --all-targets -- -D warnings

fmt: ## 直接格式化（不检查）
	$(CARGO) fmt

# ---------------------------------------------------------------- 本机原生

build: ## 本机原生编译（mac/linux 通用，产物在 target/$(NATIVE_TARGET)/release/）
	$(CARGO) build --release --target $(NATIVE_TARGET)

# ---------------------------------------------------------------- Linux（musl 静态）
#
# 持久容器 stui-build：不存在则创建、停止则启动，编译走 docker exec。
# rustup 工具链 / cargo registry / target 增量全部留在容器+卷里，二次构建零下载。
# 卷仍然保留：容器被删（如升级镜像）后重建，缓存不丢。

build-linux: ensure-container ## Docker 交叉编译双架构
	$(DOCKER) exec $(BUILD_CONTAINER) bash -c 'set -e; \
	    rustup set auto-self-update disable; \
	    if rustup toolchain list | grep -q "^$(RUST_PIN)" \
	       && rustup target list --toolchain $(RUST_PIN) --installed | grep -q "^x86_64-unknown-linux-musl$$" \
	       && rustup target list --toolchain $(RUST_PIN) --installed | grep -q "^aarch64-unknown-linux-musl$$"; then \
	      echo "==> toolchain $(RUST_PIN) + musl std 已就绪，跳过 rustup install（离线可用）"; \
	    else \
	      rustup toolchain install $(RUST_PIN) --profile minimal \
	        --target x86_64-unknown-linux-musl --target aarch64-unknown-linux-musl; \
	    fi; \
	    for t in $(LINUX_TARGETS); do \
	      echo "==> cargo +$(RUST_PIN) zigbuild --release --target $$t"; \
	      cargo +$(RUST_PIN) zigbuild --release --target $$t; \
	    done'
	$(MAKE) extract-linux

ensure-container: ## 确保 stui-build 容器存在且在运行
	@if $(DOCKER) container inspect $(BUILD_CONTAINER) >/dev/null 2>&1; then \
	  if [ "$$($(DOCKER) container inspect -f '{{.State.Running}}' $(BUILD_CONTAINER))" != "true" ]; then \
	    echo "==> starting existing container $(BUILD_CONTAINER)"; \
	    $(DOCKER) start $(BUILD_CONTAINER) >/dev/null; \
	  fi; \
	else \
	  echo "==> creating build container $(BUILD_CONTAINER) ($(IMG))"; \
	  $(DOCKER) run -d --name $(BUILD_CONTAINER) \
	    -v "$(CURDIR)":/work -w /work \
	    -v $(CARGO_VOL):/usr/local/cargo \
	    -v $(RUSTUP_VOL):/usr/local/rustup \
	    -v $(TARGET_VOL):/work/target \
	    $(IMG) sleep infinity; \
	fi

extract-linux: ensure-container ## 把容器里的产物提取到 dist/
	@mkdir -p $(DIST)
	$(DOCKER) exec $(BUILD_CONTAINER) sh -c 'set -e; \
	    for t in $(LINUX_TARGETS); do \
	      cp /work/target/$$t/release/$(BIN) /work/dist/$(BIN)-$$t; \
	      chmod 755 /work/dist/$(BIN)-$$t; \
	    done; \
	    ls -l /work/dist'

verify-linux: ## 在 Alpine 里验证产物：按宿主机架构实跑 + 双架构静态检查
	@test -f "$(DIST)/$(BIN)-x86_64-unknown-linux-musl" || { echo "先跑 make build-linux"; exit 1; }
	@echo "== 实跑与宿主机同架构的产物（若为 glibc 动态链则必然失败）=="
	$(DOCKER) run --rm -v "$(CURDIR)/$(DIST)":/d:ro $(ALPINE) sh -c 'set -e; \
	  case "$$(uname -m)" in \
	    x86_64)  b=/d/$(BIN)-x86_64-unknown-linux-musl ;; \
	    aarch64) b=/d/$(BIN)-aarch64-unknown-linux-musl ;; \
	    *) echo "跳过实跑：未知架构 $$(uname -m)"; exit 0 ;; \
	  esac; \
	  test -f "$$b" || { echo "缺少 $$b"; exit 1; }; \
	  "$$b" --version'
	@echo "== 双架构: DT_NEEDED 必须为 0（静态）=="
	$(DOCKER) run --rm -v "$(CURDIR)/$(DIST)":/d:ro $(ALPINE) sh -c 'set -e; \
	  apk add --no-cache binutils >/dev/null 2>&1; \
	  for t in $(LINUX_TARGETS); do \
	    n=$$(readelf -d /d/$(BIN)-$$t | grep -c NEEDED || true); \
	    echo "  $$t: DT_NEEDED=$$n (0 = static)"; \
	    [ "$$n" = "0" ] || { echo "FAIL: $$t 不是静态链接"; exit 1; }; \
	  done; \
	  echo "OK: both linux artifacts are static"'

# ---------------------------------------------------------------- 发布物

package: ## 归档 + SHA256SUMS
	@VERSION=$$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' Cargo.toml | head -1); \
	  VERSION="$$VERSION" scripts/package.sh

release: build-linux build package ## 全量发布

linux: build-linux ## build-linux 的别名（CI 用）

# ---------------------------------------------------------------- 清理

clean: ## 清理本机构建产物
	$(CARGO) clean
	rm -rf $(DIST)

clean-linux: ## 删除 dist/（文件可能是 root 属主；优先用构建容器，无则临时 alpine）
	@if $(DOCKER) container inspect $(BUILD_CONTAINER) >/dev/null 2>&1; then \
	  $(DOCKER) exec $(BUILD_CONTAINER) sh -c 'rm -rf /work/dist/* /work/dist/.[!.]* 2>/dev/null; true'; \
	else \
	  $(DOCKER) run --rm -v "$(CURDIR)/$(DIST)":/dst $(ALPINE) sh -c 'rm -rf /dst/* /dst/.[!.]* 2>/dev/null; true'; \
	fi

clean-container: ## 删除构建容器（卷缓存保留，下次 build-linux 自动重建）
	$(DOCKER) rm -f $(BUILD_CONTAINER)

build-shell: ## 进入构建容器排查
	$(DOCKER) exec -it $(BUILD_CONTAINER) bash
