"""Shared cgroup v2/DinD-aware Docker resource flag helper for author verification suites."""
from __future__ import annotations

import os
from pathlib import Path
import socket
import struct

MIB = 1024 * 1024
GIB = 1024 * MIB


def _read_trimmed(path: Path) -> str | None:
    try:
        return path.read_text().strip()
    except OSError:
        return None


def _socket_peer_cred(sock_path: Path) -> tuple[int, int] | None:
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
            sock.connect(str(sock_path))
            raw = sock.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, struct.calcsize("3i"))
            pid, uid, _gid = struct.unpack("3i", raw)
            return pid, uid
    except OSError:
        return None


def _ancestor_chain(leaf: Path, root: Path) -> list[Path]:
    chain: list[Path] = []
    cur = leaf if (leaf == root or root in leaf.parents) else root
    while True:
        chain.append(cur)
        if cur == root:
            break
        parent = cur.parent
        if parent == root or root in parent.parents:
            cur = parent
        else:
            chain.append(root)
            break
    return chain


def _format_cpus(milli: int) -> str:
    whole, frac = divmod(max(10, milli), 1000)
    if frac == 0:
        return str(whole)
    return f"{whole}.{frac:03d}".rstrip("0").rstrip(".")


def _parse_mem_override(raw: str) -> int | None:
    text = raw.strip().lower()
    if not text:
        return None
    idx = 0
    while idx < len(text) and (text[idx].isdigit() or text[idx] == "."):
        idx += 1
    if idx == 0:
        return None
    mult = {
        "": 1,
        "b": 1,
        "k": 1024,
        "kb": 1024,
        "kib": 1024,
        "m": MIB,
        "mb": MIB,
        "mib": MIB,
        "g": GIB,
        "gb": GIB,
        "gib": GIB,
    }.get(text[idx:].strip())
    if mult is None:
        return None
    try:
        return max(64 * MIB, int(float(text[:idx]) * mult))
    except ValueError:
        return None


def author_docker_flags(
    cpus: float,
    memory_mb: int,
    *,
    pids_limit: int | None = None,
    node_fallback: bool = False,
    cgroup_root: Path = Path("/sys/fs/cgroup"),
) -> list[str]:
    docker_host = os.environ.get("DOCKER_HOST", "").strip()
    if not docker_host:
        sock_path: Path | None = Path("/var/run/docker.sock")
    elif docker_host.startswith("unix://"):
        sock_path = Path(docker_host[len("unix://") :])
    elif docker_host.startswith("unix:"):
        sock_path = Path(docker_host[len("unix:") :])
    else:
        sock_path = None

    can_cpus = True
    can_memory = True
    can_pids = True
    cpu_ceiling_milli = max(1, len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else (os.cpu_count() or 2)) * 1000
    mem_ceiling_bytes = 64 * GIB
    meminfo = _read_trimmed(Path("/proc/meminfo"))
    if meminfo:
        for line in meminfo.splitlines():
            if line.startswith("MemTotal:"):
                parts = line.split()
                if len(parts) >= 2 and parts[1].isdigit():
                    mem_ceiling_bytes = min(mem_ceiling_bytes, int(parts[1]) * 1024)
                break
    pids_ceiling = 32768
    pid_max_txt = _read_trimmed(Path("/proc/sys/kernel/pid_max"))
    if pid_max_txt and pid_max_txt.isdigit():
        pids_ceiling = int(pid_max_txt)

    cred = _socket_peer_cred(sock_path) if sock_path is not None else None
    if cred is not None and cred[0] > 0:
        daemon_pid, daemon_uid = cred
        cg_text = _read_trimmed(Path(f"/proc/{daemon_pid}/cgroup")) or _read_trimmed(Path("/proc/self/cgroup")) or "0::/"
        if (cgroup_root / "cgroup.controllers").exists():
            rel = ""
            for line in cg_text.splitlines():
                if line.startswith("0::"):
                    rel = line[3:].strip()
                    if rel.endswith(" (deleted)"):
                        rel = rel[: -len(" (deleted)")]
                    rel = rel.lstrip("/")
                    break
            candidate = cgroup_root / rel if rel else cgroup_root
            daemon_cg = candidate if candidate.is_dir() else cgroup_root
            docker_cg = cgroup_root / "docker"

            dirs = _ancestor_chain(daemon_cg, cgroup_root)
            if docker_cg.is_dir() and docker_cg not in dirs:
                dirs.insert(0, docker_cg)
            for d in dirs:
                cpu_max = _read_trimmed(d / "cpu.max")
                if cpu_max:
                    parts = cpu_max.split()
                    if len(parts) >= 2 and parts[0] != "max" and parts[0].isdigit() and parts[1].isdigit() and int(parts[1]) > 0:
                        cpu_ceiling_milli = min(cpu_ceiling_milli, max(10, (int(parts[0]) * 1000) // int(parts[1])))
                mem_max = _read_trimmed(d / "memory.max")
                if mem_max and mem_max != "max" and mem_max.isdigit():
                    mem_ceiling_bytes = min(mem_ceiling_bytes, int(mem_max))
                p_max = _read_trimmed(d / "pids.max")
                if p_max and p_max != "max" and p_max.isdigit():
                    pids_ceiling = min(pids_ceiling, int(p_max))

            daemon_type = _read_trimmed(daemon_cg / "cgroup.type")
            if daemon_type and daemon_type != "domain":
                can_cpus = can_memory = can_pids = False
            elif docker_cg.is_dir():
                dtype = _read_trimmed(docker_cg / "cgroup.type")
                ctrls = set((_read_trimmed(docker_cg / "cgroup.controllers") or "").split())
                if dtype and dtype != "domain":
                    can_cpus = can_memory = can_pids = False
                else:
                    can_cpus = "cpu" in ctrls
                    can_memory = "memory" in ctrls
                    can_pids = "pids" in ctrls
            else:
                d_ctrls = set((_read_trimmed(daemon_cg / "cgroup.controllers") or "").split())
                if not d_ctrls and daemon_uid != 0:
                    can_cpus = can_memory = can_pids = False
                else:
                    is_unit = daemon_cg != cgroup_root and daemon_cg.name.endswith((".service", ".scope"))
                    parent_cg = daemon_cg.parent if is_unit else (cgroup_root if daemon_uid == 0 else daemon_cg)
                    ptype = _read_trimmed(parent_cg / "cgroup.type")
                    if ptype and ptype != "domain":
                        can_cpus = can_memory = can_pids = False
                    else:
                        avail = set((_read_trimmed(parent_cg / "cgroup.controllers") or "").split())
                        subtree = set((_read_trimmed(parent_cg / "cgroup.subtree_control") or "").split())
                        procs_empty = not bool(_read_trimmed(parent_cg / "cgroup.procs"))
                        can_cpus = "cpu" in avail and ("cpu" in subtree or procs_empty)
                        can_memory = "memory" in avail and ("memory" in subtree or procs_empty)
                        can_pids = "pids" in avail and ("pids" in subtree or procs_empty)

    req_milli = max(10, int(round(cpus * 1000)))
    env_cpus = os.environ.get("DEVKIT_CPUS", "").strip()
    if env_cpus:
        try:
            req_milli = max(100, int(round(float(env_cpus) * 1000)))
        except ValueError:
            pass
    target_milli = min(req_milli, cpu_ceiling_milli)

    reserve = min(max(256 * MIB, (mem_ceiling_bytes * 18) // 100), 2048 * MIB, mem_ceiling_bytes // 4)
    budget_bytes = max(256 * MIB, mem_ceiling_bytes - reserve)
    req_mem_bytes = memory_mb * MIB
    env_mem = os.environ.get("DEVKIT_MEMORY_LIMIT", "").strip()
    if env_mem:
        parsed_mem = _parse_mem_override(env_mem)
        if parsed_mem is not None:
            budget_bytes = min(parsed_mem, mem_ceiling_bytes)
            req_mem_bytes = min(req_mem_bytes, budget_bytes)
    target_mem_mb = max(64, min(req_mem_bytes, budget_bytes, mem_ceiling_bytes) // MIB)

    flags: list[str] = []
    if can_cpus:
        flags.extend(["--cpus", _format_cpus(target_milli), "--cpu-shares", "512"])
    if can_memory:
        flags.extend(["--memory", f"{target_mem_mb}m", "--memory-swap", f"{target_mem_mb}m"])
    elif node_fallback:
        heap_mb = max(256, min(2048, (target_mem_mb * 55) // 100))
        flags.extend(["--env", f"NODE_OPTIONS=--max-old-space-size={heap_mb}"])
    if can_pids and pids_limit is not None:
        flags.extend(["--pids-limit", str(max(64, min(pids_limit, pids_ceiling)))])
    return flags
