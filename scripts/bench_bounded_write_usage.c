/* Native measurement supervisor for bench_bounded_write.py (#619).
 *
 * Build: cc -std=c11 -O2 -Wall -Wextra -Werror -o usage-helper \
 *           scripts/bench_bounded_write_usage.c
 * Usage: usage-helper /absolute/new-usage.json COMMAND [ARG ...]
 *
 * A fresh exec of this small supervisor precedes fork. This matters on Linux:
 * wait4 on a child forked directly from the Python campaign can include the
 * Python parent's resident memory before exec, creating an artificial RSS
 * floor that grows with the retained fixture/verification data.
 *
 * Only the forked command's wait4 usage is reported. The supervisor's startup
 * and the Python campaign's memory/CPU are excluded. Wall time starts just
 * before fork and ends after reaping the child. stdout/stderr and process-group
 * membership are inherited; the campaign owns group cancellation/timeouts.
 *
 * The receipt is created exclusively and never replaces an earlier result.
 * Exit status is the child's status (128 + signal when signaled); supervisor
 * errors use 125. An incomplete/missing receipt must invalidate the sample.
 */
#define _GNU_SOURCE
#define _DARWIN_C_SOURCE
#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/resource.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

enum { MEASUREMENT_ERROR = 125 };

static uint64_t monotonic_ns(const struct timespec *value) {
    return (uint64_t)value->tv_sec * UINT64_C(1000000000) +
           (uint64_t)value->tv_nsec;
}

static double seconds(const struct timeval *value) {
    return (double)value->tv_sec + (double)value->tv_usec / 1000000.0;
}

int main(int argc, char **argv) {
    if (argc < 3 || argv[1][0] != '/') {
        fputs("usage: usage-helper /absolute/new-usage.json COMMAND [ARG ...]\n",
              stderr);
        return MEASUREMENT_ERROR;
    }

    int receipt_fd = open(argv[1], O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
    if (receipt_fd < 0) {
        perror("usage-helper: create exclusive receipt");
        return MEASUREMENT_ERROR;
    }

    struct timespec started;
    if (clock_gettime(CLOCK_MONOTONIC, &started) != 0) {
        perror("usage-helper: start clock");
        close(receipt_fd);
        return MEASUREMENT_ERROR;
    }
    pid_t child = fork();
    if (child < 0) {
        perror("usage-helper: fork");
        close(receipt_fd);
        return MEASUREMENT_ERROR;
    }
    if (child == 0) {
        close(receipt_fd);
        execvp(argv[2], &argv[2]);
        perror("usage-helper: exec command");
        _exit(127);
    }

    int status = 0;
    struct rusage usage = {0};
    pid_t waited;
    do {
        waited = wait4(child, &status, 0, &usage);
    } while (waited < 0 && errno == EINTR);
    if (waited < 0) {
        perror("usage-helper: wait4");
        /* Do not leave the measured command running after a supervisor error. */
        (void)kill(child, SIGKILL);
        while (waitpid(child, NULL, 0) < 0 && errno == EINTR) {
        }
        close(receipt_fd);
        return MEASUREMENT_ERROR;
    }

    struct timespec stopped;
    if (clock_gettime(CLOCK_MONOTONIC, &stopped) != 0) {
        perror("usage-helper: stop clock");
        close(receipt_fd);
        return MEASUREMENT_ERROR;
    }

    int terminating_signal = WIFSIGNALED(status) ? WTERMSIG(status) : 0;
    int exit_code = WIFEXITED(status) ? WEXITSTATUS(status) :
                    (terminating_signal ? 128 + terminating_signal : MEASUREMENT_ERROR);
    uint64_t start_ns = monotonic_ns(&started);
    uint64_t stop_ns = monotonic_ns(&stopped);
    uint64_t peak_rss_bytes = (uint64_t)usage.ru_maxrss;
#if !defined(__APPLE__)
    peak_rss_bytes *= UINT64_C(1024);
#endif
    double user_seconds = seconds(&usage.ru_utime);
    double system_seconds = seconds(&usage.ru_stime);

    FILE *receipt = fdopen(receipt_fd, "w");
    if (receipt == NULL) {
        perror("usage-helper: fdopen receipt");
        close(receipt_fd);
        return MEASUREMENT_ERROR;
    }
    int written = fprintf(receipt,
        "{\"schema\":\"br.bounded-write-usage.v1\","
        "\"start_ns\":%" PRIu64 ",\"stop_ns\":%" PRIu64 ","
        "\"wall_seconds\":%.9f,\"user_seconds\":%.9f,"
        "\"system_seconds\":%.9f,\"cpu_seconds\":%.9f,"
        "\"peak_rss_bytes\":%" PRIu64 ","
        "\"minor_faults\":%ld,\"major_faults\":%ld,"
        "\"input_blocks\":%ld,\"output_blocks\":%ld,"
        "\"exit_code\":%d,\"terminating_signal\":%d}\n",
        start_ns, stop_ns, (double)(stop_ns - start_ns) / 1000000000.0,
        user_seconds, system_seconds, user_seconds + system_seconds,
        peak_rss_bytes, usage.ru_minflt, usage.ru_majflt, usage.ru_inblock,
        usage.ru_oublock, exit_code, terminating_signal);
    if (written < 0) {
        perror("usage-helper: write receipt");
        (void)fclose(receipt);
        return MEASUREMENT_ERROR;
    }
    if (fclose(receipt) != 0) {
        perror("usage-helper: close receipt");
        return MEASUREMENT_ERROR;
    }
    return exit_code;
}
