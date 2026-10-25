// Local play diagnostics, on with DM_LAN_DEBUG=1 (a debug build bundles it: nxnative build <game> --debug; or a line in the app's Documents/env.txt).
// What they add to the log, beyond lan_hle.c's own lines (the calls that change the network, failures, the sockets made):
//   - each local play line with the time since start and the thread;
//   - every socket call with its arguments, the frequent nn::ldn calls' successes, the first packets each way;
//   - when the network ends or the game shows an error: Pia's jobs (the step each is in, its result {state, code}) and Pia's failed results, found in the game's heap,
//     and where each thread of the game is in main;
//   - on a build these were found on (Mario Kart 8 Deluxe 3.0.5, by build ID): Pia's session, mesh and statistics, and a watch that names the code which fails a session.
#pragma once
#include <stdint.h>

int lan_debug(void);
void lan_diag_info(const void *info);   // the NetworkInfo the game asked into: Pia's objects sit around it
void lan_diag_start(void);              // nn::ldn::Initialize
void lan_diag_stop(void);               // nn::ldn::Finalize
void lan_diag_dump(const char *why);    // the network ends, or the game shows an error
void lan_diag_where(const char *why);   // where each thread of the game is in main
