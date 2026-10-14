# veil C API の F-210 の機能のサンプル（ストリーミング・gRPC・L4）。先に `cargo build -p veil-capi --release`。
ROOT   ?= ../../..
CC     ?= gcc
CFLAGS ?= -O2 -Wall -Wextra -std=c99 -D_POSIX_C_SOURCE=200809L
LIBDIR ?= $(ROOT)/target/release
LIBS   ?= -lpthread -ldl -lm

features: features.c
	$(CC) $(CFLAGS) features.c -I$(ROOT)/crates/veil-capi/include -L$(LIBDIR) -lveil $(LIBS) \
		-Wl,-rpath,$(abspath $(LIBDIR)) -o features

clean:
	rm -f features

.PHONY: clean
