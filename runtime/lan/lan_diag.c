// Local play diagnostics (lan_diag.h): Pia's state read from the game's heap, and where the game's threads are. Off unless DM_LAN_DEBUG=1.
// Memory is read only on pages the system reports as mapped and in memory (mincore): the guest address space reserve is mapped but inaccessible.
#include "hle.h"
#include "lan_diag.h"
#include <stdatomic.h>
#include <sys/mman.h>
#include <mach/mach.h>
#include <pthread.h>
#include <time.h>
#include <unistd.h>

int dm_guest_as_contains(uintptr_t a);   // hle_svc.c

int lan_debug(void) {
    static _Atomic int on = -1; int v = atomic_load(&on);
    if (v < 0) { const char *e = getenv("DM_LAN_DEBUG"); v = e && !strcmp(e, "1"); atomic_store(&on, v); if (v) LOG("lan: local play diagnostics on (DM_LAN_DEBUG=1)"); }
    return v;
}
unsigned long long lan_diag_ms(void) {
    static _Atomic unsigned long long t0; struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
    unsigned long long v = (unsigned long long)t.tv_sec * 1000 + (unsigned long long)t.tv_nsec / 1000000, z = 0;
    atomic_compare_exchange_strong(&t0, &z, v);
    return v - atomic_load(&t0);
}
unsigned lan_diag_thread(void) { return (unsigned)pthread_mach_thread_np(pthread_self()); }
#define DLOG(fmt, ...) LOG("[%6llu ms th %x] lan: " fmt, lan_diag_ms(), lan_diag_thread(), ##__VA_ARGS__)

// ---------- reading the game's memory ----------
static const uint8_t *main_base(void) { for (int i = 0; i < g_nmod; i++) if (!strcmp(g_mod[i].name, "main")) return g_mod[i].base; return NULL; }
static size_t main_size(void) { for (int i = 0; i < g_nmod; i++) if (!strcmp(g_mod[i].name, "main")) return g_mod[i].size; return 0; }
static int readable(uintptr_t a, size_t n) {
    long pg = sysconf(_SC_PAGESIZE); char in = 0;
    if (!a) return 0;
    for (uintptr_t p = a & ~(uintptr_t)(pg - 1); p < a + n; p += (uintptr_t)pg) if (mincore((void *)p, (size_t)pg, &in) != 0 || !(in & 1)) return 0;
    return 1;
}
static int guest(uintptr_t a, size_t n) { return a && dm_guest_as_contains(a) && dm_guest_as_contains(a + n - 1) && readable(a, n); }
static uint64_t peek(uintptr_t a) { uint64_t v = 0; vm_size_t got = 0; return vm_read_overwrite(mach_task_self(), a, 8, (vm_address_t)&v, &got) == KERN_SUCCESS && got == 8 ? v : 0; }
static int in_main(uint64_t x) { const uint8_t *b = main_base(); return b && x >= (uint64_t)(uintptr_t)b && x < (uint64_t)(uintptr_t)b + main_size(); }
static uint64_t main_off(uint64_t x) { return x - (uint64_t)(uintptr_t)main_base(); }

// ---------- where the threads are ----------
// Stops the other threads for a moment, reads each one's pc, lr and frame-pointer chain, and logs the ones in the game's code (main offsets: the IDA addresses)
void lan_diag_where(const char *why) {
    thread_act_array_t th = NULL; mach_msg_type_number_t n = 0; thread_act_t self = mach_thread_self();
    if (task_threads(mach_task_self(), &th, &n) != KERN_SUCCESS) { mach_port_deallocate(mach_task_self(), self); return; }
    struct { unsigned id; uint64_t pc, lr, fr[10]; int nf; } snap[64]; int ns = 0;
    for (mach_msg_type_number_t i = 0; i < n && ns < 64; i++) {
        if (th[i] == self || thread_suspend(th[i]) != KERN_SUCCESS) continue;
        arm_thread_state64_t st; mach_msg_type_number_t cnt = ARM_THREAD_STATE64_COUNT;
        if (thread_get_state(th[i], ARM_THREAD_STATE64, (thread_state_t)&st, &cnt) == KERN_SUCCESS) {
            snap[ns].id = th[i]; snap[ns].pc = arm_thread_state64_get_pc(st); snap[ns].lr = arm_thread_state64_get_lr(st); snap[ns].nf = 0;
            uint64_t fp = arm_thread_state64_get_fp(st);
            for (int k = 0; k < 10 && fp && !(fp & 7); k++) { uint64_t nx = peek(fp), ra = peek(fp + 8); if (!ra) break; snap[ns].fr[snap[ns].nf++] = ra; if (nx <= fp) break; fp = nx; }
            ns++;
        }
        thread_resume(th[i]);
    }
    for (mach_msg_type_number_t i = 0; i < n; i++) mach_port_deallocate(mach_task_self(), th[i]);
    vm_deallocate(mach_task_self(), (vm_address_t)th, n * sizeof *th); mach_port_deallocate(mach_task_self(), self);
    for (int i = 0; i < ns; i++) {
        if (!in_main(snap[i].pc) && !in_main(snap[i].lr)) continue;   // the game's own code is running there, not waiting in the host
        char b[400]; int o = 0;
        #define ADDR(x) in_main(x) ? "main+" : "", (unsigned long long)(in_main(x) ? main_off(x) : (x))
        o += snprintf(b + o, sizeof b - (size_t)o, "pc %s%#llx lr %s%#llx |", ADDR(snap[i].pc), ADDR(snap[i].lr));
        for (int k = 0; k < snap[i].nf && o < (int)sizeof b - 24; k++) if (in_main(snap[i].fr[k])) o += snprintf(b + o, sizeof b - (size_t)o, " main+%#llx", (unsigned long long)main_off(snap[i].fr[k]));
        #undef ADDR
        DLOG("where (%s) thread %x: %s", why, snap[i].id, b);
    }
}

// ---------- Pia in the game's heap ----------
static _Atomic uintptr_t g_info;
void lan_diag_info(const void *info) { if (lan_debug()) atomic_store(&g_info, (uintptr_t)info); }

// Pia's jobs: +48 the step's function, +64 the name of the step, +88 the result they report to {state, code} (state 2 done, 3 failed, 4 cancelled).
// They sit around the network's info holder (the NetworkInfo the game asks into, less 24): a scan of 32 MB each way
static void pia_scan(void) {
    uintptr_t holder = atomic_load(&g_info); if (!holder) return;
    holder -= 24;
    long pg = sysconf(_SC_PAGESIZE); int jobs = 0, pairs = 0;
    static const int codes[] = {25634, 25657, 27653, 27662, 27673, 27675, 27749, 42097, 50182, 50208, 50209, 68615, 68616, 19473};   // Pia's local play codes
    for (uintptr_t a = (holder - (32u << 20)) & ~(uintptr_t)(pg - 1), end = holder + (32u << 20); a < end; a += (uintptr_t)pg) {
        if (!guest(a, (size_t)pg)) continue;
        const uint64_t *q = (const uint64_t *)a;
        for (long k = 8; k + 8 < pg / 8 && jobs < 40; k++) {
            uint64_t v = q[k];
            if (!in_main(v)) continue;   // a name in the game's image
            if ((v & (uint64_t)(pg - 1)) > (uint64_t)pg - 64 || !readable((uintptr_t)v, 64)) continue;
            const char *nm = (const char *)(uintptr_t)v;
            if (strncmp(nm, "Ldn", 3) && strncmp(nm, "Local", 5) && strncmp(nm, "CreateSession", 13) && strncmp(nm, "JoinSession", 11) && strncmp(nm, "CreateMesh", 10) && strncmp(nm, "nn::pia::", 9)) continue;
            if (!memmem(nm, strnlen(nm, 60), "Job::", 5)) continue;
            const uint64_t *job = q + k - 8;
            const int32_t *res = guest((uintptr_t)job[11], 8) ? (const int32_t *)(uintptr_t)job[11] : NULL;
            DLOG("diag Pia job %p in step \"%.60s\": result %d %d", (const void *)job, nm, res ? res[0] : -1, res ? res[1] : -1);
            jobs++;
        }
        const int32_t *w = (const int32_t *)a;
        for (long k = 0; k + 1 < pg / 4 && pairs < 20; k++) if (w[k] >= 3 && w[k] <= 4) for (size_t c = 0; c < sizeof codes / sizeof codes[0]; c++) if (w[k + 1] == codes[c]) {
            DLOG("diag Pia result at %p: %s, code %d", (const void *)(w + k), w[k] == 3 ? "failed" : "cancelled", w[k + 1]); pairs++; }
    }
    DLOG("diag Pia: %d jobs, %d failed results", jobs, pairs);
}

// ---------- Mario Kart 8 Deluxe 3.0.5 (main fe941ed5…): where its failures were found ----------
// Fixed addresses of this build only (AGENTS.md: never on another build): its Pia session, statistics, mesh and Common objects, and the heap words a watch follows
// (the guest heap is laid out the same in every run). The causes they showed: nn::socket's errno numbering (2618-0006), the play history key (27749, hle_friends.c)
#define MK8D_305 "fe941ed5ba14be5d505698da1bbf4fe7"
static int mk8d(void) { const char *b = dm_main_build(); return b && !strcmp(b, MK8D_305); }
static const void *slot(uintptr_t off) { const uint8_t *b = main_base(); uint64_t p = b ? *(const uint64_t *)(b + off) : 0; return p && readable((uintptr_t)p, 8) ? (const void *)(uintptr_t)p : NULL; }
static void mk8d_dump(void) {
    const uint8_t *b = main_base();
    const uint8_t *ses = *(uint8_t *const *)(b + 0x135A7E0), *flags = *(uint8_t *const *)(b + 0x135A8C8);
    if (guest((uintptr_t)ses, 0x150)) DLOG("diag MK8D session %p: +0xD4 %d +0xD8 %d (sub_99526C: 5 = failed)", (const void *)ses, *(const int32_t *)(ses + 0xD4), *(const int32_t *)(ses + 0xD8));
    if (guest((uintptr_t)flags, 0x50)) DLOG("diag MK8D Common: +76 thread error %d", *(const int32_t *)(flags + 76));
    const uint8_t *st = slot(0x13051F0);   // Pia's statistics: +0xA4 the last cancel code
    if (st && readable((uintptr_t)st + 0x98, 0x190)) DLOG("diag MK8D statistics: cancel code %d", *(const int32_t *)(st + 0xA4));
    const uint64_t *mp = slot(0x130C6E0); const uint8_t *mesh = mp && readable((uintptr_t)*mp, 0x140) ? (const uint8_t *)(uintptr_t)*mp : NULL;   // Mesh::Create's flags
    if (mesh) DLOG("diag MK8D mesh %p: +184 %d +202 %u +243 %u (both: a play history key per local player)", (const void *)mesh, *(const int32_t *)(mesh + 184), mesh[202], mesh[243]);
    const uint8_t *job = (const uint8_t *)0x70735e9cf8;   // LocalMatchCreateSessionJob: +152 the code it failed with
    if (guest((uintptr_t)job, 160)) DLOG("diag MK8D create-session job: +96 %d, failure code %d", *(const int32_t *)(job + 96), *(const int32_t *)(job + 152));
}
static _Atomic int g_watch;
static void *mk8d_watch(void *arg) {
    (void)arg;
    static const struct { const char *name; uintptr_t at; int size; } w[] = {
        {"create-session job failure code", 0x70735e9cf8 + 152, 4}, {"create-session job step result", 0x70735e9cf8 + 96, 4},
        {"session result state", 0x70735c5470, 4}, {"session result code", 0x70735c5474, 4}, {"session result cancel", 0x70735c5470 + 24, 1}};
    enum { N = sizeof w / sizeof w[0] };
    uint32_t last[N] = {0}; int ok[N] = {0};
    DLOG("diag MK8D watch on");
    for (unsigned long it = 0; atomic_load(&g_watch); it++) {
        if (!(it & 1023)) for (int i = 0; i < N; i++) ok[i] = guest(w[i].at, (size_t)w[i].size);
        for (int i = 0; i < N; i++) {
            if (!ok[i]) continue;
            uint32_t v = w[i].size == 1 ? *(volatile const uint8_t *)w[i].at : *(volatile const uint32_t *)w[i].at;
            if (v != last[i]) { DLOG("watch %s: %d -> %d", w[i].name, (int32_t)last[i], (int32_t)v); lan_diag_where(w[i].name); last[i] = v; }
        }
        usleep(50);
    }
    DLOG("diag MK8D watch off");
    return NULL;
}

// ---------- entry points ----------
void lan_diag_start(void) {
    if (!lan_debug()) return;
    DLOG("diag on: main %s%s", dm_main_build() ? dm_main_build() : "?", mk8d() ? " (Mario Kart 8 Deluxe 3.0.5: its Pia probes too)" : "");
    if (mk8d() && !atomic_exchange(&g_watch, 1)) { pthread_t t; if (pthread_create(&t, NULL, mk8d_watch, NULL) == 0) pthread_detach(t); else atomic_store(&g_watch, 0); }
}
void lan_diag_stop(void) { atomic_store(&g_watch, 0); }
void lan_diag_dump(const char *why) {
    if (!lan_debug()) return;
    DLOG("diag: %s", why);
    if (mk8d()) mk8d_dump();
    pia_scan();
    lan_diag_where(why);
}
