// Shaders a build left to the device (DM_XCRUN_DEFER, "Shaders on device": each library is its Metal source behind //DMMSL1, dm/linux/bin/xcrun) compiled before the game
// starts, on every core, while the launcher shows how far it is (Launcher.swift). The host compiles such a library the first time the game uses it
// (nvn_mtl.m lib_from_file), which stutters; iOS keeps what Metal compiled from source in its cache, keyed by the source and the options, so the same compile here
// makes the game's a cache hit. The source is the library's bytes as they are and the options are lib_from_file's: keep the two the same.
// The packs are read one at a time (shaderpack.c dm_shaderpack_each): all 16 of Mario Kart 8 Deluxe's are 1.9 GB, which a launcher mapping them at once (beside the
// game's address space the host reserves meanwhile) could not count on.
// Done once per set of shaders, iOS version and GPU (an iOS update empties Metal's cache): Documents/dm-shaders-prepared holds that key. iOS keeps no compiled library a
// program can save (a computer's build ships .metallib files from Apple's compiler); its cache is what stands in for them, so a launch with the marker times a sample of
// compiles, and a sample that compiles slowly (the cache emptied under storage pressure) prepares again.
// Play pressed before it ends: the same pass goes on in the background during the game (dm_shaders_background), on two cores at a time; shaders the game
// needs first are compiled on demand as before, into the same cache, so the pass passes over them at cache speed and a run stopped early resumes the same way.
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#include <dirent.h>
#include <fcntl.h>
#include <stdatomic.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

int dm_shaderpack_each(const char *dir, int digit, void (*fn)(const char *name, const void *p, size_t len, void *ctx), void *ctx);

static const char k_magic[] = "//DMMSL1 ";

static _Atomic int g_background;   // the game is running: two compiles at a time (the launcher's Play while it finishes)
static dispatch_semaphore_t g_two;
void dm_shaders_background(int on) {
    static dispatch_once_t once; dispatch_once(&once, ^{ g_two = dispatch_semaphore_create(2); });
    atomic_store(&g_background, on);
    if (on) fprintf(stderr, "[dm] shaders: the rest compiles in the background while the game runs (two at a time)\n");
}

typedef struct { const void *p; size_t len; char name[64]; } lib_t;
typedef struct {
    lib_t *v; size_t n, cap;              // the current pack's deferred libraries (compile mode)
    size_t count; uint64_t bytes;         // all deferred libraries seen
    int compile, total, step;             // compile mode: total for the progress, its step
    size_t stride; int sampled; double sample_ms;   // sample mode (stride > 0): every stride-th library compiled and timed
    id<MTLDevice> dev; MTLCompileOptions *opts;
    _Atomic int done, failed;
    void (*progress)(int, int, void *); void *pctx;
} visit_t;

static int deferred(const char *name, const void *p, size_t len) {
    size_t nl = strlen(name);
    return nl > 9 && !strcmp(name + nl - 9, ".metallib") && len > sizeof k_magic && !memcmp(p, k_magic, sizeof k_magic - 1);
}

// The batch on every core, as lib_from_file compiles: the source as it is, the build's iOS compile convention (ios_metal.py: relaxed math, preserved invariance)
static void compile_batch(visit_t *v) {
    lib_t *libs = v->v;
    dispatch_apply(v->n, dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^(size_t i) {
        @autoreleasepool {
            int bg = atomic_load(&g_background);
            if (bg) dispatch_semaphore_wait(g_two, DISPATCH_TIME_FOREVER);
            NSString *src = [[NSString alloc] initWithBytes:libs[i].p length:libs[i].len encoding:NSUTF8StringEncoding];
            NSError *e = nil;
            id<MTLLibrary> lib = src ? [v->dev newLibraryWithSource:src options:v->opts error:&e] : nil;
            if (bg) dispatch_semaphore_signal(g_two);
            if (!lib && atomic_fetch_add(&v->failed, 1) < 5)
                fprintf(stderr, "[dm] [RED] shaders: %s does not compile: %s\n", libs[i].name, src ? e.localizedDescription.UTF8String : "not UTF-8 source");
            int d = atomic_fetch_add(&v->done, 1) + 1;
            if (v->progress && (d % v->step == 0 || d == v->total)) v->progress(d, v->total, v->pctx);
        }
    });
    v->n = 0;
}

static void on_entry(const char *name, const void *p, size_t len, void *ctx) {
    visit_t *v = ctx;
    if (!name) { if (v->compile && v->n) compile_batch(v); return; }   // the end of a pack, still mapped
    if (!deferred(name, p, len)) return;
    v->count++; v->bytes += len;
    if (v->stride && v->count % v->stride == 0 && v->sampled < 24) {   // the check that Metal's cache still holds the prepared shaders
        @autoreleasepool {
            NSString *src = [[NSString alloc] initWithBytes:p length:len encoding:NSUTF8StringEncoding];
            NSDate *t = [NSDate date];
            if (src) [v->dev newLibraryWithSource:src options:v->opts error:nil];
            v->sample_ms += -t.timeIntervalSinceNow * 1000; v->sampled++;
        }
    }
    if (!v->compile) return;
    if (v->n == v->cap) { v->cap = v->cap ? v->cap * 2 : 1024; v->v = realloc(v->v, v->cap * sizeof *v->v); }
    lib_t *e = &v->v[v->n++]; e->p = p; e->len = len; strlcpy(e->name, name, sizeof e->name);
}

// Every deferred library of shader folder dir through on_entry: pack by pack, or (a folder without packs, an older build) its files
static void visit(const char *dir, visit_t *v) {
    int packs = 0;
    for (int d = 0; d < 16; d++) packs += dm_shaderpack_each(dir, d, on_entry, v) != 0;
    if (packs) return;
    DIR *dd = opendir(dir); struct dirent *e;
    if (!dd) { fprintf(stderr, "[dm] shaders: cannot open %s\n", dir); return; }
    void **maps = NULL; size_t *lens = NULL, nm = 0;
    while ((e = readdir(dd))) {
        size_t nl = strlen(e->d_name);
        if (nl < 10 || strcmp(e->d_name + nl - 9, ".metallib")) continue;
        char path[2048]; snprintf(path, sizeof path, "%s/%s", dir, e->d_name);
        int fd = open(path, O_RDONLY); struct stat st;
        if (fd < 0) continue;
        if (fstat(fd, &st) || st.st_size <= (off_t)sizeof k_magic) { close(fd); continue; }
        void *m = mmap(NULL, (size_t)st.st_size, PROT_READ, MAP_PRIVATE, fd, 0); close(fd);
        if (m == MAP_FAILED) continue;
        maps = realloc(maps, (nm + 1) * sizeof *maps); lens = realloc(lens, (nm + 1) * sizeof *lens); maps[nm] = m; lens[nm++] = (size_t)st.st_size;
        on_entry(e->d_name, m, (size_t)st.st_size, v);
    }
    closedir(dd);
    on_entry(NULL, NULL, 0, v);
    for (size_t i = 0; i < nm; i++) munmap(maps[i], lens[i]);
    free(maps); free(lens);
}

// What the marker holds: the shader folder's name (its chain version), the libraries' count and bytes, the iOS version and the GPU
static NSString *prep_key(const char *dir, const visit_t *v, id<MTLDevice> dev) {
    return [NSString stringWithFormat:@"%s\t%zu\t%llu\t%@\t%@\n", strrchr(dir, '/') ? strrchr(dir, '/') + 1 : dir, v->count, (unsigned long long)v->bytes,
            NSProcessInfo.processInfo.operatingSystemVersionString, dev.name];
}
static NSString *marker(void) {
    NSString *docs = NSSearchPathForDirectoriesInDomains(NSDocumentDirectory, NSUserDomainMask, YES).firstObject;
    return docs ? [docs stringByAppendingPathComponent:@"dm-shaders-prepared"] : nil;
}

// The number of deferred libraries in shader folder dir still to compile on this device (0: none, or prepared already)
int dm_shaders_to_prepare(const char *dir) {
    @autoreleasepool {
        id<MTLDevice> dev = MTLCreateSystemDefaultDevice();
        visit_t v = {0};
        visit(dir, &v);
        int n = (int)v.count;
        if (!dev) { fprintf(stderr, "[dm] [RED] shaders: no Metal device: nothing prepared\n"); return 0; }
        NSString *have = n ? [NSString stringWithContentsOfFile:marker() encoding:NSUTF8StringEncoding error:nil] : nil;
        int prepared = n && [have isEqualToString:prep_key(dir, &v, dev)];
        if (prepared) {   // still in Metal's cache? A cached compile takes well under a millisecond, a real one tens
            visit_t s = {0}; s.dev = dev; s.stride = v.count / 24 > 0 ? v.count / 24 : 1;
            s.opts = [MTLCompileOptions new]; s.opts.preserveInvariance = YES;
            if (@available(iOS 18.0, macOS 15.0, *)) s.opts.mathMode = MTLMathModeRelaxed;
            visit(dir, &s);
            double avg = s.sampled ? s.sample_ms / s.sampled : 0;
            fprintf(stderr, "[dm] shaders: cache check: %d sampled, %.1f ms each on average\n", s.sampled, avg);
            if (avg > 10) { prepared = 0; [NSFileManager.defaultManager removeItemAtPath:marker() error:nil]; fprintf(stderr, "[dm] shaders: Metal's cache no longer holds them: prepared again\n"); }
        }
        fprintf(stderr, "[dm] shaders: %d libraries left to the device in %s (%.0f MB of source), %s\n", n, dir, v.bytes / 1e6,
                !n ? "nothing to prepare" : prepared ? "prepared already (Documents/dm-shaders-prepared)" : "not prepared on this device yet");
        return prepared ? 0 : n;
    }
}

// Compiles every deferred library of shader folder dir as lib_from_file would, on every core, a pack at a time; progress(done, total, ctx) from the compiling threads,
// about every half percent and at the end. Returns the number that failed to compile (the game meets the same error at their first use), or -1 without a Metal device.
// The marker is written at the end, so a run that was stopped starts again (what it compiled is in Metal's cache by then, so it goes fast)
int dm_shaders_prepare(const char *dir, void (*progress)(int done, int total, void *ctx), void *ctx) {
    @autoreleasepool {
        id<MTLDevice> dev = MTLCreateSystemDefaultDevice();
        if (!dev) return -1;
        visit_t count = {0};
        visit(dir, &count);
        visit_t v = {0};
        v.compile = 1; v.total = (int)count.count; v.step = v.total / 200 > 0 ? v.total / 200 : 1;
        v.dev = dev; v.progress = progress; v.pctx = ctx;
        v.opts = [MTLCompileOptions new];   // lib_from_file's options (nvn_mtl.m)
        v.opts.preserveInvariance = YES;
        if (@available(iOS 18.0, macOS 15.0, *)) v.opts.mathMode = MTLMathModeRelaxed;
        NSDate *t0 = [NSDate date];
        fprintf(stderr, "[dm] shaders: compiling %d deferred libraries on the device before the game starts (%s)\n", v.total, dir);
        visit(dir, &v);
        double s = -t0.timeIntervalSinceNow;
        fprintf(stderr, "[dm] shaders: %d compiled in %.0f s (%.1f ms each on average, %ld cores), %d failed\n", (int)atomic_load(&v.done), s,
                v.total ? s * 1000 / v.total : 0, (long)NSProcessInfo.processInfo.activeProcessorCount, (int)atomic_load(&v.failed));
        NSString *m = marker();
        if (m) [prep_key(dir, &v, dev) writeToFile:m atomically:YES encoding:NSUTF8StringEncoding error:nil];
        free(v.v);
        return atomic_load(&v.failed);
    }
}
