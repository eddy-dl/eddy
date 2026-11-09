// Eddy (iOS): runs a script in QuickJS-ng, the fallback for when
// JavaScriptCore can't (see src/platform/ios_python.rs). What it prints with
// console.log is its output, console.error its errors. Compiled with
// QuickJS-ng's own files by build.rs.

#include <stdlib.h>
#include <string.h>

#include "quickjs.h"

typedef struct {
    char *data;
    size_t length, capacity;
} Buffer;

static void buffer_add(Buffer *buffer, const char *text, size_t length) {
    if (buffer->length + length + 1 > buffer->capacity) {
        size_t capacity = buffer->capacity ? buffer->capacity : 4096;
        while (capacity < buffer->length + length + 1) {
            capacity *= 2;
        }
        char *data = realloc(buffer->data, capacity);
        if (!data) {
            return;
        }
        buffer->data = data;
        buffer->capacity = capacity;
    }
    memcpy(buffer->data + buffer->length, text, length);
    buffer->length += length;
    buffer->data[buffer->length] = 0;
}

// console.log and friends: magic 0 prints to the output, 1 to the errors.
static JSValue console_print(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv, int magic) {
    Buffer *buffers = JS_GetContextOpaque(ctx);
    Buffer *target = &buffers[magic ? 1 : 0];
    for (int i = 0; i < argc; i++) {
        size_t length;
        const char *text = JS_ToCStringLen(ctx, &length, argv[i]);
        if (!text) {
            return JS_EXCEPTION;
        }
        if (i) {
            buffer_add(target, " ", 1);
        }
        buffer_add(target, text, length);
        JS_FreeCString(ctx, text);
    }
    buffer_add(target, "\n", 1);
    return JS_UNDEFINED;
}

// The pending exception, with its stack, into the errors.
static void report_exception(JSContext *ctx, Buffer *errors) {
    JSValue exception = JS_GetException(ctx);
    const char *text = JS_ToCString(ctx, exception);
    if (text) {
        buffer_add(errors, text, strlen(text));
        buffer_add(errors, "\n", 1);
        JS_FreeCString(ctx, text);
    }
    JSValue stack = JS_GetPropertyStr(ctx, exception, "stack");
    if (!JS_IsUndefined(stack)) {
        const char *trace = JS_ToCString(ctx, stack);
        if (trace) {
            buffer_add(errors, trace, strlen(trace));
            JS_FreeCString(ctx, trace);
        }
    }
    JS_FreeValue(ctx, stack);
    JS_FreeValue(ctx, exception);
}

static char *take(Buffer *buffer) {
    return buffer->data ? buffer->data : strdup("");
}

// Runs `source`; 0 when it ran without an exception. *out and *err are its
// output and errors, to free with eddy_qjs_free.
int eddy_qjs_eval(const char *source, size_t length, char **out, char **err) {
    Buffer buffers[2] = {{0}};
    int status = 0;
    JSRuntime *rt = JS_NewRuntime();
    if (!rt) {
        return -1;
    }
    // The caller's thread has a big stack for this (the solver recurses
    // deeply); a memory cap keeps a runaway script from taking the phone's.
    JS_SetMaxStackSize(rt, 48 << 20);
    JS_SetMemoryLimit(rt, 512 << 20);
    JSContext *ctx = JS_NewContext(rt);
    if (!ctx) {
        JS_FreeRuntime(rt);
        return -1;
    }
    JS_SetContextOpaque(ctx, buffers);

    JSValue global = JS_GetGlobalObject(ctx);
    JSValue console = JS_NewObject(ctx);
    const char *to_output[] = {"log", "info", "debug"};
    for (size_t i = 0; i < sizeof to_output / sizeof *to_output; i++) {
        JS_SetPropertyStr(ctx, console, to_output[i],
                          JS_NewCFunctionMagic(ctx, console_print, to_output[i], 1, JS_CFUNC_generic_magic, 0));
    }
    const char *to_errors[] = {"error", "warn"};
    for (size_t i = 0; i < sizeof to_errors / sizeof *to_errors; i++) {
        JS_SetPropertyStr(ctx, console, to_errors[i],
                          JS_NewCFunctionMagic(ctx, console_print, to_errors[i], 1, JS_CFUNC_generic_magic, 1));
    }
    JS_SetPropertyStr(ctx, global, "console", console);
    JS_FreeValue(ctx, global);

    JSValue result = JS_Eval(ctx, source, length, "<eddy>", JS_EVAL_TYPE_GLOBAL);
    if (JS_IsException(result)) {
        report_exception(ctx, &buffers[1]);
        status = 1;
    }
    JS_FreeValue(ctx, result);
    // Promises the script left.
    for (;;) {
        JSContext *job;
        int ran = JS_ExecutePendingJob(rt, &job);
        if (ran <= 0) {
            if (ran < 0) {
                report_exception(job, &buffers[1]);
                status = 1;
            }
            break;
        }
    }

    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
    *out = take(&buffers[0]);
    *err = take(&buffers[1]);
    return status;
}

void eddy_qjs_free(char *text) {
    free(text);
}
