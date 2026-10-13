# veil C API のサンプル。先に `cargo build -p veil-capi --release` を実行しておく。
ROOT   ?= ../../..
CC     ?= gcc
CFLAGS ?= -O2 -Wall -Wextra -std=c99
LIBDIR ?= $(ROOT)/target/release
# 静的リンクに要るシステムライブラリ（`cargo rustc -p veil-capi -- --print native-static-libs` で確かめられる）
LIBS   ?= -lpthread -ldl -lm

hello: hello.c
	$(CC) $(CFLAGS) hello.c -I$(ROOT)/crates/veil-capi/include -L$(LIBDIR) -lveil $(LIBS) \
		-Wl,-rpath,$(abspath $(LIBDIR)) -o hello

# 静的ライブラリとリンクする場合
hello-static: hello.c
	$(CC) $(CFLAGS) hello.c -I$(ROOT)/crates/veil-capi/include $(LIBDIR)/libveil.a $(LIBS) -o hello-static

clean:
	rm -f hello hello-static

.PHONY: clean hello-static
