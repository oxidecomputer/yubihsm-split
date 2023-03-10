// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use thiserror::Error;
use yubihsm::{
    asymmetric,
    object::{Id, Label},
    Capability, Domain,
};

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("failed conversion from YubiHSM Label")]
    BadLabel,

    #[error("failed conversion from YubiHSM Capability")]
    BadCapability,

    #[error("failed to parse key spec from JSON")]
    BadKeySpec { e: serde_json::Error },
}

// These structs duplicate data from the yubihsm crate
// The Algorithm and Domain types serialize but won't deserialize
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum OksAlgorithm {
    Rsa4096,
    Ecp384,
}

impl From<OksAlgorithm> for asymmetric::Algorithm {
    fn from(val: OksAlgorithm) -> Self {
        match val {
            OksAlgorithm::Rsa4096 => asymmetric::Algorithm::Rsa4096,
            OksAlgorithm::Ecp384 => asymmetric::Algorithm::EcP384,
        }
    }
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub enum OksDomain {
    DOM1,
}

impl From<OksDomain> for Domain {
    fn from(val: OksDomain) -> Self {
        match val {
            OksDomain::DOM1 => Domain::DOM1,
        }
    }
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub struct OksLabel(String);

impl TryInto<Label> for OksLabel {
    type Error = ConfigError;

    fn try_into(self) -> Result<Label, Self::Error> {
        match Label::from_bytes(self.0.as_bytes()) {
            Ok(label) => Ok(label),
            Err(_e) => Err(ConfigError::BadLabel),
        }
    }
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub enum OksCapability {
    All,
}

impl From<OksCapability> for Capability {
    fn from(val: OksCapability) -> Self {
        match val {
            OksCapability::All => Capability::all(),
        }
    }
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub enum Hash {
    Sha256,
    Sha384,
}

/// Values in this enum are mapped to OpenSSL config sections for v3 extensions.
/// All certs issued by the OKS are assumed to be intermediate CAs.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub enum Purpose {
    ProductionCodeSigningCA,
    DevelopmentCodeSigningCA,
    ProductionCodeSigning,
    DevelopmentCodeSigning,
    Identity,
}

/// NOTE: These strings correspond to config sections for v3 extensions in the
/// openssl.cnf.
impl ToString for Purpose {
    fn to_string(&self) -> String {
        let str = match self {
            Purpose::ProductionCodeSigningCA => "v3_code_signing_prod_ca",
            Purpose::DevelopmentCodeSigningCA => "v3_code_signing_dev_ca",
            Purpose::ProductionCodeSigning => "v3_code_signing_prod",
            Purpose::DevelopmentCodeSigning => "v3_code_signing_dev",
            Purpose::Identity => "v3_identity",
        };
        String::from(str)
    }
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
struct OksKeySpec {
    pub common_name: String,
    pub id: Id,
    pub algorithm: OksAlgorithm,
    pub capabilities: OksCapability,
    pub domain: OksDomain,
    pub hash: Hash,
    pub label: OksLabel,
    pub purpose: Purpose,
}

#[derive(Debug)]
pub struct KeySpec {
    pub common_name: String,
    pub id: Id,
    pub algorithm: asymmetric::Algorithm,
    pub capabilities: Capability,
    pub domain: Domain,
    pub hash: Hash,
    pub label: Label,
    pub purpose: Purpose,
}

impl FromStr for KeySpec {
    type Err = ConfigError;

    fn from_str(data: &str) -> Result<Self, Self::Err> {
        let spec: OksKeySpec = serde_json::from_str(data)
            .map_err(|e| ConfigError::BadKeySpec { e })?;
        spec.try_into()
    }
}

impl TryFrom<OksKeySpec> for KeySpec {
    type Error = ConfigError;

    fn try_from(spec: OksKeySpec) -> Result<Self, Self::Error> {
        Ok(KeySpec {
            common_name: spec.common_name,
            id: spec.id,
            algorithm: spec.algorithm.into(),
            capabilities: spec.capabilities.into(),
            domain: spec.domain.into(),
            hash: spec.hash,
            label: spec.label.try_into()?,
            purpose: spec.purpose,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON_RSA4K: &str = r#"{
            "common_name":
                "Gimlet RoT Stage0 Code Signing Engineering Offline CA A",
            "id":1,
            "algorithm":"Rsa4096",
            "capabilities":"All",
            "domain":"DOM1",
            "hash":"Sha256",
            "label":"rot-stage0-signing-root-eng-a",
            "purpose":"ProductionCodeSigning"
        }"#;

    #[test]
    fn test_rsa4k_deserialize() -> Result<()> {
        let key_spec: OksKeySpec = serde_json::from_str(&JSON_RSA4K)?;
        assert_eq!(
            key_spec.common_name,
            "Gimlet RoT Stage0 Code Signing Engineering Offline CA A",
        );
        assert_eq!(key_spec.id, 1);
        assert_eq!(key_spec.capabilities, OksCapability::All);
        assert_eq!(key_spec.domain, OksDomain::DOM1);
        assert_eq!(
            key_spec.label,
            OksLabel("rot-stage0-signing-root-eng-a".to_string())
        );
        assert_eq!(key_spec.algorithm, OksAlgorithm::Rsa4096);
        Ok(())
    }

    #[test]
    fn test_oks_spec_convert() -> Result<()> {
        let key_spec = KeySpec::from_str(JSON_RSA4K)?;

        assert_eq!(key_spec.id, 1);
        assert_eq!(key_spec.capabilities, Capability::all());
        assert_eq!(key_spec.domain, Domain::DOM1);
        assert_eq!(
            key_spec.label,
            Label::from_bytes("rot-stage0-signing-root-eng-a".as_bytes())?,
        );
        assert_eq!(key_spec.algorithm, asymmetric::Algorithm::Rsa4096);
        Ok(())
    }

    const JSON_ECP384: &str = r#"{
        "common_name": "RoT Identity Signing Offline CA",
        "id": 2,
        "algorithm":"Ecp384",
        "capabilities":"All",
        "domain":"DOM1",
        "hash":"Sha384",
        "label":"rot-identity-signing-ca",
        "purpose":"DevelopmentCodeSigning"
    }"#;

    #[test]
    fn test_ecp384_deserialize() -> Result<()> {
        let key_spec: OksKeySpec = serde_json::from_str(&JSON_ECP384)?;
        assert_eq!(key_spec.common_name, "RoT Identity Signing Offline CA",);
        assert_eq!(key_spec.id, 2);
        assert_eq!(key_spec.capabilities, OksCapability::All);
        assert_eq!(key_spec.domain, OksDomain::DOM1);
        assert_eq!(
            key_spec.label,
            OksLabel("rot-identity-signing-ca".to_string())
        );
        assert_eq!(key_spec.algorithm, OksAlgorithm::Ecp384);
        assert_eq!(key_spec.purpose, Purpose::DevelopmentCodeSigning);
        Ok(())
    }

    const JSON_IDENTITY: &str = r#"{
        "common_name": "RoT Identity Signing Offline CA",
        "id": 2,
        "algorithm":"Ecp384",
        "capabilities":"All",
        "domain":"DOM1",
        "hash":"Sha384",
        "label":"rot-identity-signing-ca",
        "purpose":"Identity"
    }"#;

    #[test]
    fn test_extensions_engineering() -> Result<()> {
        let key_spec: OksKeySpec = serde_json::from_str(&JSON_IDENTITY)?;
        assert_eq!(key_spec.purpose, Purpose::Identity);
        Ok(())
    }
}
