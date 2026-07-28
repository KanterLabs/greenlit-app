use std::fs;
use std::path::Path;
use std::process::Output;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use linux_keyutils::{KeyError, KeyRing, KeyRingIdentifier};

use crate::support::Sandbox;

static DESCRIPTION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub struct PersistentCredential {
    ring: KeyRing,
    description: String,
    active: bool,
}

impl PersistentCredential {
    pub fn new(case: &str) -> Self {
        let ring = KeyRing::get_persistent(KeyRingIdentifier::Session)
            .expect("the credential capability job requires a Linux persistent keyring");
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before the Unix epoch")
            .as_nanos();
        let sequence = DESCRIPTION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let description = format!(
            "litci-test:{}:{timestamp}:{sequence}:{case}",
            std::process::id()
        );
        assert!(
            description.len() <= 128,
            "credential test key description exceeded its production bound"
        );
        match ring.search(&description) {
            Err(KeyError::KeyDoesNotExist) => {}
            Ok(_) => panic!("credential test key unexpectedly existed before the test"),
            Err(_) => panic!("could not prove the credential test key was initially absent"),
        }
        Self {
            ring,
            description,
            active: true,
        }
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn assert_present(&self) {
        self.ring
            .search(&self.description)
            .expect("compiled litci did not create the expected persistent credential key");
    }

    pub fn cleanup(mut self) {
        let key = self
            .ring
            .search(&self.description)
            .expect("credential key was absent before exact teardown");
        self.ring
            .unlink_key(key)
            .expect("could not unlink the exact credential test key");
        match self.ring.search(&self.description) {
            Err(KeyError::KeyDoesNotExist) => {}
            Ok(_) => panic!("credential test key remained after exact teardown"),
            Err(_) => panic!("could not verify credential key absence after teardown"),
        }
        self.active = false;
    }
}

impl Drop for PersistentCredential {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Ok(key) = self.ring.search(&self.description) {
            let _cleanup_result = self.ring.unlink_key(key);
        }
    }
}

pub fn assert_clean(output: &Output, sandbox: &Sandbox, secrets: &[&str]) {
    for secret in secrets {
        assert_bytes_absent(&output.stdout, secret.as_bytes(), "standard output");
        assert_bytes_absent(&output.stderr, secret.as_bytes(), "standard error");
    }
    assert_tree_clean(sandbox.home(), secrets);
    assert_tree_clean(sandbox.root(), secrets);
    assert!(
        !sandbox.home().join(".litci/auth.json").exists(),
        "compiled litci created the forbidden plaintext auth.json"
    );
}

pub fn assert_path_clean(path: &Path, secrets: &[&str]) {
    assert_tree_clean(path, secrets);
}

pub fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "{operation} failed in the credential capability job"
    );
}

pub fn assert_failure(output: &Output, operation: &str) {
    assert!(
        !output.status.success(),
        "{operation} unexpectedly succeeded in the credential capability job"
    );
}

pub fn assert_stdout_contains(output: &Output, expected: &str, operation: &str) {
    let text = std::str::from_utf8(&output.stdout)
        .expect("compiled litci standard output was not valid UTF-8");
    assert!(
        text.contains(expected),
        "{operation} omitted its required success diagnostic"
    );
}

pub fn assert_stderr_contains(output: &Output, expected: &str, operation: &str) {
    let text = std::str::from_utf8(&output.stderr)
        .expect("compiled litci standard error was not valid UTF-8");
    assert!(
        text.contains(expected),
        "{operation} omitted its required failure diagnostic"
    );
}

pub fn assert_request_path(request: &str, expected: &str) {
    assert!(
        request.lines().next().is_some_and(|line| line == expected),
        "compiled litci called the wrong external endpoint"
    );
}

pub fn assert_bearer(request: &str, token: &str) {
    let expected = format!("Bearer {token}");
    let matched = request.lines().any(|line| {
        let Some((name, value)) = line.trim_end_matches('\r').split_once(':') else {
            return false;
        };
        name.eq_ignore_ascii_case("authorization") && value.trim() == expected
    });
    assert!(
        matched,
        "compiled litci did not use the credential loaded from the persistent keyring"
    );
}

pub fn assert_form_value(request: &str, name: &str, value: &str) {
    let body = request.split_once("\r\n\r\n").map_or("", |(_, body)| body);
    let matched = body.split('&').any(|field| {
        field
            .split_once('=')
            .is_some_and(|(field_name, field_value)| field_name == name && field_value == value)
    });
    assert!(
        matched,
        "compiled litci sent the wrong value to the external OAuth boundary"
    );
}

fn assert_tree_clean(root: &Path, secrets: &[&str]) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path).expect("inspect credential test path");
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(&path).expect("enumerate credential test directory") {
                pending.push(entry.expect("read credential test directory entry").path());
            }
            continue;
        }
        if metadata.is_file() {
            let bytes = fs::read(&path).expect("read credential test file");
            for secret in secrets {
                assert_bytes_absent(&bytes, secret.as_bytes(), "sandbox file");
            }
        }
    }
}

fn assert_bytes_absent(haystack: &[u8], needle: &[u8], location: &str) {
    if needle.is_empty() || haystack.len() < needle.len() {
        return;
    }
    assert!(
        !haystack
            .windows(needle.len())
            .any(|window| window == needle),
        "credential-bearing plaintext reached {location}"
    );
}
