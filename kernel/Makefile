# SPDX-License-Identifier: Apache-2.0
#
# Out-of-tree kbuild Makefile for the tartine module. See
# kernel/README.md for the parts of this that are unverified (no kernel
# build tree was available where this was written) and what to check
# first if `make` doesn't just work.

MODULE_NAME := tartine
obj-m += $(MODULE_NAME).o

KDIR ?= /lib/modules/$(shell uname -r)/build
PWD  := $(CURDIR)

KCORE_DIR      := $(CURDIR)/../crates/tartine-kcore
KCORE_STATICLIB := $(CURDIR)/../target/release/libtartine_kcore.a
KCORE_OBJDIR    := $(CURDIR)/.kcore-objs

# kbuild links whatever $(MODULE_NAME)-objs lists; it doesn't know how to
# consume a prebuilt .a directly. We extract the .a's own members (our
# code plus `core`/`compiler_builtins` — tartine-kcore has zero
# dependencies beyond those, by design; see its Cargo.toml) and list the
# resulting .o files explicitly instead of gluing the .a in at the
# final link step.
$(MODULE_NAME)-objs := tartine_main.o $(wildcard $(KCORE_OBJDIR)/*.o)

all: $(KCORE_OBJDIR)/.extracted
	$(MAKE) -C $(KDIR) M=$(PWD) modules

$(KCORE_STATICLIB):
	cd $(KCORE_DIR) && cargo build --release --features freestanding

$(KCORE_OBJDIR)/.extracted: $(KCORE_STATICLIB)
	rm -rf $(KCORE_OBJDIR)
	mkdir -p $(KCORE_OBJDIR)
	cd $(KCORE_OBJDIR) && ar x $(KCORE_STATICLIB)
	touch $@

clean:
	$(MAKE) -C $(KDIR) M=$(PWD) clean
	rm -rf $(KCORE_OBJDIR)

.PHONY: all clean
