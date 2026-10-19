use std::path::PathBuf;

use crate::*;

/// A generated ORTI file (AURIX_TEST_ORTI), e.g. MICROSAR OS on a TC377 with two cores.
/// Without it the real-file tests skip.
fn test_orti() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("AURIX_TEST_ORTI")?);
    p.exists().then_some(p)
}

#[test]
fn parses_the_grammar() {
    let f = parse(
        r#"
        /* a header comment */
        VERSION {
          KOIL = "2.3";
          OSSEMANTICS = "ORTI", "2.3";
          OTHER = "x", "y";
        };
        IMPLEMENTATION Vector_ORTI {
          OS {
            CTYPE "int" vs_SMP_NUMCPU, "Number of CPUs";
            TOTRACE ENUM "void *" [
              "Idle" : Idle = "&Trace_Idle",
              "INVALID_TASK" = "0x0"
            ] RUNNINGTASK[], "Running Task Identification";
          }, "OS";
          STACK {
            CTYPE "unsigned long" SIZE, "Stack Size (Byte)";
            STRING STACKDIRECTION, "Stack Direction";
          }, "Stacks";
        }; /* IMPLEMENTATION */

        OS TC37x {
          vs_SMP_NUMCPU = "2";
          RUNNINGTASK[0] = "Trace_Core0_Dyn.CurrentTask";
          RUNNINGTASK[1] = "Trace_Core1_Dyn.CurrentTask";
        };
        STACK Idle_Stack {
          SIZE = "sizeof(Stack_Idle_Dyn)";
          STACKDIRECTION = "DOWN";
        };
        "#,
    )
    .unwrap();

    assert_eq!(f.version.koil.as_deref(), Some("2.3"));
    assert_eq!(f.version.semantics.as_deref(), Some("ORTI"));
    assert!(f.version.is_2_3());
    assert_eq!(f.version.other, vec![("OTHER".to_string(), vec!["x".to_string(), "y".to_string()])]);
    assert_eq!(f.implementation, "Vector_ORTI");
    assert_eq!(f.decls.len(), 2);
    assert_eq!(f.decl("STACK").unwrap().desc, "Stacks");

    // The declaration - not the value - says whether an attribute is code or prose.
    let stack = f.decl("STACK").unwrap();
    assert_eq!(stack.attr("SIZE").unwrap().kind, AttrKind::CType);
    assert_eq!(stack.attr("STACKDIRECTION").unwrap().kind, AttrKind::String);

    let running = f.decl("OS").unwrap().attr("RUNNINGTASK").unwrap();
    assert!(running.indexed && running.totrace);
    assert_eq!(running.ctype.as_deref(), Some("void *"));
    assert_eq!(running.enums[0].link.as_deref(), Some("Idle"));
    assert_eq!(running.enums[1].value, EnumValue::Expr("0x0".into()));

    assert_eq!(f.core_count(), 2);
    let os = f.os().unwrap();
    assert_eq!(os.name, "TC37x");
    assert_eq!(os.attr_at("RUNNINGTASK", 1), Some("Trace_Core1_Dyn.CurrentTask"));
    // An indexed attribute is not reachable without its index, and the other way round.
    assert_eq!(os.attr("RUNNINGTASK"), None);
    assert_eq!(os.attr_at("vs_SMP_NUMCPU", 0), None);
    assert_eq!(f.objects_of("STACK").next().unwrap().attr("STACKDIRECTION"), Some("DOWN"));
}

#[test]
fn accepts_what_generators_vary_on() {
    // An unnamed OS object, a trailing comma in an enumerator list, and `//` comments.
    let f = parse(
        r#"
        IMPLEMENTATION X {
          OS {
            ENUM "unsigned char" [ "A" = 0, "B" = 1, ] MODE, "Mode"; // trailing comma
          }, "OS";
        };
        OS { MODE = "cfg.Mode"; };
        "#,
    )
    .unwrap();
    assert_eq!(f.os().unwrap().name, "");
    assert_eq!(f.decl("OS").unwrap().attr("MODE").unwrap().enums.len(), 2);
    // No VERSION block at all: 2.2 files and hand-written ones.
    assert!(!f.version.is_2_3());
    // A single-core file has no indices, and still answers one core.
    assert_eq!(f.core_count(), 1);
}

#[test]
fn enums_are_keyed_by_value_not_by_name() {
    // Real ORTI enumerator lists repeat names: RESOURCE.LOCKER has "Not evaluated" twice and
    // vs_LOCKED has "-" twice. Keying by name would collapse them and lose an entry.
    let f = parse(
        r#"
        IMPLEMENTATION X {
          RESOURCE {
            ENUM "void *" [ "-" = 0, "Nvm" : Nvm = 1, "-" = 2 ] LOCKED, "Locked";
          }, "Resources";
        };
        "#,
    )
    .unwrap();
    let a = f.decl("RESOURCE").unwrap().attr("LOCKED").unwrap();
    assert_eq!(a.enums.len(), 3);
    assert_eq!(a.enum_name(0), Some("-"));
    assert_eq!(a.enum_name(2), Some("-"));
    assert_eq!(a.enum_name(1), Some("Nvm"));
    assert_eq!(a.enum_link(1), Some("Nvm"));
    assert_eq!(a.enum_name(3), None);
}

#[test]
fn reports_where_a_file_is_wrong() {
    let e = parse("IMPLEMENTATION X {\n  OS {\n    WHAT \"int\" A, \"a\";\n  }, \"OS\";\n};").unwrap_err();
    assert_eq!((e.line, e.column), (3, 5));
    assert!(e.message.contains("CTYPE"), "{}", e.message);
    assert!(parse("OS X { A = ; };").is_err());
    assert!(parse("IMPLEMENTATION X { OS { CTYPE \"int\" A, \"a\"; }").is_err());
}

#[test]
fn parses_the_real_microsar_file() {
    let Some(path) = test_orti() else {
        eprintln!("test ORTI file not found, skipping");
        return;
    };
    let src = std::fs::read_to_string(&path).unwrap();
    let f = parse(&src).unwrap();

    assert_eq!(f.implementation, "Vector_ORTI");
    assert!(f.version.is_2_3(), "{:?}", f.version);

    // Half of the object types are the generator's own invention (vs_*), not OSEK's.
    let decls: Vec<&str> = f.decls.iter().map(|d| d.name.as_str()).collect();
    for d in ["OS", "TASK", "STACK", "ALARM", "RESOURCE"] {
        assert!(decls.contains(&d), "{d} not declared: {decls:?}");
    }
    assert!(decls.iter().any(|d| d.starts_with("vs_")), "{decls:?}");
    // Every object is of a declared type.
    assert!(f.objects.iter().all(|o| f.decl(&o.type_name).is_some()));
    let count = |t: &str| f.objects_of(t).count();
    assert_eq!(count("OS"), 1);
    assert!(count("TASK") > 0 && count("STACK") > 0);

    // The SMP extension, as Vector writes it.
    let os = f.os().unwrap();
    let cores = f.core_count();
    assert!(cores >= 1);
    if let Some(n) = os.attr("vs_SMP_NUMCPU") {
        assert_eq!(n.parse::<u32>().ok(), Some(cores));
    }
    for core in 0..cores {
        let running = os.attr_at("RUNNINGTASK", core).unwrap_or_else(|| panic!("RUNNINGTASK[{core}]"));
        assert!(running.starts_with("OsCfg_Trace_") && running.ends_with(".CurrentTask"), "{running}");
    }
    if cores > 1 {
        for a in ["vs_COREID", "RUNNINGTASK", "RUNNINGTASKPRIORITY", "RUNNINGISR2", "SERVICETRACE", "LASTERROR", "CURRENTAPPMODE"] {
            assert!(f.decl("OS").unwrap().attr(a).unwrap().indexed, "{a} should be per-core");
            assert!((0..cores).all(|c| os.attr_at(a, c).is_some()), "{a} missing a core");
        }
    }
    // TOTRACE marks what a trace could record, and only some attributes carry it.
    let os_decl = f.decl("OS").unwrap();
    assert!(os_decl.attr("RUNNINGTASK").unwrap().totrace);
    assert!(!os_decl.attr("LASTERROR").unwrap().totrace);

    // Priority enums are an INVERSE map: the target field holds an ordinal and the enumerator
    // name is the priority to display.
    let prio = f.decl("TASK").unwrap().attr("PRIORITY").unwrap();
    assert_eq!(prio.kind, AttrKind::Enum);
    let p0 = prio.enum_name(0).expect("a name for ordinal 0");
    assert!(p0.parse::<u64>().is_ok(), "priority {p0}");
    // ...while vs_HomePriorityInt is the same field read raw.
    assert_eq!(f.decl("TASK").unwrap().attr("vs_HomePriorityInt").unwrap().kind, AttrKind::CType);

    // Prose attributes, which must never reach the expression evaluator.
    let task = f.objects_of("TASK").next().unwrap();
    assert_eq!(f.decl("TASK").unwrap().attr("vs_Schedule").unwrap().kind, AttrKind::String);
    assert!(matches!(task.attr("vs_Schedule"), Some("FULL" | "NON")), "{:?}", task.attr("vs_Schedule"));
    assert_eq!(f.decl("ALARM").unwrap().attr("ACTION").unwrap().kind, AttrKind::String);
    if let Some(alarm) = f.objects_of("ALARM").next() {
        assert!(alarm.attr("ACTION").is_some_and(|a| !a.is_empty()), "{alarm:?}");
        assert!(alarm.attr("COUNTER").is_some(), "{alarm:?}");
    }
    assert_eq!(f.objects_of("STACK").next().unwrap().attr("STACKDIRECTION"), Some("DOWN"));

    // Expression text survives verbatim, ternaries and casts included.
    assert!(task.attr("STATE").unwrap().contains("? 0 :"), "{:?}", task.attr("STATE"));
    if let Some(lock) = f.objects_of("vs_SPINLOCK").next() {
        assert!(lock.attr("vs_STATE").unwrap().contains("~((Os_Hal_AddressType)0)"));
        assert!(lock.attr("vs_LOCK_METHOD").is_some_and(|m| m.starts_with("LOCK_")), "{lock:?}");
    }

    // Object links, which make a value in a table clickable.
    let stack_attr = f.decl("TASK").unwrap().attr("STACK").unwrap();
    let stacks: Vec<&str> = f.objects_of("STACK").map(|o| o.name.as_str()).collect();
    assert!(stack_attr.enums.iter().any(|e| e.link.as_deref().is_some_and(|l| stacks.contains(&l))));
    if let Some(owner) = f.decl("vs_SPINLOCK").and_then(|d| d.attr("vs_OWNER")) {
        let tasks: Vec<&str> = f.objects_of("TASK").map(|o| o.name.as_str()).collect();
        assert!(owner.enums.iter().any(|e| e.link.as_deref().is_some_and(|l| tasks.contains(&l))));
    }
}

/// Every attribute the panels have to *evaluate* must parse as a C expression. This is the seam
/// between the ORTI grammar and `symbols::expr`, and it is the reason the conditional operator
/// had to be added there: without it this test fails on hundreds of attributes.
#[test]
fn every_expression_attribute_parses_as_c() {
    let Some(path) = test_orti() else {
        eprintln!("test ORTI file not found, skipping");
        return;
    };
    let src = std::fs::read_to_string(&path).unwrap();
    let f = parse(&src).unwrap();

    // The only type named in a cast anywhere in this file. DWARF would answer the same: a
    // resolver that said yes to everything would turn `(x)` into a cast and misparse.
    let types = |n: &str| (n == "Os_Hal_AddressType").then_some(symbols::types::TypeId::Base(symbols::types::TypeClass::Cardinal, 4));

    let mut checked = 0usize;
    let mut ternaries = 0usize;
    let mut failures = Vec::new();
    for obj in &f.objects {
        let Some(decl) = f.decl(&obj.type_name) else { continue };
        for a in &obj.attrs {
            let Some(d) = decl.attr(&a.name) else {
                failures.push(format!("{} {}: undeclared attribute {}", obj.type_name, obj.name, a.name));
                continue;
            };
            if d.kind == AttrKind::String {
                continue;
            }
            checked += 1;
            match symbols::expr::parse(&a.value, &types) {
                Ok(e) => {
                    if matches!(e, symbols::expr::Expr::Cond(..)) {
                        ternaries += 1;
                    }
                }
                Err(e) => failures.push(format!("{} {} . {}: {e}\n    {}", obj.type_name, obj.name, a.name, a.value)),
            }
        }
    }
    assert!(failures.is_empty(), "{} of {checked} attributes did not parse:\n{}", failures.len(), failures.join("\n"));
    assert!(checked > 0 && ternaries > 0, "{checked} expression attributes checked, {ternaries} ternaries");

    // The enumerator expressions are evaluated too (`"Idle" = "&OsCfg_Trace_Idle"`).
    for d in &f.decls {
        for a in &d.attrs {
            for e in &a.enums {
                if let EnumValue::Expr(x) = &e.value {
                    assert!(symbols::expr::parse(x, &types).is_ok(), "{} . {} enumerator {:?}: {x}", d.name, a.name, e.name);
                }
            }
        }
    }
}

#[test]
fn untyped_enum_is_accepted() {
    let src = r#"IMPLEMENTATION X { OS { TOTRACE ENUM [ "a" = "&b[0]", "idle" = "0" ] RUNNINGTASK, "Running Task"; }, "OS"; };"#;
    let f = crate::parse::parse(src).expect("parses");
    let a = &f.decls[0].attrs[0];
    assert_eq!(a.ctype, None);
    assert_eq!(a.enums.len(), 2);
}
