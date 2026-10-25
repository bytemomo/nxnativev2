#!/bin/sh
# Builds and runs the local play core's tests with AddressSanitizer and UBSan (any computer with clang or gcc)
set -e
cd "$(dirname "$0")"
CC=${CC:-cc}
$CC -std=gnu11 -O1 -g -Wall -Wextra -fsanitize=address,undefined -fno-omit-frame-pointer -pthread -o /tmp/t_lan t_lan.c ../lan_core.c
/tmp/t_lan
