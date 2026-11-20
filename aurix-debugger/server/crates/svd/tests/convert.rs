//! The conversion against what the extension's TypeScript conversion made of the same files:
//! hand-written fixtures for the details (expected output written by the TypeScript), and the
//! SVD files bundled with the extension (by hash, `peripheral_options.sha256`).

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn definitions(d: &svd::Device) -> Value {
    serde_json::to_value(svd::PeripheralOptions(&d.peripherals)).unwrap()
}

fn expected(name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(fixture(name)).unwrap()).unwrap()
}

#[test]
fn fixture_as_the_extension_converted_it() {
    let d = svd::load(&fixture("stream.svd"), &[]).unwrap();
    assert_eq!(definitions(&d), expected("stream.json"));
    assert_eq!(d.name.as_deref(), Some("FIXTURE"));

    // What the view's definitions leave out.
    let zeta = d.peripherals.iter().find(|p| &*p.name == "ZETA").unwrap();
    let ctrl = &zeta.registers[0];
    let flag = ctrl.fields.iter().find(|f| &*f.name == "FLAG").unwrap();
    assert_eq!((flag.modified_write, flag.read_action, flag.access), (Some(svd::ModifiedWrite::OneToClear), Some(svd::ReadAction::Clear), Some(svd::Access::ReadOnly)));
    // A named enumeration is shared, not copied.
    let en = ctrl.fields.iter().find(|f| &*f.name == "EN").unwrap().enumeration.clone().unwrap();
    let cha = ctrl.fields.iter().find(|f| &*f.name == "CHA").unwrap().enumeration.clone().unwrap();
    assert!(std::sync::Arc::ptr_eq(&en, &cha));
}

#[test]
fn derived_peripherals_and_registers() {
    let d = svd::load(&fixture("derived.svd"), &[]).unwrap();
    assert_eq!(definitions(&d), expected("derived.json"));
}

#[test]
fn merged_extra_file() {
    let (replace, exclude) = (vec!["HSM".to_string()], vec!["HSM_BRIDGE_HSMSIDE".to_string()]);
    let extra = fixture("merge_extra.svd");
    let d = svd::load(&fixture("merge_device.svd"), &[svd::Extra { source: extra.as_path(), replace: &replace, exclude: &exclude }]).unwrap();
    assert_eq!(definitions(&d), expected("merge.json"));

    // Without replacing it, the extra file's HSM clashes with the device's.
    let none: Vec<String> = Vec::new();
    let e = svd::load(&fixture("merge_device.svd"), &[svd::Extra { source: extra.as_path(), replace: &none, exclude: &exclude }]).unwrap_err();
    assert!(e.to_string().contains("already defined by the device: HSM"), "{e}");
}

#[test]
fn errors() {
    let err = |xml: &str| svd::parse(xml, &[]).unwrap_err().to_string();
    assert!(err("<other/>").contains("no device element"));
    assert!(err("<device><name>X</name></device>").contains("no peripherals defined"));
    assert!(err("<device><peripherals>").starts_with("XML"));
    let dim = "<device><peripherals><peripheral><name>P</name><baseAddress>0</baseAddress><registers>\
               <register><name>R%s</name><dim>2</dim><addressOffset>0</addressOffset></register></registers></peripheral></peripherals></device>";
    assert!(err(dim).contains("no/invalid dimIncrement"), "{}", err(dim));
    let late = "<device><peripherals><peripheral><name>P</name><baseAddress>0</baseAddress><registers>\
                <register><name>R</name><addressOffset>0</addressOffset><fields><field><name>F</name><bitOffset>0</bitOffset><bitWidth>1</bitWidth>\
                <enumeratedValues derivedFrom=\"NOWHERE\"/></field></fields></register></registers></peripheral></peripherals></device>";
    assert!(err(late).contains("Invalid derivedFrom=NOWHERE"), "{}", err(late));
}

struct HashWriter(Sha256);

impl Write for HashWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.update(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// An extra file: path, the peripherals it replaces, those it excludes.
type ExtraFile = (PathBuf, Vec<String>, Vec<String>);

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Every SVD file bundled with the extension, and every device with its extra files merged,
/// gives exactly the definitions the TypeScript conversion gave. A file that differs from the
/// committed one (a local edit) is skipped.
#[test]
fn bundled_files_as_the_extension_converted_them() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../vscode-aurix-tcf-debugger/bin/svd");
    let Ok(index) = std::fs::read_to_string(dir.join("svd-index.json")) else {
        eprintln!("no {}, skipping the bundled SVD files", dir.display());
        return;
    };
    let index: Value = serde_json::from_str(&index).unwrap();
    let golden = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/peripheral_options.sha256")).unwrap();
    let file = |p: &Value| dir.join(Path::new(p.as_str().unwrap()).file_name().unwrap());
    let list = |v: &Value| -> Vec<String> { v.as_array().map(|a| a.iter().map(|s| s.as_str().unwrap().to_string()).collect()).unwrap_or_default() };
    let mut checked = 0;
    for line in golden.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let [name, input_sha, output_sha] = parts[..] else { panic!("bad line: {line}") };
        let (main, extras): (PathBuf, Vec<ExtraFile>) = match name.strip_suffix("-merged") {
            None => (dir.join(format!("{name}.svd.gz")), Vec::new()),
            Some(id) => {
                let e = index["entries"].as_array().unwrap().iter().find(|e| e["id"] == id).unwrap_or_else(|| panic!("{id} is not in svd-index.json"));
                let extras = e["extraSvd"].as_array().unwrap().iter().map(|x| (file(&x["path"]), list(&x["replace"]), list(&x["exclude"]))).collect();
                (file(&e["path"]), extras)
            }
        };
        let mut input = Sha256::new();
        for p in std::iter::once(&main).chain(extras.iter().map(|x| &x.0)) {
            input.update(std::fs::read(p).unwrap_or_else(|e| panic!("{}: {e}", p.display())));
        }
        if hex(&input.finalize()) != input_sha {
            eprintln!("{name}: the SVD file differs from the committed one, skipped");
            continue;
        }
        let x: Vec<svd::Extra<&Path>> = extras.iter().map(|(p, r, e)| svd::Extra { source: p.as_path(), replace: r, exclude: e }).collect();
        let d = svd::load(&main, &x).unwrap_or_else(|e| panic!("{name}: {e}"));
        let mut w = HashWriter(Sha256::new());
        serde_json::to_writer(&mut w, &svd::PeripheralOptions(&d.peripherals)).unwrap();
        assert_eq!(hex(&w.0.finalize()), output_sha, "{name}: the definitions differ from the extension's");
        checked += 1;
    }
    assert!(checked > 0, "no bundled SVD file checked");
}
