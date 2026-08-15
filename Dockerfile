# Build a static binary, ship it from scratch: the image is the gateway
# and CA roots, nothing else.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN cargo build --release -p router-bin

FROM scratch
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --from=build /src/target/release/caret-router /caret-router
EXPOSE 8080
ENTRYPOINT ["/caret-router"]
