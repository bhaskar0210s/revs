#!/bin/sh
# Installs a build from source: the server (Revs.app, which also opens revs:// links from
# the line blame card) into the extension's folder as revs-dev, and the Zed extension.
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
zed="$HOME/Library/Application Support/Zed/extensions"
work="$zed/work/revs-lsp"
extension="$zed/installed/revs-lsp"
lsregister=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister

mkdir -p "$work"
rm -rf "$work/revs-dev.new"
mkdir -p "$work/revs-dev.new"
cp -R "$repo/build/Revs.app" "$work/revs-dev.new/Revs.app"
rm -rf "$work/revs-dev"
mv "$work/revs-dev.new" "$work/revs-dev"
"$lsregister" -f "$work/revs-dev/Revs.app"

mkdir -p "$extension"
{
    cat "$repo/extension.toml"
    printf '\n[lib]\nkind = "Rust"\nversion = "0.7.0"\n'
} > "$extension/extension.toml.new"
cp "$repo/target/wasm32-wasip2/release/revs_zed.wasm" "$extension/extension.wasm"
mv "$extension/extension.toml.new" "$extension/extension.toml"

echo "Installed the Revs server in $work/revs-dev"
echo "Installed the Zed extension in $extension"
