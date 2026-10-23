// SPDX-License-Identifier: AGPL-3.0-only
//
// An LD_PRELOAD shim over fsync and fdatasync, for tocsin-store's latency
// work (TOC-62, TOC-66). Linux only.
//
//   cc -shared -fPIC -O2 -o fsync_shim.so scripts/bench/fsync_shim.c -ldl -lpthread
//   LD_PRELOAD=$PWD/fsync_shim.so TOCSIN_SYNCTRACE=1 cargo test ...
//
// TOCSIN_SYNCTRACE=1 prints one line to stderr per call:
//
//   SYNCTRACE <fsync|fdatasync> <thread> <path> <start ms> <took ms>
//
// with the start on CLOCK_MONOTONIC, the clock Rust's Instant reads, and the
// thread's name as the kernel keeps it (15 bytes: "tocsin-store-wr" is the
// writer). The write-latency bench syncs /dev/null as it opens the store,
// so that call's start is the bench's origin.
//
// TOCSIN_FSYNC_DELAY_MS=<n> sleeps n ms before each call on a file whose
// path ends in TOCSIN_FSYNC_DELAY_SUFFIX (default "-wal"): a disk whose
// syncs are slow, on demand. crates/tocsin-store/tests/slow_fsync.rs
// builds this file and runs itself under it.

#define _GNU_SOURCE
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static double now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e3 + t.tv_nsec / 1e6;
}

static void path_of(int fd, char *path, size_t size) {
    char link[64];
    snprintf(link, sizeof link, "/proc/self/fd/%d", fd);
    ssize_t n = readlink(link, path, size - 1);
    path[n > 0 ? n : 0] = 0;
}

static void delay(const char *path) {
    const char *ms = getenv("TOCSIN_FSYNC_DELAY_MS");
    if (!ms || !*ms) return;
    const char *suffix = getenv("TOCSIN_FSYNC_DELAY_SUFFIX");
    if (!suffix || !*suffix) suffix = "-wal";
    size_t p = strlen(path), s = strlen(suffix);
    if (p < s || strcmp(path + p - s, suffix) != 0) return;
    struct timespec t = {0, 0};
    long n = atol(ms);
    t.tv_sec = n / 1000;
    t.tv_nsec = (n % 1000) * 1000000L;
    nanosleep(&t, NULL);
}

static int traced(const char *what, int (*real)(int), int fd) {
    char path[512];
    path_of(fd, path, sizeof path);
    double start = now_ms();
    delay(path);
    int result = real(fd);
    const char *trace = getenv("TOCSIN_SYNCTRACE");
    if (trace && *trace && strcmp(trace, "0") != 0) {
        char thread[32] = "?";
        pthread_getname_np(pthread_self(), thread, sizeof thread);
        fprintf(stderr, "SYNCTRACE %s %s %s %.3f %.3f\n", what, thread, path, start,
                now_ms() - start);
    }
    return result;
}

int fsync(int fd) {
    static int (*real)(int);
    if (!real) real = (int (*)(int))dlsym(RTLD_NEXT, "fsync");
    return traced("fsync", real, fd);
}

int fdatasync(int fd) {
    static int (*real)(int);
    if (!real) real = (int (*)(int))dlsym(RTLD_NEXT, "fdatasync");
    return traced("fdatasync", real, fd);
}
