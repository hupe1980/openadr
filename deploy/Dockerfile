# The VTN as one binary in a distroless image.
#
# `--features` names exactly what this deployment uses: a smaller binary, and a build that fails
# rather than starts if a flag is asked for that was not compiled in.
FROM rust:1-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release --features vtn,client,sqlite,webhook,mqtt,internal-auth,mdns,tls \
 && strip target/release/openadr

FROM gcr.io/distroless/cc-debian12
COPY --from=build /src/target/release/openadr /usr/local/bin/openadr
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/openadr"]
CMD ["vtn"]
