//! Standalone smoke test for the winIDEA bindings.
//!
//! Build a self-contained .exe and run it on a machine that has winIDEA:
//!
//! ```text
//! cargo build -p winidea-sdk --example smoke --profile dev-release
//! # -> backend/target/dev-release/examples/smoke.exe
//! ```
//!
//! The binary has no dependencies beyond `isystemConnect.dll`, which it loads at
//! runtime. Copy the single .exe anywhere.
//!
//! ```text
//! smoke.exe                              # load-only checks, no winIDEA needed
//! smoke.exe --workspace C:\ws\proj.xjrf  # full run against a real instance
//! smoke.exe --dll C:\sdk\isystemConnect.dll --workspace ...
//! ```
//!
//! # What it is actually testing
//!
//! Not the symbol names or signatures -- those are already proven statically
//! (all 6,094 symbols resolve, types come from the SDK's own P/Invoke metadata).
//! This exercises the things that *cannot* be checked without running:
//!
//!   1. **The string callback.** SWIG makes the host allocate every string the
//!      DLL returns, and the DLL frees it with `free()`. We allocate with
//!      `malloc` to match. If that pairing is wrong it is heap corruption, and
//!      it shows up on the first string return.
//!   2. **The exception callbacks.** 11 + 3 function pointers registered at
//!      load. Wrong arity or order means the first SDK error corrupts something
//!      instead of producing an `Err`.
//!   3. **Handle lifetimes.** Every `Owned` frees exactly once on drop. Correct
//!      by construction, but no real destructor has ever run.
//!   4. **`CValueType` tag ordering.** Our decode checks error -> compound ->
//!      float -> address -> signed -> unsigned. Real targets may report
//!      overlapping tags.
//!
//! Each step prints PASS/FAIL and keeps going where it safely can, so one
//! failure still yields a useful report. A crash (rather than a FAIL line)
//! localises the problem to the step that was printed last -- which is why
//! every step announces itself *before* doing the work.

use std::path::PathBuf;

use winidea_sdk::{
    ConnectionMgr, WinIdea,
    address::{AddressController, SymbolFlags},
    daq::{AcquisitionMode, ConfigItem, DaqController, DaqInfo, Sample, SamplingFlags},
    data::{DataController, ExpressionController, Variable},
    debug::DebugFacade,
    execution::ExecutionController,
    ide::{IdeController, PathType, ViewType},
};

/// Tracks pass/fail so the process can exit non-zero for CI use.
struct Report {
    passed: usize,
    failed: usize,
    skipped: usize,
}

impl Report {
    fn new() -> Self {
        Self {
            passed: 0,
            failed: 0,
            skipped: 0,
        }
    }

    /// Announce a step BEFORE running it, so a hard crash still tells us where.
    fn step(&mut self, name: &str) {
        print!("  {name:.<52}");
        // Force the label out now; if the next call dies, the label is already
        // on screen and names the culprit.
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }

    fn pass(&mut self, detail: impl std::fmt::Display) {
        self.passed += 1;
        println!(" PASS  {detail}");
    }

    fn fail(&mut self, err: impl std::fmt::Display) {
        self.failed += 1;
        println!(" FAIL  {err}");
    }

    fn skip(&mut self, why: &str) {
        self.skipped += 1;
        println!(" SKIP  {why}");
    }

    /// Run a fallible step, recording the outcome either way.
    fn check<T, E: std::fmt::Display>(
        &mut self,
        name: &str,
        f: impl FnOnce() -> Result<T, E>,
        show: impl FnOnce(&T) -> String,
    ) -> Option<T> {
        self.step(name);
        match f() {
            Ok(v) => {
                let d = show(&v);
                self.pass(d);
                Some(v)
            }
            Err(e) => {
                self.fail(e);
                None
            }
        }
    }
}

struct Args {
    dll: Option<PathBuf>,
    workspace: Option<String>,
    symbol: String,
    expression: Option<String>,
    /// Comma-separated symbols to acquire via DAQ. `None` = skip DAQ entirely.
    daq: Option<Vec<String>>,
    /// Requested interval per `--daq` symbol, in ms. Shorter than the symbol
    /// list means the last value repeats; `None` = `SamplingFlags::Max`.
    ///
    /// Exists to reproduce a *mixed-tier* selection (`--rate 10,1`), which a
    /// single global rate cannot express — and which is exactly the shape that
    /// stops acquiring in the app.
    rates: Option<Vec<u32>>,
    /// Case-insensitive substring: print matching globals and their live values
    /// twice, a moment apart, so the ones actually CHANGING are visible. DAQ on
    /// a constant variable proves nothing about sampling rate.
    list: Option<String>,
    /// Struct/array symbol to expand. `None` = pick one from the enumeration.
    unfold: Option<String>,
    /// Allow the IDE checks to move/raise real winIDEA windows.
    ide_window: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut dll = None;
    let mut workspace = None;
    let mut symbol = "main".to_string();
    let mut expression = None;

    let mut daq = None;
    let mut rates = None;
    let mut list = None;
    let mut unfold = None;
    let mut ide_window = false;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dll" => dll = Some(PathBuf::from(it.next().ok_or("--dll needs a path")?)),
            "--workspace" | "-w" => workspace = Some(it.next().ok_or("--workspace needs a path")?),
            "--symbol" | "-s" => symbol = it.next().ok_or("--symbol needs a name")?,
            "--expression" | "-e" => {
                expression = Some(it.next().ok_or("--expression needs an expression")?)
            }
            "--daq" => {
                let list = it.next().ok_or("--daq needs one or more symbols")?;
                let syms: Vec<String> = list
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
                if syms.is_empty() {
                    return Err("--daq needs at least one symbol".into());
                }
                daq = Some(syms);
            }
            "--rate" => {
                let list = it.next().ok_or("--rate needs one or more intervals")?;
                let ms: Result<Vec<u32>, String> = list
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        s.parse::<u32>()
                            .map_err(|_| format!("bad --rate value {s:?}"))
                    })
                    .collect();
                let ms = ms?;
                if ms.is_empty() {
                    return Err("--rate needs at least one interval".into());
                }
                rates = Some(ms);
            }
            "--list" => list = Some(it.next().ok_or("--list needs a substring")?),
            "--unfold" => unfold = Some(it.next().ok_or("--unfold needs a symbol")?),
            "--ide-window" => ide_window = true,
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument `{other}` (try --help)")),
        }
    }
    Ok(Args {
        dll,
        workspace,
        symbol,
        expression,
        daq,
        rates,
        list,
        unfold,
        ide_window,
    })
}

fn print_usage() {
    println!(
        "\
winIDEA SDK smoke test

USAGE:
  smoke.exe [--dll <path>] [--workspace <path>] [--symbol <name>]
            [--expression <expr>] [--daq <sym,sym,...>] [--unfold <sym>]
            [--ide-window]

OPTIONS:
  --dll <path>          Path to isystemConnect.dll. Default: AURIX_WINIDEA_DLL,
                        then the usual install dirs, then the OS search path.
  -w, --workspace <p>   winIDEA workspace (.xjrf) to open. Without this, only the
                        load-time checks run -- no winIDEA instance is needed.
  -s, --symbol <name>   Function symbol for the breakpoint check. Default: main
  -e, --expression <e>  Expression to evaluate, e.g. a global variable name.
      --daq <a,b,c>     Comma-separated symbols to acquire via DAQ. Opt-in: DAQ
                        needs real debug hardware and perturbs the target, so it
                        is skipped unless this is given.
      --rate <a,b,c>    Requested interval in ms per --daq symbol, snapped to the
                        hardware tiers (max/1/10/100/1000). Fewer values than
                        symbols repeats the last one. Default: max.
                        e.g. --daq x,y --rate 10,1  -> a MIXED-tier selection.
      --unfold <sym>    Struct/array global to expand into its members. Without
                        it, the first compound global found is used.
      --ide-window      Also exercise window control (raises the winIDEA window
                        and opens a view). Visible to anyone at the machine, so
                        it is opt-in.
  -h, --help            Show this help.

EXIT CODE: 0 if every step that ran passed, 1 otherwise."
    );
}

fn main() -> std::process::ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    println!("winIDEA SDK smoke test");
    println!("======================\n");

    let mut r = Report::new();

    // --- 1. load the DLL and resolve all 6,094 symbols --------------------
    println!("[1] Library");
    let winidea = {
        let dll = args.dll.clone();
        let loaded = match &dll {
            Some(p) => {
                r.step(&format!("load {}", p.display()));
                WinIdea::load_from(p.as_os_str())
            }
            None => {
                r.step("load isystemConnect.dll (auto-discover)");
                WinIdea::load(&winidea_sdk::default_search_paths())
            }
        };
        match loaded {
            Ok(w) => {
                // Reaching here means every symbol resolved AND the SWIG
                // exception/string callbacks were registered without incident.
                r.pass("all 6094 symbols resolved, callbacks registered");
                w
            }
            Err(e) => {
                r.fail(&e);
                println!("\nCannot continue without the library.");
                println!("Point at it explicitly with --dll <path>, or set");
                println!("AURIX_WINIDEA_DLL. It ships in the winIDEA C# SDK");
                println!("under isystem-connect-cs/lib/x64/.");
                return std::process::ExitCode::FAILURE;
            }
        }
    };

    // --- 2. static call + first error round-trip --------------------------
    println!("\n[2] Callbacks (no winIDEA instance required)");

    // A static call: no handle involved, so it isolates "can we call at all"
    // from "are our handles right".
    r.check(
        "ConnectionMgr::is_started_from_winidea()",
        || ConnectionMgr::is_started_from_winidea(&winidea),
        |v| format!("-> {v}"),
    );

    // Construct + drop a real object. This is the first time a C++ destructor
    // runs through our `Owned` guard.
    r.check(
        "ConnectionMgr::new() + drop",
        || ConnectionMgr::new(&winidea).map(drop),
        |_| "handle allocated and freed".into(),
    );

    let mut conn = match ConnectionMgr::new(&winidea) {
        Ok(c) => c,
        Err(e) => {
            println!("\n  cannot create ConnectionMgr: {e}");
            return finish(&r);
        }
    };

    // First string return -> exercises the malloc/free string callback. This is
    // the single most likely place for a latent heap bug, so do it early and
    // before anything more complex.
    r.check(
        "ConnectionMgr::id()  [string callback]",
        || conn.id(),
        |v| format!("-> {:?}", truncate(v, 40)),
    );

    // Not connected yet, so this should be a clean `false` or a clean `Err` --
    // either proves the exception path works. A crash here means the callbacks
    // are wrong.
    r.check(
        "ConnectionMgr::is_connected()  [error path]",
        || conn.is_connected(false),
        |v| format!("-> {v}"),
    );

    let Some(workspace) = args.workspace.clone() else {
        println!("\n[3] Connection");
        r.skip_section("no --workspace given; skipping live checks");
        return finish(&r);
    };

    // --- 3. connect to a real instance ------------------------------------
    println!("\n[3] Connection");
    println!("      workspace: {workspace}");

    if r.check(
        "connect_mru()",
        || conn.connect_mru(&workspace),
        |_| "connected".into(),
    )
    .is_none()
    {
        println!("\n  Could not connect; skipping the remaining checks.");
        return finish(&r);
    }

    r.check(
        "is_connected(verify=true)",
        || conn.is_connected(true),
        |v| format!("-> {v}"),
    );
    r.check(
        "version()  [string callback]",
        || conn.version(),
        |v| format!("-> winIDEA {v}"),
    );

    // --- 4. execution controller ------------------------------------------
    println!("\n[4] Execution");
    match ExecutionController::new(&winidea, &conn) {
        Ok(exec) => {
            r.pass_named("ExecutionController::new()", "created");
            r.check(
                "cpu_status()  [multi-accessor + string]",
                || exec.cpu_status(),
                |s| {
                    format!(
                        "-> running={} stopped={} pc={:#x}",
                        s.is_running, s.is_stopped, s.execution_point
                    )
                },
            );
        }
        Err(e) => r.fail_named("ExecutionController::new()", e),
    }

    // --- 5. debug facade ---------------------------------------------------
    println!("\n[5] Debug");
    match DebugFacade::new(&winidea, &conn) {
        Ok(mut dbg) => {
            r.pass_named("DebugFacade::new()", "created");

            r.check(
                &format!("function_address({:?})  [CMemAddress]", args.symbol),
                || dbg.function_address(&args.symbol),
                |v| format!("-> {v}"),
            );

            if let Some(expr) = &args.expression {
                r.check(
                    &format!("evaluate({expr:?})  [CValueType]"),
                    || dbg.evaluate(expr),
                    |v| format!("-> {v}"),
                );
            } else {
                r.step("evaluate()");
                r.skip("no --expression given");
            }

            // Deliberately evaluate nonsense: the SDK should report a failure
            // through the exception callback or an error-tagged CValueType.
            // Either is fine; a crash is not.
            r.check(
                "evaluate(<invalid>)  [error handling]",
                || {
                    dbg.evaluate("__no_such_symbol_zzz__")
                        .map(|v| format!("error-value: {v}"))
                        .or_else(|e| Ok::<_, winidea_sdk::Error>(format!("Err: {e}")))
                },
                |s| format!("-> {}", truncate(s, 46)),
            );
        }
        Err(e) => r.fail_named("DebugFacade::new()", e),
    }

    // --- 5c. Symbols -------------------------------------------------------
    println!("\n[5c] Symbols");
    match AddressController::new(&winidea, &conn) {
        Ok(mut ac) => {
            r.pass_named("AddressController::new()", "created");

            let fa = r.check(
                &format!("function_address({:?})", args.symbol),
                || ac.function_address(&args.symbol),
                |a| format!("-> {a}"),
            );

            r.check(
                &format!("symbol_info({:?})", args.symbol),
                || ac.symbol_info(&args.symbol, SymbolFlags::ANY_SYMBOL),
                |i| format!("-> addr={:#x} size={} MAUs", i.address, i.size_maus),
            );

            // Round-trip: name -> address -> source line -> name. Each step
            // uses a different SDK path, so agreement is meaningful.
            if let Some(a) = fa {
                r.check(
                    "source_line_at_address()",
                    || ac.source_line_at_address(a.address),
                    |l| match l {
                        Some(loc) => format!("-> {loc}"),
                        None => "-> <no line info>".into(),
                    },
                );

                r.check(
                    "symbol_at_address()  [reverse lookup]",
                    || ac.symbol_at_address(SymbolFlags::ANY_SYMBOL, a.mem_area, a.address),
                    |n| format!("-> {:?}", truncate(n, 40)),
                );

                // Batch lookup: same address plus a deliberately bogus one, so
                // both the found and not-found paths are exercised at once.
                r.check(
                    "function_names()  [batch]",
                    || ac.function_names(&[a.address, 0xDEAD_BEEF]),
                    |v| {
                        let parts: Vec<String> = v
                            .iter()
                            .map(|f| format!("{:#x}={:?}", f.address, truncate(&f.name, 18)))
                            .collect();
                        format!("-> {}", truncate(&parts.join(" "), 46))
                    },
                );
            }

            r.check(
                "code_area(physical)",
                || ac.code_area(true),
                |a| format!("-> {a}"),
            );
        }
        Err(e) => r.fail_named("AddressController::new()", e),
    }

    // --- 5d. Bulk data -----------------------------------------------------
    println!("\n[5d] Data");
    match DataController::new(&winidea, &conn) {
        Ok(mut dc) => {
            r.pass_named("DataController::new()", "created");

            // Partitions first: symbol queries are scoped to a download file,
            // and passing a bare index 0 raises ICONNECT_E_BAD_ID.
            r.check(
                "partitions()",
                || dc.partitions(),
                |v| match v.first() {
                    Some(p) => format!(
                        "-> {} partition(s), e.g. {}",
                        v.len(),
                        truncate(&p.name, 26)
                    ),
                    None => "-> none".into(),
                },
            );
            r.check(
                "default_endian()",
                || dc.default_endian(),
                |e| format!("-> {e}"),
            );

            // Symbol enumeration needs a download file (ELF) loaded: without
            // one there is no symbol table and the SDK raises
            // ICONNECT_E_BAD_ID. Check first so a bare workspace reports SKIP
            // rather than three identical failures.
            let has_image = dc.partitions().map(|p| !p.is_empty()).unwrap_or(false);
            let vars = if has_image {
                let vars = r.check(
                    "variables()  [VariableVector]",
                    || dc.variables(),
                    |v| match v.first() {
                        Some(f) => {
                            format!("-> {} vars, e.g. {} : {}", v.len(), f.name, f.type_name)
                        }
                        None => format!("-> {} vars", v.len()),
                    },
                );
                r.check(
                    "functions()  [FunctionVector]",
                    || dc.functions(),
                    |v| match v.first() {
                        Some(f) => format!("-> {} fns, e.g. {}", v.len(), f.name),
                        None => format!("-> {} fns", v.len()),
                    },
                );
                r.check(
                    "modules()  [ModuleVector]",
                    || dc.modules(),
                    |v| match v.first() {
                        Some(m) => {
                            format!("-> {} modules, e.g. {}", v.len(), truncate(&m.name, 24))
                        }
                        None => format!("-> {} modules", v.len()),
                    },
                );
                vars
            } else {
                r.step("symbol enumeration");
                r.skip("no download file loaded (nothing to enumerate)");
                None
            };

            // Read the bytes of a real symbol, so the address actually maps.
            // Using a discovered variable avoids hardcoding a target address.
            let probe = vars
                .as_ref()
                .and_then(|v| v.iter().find(|x| x.size > 0 && x.size <= 64))
                .map(|v| (v.qualified_name.clone(), v.size));

            match probe {
                Some((name, size)) => {
                    // Resolve name -> address via the address controller, then
                    // read that many bytes.
                    let addr = AddressController::new(&winidea, &conn)
                        .and_then(|mut ac| ac.variable_address(&name));
                    match addr {
                        Ok(a) => {
                            r.check(
                                &format!("read_memory({} @ {a})", truncate(&name, 20)),
                                || dc.read_memory(a.mem_area, a.address, size as u64),
                                |b| {
                                    format!(
                                        "-> {} bytes, all readable={}, {:02x?}",
                                        b.data.len(),
                                        b.is_fully_readable(),
                                        &b.data[..b.data.len().min(8)]
                                    )
                                },
                            );
                        }
                        Err(e) => {
                            r.step("read_memory()");
                            r.skip(&format!("could not resolve {name}: {e}"));
                        }
                    }
                }
                None => {
                    r.step("read_memory()");
                    r.skip("no suitably sized variable found to probe");
                }
            }

            // Unfolding: walk INTO a struct or array. This is what turns a
            // single "gConfig" entry into the acquirable leaves a signal picker
            // needs, so it is exercised against whatever compound global this
            // image happens to have rather than a hardcoded name.
            if let Some(needle) = args.list.as_deref() {
                list_changing(&mut r, &winidea, &conn, vars.as_deref(), needle);
            }

            run_unfolding(
                &mut r,
                &winidea,
                &conn,
                vars.as_deref(),
                args.unfold.as_deref(),
            );
        }
        Err(e) => r.fail_named("DataController::new()", e),
    }

    // --- 5b. IDE control ---------------------------------------------------
    // Read-only by default: opening/moving windows is visible to whoever is
    // sitting at the machine, so only --ide-window does the disruptive parts.
    println!("\n[5b] IDE");
    match IdeController::new(&winidea, &conn) {
        Ok(mut ide) => {
            r.pass_named("IdeController::new()", "created");

            r.check(
                "path(WorkspaceDir)  [string]",
                || ide.path(PathType::WorkspaceDir),
                |p| format!("-> {}", truncate(p, 44)),
            );
            r.check(
                "default_download_file()",
                || ide.default_download_file(),
                |p| format!("-> {}", truncate(p, 44)),
            );
            r.check("num_clients()", || ide.num_clients(), |n| format!("-> {n}"));
            // Option paths need a module prefix (/IOPEN/, /IDE/); a bare
            // "Hardware.Emulator" does not resolve.
            r.check(
                "option_str(\"/IOPEN/Hardware.Emulator\")",
                || ide.option_str("/IOPEN/Hardware.Emulator"),
                |v| format!("-> {:?}", truncate(v, 40)),
            );
            r.check(
                "option_str(\"/IOPEN/Communication.Mode\")",
                || ide.option_str("/IOPEN/Communication.Mode"),
                |v| format!("-> {:?}", truncate(v, 40)),
            );
            // A branch node has no scalar value but does have a JSON subtree.
            r.check(
                "option_json(\"/IOPEN/Hardware\")  [branch]",
                || ide.option_json("/IOPEN/Hardware"),
                |j| format!("-> {} bytes", j.len()),
            );

            // invoke() exercises StrStrMap in BOTH directions plus the
            // ResultB/ResultS error convention. '/IDE/Divide' is the SDK's own
            // documented example function, so it is safe to call.
            r.check(
                "invoke(\"/IDE/Divide\")  [StrStrMap round-trip]",
                || ide.invoke("/IDE/Divide", &[("Dividend", "10"), ("Divisor", "3")]),
                |m| {
                    let mut kv: Vec<String> = m.iter().map(|(k, v)| format!("{k}={v}")).collect();
                    kv.sort();
                    format!("-> {}", truncate(&kv.join(" "), 44))
                },
            );

            if args.ide_window {
                r.check("bring_to_top()", || ide.bring_to_top(), |_| "raised".into());
                r.check(
                    "open_view(Watch)",
                    || ide.open_view(ViewType::Watch),
                    |_| "opened".into(),
                );
            } else {
                r.step("window control");
                r.skip("no --ide-window given (would move real windows)");
            }
        }
        Err(e) => r.fail_named("IdeController::new()", e),
    }

    // --- 6. DAQ ------------------------------------------------------------
    // Opt-in: DAQ needs real debug hardware, and configuring it perturbs the
    // target, so it never runs unless explicitly asked for.
    println!("\n[6] DAQ");
    match args.daq.clone() {
        None => {
            r.step("acquisition");
            r.skip("no --daq <symbol> given");
        }
        Some(symbols) => run_daq(&mut r, &winidea, &conn, &symbols, args.rates.as_deref()),
    }

    // --- 7. teardown -------------------------------------------------------
    println!("\n[7] Teardown");
    r.check(
        "disconnect_keep()",
        || conn.disconnect_keep(),
        |_| "disconnected".into(),
    );
    // `conn`, and the SDK itself, drop here -- the last chance for a
    // double-free to surface.

    finish(&r)
}

impl Report {
    fn pass_named(&mut self, name: &str, detail: &str) {
        self.step(name);
        self.pass(detail);
    }

    fn fail_named(&mut self, name: &str, err: impl std::fmt::Display) {
        self.step(name);
        self.fail(err);
    }

    fn skip_section(&mut self, why: &str) {
        self.step("live checks");
        self.skip(why);
    }
}

/// Exercise a full DAQ acquisition cycle against real debug hardware.
///
/// This is the one part of the crate that needs a live target doing something:
/// DAQ samples a running program, so a halted CPU yields nothing. The sequence
/// is the SDK's required order -- attach, configure, enable, read, disable,
/// detach -- and getting it wrong is one of the untested assumptions.
fn run_daq(
    r: &mut Report,
    winidea: &WinIdea,
    conn: &ConnectionMgr<'_>,
    symbols: &[String],
    rates: Option<&[u32]>,
) {
    let mut daq = match DaqController::new(winidea, conn) {
        Ok(d) => {
            r.pass_named("DaqController::new()", "created");
            d
        }
        Err(e) => {
            r.fail_named("DaqController::new()", e);
            return;
        }
    };

    // No attach_hw(): the SDK deprecated it as a no-op in 9.12.288, and its own
    // examples go straight from construction to info()/configure().
    let info = r.check(
        "info()",
        || daq.info(),
        |i| {
            format!(
                "-> max_items={} buf={}B tick={}ns",
                i.max_items, i.buf_size, i.tick_ns
            )
        },
    );

    // max_items == 0 means this target has no DAQ engine. The SDK's own example
    // bails out here, and so do we -- otherwise every later step fails for one
    // reason that is not a bug in the bindings.
    if let Some(i) = &info
        && !i.is_supported()
    {
        r.step("acquisition");
        r.skip("target reports max_items=0 (no DAQ hardware)");
        return;
    }

    // The acquisition mode decides how samples are drained, and a mode this
    // debugger cannot serve fails inside read() with an obscure winIDEA path
    // error rather than at configure() time. So try each in turn and report
    // which one actually works -- that answer is the useful output here.
    // Per-symbol sampling: a shorter --rate list repeats its last value, so
    // `--rate 1` sets every item to 1ms while `--rate 10,1` mixes tiers. Mixing
    // is the interesting case — a single global rate cannot express it, and it
    // is what the app configures when one variable is added at a rate differing
    // from the rest.
    let sampling: Vec<SamplingFlags> = symbols
        .iter()
        .enumerate()
        .map(|(i, _)| match rates {
            Some(rs) if !rs.is_empty() => SamplingFlags::from_interval_ms(rs[i.min(rs.len() - 1)]),
            _ => SamplingFlags::Max,
        })
        .collect();
    for (s, f) in symbols.iter().zip(&sampling) {
        println!(
            "      item {s}: requested {} -> {f:?}",
            match f.interval_ms() {
                Some(ms) => format!("{ms} ms"),
                None => "max".into(),
            }
        );
    }
    let items: Vec<ConfigItem<'_>> = symbols
        .iter()
        .zip(&sampling)
        .map(|(s, f)| ConfigItem::Symbol {
            name: s.as_str(),
            sampling: *f,
        })
        .collect();
    let modes = [
        ("Auto", AcquisitionMode::Auto),
        ("Sw", AcquisitionMode::Sw),
        ("Hw", AcquisitionMode::Hw),
    ];

    for (label, mode) in modes {
        println!("      --- acquisition mode: {label} ---");

        // Enable is what starts the loop; make sure a previous attempt is off.
        let _ = daq.enable_global(false);

        if r.check(
            &format!("configure({} item(s), {label})", items.len()),
            || daq.configure_with_mode(&items, mode),
            |h| format!("-> handles {h:?}"),
        )
        .is_none()
        {
            continue;
        }

        if r.check(
            &format!("enable_global(true, {label})"),
            || daq.enable_global(true),
            |_| "enabled".into(),
        )
        .is_none()
        {
            continue;
        }

        // DAQ runs asynchronously on the debug hardware, so give it time to
        // put something in the buffer before draining it.
        std::thread::sleep(std::time::Duration::from_millis(500));

        let status = r.check(
            &format!("status({label})"),
            || daq.status(),
            |s| {
                format!(
                    "-> available={} loops={} overflow={}",
                    s.num_samples_available, s.num_loops, s.overflow
                )
            },
        );

        // The SDK's own recorder only calls read() when samples are waiting.
        if let Some(s) = &status
            && s.num_samples_available == 0
        {
            r.step(&format!("read({label})"));
            r.skip("no samples available yet");
            continue;
        }

        // The payoff: real samples off real hardware. This proves CDAQSample's
        // accessors -- the ones with ZERO linkable symbols in the C++ static
        // lib, and the reason this crate binds the C# SDK's DLL at all -- plus
        // getDataValue(), which turns a sample into an actual number.
        match daq.read() {
            Ok(samples) if samples.is_empty() => {
                r.step(&format!("read({label})"));
                r.skip("0 samples returned");
                continue;
            }
            Err(e) => {
                r.step(&format!("read({label})"));
                r.fail(format!("{e}"));
                if e.to_string().contains("ReadCompressed") {
                    println!("        NOTE: a winIDEA-internal path error here means the");
                    println!("        winIDEA build does not match the SDK these bindings");
                    println!("        were generated from (9.21.417). Check versions.");
                }
                continue;
            }
            Ok(samples) => {
                r.step(&format!("read({label})"));
                report_samples(r, &samples, &info);
                println!("      >>> acquisition mode {label} WORKS <<<");
                break;
            }
        }
    }

    // --- reconfigure while acquiring --------------------------------------
    //
    // The app does NOT configure once and run: changing a variable's rate in
    // the picker reconfigures a LIVE acquisition, which means disable ->
    // configure -> re-enable. That sequence is where acquisition was silently
    // dying, so exercise it here rather than only the clean first-configure
    // path above. Each cycle re-reads afterwards: a reconfigure that "succeeds"
    // but leaves the engine disabled produces zero samples, which is exactly
    // the failure that looked like nothing at all.
    println!("      --- reconfigure while acquiring ---");
    for (label, ms) in [("1ms", 1u32), ("10ms", 10), ("mixed 10/1", 0)] {
        let flags: Vec<SamplingFlags> = symbols
            .iter()
            .enumerate()
            .map(|(i, _)| {
                if ms == 0 {
                    // Alternate tiers, so the mixed case is genuinely mixed.
                    SamplingFlags::from_interval_ms(if i % 2 == 0 { 10 } else { 1 })
                } else {
                    SamplingFlags::from_interval_ms(ms)
                }
            })
            .collect();
        let live: Vec<ConfigItem<'_>> = symbols
            .iter()
            .zip(&flags)
            .map(|(s, f)| ConfigItem::Symbol {
                name: s.as_str(),
                sampling: *f,
            })
            .collect();

        // Exactly the app's order.
        let seq = daq
            .enable_global(false)
            .and_then(|_| daq.configure(&live).map(|_| ()))
            .and_then(|_| daq.enable_global(true));

        r.step(&format!("reconfigure -> {label}"));
        match seq {
            Ok(()) => r.pass("disabled, configured, re-enabled"),
            Err(e) => {
                r.fail(format!("{e}"));
                continue;
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(300));
        let st = daq.status();
        let n = daq.read().map(|s| s.len()).unwrap_or(0);
        r.step(&format!("samples after {label}"));
        match st {
            // The invariant: a reconfigure that reports success must leave the
            // engine ENABLED. `global_enable == false` here is the silent stop.
            Ok(s) if !s.global_enable => {
                r.fail("global_enable=false after a successful reconfigure (silent stop)")
            }
            Ok(s) => {
                if n == 0 {
                    r.fail(format!(
                        "0 samples in 300ms (enabled={}, loops={})",
                        s.global_enable, s.num_loops
                    ));
                } else {
                    r.pass(format!("-> {n} samples, enabled={}", s.global_enable));
                }
            }
            Err(e) => r.fail(format!("status failed: {e}")),
        }
    }

    r.check(
        "enable_global(false)",
        || daq.enable_global(false),
        |_| "disabled".into(),
    );
    // `daq` drops here -- CDAQController's destructor runs.
}

/// Print globals matching `needle`, sampled twice a moment apart.
///
/// DAQ on a variable that never changes is untestable: a rate cannot be
/// distinguished from a stall when every sample carries the same value. This
/// finds candidates that actually move, so a rate test means something.
fn list_changing(
    r: &mut Report,
    winidea: &WinIdea,
    conn: &ConnectionMgr<'_>,
    vars: Option<&[Variable]>,
    needle: &str,
) {
    let Some(vars) = vars else {
        r.step("list");
        r.skip("no variables enumerated");
        return;
    };
    let mut dbg = match DebugFacade::new(winidea, conn) {
        Ok(d) => d,
        Err(e) => {
            r.fail_named("list: DebugFacade::new()", e);
            return;
        }
    };

    let q = needle.to_lowercase();
    let hits: Vec<&Variable> = vars
        .iter()
        .filter(|v| v.qualified_name.to_lowercase().contains(&q))
        .filter(|v| v.array_dimension <= 1)
        .take(40)
        .collect();

    r.step(&format!("list({needle:?})"));
    if hits.is_empty() {
        r.skip("no global matched");
        return;
    }
    r.pass(format!("-> {} match(es)", hits.len()));

    // Two reads a moment apart; anything differing between them is live.
    let read_all = |dbg: &mut DebugFacade<'_>| -> Vec<String> {
        hits.iter()
            .map(|v| {
                dbg.evaluate(&v.qualified_name)
                    .map(|x| x.to_string())
                    .unwrap_or_else(|e| format!("<{e}>"))
            })
            .collect()
    };
    let before = read_all(&mut dbg);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let after = read_all(&mut dbg);

    let mut changed = 0usize;
    for ((v, b), a) in hits.iter().zip(&before).zip(&after) {
        let moves = b != a;
        if moves {
            changed += 1;
        }
        println!(
            "        {}{} : {} = {}{}",
            if moves { "* " } else { "  " },
            truncate(&v.qualified_name, 44),
            v.type_name,
            truncate(b, 14),
            if moves {
                format!(" -> {}", truncate(a, 14))
            } else {
                String::new()
            },
        );
    }
    println!(
        "        {changed} of {} changed within 300ms (* = live)",
        hits.len()
    );
}

/// Expand a compound variable into its members, recursively.
///
/// Picks an explicitly named symbol when `--unfold` is given, otherwise the
/// first enumerated global that looks compound. Reports SKIP rather than FAIL
/// when the image has no struct/array global — that is a property of the target,
/// not a binding defect.
fn run_unfolding(
    r: &mut Report,
    winidea: &WinIdea,
    conn: &ConnectionMgr<'_>,
    vars: Option<&[Variable]>,
    requested: Option<&str>,
) {
    let mut ec = match ExpressionController::new(winidea, conn) {
        Ok(c) => {
            r.pass_named("ExpressionController::new()", "created");
            c
        }
        Err(e) => {
            r.fail_named("ExpressionController::new()", e);
            return;
        }
    };

    // Which globals look compound is a property of the toolchain's debug info,
    // not something to assume: this image names a 1-element array "uint32 [1]",
    // so the `[N]` suffix and `array_dimension` are independent signals and
    // "struct Foo" may never appear at all. Print the distinct type-name shapes
    // so the real vocabulary is visible rather than guessed at.
    if let Some(v) = vars {
        let mut shapes: std::collections::BTreeMap<String, usize> = Default::default();
        for x in v {
            // Collapse "uint32 [1]" / "uint32 [64]" to one bucket.
            let shape = match x.type_name.find('[') {
                Some(i) => format!("{}[]", &x.type_name[..i]),
                None => x.type_name.clone(),
            };
            *shapes.entry(shape).or_default() += 1;
        }
        let mut top: Vec<_> = shapes.into_iter().collect();
        top.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        println!("        type-name shapes (top 8 of {}):", top.len());
        for (shape, n) in top.iter().take(8) {
            println!("          {n:>6}  {shape}");
        }
        let with_dim = v.iter().filter(|x| x.array_dimension > 1).count();
        println!("        {with_dim} global(s) report array_dimension > 1");
    }

    // Prefer a variable the SDK itself calls an array; fall back to a
    // struct-ish type name. Skip the 1-element arrays that dominate this image
    // -- they unfold to a single member and prove nothing about recursion.
    let target = match requested {
        Some(s) => Some(s.to_string()),
        None => vars.and_then(|v| {
            v.iter()
                .find(|x| x.array_dimension > 1)
                .or_else(|| {
                    v.iter().find(|x| {
                        let t = x.type_name.trim();
                        t.starts_with("struct ") || t.starts_with("union ") || t.contains('{')
                    })
                })
                .map(|x| x.qualified_name.clone())
        }),
    };

    let Some(target) = target else {
        r.step("children()");
        r.skip("no compound global found (pass --unfold <symbol> to choose one)");
        return;
    };

    let children = r.check(
        &format!("children({})", truncate(&target, 24)),
        || ec.children(&target),
        |c| format!("-> {} member(s)", c.len()),
    );

    let Some(children) = children else { return };
    if children.is_empty() {
        r.step("unfold");
        // A variable the SDK itself reports as a multi-element array MUST have
        // members. Getting none back is a binding defect, not a scalar -- so it
        // fails rather than quietly skipping.
        let dim = vars
            .and_then(|v| v.iter().find(|x| x.qualified_name == target))
            .map(|x| x.array_dimension)
            .unwrap_or(0);
        if dim > 1 {
            r.fail(format!(
                "{target} has array_dimension {dim} but reported 0 members"
            ));
        } else {
            r.skip(&format!(
                "{target} reported no members (scalar?) -- pass --unfold <struct> to test properly"
            ));
        }
        return;
    }

    for c in children.iter().take(8) {
        println!(
            "        {} : {} ({} bytes{}){}",
            c.expression,
            c.type_name,
            c.size,
            if c.array_dimension > 0 {
                format!(", dim {}", c.array_dimension)
            } else {
                String::new()
            },
            if c.is_acquirable() {
                "  [acquirable]"
            } else {
                ""
            },
        );
    }

    // One level deeper, to prove the expression paths this builds are ones the
    // SDK accepts back. A path that only works at depth 1 would be useless for
    // a tree that expands arbitrarily far.
    if let Some(nested) = children.iter().find(|c| c.is_compound) {
        r.check(
            &format!("children({})  [nested]", truncate(&nested.expression, 20)),
            || ec.children(&nested.expression),
            |c| format!("-> {} member(s)", c.len()),
        );
    } else {
        r.step("children()  [nested]");
        r.skip("no compound member to recurse into");
    }

    let acquirable = children.iter().filter(|c| c.is_acquirable()).count();
    r.step("acquirable leaves");
    if acquirable == 0 {
        r.skip("no directly-acquirable member at this level");
    } else {
        r.pass(format!(
            "-> {acquirable}/{} member(s) can be DAQ items",
            children.len()
        ));
    }
}

/// Summarise a successful acquisition.
fn report_samples(r: &mut Report, samples: &[Sample], info: &Option<DaqInfo>) {
    let first = &samples[0];
    let last = &samples[samples.len() - 1];
    r.pass(format!(
        "-> {} samples, idx {}..{}, t {}..{}",
        samples.len(),
        first.index,
        last.index,
        first.time_ns,
        last.time_ns
    ));

    // Per-item counts, so it is obvious whether every configured symbol
    // actually produced data or only the first one did. Decoded values are
    // counted alongside: an item that samples but never decodes is the failure
    // mode worth seeing, and it is invisible in a bare sample count.
    let mut counts = std::collections::BTreeMap::new();
    for s in samples {
        let e = counts.entry(s.index).or_insert((0usize, 0usize));
        e.0 += 1;
        if s.value.is_some() {
            e.1 += 1;
        }
    }
    for (idx, (n, decoded)) in &counts {
        println!("        item {idx}: {n} samples, {decoded} decoded");
    }

    // The values themselves. This is what proves getDataValue() works: index
    // and timestamp alone would look identical if the payload never decoded.
    let with_values = samples.iter().filter(|s| s.value.is_some()).count();
    r.step("sample values");
    if with_values == 0 {
        r.fail("no sample decoded a value (getDataValue returned nothing usable)".to_string());
    } else {
        r.pass(format!(
            "-> {with_values}/{} samples decoded",
            samples.len()
        ));
        for s in samples.iter().filter(|s| s.value.is_some()).take(5) {
            // `Value` renders itself per its type tag, so this prints a real
            // number for a scalar rather than a byte blob.
            println!(
                "        t={} item {} = {}",
                s.time_ns,
                s.index,
                s.value.as_ref().expect("filtered to Some")
            );
        }
    }

    if let Some(i) = info
        && i.tick_ns > 0
        && samples.len() > 1
    {
        let span = last.time_ns.saturating_sub(first.time_ns);
        println!(
            "        span: {} ticks (~{} us)",
            span,
            span.saturating_mul(i.tick_ns as u64) / 1000
        );
    }
}

fn truncate(s: &str, max: usize) -> String {
    let one_line = s.replace(['\n', '\r'], " ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let cut: String = one_line.chars().take(max).collect();
        format!("{cut}...")
    }
}

fn finish(r: &Report) -> std::process::ExitCode {
    println!("\n======================");
    println!(
        "passed {}  failed {}  skipped {}",
        r.passed, r.failed, r.skipped
    );
    if r.failed == 0 {
        println!("\nAll checks that ran passed.");
        std::process::ExitCode::SUCCESS
    } else {
        println!("\nSome checks failed -- see the FAIL lines above.");
        std::process::ExitCode::FAILURE
    }
}
