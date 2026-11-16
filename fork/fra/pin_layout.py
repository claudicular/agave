#!/usr/bin/env python3
"""Runtime CPU layout for agave + fast lane + shredstream proxy on FRA (no restarts).

Generalizes pin_replay.py for the "execute once" core-layout experiments. Every 0.5 s it
re-resolves agave's PID (agave.service cgroup) and the proxy's PID, and pins threads by name:

  agave  solScHandleV<N>  -> HANDLER_CPUS[N]   (N >= len: AGAVE_CPUS)   [unset: AGAVE_CPUS]
         solScheduleV     -> SCHEDULER_CPUS                            [unset: AGAVE_CPUS]
         solFlExec<NN>    -> FL_EXEC_CPUS[NN]  (NN >= len: FL_EXEC_CPUS as a set)
         solFlSched       -> FL_SCHED_CPUS
         solFlIngest      -> FL_INGEST_CPUS
         solFl* (other)   -> FL_MISC_CPUS                              [unset: AGAVE_CPUS]
         everything else  -> AGAVE_CPUS
         solPohTickProd   -> POH_CPUS (+ SCHED_IDLE when POH_IDLE=1)     [unset: AGAVE_CPUS]
         solGeyserGrpcX*  -> GRPCX_CPUS                                  [unset: leave alone]
         solGeyserGrpc*   -> GRPC_CPUS                                   [unset: AGAVE_CPUS]
  proxy  every thread     -> PROXY_CPUS                                [unset: leave alone]

A freshly started agave is left alone for PIN_MIN_AGE_S (default 30) seconds: agave sizes
thread pools from its CPU affinity at startup and validates --rayon-global-threads against it.
Lists use cpuset syntax ("0-7,24-31"). Unset FL_* keys leave FL's own startup pinning intact.
Writes the active layout to LAYOUT_LOG (default /home/sol/pin_layout.log) on start.
On SIGTERM/SIGINT it exits without restoring anything (the next layout takes over).
"""
import os
import signal
import sys
import time


def parse(spec):
    out = []
    for part in (spec or "").split(","):
        part = part.strip()
        if not part:
            continue
        if "-" in part:
            a, b = part.split("-")
            out.extend(range(int(a), int(b) + 1))
        else:
            out.append(int(part))
    return out


def env_list(key):
    v = os.environ.get(key)
    return parse(v) if v else None


AGAVE = set(parse(os.environ.get("AGAVE_CPUS", "0-7,24-31")))
HANDLERS = env_list("HANDLER_CPUS")
SCHEDULER = env_list("SCHEDULER_CPUS")
FL_EXEC = env_list("FL_EXEC_CPUS")
FL_SCHED = env_list("FL_SCHED_CPUS")
FL_INGEST = env_list("FL_INGEST_CPUS")
FL_MISC = env_list("FL_MISC_CPUS")
PROXY = env_list("PROXY_CPUS")
POH = env_list("POH_CPUS")
POH_IDLE = os.environ.get("POH_IDLE", "0") in ("1", "true", "yes")
GRPC = env_list("GRPC_CPUS")
GRPCX = env_list("GRPCX_CPUS")
MIN_AGE_S = float(os.environ.get("PIN_MIN_AGE_S", "30"))
PROCS = "/sys/fs/cgroup/system.slice/agave.service/cgroup.procs"
CLK_TCK = os.sysconf("SC_CLK_TCK")


def age_s(pid):
    start_ticks = int(open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()[19])
    return float(open("/proc/uptime").read().split()[0]) - start_ticks / CLK_TCK


def agave_pids():
    try:
        pids = [int(p) for p in open(PROCS).read().split()]
    except OSError:
        return []
    out = []
    for pid in pids:
        try:
            if age_s(pid) >= MIN_AGE_S:
                out.append(pid)
        except (OSError, ValueError, IndexError):
            pass
    return out


def proxy_pids():
    out = []
    for d in os.listdir("/proc"):
        if not d.isdigit():
            continue
        try:
            if open(f"/proc/{d}/comm").read().strip().startswith("jito-shredstrea"):
                out.append(int(d))
        except OSError:
            pass
    return out


def indexed(name, prefix, table):
    """CPU for thread <prefix><N> from table[N]; the whole table when N is out of range."""
    suffix = name[len(prefix):]
    if not suffix.isdigit():
        return set(table)
    n = int(suffix)
    return {table[n]} if n < len(table) else set(table)


def want_agave(name):
    if name.startswith("solPohTickProd"):
        return set(POH) if POH else AGAVE
    if name.startswith("solGeyserGrpcX"):
        # Yellowstone's dedicated busy-polling delivery runtime pins itself (config `affinity`);
        # leave it alone unless GRPCX_CPUS overrides.
        return set(GRPCX) if GRPCX else None
    if name.startswith("solGeyserGrpc") and GRPC:
        return set(GRPC)
    if name.startswith("solScHandleV"):
        return indexed(name, "solScHandleV", HANDLERS) if HANDLERS else AGAVE
    if name.startswith("solScheduleV"):
        return set(SCHEDULER) if SCHEDULER else AGAVE
    if name.startswith("solFlExec"):
        return indexed(name, "solFlExec", FL_EXEC) if FL_EXEC else None
    if name.startswith("solFlSched"):
        return set(FL_SCHED) if FL_SCHED else None
    if name.startswith("solFlIngest"):
        return set(FL_INGEST) if FL_INGEST else None
    if name.startswith("solFl"):
        return set(FL_MISC) if FL_MISC else AGAVE
    return AGAVE


def set_idle(tid):
    """SCHED_IDLE for agave's PoH tick producer: a non-voting node never leads, so its hashing
    only needs otherwise-idle cycles."""
    try:
        if os.sched_getscheduler(tid) != os.SCHED_IDLE:
            os.sched_setscheduler(tid, os.SCHED_IDLE, os.sched_param(0))
            return 1
    except OSError:
        pass
    return 0


def apply_pid(pid, fn):
    changed = 0
    try:
        tids = os.listdir(f"/proc/{pid}/task")
    except OSError:
        return 0
    for tid in tids:
        try:
            name = open(f"/proc/{pid}/task/{tid}/comm").read().strip()
            if POH_IDLE and name.startswith("solPohTickProd"):
                changed += set_idle(int(tid))
            w = fn(name)
            if not w:
                continue
            if os.sched_getaffinity(int(tid)) != w:
                os.sched_setaffinity(int(tid), w)
                changed += 1
        except OSError:
            pass
    return changed


def apply_all():
    c = 0
    for pid in agave_pids():
        c += apply_pid(pid, want_agave)
    if PROXY:
        for pid in proxy_pids():
            c += apply_pid(pid, lambda _n: set(PROXY))
    return c


stop = False


def handle(*_):
    global stop
    stop = True


signal.signal(signal.SIGTERM, handle)
signal.signal(signal.SIGINT, handle)
layout = {k: os.environ.get(k) for k in ("AGAVE_CPUS", "HANDLER_CPUS", "SCHEDULER_CPUS", "FL_EXEC_CPUS",
                                           "FL_SCHED_CPUS", "FL_INGEST_CPUS", "FL_MISC_CPUS", "PROXY_CPUS",
                                           "POH_CPUS", "POH_IDLE", "GRPC_CPUS", "GRPCX_CPUS")}
try:
    with open(os.environ.get("LAYOUT_LOG", "/home/sol/pin_layout.log"), "a") as f:
        f.write(f"{int(time.time())} {layout}\n")
except OSError:
    pass
print("layout", layout, "first pass changed", apply_all(), flush=True)
while not stop:
    time.sleep(0.5)
    c = apply_all()
    if c:
        print(time.strftime("%H:%M:%S"), "re-pinned", c, flush=True)
sys.exit(0)
