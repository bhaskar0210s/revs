#!/bin/sh
set -eu
zed="$HOME/Library/Application Support/Zed/extensions"
lsregister=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
for app in "$zed"/work/revs-lsp/*/Revs.app; do
    [ -d "$app" ] && "$lsregister" -u "$app" || true
done
rm -rf "$zed/work/revs-lsp" "$zed/installed/revs-lsp" "$HOME/Library/Caches/Revs" "$HOME/Library/Application Support/Revs"
echo "Removed Revs (Zed settings were left as they are)"
