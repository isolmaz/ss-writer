//! Inputs of the WiX build: the MSI version, the deterministic product code and the
//! `wix build` command line.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::Error;

/// The WiX 7 OSMF EULA id accepted with `-acceptEula`.
const EULA_ID: &str = "wix7";

/// A release version `major.minor.patch` that Windows Installer can represent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReleaseVersion {
    major: u32,
    minor: u32,
    patch: u32,
}

impl ReleaseVersion {
    /// Parses `X.Y.Z` (no pre-release or build suffix: MSI versions are numeric, so a
    /// pre-release would sort after the final release) within the MSI limits
    /// (major, minor ≤ 255; patch ≤ 65535).
    pub fn parse(text: &str) -> Result<Self, Error> {
        let invalid = |reason: &str| {
            Error::Invalid(format!(
                "release version `{text}` is not usable for an MSI: {reason} (expected X.Y.Z)"
            ))
        };
        let parts: Vec<&str> = text.split('.').collect();
        let [major, minor, patch] = parts.as_slice() else {
            return Err(invalid("needs exactly three numeric parts"));
        };
        let number = |part: &str| {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid("parts must be plain numbers"));
            }
            part.parse::<u32>()
                .map_err(|_| invalid("a part is too large"))
        };
        let version = Self {
            major: number(major)?,
            minor: number(minor)?,
            patch: number(patch)?,
        };
        if version.major > 255 || version.minor > 255 || version.patch > 65535 {
            return Err(invalid("major and minor must be ≤ 255 and patch ≤ 65535"));
        }
        Ok(version)
    }

    /// `X.Y.Z`.
    pub fn text(self) -> String {
        format!("{}.{}.{}", self.major, self.minor, self.patch)
    }

    /// The Windows Installer `ProductCode` of this version: a braced upper-case GUID derived
    /// from the version, so the MSI and the winget manifest agree and rebuilds are stable.
    pub fn product_code(self) -> String {
        let digest = Sha256::digest(format!("SS Writer MSI product code {}", self.text()));
        let mut b = [0_u8; 16];
        b.copy_from_slice(&digest.as_slice()[..16]);
        b[6] = (b[6] & 0x0F) | 0x80; // version 8: custom, name-based
        b[8] = (b[8] & 0x3F) | 0x80; // RFC 4122 variant
        format!(
            "{{{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
            b[0],
            b[1],
            b[2],
            b[3],
            b[4],
            b[5],
            b[6],
            b[7],
            b[8],
            b[9],
            b[10],
            b[11],
            b[12],
            b[13],
            b[14],
            b[15]
        )
    }
}

/// Checks `tag` against `v<version>`.
pub fn check_tag(tag: &str, version: ReleaseVersion) -> Result<(), Error> {
    let expected = format!("v{}", version.text());
    if tag == expected {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "tag `{tag}` does not match the workspace version (expected `{expected}`); \
             bump `[workspace.package] version` in Cargo.toml first"
        )))
    }
}

/// Everything `wix build` needs.
#[derive(Debug)]
pub struct BuildInputs<'a> {
    /// The `.wxs` sources.
    pub sources: &'a [PathBuf],
    /// The MSI to write.
    pub output: &'a Path,
    /// `-define` pairs visible to the sources as `$(Name)`.
    pub defines: &'a [(&'a str, String)],
    /// Whether to pass the EULA acceptance flag.
    pub accept_eula: bool,
}

/// The arguments of `wix build` for a per-user x64 package without extensions.
pub fn build_args(inputs: &BuildInputs<'_>) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![OsString::from("build")];
    if inputs.accept_eula {
        args.push("-acceptEula".into());
        args.push(EULA_ID.into());
    }
    args.extend(["-arch", "x64", "-nologo", "-pdbtype", "none"].map(OsString::from));
    // Files are installed under %LOCALAPPDATA%; the harvested components use file key paths.
    args.extend(["-sice", "ICE38", "-sice", "ICE64"].map(OsString::from));
    for (name, value) in inputs.defines {
        args.push("-define".into());
        args.push(format!("{name}={value}").into());
    }
    args.push("-out".into());
    args.push(inputs.output.into());
    args.extend(inputs.sources.iter().map(OsString::from));
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_versions_parse_and_round_trip() {
        let v = ReleaseVersion::parse("1.20.300").unwrap();
        assert_eq!(v.text(), "1.20.300");
        assert!(ReleaseVersion::parse("255.255.65535").is_ok());
    }

    #[test]
    fn versions_msi_cannot_represent_are_rejected_with_a_reason() {
        for (bad, reason) in [
            ("1.2", "three"),
            ("1.2.3.4", "three"),
            ("1.2.3-rc.1", "three"),
            ("1.2.3-rc", "plain numbers"),
            ("1.2.3+build", "plain numbers"),
            ("1..3", "plain numbers"),
            ("1.2.x", "plain numbers"),
            ("1.2.99999999999", "too large"),
            ("256.0.0", "≤ 255"),
            ("0.256.0", "≤ 255"),
            ("0.0.65536", "≤ 255"),
        ] {
            let err = ReleaseVersion::parse(bad).unwrap_err().to_string();
            assert!(err.contains(reason), "{bad}: {err}");
            assert!(err.contains(bad), "{bad}: {err}");
        }
    }

    #[test]
    fn product_code_is_a_braced_uppercase_guid_that_depends_on_the_version() {
        let code = ReleaseVersion::parse("0.1.0").unwrap().product_code();
        assert_eq!(code.len(), 38, "{code}");
        assert!(code.starts_with('{') && code.ends_with('}'));
        let groups: Vec<usize> = code[1..37].split('-').map(str::len).collect();
        assert_eq!(groups, vec![8, 4, 4, 4, 12]);
        assert!(
            code[1..37]
                .chars()
                .all(|c| c == '-' || c.is_ascii_digit() || ('A'..='F').contains(&c)),
            "{code}"
        );
        assert_eq!(code.as_bytes()[15], b'8', "version nibble: {code}");
        assert!(
            matches!(code.as_bytes()[20], b'8' | b'9' | b'A' | b'B'),
            "variant: {code}"
        );
        assert_eq!(code, ReleaseVersion::parse("0.1.0").unwrap().product_code());
        assert_ne!(code, ReleaseVersion::parse("0.1.1").unwrap().product_code());
    }

    #[test]
    fn tag_must_equal_v_plus_version() {
        let v = ReleaseVersion::parse("0.3.1").unwrap();
        assert!(check_tag("v0.3.1", v).is_ok());
        let err = check_tag("v0.3.2", v).unwrap_err().to_string();
        assert!(err.contains("v0.3.2") && err.contains("v0.3.1"), "{err}");
        assert!(check_tag("0.3.1", v).is_err());
    }

    #[test]
    fn build_args_carry_defines_output_sources_and_the_optional_eula() {
        let sources = [PathBuf::from("a.wxs"), PathBuf::from("b.wxs")];
        let defines = [("Version", "1.2.3".to_owned())];
        let output = PathBuf::from("out.msi");
        let mut inputs = BuildInputs {
            sources: &sources,
            output: &output,
            defines: &defines,
            accept_eula: false,
        };
        let args: Vec<String> = build_args(&inputs)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "build");
        assert!(!args.contains(&"-acceptEula".to_owned()));
        let define_at = args.iter().position(|a| a == "-define").unwrap();
        assert_eq!(args[define_at + 1], "Version=1.2.3");
        let out_at = args.iter().position(|a| a == "-out").unwrap();
        assert_eq!(args[out_at + 1], "out.msi");
        assert_eq!(&args[args.len() - 2..], ["a.wxs", "b.wxs"]);

        inputs.accept_eula = true;
        let args: Vec<String> = build_args(&inputs)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let eula_at = args.iter().position(|a| a == "-acceptEula").unwrap();
        assert_eq!(args[eula_at + 1], "wix7");
    }
}
