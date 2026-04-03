# Chitty Workspace - Multi-stage Docker build
# Produces a headless server image — access the chat UI via browser at http://localhost:8770
#
# Build:  docker build -t chitty-workspace .
# Run:    docker run -p 8770:8770 chitty-workspace
# Data:   docker run -p 8770:8770 -v chitty-data:/root/.local/share/chitty-workspace chitty-workspace

# ── Stage 1: Build Rust binary ────────────────────────────
FROM rust:1.86-bookworm AS builder

# Install Linux deps for tao/wry/tray-icon (they compile even in headless mode)
RUN apt-get update && apt-get install -y --no-install-recommends \
    libgtk-3-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev \
    pkg-config build-essential libssl-dev cmake clang \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY assets/ assets/

RUN cargo build --release

# ── Stage 2: Runtime ──────────────────────────────────────
FROM python:3.11-slim-bookworm

# Runtime libs for the Rust binary (GTK not needed — headless mode skips UI)
RUN apt-get update && apt-get install -y --no-install-recommends \
    libsqlite3-0 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Python sidecar dependencies
COPY sidecar/requirements.txt /tmp/requirements.txt
RUN pip install --no-cache-dir -r /tmp/requirements.txt && rm /tmp/requirements.txt

# Rust binary
COPY --from=builder /app/target/release/chitty-workspace /usr/local/bin/chitty-workspace

# Marketplace assets (seeded to data dir on first run)
COPY --from=builder /app/assets/marketplace /usr/local/bin/assets/marketplace

# Sidecar scripts
COPY sidecar/inference_server.py /usr/local/bin/sidecar/inference_server.py
COPY sidecar/media_engine.py /usr/local/bin/sidecar/media_engine.py
COPY sidecar/requirements.txt /usr/local/bin/sidecar/requirements.txt
COPY sidecar/requirements-full.txt /usr/local/bin/sidecar/requirements-full.txt

ENV HOME=/root
EXPOSE 8770

# Persist database, config, installed packages, and models across restarts
VOLUME ["/root/.local/share/chitty-workspace"]

ENTRYPOINT ["chitty-workspace", "run", "--headless"]
