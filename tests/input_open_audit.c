/* Test-only libc audit: report and deny attempts to open /dev/input.
 * This is not a production sandbox and cannot intercept direct syscalls.
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static const char active[] = "WISPR_TEST_INPUT_AUDIT_ACTIVE\n";
static const char denied[] = "WISPR_TEST_INPUT_OPEN_DENIED ";

__attribute__((constructor)) static void audit_loaded(void)
{
    (void)write(STDERR_FILENO, active, sizeof(active) - 1);
}

static int deny_input(const char *path)
{
    if (path && strncmp(path, "/dev/input", 10) == 0 &&
        (path[10] == '\0' || path[10] == '/')) {
        (void)write(STDERR_FILENO, denied, sizeof(denied) - 1);
        (void)write(STDERR_FILENO, path, strnlen(path, 4096));
        (void)write(STDERR_FILENO, "\n", 1);
        errno = EACCES;
        return 1;
    }
    return 0;
}

static mode_t open_mode(int flags, va_list args)
{
    if ((flags & O_CREAT) || ((flags & O_TMPFILE) == O_TMPFILE))
        return va_arg(args, mode_t);
    return 0;
}

#define AUDIT_OPEN(name)                                                \
    int name(const char *path, int flags, ...)                           \
    {                                                                  \
        if (deny_input(path)) return -1;                                \
        va_list args;                                                  \
        va_start(args, flags);                                          \
        mode_t mode = open_mode(flags, args);                           \
        va_end(args);                                                  \
        int (*original)(const char *, int, ...) = dlsym(RTLD_NEXT, #name); \
        return original(path, flags, mode);                             \
    }

#define AUDIT_OPENAT(name)                                              \
    int name(int directory, const char *path, int flags, ...)            \
    {                                                                  \
        if (deny_input(path)) return -1;                                \
        va_list args;                                                  \
        va_start(args, flags);                                          \
        mode_t mode = open_mode(flags, args);                           \
        va_end(args);                                                  \
        int (*original)(int, const char *, int, ...) =                   \
            dlsym(RTLD_NEXT, #name);                                    \
        return original(directory, path, flags, mode);                  \
    }

AUDIT_OPEN(open)
AUDIT_OPEN(open64)
AUDIT_OPENAT(openat)
AUDIT_OPENAT(openat64)

DIR *opendir(const char *path)
{
    if (deny_input(path)) return NULL;
    DIR *(*original)(const char *) = dlsym(RTLD_NEXT, "opendir");
    return original(path);
}

FILE *fopen(const char *path, const char *mode)
{
    if (deny_input(path)) return NULL;
    FILE *(*original)(const char *, const char *) = dlsym(RTLD_NEXT, "fopen");
    return original(path, mode);
}

FILE *fopen64(const char *path, const char *mode)
{
    if (deny_input(path)) return NULL;
    FILE *(*original)(const char *, const char *) = dlsym(RTLD_NEXT, "fopen64");
    return original(path, mode);
}
