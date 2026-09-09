# The rendezvous server, as a static binary in an image with nothing else in it.
#
# Nothing in this server is platform specific, so it links statically against musl and the
# final image needs no libraries, no shell, and no package manager. What ships is one file,
# which is also the entire attack surface.
#
# The account API it *serves* is plain HTTP, deliberately: TLS belongs to the reverse proxy in
# front of it, which already exists and already knows how to renew a certificate. The server is
# nonetheless a TLS *client* — it calls a mail provider to send verification codes — and that
# is `rustls` on `ring`, which is C and assembly. So this build is not the pure-Rust one it
# once was, and the base image has to carry a C toolchain.

FROM rust:1-alpine AS build

# musl-dev supplies the linker's startup files, and the base image already carries gcc for the
# Rust linker itself. Both are needed: `ring` compiles C and assembles for the target, and
# without them the build stops at its build script rather than at anything of ours.
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

# The directory accounts are kept in, made here so that it exists in the final image with the
# right ownership. A named volume takes the ownership of whatever it is mounted over, and a
# scratch image has nothing to take it from — so without this the volume arrives owned by root
# and the server, which is not root, cannot write the file it was asked to keep.
RUN mkdir -p /var/lib/prism && chown 65534:65534 /var/lib/prism

FROM scratch

COPY --from=build /prism-rendezvous /prism-rendezvous
COPY --from=build --chown=65534:65534 /var/lib/prism /var/lib/prism

# Certificates for the mail provider it calls to send verification codes. A scratch image has
# no trust store, and without one every send fails at the handshake.
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

# Running as a number rather than a name because there is no /etc/passwd in the image to hold
# one. The only thing it writes is the accounts directory, and only when asked to keep accounts.
USER 65534:65534

EXPOSE 47300/udp
EXPOSE 47380/tcp

ENTRYPOINT ["/prism-rendezvous"]
CMD ["--bind", "0.0.0.0:47300"]
