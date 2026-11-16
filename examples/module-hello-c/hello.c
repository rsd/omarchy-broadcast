/*
 * hello.c: an example omarchy-broadcast module written in C against the
 * module header alone (omarchy_broadcast_module.h), with no other library.
 *
 * It provides one stage, `x-example-hello-c` (the vendor form of a stage of
 * a module's own), which tints the picture warm: on host NV12 frames it
 * moves every chroma sample towards red and away from blue by an amount set
 * with the `strength` parameter (0 to 100, load time and live). Luma is
 * never touched.
 *
 * Build:
 *   cc -std=c11 -Wall -Wextra -Werror -shared -fPIC -O2 \
 *      -I <omarchy-broadcast>/crates/obc-module-abi/include \
 *      -o libobc_module_hello_c.so hello.c
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */

#include <omarchy_broadcast_module.h>

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define STAGE OBC_PRIVATE_STAGE_PREFIX "example-hello-c"
#define STRENGTH_KEY "strength"
#define DEFAULT_STRENGTH 50
/* How far, in 8-bit chroma steps, a strength of 100 moves each sample. */
#define MAX_SHIFT 24

/* The host, as given to obc_module_entry: valid until the library is
 * unloaded. */
static const obc_host *g_host;

/* The reason of the last failed init on this thread (last_error(NULL)). */
static _Thread_local char g_init_error[256];

struct obc_module {
    char error[256];
};

struct obc_instance {
    char error[256];
    int loaded;
    int strength;
    int full_range;
};

static void log_line(obc_log_level level, const char *msg)
{
    if (g_host != NULL && g_host->log != NULL) {
        g_host->log(g_host->user, level, msg);
    }
}

static obc_status fail(char *buf, size_t size, obc_status status, const char *why)
{
    snprintf(buf, size, "%s", why);
    return status;
}

static int clamp(int v, int lo, int hi)
{
    return v < lo ? lo : (v > hi ? hi : v);
}

/* The strength among `n` parameters, or `fallback`. A parameter of another
 * stage or key, or of another kind, is ignored. */
static int strength_of(const obc_param *params, size_t n, int fallback)
{
    for (size_t i = 0; params != NULL && i < n; i++) {
        const obc_param *p = &params[i];
        if (strcmp(p->stage, STAGE) == 0 && strcmp(p->key, STRENGTH_KEY) == 0
            && p->value.kind == OBC_VALUE_INT) {
            return clamp((int)p->value.data.i, 0, 100);
        }
    }
    return fallback;
}

static obc_status hello_init(const char *config_json, obc_module **out)
{
    (void)config_json;
    *out = NULL;
    obc_module *m = calloc(1, sizeof *m);
    if (m == NULL) {
        return fail(g_init_error, sizeof g_init_error, OBC_ERR_FAILED, "out of memory");
    }
    g_init_error[0] = '\0';
    log_line(OBC_LOG_DEBUG, "hello-c module initialised");
    *out = m;
    return OBC_OK;
}

static obc_status hello_probe(obc_module *module, const char **out_json)
{
    (void)module;
    *out_json = "{\"stages\":[{\"id\":\"" STAGE "\",\"available\":true}]}";
    return OBC_OK;
}

#define GROUP OBC_PRIVATE_GROUP_PREFIX "example_hello_c"

static obc_status hello_describe_settings(obc_module *module, const char **out_json)
{
    (void)module;
    *out_json = "{\"groups\":[{\"id\":\"" GROUP "\",\"label\":\"Hello module (C)\",\"settings\":["
                "{\"id\":\"effect." GROUP ".enabled\",\"label\":\"Warm tint\",\"type\":\"bool\","
                "\"default\":false,\"stage\":\"" STAGE "\",\"role\":\"enable\",\"hint\":\"switch\"},"
                "{\"id\":\"effect." GROUP ".strength\",\"label\":\"Strength\",\"type\":\"int\","
                "\"min\":0,\"max\":100,\"step\":5,\"default\":50,\"unit\":\"%\",\"stage\":\"" STAGE
                "\",\"key\":\"" STRENGTH_KEY "\",\"apply\":\"live\",\"hint\":\"slider\"}]}]}";
    return OBC_OK;
}

static obc_status hello_instance_create(obc_module *module, obc_instance **out)
{
    *out = calloc(1, sizeof **out);
    if (*out == NULL) {
        return fail(module->error, sizeof module->error, OBC_ERR_FAILED, "out of memory");
    }
    return OBC_OK;
}

static void hello_instance_destroy(obc_instance *instance)
{
    free(instance);
}

static obc_status hello_instance_load(obc_instance *inst, const obc_chain_request *req,
                                      obc_load_report *out_report)
{
    if (req == NULL || req->struct_size < OBC_CHAIN_REQUEST_SIZE_1_0 || out_report == NULL
        || out_report->struct_size < OBC_LOAD_REPORT_SIZE_1_0) {
        return fail(inst->error, sizeof inst->error, OBC_ERR_INVALID, "a request or report too small");
    }
    for (size_t i = 0; i < req->n_stages; i++) {
        if (strcmp(req->stages[i], STAGE) != 0) {
            return fail(inst->error, sizeof inst->error, OBC_ERR_INVALID, "unknown stage");
        }
    }
    if (req->input_kind != OBC_FRAME_HOST || req->input_format != OBC_FORMAT_NV12
        || req->output_format != OBC_FORMAT_NV12 || req->output_width != req->input_width
        || req->output_height != req->input_height) {
        return fail(inst->error, sizeof inst->error, OBC_ERR_UNSUPPORTED,
                    "the hello-c stage runs on host NV12 frames only, without scaling");
    }
    inst->strength = strength_of(req->params, req->n_params, DEFAULT_STRENGTH);
    inst->full_range = req->full_range != 0;
    inst->loaded = 1;
    /* The 1.0 fields only: they always fit in the host's struct_size. */
    out_report->load_ms = 0.0f;
    out_report->warmup_ms = -1.0f;
    out_report->reused_model = 0;
    return OBC_OK;
}

static int host_nv12(const obc_frame *f)
{
    return f != NULL && f->struct_size >= OBC_FRAME_SIZE_1_0 && f->kind == OBC_FRAME_HOST
           && f->format == OBC_FORMAT_NV12 && f->n_planes >= 2;
}

static uint8_t *plane(const obc_frame *f, int i)
{
    return (uint8_t *)(uintptr_t)(f->planes[i].ptr_or_fd + f->planes[i].offset);
}

static obc_status hello_instance_process(obc_instance *inst, const obc_frame *in,
                                         const obc_frame *out, const obc_param *live,
                                         size_t n_live)
{
    if (!inst->loaded) {
        return fail(inst->error, sizeof inst->error, OBC_ERR_STATE, "nothing is loaded");
    }
    if (!host_nv12(in) || !host_nv12(out)) {
        return fail(inst->error, sizeof inst->error, OBC_ERR_UNSUPPORTED,
                    "the hello-c stage runs on host NV12 frames only");
    }
    if (in->width != out->width || in->height != out->height) {
        return fail(inst->error, sizeof inst->error, OBC_ERR_INVALID, "the frame sizes differ");
    }
    inst->strength = strength_of(live, n_live, inst->strength);
    size_t w = out->width, h = out->height;
    if (plane(in, 0) != plane(out, 0)) {
        for (size_t p = 0; p < 2; p++) {
            size_t rows = p == 0 ? h : h / 2;
            for (size_t r = 0; r < rows; r++) {
                memcpy(plane(out, (int)p) + r * out->planes[p].pitch,
                       plane(in, (int)p) + r * in->planes[p].pitch, w);
            }
        }
    }
    int shift = (inst->strength * MAX_SHIFT + 50) / 100;
    int lo = inst->full_range ? 0 : 16, hi = inst->full_range ? 255 : 240;
    for (size_t r = 0; shift != 0 && r < h / 2; r++) {
        uint8_t *uv = plane(out, 1) + r * out->planes[1].pitch;
        for (size_t x = 0; x + 1 < w; x += 2) {
            uv[x] = (uint8_t)clamp(uv[x] - shift, lo, hi);
            uv[x + 1] = (uint8_t)clamp(uv[x + 1] + shift, lo, hi);
        }
    }
    return OBC_OK;
}

static obc_status hello_instance_reset_temporal(obc_instance *inst, const char **out_dropped_json)
{
    (void)inst;
    *out_dropped_json = "[]";
    return OBC_OK;
}

static void hello_instance_unload(obc_instance *inst)
{
    inst->loaded = 0;
}

static const char *hello_last_error(const obc_module *module)
{
    return module == NULL ? g_init_error : module->error;
}

static const char *hello_instance_last_error(const obc_instance *inst)
{
    return inst->error;
}

static void hello_destroy(obc_module *module)
{
    free(module);
}

/* The vtable of this header's ABI version. The entries the module does not
 * provide are null: `check`, the reserved `instance_park` and
 * `instance_unpark`, the audio entries (of another kind), the status values
 * and the per-frame background. */
static const obc_module_vtable VTABLE = {
    .struct_size = sizeof(obc_module_vtable),
    .abi_version = OBC_MODULE_ABI_VERSION,
    .init = hello_init,
    .probe = hello_probe,
    .describe_settings = hello_describe_settings,
    .instance_create = hello_instance_create,
    .instance_destroy = hello_instance_destroy,
    .instance_load = hello_instance_load,
    .instance_process = hello_instance_process,
    .instance_reset_temporal = hello_instance_reset_temporal,
    .instance_unload = hello_instance_unload,
    .last_error = hello_last_error,
    .instance_last_error = hello_instance_last_error,
    .destroy = hello_destroy,
};

uint32_t obc_module_abi_version(void)
{
    return OBC_MODULE_ABI_VERSION;
}

const obc_module_vtable *obc_module_entry(const obc_host *host)
{
    if (host == NULL || host->struct_size < OBC_HOST_SIZE_1_0
        || (host->abi_version >> 16) != OBC_MODULE_ABI_MAJOR) {
        return NULL;
    }
    g_host = host;
    return &VTABLE;
}
