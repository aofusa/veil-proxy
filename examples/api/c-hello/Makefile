# veil C API のサンプル。先に `cargo build -p veil-capi --release` を実行しておく。
ROOT   ?= ../../..
CC     ?= gcc
CFLAGS ?= -O2 -Wall -Wextra -std=c99
LIBDIR ?= $(ROOT)/target/release

hello: hello.c
	$(CC) $(CFLAGS) hello.c -I$(ROOT)/crates/veil-capi/include -L$(LIBDIR) -lveil -lpthread -ldl -lm \
		-Wl,-rpath,$(abspath $(LIBDIR)) -o hello

# 静的ライブラリとリンクする場合
hello-static: hello.c
	$(CC) $(CFLAGS) hello.c -I$(ROOT)/crates/veil-capi/include $(LIBDIR)/libveil.a -lpthread -ldl -lm -o hello-static

clean:
	rm -f hello hello-static

.PHONY: clean hello-static
