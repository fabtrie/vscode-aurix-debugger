//! Diagnostic probe for `CDataController2::getExpressionType`.
//!
//! `ExpressionController::children()` returned zero members for a global the SDK
//! itself reports as an 8-element array, which cannot be right. This isolates
//! the cause by sweeping the two things that could plausibly matter:
//!
//!   * the undocumented `flags` argument (the wrapper passes 0), and
//!   * how the expression is spelled (bare name, qualified name, `&name`,
//!     `name[0]`).
//!
//! For each combination it reports whether a type object came back, whether it
//! had an `Expression()` variable, and how many `Children()` it exposed. The
//! goal is a single line that shows non-zero children — that combination is
//! what the wrapper should use.
//!
//! It also answers the mirror-image question: a **scalar shown as a container**
//! in the picker, which is not an unfolding failure but a *classification* one.
//! The picker judges a root node with an allow-list of scalar type-name
//! spellings (`is_scalar_type`, `backend/src/winidea/worker.rs`), fed by
//! `CVariable_getType`. A spelling the list does not know (`unsigned char`
//! vs `uint8`, a qualifier like `volatile`, a typedef) is called compound, which
//! gives it a twisty and — since only scalars are acquirable — makes it
//! unselectable. `--list-nonscalar` prints every global that lands there, and
//! `--symbol` prints the data-controller type string next to the expression
//! controller's, since the two accessors need not agree.
//!
//! Run:
//! ```text
//! cargo run --example unfold_probe --profile dev-release -- \
//!   --workspace <ws.xjrf> [--symbol <global>] [--list-nonscalar] [--dll <path>]
//! ```

use std::ffi::{CStr, CString, c_void};
use std::path::PathBuf;

use winidea_sdk::{ConnectionMgr, WinIdea, data::DataController};

fn main() -> std::process::ExitCode {
    let mut dll: Option<PathBuf> = None;
    let mut workspace: Option<String> = None;
    let mut symbol: Option<String> = None;

    let mut list_nonscalar = false;
    let mut batch_probe: Option<usize> = None;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dll" => dll = it.next().map(PathBuf::from),
            "--workspace" | "-w" => workspace = it.next(),
            "--symbol" | "-s" => symbol = it.next(),
            "--list-nonscalar" => list_nonscalar = true,
            "--batch-probe" => batch_probe = it.next().and_then(|n| n.parse().ok()).or(Some(200)),
            other => {
                eprintln!("unknown argument `{other}`");
                return std::process::ExitCode::FAILURE;
            }
        }
    }

    let Some(workspace) = workspace else {
        eprintln!("--workspace is required");
        return std::process::ExitCode::FAILURE;
    };

    let winidea = match dll {
        Some(p) => WinIdea::load_from(p.as_os_str()),
        None => WinIdea::load(&winidea_sdk::default_search_paths()),
    };
    let winidea = match winidea {
        Ok(w) => w,
        Err(e) => {
            eprintln!("load failed: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let mut conn = match ConnectionMgr::new(&winidea) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ConnectionMgr::new failed: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if let Err(e) = conn.connect_mru(&workspace) {
        eprintln!("connect failed: {e}");
        return std::process::ExitCode::FAILURE;
    }
    println!("connected to {workspace}\n");

    // Pick a compound global if none was named: prefer a real multi-element
    // array, since its children are unambiguous.
    // Scoped so the controller's borrow of `conn` ends before the disconnect.
    let vars = {
        let mut dc = match DataController::new(&winidea, &conn) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("DataController::new failed: {e}");
                return std::process::ExitCode::FAILURE;
            }
        };
        dc.variables().unwrap_or_default()
    };

    // What the *data* controller reports for the named symbol. This is the
    // string the picker classifies with (`is_scalar_type` in the app's
    // `winidea/worker.rs`), and it is NOT necessarily the same string
    // `IVariable_TypeName` gives below: `CVariable_getType` and the expression
    // controller's accessor are different SDK calls. A scalar shown as a
    // container in the picker is exactly that disagreement, so print both.
    if let Some(s) = &symbol {
        match vars.iter().find(|v| v.name == *s || v.qualified_name == *s) {
            Some(v) => println!(
                "DataController: name={:?} qualified={:?}\n  \
                 getType()={:?}  scope={:?}  NumBytes={}  ArrayDimension={}\n",
                v.name, v.qualified_name, v.type_name, v.scope, v.size, v.array_dimension,
            ),
            None => println!(
                "DataController: {s:?} is NOT in the globals list \
                 (probing it as a bare expression)\n"
            ),
        }
    }

    let candidates: Vec<(String, String, i64)> = match &symbol {
        Some(s) => vars
            .iter()
            .find(|v| v.name == *s || v.qualified_name == *s)
            .map(|v| vec![(v.name.clone(), v.qualified_name.clone(), v.array_dimension)])
            .unwrap_or_else(|| vec![(s.clone(), s.clone(), 0)]),
        None => vars
            .iter()
            .filter(|v| v.array_dimension > 1)
            .take(3)
            .map(|v| (v.name.clone(), v.qualified_name.clone(), v.array_dimension))
            .collect(),
    };

    // Every global the picker would mark as a container, grouped by type name.
    // A scalar C type appearing here is a classifier miss: the app's
    // `is_scalar_type` allow-list did not recognise the spelling the SDK used.
    if list_nonscalar {
        let mut misses: Vec<(&str, &str, i64, i64)> = vars
            .iter()
            .filter(|v| {
                let is_array = v.array_dimension > 1;
                is_array || !app_is_scalar_type(&v.type_name)
            })
            .map(|v| {
                (
                    v.type_name.as_str(),
                    v.name.as_str(),
                    v.size,
                    v.array_dimension,
                )
            })
            .collect();
        misses.sort_unstable();

        println!(
            "{} of {} globals would show an expand twisty.\n\
             Grouped by type name (first example each); a plain scalar C type \n\
             in this list is a classifier miss:\n",
            misses.len(),
            vars.len()
        );
        let mut last = "";
        let mut shown = 0usize;
        for (ty, name, size, dim) in &misses {
            if *ty == last {
                continue;
            }
            last = ty;
            shown += 1;
            println!("  {ty:<48} e.g. {name}  ({size}B, dim {dim})");
        }
        println!("\n{shown} distinct type names.");
        if symbol.is_none() {
            let _ = conn.disconnect_keep();
            return std::process::ExitCode::SUCCESS;
        }
        println!();
    }

    // Reproduce the app's access pattern: many `has_children` calls in sequence
    // on ONE controller, which is where order-dependent failures show up. Each
    // expression is probed twice — once in sequence and once in isolation — so a
    // symbol whose verdict depends on what preceded it is visible as a
    // disagreement between the two columns.
    if let Some(limit) = batch_probe {
        // Scoped so every controller drops before the disconnect below: they
        // borrow `conn`, which `disconnect_keep` needs mutably.
        let (seq, subject_count) = {
            let mut ec = match winidea_sdk::data::ExpressionController::new(&winidea, &conn) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("ExpressionController::new failed: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            };

            let subjects: Vec<&winidea_sdk::data::Variable> = vars.iter().take(limit).collect();
            println!(
                "Batch-probing {} expressions in sequence, then re-probing each alone.\n\
             A row where SEQ and ALONE disagree is order-dependent — the symptom of\n\
             a shared error latch leaking between calls.\n",
                subjects.len()
            );

            let mut seq: Vec<(String, String)> = Vec::new();
            for v in &subjects {
                let verdict = match ec.has_children(&v.qualified_name) {
                    Ok(true) => "compound".to_string(),
                    Ok(false) => "scalar".to_string(),
                    Err(e) => format!("ERR({e})"),
                };
                seq.push((v.qualified_name.clone(), verdict));
            }

            let mut disagreements = 0usize;
            let mut errors = 0usize;
            for (expr, seq_verdict) in &seq {
                // A fresh controller per call removes any inherited state.
                let alone = match winidea_sdk::data::ExpressionController::new(&winidea, &conn) {
                    Ok(solo) => match solo.has_children(expr) {
                        Ok(true) => "compound".to_string(),
                        Ok(false) => "scalar".to_string(),
                        Err(e) => format!("ERR({e})"),
                    },
                    Err(e) => format!("ERR(controller: {e})"),
                };
                if seq_verdict.starts_with("ERR") {
                    errors += 1;
                }
                if *seq_verdict != alone {
                    disagreements += 1;
                    println!("  DISAGREE  seq={seq_verdict:<12} alone={alone:<12} {expr}");
                }
            }

            println!(
                "\n{} probed, {} sequence errors, {} disagreements.",
                seq.len(),
                errors,
                disagreements
            );
            let count = seq.len();
            println!(
                "{}",
                if disagreements == 0 {
                    "No order-dependence: verdicts are stable regardless of what precedes them."
                } else {
                    "ORDER-DEPENDENT: a call's result depends on the previous call. \
                 This is the flapping compound/scalar bug."
                }
            );
            (seq, count)
        };
        let _ = (seq, subject_count);

        let _ = conn.disconnect_keep();
        return std::process::ExitCode::SUCCESS;
    }

    if candidates.is_empty() {
        eprintln!("no compound global found to probe");
        return std::process::ExitCode::FAILURE;
    }

    let sdk = winidea.raw();

    // SAFETY: `conn` outlives this controller.
    let dc2 = unsafe {
        (sdk.new_CDataController2
            .expect("export missing from this isystemConnect.dll"))(conn.as_ptr())
    };
    if dc2.is_null() {
        eprintln!("new_CDataController2 returned null");
        return std::process::ExitCode::FAILURE;
    }
    let _ = winidea.take_error();

    // Flag values worth trying. 0 is what the wrapper uses today; the rest are
    // the plausible "give me the type tree" bits from neighbouring enums
    // (EGetGlobalsFlags-style partition selectors, and small bitfields).
    let flag_values: [u32; 8] = [
        0,
        1,
        2,
        0x1_0000,    // efType2 (IConnectDebug/EEvaluate)
        0x10_0000,   // gvfPartitionDefault
        0x20_0000,   // gvfPartitionCurrent
        0x10_0001,   // partition default + bit0
        0xFFFF_FFFF, // everything, to see if ANY bit changes behaviour
    ];

    for (name, qualified, dim) in &candidates {
        println!("=== {name}   (qualified: {qualified}, array_dimension: {dim}) ===");

        // The several ways the same variable can be named.
        let spellings: Vec<(String, String)> = vec![
            ("bare".into(), name.clone()),
            ("qualified".into(), qualified.clone()),
            ("index0".into(), format!("{name}[0]")),
            ("addr-of".into(), format!("&{name}")),
        ];

        for (label, expr) in &spellings {
            let Ok(c) = CString::new(expr.as_str()) else {
                continue;
            };
            for flags in flag_values {
                // SAFETY: live controller; the SDK copies the string.
                let ty = unsafe {
                    (sdk.CDataController2_getExpressionType
                        .expect("export missing from this isystemConnect.dll"))(
                        dc2,
                        flags,
                        c.as_ptr(),
                    )
                };
                let err = winidea.take_error().err();

                if ty.is_null() {
                    // Only report the failure once per spelling to stay readable.
                    if flags == 0 {
                        println!(
                            "  {label:<10} flags=0x{flags:<8x} -> null{}",
                            err.map(|e| format!(" ({e})")).unwrap_or_default()
                        );
                    }
                    continue;
                }

                // SAFETY: `ty` is a live IExpressionType.
                let children = unsafe {
                    (sdk.IExpressionType_Children
                        .expect("export missing from this isystemConnect.dll"))(
                        ty
                    )
                };
                let _ = winidea.take_error();
                let n = if children.is_null() {
                    0
                } else {
                    // SAFETY: live IVectorVariables.
                    let n = unsafe {
                        (sdk.IVectorVariables_size
                            .expect("export missing from this isystemConnect.dll"))(
                            children
                        )
                    };
                    let _ = winidea.take_error();
                    n
                };

                // SAFETY: `ty` is live.
                let self_var = unsafe {
                    (sdk.IExpressionType_Expression
                        .expect("export missing from this isystemConnect.dll"))(
                        ty
                    )
                };
                let _ = winidea.take_error();
                let self_desc = if self_var.is_null() {
                    "<no Expression()>".to_string()
                } else {
                    // SAFETY: `self_var` is a live IVariable borrowed from `ty`.
                    unsafe {
                        let nm = (sdk
                            .IVariable_Name
                            .expect("export missing from this isystemConnect.dll"))(
                            self_var
                        );
                        let tn = (sdk
                            .IVariable_TypeName
                            .expect("export missing from this isystemConnect.dll"))(
                            self_var
                        );
                        let d = (sdk
                            .IVariable_ArrayDimension
                            .expect("export missing from this isystemConnect.dll"))(
                            self_var
                        );
                        let sz = (sdk
                            .IVariable_NumBytes
                            .expect("export missing from this isystemConnect.dll"))(
                            self_var
                        );
                        format!("{} : {} (dim {d}, {sz}B)", cstr(nm), cstr(tn))
                    }
                };

                if n > 0 || flags == 0 {
                    println!(
                        "  {label:<10} flags=0x{flags:<8x} -> children={n:<4} self={self_desc}"
                    );
                }
                if n > 0 {
                    // Show the first few, since this is the answer we want.
                    for i in 0..n.min(4) {
                        // SAFETY: 0 <= i < size.
                        let e = unsafe {
                            (sdk.IVectorVariables_at
                                .expect("export missing from this isystemConnect.dll"))(
                                children, i
                            )
                        };
                        let _ = winidea.take_error();
                        if e.is_null() {
                            continue;
                        }
                        // SAFETY: `e` is a live IVariable owned by the vector.
                        unsafe {
                            println!(
                                "        [{i}] {} : {} ({}B @ {:#x})",
                                cstr((sdk
                                    .IVariable_Name
                                    .expect("export missing from this isystemConnect.dll"))(
                                    e
                                )),
                                cstr((sdk
                                    .IVariable_TypeName
                                    .expect("export missing from this isystemConnect.dll"))(
                                    e
                                )),
                                (sdk.IVariable_NumBytes
                                    .expect("export missing from this isystemConnect.dll"))(
                                    e
                                ),
                                (sdk.IVariable_Address
                                    .expect("export missing from this isystemConnect.dll"))(
                                    e
                                ),
                            );
                        }
                        let _ = winidea.take_error();
                    }
                }

                // SAFETY: `ty` is a live IExpressionType from this controller.
                unsafe {
                    (sdk.CDataController2_release__SWIG_1
                        .expect("export missing from this isystemConnect.dll"))(
                        dc2, ty
                    )
                };
                let _ = winidea.take_error();
            }
        }
        println!();
    }

    // SAFETY: `dc2` was minted by new_CDataController2 and is freed once.
    unsafe {
        (sdk.delete_CDataController2
            .expect("export missing from this isystemConnect.dll"))(dc2)
    };
    let _ = winidea.take_error();

    let _ = conn.disconnect_keep();
    std::process::ExitCode::SUCCESS
}

/// The app's root-node classifier, copied VERBATIM from
/// `backend/src/winidea/worker.rs` (`is_scalar_type`) as of this probe's last
/// edit. Duplicated on purpose: the probe's job is to report what the picker
/// decides, so it must use the picker's own rule rather than a better one. If
/// the app's list changes, update this copy or the probe starts lying.
fn app_is_scalar_type(type_name: &str) -> bool {
    let base = match type_name.find('[') {
        Some(i) => type_name[..i].trim(),
        None => type_name.trim(),
    };
    matches!(
        base,
        "uint8"
            | "uint16"
            | "uint32"
            | "uint64"
            | "sint8"
            | "sint16"
            | "sint32"
            | "sint64"
            | "int8"
            | "int16"
            | "int32"
            | "int64"
            | "float32"
            | "float64"
            | "float"
            | "double"
            | "boolean"
            | "bool"
            | "char"
            | "signed char"
            | "unsigned char"
            | "short"
            | "unsigned short"
            | "long"
            | "unsigned long"
            | "int"
            | "unsigned int"
    )
}

/// Render a SDK-returned C string, or a placeholder when null.
fn cstr(p: *const std::ffi::c_char) -> String {
    if p.is_null() {
        return "<null>".into();
    }
    // SAFETY: the SDK returns NUL-terminated strings through the registered
    // string callback.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// Silence the unused-import warning for `c_void`, which documents the handle
/// type the raw calls traffic in.
#[allow(dead_code)]
type Handle = *mut c_void;
