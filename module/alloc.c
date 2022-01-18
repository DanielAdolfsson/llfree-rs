#include <linux/completion.h>
#include <linux/delay.h>
#include <linux/gfp.h>
#include <linux/kernel.h>
#include <linux/kthread.h>
#include <linux/module.h>
#include <linux/slab.h>

MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Kernel Alloc Test");
MODULE_AUTHOR("Lars Wrenger");

#define MOD "[" KBUILD_MODNAME "]: "

#ifndef NUM_ALLOCS
#define NUM_ALLOCS (2 * 512 * 512)
#endif

#ifndef CPU_STRIDE
#define CPU_STRIDE 2
#endif

#ifndef THREADS_MAX
#define THREADS_MAX 6UL
#elif THREADS_MAX > 48
#error "THREADS_MAX cannot be larger then 48"
#endif

#ifndef ITERATIONS
#define ITERATIONS 4
#endif

static const u64 threads[] = {1,  2,  4,  6,  8,  12, 16, 20,
                              24, 28, 32, 36, 40, 44, 48};
#define THREADS_LEN (sizeof(threads) / sizeof(*threads))

static struct task_struct *tasks[THREADS_MAX];
static DECLARE_COMPLETION(start_barrier);
static DECLARE_COMPLETION(mid_barrier);
static struct completion barriers[THREADS_MAX];

struct thread_perf {
    atomic64_t get;
    atomic64_t put;
};
static struct thread_perf thread_perf[THREADS_MAX];

struct perf {
    u64 get_min;
    u64 get_avg;
    u64 get_max;
    u64 put_min;
    u64 put_avg;
    u64 put_max;
};
static struct perf perf[ITERATIONS * THREADS_LEN];

static ssize_t out_show(struct kobject *kobj, struct kobj_attribute *attr,
                        char *buf) {
    ssize_t i, iter;
    struct perf *p;
    ssize_t len = sprintf(buf, "alloc,threads,iteration,get_min,get_avg,"
                               "get_max,put_min,put_avg,put_max,total\n");

    for (i = 0; threads[i] <= THREADS_MAX; i++) {
        for (iter = 0; iter < ITERATIONS; iter++) {
            p = &perf[i * ITERATIONS + iter];
            len += sprintf(
                buf + len,
                "KernelAlloc,%llu,%lu,%llu,%llu,%llu,%llu,%llu,%llu,0\n",
                threads[i], iter, p->get_min, p->get_avg, p->get_max,
                p->put_min, p->put_avg, p->put_max);
        }
    }
    return len;
}

static struct kobj_attribute out_attribute = __ATTR(out, 0444, out_show, NULL);

static struct attribute *attrs[] = {
    &out_attribute.attr,
    NULL, /* need to NULL terminate the list of attributes */
};

static struct attribute_group attr_group = {
    .attrs = attrs,
};
static struct kobject *output;

static u64 cycles(void) {
    u32 lo, hi;
    asm volatile("rdtsc" : "=eax"(lo), "=edx"(hi) :);
    return ((u64)lo) | ((u64)hi) << 32;
};

static int worker(void *data) {
    u64 j;
    u64 tid = (u64)data;
    u64 timer;

#ifndef REALLOC

    struct page **pages =
        kmalloc_array(NUM_ALLOCS, sizeof(struct page *), GFP_KERNEL);

    printk(KERN_INFO MOD "Worker %llu\n", tid);

    if (pages == NULL) {
        printk(KERN_ERR MOD "kmalloc failed");
        return -1;
    }

    // complete initialization
    complete(&barriers[tid]);

    // Start allocations
    wait_for_completion(&start_barrier);

    timer = cycles();
    for (j = 0; j < NUM_ALLOCS; j++) {
        pages[j] = alloc_page(GFP_USER);
        if (pages == NULL) {
            printk(KERN_ERR MOD "alloc_page failed");
        }
    }
    timer = (cycles() - timer) / NUM_ALLOCS;
    atomic64_set(&thread_perf[tid].get, timer);
    printk(KERN_INFO MOD "Alloc %llu\n", timer);

    complete(&barriers[tid]);

    // Start frees
    wait_for_completion(&mid_barrier);

    timer = cycles();
    for (j = 0; j < NUM_ALLOCS; j++) {
        __free_page(pages[j]);
    }
    timer = (cycles() - timer) / NUM_ALLOCS;
    atomic64_set(&thread_perf[tid].put, timer);
    printk(KERN_INFO MOD "Free %llu\n", timer);

#else // REALLOC

    struct page *page;

    // complete initialization
    complete(&barriers[tid]);

    // Start reallocs
    wait_for_completion(&mid_barrier);

    timer = cycles();
    for (j = 0; j < NUM_ALLOCS; j++) {
        page = alloc_page(GFP_USER);
        if (page == NULL) {
            printk(KERN_ERR MOD "alloc_page failed");
        }
        __free_page(page);
    }
    timer = (cycles() - timer) / NUM_ALLOCS;
    atomic64_set(&thread_perf[tid].get, timer);
    atomic64_set(&thread_perf[tid].put, timer);
    printk(KERN_INFO MOD "Realloc %llu\n", timer);

#endif // REALLOC

    complete(&barriers[tid]);

    return 0;
}

static int alloc_init_module(void) {
    int retval;
    u64 i, t, get, put, iter;
    struct perf *p;

    printk(KERN_INFO MOD "Init\n");

    output = kobject_create_and_add(KBUILD_MODNAME, kernel_kobj);
    if (!output) {
        printk(KERN_ERR MOD "KObj failed\n");
        return -ENOMEM;
    }

    retval = sysfs_create_group(output, &attr_group);
    if (retval) {
        printk(KERN_ERR MOD "Sysfs failed\n");
        kobject_put(output);
    }

    for (i = 0; threads[i] <= THREADS_MAX; i++) {
        for (iter = 0; iter < ITERATIONS; iter++) {
            printk(KERN_INFO MOD "Start threads %llu\n", threads[i]);
            for (t = 0; t < threads[i]; t++) {
                tasks[t] = kthread_create(worker, (void *)t, "worker");
                if (IS_ERR(tasks[t])) {
                    printk(KERN_ERR MOD "Unable to init %llu\n", t);
                    return PTR_ERR(tasks[t]);
                }
                kthread_bind(tasks[t], CPU_STRIDE * t);
                init_completion(&barriers[t]);
                wake_up_process(tasks[t]);
            }

            // Init
            for (t = 0; t < threads[i]; t++) {
                wait_for_completion(&barriers[t]);
                reinit_completion(&barriers[t]);
            }

            printk(KERN_INFO MOD "Exec %llu threads\n", threads[i]);
            p = &perf[i * ITERATIONS + iter];
            p->get_min = (u64)-1;
            p->get_avg = 0;
            p->get_max = 0;
            p->put_min = (u64)-1;
            p->put_avg = 0;
            p->put_max = 0;

#ifndef REALLOC
            // Alloc
            complete_all(&start_barrier);

            printk(KERN_INFO MOD "Waiting for workers...\n");

            for (t = 0; t < threads[i]; t++) {
                wait_for_completion(&barriers[t]);
                reinit_completion(&barriers[t]);
            }
#endif // REALLOC

            // Free
            complete_all(&mid_barrier);

            for (t = 0; t < threads[i]; t++) {
                wait_for_completion(&barriers[t]);
                reinit_completion(&barriers[t]);
                get = atomic64_read(&thread_perf[t].get);
                put = atomic64_read(&thread_perf[t].put);

                p->get_min = min(p->get_min, get);
                p->get_avg += get;
                p->get_max = max(p->get_max, get);
                p->put_min = min(p->put_min, put);
                p->put_avg += put;
                p->put_max = max(p->put_max, put);
            }
            p->get_avg /= threads[i];
            p->put_avg /= threads[i];

            reinit_completion(&start_barrier);
            reinit_completion(&mid_barrier);
        }
    }

    printk(KERN_INFO MOD "Finished\n");

    return 0;
}

static void alloc_cleanup_module(void) {
    printk(KERN_INFO MOD "End\n");
    kobject_put(output);
}

module_init(alloc_init_module);
module_exit(alloc_cleanup_module);