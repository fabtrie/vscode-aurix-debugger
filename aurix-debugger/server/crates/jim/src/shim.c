/* C side of the Rust bindings: keeps Jim's struct layout and macros out of Rust. */
#include <stdlib.h>
#include "jim.h"

extern int rj_rust_dispatch(void *data, Jim_Interp *interp, int argc, const char **argv, const int *lens);
extern void rj_rust_free(void *data);

Jim_Interp *rj_create(void)
{
    Jim_Interp *interp = Jim_CreateInterp();
    Jim_RegisterCoreCommands(interp);
    Jim_InitStaticExtensions(interp);
    return interp;
}

void rj_free(Jim_Interp *interp)
{
    Jim_FreeInterp(interp);
}

int rj_eval(Jim_Interp *interp, const char *script, int global)
{
    return global ? Jim_EvalGlobal(interp, script) : Jim_Eval(interp, script);
}

int rj_eval_file(Jim_Interp *interp, const char *path)
{
    return Jim_EvalFileGlobal(interp, path);
}

const char *rj_result(Jim_Interp *interp, int *len)
{
    return Jim_GetString(Jim_GetResult(interp), len);
}

/* Replaces the result with an error message including the Tcl stack trace. */
const char *rj_error_message(Jim_Interp *interp, int *len)
{
    Jim_MakeErrorMessage(interp);
    return Jim_GetString(Jim_GetResult(interp), len);
}

void rj_set_result(Jim_Interp *interp, const char *s, int len)
{
    Jim_SetResultString(interp, s, len);
}

static int rj_cmd_proc(Jim_Interp *interp, int argc, Jim_Obj *const *argv)
{
    const char *sv_small[16];
    int lv_small[16];
    const char **sv = sv_small;
    int *lv = lv_small;
    int i, rc;

    if (argc > 16) {
        sv = malloc(sizeof(*sv) * argc);
        lv = malloc(sizeof(*lv) * argc);
    }
    for (i = 0; i < argc; i++)
        sv[i] = Jim_GetString(argv[i], &lv[i]);
    rc = rj_rust_dispatch(Jim_CmdPrivData(interp), interp, argc, sv, lv);
    if (argc > 16) {
        free(sv);
        free(lv);
    }
    return rc;
}

static void rj_del_proc(Jim_Interp *interp, void *data)
{
    (void)interp;
    rj_rust_free(data);
}

int rj_create_command(Jim_Interp *interp, const char *name, void *data)
{
    return Jim_CreateCommand(interp, name, rj_cmd_proc, data, rj_del_proc);
}

int rj_delete_command(Jim_Interp *interp, const char *name)
{
    return Jim_DeleteCommand(interp, Jim_NewStringObj(interp, name, -1));
}

int rj_set_global(Jim_Interp *interp, const char *name, const char *value, int len)
{
    return Jim_SetGlobalVariableStr(interp, name, Jim_NewStringObj(interp, value, len));
}

const char *rj_get_global(Jim_Interp *interp, const char *name, int *len)
{
    Jim_Obj *o = Jim_GetGlobalVariableStr(interp, name, JIM_NONE);
    if (!o)
        return NULL;
    return Jim_GetString(o, len);
}
