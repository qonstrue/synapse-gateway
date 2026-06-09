FROM rust:1-bookworm AS build
WORKDIR /app
COPY . .
RUN cargo build --release --bin synapse-gateway

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /app/target/release/synapse-gateway /usr/local/bin/synapse-gateway
COPY config/ /app/config/
COPY migrations/ /app/migrations/
EXPOSE 8080 9090
ENTRYPOINT ["synapse-gateway"]
