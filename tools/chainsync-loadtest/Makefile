export CARGO_PROFILE_RELEASE_CODEGEN_UNITS = 1
export CARGO_PROFILE_RELEASE_LTO = fat
export DOCKER_BUILDKIT = 1
export RUSTFLAGS = -D warnings
export NEAR_RELEASE_BUILD = release
export CARGO_TARGET_DIR = target

all:
	cargo build -p chainsync-loadtest --release
