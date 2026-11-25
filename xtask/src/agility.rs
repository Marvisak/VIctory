//! Stage an app-local D3D12 Agility SDK runtime for Windows builds.
//!
//! Downloads the pinned `Microsoft.Direct3D.D3D12` NuGet package, checks its
//! SHA-256, extracts `D3D12Core.dll` (and the debug layer when present), and
//! copies them into `target/<profile>/D3D12/` so `apps/victory`'s build
//! script can export the loader's opt-in symbols. The DLLs are never stored
//! in the repository (asset policy).

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Pinned Agility SDK package (matches `D3D12SDKVersion` 614 in `apps/victory`).
const PACKAGE: &str = "Microsoft.Direct3D.D3D12";
const VERSION: &str = "1.614.1";
const NUPKG_URL: &str = "https://api.nuget.org/v3-flatcontainer/microsoft.direct3d.d3d12/1.614.1/microsoft.direct3d.d3d12.1.614.1.nupkg";
/// `sha256sum` of the NuGet package bytes (verified 2026-11-25).
const NUPKG_SHA256: &str = "9880aa91602dd51dd6cf7911a2bca7a2323513b15338573cde014b3356eeaff2";
/// `sha256sum` of `build/native/bin/x64/D3D12Core.dll` inside that package.
const CORE_DLL_SHA256: &str = "8a23d826b25b4329522ff451cb52b7f2b34d7f2913cfeb878371ce8bd765fe2d";
const CORE_DLL_IN_PACKAGE: &str = "build/native/bin/x64/D3D12Core.dll";
const LAYERS_DLL_IN_PACKAGE: &str = "build/native/bin/x64/d3d12SDKLayers.dll";

pub fn run(root: &Path, profile: &str) -> Result<()> {
    let cache = root.join("target").join("agility-cache").join(VERSION);
    fs::create_dir_all(&cache).with_context(|| format!("creating {}", cache.display()))?;
    let nupkg = cache.join(format!("{PACKAGE}.{VERSION}.nupkg"));
    if !nupkg.is_file() || !sha256_matches(&nupkg, NUPKG_SHA256)? {
        download(NUPKG_URL, &nupkg)?;
    }
    ensure_sha256(&nupkg, NUPKG_SHA256)?;

    let extract = cache.join("extract");
    if extract.exists() {
        fs::remove_dir_all(&extract)
            .with_context(|| format!("removing {}", extract.display()))?;
    }
    fs::create_dir_all(&extract).with_context(|| format!("creating {}", extract.display()))?;
    unzip(&nupkg, &extract)?;

    let core_src = extract.join(CORE_DLL_IN_PACKAGE);
    ensure_sha256(&core_src, CORE_DLL_SHA256)?;

    let dest = root.join("target").join(profile).join("D3D12");
    fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;
    let core_dest = dest.join("D3D12Core.dll");
    fs::copy(&core_src, &core_dest).with_context(|| {
        format!(
            "copying {} -> {}",
            core_src.display(),
            core_dest.display()
        )
    })?;
    ensure_sha256(&core_dest, CORE_DLL_SHA256)?;

    let layers_src = extract.join(LAYERS_DLL_IN_PACKAGE);
    if layers_src.is_file() {
        let layers_dest = dest.join("d3d12SDKLayers.dll");
        fs::copy(&layers_src, &layers_dest).with_context(|| {
            format!(
                "copying {} -> {}",
                layers_src.display(),
                layers_dest.display()
            )
        })?;
        println!("staged {}", layers_dest.display());
    }

    println!("staged {}", core_dest.display());
    let build = if profile == "release" {
        "cargo build --release -p victory"
    } else {
        "cargo build -p victory"
    };
    println!(
        "rebuild with `{build}` so the binary exports D3D12SDKVersion=614 \
         (Agility SDK {VERSION})"
    );
    Ok(())
}

fn download(url: &str, dest: &Path) -> Result<()> {
    println!("downloading {url}");
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension("nupkg.partial");
    // Prefer curl (Windows 10+ and Linux agents both ship it). Fall back to
    // PowerShell on hosts without curl in PATH.
    let status = Command::new("curl")
        .args(["-fsSL", "--retry", "3", "-o"])
        .arg(&tmp)
        .arg(url)
        .status();
    match status {
        Ok(s) if s.success() => {}
        _ => {
            let ps = format!(
                "Invoke-WebRequest -UseBasicParsing -Uri '{}' -OutFile '{}'",
                url,
                tmp.display()
            );
            let status = Command::new("powershell")
                .args(["-NoProfile", "-Command", &ps])
                .status()
                .context("download: curl and powershell both failed to start")?;
            anyhow::ensure!(status.success(), "powershell download failed");
        }
    }
    fs::rename(&tmp, dest).with_context(|| format!("renaming {}", tmp.display()))?;
    Ok(())
}

fn unzip(nupkg: &Path, dest: &Path) -> Result<()> {
    // NuGet packages are zip archives. `tar` on some hosts rejects `.nupkg`
    // even though it is a zip; prefer `unzip`, then PowerShell.
    let status = Command::new("unzip")
        .args(["-q", "-o"])
        .arg(nupkg)
        .arg("-d")
        .arg(dest)
        .status();
    if let Ok(s) = status
        && s.success()
    {
        return Ok(());
    }
    let status = Command::new("tar")
        .args(["-xf"])
        .arg(nupkg)
        .arg("-C")
        .arg(dest)
        .status();
    if let Ok(s) = status
        && s.success()
    {
        return Ok(());
    }
    let ps = format!(
        "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
        nupkg.display(),
        dest.display()
    );
    let status = Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .status()
        .context("unzip: unzip, tar, and powershell all failed to start")?;
    anyhow::ensure!(status.success(), "powershell Expand-Archive failed");
    Ok(())
}

fn ensure_sha256(path: &Path, expect: &str) -> Result<()> {
    if !sha256_matches(path, expect)? {
        bail!("{} SHA-256 mismatch (expected {expect})", path.display());
    }
    Ok(())
}

fn sha256_matches(path: &Path, expect: &str) -> Result<bool> {
    let got = sha256_hex(path)?;
    Ok(got.eq_ignore_ascii_case(expect))
}

fn sha256_hex(path: &Path) -> Result<String> {
    // Prefer a small Python one-liner (agents and Windows installs often have
    // it). Fall back to sha256sum / CertUtil.
    for python in ["python3", "python"] {
        let out = Command::new(python)
            .args([
                "-c",
                "import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],'rb').read()).hexdigest())",
                &path.display().to_string(),
            ])
            .output();
        if let Ok(out) = out
            && out.status.success()
        {
            return Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned());
        }
    }
    let out = Command::new("sha256sum").arg(path).output();
    if let Ok(out) = out
        && out.status.success()
    {
        let line = String::from_utf8_lossy(&out.stdout);
        let hex = line.split_whitespace().next().unwrap_or("").to_owned();
        if !hex.is_empty() {
            return Ok(hex);
        }
    }
    let cert = Command::new("CertUtil")
        .args(["-hashfile"])
        .arg(path)
        .arg("SHA256")
        .output()
        .with_context(|| format!("hashing {}", path.display()))?;
    anyhow::ensure!(
        cert.status.success(),
        "CertUtil failed for {}",
        path.display()
    );
    let text = String::from_utf8_lossy(&cert.stdout);
    let hex: String = text
        .lines()
        .map(str::trim)
        .find(|l| {
            let digits: String = l.chars().filter(|c| c.is_ascii_hexdigit()).collect();
            digits.len() == 64
        })
        .map(|l| l.chars().filter(|c| c.is_ascii_hexdigit()).collect())
        .unwrap_or_default();
    anyhow::ensure!(!hex.is_empty(), "could not parse CertUtil output");
    Ok(hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn package_constants_agree() {
        assert!(NUPKG_SHA256.len() == 64);
        assert!(CORE_DLL_SHA256.len() == 64);
        assert!(CORE_DLL_IN_PACKAGE.ends_with("D3D12Core.dll"));
        assert_eq!(VERSION, "1.614.1");
    }

    #[test]
    fn sha256_of_known_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("probe.bin");
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(b"victory-agility").unwrap();
        // printf 'victory-agility' | sha256sum
        let expect = "c6e7e22d1242584501d84ced29040f0e00f89e25d2ed55fbcb9693f45136e665";
        assert_eq!(sha256_hex(&path).unwrap(), expect);
        assert!(sha256_matches(&path, expect).unwrap());
        assert!(!sha256_matches(&path, NUPKG_SHA256).unwrap());
    }
}
