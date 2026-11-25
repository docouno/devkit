// Use the SDK definitions for kernel structures absent from Rust's libc bindings.
#include <errno.h>
#include <libproc.h>
#include <mach-o/dyld.h>
#include <stdint.h>
#include <sys/proc.h>
#include <sys/proc_info.h>
#include <sys/stat.h>
#include <sys/sysctl.h>
#include <unistd.h>

int devkit_sysctl_process(pid_t pid, uint32_t *parent, uint64_t *start) {
    int mib[] = {CTL_KERN, KERN_PROC, KERN_PROC_PID, pid};
    struct kinfo_proc entry;
    size_t size = sizeof(entry);
    errno = 0;
    if (sysctl(mib, 4, &entry, &size, NULL, 0) == -1)
        return errno ? errno : EIO;
    if (size != sizeof(entry) || entry.kp_proc.p_pid != pid ||
        entry.kp_proc.p_stat == SZOMB)
        return ESRCH;
    *parent = entry.kp_eproc.e_ppid;
    *start = (uint64_t)entry.kp_proc.p_starttime.tv_sec * 1000000 +
             entry.kp_proc.p_starttime.tv_usec;
    return 0;
}

int devkit_mapped_executable_identity(uint32_t *device, uint64_t *inode) {
    uintptr_t header = (uintptr_t)_dyld_get_image_header(0);
    struct proc_regionwithpathinfo info;
    if (!header)
        return ENOEXEC;
    errno = 0;
    int size = proc_pidinfo(getpid(), PROC_PIDREGIONPATHINFO, header, &info, sizeof(info));
    if (size != sizeof(info))
        return size <= 0 && errno ? errno : EIO;
    if (header < info.prp_prinfo.pri_address ||
        header - info.prp_prinfo.pri_address >= info.prp_prinfo.pri_size ||
        !S_ISREG(info.prp_vip.vip_vi.vi_stat.vst_mode))
        return ENOEXEC;
    *device = info.prp_vip.vip_vi.vi_stat.vst_dev;
    *inode = info.prp_vip.vip_vi.vi_stat.vst_ino;
    return 0;
}

int devkit_volume_case_sensitive(const char *path) {
    errno = 0;
    long value = pathconf(path, _PC_CASE_SENSITIVE);
    if (value == 1)
        return 1;
    if (value == 0)
        return 0;
    return -1;
}
