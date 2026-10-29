# syntax=docker/dockerfile:1
# Obraz serwera: statyczny plik (musl) na distroless, bez powłoki, jako użytkownik nonroot (65532).
# Dane (certyfikat, klucz, baza kont, aktualizacje klienta) w /data: montuj katalog z hosta
# (deploy/docker-compose.yml), bez tego zostają w warstwie kontenera i giną z nim.

FROM rust:1-alpine AS build
RUN apk add --no-cache build-base
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --locked --release -p server \
 && cp target/release/komunikator-server /komunikator-server
RUN mkdir -p /data/updates

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /komunikator-server /usr/local/bin/komunikator-server
COPY deploy/komunikator.toml /etc/komunikator/komunikator.toml
COPY --from=build --chown=65532:65532 /data /data
EXPOSE 7700/udp
ENTRYPOINT ["/usr/local/bin/komunikator-server"]
CMD ["run", "--config", "/etc/komunikator/komunikator.toml"]
