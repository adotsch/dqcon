all: dqcon

# Rust dqcon builder image (build once)
DQCON_BUILDER=dqcon-builder
DQCON_RUST_RUN=docker run --rm -u $$(id -u):$$(id -g) -v $$(pwd)/dqcon_rs:/src -w /src -e CARGO_HOME=/tmp/cargo -e HOME=/tmp -e XDG_CACHE_HOME=/tmp/.cache

dqcon-builder:
	docker build -f Dockerfile -t $(DQCON_BUILDER) .

dqcon: dqcon-builder dqcon-linux-amd64	
	cp dqcon-linux-amd64 dqcon

# Cross-compilation targets for different platforms
cross: dqcon-builder cross-dqcon
cross-dqcon: dqcon-linux-amd64 dqcon-linux-arm64 dqcon-windows-amd64.exe dqcon-windows-arm64.exe dqcon-darwin-amd64 dqcon-darwin-arm64

# dqcon Rust targets (requires: make dqcon-builder)
DQCON_SRC=dqcon_rs/src/main.rs dqcon_rs/Cargo.toml

dqcon-linux-amd64: $(DQCON_SRC)
	$(DQCON_RUST_RUN) $(DQCON_BUILDER) cargo zigbuild --release --target x86_64-unknown-linux-musl
	cp dqcon_rs/target/x86_64-unknown-linux-musl/release/dqcon dqcon-linux-amd64

dqcon-linux-arm64: $(DQCON_SRC)
	$(DQCON_RUST_RUN) $(DQCON_BUILDER) cargo zigbuild --release --target aarch64-unknown-linux-musl
	cp dqcon_rs/target/aarch64-unknown-linux-musl/release/dqcon dqcon-linux-arm64

dqcon-windows-amd64.exe: $(DQCON_SRC)
	$(DQCON_RUST_RUN) $(DQCON_BUILDER) cargo zigbuild --release --target x86_64-pc-windows-gnullvm
	cp dqcon_rs/target/x86_64-pc-windows-gnullvm/release/dqcon.exe dqcon-windows-amd64.exe

dqcon-windows-arm64.exe: $(DQCON_SRC)
	$(DQCON_RUST_RUN) $(DQCON_BUILDER) cargo zigbuild --release --target aarch64-pc-windows-gnullvm
	cp dqcon_rs/target/aarch64-pc-windows-gnullvm/release/dqcon.exe dqcon-windows-arm64.exe

dqcon-darwin-amd64: $(DQCON_SRC)
	$(DQCON_RUST_RUN) $(DQCON_BUILDER) cargo zigbuild --release --target x86_64-apple-darwin
	cp dqcon_rs/target/x86_64-apple-darwin/release/dqcon dqcon-darwin-amd64

dqcon-darwin-arm64: $(DQCON_SRC)
	$(DQCON_RUST_RUN) $(DQCON_BUILDER) cargo zigbuild --release --target aarch64-apple-darwin
	cp dqcon_rs/target/aarch64-apple-darwin/release/dqcon dqcon-darwin-arm64

zip: dqcon.zip

dqcon.zip: cross
	zip -9 dqcon.zip README.md LICENSE dqcon-*64* -x "*.zip"

zips: cross
	cp dqcon-linux-amd64 dqcon && zip dqcon-linux-amd64.zip dqcon
	cp dqcon-linux-arm64 dqcon && zip dqcon-linux-arm64.zip dqcon
	cp dqcon-windows-amd64.exe dqcon.exe && zip dqcon-windows-amd64.zip dqcon.exe
	cp dqcon-windows-arm64.exe dqcon.exe && zip dqcon-windows-arm64.zip dqcon.exe
	cp dqcon-darwin-amd64 dqcon && zip dqcon-darwin-amd64.zip dqcon
	cp dqcon-darwin-arm64 dqcon && zip dqcon-darwin-arm64.zip dqcon
	rm dqcon dqcon.exe

arch:
	zip -r archive/dqcon_src_$$(date +%Y%m%d_%H%M%S).zip . -x "dqcon_rs/target/*" -x "test/target/*" -x "archive/*" -x ".git/*" -x ".DS_Store" -x "dqcon" -x "dqcon-*" -x "*.exe" -x "*.zip"

clean:
	rm -f dqcon dqcon-linux-* dqcon-windows-* dqcon-darwin-* dqcon.zip dqcon-*.zip
	rm -rf dqcon_rs/target
