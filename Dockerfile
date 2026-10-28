# syntax=docker/dockerfile:1
#
# Max-performance build (no PGO) for Raspberry Pi 5 (Cortex-A76), built natively on an
# arm64 GitHub runner. Output: one fully static musl binary in an otherwise empty image.
#
#  - fat LTO + 1 codegen unit + panic=abort (Cargo.toml [profile.release])
#  - cross-language LTO: the C code (mimalloc, ring) is compiled to LLVM bitcode by clang
#    and optimised together with the Rust code by lld (--lto-O3)
#  - everything tuned for TARGET_CPU (binary needs ARMv8.2-A: Pi 5 yes, Pi 4 no)
#  - mimalloc replaces musl's slow malloc
#  - TLS roots are compiled in (webpki-roots), so the image needs no certificate files

ARG RUST_VERSION=1

FROM rust:${RUST_VERSION}-alpine AS builder
ARG TARGET_CPU=cortex-a76

# Cross-language LTO needs clang/lld from the same LLVM major version rustc uses
# (lld must be at least as new as rustc's LLVM to read its bitcode). Detect it and
# install the matching versioned packages from Alpine edge.
RUN <<'EOF'
set -eux
LLVM=$(rustc -vV | sed -n 's/^LLVM version: \([0-9]*\).*/\1/p')
apk add --no-cache musl-dev
apk add --no-cache --repository=https://dl-cdn.alpinelinux.org/alpine/edge/main \
    "clang${LLVM}" "lld${LLVM}" "llvm${LLVM}"

LLVM_BIN="/usr/lib/llvm${LLVM}/bin"
for tool in clang llvm-ar llvm-readelf; do
    [ -x "$LLVM_BIN/$tool" ] || { echo "missing $LLVM_BIN/$tool"; exit 1; }
done
LLD=$(command -v "$LLVM_BIN/ld.lld" || command -v "ld.lld-${LLVM}" || command -v ld.lld) \
    || { echo "ld.lld for LLVM ${LLVM} not found"; exit 1; }

TRIPLE=$(rustc -vV | sed -n 's/^host: //p')          # aarch64-unknown-linux-musl on the arm runner
TU=$(echo "$TRIPLE" | tr 'a-z-' 'A-Z_')               # AARCH64_UNKNOWN_LINUX_MUSL
TL=$(echo "$TRIPLE" | tr '-' '_')                     # aarch64_unknown_linux_musl

# Target-specific variables only: with an explicit --target, build scripts and
# proc-macros are compiled without these flags (they'd break under linker-plugin-lto).
cat > /build.env <<ENV
export PATH="$LLVM_BIN:\$PATH"
export TRIPLE="$TRIPLE"
export CARGO_INCREMENTAL=0
export CC_$TL="$LLVM_BIN/clang"
export AR_$TL="$LLVM_BIN/llvm-ar"
export CFLAGS_$TL="-O3 -flto=thin -mcpu=$TARGET_CPU"
export CARGO_TARGET_${TU}_LINKER="$LLVM_BIN/clang"
export CARGO_TARGET_${TU}_RUSTFLAGS="-Ctarget-cpu=$TARGET_CPU -Clinker-plugin-lto -Clink-arg=--ld-path=$LLD -Clink-arg=-Wl,--lto-O3"
ENV
cat /build.env
EOF

WORKDIR /src

# Dependencies first, in their own layer, so source-only changes reuse them.
COPY Cargo.toml Cargo.lock ./
RUN . /build.env \
 && mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked --target "$TRIPLE" \
 && rm -rf src

COPY src ./src
RUN <<'EOF'
set -eux
. /build.env
touch src/main.rs
cargo build --release --locked --target "$TRIPLE"
cp "target/$TRIPLE/release/reddit-rss" /reddit-rss
# must be fully static: scratch has no dynamic loader
if llvm-readelf -l /reddit-rss | grep -q INTERP; then echo "binary is not static"; exit 1; fi
llvm-readelf -p .comment /reddit-rss || true
EOF


FROM scratch
COPY --from=builder /reddit-rss /reddit-rss
# Don't let mimalloc commit memory up front: same speed, ~6 MB less RSS.
ENV MIMALLOC_ARENA_EAGER_COMMIT=0
# Unprivileged user; Docker allows binding port 80 inside the container network namespace.
USER 65534:65534
EXPOSE 80
ENTRYPOINT ["/reddit-rss"]
