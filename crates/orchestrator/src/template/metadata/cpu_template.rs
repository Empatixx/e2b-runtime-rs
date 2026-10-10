use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::null_as_default;
use crate::firecracker::version::parse_firecracker_version;

const MIN_TSC_KHZ: i64 = 100_000;
const MAX_TSC_KHZ: i64 = 10_000_000;
const DIGEST_BYTES: usize = 6;
const CPUID_REGISTERS: [&str; 4] = ["eax", "ebx", "ecx", "edx"];
const TEMPLATE_FIELDS: [&str; 6] = [
    "cpuid_modifiers",
    "kvm_capabilities",
    "msr_modifiers",
    "reg_modifiers",
    "vcpu_features",
    "x86_tsc_khz",
];
const CPUID_LEAF_FIELDS: [&str; 4] = ["flags", "leaf", "modifiers", "subleaf"];
const CPUID_REGISTER_FIELDS: [&str; 2] = ["bitmap", "register"];
const REGISTER_FIELDS: [&str; 2] = ["addr", "bitmap"];
const VCPU_FEATURE_FIELDS: [&str; 2] = ["bitmap", "index"];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuTemplate {
    #[serde(default, deserialize_with = "null_as_default", skip_serializing_if = "Vec::is_empty")]
    pub cpuid_modifiers: Vec<Option<CpuidLeafModifier>>,
    #[serde(default, deserialize_with = "null_as_default", skip_serializing_if = "Vec::is_empty")]
    pub kvm_capabilities: Vec<String>,
    #[serde(default, deserialize_with = "null_as_default", skip_serializing_if = "Vec::is_empty")]
    pub msr_modifiers: Vec<Option<RegisterModifier>>,
    #[serde(default, deserialize_with = "null_as_default", skip_serializing_if = "Vec::is_empty")]
    pub reg_modifiers: Vec<Option<RegisterModifier>>,
    #[serde(default, deserialize_with = "null_as_default", skip_serializing_if = "Vec::is_empty")]
    pub vcpu_features: Vec<Option<VcpuFeature>>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub x86_tsc_khz: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuidLeafModifier {
    pub flags: Option<i32>,
    pub leaf: Option<String>,
    pub modifiers: Option<Vec<Option<CpuidRegisterModifier>>>,
    pub subleaf: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuidRegisterModifier {
    pub bitmap: Option<String>,
    pub register: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterModifier {
    pub addr: Option<String>,
    pub bitmap: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcpuFeature {
    pub bitmap: Option<String>,
    pub index: Option<i32>,
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

pub fn applied_digest(template: Option<&CpuTemplate>) -> String {
    template
        .filter(|template| !template.is_empty())
        .map_or_else(String::new, CpuTemplate::digest)
}

impl CpuTemplate {
    pub fn parse(raw: &str) -> Result<Option<Self>, String> {
        let raw = raw.trim();
        if raw.is_empty() || raw == "null" {
            return Ok(None);
        }
        let document: Value = serde_json::from_str(raw).map_err(|e| format!("invalid CPU template: {e}"))?;
        reject_unknown_fields(&document)?;
        let template: Self =
            serde_json::from_value(document.clone()).map_err(|e| format!("invalid CPU template: {e}"))?;
        template.validate_fields()?;
        template.reject_null_entries()?;
        if document.get("x86_tsc_khz").and_then(Value::as_i64) == Some(0) {
            return Err(
                "invalid CPU template: x86_tsc_khz 0 is below the minimum of 100000 kHz; omit the field to keep the host TSC frequency"
                    .to_owned(),
            );
        }
        Ok((!template.is_empty()).then_some(template))
    }

    pub fn validate(&self, firecracker_version: &str) -> Result<(), String> {
        let version = parse_firecracker_version(firecracker_version)?;
        if self.x86_tsc_khz != 0 && !version.has_tsc_khz_template() {
            return Err("CPU template sets x86_tsc_khz, which this Firecracker version does not support".to_owned());
        }
        self.validate_for(go_arch(std::env::consts::ARCH))
    }

    pub fn validate_for(&self, arch: &str) -> Result<(), String> {
        self.validate_fields()?;
        let x86_only = [
            (self.x86_tsc_khz != 0, "x86_tsc_khz"),
            (!self.cpuid_modifiers.is_empty(), "cpuid_modifiers"),
            (!self.msr_modifiers.is_empty(), "msr_modifiers"),
        ];
        let arm_only = [
            (!self.reg_modifiers.is_empty(), "reg_modifiers"),
            (!self.vcpu_features.is_empty(), "vcpu_features"),
        ];
        let wrong_on_x86 = (arch != "amd64").then(|| first_set(&x86_only)).flatten().map(|field| {
            format!("CPU template sets {field}, which Firecracker supports only on x86_64 (host is {arch})")
        });
        let wrong_on_arm = (arch != "arm64").then(|| first_set(&arm_only)).flatten().map(|field| {
            format!("CPU template sets {field}, which Firecracker supports only on aarch64 (host is {arch})")
        });
        wrong_on_x86.or(wrong_on_arm).map_or(Ok(()), Err)
    }

    pub fn is_empty(&self) -> bool {
        self.x86_tsc_khz == 0
            && self.cpuid_modifiers.is_empty()
            && self.kvm_capabilities.is_empty()
            && self.msr_modifiers.is_empty()
            && self.reg_modifiers.is_empty()
            && self.vcpu_features.is_empty()
    }

    pub fn canonical(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn digest(&self) -> String {
        hex::encode(&Sha256::digest(self.canonical().as_bytes())[..DIGEST_BYTES])
    }

    fn validate_fields(&self) -> Result<(), String> {
        let invalid = |e: String| format!("invalid CPU template: {e}");
        self.cpuid_modifiers
            .iter()
            .flatten()
            .try_for_each(CpuidLeafModifier::validate)
            .map_err(invalid)?;
        self.msr_modifiers
            .iter()
            .flatten()
            .chain(self.reg_modifiers.iter().flatten())
            .try_for_each(RegisterModifier::validate)
            .map_err(invalid)?;
        self.vcpu_features
            .iter()
            .flatten()
            .try_for_each(VcpuFeature::validate)
            .map_err(invalid)?;
        match self.x86_tsc_khz {
            0 => Ok(()),
            khz if khz < MIN_TSC_KHZ => Err(invalid(format!(
                "x86_tsc_khz in body should be greater than or equal to {MIN_TSC_KHZ}"
            ))),
            khz if khz > MAX_TSC_KHZ => Err(invalid(format!(
                "x86_tsc_khz in body should be less than or equal to {MAX_TSC_KHZ}"
            ))),
            _ => Ok(()),
        }
    }

    fn reject_null_entries(&self) -> Result<(), String> {
        let has_null = self.msr_modifiers.contains(&None)
            || self.reg_modifiers.contains(&None)
            || self.vcpu_features.contains(&None)
            || self.cpuid_modifiers.iter().any(|leaf| {
                leaf.as_ref().is_none_or(|leaf| {
                    leaf.modifiers
                        .as_ref()
                        .is_some_and(|modifiers| modifiers.contains(&None))
                })
            });
        match has_null {
            true => Err("invalid CPU template: a modifier collection contains a null entry".to_owned()),
            false => Ok(()),
        }
    }
}

impl CpuidLeafModifier {
    fn validate(&self) -> Result<(), String> {
        self.flags.ok_or_else(|| required("flags"))?;
        self.leaf.as_ref().ok_or_else(|| required("leaf"))?;
        self.subleaf.as_ref().ok_or_else(|| required("subleaf"))?;
        self.modifiers
            .as_ref()
            .ok_or_else(|| required("modifiers"))?
            .iter()
            .flatten()
            .try_for_each(CpuidRegisterModifier::validate)
    }
}

impl CpuidRegisterModifier {
    fn validate(&self) -> Result<(), String> {
        self.bitmap.as_ref().ok_or_else(|| required("bitmap"))?;
        let register = self.register.as_ref().ok_or_else(|| required("register"))?;
        match CPUID_REGISTERS.contains(&register.as_str()) {
            true => Ok(()),
            false => Err(format!(
                "register in body should be one of [eax ebx ecx edx]: {register}"
            )),
        }
    }
}

impl RegisterModifier {
    fn validate(&self) -> Result<(), String> {
        self.addr.as_ref().ok_or_else(|| required("addr"))?;
        self.bitmap.as_ref().ok_or_else(|| required("bitmap")).map(|_| ())
    }
}

impl VcpuFeature {
    fn validate(&self) -> Result<(), String> {
        self.bitmap.as_ref().ok_or_else(|| required("bitmap"))?;
        self.index.ok_or_else(|| required("index")).map(|_| ())
    }
}

fn first_set<'a>(fields: &[(bool, &'a str)]) -> Option<&'a str> {
    fields.iter().find(|(set, _)| *set).map(|(_, field)| *field)
}

fn required(field: &str) -> String {
    format!("{field} in body is required")
}

fn go_arch(arch: &str) -> &str {
    match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

fn reject_unknown_fields(document: &Value) -> Result<(), String> {
    let object = document
        .as_object()
        .ok_or("invalid CPU template: json: cannot unmarshal array into Go value of type cputemplate.Template")?;
    check_fields(object, &TEMPLATE_FIELDS)?;
    entries(document, "cpuid_modifiers").try_for_each(|leaf| {
        check_object(leaf, &CPUID_LEAF_FIELDS)?;
        entries(leaf, "modifiers").try_for_each(|modifier| check_object(modifier, &CPUID_REGISTER_FIELDS))
    })?;
    ["msr_modifiers", "reg_modifiers"]
        .into_iter()
        .flat_map(|key| entries(document, key))
        .try_for_each(|modifier| check_object(modifier, &REGISTER_FIELDS))?;
    entries(document, "vcpu_features").try_for_each(|feature| check_object(feature, &VCPU_FEATURE_FIELDS))
}

fn entries<'a>(value: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    value.get(key).and_then(Value::as_array).into_iter().flatten()
}

fn check_object(value: &Value, allowed: &[&str]) -> Result<(), String> {
    value.as_object().map_or(Ok(()), |object| check_fields(object, allowed))
}

fn check_fields(object: &serde_json::Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    object
        .keys()
        .find(|key| !allowed.contains(&key.as_str()))
        .map_or(Ok(()), |key| {
            Err(format!("invalid CPU template: json: unknown field \"{key}\""))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_template_is_written_in_go_field_order() {
        let template = CpuTemplate {
            cpuid_modifiers: vec![Some(CpuidLeafModifier {
                flags: Some(0),
                leaf: Some("0x1".to_owned()),
                modifiers: Some(vec![Some(CpuidRegisterModifier {
                    bitmap: Some("0b0".to_owned()),
                    register: Some("ecx".to_owned()),
                })]),
                subleaf: Some("0x0".to_owned()),
            })],
            kvm_capabilities: vec!["!121".to_owned()],
            msr_modifiers: vec![Some(RegisterModifier {
                addr: Some("0x10a".to_owned()),
                bitmap: Some("0b0".to_owned()),
            })],
            x86_tsc_khz: 3_200_000,
            ..CpuTemplate::default()
        };

        assert_eq!(
            serde_json::to_string(&template).unwrap(),
            r#"{"cpuid_modifiers":[{"flags":0,"leaf":"0x1","modifiers":[{"bitmap":"0b0","register":"ecx"}],"subleaf":"0x0"}],"kvm_capabilities":["!121"],"msr_modifiers":[{"addr":"0x10a","bitmap":"0b0"}],"x86_tsc_khz":3200000}"#
        );
    }

    #[test]
    fn missing_pointers_are_written_as_null_like_go() {
        let template: CpuTemplate = serde_json::from_str(r#"{"msr_modifiers":[{"addr":"0x1"},null]}"#).unwrap();

        assert_eq!(
            serde_json::to_string(&template).unwrap(),
            r#"{"msr_modifiers":[{"addr":"0x1","bitmap":null},null]}"#
        );
    }

    const CPUID_TEMPLATE: &str = r#"{
        "cpuid_modifiers": [
            {
                "leaf": "0x1",
                "subleaf": "0x0",
                "flags": 0,
                "modifiers": [{"register": "ecx", "bitmap": "0bxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx0"}]
            }
        ],
        "msr_modifiers": [{"addr": "0x10a", "bitmap": "0b0000000000000000000000000000000000000000000000000000000000000000"}],
        "x86_tsc_khz": 3200000
    }"#;

    fn tsc(khz: i64) -> CpuTemplate {
        CpuTemplate {
            x86_tsc_khz: khz,
            ..CpuTemplate::default()
        }
    }

    fn kvm() -> CpuTemplate {
        CpuTemplate {
            kvm_capabilities: vec!["!121".to_owned()],
            ..CpuTemplate::default()
        }
    }

    fn msr() -> CpuTemplate {
        CpuTemplate {
            msr_modifiers: vec![Some(RegisterModifier {
                addr: Some("0x10a".to_owned()),
                bitmap: Some("0b0".to_owned()),
            })],
            ..CpuTemplate::default()
        }
    }

    #[test]
    fn an_empty_or_null_document_is_no_template_not_an_error() {
        for raw in ["", "   ", "null", " null\n", "{}", r#"{"kvm_capabilities": []}"#] {
            assert_eq!(CpuTemplate::parse(raw), Ok(None), "document {raw:?}");
        }
    }

    #[test]
    fn a_tsc_only_template_sets_only_the_tsc_frequency_and_the_bounds_are_inclusive() {
        assert_eq!(
            CpuTemplate::parse(r#"{"x86_tsc_khz": 3200000}"#),
            Ok(Some(tsc(3_200_000)))
        );
        for khz in [100_000, 10_000_000] {
            assert_eq!(
                CpuTemplate::parse(&format!(r#"{{"x86_tsc_khz": {khz}}}"#)),
                Ok(Some(tsc(khz)))
            );
        }
    }

    #[test]
    fn a_full_template_keeps_every_collection_and_reads_back_from_its_string() {
        let parsed = CpuTemplate::parse(CPUID_TEMPLATE).unwrap().unwrap();

        assert_eq!(parsed.cpuid_modifiers.len(), 1);
        let leaf = parsed.cpuid_modifiers[0].as_ref().unwrap();
        assert_eq!(leaf.leaf.as_deref(), Some("0x1"));
        let register = leaf.modifiers.as_ref().unwrap()[0].as_ref().unwrap();
        assert_eq!(register.register.as_deref(), Some("ecx"));
        assert_eq!(parsed.msr_modifiers.len(), 1);
        assert_eq!(parsed.x86_tsc_khz, 3_200_000);
        assert_eq!(CpuTemplate::parse(&parsed.canonical()), Ok(Some(parsed)));
    }

    #[test]
    fn documents_firecracker_would_reject_are_refused() {
        for (name, raw) in [
            ("an unknown field", r#"{"tsc_khz": 3200000}"#),
            (
                "an unknown field in a modifier",
                r#"{"msr_modifiers": [{"addr": "0x10a", "bitmap": "0b0", "mask": "0b1"}]}"#,
            ),
            ("a fractional TSC frequency", r#"{"x86_tsc_khz": 3200000.5}"#),
            ("a zero TSC frequency", r#"{"x86_tsc_khz": 0}"#),
            ("a negative TSC frequency", r#"{"x86_tsc_khz": -1}"#),
            ("a TSC frequency given in MHz", r#"{"x86_tsc_khz": 3200}"#),
            ("a TSC frequency given in Hz", r#"{"x86_tsc_khz": 3200000000}"#),
            ("a TSC frequency just below 100 MHz", r#"{"x86_tsc_khz": 99999}"#),
            ("a TSC frequency just above 10 GHz", r#"{"x86_tsc_khz": 10000001}"#),
            ("a TSC frequency beyond 32 bits", r#"{"x86_tsc_khz": 4294967296}"#),
            (
                "a modifier missing a required field",
                r#"{"msr_modifiers": [{"addr": "0x10a"}]}"#,
            ),
            (
                "an unknown cpuid register",
                r#"{"cpuid_modifiers": [{"leaf": "0x1", "subleaf": "0x0", "flags": 0, "modifiers": [{"register": "esi", "bitmap": "0b0"}]}]}"#,
            ),
            ("a null MSR modifier", r#"{"msr_modifiers": [null]}"#),
            ("a null CPUID modifier", r#"{"cpuid_modifiers": [null]}"#),
            (
                "a null CPUID register modifier",
                r#"{"cpuid_modifiers": [{"leaf": "0x1", "subleaf": "0x0", "flags": 0, "modifiers": [null]}]}"#,
            ),
            ("a null register modifier", r#"{"reg_modifiers": [null]}"#),
            ("a null vCPU feature", r#"{"vcpu_features": [null]}"#),
            (
                "a null entry after a valid one",
                r#"{"msr_modifiers": [{"addr": "0x10a", "bitmap": "0b0"}, null]}"#,
            ),
            ("a non-object document", r#"["x86_tsc_khz"]"#),
            ("malformed JSON", r#"{"x86_tsc_khz": }"#),
            ("trailing data", r#"{"x86_tsc_khz": 3200000} {}"#),
        ] {
            assert!(CpuTemplate::parse(raw).is_err(), "{name}");
        }
    }

    #[test]
    fn each_architectures_firecracker_rejects_the_others_fields() {
        let cpuid = CpuTemplate {
            cpuid_modifiers: vec![Some(CpuidLeafModifier {
                leaf: Some("0x1".to_owned()),
                subleaf: Some("0x0".to_owned()),
                flags: Some(0),
                modifiers: Some(Vec::new()),
            })],
            ..CpuTemplate::default()
        };
        let reg = CpuTemplate {
            reg_modifiers: vec![Some(RegisterModifier {
                addr: Some("0x1".to_owned()),
                bitmap: Some("0b0".to_owned()),
            })],
            ..CpuTemplate::default()
        };
        let features = CpuTemplate {
            vcpu_features: vec![Some(VcpuFeature {
                index: Some(0),
                bitmap: Some("0b0".to_owned()),
            })],
            ..CpuTemplate::default()
        };
        for (field, template) in [
            ("x86_tsc_khz", tsc(3_200_000)),
            ("cpuid_modifiers", cpuid),
            ("msr_modifiers", msr()),
        ] {
            assert_eq!(template.validate_for("amd64"), Ok(()), "{field}");
            let error = template.validate_for("arm64").unwrap_err();
            assert!(error.contains(field) && error.contains("arm64"), "{error}");
        }
        for (field, template) in [("reg_modifiers", reg), ("vcpu_features", features)] {
            assert_eq!(template.validate_for("arm64"), Ok(()), "{field}");
            let error = template.validate_for("amd64").unwrap_err();
            assert!(error.contains(field) && error.contains("amd64"), "{error}");
        }
        for arch in ["amd64", "arm64"] {
            assert_eq!(kvm().validate_for(arch), Ok(()));
            assert_eq!(CpuTemplate::default().validate_for(arch), Ok(()));
        }
    }

    #[test]
    fn a_frequency_out_of_range_from_stored_metadata_is_rejected() {
        for khz in [-1, 99_999, 10_000_001] {
            assert!(tsc(khz).validate_for("amd64").is_err(), "{khz}");
        }
    }

    #[test]
    fn validate_uses_the_host_architecture() {
        let template = msr();

        assert_eq!(
            template.validate_for(go_arch(std::env::consts::ARCH)).is_ok(),
            template.validate("v1.14-0.2.0").is_ok()
        );
    }

    #[test]
    fn only_a_template_that_changes_nothing_is_empty() {
        assert!(CpuTemplate::default().is_empty());
        assert!(!tsc(3_200_000).is_empty());
        assert!(!kvm().is_empty());
        assert!(
            !CpuTemplate {
                vcpu_features: vec![Some(VcpuFeature::default())],
                ..CpuTemplate::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn the_string_is_canonical_and_the_digest_short_and_stable() {
        let a = CpuTemplate::parse(r#"{"x86_tsc_khz": 3200000, "kvm_capabilities": ["!121"]}"#)
            .unwrap()
            .unwrap();
        let b = CpuTemplate::parse("{\n  \"kvm_capabilities\": [\"!121\"],\n  \"x86_tsc_khz\": 3200000\n}")
            .unwrap()
            .unwrap();

        assert_eq!(a.canonical(), b.canonical());
        assert_eq!(a.digest(), b.digest());
        assert_eq!(tsc(3_200_000).canonical(), r#"{"x86_tsc_khz":3200000}"#);
        assert_ne!(tsc(3_200_000).canonical(), tsc(2_400_000).canonical());
        assert_ne!(tsc(3_200_000).digest(), tsc(2_400_000).digest());
        assert_eq!(tsc(3_200_000).digest().len(), 12);
    }

    #[test]
    fn the_tsc_frequency_needs_a_release_that_serves_it() {
        let error = tsc(3_200_000).validate("v1.14-0.2.0").unwrap_err();
        assert!(error.contains("x86_tsc_khz"), "{error}");
        assert_eq!(kvm().validate("v1.14-0.2.0"), Ok(()));
        assert_eq!(CpuTemplate::default().validate("v1.14-0.2.0"), Ok(()));
        if go_arch(std::env::consts::ARCH) == "amd64" {
            assert_eq!(tsc(3_200_000).validate("v1.14-0.3.0"), Ok(()));
        }
    }

    #[test]
    fn a_template_is_checked_against_the_version_it_boots_with() {
        assert!(
            tsc(3_200_000)
                .validate("v1.14-0.2.0")
                .unwrap_err()
                .contains("x86_tsc_khz")
        );
        assert!(kvm().validate("not-a-version").is_err());
        assert_eq!(kvm().validate("v1.14-0.3.0"), Ok(()));
    }

    #[test]
    fn the_applied_digest_is_empty_for_no_template_and_an_empty_one() {
        assert_eq!(applied_digest(None), "");
        assert_eq!(applied_digest(Some(&CpuTemplate::default())), "");
        let parsed = CpuTemplate::parse(CPUID_TEMPLATE).unwrap().unwrap();
        assert_eq!(applied_digest(Some(&parsed)), parsed.digest());
    }
}
