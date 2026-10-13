#!/bin/sh
# Builds a small repository with a known history for the end-to-end tests.
set -e
repo="$1"
rm -rf "$repo"
mkdir -p "$repo/src"
cd "$repo"
git init -q -b main
git config user.name "Test User"
git config user.email "test@example.com"
git config commit.gpgsign false
git remote add origin git@github.com:example/project.git

commit() { # message, epoch, [author name, author email]
    if [ -n "$3" ]; then
        env GIT_AUTHOR_DATE="@$2 +0000" GIT_COMMITTER_DATE="@$2 +0000" \
            GIT_AUTHOR_NAME="$3" GIT_AUTHOR_EMAIL="$4" git commit -q -m "$1"
    else
        env GIT_AUTHOR_DATE="@$2 +0000" GIT_COMMITTER_DATE="@$2 +0000" git commit -q -m "$1"
    fi
}

printf 'one\ntwo\nthree\nfour\nfive\n' > src/old_name.txt
git add . && commit "Add file (#1)" 1600000000

printf 'one\nTWO\nthree\nfour\nfive\n' > src/old_name.txt
git add . && commit "Shout two" 1650000000

git mv src/old_name.txt src/app.txt
commit "Rename to app.txt" 1660000000

printf 'one\nTWO\nthree\ninserted\nfour\nfive\nsix\n' > src/app.txt
git add .
commit "Insert line and add six

Longer description of the change.
Fixes #42." 1700000000 "Other Dev" "other@example.com"

printf 'ONE\nTWO\nthree\ninserted\nfour\nfive\nsix\n' > src/app.txt
git add . && commit "Shout one" 1750000000

git config core.autocrlf false
printf 'first\r\nsecond\r\n' > src/crlf.txt
git add . && commit "Add CRLF file" 1760000000

# Removals: two adjacent lines from different commits, then one line.
printf 'ONE\nTWO\nfour\nfive\nsix\n' > src/app.txt
git add . && commit "Drop three and inserted" 1770000000

printf 'ONE\nTWO\nfour\nsix\n' > src/app.txt
git add . && commit "Drop five" 1780000000
