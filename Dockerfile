# ============================================================
#  ZLMediaKit-RS Docker 镜像
#  多阶段构建：编译阶段 + 运行阶段，最终镜像最小化
# ============================================================

# ---- 编译阶段 ----
FROM rust:1.80-slim-bookworm AS builder

# 安装编译依赖
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y \
    cmake \
    build-essential \
    pkg-config \
    libssl-dev \
    libsrt-gnutls-dev \
    ffmpeg \
    --no-install-recommends \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# 先复制依赖清单，利用 Docker 缓存层
COPY Cargo.toml Cargo.lock ./
COPY crates/core/Cargo.toml      crates/core/
COPY crates/codec/Cargo.toml     crates/codec/
COPY crates/rtmp/Cargo.toml      crates/rtmp/
COPY crates/rtsp/Cargo.toml      crates/rtsp/
COPY crates/http/Cargo.toml      crates/http/
COPY crates/flv/Cargo.toml       crates/flv/
COPY crates/hls/Cargo.toml       crates/hls/
COPY crates/mp4/Cargo.toml       crates/mp4/
COPY crates/srt/Cargo.toml       crates/srt/
COPY crates/transcode/Cargo.toml crates/transcode/
COPY crates/webrtc/Cargo.toml    crates/webrtc/
COPY crates/server/Cargo.toml    crates/server/

# 创建占位 src，只解析依赖不编译
RUN mkdir -p crates/core/src crates/codec/src crates/rtmp/src \
    crates/rtsp/src crates/http/src crates/flv/src crates/hls/src \
    crates/mp4/src crates/srt/src crates/transcode/src \
    crates/webrtc/src crates/server/src && \
    echo 'fn main() {}' > crates/server/src/main.rs && \
    for d in core codec rtmp rtsp http flv hls mp4 srt transcode webrtc; do \
      echo '' > crates/$d/src/lib.rs; \
    done

# 仅下载并编译依赖（利用缓存）
RUN cargo build --release 2>/dev/null || true

# 复制全部源码
COPY crates/ crates/
RUN touch crates/server/src/main.rs crates/*/src/lib.rs

# 正式编译（默认 features，可选开启转码）
ARG BUILD_FEATURES=""
RUN if [ -n "$BUILD_FEATURES" ]; then \
      cargo build --release --features "$BUILD_FEATURES"; \
    else \
      cargo build --release; \
    fi


# ---- 运行阶段 ----
FROM debian:bookworm-slim

# 运行时依赖
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y \
    ca-certificates \
    libsrt-gnutls1.5 \
    ffmpeg \
    --no-install-recommends \
    && rm -rf /var/lib/apt/lists/*

# 创建运行用户
RUN useradd --system --no-create-home --shell /usr/sbin/nologin zlm

# 复制编译产物
COPY --from=builder /build/target/release/zlmediakit /usr/local/bin/zlmediakit
COPY conf/ /etc/zlmediakit/

# 创建数据和静态文件目录
RUN mkdir -p /var/lib/zlmediakit/record /var/lib/zlmediakit/www && \
    chown -R zlm:zlm /var/lib/zlmediakit

USER zlm
WORKDIR /var/lib/zlmediakit

# 环境变量（可通过 docker run -e 覆盖）
ENV ZLM_CONFIG=/etc/zlmediakit/config.toml
ENV ZLM_RTMP_PORT=1935
ENV ZLM_RTSP_PORT=8554
ENV ZLM_HTTP_PORT=8080
ENV ZLM_API_PORT=8081
ENV ZLM_WEBRTC_PORT=9000

# 暴露默认端口
EXPOSE 1935 8554 8080 8081 9000

ENTRYPOINT ["zlmediakit", "--config", "/etc/zlmediakit/config.toml"]
