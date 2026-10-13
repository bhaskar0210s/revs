# Apple silicon, macOS 27+. Builds with the rustup toolchain (Zed extensions need its
# wasm32-wasip2 target, which a Homebrew Rust earlier on PATH doesn't have).
TOOLCHAIN ?= $(HOME)/.rustup/toolchains/stable-aarch64-apple-darwin/bin
CARGO = PATH="$(TOOLCHAIN):$$PATH" cargo
export MACOSX_DEPLOYMENT_TARGET = 27.0

# The version shown in Zed's extension list (also the release tag and the app bundle's version).
VERSION ?= $(shell sed -n 's/^version = "\(.*\)"/\1/p' extension.toml)

.PHONY: all server extension bundle zip test install uninstall clean

all: server extension

server:
	RUSTFLAGS="-C target-cpu=apple-m1 --remap-path-prefix=$(HOME)=~" $(CARGO) build --release --manifest-path server/Cargo.toml

extension:
	RUSTFLAGS="--remap-path-prefix=$(HOME)=~" $(CARGO) build --release --target wasm32-wasip2

# Revs.app: the server in a small app bundle, which also opens revs:// links.
bundle: server
	rm -rf build/Revs.app
	mkdir -p build/Revs.app/Contents/MacOS
	sed 's/@VERSION@/$(VERSION)/' bundle/Info.plist > build/Revs.app/Contents/Info.plist
	cp server/target/release/revs build/Revs.app/Contents/MacOS/revs
	codesign --force --sign - build/Revs.app

# The release asset the extension downloads.
zip: bundle
	rm -f build/revs-aarch64-apple-darwin.zip
	cd build && ditto -c -k --norsrc --noextattr --noqtn --noacl --keepParent Revs.app revs-aarch64-apple-darwin.zip

test: server
	$(CARGO) test --manifest-path server/Cargo.toml
	rm -rf .e2e && mkdir .e2e && python3 tests/e2e.py server/target/release/revs .e2e && rm -rf .e2e

install: bundle extension
	./script/install.sh

uninstall:
	./script/uninstall.sh

clean:
	rm -rf build
	$(CARGO) clean
	$(CARGO) clean --manifest-path server/Cargo.toml
