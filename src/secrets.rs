//! Encryption of stored passwords.
//!
//! On Windows secrets are protected with DPAPI (user scope): only the Windows
//! account that saved a credential can decrypt it, so run the console under
//! the same account that created the credentials (e.g. a service account).
//! On Linux they are stored base64-encoded and protected only by the
//! permissions of the data directory; prefer SSH keys there.

use anyhow::{Result, anyhow};
use base64::{Engine, engine::general_purpose::STANDARD as B64};

#[cfg(windows)]
const PREFIX: &str = "dpapi:";
#[cfg(not(windows))]
const PREFIX: &str = "b64:";

pub fn protect(plain: &str) -> Result<String> {
    let data = platform::protect(plain.as_bytes())?;
    Ok(format!("{PREFIX}{}", B64.encode(data)))
}

pub fn unprotect(stored: &str) -> Result<String> {
    let body = stored
        .strip_prefix(PREFIX)
        .ok_or_else(|| anyhow!("credential was saved on another platform; re-enter it"))?;
    let raw = B64.decode(body)?;
    let plain = platform::unprotect(&raw)?;
    Ok(String::from_utf8(plain)?)
}

#[cfg(windows)]
mod platform {
    use anyhow::{Result, bail};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
    }

    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        // SAFETY: DPAPI returned a LocalAlloc'd buffer of cbData bytes.
        unsafe {
            let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            LocalFree(out.pbData as _);
            v
        }
    }

    pub fn protect(data: &[u8]) -> Result<Vec<u8>> {
        let input = blob(data);
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: input points at `data` for the duration of the call.
        let ok = unsafe {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            bail!("CryptProtectData failed: {}", std::io::Error::last_os_error());
        }
        Ok(unsafe { take(out) })
    }

    pub fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
        let input = blob(data);
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: as above.
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            bail!(
                "cannot decrypt credential (saved by a different Windows account?): {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(unsafe { take(out) })
    }
}

#[cfg(not(windows))]
mod platform {
    use anyhow::Result;

    pub fn protect(data: &[u8]) -> Result<Vec<u8>> {
        Ok(data.to_vec())
    }

    pub fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
        Ok(data.to_vec())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let enc = super::protect("p@ss wörd").unwrap();
        assert_ne!(enc, "p@ss wörd");
        assert_eq!(super::unprotect(&enc).unwrap(), "p@ss wörd");
    }
}
