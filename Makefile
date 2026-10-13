# Apple silicon, macOS 27+. Builds with the rustup toolchain (Zed extensions need its
# wasm32-wasip2 target, which a Homebrew Rust earlier on PATH doesn't have).
TOOLCHAIN ?= $(HOME)/.rustup/toolchains/stable-aarch64-apple-darwin/bin
CARGO = PATH="$(TOOLCHAIN):$$PATH" cargo
export MACOSX_DEPLOYMENT_TARGET = 27.0

# The version shown in Zed's extension list (also the release tag and the app bundle's version).
VERSION ?= $(shell sed -n 's/^version = "\(.*\)"/\1/p' extension.toml)

.PHONY: all server extension bundle zip sums checksums check-checksums test test-shipped install uninstall clean

all: server extension

server:
	RUSTFLAGS="-C target-cpu=apple-m1 --remap-path-prefix=$(HOME)=~" $(CARGO) build --release --locked --manifest-path server/Cargo.toml

extension:
	RUSTFLAGS="--remap-path-prefix=$(HOME)=~" $(CARGO) build --release --locked --target wasm32-wasip2

# Revs.app: the server in a small app bundle, which also opens revs:// links.
bundle: server
	rm -rf build/Revs.app
	mkdir -p build/Revs.app/Contents/MacOS
	sed 's/@VERSION@/$(VERSION)/' bundle/Info.plist > build/Revs.app/Contents/Info.plist
	cp server/target/release/revs build/Revs.app/Contents/MacOS/revs
	# No symbols or build paths in what ships (cargo's own stripping can fail quietly).
	strip build/Revs.app/Contents/MacOS/revs
	nm -ap build/Revs.app/Contents/MacOS/revs > build/revs.nm
	! grep -q ' OSO ' build/revs.nm
	# The hardened runtime: no code injected through DYLD_* variables.
	codesign --force --options runtime --sign - build/Revs.app

# The release asset the extension downloads.
zip: bundle
	rm -f build/revs-aarch64-apple-darwin.zip
	cd build && ditto -c -k --norsrc --noextattr --noqtn --noacl --keepParent Revs.app revs-aarch64-apple-darwin.zip

# Every file of the bundle with its SHA-256, as server.sha256 lists them.
SUMS = find Revs.app -type f -print0 | LC_ALL=C sort -z | xargs -0 shasum -a 256

sums:
	@cd build && $(SUMS)

# Releasing: push the version change to main, then `make checksums` takes the checksums of
# the server CI built from it, for the extension to check its download against. Commit
# server.sha256 and tag that commit: the release job builds the server again and publishes
# it only if it matches. A published version is never rebuilt; the next one gets a new tag.
checksums:
	rm -rf build/ci && mkdir -p build/ci
	run=$$(gh run list -R bhaskar0210s/revs --workflow build --branch main --commit "$$(git rev-parse HEAD)" --status success --limit 1 --json databaseId --jq '.[0].databaseId') && \
	  test -n "$$run" && gh run download "$$run" -R bhaskar0210s/revs -n revs-aarch64-apple-darwin -D build/ci
	cd build/ci && ditto -x -k revs-aarch64-apple-darwin.zip . && $(SUMS) > ../../server.sha256
	grep -q '  Revs.app/Contents/MacOS/revs$$' server.sha256
	cat server.sha256

# Whether the bundle just built is exactly the one server.sha256 lists.
check-checksums:
	cd build && shasum -a 256 -c ../server.sha256
	cd build && $(SUMS) | diff - ../server.sha256

# The tests again, on the server as it ships (stripped, hardened runtime), outside its bundle
# (after `make bundle` or `make zip`).
test-shipped:
	cp build/Revs.app/Contents/MacOS/revs build/revs-shipped
	rm -rf .e2e && mkdir .e2e && python3 tests/e2e.py build/revs-shipped .e2e && rm -rf .e2e

test: server
	$(CARGO) test --locked --lib
	$(CARGO) test --locked --manifest-path server/Cargo.toml
	rm -rf .e2e && mkdir .e2e && python3 tests/e2e.py server/target/release/revs .e2e && rm -rf .e2e

install: bundle extension
	./script/install.sh

uninstall:
	./script/uninstall.sh

clean:
	rm -rf build
	$(CARGO) clean
	$(CARGO) clean --manifest-path server/Cargo.toml
