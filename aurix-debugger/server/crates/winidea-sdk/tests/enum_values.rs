//! Drift guard: the enum values our safe wrappers send must match the SDK's own.
//!
//! Enum discriminants are the one place a wrapper can be silently wrong. A bad
//! symbol name fails loudly at load time and a bad signature is caught by the
//! P/Invoke metadata, but a bad *integer* just configures the wrong sampling
//! rate or the wrong shutdown policy and reports success.
//!
//! `tools/enums.json` is extracted verbatim from `IConnectCSLib.dll` by
//! `tools/extract_enums.py`. Re-running that after an SDK upgrade will fail this
//! test if iSYSTEM ever renumbers a constant we depend on.

use std::collections::BTreeMap;

/// The subset of `tools/enums.json` this crate's wrappers rely on.
///
/// Parsed with a tiny hand-rolled scanner rather than pulling in serde_json as a
/// dev-dependency: the file is flat `{"Type": {"NAME": <int>}}` and this keeps
/// the crate's dependency footprint at exactly `thiserror` + `libloading`.
fn load(json: &str, want_type: &str) -> BTreeMap<String, i64> {
    let mut out = BTreeMap::new();

    // Find `"<want_type>": {` and take everything up to the matching `}`.
    let key = format!("\"{want_type}\"");
    let Some(start) = json.find(&key) else {
        panic!("enum type `{want_type}` not found in enums.json");
    };
    let rest = &json[start + key.len()..];
    let Some(open) = rest.find('{') else {
        panic!("malformed entry for `{want_type}`");
    };
    let Some(close) = rest[open..].find('}') else {
        panic!("unterminated entry for `{want_type}`");
    };
    let body = &rest[open + 1..open + close];

    for part in body.split(',') {
        let Some((name, value)) = part.split_once(':') else {
            continue;
        };
        let name = name.trim().trim_matches('"');
        let value = value.trim();
        if name.is_empty() || value.is_empty() {
            continue;
        }
        let parsed: i64 = value
            .parse()
            .unwrap_or_else(|_| panic!("non-integer value for {name}: {value}"));
        out.insert(name.to_string(), parsed);
    }
    assert!(!out.is_empty(), "no constants parsed for `{want_type}`");
    out
}

const ENUMS: &str = include_str!("../tools/enums.json");

#[test]
fn daq_sampling_flags_match_sdk() {
    let sdk = load(ENUMS, "CDAQController/EDAQSamplingFlags");

    // Mirrors winidea_sdk::daq::SamplingFlags::to_raw.
    assert_eq!(sdk["daqSampleMax"], 0, "SamplingFlags::Max");
    assert_eq!(sdk["daqSample1ms"], 1, "SamplingFlags::Every1ms");
    assert_eq!(sdk["daqSample10ms"], 2, "SamplingFlags::Every10ms");
    assert_eq!(sdk["daqSample100ms"], 3, "SamplingFlags::Every100ms");
    assert_eq!(sdk["daqSample1s"], 4, "SamplingFlags::Every1s");
}

#[test]
fn daq_acquisition_mode_matches_sdk() {
    let sdk = load(ENUMS, "CDAQConfig/EMode");

    // Mirrors winidea_sdk::daq::AcquisitionMode::to_raw.
    assert_eq!(sdk["Auto"], 0, "AcquisitionMode::Auto");
    assert_eq!(sdk["SW"], 1, "AcquisitionMode::Sw");
    assert_eq!(sdk["HW"], 2, "AcquisitionMode::Hw");
}

/// All 23 view types, in order. A wrong number opens the wrong pane -- harmless
/// but confusing, and silent.
#[test]
fn view_type_matches_sdk() {
    let sdk = load(ENUMS, "CIDEController/ViewType");

    // Mirrors winidea_sdk::ide::ViewType::to_raw, in declaration order.
    for (name, expect) in [
        ("Project", 0),
        ("Symbols", 1),
        ("Callstack", 2),
        ("Locals", 3),
        ("This", 4),
        ("Disassembly", 5),
        ("CoreRegisters", 6),
        ("Terminal", 7),
        ("Tasks", 8),
        ("SFRs", 9),
        ("Output", 10),
        ("Build", 11),
        ("FindInFiles", 12),
        ("Tools", 13),
        ("Script", 14),
        ("Progress", 15),
        ("Watch", 16),
        ("Memory", 17),
        ("SFR", 18),
        ("FrameworkPlugin", 19),
        ("Document", 20),
        ("SessionExplorer", 21),
        ("NoneView", 22),
    ] {
        assert_eq!(sdk[name], expect, "ViewType::{name}");
    }
}

/// `read_memory` decides per-MAU readability by comparing the SDK's status
/// byte against `Access_OK`. If that constant were not 0, every byte of a
/// perfectly good read would be reported unreadable.
#[test]
fn access_result_matches_sdk() {
    let sdk = load(ENUMS, "EAccessResult");

    // Mirrors winidea_sdk::data::ACCESS_OK.
    assert_eq!(sdk["Access_OK"], 0, "data::ACCESS_OK");
    assert_ne!(sdk["Access_Fail"], 0, "failure must be distinguishable from OK");
}

#[test]
fn symbol_flags_match_sdk() {
    let sdk = load(ENUMS, "IConnectDebug/ESymbolFlags");

    // Mirrors winidea_sdk::address::SymbolFlags constants.
    assert_eq!(sdk["sVariables"], 0x01, "SymbolFlags::VARIABLES");
    assert_eq!(sdk["sLabels"], 0x02, "SymbolFlags::LABELS");
    assert_eq!(sdk["sFunctions"], 0x04, "SymbolFlags::FUNCTIONS");
    assert_eq!(sdk["sLines"], 0x08, "SymbolFlags::LINES");
    assert_eq!(sdk["sConstants"], 0x10, "SymbolFlags::CONSTANTS");
    assert_eq!(sdk["sSourceCode"], 0x80, "SymbolFlags::SOURCE_CODE");

    // The symbol-kind bits must not collide with the scope field, or ORing a
    // kind would silently change the scope.
    let scope_mask = sdk["sScopeMask"];
    for name in ["sVariables", "sLabels", "sFunctions", "sConstants"] {
        assert_eq!(sdk[name] & scope_mask, 0, "{name} overlaps sScopeMask");
    }
}

#[test]
fn path_type_matches_sdk() {
    let sdk = load(ENUMS, "CIDEController/EPathType");

    // Mirrors winidea_sdk::ide::PathType::to_raw.
    assert_eq!(sdk["WINIDEA_EXE_DIR"], 0, "PathType::ExeDir");
    assert_eq!(sdk["WORKSPACE_DIR"], 1, "PathType::WorkspaceDir");
    assert_eq!(sdk["WORKSPACE_FILE_NAME"], 2, "PathType::WorkspaceFileName");
}

#[test]
fn timeout_mode_matches_sdk() {
    let sdk = load(ENUMS, "CExecutionController/ETimeoutMode");

    // Mirrors winidea_sdk::execution::TimeoutMode::to_raw.
    assert_eq!(sdk["TOUT_NONE"], 0, "TimeoutMode::None");
    assert_eq!(sdk["TOUT_1s"], 1, "TimeoutMode::OneSecond");
    assert_eq!(sdk["TOUT_10s"], 2, "TimeoutMode::TenSeconds");
    assert_eq!(sdk["TOUT_100s"], 3, "TimeoutMode::HundredSeconds");
    assert_eq!(sdk["TOUT_INF"], 4, "TimeoutMode::Infinite");
}

/// `EAccessFlags` is the first parameter of every memory/register/expression
/// call — the SDK calls it `accessFlags`, NOT a core or context index.
#[test]
fn access_flags_match_sdk() {
    let sdk = load(ENUMS, "IConnectDebug/EAccessFlags");

    // Mirrors winidea_sdk::debug::AccessFlags constants.
    assert_eq!(sdk["fRealTime"], 0x0000_0001, "AccessFlags::REAL_TIME");
    assert_eq!(sdk["fNoRefresh"], 0x0000_0040, "AccessFlags::NO_REFRESH");
    assert_eq!(sdk["fDirect"], 0x0000_0080, "AccessFlags::DIRECT");

    // AccessFlags::DEFAULT is 0, which the SDK spells as these "none" flags.
    assert_eq!(sdk["fCacheNever"], 0, "AccessFlags::DEFAULT");
    assert_eq!(sdk["fMonitor"], 0);

    // Every named flag must fit the SDK's overall mask.
    let mask = sdk["fAccessFlagsMask"];
    for name in ["fRealTime", "fNoRefresh", "fDirect"] {
        assert_eq!(sdk[name] & !mask, 0, "{name} escapes fAccessFlagsMask");
    }
}

/// These caught a real bug: the `bHW_acc*` flags live at bit 16, but a
/// plausible-looking bits-0-and-1 encoding compiles fine and silently sets an
/// unrelated breakpoint flag.
#[test]
fn hw_access_flags_match_sdk() {
    let sdk = load(ENUMS, "IConnectDebug/EBreakpointFlags");

    // Mirrors winidea_sdk::debug::HwAccess::to_raw.
    assert_eq!(sdk["bHW_accAny"], 0x0000_0000, "HwAccess::Any");
    assert_eq!(sdk["bHW_accRD"], 0x0001_0000, "HwAccess::Read");
    assert_eq!(sdk["bHW_accWR"], 0x0002_0000, "HwAccess::Write");
    assert_eq!(sdk["bHW_accRW"], 0x0003_0000, "HwAccess::ReadWrite");
    assert_eq!(sdk["bHW_accFetch"], 0x0004_0000, "HwAccess::Fetch");

    // Every access value must fit inside the SDK's own mask for that field.
    let mask = sdk["bHW_accMask"];
    for name in ["bHW_accAny", "bHW_accRD", "bHW_accWR", "bHW_accRW", "bHW_accFetch"] {
        assert_eq!(sdk[name] & !mask, 0, "{name} escapes bHW_accMask");
    }

    // Read|Write == ReadWrite, confirming these are bit flags rather than an
    // ordinal sequence we could have mis-encoded as 0,1,2,3.
    assert_eq!(sdk["bHW_accRD"] | sdk["bHW_accWR"], sdk["bHW_accRW"]);
}

#[test]
fn detach_flags_match_sdk() {
    let sdk = load(ENUMS, "IConnect/EDetachFlags");

    // Mirrors winidea_sdk::connection::{CloseServer,AutoSave}::to_raw.
    assert_eq!(sdk["dfCloseServerNone"], 0x00, "CloseServer::Never");
    assert_eq!(sdk["dfCloseServerIfLastClient"], 0x01, "CloseServer::IfLastClient");
    assert_eq!(sdk["dfCloseServerUnconditional"], 0x02, "CloseServer::Unconditional");
    assert_eq!(sdk["dfCloseServerMask"], 0x0F, "CloseServer::MASK");

    assert_eq!(sdk["dfCloseAutoSaveDefault"], 0x00, "AutoSave::Default");
    assert_eq!(sdk["dfCloseAutoSaveAll"], 0x10, "AutoSave::All");
    assert_eq!(sdk["dfCloseAutoSaveNone"], 0x20, "AutoSave::None");
    assert_eq!(sdk["dfCloseAutoSaveMask"], 0x30, "AutoSave::MASK");

    // The two fields must occupy disjoint bits, or combining them corrupts both.
    assert_eq!(
        sdk["dfCloseServerMask"] & sdk["dfCloseAutoSaveMask"],
        0,
        "close-server and autosave bits overlap; DetachFlags::to_raw is unsound"
    );
}
