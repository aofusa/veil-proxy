# veil C helpers kit のサンプル。先に `cargo build -p veil-capi-helpers --release`（libveil_kit。libveil と一緒にリンクしない）。
ROOT   ?= ../../..
CC     ?= gcc
CFLAGS ?= -O2 -Wall -Wextra -std=c11 -D_POSIX_C_SOURCE=200809L -Wno-missing-field-initializers
LIBDIR ?= $(ROOT)/target/release
LIBS   ?= -lpthread -ldl -lm

kit: kit.c
	$(CC) $(CFLAGS) kit.c -I$(ROOT)/crates/veil-capi/include -I$(ROOT)/crates/veil-capi-helpers/include \
		-L$(LIBDIR) -lveil_kit $(LIBS) -Wl,-rpath,$(abspath $(LIBDIR)) -o kit

clean:
	rm -f kit

.PHONY: clean
