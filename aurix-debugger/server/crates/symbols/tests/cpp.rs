//! C++ symbols: the GCC builds of `testdata/cpp_test.cpp` (see there for how they were built).

use std::path::{Path, PathBuf};

use object::{Object, ObjectSection};
use symbols::expr::{Eval, Place, SymbolRef, Target, Value};
use symbols::Symbols;

fn elf(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name)
}

fn load(name: &str) -> Symbols {
    Symbols::load(&elf(name)).unwrap()
}

/// A halted target: memory from the ELF's sections; every other address (the stack) holds
/// `stack_word` in each word, and every register points to the stack.
struct Mock {
    sections: Vec<(u32, Vec<u8>)>,
    pc: Option<u32>,
    stack_word: u32,
}

impl Mock {
    fn new(name: &str, pc: Option<u32>, stack_word: u32) -> Mock {
        let data = std::fs::read(elf(name)).unwrap();
        let obj = object::File::parse(&*data).unwrap();
        let sections = obj
            .sections()
            .filter(|s| s.address() != 0)
            .map(|s| (s.address() as u32, s.uncompressed_data().map(|d| d.into_owned()).unwrap_or_default()))
            .collect();
        Mock { sections, pc, stack_word }
    }
}

impl Target for Mock {
    fn pc(&self) -> Option<u32> {
        self.pc
    }
    fn reg(&self, _: u16) -> Option<u32> {
        Some(0xd000_1000)
    }
    fn read(&mut self, addr: u32, len: usize) -> Result<Vec<u8>, String> {
        for (base, bytes) in &self.sections {
            if addr >= *base && (addr - base) as usize + len <= bytes.len() {
                let o = (addr - base) as usize;
                return Ok(bytes[o..o + len].to_vec());
            }
        }
        Ok((0..len).map(|i| self.stack_word.to_le_bytes()[(addr as usize + i) % 4]).collect())
    }
    fn symbol(&mut self, _: &str) -> Option<SymbolRef> {
        None
    }
}

fn eval(s: &Symbols, t: &mut Mock, text: &str) -> Value {
    Eval::new(s, t).evaluate(text).unwrap_or_else(|e| panic!("{text}: {e}"))
}

fn int(s: &Symbols, t: &mut Mock, text: &str) -> i64 {
    let mut ev = Eval::new(s, t);
    let v = ev.evaluate(text).unwrap_or_else(|e| panic!("{text}: {e}"));
    let b = ev.load(&v).unwrap();
    let mut w = [0u8; 8];
    w[..b.len()].copy_from_slice(&b);
    i64::from_le_bytes(w)
}

fn type_name(s: &Symbols, v: &Value) -> String {
    s.type_info(&v.ty).name.unwrap_or_default()
}

fn global_addr(s: &Symbols, name: &str) -> u32 {
    let gi = s.global_indices(name);
    assert_eq!(gi.len(), 1, "{name}");
    s.globals[gi[0]].addr
}

#[test]
fn functions_have_qualified_names() {
    for f in ["cpp_test_O0.elf", "cpp_test_O2.elf"] {
        let s = load(f);
        let names: Vec<&str> = s.functions.iter().map(|f| f.name.as_str()).collect();
        for n in ["app::motor::Controller::step", "app::motor::Controller::run", "app::motor::Controller::~Controller", "c_func", "main"] {
            assert!(names.contains(&n), "{f}: no {n} in {names:?}");
        }
        // Any trailing part of the qualified name finds it; a leading :: is ignored.
        let step = s.function_indices("app::motor::Controller::step");
        assert_eq!(step.len(), 1);
        for n in ["motor::Controller::step", "Controller::step", "step", "::app::motor::Controller::step"] {
            assert_eq!(s.function_indices(n), step, "{f}: {n}");
        }
        assert_eq!(s.resolve_location("Controller::step"), Some(s.functions[step[0]].low));
        // Both overloads.
        assert_eq!(s.resolve_locations("overloaded").len(), 2, "{f}");
    }

    let s = load("cpp_test_O0.elf");
    // Out-of-line definitions of inline members, constructors and destructors (DW_AT_abstract_origin).
    for n in ["app::motor::Controller::ref", "app::motor::Controller::Controller", "app::motor::Base::run", "app::Ring<short int, 4>::push"] {
        assert_eq!(s.function_indices(n).len(), 1, "{n}");
    }
    // The deleting and the complete-object destructor.
    assert_eq!(s.resolve_locations("Controller::~Controller").len(), 2);
    let push = s.functions_named("Ring<short int, 4>::push");
    assert_eq!(push.len(), 1);
    assert!(s.functions_named("push")[0].external);
}

#[test]
fn globals_have_qualified_names() {
    for f in ["cpp_test_O0.elf", "cpp_test_O2.elf"] {
        let s = load(f);
        for (n, short) in [
            ("app::motor::g_ctrl", "g_ctrl"),
            ("app::motor::g_mode", "motor::g_mode"),
            ("app::motor::counter", "counter"),
            ("app::motor::Controller::instances", "Controller::instances"),
            ("app::g_ring", "g_ring"),
            ("plain_global", "::plain_global"),
        ] {
            let gi = s.global_indices(n);
            assert_eq!(gi.len(), 1, "{f}: {n}");
            assert_eq!(s.global_indices(short), gi, "{f}: {short}");
            assert!(s.globals[gi[0]].external, "{f}: {n}");
        }
    }
    let s = load("cpp_test_O0.elf");
    let hidden = s.global_indices("hidden");
    assert_eq!(s.globals[hidden[0]].name, "(anonymous namespace)::hidden");
    assert!(!s.globals[hidden[0]].external);
}

#[test]
fn types_are_named_as_in_cpp() {
    let s = load("cpp_test_O0.elf");
    let ty = |n: &str| s.global_type(s.global_indices(n)[0]);
    let name = |n: &str| s.type_info(&ty(n)).name.unwrap();
    assert_eq!(name("g_ctrl"), "app::motor::Controller");
    assert_eq!(name("g_mode"), "app::motor::Mode");
    assert_eq!(name("g_ring"), "app::Ring<short int, 4>");
    assert_eq!(name("Controller::instances"), "int");
    assert!(s.is_union(&ty("g_word")));
    assert!(!s.is_union(&ty("g_ctrl")));

    // The base class is an unnamed member; the static member is not a member of the object.
    let members: Vec<Option<String>> = s.members(&ty("g_ctrl")).into_iter().map(|m| m.name).collect();
    assert_eq!(members, [None, Some("speed".into()), Some("limit".into())]);
    let id = s.find_member(&ty("g_ctrl"), "id").expect("member of the base class");
    assert_eq!(id.offset, 4, "after the vtable pointer");

    for n in ["app::motor::Controller", "motor::Controller", "Controller", "struct Controller"] {
        assert!(!s.types_named(n).is_empty(), "{n}");
    }
}

#[test]
fn expressions_with_cpp_names() {
    let s = load("cpp_test_O0.elf");
    let ctrl = global_addr(&s, "g_ctrl");
    let mut t = Mock::new("cpp_test_O0.elf", None, 0);

    assert_eq!(eval(&s, &mut t, "app::motor::g_ctrl.speed").place, Place::Memory(ctrl + 8));
    assert_eq!(eval(&s, &mut t, "::app::motor::g_ctrl.speed").place, Place::Memory(ctrl + 8));
    assert_eq!(eval(&s, &mut t, "motor::g_ctrl.id").place, Place::Memory(ctrl + 4));
    assert_eq!(eval(&s, &mut t, "(&g_ctrl)->limit").place, Place::Memory(ctrl + 12));
    assert_eq!(int(&s, &mut t, "app::motor::g_word.b[0]"), 0x44);
    assert_eq!(int(&s, &mut t, "plain_global + ::plain_global"), 6);
    assert_eq!(int(&s, &mut t, "app::motor::Mode::Fast"), 2);
    assert_eq!(int(&s, &mut t, "g_mode == Mode::Slow"), 1);
    assert_eq!(int(&s, &mut t, "1 ? Controller::instances : plain_global"), 0);
    assert_eq!(int(&s, &mut t, "sizeof(app::motor::Controller)"), 16);
    let p = eval(&s, &mut t, "(motor::Controller*)0x70000000");
    assert_eq!(type_name(&s, &p), "app::motor::Controller*");
}

#[test]
fn class_declared_in_the_unit_of_a_global() {
    // cpp_test2.cpp defines g_widget but only declares app::Widget (its vtable is elsewhere).
    for f in ["cpp_test_O0.elf", "cpp_test_O2.elf"] {
        let s = load(f);
        let gi = s.global_indices("g_widget")[0];
        let ty = s.global_type(gi);
        let info = s.type_info(&ty);
        assert_eq!((info.name.as_deref(), info.size), (Some("app::Widget"), 12), "{f}");
        let members: Vec<Option<String>> = s.members(&ty).into_iter().map(|m| m.name).collect();
        assert_eq!(members, [Some("_vptr.Widget".into()), Some("count".into()), Some("level".into())], "{f}");

        let w = s.globals[gi].addr;
        let mut t = Mock::new(f, None, 0);
        assert_eq!(eval(&s, &mut t, "g_widget.count").place, Place::Memory(w + 4));
        assert_eq!(eval(&s, &mut t, "(&g_widget)->level").place, Place::Memory(w + 8));
        assert_eq!(int(&s, &mut t, "sizeof(g_widget)"), 12);
    }
}

#[test]
fn static_members() {
    let s = load("cpp_test_O0.elf");
    let mut t = Mock::new("cpp_test_O0.elf", None, 0);
    let made = global_addr(&s, "app::Widget::made");
    assert_eq!(eval(&s, &mut t, "g_widget.made").place, Place::Memory(made));
    assert_eq!(int(&s, &mut t, "(&g_widget)->made"), 2);
    let instances = global_addr(&s, "Controller::instances");
    assert_eq!(eval(&s, &mut t, "g_ctrl.instances").place, Place::Memory(instances));
    assert!(Eval::new(&s, &mut t).evaluate("g_ctrl.nothing").is_err());

    // In a member function, through `this`.
    let ctrl = global_addr(&s, "g_ctrl");
    let f = s.functions_named("Controller::step")[0];
    let pc = s.lines_in_range(f.low, f.high).iter().map(|l| l.start).find(|a| *a > f.low).unwrap();
    let mut t = Mock::new("cpp_test_O0.elf", Some(pc), ctrl);
    assert_eq!(eval(&s, &mut t, "instances").place, Place::Memory(instances));
    assert_eq!(eval(&s, &mut t, "this->instances").place, Place::Memory(instances));
}

#[test]
fn locals_in_member_functions() {
    let s = load("cpp_test_O0.elf");
    let ctrl = global_addr(&s, "g_ctrl");
    let at = |f: &str| {
        let f = s.functions_named(f)[0];
        // Past the prologue, in the function's first line after its opening one.
        s.lines_in_range(f.low, f.high).iter().map(|l| l.start).find(|a| *a > f.low).unwrap()
    };

    // `this` points to g_ctrl; the class's members are found through it.
    let pc = at("Controller::step");
    let mut t = Mock::new("cpp_test_O0.elf", Some(pc), ctrl);
    let locals: Vec<String> = s.locals_at(pc).into_iter().map(|l| l.name).collect();
    assert!(locals.contains(&"this".into()) && locals.contains(&"n".into()), "{locals:?}");
    assert_eq!(eval(&s, &mut t, "speed").place, Place::Memory(ctrl + 8));
    assert_eq!(eval(&s, &mut t, "limit").place, Place::Memory(ctrl + 12));
    assert_eq!(eval(&s, &mut t, "id").place, Place::Memory(ctrl + 4));
    assert_eq!(eval(&s, &mut t, "this->speed").place, Place::Memory(ctrl + 8));
    assert_eq!(type_name(&s, &eval(&s, &mut t, "this")), "app::motor::Controller* const");

    // A reference reads as the object it refers to.
    let pc = at("add_ref");
    let mut t = Mock::new("cpp_test_O0.elf", Some(pc), ctrl);
    let r = eval(&s, &mut t, "r");
    assert_eq!((type_name(&s, &r), r.place), ("int".into(), Place::Memory(ctrl)));
    assert_eq!(type_name(&s, &eval(&s, &mut t, "k")), "const int");
    assert_eq!(type_name(&s, &eval(&s, &mut t, "&r")), "int*");

    // An out-of-line destructor names its parameters through DW_AT_abstract_origin.
    for f in s.functions_named("Controller::~Controller") {
        let names: Vec<String> = s.locals_at(f.low).into_iter().map(|l| l.name).collect();
        assert!(names.contains(&"this".into()), "{names:?}");
    }
}

#[test]
fn member_paths_for_a_variable_picker() {
    let s = load("cpp_test_O0.elf");
    let paths: Vec<(&str, u32, u32)> = s.member_paths().iter().map(|m| (m.expression.as_str(), m.address, m.size)).collect();
    let ctrl = global_addr(&s, "g_ctrl");
    let widget = global_addr(&s, "g_widget");
    for want in [
        // A member of the base class is named as a member of the object; no vtable pointer.
        ("app::motor::g_ctrl.id", ctrl + 4, 4),
        ("app::motor::g_ctrl.speed", ctrl + 8, 4),
        ("app::motor::g_ctrl.limit", ctrl + 12, 4),
        ("app::g_ring.buf", global_addr(&s, "g_ring"), 8),
        ("app::g_ring.head", global_addr(&s, "g_ring") + 8, 4),
        // A class the global's unit only declares.
        ("g_widget.count", widget + 4, 4),
        ("g_widget.level", widget + 8, 2),
    ] {
        assert!(paths.contains(&want), "{want:?} not in {paths:?}");
    }
    assert!(!paths.iter().any(|p| p.0.contains("_vptr")), "{paths:?}");
    // Every path evaluates to its address.
    let mut t = Mock::new("cpp_test_O0.elf", None, 0);
    for (e, addr, _) in &paths {
        assert_eq!(eval(&s, &mut t, e).place, Place::Memory(*addr), "{e}");
    }
}
