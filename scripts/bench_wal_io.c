/* Optional Linux/glibc I/O counter for bench_wal_open.py (Beads #598).
 *
 * Build: cc -shared -fPIC -O2 -Wall -Wextra -o libwal_io_counter.so \
 *           scripts/bench_wal_io.c -ldl -pthread
 * Use only in separate, untimed diagnostic runs. The readlink and counting
 * overhead deliberately makes this unsuitable for latency measurements.
 * Counts successful pread/pread64 returns (including short reads and EOF) on
 * basenames ending in "-wal", with failed requests reported separately. It
 * does not emulate I/O, change bytes, or require ptrace. WAL_IO_OUTPUT names a new
 * JSON result file. No file is written unless that variable is present.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <limits.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <unistd.h>

struct bucket {
    size_t requested;
    uint64_t calls;
};

static struct bucket buckets[128];
static size_t bucket_count;
static uint64_t returned_bytes;
static uint64_t frame_zero_reads;
static uint64_t header_reads;
static uint64_t overflow_calls;
static uint64_t failed_calls;
static pthread_mutex_t counts_lock = PTHREAD_MUTEX_INITIALIZER;
static ssize_t (*next_pread)(int, void *, size_t, off_t);
static ssize_t (*next_pread64)(int, void *, size_t, off64_t);
static const char *output_path;

__attribute__((constructor)) static void initialize(void) {
    *(void **)(&next_pread) = dlsym(RTLD_NEXT, "pread");
    *(void **)(&next_pread64) = dlsym(RTLD_NEXT, "pread64");
    output_path = getenv("WAL_IO_OUTPUT");
    if (!next_pread || !next_pread64) {
        static const char message[] = "WAL counter cannot resolve libc pread functions\n";
        (void)write(STDERR_FILENO, message, sizeof(message) - 1);
        _exit(125);
    }
}

static int is_wal_path(const char *path) {
    const char *name = strrchr(path, '/');
    name = name ? name + 1 : path;
    size_t length = strlen(name);
    static const char deleted_suffix[] = " (deleted)";
    const size_t deleted_length = sizeof(deleted_suffix) - 1;
    if (length >= deleted_length &&
        memcmp(name + length - deleted_length, deleted_suffix, deleted_length) == 0) {
        length -= deleted_length;
    }
    return length >= 4 && memcmp(name + length - 4, "-wal", 4) == 0;
}

static void observe(int fd, size_t requested, int64_t offset, ssize_t returned) {
    if (!output_path) {
        return;
    }
    char descriptor[64];
    char path[PATH_MAX + 1];
    (void)snprintf(descriptor, sizeof(descriptor), "/proc/self/fd/%d", fd);
    ssize_t length = readlink(descriptor, path, sizeof(path) - 1);
    if (length < 0) {
        return;
    }
    path[length] = '\0';
    if (!is_wal_path(path)) {
        return;
    }
    pthread_mutex_lock(&counts_lock);
    if (returned < 0) {
        failed_calls++;
        pthread_mutex_unlock(&counts_lock);
        return;
    }
    size_t index;
    for (index = 0; index < bucket_count; index++) {
        if (buckets[index].requested == requested) {
            break;
        }
    }
    if (index == sizeof(buckets) / sizeof(buckets[0])) {
        overflow_calls++;
    } else {
        if (index == bucket_count) {
            buckets[index].requested = requested;
            bucket_count++;
        }
        buckets[index].calls++;
    }
    returned_bytes += (uint64_t)returned;
    frame_zero_reads += offset == 32;
    header_reads += offset == 0;
    pthread_mutex_unlock(&counts_lock);
}

ssize_t pread(int fd, void *buffer, size_t count, off_t offset) {
    ssize_t result = next_pread(fd, buffer, count, offset);
    int saved_errno = errno;
    observe(fd, count, (int64_t)offset, result);
    errno = saved_errno;
    return result;
}

ssize_t pread64(int fd, void *buffer, size_t count, off64_t offset) {
    ssize_t result = next_pread64(fd, buffer, count, offset);
    int saved_errno = errno;
    observe(fd, count, (int64_t)offset, result);
    errno = saved_errno;
    return result;
}

__attribute__((destructor)) static void finish(void) {
    if (!output_path) {
        return;
    }
    /* Refuse to overwrite another run's evidence. */
    int descriptor = open(output_path, O_WRONLY | O_CREAT | O_EXCL, 0600);
    if (descriptor < 0) {
        perror("WAL counter output");
        return;
    }
    FILE *out = fdopen(descriptor, "w");
    if (!out) {
        close(descriptor);
        return;
    }
    pthread_mutex_lock(&counts_lock);
    fprintf(out, "{\"pread_calls_by_requested_bytes\":{");
    for (size_t index = 0; index < bucket_count; index++) {
        fprintf(out, "%s\"%zu\":%" PRIu64,
                index ? "," : "", buckets[index].requested, buckets[index].calls);
    }
    fprintf(out, "},\"pread_bytes_returned\":%" PRIu64
                 ",\"frame_zero_reads\":%" PRIu64
                 ",\"header_reads\":%" PRIu64
                 ",\"overflow_calls\":%" PRIu64
                 ",\"pread_failed_calls\":%" PRIu64 "}\n",
            returned_bytes, frame_zero_reads, header_reads, overflow_calls, failed_calls);
    pthread_mutex_unlock(&counts_lock);
    fclose(out);
}
