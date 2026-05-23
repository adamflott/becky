//! CPU description model used by resource constraints and host scanning.
//!
//! The types in this module describe CPU identity, architecture, topology,
//! caches, memory controller capabilities, power, security, accelerators,
//! interconnects, and firmware. They are intentionally descriptive data types;
//! provider crates are responsible for collecting or matching them.

#![allow(dead_code)]
// cpu_desc.rs

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;

/// Enable serde by compiling with `--features serde`.
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// High-level description of a (model of) CPU and how it may be configured.
///
/// This is designed to describe *a CPU family/model*, not a live system reading.
/// Use `Topology` fields to represent possible/core-present layouts, including big.LITTLE.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cpu {
    pub identity: Identity,
    pub architecture: Architecture,
    pub manufacturing: Manufacturing,
    pub packaging: Packaging,
    pub topology: Topology,
    pub caches: Vec<Cache>, // package-level caches (e.g., shared L3/L4/eDRAM)
    pub memory: MemoryController,
    pub power: Power,
    pub security: Security,
    pub accelerators: Vec<Accelerator>,
    pub interconnects: Vec<Interconnect>,
    pub firmware: Firmware,
    pub features: FeatureSets,
    /// Extra vendor/arch/platform-specific key-value pairs for long-tail details.
    pub extras: BTreeMap<String, String>,
}

impl Cpu {
    pub fn count(&self) -> NonZeroUsize {
        self.topology.core_count
    }
}
/// Vendor + marketing names + numeric IDs (family/model/stepping where applicable).
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub vendor: Vendor,
    /// Human-friendly product/marketing name (e.g., "Ryzen 9 7950X").
    pub product_name: Option<String>,
    /// Part number / OPN / ordering code.
    pub part_number: Option<String>,
    /// Family/model/stepping (x86) or SOC/part revs on other ISAs.
    pub numeric_ids: NumericIds,
    /// Segment (server/desktop/mobile/embedded).
    pub market_segment: MarketSegment,
    /// Year of initial release (if known).
    pub release_year: Option<u16>,
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Vendor {
    Intel,
    AMD,
    ARM, // ARM Ltd. cores (Cortex/Neoverse)
    Apple,
    Qualcomm,
    IBM,
    SiFive,
    Loongson,
    MIPS,
    SunOracle,
    Samsung,
    HiSilicon,
    NVIDIA,
    VIA,
    Other(String),
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NumericIds {
    // x86-ish
    pub family: Option<u16>,
    pub model: Option<u16>,
    pub stepping: Option<u16>,
    // Alternative identifiers (e.g., "CPUID leaf signature", PVR, MIDR, etc.).
    pub alt: BTreeMap<String, String>,
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarketSegment {
    Server,
    Desktop,
    Mobile,
    Embedded,
    Edge,
    IoT,
    Other(String),
}

/// ISA + ISA-specific feature families (typed where feasible, open-ended otherwise).
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Architecture {
    pub word_size: WordSize,
    pub endianness: Endianness,
    pub isa: Isa,
    pub microarchitecture_name: Option<String>, // e.g., "Zen 4", "Golden Cove", "Cortex-A78"
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WordSize {
    W32,
    W64,
    Other(u16),
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endianness {
    Little,
    Big,
    Bi, // runtime/configurable
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Isa {
    X86_64(X86Features),
    X86_32(X86Features),
    AArch64(AArch64Features),
    Armv7(Arm32Features),
    RiscV(RiscVFeatures),
    PowerPC64(PpcFeatures),
    S390x(S390xFeatures),
    Mips64(MipsFeatures),
    Sparc64(SparcFeatures),
    Other { name: String, features: BTreeSet<String> },
}

/// Representative x86 feature set (not exhaustive) with escape hatches.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct X86Features {
    pub sse: bool,
    pub sse2: bool,
    pub sse3: bool,
    pub ssse3: bool,
    pub sse4_1: bool,
    pub sse4_2: bool,
    pub aesni: bool,
    pub avx: bool,
    pub avx2: bool,
    pub avx_vnni: bool,
    pub avx512f: bool,
    pub avx512dq: bool,
    pub avx512ifma: bool,
    pub avx512pf: bool,
    pub avx512er: bool,
    pub avx512cd: bool,
    pub avx512bw: bool,
    pub avx512vl: bool,
    pub avx512_vnni: bool,
    pub avx512_bf16: bool,
    pub avx10: bool,
    pub fma3: bool,
    pub bmi1: bool,
    pub bmi2: bool,
    pub f16c: bool,
    pub popcnt: bool,
    pub rdrand: bool,
    pub rdseed: bool,
    pub sgx: bool,
    pub tdx: bool,
    pub vt_x: bool,
    pub vt_d: bool,
    pub smx: bool,
    pub smep: bool,
    pub smap: bool,
    pub shstk: bool, // CET shadow stack
    pub ibt: bool,   // CET indirect branch tracking
    pub tsx: bool,
    pub umip: bool,
    pub movbe: bool,
    pub clflush: bool,
    pub clwb: bool,
    pub cldemote: bool,
    pub mpx: bool,
    pub adx: bool,
    pub sha: bool,               // SHA extensions
    pub other: BTreeSet<String>, // unknown/rare flags
}

/// Representative AArch64 features (not exhaustive) + escape hatch.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AArch64Features {
    pub neon: bool,
    pub sve: bool,
    pub sve2: bool,
    pub sme: bool,
    pub sme2: bool,
    pub dotprod: bool,
    pub fp16: bool,
    pub fp16fml: bool,
    pub aes: bool,
    pub sha1: bool,
    pub sha2: bool,
    pub sha3: bool,
    pub crc32: bool,
    pub atomics: bool,      // LSE
    pub pointer_auth: bool, // PAC
    pub mte: bool,
    pub ras: bool,
    pub bt: bool,  // branch target identification
    pub rme: bool, // Realm Management Extension
    pub other: BTreeSet<String>,
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Arm32Features {
    pub neon: bool,
    pub vfpv3: bool,
    pub vfpv4: bool,
    pub trustzone: bool,
    pub other: BTreeSet<String>,
}

/// RISC-V: base and extensions (use strings for the Z* zoo, but keep typed base).
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RiscVFeatures {
    pub base: RiscVBase,                   // RV32I/RV64I/etc.
    pub standard_exts: BTreeSet<RiscVStd>, // M, A, F, D, C, V, B, K, H...
    pub z_exts: BTreeSet<String>,          // e.g., "Zicsr", "Zifencei", "Zba", ...
    pub vendor_exts: BTreeSet<String>,     // e.g., "Xcustom"
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RiscVBase {
    RV32I,
    RV64I,
    RV32E,
    Other(String),
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiscVStd {
    M,
    A,
    F,
    D,
    C,
    V,
    B,
    K,
    H,
    P,
    Other(String),
}

// Power/IBM, S/390, MIPS, SPARC summarized with escape hatch.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PpcFeatures {
    pub altivec: bool,
    pub vsx: bool,
    pub other: BTreeSet<String>,
}
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct S390xFeatures {
    pub vector: bool,
    pub msa: bool,
    pub other: BTreeSet<String>,
}
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MipsFeatures {
    pub mips64r2: bool,
    pub simd: bool,
    pub other: BTreeSet<String>,
}
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SparcFeatures {
    pub vis: bool,
    pub other: BTreeSet<String>,
}

/// Silicon/production details.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manufacturing {
    /// e.g., 7.0 = "7 nm" (marketing or effective); unit is nanometers.
    pub process_nm: Option<usize>,
    /// Number of physical dies in the package (chiplets included).
    pub die_count: Option<NonZeroUsize>,
    /// For chiplet designs (e.g., CCD/CCX/IO die), free-form map like {"CCD": "2", "IOD": "1"}.
    pub chiplet_breakdown: BTreeMap<String, String>,
    /// Wafer/source fab info (e.g., "TSMC N5", "Intel 4").
    pub fab: Option<String>,
}

/// Physical package and socketing.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packaging {
    pub package: PackageType,
    /// Compatible sockets (e.g., "AM5", "LGA1700", "BGA-XYZ").
    pub socket_names: BTreeSet<String>,
    /// TJmax (°C).
    pub tj_max_c: Option<u16>,
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackageType {
    LGA,
    PGA,
    BGA,
    SIP, // system-in-package
    MCM, // multi-chip module
    Other(String),
}

/// Core/Thread layout, with optional per-core descriptors to capture heterogeneity.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Topology {
    /// Total physical cores (present/enableable) across the package.
    pub core_count: NonZeroUsize,
    /// Total logical threads (across SMT).
    pub thread_count: NonZeroUsize,
    /// If true, hardware threads per core > 1 (SMT/HT).
    pub smt: bool,
    /// big.LITTLE / heterogeneous core types.
    pub core_kinds: Vec<CoreKindCount>,
    /// Optional per-core listing for fine detail (NUMA ID, affinity, freq caps, features).
    pub cores: Vec<CoreDescriptor>,
    /// NUMA domains with CPU sets.
    pub numa: Vec<NumaNode>,
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreKindCount {
    pub kind: CoreKind,
    pub cores: NonZeroUsize,
    pub threads_per_core: NonZeroUsize,
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoreKind {
    Performance,
    Efficiency,
    Balanced,
    VectorHeavy,
    LowPower,
    Other(String),
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CoreDescriptor {
    pub id: Option<usize>, // logical/core-id as defined by firmware/OS
    pub kind: Option<CoreKind>,
    pub max_mhz: Option<u32>,               // per-core max single-core turbo, in MHz
    pub base_mhz: Option<u32>,              // per-core base, in MHz
    pub isa_overrides: Option<FeatureSets>, // per-core feature diffs
    pub numa_node: Option<usize>,
    pub extras: BTreeMap<String, String>,
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NumaNode {
    pub id: usize,
    /// CPU IDs (logical) associated with this NUMA node (optional if described elsewhere).
    pub cpu_ids: Vec<usize>,
    /// Local memory (bytes), if known.
    pub local_memory_bytes: Option<u64>,
}

/// Cache descriptions for L1..L4, instruction/data/unified, sharing scope, etc.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cache {
    pub level: CacheLevel,
    pub kind: CacheKind,
    pub size_bytes: u64,
    pub line_size: Option<u32>,
    pub associativity: Option<u32>, // ways
    /// Scope of sharing (per-core, per-CCX/cluster, per-die, per-package, system).
    pub shared_scope: CacheScope,
    pub inclusive: Option<bool>,
    pub extras: BTreeMap<String, String>,
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CacheLevel {
    L1,
    L2,
    L3,
    L4,
    Other(String),
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CacheKind {
    Instruction,
    Data,
    Unified,
    Trace,
    Other(String),
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CacheScope {
    PerThread,
    PerCore,
    PerCluster,
    PerDie,
    PerPackage,
    System,
    Other(String),
}

/// Integrated memory controller capabilities (max, ECC, channels, types).
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryController {
    pub channels: Option<NonZeroUsize>,
    pub max_capacity_bytes: Option<u64>,
    pub ecc_supported: Option<bool>,
    pub memory_types: BTreeSet<MemoryType>,
    /// Nominal max data rates (MT/s) per type (e.g., {"DDR5": "6400"}).
    pub max_data_rates_mtps: BTreeMap<String, u32>,
    pub extras: BTreeMap<String, String>,
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MemoryType {
    DDR3,
    DDR4,
    DDR5,
    LPDDR4X,
    LPDDR5,
    HBM2,
    HBM2e,
    HBM3,
    Other(String),
}

/// TDP, clocks, boost/turbo behavior, configurable TDPs, and management features.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Power {
    /// Base (all-core) frequency in MHz (if known).
    pub base_mhz: Option<u32>,
    /// Single-core max boost in MHz (if known).
    pub max_single_core_boost_mhz: Option<u32>,
    /// Max all-core boost in MHz (if known).
    pub max_all_core_boost_mhz: Option<u32>,
    /// Nominal TDP in watts.
    pub tdp_watts: Option<u16>,
    /// Configurable TDP range in watts (e.g., 35–65).
    pub ctdp_watts: Option<(u16, u16)>,
    /// Power states / DVFS / turbo presence.
    pub turbo_available: bool,
    pub p_states: bool,
    pub c_states: bool,
    pub extras: BTreeMap<String, String>,
}

/// Virtualization, enclaves, mitigations, and known vuln classes (Meltdown/Spectre/etc.).
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Security {
    pub virtualization: Virtualization,
    pub enclaves: Enclaves,
    pub mitigations: BTreeSet<String>,           // e.g., "retpoline", "IBRS", "SMT mitigated"
    pub known_vulnerabilities: BTreeSet<String>, // e.g., "Spectre v1", "Downfall"
    pub extras: BTreeMap<String, String>,
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Virtualization {
    pub hw_vm: bool, // VT-x / AMD-V / HVF / KVM cap
    pub iommu: bool, // VT-d / AMD-Vi / SMMU
    pub nested: bool,
    pub sriov: bool,
    pub extras: BTreeMap<String, String>,
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Enclaves {
    pub intel_sgx: bool,
    pub intel_tdx: bool,
    pub amd_sev: bool,
    pub amd_sev_es: bool,
    pub amd_sev_snp: bool,
    pub arm_rme: bool,
    pub other: BTreeSet<String>,
}

/// iGPU, NPU, media blocks, DSPs, and miscellaneous accelerators.
/// Use `kind`+`details` for open-ended description and numbers for perf/limits where known.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accelerator {
    pub kind: AcceleratorKind,
    pub vendor: Option<String>, // e.g., "AMD", "Intel", "Apple"
    pub model: Option<String>,  // e.g., "RDNA3 iGPU", "Xe-LP"
    pub compute_units: Option<u32>,
    pub max_mhz: Option<u32>,
    pub vram_bytes: Option<u64>, // shared or dedicated; see extras
    pub extras: BTreeMap<String, String>,
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AcceleratorKind {
    IntegratedGpu,
    NpuAi,
    Dsp,
    MediaEncodeDecode,
    Vision,
    Crypto,
    Other(String),
}

/// Links/fabrics on-die and off-package (UPI/QPI/IF/AMBA/etc.).
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interconnect {
    pub kind: InterconnectKind,
    pub version: Option<String>, // e.g., "UPI 2.0", "Infinity Fabric 3"
    pub width_bits: Option<u32>,
    pub rate_mtps: Option<u64>, // mega-transfers/s per lane or aggregate
    pub extras: BTreeMap<String, String>,
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
/// Links/fabrics on-die and off-package.
#[allow(non_camel_case_types)]
pub enum InterconnectKind {
    UPI,
    QPI,
    InfinityFabric,
    HyperTransport,
    AMBA_AXI,
    AMBA_CHI,
    CCIX,
    CXL,
    NVLink,
    Custom(String),
}

/// Microcode/firmware identifiers and updatable capabilities.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Firmware {
    pub microcode_version: Option<String>,
    pub updatable: bool,
    pub extras: BTreeMap<String, String>,
}

/// Grouped feature sets so you can keep ISA-specific bits together and still add arbitrary tags.
///
/// `tags` is great for things like "Intel Thread Director", "Precision Boost", etc.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FeatureSets {
    pub tags: BTreeSet<String>,
    pub x86: Option<X86Features>,
    pub aarch64: Option<AArch64Features>,
    pub arm32: Option<Arm32Features>,
    pub riscv: Option<RiscVFeatures>,
    pub ppc: Option<PpcFeatures>,
    pub s390x: Option<S390xFeatures>,
    pub mips: Option<MipsFeatures>,
    pub sparc: Option<SparcFeatures>,
    pub other: BTreeMap<String, BTreeSet<String>>,
}
