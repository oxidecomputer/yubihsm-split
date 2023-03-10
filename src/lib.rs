// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::{Context, Result};
use hex::ToHex;
use log::{debug, error, info, warn};
use static_assertions as sa;
use std::{fs, io, path::Path, str::FromStr};
use thiserror::Error;
use yubihsm::{
    authentication::{self, Key, DEFAULT_AUTHENTICATION_KEY_ID},
    object::{Id, Label, Type},
    wrap, Capability, Client, Domain,
};
use zeroize::Zeroize;

pub mod config;

use config::KeySpec;

const ALG: wrap::Algorithm = wrap::Algorithm::Aes256Ccm;
const CAPS: Capability = Capability::all();
const DELEGATED_CAPS: Capability = Capability::all();
const DOMAIN: Domain = Domain::all();
const ID: Id = 0x1;
const KEY_LEN: usize = 32;
const LABEL: &str = "backup";

const SHARES: u8 = 5;
const THRESHOLD: u8 = 3;
sa::const_assert!(THRESHOLD <= SHARES);

#[derive(Error, Debug)]
pub enum HsmError {
    #[error("failed conversion from YubiHSM Domain")]
    BadDomain,
    #[error("failed conversion from YubiHSM Label")]
    BadLabel,
    #[error("failed to create self signed cert for key")]
    SelfCertGenFail,
    #[error("your yubihms is broke")]
    Version,
}

const PASSWD_PROMPT: &str = "Enter new HSM password: ";
const PASSWD_PROMPT2: &str = "Enter password again to confirm: ";

/// Generate an asymmetric key from the provided specification.
pub fn generate(
    client: &Client,
    key_spec: &Path,
    wrap_id: Id,
    out_dir: &Path,
) -> Result<()> {
    let json = fs::read_to_string(key_spec)?;
    debug!("spec as json: {}", json);

    let spec = config::KeySpec::from_str(&json)?;
    debug!("KeySpec from {}: {:#?}", key_spec.display(), spec);

    let id = client.generate_asymmetric_key(
        spec.id,
        spec.label.clone(),
        spec.domain,
        spec.capabilities,
        spec.algorithm,
    )?;
    debug!("new {:#?} key w/ id: {}", spec.algorithm, id);

    debug!(
        "exporting new asymmetric key under wrap-key w/ id: {}",
        wrap_id
    );
    let msg = client.export_wrapped(wrap_id, Type::AsymmetricKey, id)?;
    let msg_json = serde_json::to_string(&msg)?;

    debug!("exported asymmetric key: {:#?}", msg_json);

    let mut out_pathbuf = out_dir.to_path_buf();
    out_pathbuf.push(format!("{}.json", spec.label));

    debug!("writing to: {}", out_pathbuf.display());
    fs::write(out_pathbuf, msg_json)?;

    debug!(
        "bootstrapping CA files for key with label: {}",
        spec.label.to_string()
    );

    Ok(())
}

// NOTE: before using the pkcs11 engine the connector must be running:
// sudo systemctl start yubihsm-connector
macro_rules! openssl_cnf_fmt {
    () => {
        r#"
openssl_conf                = default_modules

[default_modules]
engines                     = engine_section

[engine_section]
pkcs11                      = pkcs11_section

[pkcs11_section]
engine_id                   = pkcs11
MODULE_PATH                 = /usr/lib/pkcs11/yubihsm_pkcs11.so
INIT_ARGS                   = connector=http://127.0.0.1:12345 debug
init                        = 0
# PIN format: "<auth key id><auth key password>"
# password must be 12 characters, 4 for the key id, 8 for the password
#PIN                         = "0001password"

[ ca ]
default_ca                  = CA_default

[ CA_default ]
dir                         = ./
certs                       = $dir/certs
crl_dir                     = $dir/crl
database                    = $dir/index.txt
new_certs_dir               = $dir/newcerts
certificate                 = $dir/certs/ca.cert.pem
serial                      = $dir/serial
# key format:   <slot>:<key id>
private_key                 = 0:{key:#04}
x509_extensions             = v3_ca
name_opt                    = ca_default
cert_opt                    = ca_default
# certs may be retired, but they won't expire
default_enddate             = 99991231235959Z
default_crl_days            = 30
default_md                  = {hash:?}
preserve                    = no
policy                      = policy_match
email_in_dn                 = no
rand_serial                 = no
unique_subject              = yes

[ policy_match ]
countryName                 = optional
stateOrProvinceName         = optional
organizationName            = optional
organizationalUnitName      = optional
commonName                  = supplied
emailAddress                = optional

[ req ]
default_md                  = {hash:?}
x509_extensions             = v3_ca
string_mask                 = utf8only
default_enddate             = 99991231235959Z

[ v3_ca ]
subjectKeyIdentifier        = hash
authorityKeyIdentifier      = keyid:always,issuer
basicConstraints            = critical,CA:true
"#
    };
}

pub fn ca_init(key_spec: &Path, out: &Path) -> Result<()> {
    let json = fs::read_to_string(key_spec)?;
    debug!("spec as json: {}", json);

    let spec = config::KeySpec::from_str(&json)?;
    debug!("KeySpec from {}: {:#?}", key_spec.display(), spec);

    let pwd = std::env::current_dir()?;
    debug!("got current directory: {:?}", pwd);

    // setup CA directory structure
    bootstrap_ca(&spec, out)?;

    let ca_dir = format!("{}/{}", out.display(), spec.label);
    std::env::set_current_dir(&ca_dir)?;
    debug!("setting current directory: {}", ca_dir);

    use std::process::Command;

    debug!("starting connector");
    let mut connector = Command::new("yubihsm-connector").spawn()?;

    debug!("connector started");
    std::thread::sleep(std::time::Duration::from_millis(2000));

    let mut cmd = Command::new("openssl");
    let output = cmd
        .arg("req")
        .arg("-config")
        .arg("openssl.cnf")
        .arg("-new")
        .arg("-subj")
        .arg(format!("/CN={}/", spec.common_name))
        .arg("-engine")
        .arg("pkcs11")
        .arg("-keyform")
        .arg("engine")
        .arg("-key")
        .arg(format!("0:{:#04}", spec.id))
        .arg("-out")
        .arg("csr.pem")
        .output()?;

    info!("executing command: \"{:#?}\"", cmd);

    if !output.status.success() {
        warn!("command failed with status: {}", output.status);
        warn!("stderr: \"{}\"", String::from_utf8_lossy(&output.stderr));
        connector.kill()?;
        return Err(HsmError::SelfCertGenFail.into());
    }

    let mut cmd = Command::new("openssl");
    let output = cmd
        .arg("ca")
        .arg("-batch")
        .arg("-selfsign")
        .arg("-config")
        .arg("openssl.cnf")
        .arg("-engine")
        .arg("pkcs11")
        .arg("-keyform")
        .arg("engine")
        .arg("-keyfile")
        .arg(format!("0:{:#04}", spec.id))
        .arg("-in")
        .arg("csr.pem")
        .arg("-out")
        .arg("certs/ca.cert.pem")
        .output()?;

    info!("executing command: \"{:#?}\"", cmd);

    if !output.status.success() {
        warn!("command failed with status: {}", output.status);
        warn!("stderr: \"{}\"", String::from_utf8_lossy(&output.stderr));
        connector.kill()?;
        return Err(HsmError::SelfCertGenFail.into());
    }

    connector.kill()?;

    std::env::set_current_dir(pwd)?;

    Ok(())
}

//
fn bootstrap_ca(key_spec: &KeySpec, out_dir: &Path) -> Result<()> {
    // create CA directory from key_spec.label
    let mut ca_dir = out_dir.to_path_buf();
    ca_dir.push(key_spec.label.to_string());
    info!("bootstrapping CA files in: {}", ca_dir.display());
    debug!("creating directory: {}", ca_dir.display());
    fs::create_dir(&ca_dir)?;

    // create directories expected by `openssl ca` certs, crl, newcerts,
    for dir in ["certs", "crl", "newcerts"] {
        ca_dir.push(dir);
        debug!("creating directory: {}?", ca_dir.display());
        fs::create_dir(&ca_dir)?;
        ca_dir.pop();
    }

    // the 'private' directory is a special case w/ restricted permissions
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;
    ca_dir.push("private");
    debug!("creating directory: {}?", ca_dir.display());
    fs::create_dir(&ca_dir)?;
    let perms = Permissions::from_mode(0o700);
    debug!(
        "setting permissions on directory {} to {:#?}",
        ca_dir.display(),
        perms
    );
    fs::set_permissions(&ca_dir, perms)?;
    ca_dir.pop();

    // touch 'index.txt' file
    use std::fs::OpenOptions;
    ca_dir.push("index.txt");
    debug!("touching file {}", ca_dir.display());
    OpenOptions::new().create(true).write(true).open(&ca_dir)?;
    ca_dir.pop();

    // write initial serial number to 'serial' (echo 1000 > serial)
    ca_dir.push("serial");
    let sn = 1000u32;
    debug!(
        "setting initial serial number to {} in file {}",
        sn,
        ca_dir.display()
    );
    fs::write(&ca_dir, sn.to_string())?;
    ca_dir.pop();

    // create & write out an openssl.cnf
    ca_dir.push("openssl.cnf");
    fs::write(
        &ca_dir,
        format!(openssl_cnf_fmt!(), key = key_spec.id, hash = key_spec.hash),
    )?;
    ca_dir.pop();

    // TODO: I'd like to generate self signed certs for the CA created here
    // but we're using the USB connector and it can't be closed so that we
    // can start the yubihsm-connector process :(
    // NOTE: the yubihsm.rs example http server doesn't work with the
    // yubihsm-shell I've got installed, fails with
    // "Unable to find a suitable connector"

    Ok(())
}

// consts for our authentication credential
const AUTH_DOMAINS: Domain = Domain::all();
const AUTH_CAPS: Capability = Capability::all();
const AUTH_DELEGATED: Capability = Capability::all();
const AUTH_ID: Id = 2;
const AUTH_LABEL: &str = "admin";

/// This function prompts the user to enter M of the N backup shares. It
/// uses these shares to reconstitute the wrap key. This wrap key can then
/// be used to restore previously backed up / export wrapped keys.
pub fn restore(client: &Client) -> Result<()> {
    let mut shares: Vec<String> = Vec::new();

    for i in 1..=THRESHOLD {
        println!("Enter share[{}]: ", i);
        shares.push(io::stdin().lines().next().unwrap().unwrap());
    }

    for (i, share) in shares.iter().enumerate() {
        println!("share[{}]: {}", i, share);
    }

    let wrap_key =
        rusty_secrets::recover_secret(shares).unwrap_or_else(|err| {
            println!("Unable to recover key: {}", err);
            std::process::exit(1);
        });

    debug!("restored wrap key: {}", wrap_key.encode_hex::<String>());

    // put restored wrap key the YubiHSM as an Aes256Ccm wrap key
    let id = client
        .put_wrap_key(
            ID,
            Label::from_bytes(LABEL.as_bytes())?,
            DOMAIN,
            CAPS,
            DELEGATED_CAPS,
            ALG,
            wrap_key,
        )
        .with_context(|| {
            format!(
                "Failed to put wrap key into YubiHSM domains {:?} with id {}",
                DOMAIN, ID
            )
        })?;
    info!("wrap id: {}", id);

    Ok(())
}

/// Initialize a new YubiHSM 2 by creating:
/// - a new wap key for backup
/// - a new auth key derived from a user supplied password
/// This new auth key is backed up / exported under wrap using the new wrap
/// key. This backup is written to the provided directory path. Finally this
/// function removes the default authentication credentials.
pub fn initialize(client: &Client, out_dir: &Path) -> Result<()> {
    // get 32 bytes from YubiHSM PRNG
    // TODO: zeroize
    let wrap_key = client.get_pseudo_random(KEY_LEN)?;
    info!("got {} bytes from YubiHSM PRNG", KEY_LEN);
    debug!("got wrap key: {}", wrap_key.encode_hex::<String>());

    // put 32 random bytes into the YubiHSM as an Aes256Ccm wrap key
    let id = client
        .put_wrap_key::<Vec<u8>>(
            ID,
            Label::from_bytes(LABEL.as_bytes())?,
            DOMAIN,
            CAPS,
            DELEGATED_CAPS,
            ALG,
            wrap_key.clone(),
        )
        .with_context(|| {
            format!(
                "Failed to put wrap key into YubiHSM domains {:?} with id {}",
                DOMAIN, ID
            )
        })?;
    info!("wrap id: {}", id);

    // do the stuff from replace-auth.sh
    personalize(client, id, out_dir)?;

    let shares = rusty_secrets::generate_shares(THRESHOLD, SHARES, &wrap_key)
        .with_context(|| {
        format!(
            "Failed to split secret into {} shares with threashold {}",
            SHARES, THRESHOLD
        )
    })?;

    println!(
        "WARNING: The wrap / backup key has been created and stored in the\n\
        YubiHSM. It will now be split into {} key shares. The operator must\n\
        record these shares as they're displayed. Failure to do so will\n\
        result in the inability to reconstruct this key and restore\n\
        backups.\n\n\
        Press enter to begin the key share recording process ...",
        SHARES
    );

    wait_for_line();
    clear_screen();

    for (i, share) in shares.iter().enumerate() {
        let share_num = i + 1;
        println!(
            "When key custodian {share} is steated, press enter to display \
            share {share}",
            share = share_num
        );
        wait_for_line();

        // Can we generate a QR code, photograph it & then recover the key by
        // reading them back through the camera?
        println!("\n{}\n", share);
        println!("When you are done recording this key share, press enter");
        wait_for_line();
        clear_screen();
    }

    Ok(())
}

// create a new auth key, remove the default auth key, then export the new
// auth key under the wrap key with the provided id
fn personalize(client: &Client, wrap_id: Id, out_dir: &Path) -> Result<()> {
    debug!(
        "personalizing with wrap key {} and out_dir {}",
        wrap_id,
        out_dir.display()
    );
    // get a new password from the user
    let mut password = loop {
        let password = rpassword::prompt_password(PASSWD_PROMPT).unwrap();
        let mut password2 = rpassword::prompt_password(PASSWD_PROMPT2).unwrap();
        if password != password2 {
            error!("the passwords entered do not match");
        } else {
            password2.zeroize();
            break password;
        }
    };
    debug!("got the same password twice: {}", password);

    // not compatible with Zeroizing wrapper
    let auth_key = Key::derive_from_password(password.as_bytes());

    debug!("putting new auth key from provided password");
    // create a new auth key
    client.put_authentication_key(
        AUTH_ID,
        AUTH_LABEL.into(),
        AUTH_DOMAINS,
        AUTH_CAPS,
        AUTH_DELEGATED,
        authentication::Algorithm::default(), // can't be used in const
        auth_key,
    )?;

    debug!("deleting default auth key");
    client.delete_object(
        DEFAULT_AUTHENTICATION_KEY_ID,
        Type::AuthenticationKey,
    )?;

    debug!("exporting new auth key under wrap-key w/ id: {}", wrap_id);
    let msg =
        client.export_wrapped(wrap_id, Type::AuthenticationKey, AUTH_ID)?;

    // include additional metadata (enough to reconstruct current state)?
    let msg_json = serde_json::to_string(&msg)?;

    debug!("msg_json: {:#?}", msg_json);

    // we need to append a name for our file
    let mut out_dir = out_dir.to_path_buf();
    out_dir.push(format!("{}.json", AUTH_LABEL));

    debug!("writing to: {}", out_dir.display());
    fs::write(out_dir, msg_json)?;

    password.zeroize();

    Ok(())
}

/// This "clears" the screen using terminal control characters. If your
/// terminal has a scroll bar that can be used to scroll back to previous
/// screens that had been "cleared".
fn clear_screen() {
    print!("{esc}[2J{esc}[1;1H", esc = 27 as char);
}

/// This function is used when displaying key shares as a way for the user to
/// control progression through the key shares displayed in the terminal.
fn wait_for_line() {
    let _ = io::stdin().lines().next().unwrap().unwrap();
}
