"""Platform names used by native release and consumer verification."""
import platform


LINUX_X64 = "x86_64-unknown-linux-musl"
LINUX_ARM64 = "aarch64-unknown-linux-musl"
DARWIN_ARM64 = "aarch64-apple-darwin"


def host_target():
    system, machine = platform.system(), platform.machine()
    try:
        return {
            ("Linux", "x86_64"): LINUX_X64,
            ("Linux", "aarch64"): LINUX_ARM64,
            ("Linux", "arm64"): LINUX_ARM64,
            ("Darwin", "arm64"): DARWIN_ARM64,
        }[system, machine]
    except KeyError:
        raise RuntimeError(f"Unsupported verification host {system} {machine}") from None


def linux_arch():
    """Docker architecture on native Linux verification runners."""
    return {LINUX_X64: "amd64", LINUX_ARM64: "arm64"}[host_target()]


def payload_name(target):
    return "devkit" if target == LINUX_X64 else "devkit-" + target


def payload_engine(payload):
    return payload / payload_name(host_target())


def engine_asset(index, target=None):
    engines = index.get("engines", {index["target"]: index["engine"]})
    return engines[target or host_target()]


def executable_header_matches(header, target):
    if target == DARWIN_ARM64:
        return (len(header) >= 32 and header[:4] == b"\xcf\xfa\xed\xfe"
                and int.from_bytes(header[4:8], "little") == 0x0100000C
                and int.from_bytes(header[12:16], "little") == 2)
    machine = {LINUX_X64: 62, LINUX_ARM64: 183}[target]
    return (len(header) >= 64 and header[:7] == b"\x7fELF\x02\x01\x01"
            and int.from_bytes(header[16:18], "little") in (2, 3)
            and int.from_bytes(header[18:20], "little") == machine)
