use serde::{Deserialize, Serialize};

use super::null_as_default;

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
}
