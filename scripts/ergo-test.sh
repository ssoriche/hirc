#!/usr/bin/env bash
# Local Ergo for hirc's tests.
#
#   scripts/ergo-test.sh setup   download Ergo, make a test CA + localhost cert
#   scripts/ergo-test.sh run     run a server by hand on 16667 (plain) / 16697 (TLS)
#
# Everything lives in target/ergo/, which git ignores.
set -euo pipefail

cd "$(dirname "$0")/.."
VERSION=2.19.1
DIR=target/ergo

setup() {
    mkdir -p "$DIR"
    case "$(uname -s)-$(uname -m)" in
        Darwin-arm64) ASSET="ergo-$VERSION-macos-arm64.tar.gz" ;;
        Darwin-x86_64) ASSET="ergo-$VERSION-macos-x86_64.tar.gz" ;;
        Linux-x86_64) ASSET="ergo-$VERSION-linux-x86_64.tar.gz" ;;
        Linux-aarch64) ASSET="ergo-$VERSION-linux-arm64.tar.gz" ;;
        *) echo "unsupported platform" >&2; exit 1 ;;
    esac
    if [ ! -x "$DIR/ergo" ]; then
        BASE="https://github.com/ergochat/ergo/releases/download/v$VERSION"
        curl -fsSL -o "$DIR/$ASSET" "$BASE/$ASSET"
        curl -fsSL -o "$DIR/checksums.txt" "$BASE/ergo-$VERSION-checksums.txt"
        (cd "$DIR" && grep " $ASSET\$" checksums.txt | shasum -a 256 -c -)
        tar -xzf "$DIR/$ASSET" -C "$DIR"
        cp "$DIR/${ASSET%.tar.gz}/ergo" "$DIR/ergo"
    fi
    if [ ! -f "$DIR/ca.pem" ]; then
        # rustls won't accept Ergo's own self-signed cert (no SAN), so sign a
        # localhost cert with a throwaway CA and trust the CA in tests. The leaf
        # lives <825 days: the macOS platform verifier rejects longer ones.
        openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 3650 \
            -subj "/CN=hirc test CA" -keyout "$DIR/ca.key" -out "$DIR/ca.pem" \
            -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign" 2>/dev/null
        openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=localhost" \
            -keyout "$DIR/privkey.pem" -out "$DIR/leaf.csr" 2>/dev/null
        printf "subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\n" >"$DIR/leaf.ext"
        openssl x509 -req -in "$DIR/leaf.csr" -CA "$DIR/ca.pem" -CAkey "$DIR/ca.key" -CAcreateserial \
            -days 800 -out "$DIR/fullchain.pem" -extfile "$DIR/leaf.ext" 2>/dev/null
    fi
    "$DIR/ergo" --version
}

run() {
    RUN="$DIR/manual"
    mkdir -p "$RUN"
    sed -e 's/@PLAIN_PORT@/16667/' -e 's/@TLS_PORT@/16697/' tests/ergo/ircd.yaml.in >"$RUN/ircd.yaml"
    cp "$DIR/fullchain.pem" "$DIR/privkey.pem" "$RUN/"
    cd "$RUN"
    [ -f ircd.db ] || ../ergo initdb --conf ircd.yaml --quiet
    exec ../ergo run --conf ircd.yaml
}

case "${1:-}" in
    setup) setup ;;
    run) run ;;
    *) echo "usage: $0 setup|run" >&2; exit 2 ;;
esac
