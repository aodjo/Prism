# The rendezvous server, as a static binary in an image with nothing else in it.
#
# Nothing in this server is platform specific and nothing it depends on is C, so it links
# statically against musl and the final image needs no libraries, no shell, and no package
# manager. What ships is one file of a few megabytes, which is also the entire attack surface.
#
# The account API it serves is plain HTTP, deliberately: TLS belongs to the reverse proxy in
# front of it, which is a thing that already exists and already knows how to renew a
# certificate. Terminating TLS here would mean a cryptographic provider, and the ones worth
# trusting are C — which would cost exactly the property this file is about.

FROM rust:1-alpine AS build

# musl-dev supplies the linker's startup files. No other C is involved: every dependency in
# this binary is pure Rust, which is why the resulting image can be empty.
RUN apk add --no-cache musl-dev

WORKDIR /src

# The whole workspace, because the server depends on prism-core and Cargo resolves versions
# across the workspace as a unit. The .dockerignore keeps this to source.
COPY . .

# Cache mounts so a rebuild after a source change does not recompile every dependency. The
# binary is copied out of the cached target directory, since the mount does not survive the
# layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p prism-rendezvous \
     && cp target/release/prism-rendezvous /prism-rendezvous

FROM scratch

COPY --from=build /prism-rendezvous /prism-rendezvous

# Nothing here needs to write, read a file, or resolve a name. Running as a number rather than
# a name because there is no /etc/passwd in the image to hold one.
USER 65534:65534

EXPOSE 47300/udp

ENTRYPOINT ["/prism-rendezvous"]
CMD ["--bind", "0.0.0.0:47300"]
