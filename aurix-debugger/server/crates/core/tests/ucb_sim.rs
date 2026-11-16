//! UCB read, review and guarded write against the simulator's DMU model.

use aurix_core::sim::SimAdapter;
use aurix_core::ucb::{self, get32, CONFIRMED};
use aurix_core::ucb_write::Origin;
use aurix_core::Server;
use serde_json::Value;

fn server(dir: &std::path::Path) -> Server {
    aurix_core::log::set_debug_level(0);
    let s = Server::new();
    s.cx().with(|c| c.adapter_override = Some("sim".into()));
    s.interp.eval_global("adapter driver sim").unwrap();
    s.interp.eval_global("init").unwrap();
    s.interp.eval_global(&format!("ucb dir {{{}}}", dir.display())).unwrap();
    s
}

fn tcl(s: &Server, cmd: &str) -> Result<String, String> {
    s.interp.eval_global(cmd)
}

fn json(s: &Server, cmd: &str) -> Value {
    serde_json::from_str(&tcl(s, cmd).unwrap_or_else(|e| panic!("{cmd}: {e}"))).unwrap()
}

fn sim<R>(s: &Server, f: impl FnOnce(&mut SimAdapter) -> R) -> R {
    s.cx().with(|c| f(c.adapter.as_mut().unwrap().as_any_mut().downcast_mut::<SimAdapter>().unwrap()))
}

/// A reset: the DMU installs the UCBs, the session forgets what it read.
fn reset(s: &Server) {
    sim(s, |a| a.dmu.install());
    s.cx().with(|c| {
        c.ucb.written_since_reset.clear();
        c.ucb.readout = None;
        c.ucb.review = None;
    });
}

fn ucb_of<'a>(v: &'a Value, name: &str) -> &'a Value {
    v["ucbs"].as_array().unwrap().iter().find(|u| u["name"] == name).unwrap()
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("aurix-ucb-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn review_token(s: &Server) -> (Value, String) {
    let rv = json(s, "ucb review -json");
    let t = rv["token"].as_str().unwrap().to_string();
    (rv, t)
}

#[test]
fn read_review_write_and_export() {
    let dir = tmp("main");
    let s = server(&dir);

    // Read: device, status, states, defaults.
    let v = json(&s, "ucb read -json");
    assert_eq!(v["device"]["name"], "TC37x");
    assert_eq!(v["device"]["pf_sectors"], serde_json::json!([192, 192]));
    assert_eq!(v["status"]["boot"], "0xA0000000 (BMHD0)");
    assert_eq!(ucb_of(&v, "DBG")["state"], "UNLOCKED");
    assert_eq!(ucb_of(&v, "SSW")["state"], "CONFIRMED");
    assert!(ucb_of(&v, "BMHD0")["changed"].as_u64().unwrap() > 0);
    assert_eq!(ucb_of(&v, "BMHD1")["changed"], 0);
    // The baseline of the chip is saved on the first readout.
    assert!(std::fs::read_dir(dir.join("baseline")).unwrap().count() == 1);

    // Stage a second boot header by field; the checksum is computed.
    tcl(&s, "ucb set BMHD1.STAD 0xA0000000").unwrap();
    tcl(&s, "ucb set BMHD1.BMI_BMHDID.HWCFG {Internal flash}").unwrap();
    assert!(tcl(&s, "ucb set BMHD1.CRCBMHD 1").is_err(), "checksums are not edited");
    assert!(tcl(&s, "ucb set SSW.+0x000 1").is_err(), "Infineon data is read only");
    let (rv, token) = review_token(&s);
    assert_eq!(rv["blocked"], false, "{rv}");
    assert_eq!(rv["changes"][0]["ucb"], "BMHD1");
    assert_eq!(rv["changes"][0]["targets"][0]["method"], "program", "an empty UCB needs no erase");

    // Writing: only with the flag, only from a person, only with the token of the review.
    assert!(tcl(&s, &format!("ucb write -token {token}")).unwrap_err().contains("ucbWrite"));
    s.cx().with(|c| c.ucb.write_enabled = true);
    s.cx().with(|c| c.tcl_origin = Origin::Api);
    assert!(tcl(&s, &format!("ucb write -token {token}")).unwrap_err().contains("REST/MCP"));
    // Also when nested in a script.
    assert!(tcl(&s, &format!("eval [list ucb write -token {token}]")).unwrap_err().contains("REST/MCP"));
    s.cx().with(|c| c.tcl_origin = Origin::Person);
    assert!(tcl(&s, "ucb write -token 0123").unwrap_err().contains("token"));
    let out = json(&s, &format!("ucb write -json -noreset -token {token}"));
    assert_eq!(out["written"].as_array().unwrap().len(), 2);
    assert_eq!(out["written"][0]["index"], 9, "COPY first");
    assert!(std::path::Path::new(out["backup"].as_str().unwrap()).is_file());
    assert!(std::fs::read_to_string(dir.join("ucb-log.txt")).unwrap().contains("UCB_BMHD1"));
    let (b1, b9) = sim(&s, |a| (a.dmu.ucb[1], a.dmu.ucb[9]));
    assert!(ucb::bmhd_valid(&b1) && b1 == b9);

    // HSM boot without HSM code in the selected sector. No core is examined here, so nobody knows
    // the cores are halted: the review defers the flash check (no DMU command), the write does it
    // after halting and refuses.
    for k in 0..4 {
        tcl(&s, &format!("ucb set HSMCOTP0.PROCONHSMCBS.BOOTSEL{k} 5")).unwrap();
    }
    tcl(&s, "ucb set HSMCOTP0.PROCONHSMCFG.HSMBOOTEN yes").unwrap();
    let (rv, token) = review_token(&s);
    assert_eq!(rv["checks_deferred"], true, "{rv}");
    assert!(rv["problems"].as_array().unwrap().iter().any(|p| p["title"].as_str().unwrap().contains("checked when writing")), "{rv}");
    assert_eq!(rv["confirm"]["level"], "type");
    let e = tcl(&s, &format!("ucb write -noreset -token {token} -confirm TC37x")).unwrap_err();
    assert!(e.contains("no HSM code"), "{e}");
    // With code in that sector it passes, and needs the device name typed.
    sim(&s, |a| a.dmu.programmed_sectors.insert(0xA001_4000));
    let (rv, token) = review_token(&s);
    assert_eq!(rv["blocked"], false, "{rv}");
    assert!(tcl(&s, &format!("ucb write -noreset -token {token}")).unwrap_err().contains("-confirm TC37x"));
    tcl(&s, &format!("ucb write -noreset -token {token} -confirm TC37x")).unwrap();
    tcl(&s, "ucb discard").unwrap();

    // Debug lock with a password: written UNLOCKED, confirmed only after a reset.
    tcl(&s, "ucb set DBG.PW 11111111 22222222 33333333 44444444 55555555 66666666 77777777 88888888").unwrap();
    tcl(&s, "ucb set DBG.PROCONDBG.DBGIFLCK yes").unwrap();
    let (rv, token) = review_token(&s);
    assert_eq!(rv["confirm"]["level"], "password");
    tcl(&s, &format!("ucb write -noreset -token {token}")).unwrap();
    assert_eq!(s.cx().with(|c| c.adapter_settings.debug_password), Some([0x1111_1111, 0x2222_2222, 0x3333_3333, 0x4444_4444, 0x5555_5555, 0x6666_6666, 0x7777_7777, 0x8888_8888]));
    tcl(&s, "ucb confirm DBG").unwrap();
    let (rv, _) = review_token(&s);
    assert!(rv["problems"].as_array().unwrap().iter().any(|p| p["title"] == "Reset first"), "{rv}");
    reset(&s);
    tcl(&s, "ucb confirm DBG").unwrap();
    let (rv, token) = review_token(&s);
    assert_eq!(rv["blocked"], false, "{rv}");
    assert_eq!(rv["changes"][0]["targets"][0]["method"], "program", "UNLOCKED -> CONFIRMED is an over-program");
    tcl(&s, &format!("ucb write -noreset -token {token}")).unwrap();
    assert_eq!(sim(&s, |a| get32(&a.dmu.ucb[18], 0x1F0)), CONFIRMED);
    reset(&s);
    // Now read-protected: unreadable until unlocked with the password.
    let v = json(&s, "ucb read -json DBG");
    assert_eq!(ucb_of(&v, "DBG")["words"][0]["readable"], false);
    tcl(&s, "ucb password DBG 0 0 0 0 0 0 0 1").unwrap();
    assert!(tcl(&s, "ucb unlock DBG").unwrap_err().contains("wrong password"));
    tcl(&s, "ucb password DBG 11111111 22222222 33333333 44444444 55555555 66666666 77777777 88888888").unwrap();
    assert!(tcl(&s, "ucb unlock DBG").is_err(), "blocked until the next reset");
    reset(&s);
    tcl(&s, "ucb unlock DBG").unwrap();
    let v = json(&s, "ucb read -json DBG");
    assert_eq!(ucb_of(&v, "DBG")["words"][0]["readable"], true);

    // Export: a selection of UCBs as Intel HEX.
    let f = dir.join("boot.hex");
    let ex = json(&s, &format!("ucb export {{{}}} -json -class boot -unconfirmed", f.display()));
    assert_eq!(ex["exported"], serde_json::json!(["BMHD0", "BMHD1", "BMHD2", "BMHD3"]));
    let img = aurix_core::image::Image::from_ihex(&std::fs::read_to_string(&f).unwrap()).unwrap();
    assert_eq!(img.size(), 8 * ucb::SIZE);
    let ex = json(&s, &format!("ucb export {{{}}} -json -changed -which orig", f.display()));
    let names: Vec<&str> = ex["exported"].as_array().unwrap().iter().map(|n| n.as_str().unwrap()).collect();
    assert!(names.contains(&"BMHD1") && names.contains(&"DBG") && !names.contains(&"BMHD2"), "{names:?}");
    assert_eq!(ex["contains_passwords"], true);

    // The HEX path: the UCB records of an image file, reviewed like edits.
    let mut b2 = sim(&s, |a| a.dmu.ucb[1]);
    ucb::put32(&mut b2, 4, 0xA000_0000);
    let hex = aurix_core::image::Image::from_sections(vec![aurix_core::image::Section { addr: ucb::ucb_addr(2), data: b2.to_vec() }]);
    let hf = dir.join("bmhd2.hex");
    std::fs::write(&hf, hex.to_ihex()).unwrap();
    let rv = json(&s, &format!("ucb review -json {{{}}}", hf.display()));
    assert_eq!(rv["changes"][0]["ucb"], "BMHD2");
    assert!(rv["notes"][0].as_str().unwrap().contains("ORIG and COPY"));
    let token = rv["token"].as_str().unwrap();
    tcl(&s, &format!("ucb write -noreset -token {token}")).unwrap();
    assert_eq!(sim(&s, |a| a.dmu.ucb[10]), b2);
}

/// Compare the device with the UCB records of a HEX file, and stage from it per field, per UCB or all.
#[test]
fn compare_with_hex_and_apply() {
    use aurix_core::image::{Image, Section};
    let dir = tmp("compare");
    let s = server(&dir);
    tcl(&s, "ucb read").unwrap();
    let dev = |i: usize| sim(&s, |a| a.dmu.ucb[i]);
    // The file: a boot header in BMHD1 (ORIG only), PROCONRAM in DFLASH (ORIG and COPY), a debug
    // lock with a password in DBG (ORIG only), and other Infineon data in SSW.
    let mut b1 = dev(1);
    ucb::put32(&mut b1, 0, 0xB359_007E);
    ucb::put32(&mut b1, 4, 0xA000_0000);
    let crc = ucb::bmhd_crc(0xB359_007E, 0xA000_0000);
    ucb::put32(&mut b1, 8, crc);
    ucb::put32(&mut b1, 12, !crc);
    let mut df = dev(17);
    ucb::put32(&mut df, 8, 3);
    let mut dbg = dev(18);
    ucb::put32(&mut dbg, 0, 2);
    for k in 0..8 {
        ucb::put32(&mut dbg, 0x100 + 4 * k, 0x1111_0000 + k as u32);
    }
    let mut ssw = dev(4);
    ucb::put32(&mut ssw, 0xE8, 0x1234_5678);
    let sec = |i: u8, c: &ucb::Content| Section { addr: ucb::ucb_addr(i), data: c.to_vec() };
    let img = Image::from_sections(vec![sec(1, &b1), sec(17, &df), sec(25, &df), sec(18, &dbg), sec(4, &ssw)]);
    let f = dir.join("post_build.hex");
    std::fs::write(&f, img.to_ihex()).unwrap();

    let v = json(&s, &format!("ucb compare -json {{{}}}", f.display()));
    assert_eq!((v["in_file"].as_u64(), v["differ"].as_u64(), v["applicable"].as_u64()), (Some(4), Some(4), Some(3)), "{v}");
    let u = |v: &Value, n: &str| v["ucbs"].as_array().unwrap().iter().find(|u| u["name"] == n).unwrap().clone();
    assert_eq!(u(&v, "BMHD1")["in_file"], "orig");
    assert_eq!(u(&v, "DFLASH")["in_file"], "both");
    assert_eq!(u(&v, "BMHD2")["in_file"], "none");
    assert!(u(&v, "SSW")["read_only_reason"].is_string());
    // BMHD1: HWCFG, LSENA0..2 and STAD differ; the header ID and checksums are computed, not counted.
    assert_eq!(u(&v, "BMHD1")["differs"], 5, "{}", u(&v, "BMHD1"));
    // Passwords are compared but never shown.
    let pw0 = u(&v, "DBG")["words"].as_array().unwrap().iter().find(|w| w["name"] == "PW0").unwrap()["fields"][0].clone();
    assert_eq!((pw0["dev_text"].as_str(), pw0["file_text"].as_str(), pw0.get("file").is_none()), (Some("zero"), Some("set"), true));

    // One field: only STAD; the header ID and checksum follow.
    tcl(&s, "ucb apply BMHD1.STAD").unwrap();
    let v = json(&s, "ucb compare -json");
    let stad = u(&v, "BMHD1")["words"][1]["fields"][0].clone();
    assert_eq!((stad["taken"].as_bool(), u(&v, "BMHD1")["taken"].as_u64()), (Some(true), Some(1)));
    // Computed fields cannot be applied; read-only UCBs neither.
    assert!(tcl(&s, "ucb apply BMHD1.CRCBMHD.CRC").unwrap_err().contains("computed"));
    assert!(tcl(&s, "ucb apply SSW").unwrap_err().contains("read only"));
    // A whole UCB.
    tcl(&s, "ucb apply BMHD1").unwrap();
    assert_eq!(u(&json(&s, "ucb compare -json"), "BMHD1")["taken"], 5);
    // The password alone.
    tcl(&s, "ucb apply DBG.PW").unwrap();
    // Undo the password alone: back to the device's, the rest of DBG stays.
    tcl(&s, "ucb revert DBG.PW").unwrap();
    assert_eq!(u(&json(&s, "ucb compare -json"), "DBG")["taken"], 0);
    // All: the rest, SSW skipped with the reason.
    let out = tcl(&s, "ucb apply all").unwrap();
    assert!(out.contains("UCB_DFLASH") && out.contains("not applied: UCB_SSW"), "{out}");
    let v = json(&s, "ucb compare -json");
    for n in ["BMHD1", "DFLASH", "DBG"] {
        assert_eq!(u(&v, n)["taken"], u(&v, n)["differs"], "{n}");
    }
    // What was taken is reviewed like any edit.
    let (rv, _) = review_token(&s);
    let names: Vec<&str> = rv["changes"].as_array().unwrap().iter().map(|c| c["ucb"].as_str().unwrap()).collect();
    assert_eq!(names, ["BMHD1", "DFLASH", "DBG"], "{rv}");
    assert!(rv["problems"].as_array().unwrap().iter().any(|p| p["title"] == "Debug interface locked"));
    tcl(&s, "ucb compare -close").unwrap();
    assert!(tcl(&s, "ucb apply all").unwrap_err().contains("ucb compare"));
}

/// A power cut at any step of rewriting BMHD0 leaves a device that boots.
#[test]
fn power_cut_never_loses_the_boot_header() {
    let dir = tmp("cut");
    let s = server(&dir);
    s.cx().with(|c| c.ucb.write_enabled = true);
    tcl(&s, "ucb set BMHD0.STAD 0xA0001000").unwrap();
    let (_, token) = review_token(&s);
    let out = json(&s, &format!("ucb write -json -noreset -token {token}"));
    let total: u64 = out["written"].as_array().unwrap().iter().map(|w| w["pages"].as_u64().unwrap() + 1).sum();
    for cut in 0..total as usize {
        let s = server(&dir);
        s.cx().with(|c| c.ucb.write_enabled = true);
        tcl(&s, "ucb set BMHD0.STAD 0xA0001000").unwrap();
        let (_, token) = review_token(&s);
        sim(&s, |a| a.dmu.cut_after = Some(cut));
        assert!(tcl(&s, &format!("ucb write -noreset -token {token}")).is_err(), "cut after {cut} commands");
        let ok = sim(&s, |a| {
            a.dmu.install();
            ucb::bmhd_valid(&a.dmu.ucb[0]) || ucb::bmhd_valid(&a.dmu.ucb[8])
        });
        assert!(ok, "no valid boot header after a cut at command {cut}");
    }
}
