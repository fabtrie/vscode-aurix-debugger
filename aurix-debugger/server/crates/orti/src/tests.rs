use std::path::PathBuf;

use crate::*;

/// The generated ORTI file this crate is developed against: MICROSAR OS on a TC377, two cores.
/// Absent on other machines, where the real-file tests skip.
fn test_orti() -> Option<PathBuf> {
    let p = std::env::var("AURIX_TEST_ORTI")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("C:/workspace/app/vector/Applications/Appl/GenData/Os_Trace.ORT"));
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

    // Half of these object types are the generator's own invention, not OSEK's.
    let decls: Vec<&str> = f.decls.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        decls,
        vec!["OS", "TASK", "STACK", "ALARM", "RESOURCE", "vs_ISR", "vs_SCHEDULETABLE", "vs_EVENT", "vs_SPINLOCK", "vs_OS_CONFIG"]
    );

    let count = |t: &str| f.objects_of(t).count();
    assert_eq!(
        (count("OS"), count("TASK"), count("STACK"), count("ALARM"), count("RESOURCE")),
        (1, 38, 50, 61, 3)
    );
    assert_eq!((count("vs_ISR"), count("vs_SPINLOCK"), count("vs_EVENT"), count("vs_OS_CONFIG")), (30, 26, 158, 1));
    // Declared but unused: the generator emits the type with no instances.
    assert_eq!(count("vs_SCHEDULETABLE"), 0);
    assert_eq!(f.objects.len(), 368);

    // The SMP extension, as Vector writes it.
    assert_eq!(f.core_count(), 2);
    let os = f.os().unwrap();
    assert_eq!(os.name, "TC37x");
    assert_eq!(os.attr("vs_SMP_NUMCPU"), Some("2"));
    assert_eq!(os.attr_at("RUNNINGTASK", 0), Some("OsCfg_Trace_OsCore0_Dyn.CurrentTask"));
    assert_eq!(os.attr_at("RUNNINGTASK", 1), Some("OsCfg_Trace_OsCore1_Dyn.CurrentTask"));
    assert_eq!(os.attr_at("vs_COREID", 1), Some("OsCfg_Core_OsCore1.Core.Id"));
    for a in ["vs_COREID", "RUNNINGTASK", "RUNNINGTASKPRIORITY", "RUNNINGISR2", "SERVICETRACE", "LASTERROR", "CURRENTAPPMODE"] {
        assert!(f.decl("OS").unwrap().attr(a).unwrap().indexed, "{a} should be per-core");
        assert!(os.attr_at(a, 0).is_some() && os.attr_at(a, 1).is_some(), "{a} missing a core");
    }
    // TOTRACE marks what a trace could record, and only some attributes carry it.
    let os_decl = f.decl("OS").unwrap();
    assert!(os_decl.attr("RUNNINGTASK").unwrap().totrace);
    assert!(!os_decl.attr("LASTERROR").unwrap().totrace);

    // Priority enums are an INVERSE map: the target field holds an ordinal and the enumerator
    // name is the priority to display. Reading 0 out of the task's Priority field means 700.
    let prio = f.decl("TASK").unwrap().attr("PRIORITY").unwrap();
    assert_eq!(prio.kind, AttrKind::Enum);
    assert_eq!(prio.enum_name(0), Some("700"));
    assert_eq!(prio.enum_name(27), Some("4294967295"));
    // ...while vs_HomePriorityInt is the same field read raw.
    assert_eq!(f.decl("TASK").unwrap().attr("vs_HomePriorityInt").unwrap().kind, AttrKind::CType);

    // Prose attributes, which must never reach the expression evaluator.
    let task = f.objects_of("TASK").find(|o| o.name == "Task_Core0").unwrap();
    assert_eq!(f.decl("TASK").unwrap().attr("vs_Schedule").unwrap().kind, AttrKind::String);
    assert_eq!(task.attr("vs_Schedule"), Some("FULL"));
    let alarm = f.objects_of("ALARM").find(|o| o.name == "Alarm_MainFunction").unwrap();
    assert_eq!(f.decl("ALARM").unwrap().attr("ACTION").unwrap().kind, AttrKind::String);
    assert_eq!(
        alarm.attr("ACTION"),
        Some("SET_EVENT Task_Core1 Event_MainFunction")
    );
    assert_eq!(alarm.attr("COUNTER"), Some("OsCounterCore1"));
    assert_eq!(f.objects_of("STACK").next().unwrap().attr("STACKDIRECTION"), Some("DOWN"));

    // Expression text survives verbatim, ternaries and casts included.
    assert!(task.attr("STATE").unwrap().contains("? 0 :"), "{:?}", task.attr("STATE"));
    let lock = f.objects_of("vs_SPINLOCK").find(|o| o.name == "OsSpinlock_A").unwrap();
    assert!(lock.attr("vs_STATE").unwrap().contains("~((Os_Hal_AddressType)0)"));
    assert_eq!(lock.attr("vs_LOCK_METHOD"), Some("LOCK_CAT2_INTERRUPTS"));

    // Object links, which make a value in a table clickable.
    let stack_attr = f.decl("TASK").unwrap().attr("STACK").unwrap();
    assert!(stack_attr.enums.iter().any(|e| e.link.as_deref() == Some("OsCfg_Stack_OsCore0_Task_Prio700")));
    let owner = f.decl("vs_SPINLOCK").unwrap().attr("vs_OWNER").unwrap();
    assert!(owner.enums.iter().any(|e| e.link.as_deref() == Some("Task_Core0")));
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
    assert!(checked > 1000, "only {checked} expression attributes checked");
    assert!(ternaries > 100, "expected the file to be full of ternaries, found {ternaries}");

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
