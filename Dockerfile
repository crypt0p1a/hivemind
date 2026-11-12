# ---- build ----
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

# ---- runtime ----
FROM debian:bookworm-slim
# Tools the agents need for their work (Bash): extend for your projects.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl git bash ripgrep jq python3 openssh-client \
 && rm -rf /var/lib/apt/lists/*

# claude refuses --dangerously-skip-permissions as root: run as a regular user.
ARG UID=1000
RUN useradd -m -u ${UID} -s /bin/bash hive
USER hive
RUN curl -fsSL https://claude.ai/install.sh | bash

COPY --from=build /src/target/release/hive /usr/local/bin/hive

ENV PATH=/home/hive/.local/bin:$PATH \
    HIVE_HOME=/company \
    HIVE_SOCKET=/tmp/hive.sock \
    CLAUDE_CONFIG_DIR=/home/hive/.claude
WORKDIR /company
ENTRYPOINT ["hive"]
CMD ["daemon"]
