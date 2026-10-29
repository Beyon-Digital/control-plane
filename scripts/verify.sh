#!/bin/sh
set -eu
cargo fmt --all --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
npm --prefix sidecars/claude ci --ignore-scripts
npm --prefix sidecars/claude run build
node --test sidecars/push/server.test.mjs
python3 scripts/install_worker_test.py
python3 scripts/wsl_transfer_test.py
python3 scripts/acceptance_test.py
python3 -m unittest discover -s scripts/release/tests
python3 -m unittest discover -s helpers/linux/tests
python3 -m unittest discover -s helpers/windows -p 'test_*.py'
if [ "$(uname -s)" = Linux ]; then
  python3 scripts/wsl_bridge_test.py
fi
if [ "$(uname -s)" = Darwin ]; then
  swift test --package-path apple
  sh helpers/macos/build.sh
fi
