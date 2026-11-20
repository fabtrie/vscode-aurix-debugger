//! Peripherals, clusters, registers and fields by their path (`STM0`, `STM0.TIM0`,
//! `GTM.TOM0.CH1.CTRL`, `STM0.TIM0.STM31_0`), with the absolute address and what a register
//! inherits from its clusters and peripheral (size, access, reset value).

use crate::{Access, Cluster, Device, Field, Peripheral, Register};

/// A register where it is, with what it inherits.
#[derive(Clone, Debug)]
pub struct RegisterInfo<'d> {
    /// `PERIPHERAL[.CLUSTER...].REGISTER`, named as the SVD names them.
    pub path: String,
    pub address: u64,
    /// Bits: the register's, else its cluster's or peripheral's.
    pub size: u32,
    pub access: Access,
    pub reset_value: u64,
    pub register: &'d Register,
    /// The path of the register's debug view, a sibling that reads the same content without
    /// the side effect of reading this one (a FIFO pop, a cleared valid flag): `ASCLIN0.RXDATA`
    /// has `ASCLIN0.RXDATAD`, `EVADC.G0RES[0]` has `EVADC.G0RESD[0]`. Infineon's descriptions
    /// declare no `readAction` for these; the debug view is how they show.
    pub debug_view: Option<String>,
}

/// The sibling of `r` that is its debug view (see [`RegisterInfo::debug_view`]).
fn debug_view<'d>(r: &Register, siblings: &'d [Register]) -> Option<&'d Register> {
    let name = &*r.name;
    // `NAME` -> `NAMED`, `NAME[3]` -> `NAMED[3]`, `NAME3` -> `NAMED3`.
    let cut = name.find('[').unwrap_or_else(|| name.trim_end_matches(|c: char| c.is_ascii_digit()).len());
    let twin = format!("{}D{}", &name[..cut], &name[cut..]);
    siblings.iter().find(|s| *s.name == *twin && s.description.to_ascii_lowercase().contains("debug"))
}

impl RegisterInfo<'_> {
    /// The size in bytes (1, 2, 4 or 8), from the size in bits.
    pub fn bytes(&self) -> usize {
        (self.size as usize).div_ceil(8).clamp(1, 8)
    }
}

/// A cluster where it is.
#[derive(Clone, Debug)]
pub struct ClusterInfo<'d> {
    pub path: String,
    pub address: u64,
    pub cluster: &'d Cluster,
    scope: Scope,
}

/// What a path names.
#[derive(Clone, Debug)]
pub enum Node<'d> {
    Peripheral(&'d Peripheral),
    Cluster(ClusterInfo<'d>),
    Register(RegisterInfo<'d>),
    Field(RegisterInfo<'d>, &'d Field),
}

/// What a register inherits at a place.
#[derive(Clone, Debug)]
struct Scope {
    path: String,
    base: u64,
    size: u32,
    access: Access,
    reset_value: u64,
}

impl Scope {
    fn of(p: &Peripheral) -> Scope {
        Scope { path: p.name.to_string(), base: p.base_address, size: p.size, access: p.access, reset_value: p.reset_value }
    }

    fn cluster<'d>(&self, c: &'d Cluster) -> ClusterInfo<'d> {
        let scope = Scope {
            path: format!("{}.{}", self.path, c.name),
            base: self.base.wrapping_add(c.address_offset),
            size: c.size.filter(|&s| s > 0).unwrap_or(self.size),
            access: c.access.unwrap_or(self.access),
            reset_value: c.reset_value.unwrap_or(self.reset_value),
        };
        ClusterInfo { path: scope.path.clone(), address: scope.base, cluster: c, scope }
    }

    fn register<'d>(&self, r: &'d Register, siblings: &'d [Register]) -> RegisterInfo<'d> {
        RegisterInfo {
            debug_view: debug_view(r, siblings).map(|d| format!("{}.{}", self.path, d.name)),
            path: format!("{}.{}", self.path, r.name),
            address: self.base.wrapping_add(r.address_offset),
            size: r.size.filter(|&s| s > 0).unwrap_or(self.size),
            access: r.access.unwrap_or(self.access),
            reset_value: r.reset_value.unwrap_or(self.reset_value),
            register: r,
        }
    }

    fn collect<'d>(&self, registers: &'d [Register], clusters: &'d [Cluster], out: &mut Vec<RegisterInfo<'d>>) {
        out.extend(registers.iter().map(|r| self.register(r, registers)));
        for c in clusters {
            let ci = self.cluster(c);
            ci.scope.collect(&c.registers, &c.clusters, out);
        }
    }
}

fn find_in<'d>(registers: &'d [Register], clusters: &'d [Cluster], scope: &Scope, rest: &[&str]) -> Option<Node<'d>> {
    let (first, tail) = rest.split_first()?;
    if let Some(r) = registers.iter().find(|r| r.name.eq_ignore_ascii_case(first)) {
        let info = scope.register(r, registers);
        return match tail {
            [] => Some(Node::Register(info)),
            [f] => r.fields.iter().find(|x| x.name.eq_ignore_ascii_case(f)).map(|f| Node::Field(info, f)),
            _ => None,
        };
    }
    let c = clusters.iter().find(|c| c.name.eq_ignore_ascii_case(first))?;
    let ci = scope.cluster(c);
    if tail.is_empty() {
        return Some(Node::Cluster(ci));
    }
    find_in(&c.registers, &c.clusters, &ci.scope, tail)
}

impl Peripheral {
    /// Its registers, those of its clusters included.
    pub fn register_count(&self) -> usize {
        fn count(regs: &[Register], clusters: &[Cluster]) -> usize {
            regs.len() + clusters.iter().map(|c| count(&c.registers, &c.clusters)).sum::<usize>()
        }
        count(&self.registers, &self.clusters)
    }
}

impl Device {
    /// A peripheral by name, ignoring case.
    pub fn peripheral(&self, name: &str) -> Option<&Peripheral> {
        self.peripherals.iter().find(|p| p.name.eq_ignore_ascii_case(name))
    }

    /// What a path names, ignoring case: a peripheral, cluster, register or field.
    pub fn find(&self, path: &str) -> Option<Node<'_>> {
        let parts: Vec<&str> = path.trim().split('.').collect();
        let (first, rest) = parts.split_first()?;
        let p = self.peripheral(first)?;
        if rest.is_empty() {
            return Some(Node::Peripheral(p));
        }
        find_in(&p.registers, &p.clusters, &Scope::of(p), rest)
    }

    /// The registers a node stands for, in address order: all of a peripheral or cluster, the
    /// register itself, a field's register.
    pub fn registers<'d>(&'d self, node: &Node<'d>) -> Vec<RegisterInfo<'d>> {
        let mut out = Vec::new();
        match node {
            Node::Peripheral(p) => Scope::of(p).collect(&p.registers, &p.clusters, &mut out),
            Node::Cluster(c) => c.scope.collect(&c.cluster.registers, &c.cluster.clusters, &mut out),
            Node::Register(r) | Node::Field(r, _) => out.push(r.clone()),
        }
        out.sort_by_key(|r| r.address);
        out
    }
}

#[cfg(test)]
mod tests {
    use crate::Access;

    const XML: &str = r#"<device><size>32</size><peripherals>
      <peripheral><name>TIM</name><baseAddress>0xF0001000</baseAddress><size>16</size><access>read-only</access>
        <registers>
          <register><name>B</name><addressOffset>0x8</addressOffset></register>
          <register><name>BD</name><description>B, Debug View</description><addressOffset>0xC</addressOffset></register>
          <register><name>A</name><addressOffset>0x4</addressOffset><size>32</size><access>read-write</access>
            <fields><field><name>EN</name><bitOffset>0</bitOffset><bitWidth>1</bitWidth></field></fields></register>
          <cluster><name>CH[%s]</name><dim>2</dim><dimIncrement>0x10</dimIncrement><addressOffset>0x100</addressOffset><size>8</size>
            <register><name>CTRL</name><addressOffset>0x4</addressOffset></register>
          </cluster>
        </registers></peripheral>
    </peripherals></device>"#;

    #[test]
    fn paths_addresses_and_inheritance() {
        let d = crate::parse(XML, &[]).unwrap();
        let Some(crate::Node::Register(a)) = d.find("tim.a") else { panic!() };
        assert_eq!((a.path.as_str(), a.address, a.size, a.access), ("TIM.A", 0xF000_1004, 32, Access::ReadWrite));
        let Some(crate::Node::Register(b)) = d.find("TIM.B") else { panic!() };
        assert_eq!((b.size, b.access, b.bytes()), (16, Access::ReadOnly, 2), "inherited from the peripheral");
        assert_eq!(b.debug_view.as_deref(), Some("TIM.BD"));
        assert!(a.debug_view.is_none());
        let Some(crate::Node::Register(c)) = d.find("TIM.CH[1].CTRL") else { panic!() };
        assert_eq!((c.address, c.size), (0xF000_1114, 8), "from the cluster");
        assert!(matches!(d.find("TIM.A.EN"), Some(crate::Node::Field(_, f)) if &*f.name == "EN"));
        assert!(matches!(d.find("TIM.CH[0]"), Some(crate::Node::Cluster(c)) if c.address == 0xF000_1100));
        assert!(d.find("TIM.NOPE").is_none());
        assert!(d.find("TIM.A.EN.X").is_none());
        let all = d.registers(&d.find("TIM").unwrap());
        let paths: Vec<&str> = all.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, ["TIM.A", "TIM.B", "TIM.BD", "TIM.CH[0].CTRL", "TIM.CH[1].CTRL"], "in address order");
        assert_eq!(d.peripherals[0].register_count(), 5);
    }
}
