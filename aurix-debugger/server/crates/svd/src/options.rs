//! The peripheral viewer's definitions (its `PeripheralOptions`, which the view takes from a
//! command instead of reading an SVD file): what the extension's TypeScript conversion returned.
//!
//! Keys are written in sorted order (enumeration values by their decimal text, as JSON object
//! keys sort): the order does not matter to the view, and a canonical form makes two outputs
//! comparable byte for byte.

use serde::ser::{SerializeMap, SerializeSeq, Serializer};
use serde::Serialize;

use crate::{Access, Cluster, Enumeration, Field, Peripheral, ReadAction, Register};

/// The device's peripherals as the view's definitions: `serde_json::to_writer(w,
/// &PeripheralOptions(&device.peripherals))`.
pub struct PeripheralOptions<'a>(pub &'a [Peripheral]);

impl Serialize for PeripheralOptions<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for p in self.0 {
            seq.serialize_element(&P(p))?;
        }
        seq.end()
    }
}

fn access(a: Access) -> u8 {
    a as u8
}

fn read_action(a: ReadAction) -> u8 {
    a as u8
}

struct P<'a>(&'a Peripheral);
struct C<'a>(&'a [Cluster]);
struct R<'a>(&'a [Register]);
struct F<'a>(&'a [Field]);
struct E<'a>(&'a Enumeration);

impl Serialize for P<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let p = self.0;
        let mut m = s.serialize_map(None)?;
        m.serialize_entry("accessType", &access(p.access))?;
        m.serialize_entry("baseAddress", &p.base_address)?;
        if !p.clusters.is_empty() {
            m.serialize_entry("clusters", &C(&p.clusters))?;
        }
        m.serialize_entry("description", &*p.description)?;
        if let Some(g) = &p.group_name {
            m.serialize_entry("groupName", &**g)?;
        }
        m.serialize_entry("name", &*p.name)?;
        if !p.registers.is_empty() {
            m.serialize_entry("registers", &R(&p.registers))?;
        }
        m.serialize_entry("resetValue", &p.reset_value)?;
        m.serialize_entry("size", &p.size)?;
        m.serialize_entry("totalLength", &p.total_length)?;
        m.end()
    }
}

impl Serialize for C<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for c in self.0 {
            seq.serialize_element(&OneCluster(c))?;
        }
        seq.end()
    }
}

struct OneCluster<'a>(&'a Cluster);

impl Serialize for OneCluster<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let c = self.0;
        let mut m = s.serialize_map(None)?;
        if let Some(a) = c.access {
            m.serialize_entry("accessType", &access(a))?;
        }
        m.serialize_entry("addressOffset", &c.address_offset)?;
        if !c.clusters.is_empty() {
            m.serialize_entry("clusters", &C(&c.clusters))?;
        }
        m.serialize_entry("description", &*c.description)?;
        m.serialize_entry("name", &*c.name)?;
        if !c.registers.is_empty() {
            m.serialize_entry("registers", &R(&c.registers))?;
        }
        if let Some(r) = c.reset_value {
            m.serialize_entry("resetValue", &r)?;
        }
        if let Some(z) = c.size {
            m.serialize_entry("size", &z)?;
        }
        m.end()
    }
}

impl Serialize for R<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for r in self.0 {
            seq.serialize_element(&OneRegister(r))?;
        }
        seq.end()
    }
}

struct OneRegister<'a>(&'a Register);

impl Serialize for OneRegister<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let r = self.0;
        let mut m = s.serialize_map(None)?;
        if let Some(a) = r.access {
            m.serialize_entry("accessType", &access(a))?;
        }
        m.serialize_entry("addressOffset", &r.address_offset)?;
        m.serialize_entry("description", &*r.description)?;
        m.serialize_entry("fields", &F(&r.fields))?;
        m.serialize_entry("name", &*r.name)?;
        if let Some(a) = r.read_action {
            m.serialize_entry("readAction", &read_action(a))?;
        }
        if let Some(v) = r.reset_value {
            m.serialize_entry("resetValue", &v)?;
        }
        if let Some(z) = r.size {
            m.serialize_entry("size", &z)?;
        }
        m.end()
    }
}

impl Serialize for F<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for f in self.0 {
            seq.serialize_element(&OneField(f))?;
        }
        seq.end()
    }
}

struct OneField<'a>(&'a Field);

impl Serialize for OneField<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let f = self.0;
        let mut m = s.serialize_map(None)?;
        if let Some(a) = f.access {
            m.serialize_entry("accessType", &access(a))?;
        }
        m.serialize_entry("description", &*f.description)?;
        if let Some(e) = &f.enumeration {
            m.serialize_entry("enumeration", &E(e))?;
        }
        m.serialize_entry("name", &*f.name)?;
        m.serialize_entry("offset", &f.offset)?;
        if let Some(a) = f.read_action {
            m.serialize_entry("readAction", &read_action(a))?;
        }
        m.serialize_entry("width", &f.width)?;
        m.end()
    }
}

impl Serialize for E<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        // JSON object keys: the values' decimal text, sorted as text.
        let mut keyed: Vec<(String, &crate::EnumValue)> = self.0.values.iter().map(|v| (v.value.to_string(), v)).collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        let mut m = s.serialize_map(Some(keyed.len()))?;
        for (k, v) in keyed {
            m.serialize_entry(&k, &EnumEntry(v))?;
        }
        m.end()
    }
}

struct EnumEntry<'a>(&'a crate::EnumValue);

impl Serialize for EnumEntry<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let v = self.0;
        let mut m = s.serialize_map(Some(3))?;
        m.serialize_entry("description", &*v.description)?;
        m.serialize_entry("name", &*v.name)?;
        m.serialize_entry("value", &v.value)?;
        m.end()
    }
}
