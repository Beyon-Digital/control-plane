#!/bin/sh
# Generate a private development CA and a server identity. Never auto-trust it.
set -eu
DEST=${1:-.control-plane/tls}
NAME=${2:-localhost}
case "$NAME" in *[!a-zA-Z0-9.-]*|'') echo 'Use a DNS host name' >&2; exit 1;; esac
umask 077
mkdir -p "$DEST"
if [ -e "$DEST/ca.key" ] || [ -e "$DEST/server.key" ]; then
  echo "Certificates already exist in $DEST; refusing to overwrite" >&2
  exit 1
fi
openssl req -x509 -newkey rsa:3072 -nodes -days 365 -keyout "$DEST/ca.key" -out "$DEST/ca.pem" -subj '/CN=Control Plane Development CA' -addext 'basicConstraints=critical,CA:TRUE' -addext 'keyUsage=critical,keyCertSign,cRLSign'
openssl req -newkey rsa:3072 -nodes -keyout "$DEST/server.key" -out "$DEST/server.csr" -subj "/CN=$NAME"
cat > "$DEST/server.ext" <<EOF
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:$NAME,DNS:localhost,IP:127.0.0.1,IP:::1
EOF
openssl x509 -req -in "$DEST/server.csr" -CA "$DEST/ca.pem" -CAkey "$DEST/ca.key" -CAcreateserial -out "$DEST/server.pem" -days 90 -extfile "$DEST/server.ext"
openssl x509 -in "$DEST/ca.pem" -outform der -out "$DEST/ca.cer"
openssl x509 -in "$DEST/server.pem" -noout -fingerprint -sha256
echo "Trust ca.cer explicitly on your test devices. Keep ca.key private."
