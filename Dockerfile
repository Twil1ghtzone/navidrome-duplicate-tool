# ---- Build ----
FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml ./
RUN mkdir src && echo "fn main(){}" > src/main.rs && cargo build --release && rm -rf src
COPY src ./src
COPY templates ./templates
RUN touch src/main.rs && cargo build --release

# ---- Runtime ----
FROM debian:bookworm-slim
RUN mkdir -p /config /backups && chmod 777 /config /backups
COPY --from=builder /app/target/release/navidrome-dedupe /usr/local/bin/navidrome-dedupe
ENV BIND=0.0.0.0:8080 NAVIDROME_DB=/data/navidrome.db CONFIG_DIR=/config BACKUP_DIR=/backups
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s CMD ["navidrome-dedupe","healthcheck"]
ENTRYPOINT ["navidrome-dedupe"]
