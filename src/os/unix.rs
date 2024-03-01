use std::{
    env,
    ffi::{c_void, CStr, OsString},
    fs,
    io::{Error, ErrorKind},
    mem,
    os::{
        raw::{c_char, c_int},
        unix::ffi::OsStringExt,
    },
};
#[cfg(target_os = "macos")]
use std::{
    os::{
        raw::{c_long, c_uchar},
        unix::ffi::OsStrExt,
    },
    ptr::null_mut,
};

use nix::unistd::{Uid, User};

use crate::{
    os::{Os, Target},
    Arch, DesktopEnv, Language, Platform, Result,
};

extern "system" {
    fn gethostname(name: *mut c_void, len: usize) -> i32;
}

#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
#[link(name = "SystemConfiguration", kind = "framework")]
extern "system" {
    fn CFStringGetCString(
        the_string: *mut c_void,
        buffer: *mut u8,
        buffer_size: c_long,
        encoding: u32,
    ) -> c_uchar;
    fn CFStringGetLength(the_string: *mut c_void) -> c_long;
    fn CFStringGetMaximumSizeForEncoding(
        length: c_long,
        encoding: u32,
    ) -> c_long;
    fn SCDynamicStoreCopyComputerName(
        store: *mut c_void,
        encoding: *mut u32,
    ) -> *mut c_void;
    fn CFRelease(cf: *const c_void);
}

enum Name {
    User,
    Real,
}

unsafe fn strlen(cs: *const c_void) -> usize {
    let mut len = 0;
    let mut cs: *const u8 = cs.cast();
    while *cs != 0 {
        len += 1;
        cs = cs.offset(1);
    }
    len
}

#[cfg(target_os = "macos")]
fn os_from_cfstring(string: *mut c_void) -> OsString {
    if string.is_null() {
        return "".to_string().into();
    }

    unsafe {
        let len = CFStringGetLength(string);
        let capacity =
            CFStringGetMaximumSizeForEncoding(len, 134_217_984 /* UTF8 */) + 1;
        let mut out = Vec::with_capacity(capacity as usize);
        if CFStringGetCString(
            string,
            out.as_mut_ptr(),
            capacity,
            134_217_984, /* UTF8 */
        ) != 0
        {
            out.set_len(strlen(out.as_ptr().cast())); // Remove trailing NUL byte
            out.shrink_to_fit();
            CFRelease(string);
            OsString::from_vec(out)
        } else {
            CFRelease(string);
            "".to_string().into()
        }
    }
}

#[inline(always)]
fn getpwuid(name: Name) -> Result<OsString> {
    let user = User::from_uid(Uid::effective())?
        .ok_or_else(|| Error::new(ErrorKind::NotFound, "Null record"))?;

    match name {
        Name::User => Ok(OsString::from(user.name)),
        Name::Real => {
            // * The full user name is stored in the gecos field, which is
            //   exposed by nix as a `CString` (C-style null-terminated string).
            // * `CString::into_bytes` converts the string into a `Vec<u8>`
            //   without the trailing null.
            // * `OsString::from_vec`, only available on Unix, converts the
            //   `Vec<u8>` into an `OsString`.
            Ok(OsString::from_vec(user.gecos.into_bytes()))
        }
    }
}

#[cfg(target_os = "macos")]
fn distro_xml(data: String) -> Result<String> {
    let mut product_name = None;
    let mut user_visible_version = None;

    if let Some(start) = data.find("<dict>") {
        if let Some(end) = data.find("</dict>") {
            let mut set_product_name = false;
            let mut set_user_visible_version = false;

            for line in data[start + "<dict>".len()..end].lines() {
                let line = line.trim();

                if line.starts_with("<key>") {
                    match line["<key>".len()..].trim_end_matches("</key>") {
                        "ProductName" => set_product_name = true,
                        "ProductUserVisibleVersion" => {
                            set_user_visible_version = true
                        }
                        "ProductVersion" => {
                            if user_visible_version.is_none() {
                                set_user_visible_version = true
                            }
                        }
                        _ => {}
                    }
                } else if line.starts_with("<string>") {
                    if set_product_name {
                        product_name = Some(
                            line["<string>".len()..]
                                .trim_end_matches("</string>"),
                        );
                        set_product_name = false;
                    } else if set_user_visible_version {
                        user_visible_version = Some(
                            line["<string>".len()..]
                                .trim_end_matches("</string>"),
                        );
                        set_user_visible_version = false;
                    }
                }
            }
        }
    }

    Ok(if let Some(product_name) = product_name {
        if let Some(user_visible_version) = user_visible_version {
            format!("{} {}", product_name, user_visible_version)
        } else {
            product_name.to_string()
        }
    } else {
        user_visible_version
            .map(|v| format!("Mac OS (Unknown) {}", v))
            .ok_or_else(|| {
                Error::new(ErrorKind::InvalidData, "Parsing failed")
            })?
    })
}

struct LangIter {
    array: String,
    index: Option<bool>,
}

impl Iterator for LangIter {
    type Item = String;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index? && self.array.contains('-') {
            self.index = Some(false);
            let mut temp = self.array.split('-').next()?.to_string();
            mem::swap(&mut temp, &mut self.array);
            Some(temp)
        } else {
            self.index = None;
            let mut temp = String::new();
            mem::swap(&mut temp, &mut self.array);
            Some(temp)
        }
    }
}

#[inline(always)]
pub(crate) fn lang() -> impl Iterator<Item = String> {
    const DEFAULT_LANG: &str = "en_US";

    let array = env::var("LANG")
        .unwrap_or_default()
        .split('.')
        .next()
        .unwrap_or(DEFAULT_LANG)
        .to_string();
    let array = if array == "C" {
        DEFAULT_LANG.to_string()
    } else {
        array
    };

    LangIter {
        array: array.replace('_', "-"),
        index: Some(true),
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "illumos"
))]
#[repr(C)]
struct UtsName {
    sysname: [c_char; 256],
    nodename: [c_char; 256],
    release: [c_char; 256],
    version: [c_char; 256],
    machine: [c_char; 256],
}

#[cfg(target_os = "dragonfly")]
#[repr(C)]
struct UtsName {
    sysname: [c_char; 32],
    nodename: [c_char; 32],
    release: [c_char; 32],
    version: [c_char; 32],
    machine: [c_char; 32],
}

#[cfg(any(target_os = "linux", target_os = "android",))]
#[repr(C)]
struct UtsName {
    sysname: [c_char; 65],
    nodename: [c_char; 65],
    release: [c_char; 65],
    version: [c_char; 65],
    machine: [c_char; 65],
    domainname: [c_char; 65],
}

// Buffer initialization
impl Default for UtsName {
    fn default() -> Self {
        unsafe { mem::zeroed() }
    }
}

#[inline(always)]
unsafe fn uname(buf: *mut UtsName) -> c_int {
    extern "C" {
        #[cfg(not(target_os = "freebsd"))]
        fn uname(buf: *mut UtsName) -> c_int;

        #[cfg(target_os = "freebsd")]
        fn __xuname(nmln: c_int, buf: *mut c_void) -> c_int;
    }

    // Polyfill `uname()` for FreeBSD
    #[inline(always)]
    #[cfg(target_os = "freebsd")]
    unsafe extern "C" fn uname(buf: *mut UtsName) -> c_int {
        __xuname(256, buf.cast())
    }

    uname(buf)
}

impl Target for Os {
    fn langs(self) -> Vec<Language> {
        todo!()
    }

    fn realname(self) -> Result<OsString> {
        getpwuid(Name::Real)
    }

    fn username(self) -> Result<OsString> {
        getpwuid(Name::User)
    }

    fn devicename(self) -> Result<OsString> {
        #[cfg(target_os = "macos")]
        {
            let out = os_from_cfstring(unsafe {
                SCDynamicStoreCopyComputerName(null_mut(), null_mut())
            });

            if out.as_bytes().is_empty() {
                return Err(Error::new(ErrorKind::InvalidData, "Empty record"));
            }

            Ok(out)
        }

        #[cfg(target_os = "illumos")]
        {
            let mut nodename = fs::read("/etc/nodename")?;

            // Remove all at and after the first newline (before end of file)
            if let Some(slice) = nodename.split(|x| *x == b'\n').next() {
                nodename.drain(slice.len()..);
            }

            if nodename.is_empty() {
                return Err(Error::new(ErrorKind::InvalidData, "Empty record"));
            }

            Ok(OsString::from_vec(nodename))
        }

        #[cfg(not(any(target_os = "macos", target_os = "illumos")))]
        {
            let machine_info = fs::read("/etc/machine-info")?;

            for i in machine_info.split(|b| *b == b'\n') {
                let mut j = i.split(|b| *b == b'=');

                if j.next() == Some(b"PRETTY_HOSTNAME") {
                    if let Some(value) = j.next() {
                        // FIXME: Can " be escaped in pretty name?
                        return Ok(OsString::from_vec(value.to_vec()));
                    }
                }
            }

            Err(Error::new(ErrorKind::NotFound, "Missing record"))
        }
    }

    fn hostname(self) -> Result<String> {
        // Maximum hostname length = 255, plus a NULL byte.
        let mut string = Vec::<u8>::with_capacity(256);

        unsafe {
            if gethostname(string.as_mut_ptr().cast(), 255) == -1 {
                return Err(Error::last_os_error());
            }

            string.set_len(strlen(string.as_ptr().cast()));
        };

        String::from_utf8(string).map_err(|_| {
            Error::new(ErrorKind::InvalidData, "Hostname not valid UTF-8")
        })
    }

    fn distro(self) -> Result<String> {
        #[cfg(target_os = "macos")]
        {
            if let Ok(data) = fs::read_to_string(
                "/System/Library/CoreServices/ServerVersion.plist",
            ) {
                distro_xml(data)
            } else if let Ok(data) = fs::read_to_string(
                "/System/Library/CoreServices/SystemVersion.plist",
            ) {
                distro_xml(data)
            } else {
                Err(Error::new(ErrorKind::NotFound, "Missing record"))
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            let program = fs::read("/etc/os-release")?;
            let distro = String::from_utf8_lossy(&program);
            let err = || Error::new(ErrorKind::InvalidData, "Parsing failed");
            let mut fallback = None;

            for i in distro.split('\n') {
                let mut j = i.split('=');

                match j.next().ok_or_else(err)? {
                    "PRETTY_NAME" => {
                        return Ok(j
                            .next()
                            .ok_or_else(err)?
                            .trim_matches('"')
                            .to_string());
                    }
                    "NAME" => {
                        fallback = Some(
                            j.next()
                                .ok_or_else(err)?
                                .trim_matches('"')
                                .to_string(),
                        )
                    }
                    _ => {}
                }
            }

            fallback.ok_or_else(err)
        }
    }

    fn desktop_env(self) -> DesktopEnv {
        #[cfg(target_os = "macos")]
        let env = "Aqua";
        // FIXME: WhoAmI 2.0: use `let else`
        #[cfg(not(target_os = "macos"))]
        let env = env::var_os("DESKTOP_SESSION");
        #[cfg(not(target_os = "macos"))]
        let env = if let Some(ref env) = env {
            env.to_string_lossy()
        } else {
            return DesktopEnv::Unknown("Unknown".to_string());
        };

        if env.eq_ignore_ascii_case("AQUA") {
            DesktopEnv::Aqua
        } else if env.eq_ignore_ascii_case("GNOME") {
            DesktopEnv::Gnome
        } else if env.eq_ignore_ascii_case("LXDE") {
            DesktopEnv::Lxde
        } else if env.eq_ignore_ascii_case("OPENBOX") {
            DesktopEnv::Openbox
        } else if env.eq_ignore_ascii_case("I3") {
            DesktopEnv::I3
        } else if env.eq_ignore_ascii_case("UBUNTU") {
            DesktopEnv::Ubuntu
        } else if env.eq_ignore_ascii_case("PLASMA5") {
            DesktopEnv::Kde
        // TODO: Other Linux Desktop Environments
        } else {
            DesktopEnv::Unknown(env.to_string())
        }
    }

    #[inline(always)]
    fn platform(self) -> Platform {
        #[cfg(not(any(
            target_os = "macos",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "bitrig",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "illumos"
        )))]
        {
            Platform::Linux
        }

        #[cfg(target_os = "macos")]
        {
            Platform::MacOS
        }

        #[cfg(any(
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "bitrig",
            target_os = "openbsd",
            target_os = "netbsd"
        ))]
        {
            Platform::Bsd
        }

        #[cfg(target_os = "illumos")]
        {
            Platform::Illumos
        }
    }

    #[inline(always)]
    fn arch(self) -> Result<Arch> {
        let mut buf = UtsName::default();

        if unsafe { uname(&mut buf) } == -1 {
            return Err(Error::last_os_error());
        }

        let arch_str =
            unsafe { CStr::from_ptr(buf.machine.as_ptr()) }.to_string_lossy();

        Ok(match arch_str.as_ref() {
            "aarch64" | "arm64" | "aarch64_be" | "armv8b" | "armv8l" => {
                Arch::Arm64
            }
            "armv5" => Arch::ArmV5,
            "armv6" | "arm" => Arch::ArmV6,
            "armv7" => Arch::ArmV7,
            "i386" => Arch::I386,
            "i586" => Arch::I586,
            "i686" | "i686-AT386" => Arch::I686,
            "mips" => Arch::Mips,
            "mipsel" => Arch::MipsEl,
            "mips64" => Arch::Mips64,
            "mips64el" => Arch::Mips64El,
            "powerpc" | "ppc" | "ppcle" => Arch::PowerPc,
            "powerpc64" | "ppc64" | "ppc64le" => Arch::PowerPc64,
            "powerpc64le" => Arch::PowerPc64Le,
            "riscv32" => Arch::Riscv32,
            "riscv64" => Arch::Riscv64,
            "s390x" => Arch::S390x,
            "sparc" => Arch::Sparc,
            "sparc64" => Arch::Sparc64,
            "x86_64" | "amd64" => Arch::X64,
            _ => Arch::Unknown(arch_str.into_owned()),
        })
    }
}
