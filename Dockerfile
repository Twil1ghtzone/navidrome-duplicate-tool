# ---- Build ----
FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main(){}" > src/main.rs && cargo build --release && rm -rf src
COPY src ./src
COPY templates ./templates
RUN touch src/main.rs && cargo build --release

# ---- Runtime ----
FROM debian:bookworm-slim
LABEL org.opencontainers.image.source="https://github.com/Twil1ghtzone/navidrome-duplicate-tool" \
      org.opencontainers.image.description="Navidrome Duplicate Tool (inhaltsbasierte Duplikatsuche mit vollstaendiger Uebernahme von Favoriten, Playlists und Historie)" \
      org.opencontainers.image.licenses="MIT"
RUN mkdir -p /config /backups /trash && chmod 777 /config /backups /trash
COPY --from=builder /app/target/release/navidrome-dedupe /usr/local/bin/navidrome-dedupe
ENV BIND=0.0.0.0:8080 NAVIDROME_DB=/data/navidrome.db CONFIG_DIR=/config BACKUP_DIR=/backups TRASH_DIR=/trash
EXPOSE 8080
VOLUME ["/config", "/backups", "/trash"]
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s CMD ["navidrome-dedupe","healthcheck"]
ENTRYPOINT ["navidrome-dedupe"]
