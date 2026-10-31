#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <stdarg.h>
#include <unistd.h>
#include <sys/syscall.h>

#ifndef __NR_close_range
#define __NR_close_range 436
#endif

/*
 * Android < 14 seccomp compatibility shim:
 * In Android 12 and 13, the seccomp filter kills the process with SIGSYS (159)
 * when syscall(__NR_close_range) is called.
 * Intercepting syscall(__NR_close_range) and returning ENOSYS instructs Bun
 * to safely fall back to standard /proc/self/fd enumeration.
 */
int close_range(unsigned int first, unsigned int last, unsigned int flags) {
	errno = ENOSYS;
	return -1;
}

long syscall(long number, ...) {
	if (number == __NR_close_range) {
		errno = ENOSYS;
		return -1;
	}

	static long (*real_syscall)(long number, ...) = NULL;
	if (!real_syscall) {
		real_syscall = (long (*)(long, ...))dlsym(RTLD_NEXT, "syscall");
		if (!real_syscall) {
			errno = ENOSYS;
			return -1;
		}
	}

	va_list args;
	va_start(args, number);
	long a0 = va_arg(args, long);
	long a1 = va_arg(args, long);
	long a2 = va_arg(args, long);
	long a3 = va_arg(args, long);
	long a4 = va_arg(args, long);
	long a5 = va_arg(args, long);
	va_end(args);

	return real_syscall(number, a0, a1, a2, a3, a4, a5);
}
