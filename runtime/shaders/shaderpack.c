// The iOS shader libraries in 16 pack files (shaderpack.py writes them into the app's msl/b-<chain>/): the host's reads of <key>_*.metallib / .json there
// (nvn_mtl.m lib_from_file and shader_load) find the bytes in the pack of the key's first hex digit, mapped once for the app's life. A folder without packs
// (an older build, a Mac host) keeps its files: NULL here, and the caller opens the file.
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#define NAME 48
typedef struct { char name[NAME]; uint64_t off, len; } pk_ent;
typedef struct { const uint8_t *base; size_t size; const pk_ent *ent; uint32_t n; int tried; } pk;
static pk g_pk[16];
static char g_dir[1024];
static pthread_mutex_t g_mu = PTHREAD_MUTEX_INITIALIZER;

static int hexv(char c) { return c >= '0' && c <= '9' ? c - '0' : c >= 'a' && c <= 'f' ? c - 'a' + 10 : -1; }

static void pk_open(pk *p, const char *dir, int digit) {   // under g_mu; a pack that is missing or not well formed stays unmapped
    char path[1100]; snprintf(path, sizeof path, "%s/shaders-%x.pack", dir, digit);
    int fd = open(path, O_RDONLY); struct stat st;
    if (fd < 0) return;
    if (fstat(fd, &st) || st.st_size < 16) { close(fd); return; }
    void *m = mmap(NULL, (size_t)st.st_size, PROT_READ, MAP_PRIVATE, fd, 0); close(fd);
    if (m == MAP_FAILED) return;
    const uint8_t *b = m; uint32_t ver, n; memcpy(&ver, b + 4, 4); memcpy(&n, b + 8, 4);
    size_t size = (size_t)st.st_size;
    if (memcmp(b, "DMPK", 4) || ver != 1 || (uint64_t)16 + (uint64_t)n * sizeof(pk_ent) > size) { munmap(m, size); fprintf(stderr, "[dm] [RED] shader pack %s is not well formed: its files are read from the folder\n", path); return; }
    const pk_ent *e = (const pk_ent *)(b + 16);
    for (uint32_t i = 0; i < n; i++) if (e[i].off > size || e[i].len > size - e[i].off) { munmap(m, size); fprintf(stderr, "[dm] [RED] shader pack %s has an entry past its end\n", path); return; }
    p->base = b; p->size = size; p->ent = e; p->n = n;
}

// Pack `digit` (0..15) of shader folder dir, mapped apart from the run's own (the launcher reads them before the host has its folder) and only while it is read:
// fn(name, bytes, length, ctx) for each file in name order, then fn(NULL, NULL, 0, ctx) while the bytes are still mapped. Returns the number of files, 0 when the pack is
// missing, -1 when it could not be read (said on stderr)
int dm_shaderpack_each(const char *dir, int digit, void (*fn)(const char *name, const void *p, size_t len, void *ctx), void *ctx) {
    char path[1100]; snprintf(path, sizeof path, "%s/shaders-%x.pack", dir, digit);
    if (access(path, F_OK)) return 0;
    pk one = {0}; pk_open(&one, dir, digit);
    if (!one.base) { fprintf(stderr, "[dm] [RED] shader pack %s could not be opened or mapped (%s)\n", path, strerror(errno)); return -1; }
    for (uint32_t i = 0; i < one.n; i++) {
        char name[NAME + 1]; memcpy(name, one.ent[i].name, NAME); name[NAME] = 0;
        fn(name, one.base + one.ent[i].off, (size_t)one.ent[i].len, ctx);
    }
    fn(NULL, NULL, 0, ctx);
    munmap((void *)one.base, one.size);
    return (int)one.n;
}

// path: a file in the shader folder → its bytes in a pack and *len, or NULL
const void *dm_shaderpack_find(const char *path, size_t *len) {
    const char *slash = strrchr(path, '/'); if (!slash || !slash[1]) return NULL;
    const char *name = slash + 1; size_t dl = (size_t)(slash - path);
    int digit = hexv(name[0]); if (digit < 0 || strlen(name) >= NAME) return NULL;
    pthread_mutex_lock(&g_mu);
    if (!g_dir[0] && dl < sizeof g_dir) { memcpy(g_dir, path, dl); g_dir[dl] = 0; }   // one shader folder per run
    int same = strlen(g_dir) == dl && !memcmp(g_dir, path, dl);
    pk *p = &g_pk[digit];
    if (same && !p->tried) { p->tried = 1; pk_open(p, g_dir, digit); }
    pthread_mutex_unlock(&g_mu);
    if (!same || !p->base) return NULL;
    uint32_t lo = 0, hi = p->n;
    while (lo < hi) {
        uint32_t mid = (lo + hi) / 2; int c = strncmp(name, p->ent[mid].name, NAME);
        if (!c) { *len = (size_t)p->ent[mid].len; return p->base + p->ent[mid].off; }
        if (c < 0) hi = mid; else lo = mid + 1;
    }
    return NULL;
}
