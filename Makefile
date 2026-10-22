# Build libbp (Rust), the LD_PRELOAD shim (C), and the unmodified bp-socket sender/receiver.

CC            ?= gcc
CARGO         ?= cargo
BPSOCKET_REPO ?= https://github.com/DTN-MTP/bp-socket
# Pinned so the demo always runs against the sender.c / receiver.c this PoC was tested with.
BPSOCKET_REF  ?= 160298d4edd8f318e3c92f48f210c34943abe8ea
BPSOCKET_DIR  ?= third_party/bp-socket

LIBBP_A  := libbp/target/release/libbp.a
CFLAGS   ?= -O2 -g
WARNINGS := -Wall -Wextra -Wno-unused-parameter

.PHONY: all test demo deps clean fetch

all: libbpshim.so sender receiver

$(LIBBP_A): $(wildcard libbp/src/*.rs) libbp/Cargo.toml libbp/build.rs libbp/cbindgen.toml
	$(CARGO) build --release --manifest-path libbp/Cargo.toml

libbp/include/bp.h: $(LIBBP_A)

libbpshim.so: shim/bpshim.c shim/bpshim.map libbp/include/bp.h $(LIBBP_A) | $(BPSOCKET_DIR)/include/bp_socket.h
	$(CC) $(CFLAGS) $(WARNINGS) -fPIC -shared -o $@ shim/bpshim.c \
	    -Ilibbp/include -I$(BPSOCKET_DIR)/include \
	    -Wl,--version-script=shim/bpshim.map -Wl,-z,defs \
	    $(LIBBP_A) -lgcc_s -lpthread -ldl -lm

# The app programs are the repo's own files, compiled as-is.
sender receiver: %: $(BPSOCKET_DIR)/%.c
	$(CC) $(CFLAGS) -I$(BPSOCKET_DIR)/include -o $@ $<

$(BPSOCKET_DIR)/include/bp_socket.h $(BPSOCKET_DIR)/sender.c $(BPSOCKET_DIR)/receiver.c:
	$(MAKE) fetch

fetch:
	@[ -d $(BPSOCKET_DIR)/.git ] || { \
	  mkdir -p $(BPSOCKET_DIR) && cd $(BPSOCKET_DIR) && git init -q && \
	  git remote add origin $(BPSOCKET_REPO) && \
	  git fetch -q --depth 1 origin $(BPSOCKET_REF) && git checkout -q FETCH_HEAD; }

# Installs the dtn7-rs daemon (dtnd) into ./.tools/bin (no root needed).
deps:
	$(CARGO) install dtn7 --locked --root .tools

# Rust unit/integration tests (fake dtn7 node), then the shim tests against a real dtnd.
test: all
	$(CARGO) test --release --manifest-path libbp/Cargo.toml
	tests/run_shim_test.sh

demo: all
	./demo.sh

clean:
	$(CARGO) clean --manifest-path libbp/Cargo.toml
	rm -f libbpshim.so sender receiver
